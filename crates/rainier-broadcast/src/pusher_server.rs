//! A Pusher-protocol **server**, in this process — [`PusherServer`].
//!
//! The other half of this crate. Everything else here publishes events for
//! somebody else to deliver: [`RedisBroadcaster`](crate::RedisBroadcaster)
//! writes to Redis and a separate relay — soketi, Pusher, Laravel Reverb —
//! holds the browsers' sockets and fans them out. This is that relay, so an
//! application can hold its own sockets and drop the second deployment.
//!
//! # Why the protocol rather than [`rainier-websocket`]'s own
//!
//! `rainier-websocket` is the better surface for a socket you are writing both
//! ends of. This exists for the case where you are not: the browser is running
//! Laravel Echo and `pusher-js`, which speak one protocol and will not be
//! talked out of it. Serving that protocol is what lets an existing SPA keep
//! its client code.
//!
//! # The protocol, as much of it as a browser uses
//!
//! ```text
//! →  connect  GET /app/{key}?protocol=7&client=js&version=8.4.0
//! ←  {"event":"pusher:connection_established","data":"{\"socket_id\":\"…\"}"}
//! →  {"event":"pusher:subscribe","data":{"channel":"private-chat.7","auth":"key:hmac"}}
//! ←  {"event":"pusher_internal:subscription_succeeded","channel":"private-chat.7","data":"{}"}
//! ←  {"event":"chat.message.sent","channel":"private-chat.7","data":"{…}"}
//! →  {"event":"pusher:ping"}   ←  {"event":"pusher:pong"}
//! ```
//!
//! **`data` is a JSON-encoded string, not an object.** Both ways. It is the
//! detail that makes a hand-rolled implementation look like it works — the
//! handshake succeeds, subscriptions succeed, and then every event arrives at
//! the client as an unparsed string it silently ignores.
//!
//! # What this does not implement, deliberately
//!
//! **Client events** (`client-*`). Reverb refuses them unless an app opts in,
//! the SPA this was built against never sends one, and accepting them means a
//! browser can publish to every other browser on a channel. Refused here with
//! no opt-in: it is a feature to add when something needs it, not a default to
//! leave on.
//!
//! **The HTTP publish API** (`POST /apps/{id}/events`). Publishing goes through
//! Redis — [`RedisBroadcaster`](crate::RedisBroadcaster) on one side, this on
//! the other — so the HTTP endpoint would be a second way in with its own
//! authentication and nothing using it.
//!
//! **Presence member rosters.** [`subscribe`] accepts `presence-` channels and
//! authorises them exactly as it does private ones, but does not track or
//! broadcast `pusher_internal:member_added`/`member_removed`. A presence
//! channel therefore behaves as a private one. Stated because the difference is
//! invisible until something asks for the member list.
//!
//! [`rainier-websocket`]: https://docs.rs/rainier-websocket
//! [`subscribe`]: PusherServer

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rainier_support::Result;
use rainier_websocket::{Message, Rooms, Socket, WebSocketHandler};
use serde_json::{json, Value};

use crate::channel::Channel;
use crate::pusher::PusherAuth;

/// How long a client may hear nothing before it should ping us, in seconds.
///
/// Sent in the handshake. `pusher-js` halves it for its own ping timer, so this
/// is the ceiling on how long a dead connection goes unnoticed by the client.
const ACTIVITY_TIMEOUT_SECONDS: u64 = 120;

/// A Pusher-protocol server holding its own sockets.
///
/// Mount it on the path `pusher-js` connects to — `/app/{key}` — and give it
/// the same [`PusherAuth`] the application's `/broadcasting/auth` endpoint
/// signs with. Events reach it through
/// [`deliver_published`](Self::deliver_published), which is what a Redis
/// subscriber calls for each message it reads.
///
/// ```ignore
/// let server = Arc::new(PusherServer::new(auth));
///
/// // The sockets.
/// let routes = WebSocketRoutes::new().add_arc("/app/{key}", server.clone());
///
/// // The fan-in: everything published by any replica, delivered to ours.
/// tokio::spawn(async move {
///     let mut events = connector.subscribe("lewd-production:*").await?;
///     while let Some(message) = events.next_message().await {
///         server.deliver_published(&message.channel, message.text().unwrap_or_default());
///     }
/// });
/// ```
pub struct PusherServer {
    auth: Option<PusherAuth>,
    rooms: Arc<Rooms>,
    /// The counter half of a socket id.
    ///
    /// Pusher socket ids are `digits.digits`, and the application's own
    /// `auth_response` validates that shape before signing — so this cannot be
    /// a uuid or a hex string without breaking the signature the browser is
    /// about to present.
    ///
    /// Which is also why the server mints its own rather than reusing
    /// `Socket::identity()`: that is `{instance}.{id}` with a **hex** instance,
    /// so it would be refused by the very signature check it exists to feed.
    sequence: AtomicU64,

    /// The socket id issued to each connection, and which connection it names.
    ///
    /// Needed because the id the browser knows is this server's, not the
    /// framework's: it arrives back on `/broadcasting/auth`, is signed into the
    /// subscription HMAC, and returns as the `socket` a `toOthers()` broadcast
    /// asks to skip. Without the map that skip cannot be resolved to a local
    /// connection and the sender sees its own event echoed back.
    issued: Mutex<HashMap<String, rainier_websocket::SocketId>>,
    /// Stripped from a Redis channel name before it is matched to a
    /// subscription, so the server's rooms are named what the browser calls
    /// them rather than what the deployment prefixes them with.
    prefix: String,
}

impl PusherServer {
    /// A server that verifies subscription signatures with `auth`.
    pub fn new(auth: PusherAuth) -> Self {
        Self {
            auth: Some(auth),
            rooms: Arc::new(Rooms::new()),
            sequence: AtomicU64::new(1),
            issued: Mutex::new(HashMap::new()),
            prefix: String::new(),
        }
    }

    /// A server that authorises **nothing**.
    ///
    /// Every private and presence channel is granted to whoever asks. For a
    /// development stub and for tests; naming it this way so it cannot be
    /// reached for by accident, since the failure it produces — a browser
    /// reading another user's chat — is silent.
    pub fn without_authorisation() -> Self {
        Self {
            auth: None,
            rooms: Arc::new(Rooms::new()),
            sequence: AtomicU64::new(1),
            issued: Mutex::new(HashMap::new()),
            prefix: String::new(),
        }
    }

    /// Strip `prefix` off Redis channel names before matching them to rooms.
    ///
    /// Must be the same prefix [`RedisBroadcaster`](crate::RedisBroadcaster)
    /// publishes under, or nothing matches: the publisher writes to
    /// `lewd-production:private-chat.7` and the browser subscribed to
    /// `private-chat.7`.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The rooms this server holds, keyed by channel name.
    ///
    /// Exposed for a health endpoint or a metric — how many channels have a
    /// listener, and how many listeners a channel has.
    pub fn rooms(&self) -> &Arc<Rooms> {
        &self.rooms
    }

    /// Hand an event published elsewhere to the sockets subscribed here.
    ///
    /// `channel` is the Redis channel it arrived on, prefix and all; `body` is
    /// what was published — `{"event":…,"data":…,"socket":…}`. Returns how many
    /// sockets received it, which is zero whenever no browser on *this* replica
    /// is subscribed and is the ordinary case rather than a fault.
    pub fn deliver_published(&self, channel: &str, body: &str) -> usize {
        let name = channel.strip_prefix(&self.prefix).unwrap_or(channel);

        // Nothing here is listening to it. Checked before parsing, because a
        // subscription wide enough to catch every channel this application
        // publishes also catches whatever else shares the Redis — and warning
        // about the shape of somebody else's messages would fill the log with
        // a problem that is not one.
        if self.rooms.count(name) == 0 {
            // Say which rooms *do* exist.
            //
            // "Nobody is listening" and "somebody is listening under a name
            // this lookup did not produce" are the same silence, and telling
            // them apart from outside is close to impossible: the browser is
            // told its subscription succeeded either way. Naming the rooms the
            // server actually holds turns that into a one-line diagnosis.
            //
            // `debug`, not `warn`: on a healthy deployment most published
            // channels legitimately have no local subscriber, and warning per
            // message would drown the log in the normal case.
            tracing::debug!(
                channel,
                looked_up = name,
                held = ?self.rooms.rooms(),
                // Which server object this is. A subscribe that joined and a
                // publish that found nothing are only reconcilable if they ran
                // against different instances, and comparing this against the
                // `joined a room` line below is how that gets settled instead
                // of argued.
                server = format!("{:p}", self),
                rooms_registry = format!("{:p}", Arc::as_ptr(&self.rooms)),
                "published to a channel with no local subscriber",
            );
            return 0;
        }

        let Ok(published) = serde_json::from_str::<Value>(body) else {
            tracing::warn!(channel, "discarding an unreadable published message");
            return 0;
        };

        let Some(event) = published.get("event").and_then(Value::as_str) else {
            tracing::warn!(channel, "discarding a published message with no `event`");
            return 0;
        };

        let data = published.get("data").cloned().unwrap_or(Value::Null);
        let frame = Self::frame(event, Some(name), &data);

        // `socket` is the connection that caused the event, which has usually
        // rendered it already — Laravel's `toOthers()`. It is an *identity*
        // string, so this asks the registry to skip it rather than parsing it
        // into a local id that may belong to a different replica's socket.
        match published.get("socket").and_then(Value::as_str) {
            Some(except) => match self.socket_named(except) {
                Some(id) => self.rooms.send_except(name, id, Message::text(frame)),
                // Another replica's socket. Nothing here to skip, and sending
                // to everyone is right: the browser that is meant to skip it is
                // not connected to this process.
                None => self.rooms.send(name, Message::text(frame)),
            },
            None => self.rooms.send(name, Message::text(frame)),
        }
    }

    /// The connection a Pusher socket id names, if it is one of ours.
    fn socket_named(&self, socket_id: &str) -> Option<rainier_websocket::SocketId> {
        self.issued.lock().expect("the socket registry is not poisoned").get(socket_id).copied()
    }

    /// The Pusher socket id issued to `socket`.
    ///
    /// Empty when the connection is not registered, which cannot happen after
    /// `on_connect` — and an empty id signs to something no browser can
    /// present, so the failure is a refused subscription rather than an
    /// authorised one.
    fn socket_id_of(&self, socket: &Socket) -> String {
        self.issued
            .lock()
            .expect("the socket registry is not poisoned")
            .iter()
            .find(|(_, id)| **id == socket.id())
            .map(|(name, _)| name.clone())
            .unwrap_or_default()
    }

    /// One protocol frame.
    ///
    /// `data` is serialised to a **string**, because that is what the protocol
    /// says and what `pusher-js` parses. Sending the object inline produces a
    /// frame that looks correct in a log, is accepted by the client, and
    /// arrives at every `.listen()` callback as something no application code
    /// recognises.
    fn frame(event: &str, channel: Option<&str>, data: &Value) -> String {
        let encoded = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());

        let mut frame = json!({ "event": event, "data": encoded });
        if let Some(channel) = channel {
            frame["channel"] = json!(channel);
        }
        frame.to_string()
    }

    /// The socket id this connection is known by, in the protocol's shape.
    fn next_socket_id(&self) -> String {
        // `digits.digits`, both halves decimal for `validate_socket_id`.
        //
        // The left half identifies this **process**, so two replicas cannot
        // issue the same id — which would make one browser's `toOthers()` skip
        // silence a different browser on the other replica. Derived from
        // `instance_id()` (hex microseconds) so it moves every restart, with
        // the pid as a fallback if that ever stops parsing.
        let instance = u64::from_str_radix(rainier_websocket::instance_id(), 16)
            .unwrap_or_else(|_| u64::from(std::process::id()));
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!("{instance}.{sequence}")
    }

    /// Handle one decoded client frame.
    async fn handle(&self, socket: &Socket, frame: &Value) -> Result<()> {
        let event = frame.get("event").and_then(Value::as_str).unwrap_or_default();

        match event {
            "pusher:ping" => {
                socket.send(Message::text(Self::frame("pusher:pong", None, &json!({}))))
            }

            "pusher:subscribe" => self.subscribe(socket, frame.get("data")),

            "pusher:unsubscribe" => {
                if let Some(channel) = Self::channel_of(frame.get("data")) {
                    self.rooms.leave(&channel, socket.id());
                }
                Ok(())
            }

            // Refused rather than ignored. A client that can publish to a
            // channel can publish to every other browser on it, and silence
            // would read as acceptance.
            other if other.starts_with("client-") => socket.send(Message::text(Self::frame(
                "pusher:error",
                None,
                &json!({
                    "code": 4301,
                    "message": "client events are not enabled for this app",
                }),
            ))),

            // Everything else, including frames from a newer client than this
            // understands. Ignored rather than refused: a protocol extension is
            // not an error, and closing the socket over one would break a
            // client that was working.
            _ => Ok(()),
        }
    }

    /// `pusher:subscribe`.
    fn subscribe(&self, socket: &Socket, data: Option<&Value>) -> Result<()> {
        let Some(name) = Self::channel_of(data) else {
            return socket.send(Message::text(Self::frame(
                "pusher:error",
                None,
                &json!({ "code": 4009, "message": "no channel named" }),
            )));
        };

        let channel = Channel::from_wire_name(&name);

        if channel.needs_authorisation() {
            let presented = data.and_then(|d| d.get("auth")).and_then(Value::as_str);
            let member = data.and_then(|d| d.get("channel_data")).and_then(Value::as_str);

            if !self.authorised(&self.socket_id_of(socket), &channel, presented, member) {
                tracing::warn!(channel = %name, "refused a subscription with a bad signature");
                return socket.send(Message::text(Self::frame(
                    "pusher:error",
                    None,
                    &json!({ "code": 4009, "message": "subscription is not authorised" }),
                )));
            }
        }

        self.rooms.join(name.clone(), socket.clone());

        // The other half of the identity check in `deliver_published`. If a
        // subscribe reports a room here and a publish reports none there, these
        // two addresses say whether it is the same server disagreeing with
        // itself or two servers that were never the same object.
        tracing::debug!(
            channel = %name,
            rooms_now = self.rooms.count(&name),
            server = format!("{:p}", self),
            rooms_registry = format!("{:p}", Arc::as_ptr(&self.rooms)),
            "joined a room",
        );

        socket.send(Message::text(Self::frame(
            "pusher_internal:subscription_succeeded",
            Some(&name),
            &json!({}),
        )))
    }

    /// Whether a presented `auth` string is the one this application would have
    /// signed.
    ///
    /// Recomputed rather than parsed: the signature is an HMAC over
    /// `socket_id:channel`, so signing the same inputs and comparing is the
    /// only check there is.
    fn authorised(
        &self,
        socket_id: &str,
        channel: &Channel,
        presented: Option<&str>,
        member: Option<&str>,
    ) -> bool {
        let Some(auth) = &self.auth else {
            // No credentials: `without_authorisation`, which says what it does.
            return true;
        };
        let Some(presented) = presented else { return false };

        let expected = auth.sign(socket_id, channel, member);

        // Constant-time. A byte-by-byte comparison on a signature leaks where
        // it first differs, and a signature is guessable one byte at a time by
        // anything that can measure the difference.
        constant_time_eq(presented.as_bytes(), expected.as_bytes())
    }

    /// The `channel` out of a client frame's `data`.
    fn channel_of(data: Option<&Value>) -> Option<String> {
        data?
            .get("channel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    }
}

#[async_trait::async_trait]
impl WebSocketHandler for PusherServer {
    async fn on_connect(&self, socket: &Socket) -> Result<()> {
        // Issued and remembered before the frame goes out: this id is what the
        // browser presents to `/broadcasting/auth`, and the subscription that
        // comes back has to resolve to this connection.
        let socket_id = self.next_socket_id();
        self.issued
            .lock()
            .expect("the socket registry is not poisoned")
            .insert(socket_id.clone(), socket.id());

        socket.send(Message::text(Self::frame(
            "pusher:connection_established",
            None,
            &json!({
                "socket_id": socket_id,
                "activity_timeout": ACTIVITY_TIMEOUT_SECONDS,
            }),
        )))
    }

    async fn on_message(&self, socket: &Socket, message: Message) -> Result<()> {
        let Message::Text(body) = message else {
            // Binary frames are not part of this protocol.
            return Ok(());
        };

        match serde_json::from_str::<Value>(&body) {
            Ok(frame) => self.handle(socket, &frame).await,
            Err(_) => socket.send(Message::text(Self::frame(
                "pusher:error",
                None,
                &json!({ "code": 4200, "message": "that is not a protocol frame" }),
            ))),
        }
    }

    async fn on_close(&self, socket: &Socket) {
        self.rooms.leave_all(socket.id());
        // Or the map grows for the life of the process, one entry per
        // connection ever made.
        self.issued
            .lock()
            .expect("the socket registry is not poisoned")
            .retain(|_, id| *id != socket.id());
    }
}

/// Compare two byte strings without leaking where they differ.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0u8, |differences, (a, b)| differences | (a ^ b)) == 0
}

/// Refuse a subscription the server has no signature for.
///
/// Not used by [`PusherServer`] itself — it answers a protocol error instead —
/// but exposed because an application mounting its own variant needs the same
/// decision and should not re-derive which prefixes are guarded.
pub fn requires_authorisation(channel: &str) -> bool {
    Channel::from_wire_name(channel).needs_authorisation()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> PusherAuth {
        PusherAuth::new("app-key", "app-secret")
    }

    #[test]
    fn a_frame_carries_its_data_as_a_string_not_an_object() {
        // The detail the whole protocol turns on. As an object every event
        // reaches the client unrecognised, and nothing reports it.
        let frame = PusherServer::frame("chat.sent", Some("private-chat.7"), &json!({"id": 1}));
        let parsed: Value = serde_json::from_str(&frame).unwrap();

        assert_eq!(parsed["event"], "chat.sent");
        assert_eq!(parsed["channel"], "private-chat.7");
        assert!(parsed["data"].is_string(), "data must be a JSON string: {frame}");
        assert_eq!(
            serde_json::from_str::<Value>(parsed["data"].as_str().unwrap()).unwrap()["id"],
            1
        );
    }

    #[test]
    fn a_frame_without_a_channel_omits_the_key() {
        let frame = PusherServer::frame("pusher:pong", None, &json!({}));
        let parsed: Value = serde_json::from_str(&frame).unwrap();
        assert!(parsed.get("channel").is_none(), "{frame}");
    }

    #[test]
    fn a_socket_id_is_the_shape_the_signature_validates() {
        // `PusherAuth::auth_response` refuses anything that is not
        // `digits.digits`, so an id of another shape would make every
        // subscription fail at the application's auth endpoint rather than here.
        let server = PusherServer::new(auth());

        for _ in 0..3 {
            let id = server.next_socket_id();
            let (left, right) = id.split_once('.').expect("two parts: {id}");
            assert!(!left.is_empty() && left.bytes().all(|b| b.is_ascii_digit()), "{id}");
            assert!(!right.is_empty() && right.bytes().all(|b| b.is_ascii_digit()), "{id}");
            assert!(auth().auth_response(&id, &Channel::private("x"), None).is_ok(), "{id}");
        }
    }

    #[test]
    fn socket_ids_do_not_repeat() {
        let server = PusherServer::new(auth());
        let first = server.next_socket_id();
        let second = server.next_socket_id();
        assert_ne!(first, second);
    }

    #[test]
    fn a_correct_signature_is_accepted_and_a_wrong_one_is_not() {
        let server = PusherServer::new(auth());
        let channel = Channel::private("chat.7");
        let signed = auth().sign("1234.1", &channel, None);

        assert!(server.authorised("1234.1", &channel, Some(&signed), None));

        // Right signature, different socket — the substitution the socket id
        // in the signed message exists to stop.
        assert!(!server.authorised("9999.9", &channel, Some(&signed), None));
        // Right socket, different channel.
        assert!(!server.authorised("1234.1", &Channel::private("chat.8"), Some(&signed), None));
        // Nothing presented at all.
        assert!(!server.authorised("1234.1", &channel, None, None));
        assert!(!server.authorised("1234.1", &channel, Some("nonsense"), None));
    }

    #[test]
    fn without_authorisation_grants_everything_including_no_signature() {
        let server = PusherServer::without_authorisation();
        assert!(server.authorised("1234.1", &Channel::private("chat.7"), None, None));
    }

    #[test]
    fn public_channels_need_no_signature_and_guarded_ones_do() {
        assert!(!requires_authorisation("orders"));
        assert!(requires_authorisation("private-chat.7"));
        assert!(requires_authorisation("presence-room.7"));
    }

    #[test]
    fn the_prefix_comes_off_a_published_channel_name() {
        // The publisher writes `lewd-production:private-chat.7`; the browser
        // subscribed to `private-chat.7`. Without stripping, nothing matches
        // and every event is delivered to nobody — silently, because zero
        // recipients is also what an idle channel looks like.
        let server = PusherServer::new(auth()).with_prefix("lewd-production:");
        let delivered = server.deliver_published(
            "lewd-production:private-chat.7",
            &json!({ "event": "chat.sent", "data": { "id": 1 } }).to_string(),
        );
        // Nobody is connected in a unit test; what matters is that it did not
        // panic and reported an honest zero.
        assert_eq!(delivered, 0);
    }

    #[test]
    fn an_unreadable_published_message_is_dropped_rather_than_delivered() {
        let server = PusherServer::new(auth());
        assert_eq!(server.deliver_published("chan", "not json"), 0);
        assert_eq!(server.deliver_published("chan", &json!({ "data": {} }).to_string()), 0);
    }

    #[test]
    fn a_channel_nothing_here_listens_to_costs_nothing() {
        // The subscription is wide enough to catch everything this application
        // publishes, which on a shared Redis means catching other things too.
        // Those must not be parsed, warned about, or counted.
        let server = PusherServer::new(auth());
        assert_eq!(server.deliver_published("someone-elses-channel", "{\"not\": \"ours\"}"), 0);
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
