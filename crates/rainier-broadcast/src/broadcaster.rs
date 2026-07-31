//! Where broadcasts go — the [`Broadcaster`] port and the drivers that ship.

use std::sync::Mutex;

use serde_json::{json, Value};

use rainier_support::{BoxFuture, Result};

use crate::channel::Channel;
use crate::event::Broadcast;

/// One way of publishing a broadcast.
///
/// A port, so an application adds Pusher over HTTP, Ably, or a WebSocket server
/// of its own without anything here changing.
pub trait Broadcaster: Send + Sync + 'static {
    /// The driver's name, for diagnostics.
    fn name(&self) -> &'static str;

    /// Publish it.
    fn publish<'a>(&'a self, broadcast: &'a Broadcast) -> BoxFuture<'a, Result<()>>;

    /// The body the `/broadcasting/auth` endpoint should return.
    ///
    /// Driver-specific, because what proves a subscription is allowed depends
    /// on who is checking. A Pusher-protocol server wants an HMAC over the
    /// socket and channel — see [`PusherAuth`](crate::PusherAuth). A driver with
    /// no such check returns the default, which grants the subscription and
    /// signs nothing.
    ///
    /// `member` is the presence roster entry, `None` for a private channel.
    fn auth_response(
        &self,
        socket_id: &str,
        channel: &Channel,
        member: Option<&Value>,
    ) -> Result<Value> {
        let _ = (socket_id, channel);
        Ok(match member {
            Some(member) => json!({ "channel_data": member }),
            None => json!({}),
        })
    }
}

/// Writes broadcasts to the log. The safe default.
///
/// Publishes nothing, which is what makes it the right thing to have by
/// accident: an application with no broadcaster configured logs what it would
/// have sent rather than failing a request, and nothing reaches a browser.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogBroadcaster;

impl Broadcaster for LogBroadcaster {
    fn name(&self) -> &'static str {
        "log"
    }

    fn publish<'a>(&'a self, broadcast: &'a Broadcast) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::info!(
                event = broadcast.event,
                channels = ?broadcast.channels.iter().map(Channel::wire_name).collect::<Vec<_>>(),
                except = broadcast.except.as_deref().unwrap_or("-"),
                "broadcast"
            );
            Ok(())
        })
    }
}

/// Keeps broadcasts in memory so a test can assert on them.
///
/// Never right outside a test — nothing is published and the vector grows
/// until the process ends.
#[derive(Debug, Default)]
pub struct MemoryBroadcaster {
    sent: Mutex<Vec<Broadcast>>,
}

impl MemoryBroadcaster {
    /// An empty broadcaster.
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything captured so far.
    pub fn sent(&self) -> Vec<Broadcast> {
        self.lock().clone()
    }

    /// How many were captured.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether none were.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Everything published on `channel`, by its wire name.
    pub fn on(&self, channel: &str) -> Vec<Broadcast> {
        self.lock()
            .iter()
            .filter(|b| b.channels.iter().any(|c| c.wire_name() == channel))
            .cloned()
            .collect()
    }

    /// Forget everything.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Panic unless `event` was broadcast on `channel`.
    ///
    /// # Panics
    ///
    /// If it was not — with what *was* sent, because "nothing matched" is
    /// rarely the useful half of the message.
    pub fn assert_broadcast(&self, event: &str, channel: &str) {
        let sent = self.sent();
        assert!(
            sent.iter().any(|b| {
                b.event == event && b.channels.iter().any(|c| c.wire_name() == channel)
            }),
            "expected `{event}` on `{channel}`, but what was sent was {:?}",
            sent.iter()
                .map(|b| (
                    b.event.clone(),
                    b.channels.iter().map(Channel::wire_name).collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>()
        );
    }

    /// Panic unless exactly `times` broadcasts were published.
    ///
    /// # Panics
    ///
    /// If the count differs.
    pub fn assert_broadcast_times(&self, times: usize) {
        let actual = self.len();
        assert_eq!(actual, times, "expected {times} broadcast(s), but {actual} were published");
    }

    /// Panic unless nothing was broadcast.
    ///
    /// # Panics
    ///
    /// If anything was.
    pub fn assert_nothing_broadcast(&self) {
        assert!(self.is_empty(), "expected no broadcasts, but {:?} were sent", self.sent());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Broadcast>> {
        self.sent.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Broadcaster for MemoryBroadcaster {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn publish<'a>(&'a self, broadcast: &'a Broadcast) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.lock().push(broadcast.clone());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Broadcastable;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Ping {
        room: u64,
    }

    impl Broadcastable for Ping {
        fn broadcast_on(&self) -> Vec<Channel> {
            vec![Channel::private(format!("room.{}", self.room))]
        }
    }

    #[tokio::test]
    async fn the_memory_broadcaster_records_what_it_was_given() {
        let broadcaster = MemoryBroadcaster::new();
        let broadcast = Broadcast::of(&Ping { room: 1 }).unwrap();

        broadcaster.publish(&broadcast).await.unwrap();

        broadcaster.assert_broadcast("Ping", "private-room.1");
        broadcaster.assert_broadcast_times(1);
        assert_eq!(broadcaster.on("private-room.1").len(), 1);
    }

    #[tokio::test]
    async fn the_log_broadcaster_publishes_nothing_and_succeeds() {
        // The property that makes it a safe default.
        let broadcast = Broadcast::of(&Ping { room: 1 }).unwrap();
        assert!(LogBroadcaster.publish(&broadcast).await.is_ok());
    }

    #[test]
    #[should_panic(expected = "expected `Pong` on `private-room.1`")]
    fn an_assertion_that_did_not_happen_says_what_did() {
        MemoryBroadcaster::new().assert_broadcast("Pong", "private-room.1");
    }

    #[test]
    fn a_driver_with_nothing_to_sign_still_answers_the_auth_endpoint() {
        let response =
            LogBroadcaster.auth_response("1.1", &Channel::private("room.1"), None).unwrap();
        assert_eq!(response, json!({}));

        let member = json!({ "user_id": 7 });
        let presence = LogBroadcaster
            .auth_response("1.1", &Channel::presence("room.1"), Some(&member))
            .unwrap();
        assert_eq!(presence["channel_data"], member);
    }
}
