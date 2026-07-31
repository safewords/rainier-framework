//! Relaying broadcasts back into the sockets **this** process holds.
//!
//! [`rainier-websocket`](rainier_websocket) says, in its own documentation,
//! that a `Rooms` registry lives in one process's memory and that two replicas
//! behind a load balancer therefore have two sets of rooms. This is the module
//! that stops that being true.
//!
//! ```text
//!   POST /orders/7/ship  ─▶ replica A ─▶ Broadcasting ─▶ kafka topic
//!                                                            │
//!                                        ┌───────────────────┴────────────┐
//!                                        ▼                                ▼
//!                                 relay on replica A              relay on replica B
//!                                        │                                │
//!                                   its Rooms                        its Rooms
//!                                        │                                │
//!                                    browsers                         browsers
//! ```
//!
//! Every replica publishes to the topic and every replica reads from it,
//! including the one that published — which is what makes a broadcast reach a
//! browser regardless of which replica served the request that caused it.
//!
//! # Two processes or one
//!
//! Nothing here needs a second deployment. [`spawn`] starts the relay inside
//! the web process, next to the sockets it feeds, so the whole of "broadcasting
//! that works behind a load balancer" is a Kafka topic and one line in a
//! provider.

use std::sync::Arc;
use std::time::Duration;

use rainier_broadcast::kafka::{KafkaRelay, RelayedBroadcast};
use rainier_support::Result;
use rainier_websocket::{socket_from_identity, Rooms};

/// How long to wait before reconnecting after the relay falls over.
///
/// A broker restart or a leader election should not permanently stop a
/// replica's broadcasts, and hammering a cluster that is having a bad time is
/// how a blip becomes an outage.
const RECONNECT_AFTER: Duration = Duration::from_secs(5);

/// Decides the room a channel's broadcasts belong in, or that they belong
/// nowhere here.
type RoomNamer = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Delivers relayed broadcasts to the sockets in a [`Rooms`] registry.
///
/// ```no_run
/// use std::sync::Arc;
/// use rainier_framework::relay::SocketFanOut;
/// use rainier_websocket::Rooms;
///
/// let rooms = Arc::new(Rooms::new());
///
/// // Channels are named `private-orders.7`; this application's rooms are
/// // named `orders.7`.
/// let fan_out = SocketFanOut::new(Arc::clone(&rooms))
///     .naming_rooms(|channel| Some(channel.trim_start_matches("private-").to_string()));
/// # let _ = fan_out;
/// ```
#[derive(Clone)]
pub struct SocketFanOut {
    rooms: Arc<Rooms>,
    room_for: RoomNamer,
}

impl SocketFanOut {
    /// Deliver to `rooms`, with the room named exactly as the channel is.
    pub fn new(rooms: Arc<Rooms>) -> Self {
        Self { rooms, room_for: Arc::new(|channel: &str| Some(channel.to_string())) }
    }

    /// Decide the room a channel's broadcasts belong in.
    ///
    /// `None` drops the broadcast, which is how a relay ignores channels this
    /// process does not serve — a topic shared with another application, or a
    /// channel that only a Pusher client listens to.
    pub fn naming_rooms<F>(mut self, room_for: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        self.room_for = Arc::new(room_for);
        self
    }

    /// Send one broadcast to whoever is in the room. Returns how many got it.
    ///
    /// `to_others` is honoured **here**, at the point of delivery, because this
    /// is the only place that knows which sockets it is talking to. An identity
    /// minted by another replica names no socket in this process, so nothing is
    /// skipped and everyone here is told — which is the correct answer, not a
    /// fallback.
    pub fn deliver(&self, broadcast: &RelayedBroadcast) -> usize {
        let Some(room) = (self.room_for)(&broadcast.channel) else {
            return 0;
        };

        let body = broadcast.wire_payload().to_string();

        match broadcast.except.as_deref().and_then(socket_from_identity) {
            Some(socket) => self.rooms.send_except(&room, socket, body),
            None => self.rooms.send(&room, body),
        }
    }

    /// The registry being fanned out to.
    pub fn rooms(&self) -> &Arc<Rooms> {
        &self.rooms
    }
}

/// Read the topic and deliver, until something goes wrong.
///
/// Returns on the first failure rather than swallowing it — [`spawn`] is what
/// adds the retry, and a caller running this directly usually wants to know.
pub async fn run(relay: &KafkaRelay, fan_out: &SocketFanOut) -> Result<()> {
    let mut cursor = relay.subscribe().await?;

    tracing::info!(
        topic = relay.topic(),
        partitions = cursor.len(),
        "relaying broadcasts to this process's sockets"
    );

    loop {
        for broadcast in relay.poll(&mut cursor).await? {
            let reached = fan_out.deliver(&broadcast);

            tracing::debug!(
                channel = broadcast.channel,
                event = broadcast.event,
                sockets = reached,
                "relayed"
            );
        }
    }
}

/// Run the relay in the background, reconnecting when it falls over.
///
/// Call it from a provider's `boot`, in the process that serves WebSockets:
///
/// ```ignore
/// let relay = KafkaRelay::new(Arc::clone(&kafka), "broadcasts");
/// relay::spawn(relay, SocketFanOut::new(Arc::clone(&rooms)));
/// ```
///
/// The task carries the facade scope with it, so anything the fan-out reaches
/// can resolve from the container — a spawned task inherits neither the thread
/// nor the task scope on its own.
pub fn spawn(relay: KafkaRelay, fan_out: SocketFanOut) -> tokio::task::JoinHandle<()> {
    rainier_container::spawn_with_facades(async move {
        loop {
            if let Err(e) = run(&relay, &fan_out).await {
                tracing::error!(
                    topic = relay.topic(),
                    error = %e,
                    "the broadcast relay stopped; reconnecting"
                );
            }

            tokio::time::sleep(RECONNECT_AFTER).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_drivers::kafka::KafkaPosition;
    use rainier_websocket::{Socket, SocketId};
    use serde_json::json;
    use tokio::sync::mpsc;

    fn broadcast(channel: &str, except: Option<&str>) -> RelayedBroadcast {
        RelayedBroadcast {
            channel: channel.to_string(),
            event: "OrderShipped".into(),
            payload: json!({ "id": 7 }),
            except: except.map(str::to_string),
            position: KafkaPosition { topic: "broadcasts".into(), partition: 0, offset: 1 },
        }
    }

    fn socket() -> (Socket, mpsc::UnboundedReceiver<rainier_websocket::Outbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Socket::new(SocketId::next(), "/ws", vec![], tx), rx)
    }

    #[test]
    fn a_broadcast_reaches_the_room_named_after_its_channel() {
        let rooms = Arc::new(Rooms::new());
        let (socket, mut rx) = socket();
        rooms.join("private-orders.7", socket);

        let reached = SocketFanOut::new(rooms).deliver(&broadcast("private-orders.7", None));

        assert_eq!(reached, 1);
        assert!(rx.try_recv().is_ok(), "the socket should have been sent something");
    }

    #[test]
    fn the_room_name_can_be_derived_from_the_channel() {
        let rooms = Arc::new(Rooms::new());
        let (socket, mut rx) = socket();
        rooms.join("orders.7", socket);

        let fan_out = SocketFanOut::new(rooms)
            .naming_rooms(|channel| Some(channel.trim_start_matches("private-").to_string()));

        assert_eq!(fan_out.deliver(&broadcast("private-orders.7", None)), 1);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn a_channel_this_process_does_not_serve_is_dropped() {
        let rooms = Arc::new(Rooms::new());
        let (socket, mut rx) = socket();
        rooms.join("private-orders.7", socket);

        let fan_out = SocketFanOut::new(rooms).naming_rooms(|_| None);

        assert_eq!(fan_out.deliver(&broadcast("private-orders.7", None)), 0);
        assert!(rx.try_recv().is_err(), "nothing should have been sent");
    }

    #[test]
    fn to_others_skips_the_socket_that_caused_it() {
        let rooms = Arc::new(Rooms::new());
        let (mine, mut mine_rx) = socket();
        let (theirs, mut theirs_rx) = socket();

        let identity = mine.identity();
        rooms.join("news", mine);
        rooms.join("news", theirs);

        let reached = SocketFanOut::new(rooms).deliver(&broadcast("news", Some(&identity)));

        assert_eq!(reached, 1);
        assert!(mine_rx.try_recv().is_err(), "the socket that caused it hears nothing");
        assert!(theirs_rx.try_recv().is_ok(), "everyone else does");
    }

    #[test]
    fn an_identity_from_another_replica_skips_nobody_here() {
        // The subtle one. Socket ids are per-process counters, so replica B
        // also has a socket `7`. Skipping by the bare number would silence an
        // unrelated browser on every replica but the one that published.
        let rooms = Arc::new(Rooms::new());
        let (socket, mut rx) = socket();
        let number = socket.id().get();
        rooms.join("news", socket);

        let reached = SocketFanOut::new(rooms)
            .deliver(&broadcast("news", Some(&format!("another-replica.{number}"))));

        assert_eq!(reached, 1, "that socket is not in this process");
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn the_body_is_the_event_and_its_data() {
        let rooms = Arc::new(Rooms::new());
        let (socket, mut rx) = socket();
        rooms.join("news", socket);

        SocketFanOut::new(rooms).deliver(&broadcast("news", None));

        match rx.try_recv().unwrap() {
            rainier_websocket::Outbound::Send(message) => {
                let body: serde_json::Value =
                    serde_json::from_str(message.as_text().unwrap()).unwrap();

                assert_eq!(body["event"], "OrderShipped");
                assert_eq!(body["data"]["id"], 7);
            }
            other => panic!("{other:?}"),
        }
    }
}
