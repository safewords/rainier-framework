//! The handle a handler holds — [`Socket`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::mpsc;

use rainier_support::{Error, Result};

use crate::message::Message;

/// A unique id for one connection, for the life of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SocketId(u64);

impl SocketId {
    /// The next id. Monotonic, so ordering by it is arrival order.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// The id as a number, for logs and for keying a map.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for SocketId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// This process's share of a socket identity.
///
/// A [`SocketId`] is a counter, so replica A and replica B both have a socket
/// `7`. That is fine while the only thing reading the number is the process
/// that issued it, and wrong the moment a broadcast crosses between them: "do
/// not send this to socket 7" would silence a different browser on every other
/// replica.
///
/// So [`Socket::identity`] pairs the counter with this, which is generated once
/// per process from the clock. Two processes agreeing on it would need to have
/// started in the same microsecond.
pub fn instance_id() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

    INSTANCE.get_or_init(|| {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_micros())
            .unwrap_or_default();

        format!("{micros:x}")
    })
}

/// The socket an identity names, if it is one of **this** process's.
///
/// `None` for an identity minted by another replica, which is the answer that
/// makes a fan-out correct: that socket is not here, so there is nothing to
/// skip and everyone here should be told.
///
/// ```
/// use rainier_websocket::socket::{socket_from_identity, instance_id};
///
/// assert_eq!(socket_from_identity(&format!("{}.7", instance_id())).map(|id| id.get()), Some(7));
/// assert!(socket_from_identity("some-other-replica.7").is_none());
/// assert!(socket_from_identity("nonsense").is_none());
/// ```
pub fn socket_from_identity(identity: &str) -> Option<SocketId> {
    let (instance, id) = identity.rsplit_once('.')?;

    if instance != instance_id() {
        return None;
    }

    id.parse().ok().map(SocketId)
}

/// One connected client.
///
/// Cheap to clone and safe to keep: sending goes through a channel, so a handle
/// stored in a room registry does not hold the connection's task or its
/// buffers. Sending to a socket that has gone away is an error, not a panic.
///
/// ```ignore
/// socket.send("hello").await?;
/// socket.send_json(&Update { count: 3 }).await?;
/// socket.close().await;
/// ```
#[derive(Debug, Clone)]
pub struct Socket {
    id: SocketId,
    path: Arc<str>,
    params: Arc<Vec<(String, String)>>,
    outbound: mpsc::UnboundedSender<Outbound>,
    closed: Arc<AtomicBool>,
}

/// What the connection task pulls off the queue.
#[derive(Debug)]
pub enum Outbound {
    /// Send this frame.
    Send(Message),
    /// Close the connection, with an optional reason.
    Close(Option<String>),
}

impl Socket {
    /// Build a handle over `outbound`.
    ///
    /// The transport owns the receiving half; a handler is only ever given
    /// this side.
    pub fn new(
        id: SocketId,
        path: impl Into<Arc<str>>,
        params: Vec<(String, String)>,
        outbound: mpsc::UnboundedSender<Outbound>,
    ) -> Self {
        Self {
            id,
            path: path.into(),
            params: Arc::new(params),
            outbound,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// This connection's id.
    pub fn id(&self) -> SocketId {
        self.id
    }

    /// This connection's id, qualified so it is unique across replicas.
    ///
    /// **This is what to send the browser**, and what it should echo back in
    /// `X-Socket-ID` so [`to_others`](../rainier_broadcast/struct.Broadcasting.html#method.event_except)
    /// can skip it:
    ///
    /// ```ignore
    /// async fn on_connect(&self, socket: &Socket) -> Result<()> {
    ///     socket.send_json(&json!({ "socket_id": socket.identity() })).await
    /// }
    /// ```
    ///
    /// Sending the bare [`id`](Self::id) instead works on one replica and
    /// fails quietly on two: every replica has a socket `7`, so "everyone
    /// except 7" would silence an unrelated browser on each of the others.
    pub fn identity(&self) -> String {
        format!("{}.{}", instance_id(), self.id.get())
    }

    /// The path it connected to.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// A route parameter captured from the path — `rooms/{room}` gives
    /// `room`.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// A route parameter, parsed.
    ///
    /// A **bad request** when it will not parse: the value came from the URL
    /// the client asked for.
    pub fn parse_param<T>(&self, name: &str) -> Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        let raw = self
            .param(name)
            .ok_or_else(|| Error::internal(format!("the route captured no `{name}`")))?;

        raw.parse::<T>()
            .map_err(|e| Error::bad_request(format!("`{name}` in the path is invalid: {e}")))
    }

    /// Every captured parameter.
    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }

    /// Send a frame.
    ///
    /// Queued rather than written: a handler that awaited the socket would
    /// block on a slow client, and one slow client must not hold up the task
    /// reading everyone else's messages.
    pub fn send(&self, message: impl Into<Message>) -> Result<()> {
        self.push(Outbound::Send(message.into()))
    }

    /// Send a value as a JSON text frame.
    pub fn send_json(&self, value: &impl Serialize) -> Result<()> {
        self.send(Message::json(value)?)
    }

    /// Close the connection.
    ///
    /// Idempotent: closing twice is not an error, because two halves of a
    /// handler deciding to close at once is normal.
    pub fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.push(Outbound::Close(None))
    }

    /// Close with a reason the peer will see.
    pub fn close_with(&self, reason: impl Into<String>) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.push(Outbound::Close(Some(reason.into())))
    }

    /// Whether this handle has been asked to close, or the connection has
    /// gone.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.outbound.is_closed()
    }

    fn push(&self, outbound: Outbound) -> Result<()> {
        self.outbound.send(outbound).map_err(|_| {
            // The peer disconnected. Routine — a registry holding stale
            // handles finds out this way — so it is an error to handle, not a
            // panic and not a silent success.
            Error::internal(format!("socket {} has gone away", self.id))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket() -> (Socket, mpsc::UnboundedReceiver<Outbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Socket::new(SocketId::next(), "/ws/rooms/7", vec![("room".into(), "7".into())], tx), rx)
    }

    #[test]
    fn an_identity_is_the_process_and_the_socket() {
        let (first, _rx) = socket();
        let (second, _rx2) = socket();

        assert_ne!(first.identity(), second.identity(), "two sockets, two identities");

        // Both halves are there, and the process half is the same for both:
        // it is what stops replica B skipping *its* socket 7 when replica A
        // said "everyone except 7".
        assert!(first.identity().ends_with(&format!(".{}", first.id().get())));
        assert_eq!(
            first.identity().split('.').next(),
            second.identity().split('.').next(),
            "one process, one instance id"
        );
    }

    #[test]
    fn a_send_is_queued_not_written() {
        let (socket, mut rx) = socket();

        socket.send("hello").unwrap();

        match rx.try_recv().unwrap() {
            Outbound::Send(Message::Text(text)) => assert_eq!(text, "hello"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn route_parameters_come_through() {
        let (socket, _rx) = socket();

        assert_eq!(socket.param("room"), Some("7"));
        assert_eq!(socket.parse_param::<u64>("room").unwrap(), 7);
        assert_eq!(socket.param("nope"), None);
    }

    #[test]
    fn a_parameter_that_will_not_parse_is_a_bad_request() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let socket =
            Socket::new(SocketId::next(), "/ws/rooms/x", vec![("room".into(), "x".into())], tx);

        assert_eq!(socket.parse_param::<u64>("room").unwrap_err().status(), 400);
    }

    #[test]
    fn closing_twice_is_not_an_error() {
        let (socket, mut rx) = socket();

        socket.close().unwrap();
        socket.close().unwrap();

        assert!(matches!(rx.try_recv(), Ok(Outbound::Close(None))));
        assert!(rx.try_recv().is_err(), "only one close should be queued");
        assert!(socket.is_closed());
    }

    #[test]
    fn sending_to_a_socket_that_went_away_is_an_error_not_a_panic() {
        let (socket, rx) = socket();
        drop(rx);

        let err = socket.send("hello").unwrap_err();
        assert!(err.message().contains("gone away"), "{}", err.message());
        assert!(socket.is_closed());
    }

    #[test]
    fn ids_are_unique_and_ordered() {
        let first = SocketId::next();
        let second = SocketId::next();

        assert!(second > first);
    }
}
