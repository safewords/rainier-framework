//! WebSockets — a connection that stays open, served alongside HTTP.
//!
//! ```ignore
//! pub struct Chat {
//!     rooms: Arc<Rooms>,
//! }
//!
//! #[async_trait]
//! impl WebSocketHandler for Chat {
//!     async fn on_connect(&self, socket: &Socket) -> Result<()> {
//!         let room = socket.param("room").unwrap_or("lobby").to_string();
//!         self.rooms.join(&room, socket.clone());
//!         Ok(())
//!     }
//!
//!     async fn on_message(&self, socket: &Socket, message: Message) -> Result<()> {
//!         let room = socket.param("room").unwrap_or("lobby");
//!         self.rooms.send_except(room, socket.id(), message);
//!         Ok(())
//!     }
//!
//!     async fn on_close(&self, socket: &Socket) {
//!         self.rooms.leave_all(socket.id());
//!     }
//! }
//!
//! // routes/ws.rs
//! WebSocketRoutes::new().add("/ws/rooms/{room}", Chat { rooms })
//! ```
//!
//! # It shares the HTTP server
//!
//! A WebSocket connection *starts* as an HTTP request — a `GET` carrying
//! `Upgrade: websocket`. So there is no second listener, no second port and no
//! second runtime: the same accept loop takes both, and a socket is a
//! connection that answered `101` instead of `200` and then kept going.
//!
//! Concurrency falls out of that rather than being arranged. Every connection
//! is already its own task, so a thousand idle sockets are a thousand parked
//! futures and cost nothing while they wait.
//!
//! ```text
//! GET /ws/rooms/7  Upgrade: websocket
//!        │
//!        ├── no handler at that path ──▶ 404, like any other route
//!        ├── handler says no ──────────▶ 403, before the handshake
//!        └── 101 Switching Protocols ──▶ on_connect → on_message* → on_close
//! ```
//!
//! # This crate is the contract, not the transport
//!
//! Nothing here speaks TCP or knows what a frame looks like on the wire —
//! [`rainier-server`](../rainier_server/index.html) does the upgrade and the
//! framing. That split is what lets a handler be tested by calling it:
//! [`Socket`] is a channel, so `on_message` can be driven with no network at
//! all.
//!
//! # Broadcasting, and when to use which
//!
//! [`rainier-broadcast`](../rainier_broadcast/index.html) also pushes to
//! browsers, and the two are not competitors:
//!
//! | | [Broadcasting](../rainier_broadcast/index.html) | This |
//! |---|---|---|
//! | Protocol | Pusher's | whatever you write |
//! | Who holds the socket | a relay, **or** your process with `pusher-server` | your process |
//! | Direction | out only | both ways |
//! | Scales across instances | yes, through Redis | one instance per socket |
//! | Client | `pusher-js`, Laravel Echo | anything that speaks WebSocket |
//!
//! The deciding question is **who writes the client**. A browser already
//! running Laravel Echo speaks one protocol and will not be talked out of it,
//! so that is broadcasting whether or not the sockets end up in your process —
//! `rainier-broadcast`'s `pusher-server` feature holds them here and still fans
//! out over Redis. Reach for this crate when both ends are yours: a bespoke
//! message shape, or a client that needs to talk back.
//!
//! Note the fourth row before choosing this one. A [`Rooms`] registry lives in
//! one process's memory, so two instances behind a load balancer have two sets
//! of rooms and neither sees the other's — where broadcasting shares
//! subscriptions through Redis and does not care which instance a browser
//! reached.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod handler;
pub mod message;
pub mod socket;

pub use handler::{Matched, Rooms, WebSocketHandler, WebSocketRoutes};
pub use message::Message;
pub use socket::{instance_id, socket_from_identity, Outbound, Socket, SocketId};
