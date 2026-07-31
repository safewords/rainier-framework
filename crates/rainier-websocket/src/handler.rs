//! What an application writes — [`WebSocketHandler`] and the route table.

use std::collections::HashMap;
use std::sync::Arc;

use rainier_http::Request;
use rainier_support::Result;

use crate::message::Message;
use crate::socket::Socket;

/// One WebSocket endpoint.
///
/// ```ignore
/// pub struct Chat;
///
/// #[async_trait]
/// impl WebSocketHandler for Chat {
///     async fn on_message(&self, socket: &Socket, message: Message) -> Result<()> {
///         socket.send(format!("you said: {}", message.as_text().unwrap_or("")))
///     }
/// }
/// ```
///
/// # The three hooks
///
/// `on_connect` runs once, after the handshake. `on_message` runs per frame.
/// `on_close` runs once, **whatever ended the connection** — a clean close, a
/// dropped TCP connection, or an error from one of the other two. That last
/// guarantee is the one worth relying on: a room registry that only cleaned up
/// on a polite goodbye would leak an entry for every client that closed its
/// laptop.
///
/// # Errors
///
/// An error from `on_connect` or `on_message` closes the connection and is
/// logged. It is not sent to the client — the same rule a
/// [5xx](../rainier_http/index.html) follows, and for the same reason: an
/// error message is written for you, not for whoever is connected.
#[async_trait::async_trait]
pub trait WebSocketHandler: Send + Sync + 'static {
    /// A new client has finished the handshake.
    ///
    /// The place to greet them, register them in a room, or close the
    /// connection because they should not be here.
    async fn on_connect(&self, socket: &Socket) -> Result<()> {
        let _ = socket;
        Ok(())
    }

    /// A frame arrived.
    async fn on_message(&self, socket: &Socket, message: Message) -> Result<()>;

    /// The connection has ended, however it ended.
    ///
    /// Runs exactly once per connection. Cannot fail, because there is nothing
    /// left to fail into — clean up and log.
    async fn on_close(&self, socket: &Socket) {
        let _ = socket;
    }

    /// Whether this request may open a socket at all.
    ///
    /// Runs **before the handshake**, with the HTTP request that asked to
    /// upgrade — so it has the headers, the cookies and whatever the
    /// middleware put in the extensions. Returning `false` answers `403` and
    /// no socket is created.
    ///
    /// The default allows everyone, which is right for a public feed and
    /// wrong for anything else. A socket is a route: it needs the same
    /// thought about who may reach it.
    fn authorize(&self, request: &Request) -> bool {
        let _ = request;
        true
    }
}

/// A matched route: the handler, and what its pattern captured.
pub type Matched = (Arc<dyn WebSocketHandler>, Vec<(String, String)>);

/// A handler under a path pattern.
struct Route {
    pattern: String,
    segments: Vec<Segment>,
    handler: Arc<dyn WebSocketHandler>,
}

#[derive(Debug, Clone, PartialEq)]
enum Segment {
    Literal(String),
    Capture(String),
}

/// The WebSocket routes an application declares — `routes/ws.rs`.
///
/// Separate from the HTTP router because the two answer different things: an
/// HTTP route returns a response and is done, a socket route starts a
/// conversation. Sharing one table would mean one of them pretending.
///
/// ```ignore
/// pub fn routes() -> WebSocketRoutes {
///     WebSocketRoutes::new()
///         .add("/ws/rooms/{room}", Chat::new(rooms))
///         .add("/ws/notifications", Notifications)
/// }
/// ```
#[derive(Default)]
pub struct WebSocketRoutes {
    routes: Vec<Route>,
}

impl WebSocketRoutes {
    /// No routes. Every upgrade is a `404` until something is declared.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve `handler` at `pattern`.
    ///
    /// `{name}` captures a segment, readable from
    /// [`Socket::param`](crate::Socket::param). The first matching pattern
    /// wins, so declare the specific before the general.
    pub fn add(mut self, pattern: impl Into<String>, handler: impl WebSocketHandler) -> Self {
        self.push(pattern, Arc::new(handler));
        self
    }

    /// Serve a handler you already share.
    pub fn add_arc(
        mut self,
        pattern: impl Into<String>,
        handler: Arc<dyn WebSocketHandler>,
    ) -> Self {
        self.push(pattern, handler);
        self
    }

    fn push(&mut self, pattern: impl Into<String>, handler: Arc<dyn WebSocketHandler>) {
        let pattern = pattern.into();
        self.routes.push(Route { segments: parse_pattern(&pattern), pattern, handler });
    }

    /// The handler for `path`, and whatever its pattern captured.
    pub fn match_path(&self, path: &str) -> Option<Matched> {
        self.routes.iter().find_map(|route| {
            match_segments(&route.segments, path).map(|params| (Arc::clone(&route.handler), params))
        })
    }

    /// The declared patterns, for `route:list`.
    pub fn patterns(&self) -> Vec<&str> {
        self.routes.iter().map(|route| route.pattern.as_str()).collect()
    }

    /// How many are declared.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether none are.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

impl std::fmt::Debug for WebSocketRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketRoutes").field("patterns", &self.patterns()).finish()
    }
}

fn parse_pattern(pattern: &str) -> Vec<Segment> {
    pattern
        .trim_matches('/')
        .split('/')
        .map(|segment| match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => Segment::Capture(name.to_string()),
            None => Segment::Literal(segment.to_string()),
        })
        .collect()
}

/// Match a path, exactly — segment for segment.
///
/// A pattern does not swallow extra segments: `/ws/rooms/{room}` is not
/// `/ws/rooms/7/messages`. A route that matched more than it named would
/// serve paths its author never considered.
fn match_segments(segments: &[Segment], path: &str) -> Option<Vec<(String, String)>> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() != segments.len() {
        return None;
    }

    let mut params = Vec::new();
    for (segment, part) in segments.iter().zip(parts) {
        match segment {
            Segment::Literal(literal) if literal == part => {}
            Segment::Literal(_) => return None,
            Segment::Capture(_) if part.is_empty() => return None,
            Segment::Capture(name) => params.push((name.clone(), part.to_string())),
        }
    }
    Some(params)
}

/// Every socket currently connected, grouped by a key you choose.
///
/// The thing every non-trivial socket application needs and none of them want
/// to write twice: a chat needs the room's members, a live dashboard needs
/// everyone watching one account.
///
/// ```ignore
/// rooms.join("lobby", socket.clone());
/// rooms.send("lobby", Message::text("someone arrived"));
/// rooms.leave("lobby", socket.id());
/// ```
///
/// Handles to sockets that have gone away are dropped on the next send rather
/// than reaped on a timer, so a client that vanished without a close frame
/// costs one failed send and then nothing.
#[derive(Debug, Default)]
pub struct Rooms {
    members: std::sync::Mutex<HashMap<String, Vec<Socket>>>,
}

impl Rooms {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `socket` to `room`.
    pub fn join(&self, room: impl Into<String>, socket: Socket) {
        let mut members = self.lock();
        let room = members.entry(room.into()).or_default();

        // Joining twice would deliver twice, and a client that reconnects on
        // the same id is the ordinary way that happens.
        if !room.iter().any(|existing| existing.id() == socket.id()) {
            room.push(socket);
        }
    }

    /// Remove one socket from `room`.
    pub fn leave(&self, room: &str, socket: crate::socket::SocketId) {
        let mut members = self.lock();
        if let Some(room_members) = members.get_mut(room) {
            room_members.retain(|existing| existing.id() != socket);
        }
    }

    /// Remove a socket from **every** room.
    ///
    /// What `on_close` should call: a handler that tracked which rooms a
    /// socket was in would be keeping a second copy of this map.
    pub fn leave_all(&self, socket: crate::socket::SocketId) {
        let mut members = self.lock();
        for room in members.values_mut() {
            room.retain(|existing| existing.id() != socket);
        }
        members.retain(|_, room| !room.is_empty());
    }

    /// Send to everyone in `room`. Returns how many received it.
    pub fn send(&self, room: &str, message: impl Into<Message>) -> usize {
        self.deliver(room, message.into(), None)
    }

    /// Send to everyone in `room` **except** one socket — the one that caused
    /// the message, which has usually shown it already.
    pub fn send_except(
        &self,
        room: &str,
        except: crate::socket::SocketId,
        message: impl Into<Message>,
    ) -> usize {
        self.deliver(room, message.into(), Some(except))
    }

    /// How many sockets are in `room`.
    pub fn count(&self, room: &str) -> usize {
        self.lock().get(room).map_or(0, Vec::len)
    }

    /// The rooms that have anyone in them.
    pub fn rooms(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    fn deliver(
        &self,
        room: &str,
        message: Message,
        except: Option<crate::socket::SocketId>,
    ) -> usize {
        let mut members = self.lock();
        let Some(room_members) = members.get_mut(room) else { return 0 };

        let mut delivered = 0;
        room_members.retain(|socket| {
            if Some(socket.id()) == except {
                return true;
            }
            match socket.send(message.clone()) {
                Ok(()) => {
                    delivered += 1;
                    true
                }
                // Gone. Drop the handle here rather than sweeping later.
                Err(_) => false,
            }
        });
        delivered
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Socket>>> {
        self.members.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::{Outbound, SocketId};
    use tokio::sync::mpsc;

    struct Echo;

    #[async_trait::async_trait]
    impl WebSocketHandler for Echo {
        async fn on_message(&self, socket: &Socket, message: Message) -> Result<()> {
            socket.send(message)
        }
    }

    fn socket(path: &str) -> (Socket, mpsc::UnboundedReceiver<Outbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Socket::new(SocketId::next(), path, Vec::new(), tx), rx)
    }

    #[test]
    fn a_pattern_captures_its_segments() {
        let routes = WebSocketRoutes::new().add("/ws/rooms/{room}", Echo);

        let (_, params) = routes.match_path("/ws/rooms/lobby").expect("matches");
        assert_eq!(params, vec![("room".to_string(), "lobby".to_string())]);
    }

    #[test]
    fn a_pattern_does_not_swallow_extra_segments() {
        let routes = WebSocketRoutes::new().add("/ws/rooms/{room}", Echo);

        assert!(routes.match_path("/ws/rooms/lobby/messages").is_none());
        assert!(routes.match_path("/ws/rooms").is_none());
    }

    #[test]
    fn an_undeclared_path_matches_nothing() {
        let routes = WebSocketRoutes::new().add("/ws/chat", Echo);
        assert!(routes.match_path("/ws/other").is_none());
    }

    #[test]
    fn the_first_matching_pattern_wins() {
        let routes =
            WebSocketRoutes::new().add("/ws/rooms/lobby", Echo).add("/ws/rooms/{room}", Echo);

        let (_, params) = routes.match_path("/ws/rooms/lobby").expect("matches");
        assert!(params.is_empty(), "the literal route should win");
    }

    #[tokio::test]
    async fn a_room_delivers_to_its_members() {
        let rooms = Rooms::new();
        let (first, mut first_rx) = socket("/ws");
        let (second, mut second_rx) = socket("/ws");

        rooms.join("lobby", first.clone());
        rooms.join("lobby", second);

        assert_eq!(rooms.send("lobby", "hello"), 2);
        assert!(first_rx.try_recv().is_ok());
        assert!(second_rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn send_except_skips_the_sender() {
        let rooms = Rooms::new();
        let (first, mut first_rx) = socket("/ws");
        let (second, mut second_rx) = socket("/ws");
        let sender = first.id();

        rooms.join("lobby", first);
        rooms.join("lobby", second);

        assert_eq!(rooms.send_except("lobby", sender, "hello"), 1);
        assert!(first_rx.try_recv().is_err(), "the sender should not get their own message");
        assert!(second_rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn joining_twice_delivers_once() {
        let rooms = Rooms::new();
        let (socket, _rx) = socket("/ws");

        rooms.join("lobby", socket.clone());
        rooms.join("lobby", socket);

        assert_eq!(rooms.count("lobby"), 1);
    }

    #[tokio::test]
    async fn a_socket_that_went_away_is_dropped_on_the_next_send() {
        let rooms = Rooms::new();
        let (alive, _alive_rx) = socket("/ws");
        let (gone, gone_rx) = socket("/ws");

        rooms.join("lobby", alive);
        rooms.join("lobby", gone);
        drop(gone_rx);

        assert_eq!(rooms.send("lobby", "hello"), 1, "only the live one received it");
        assert_eq!(rooms.count("lobby"), 1, "and the dead handle is gone");
    }

    #[tokio::test]
    async fn leaving_every_room_is_one_call() {
        let rooms = Rooms::new();
        let (socket, _rx) = socket("/ws");
        let id = socket.id();

        rooms.join("a", socket.clone());
        rooms.join("b", socket);
        rooms.leave_all(id);

        assert_eq!(rooms.count("a"), 0);
        assert!(rooms.rooms().is_empty(), "empty rooms are forgotten");
    }
}
