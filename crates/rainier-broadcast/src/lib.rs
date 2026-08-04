//! Broadcasting — an event pushed to a browser, and the authorisation that
//! decides who may listen.
//!
//! Three parts: an event that says which
//! [channels](Channel) it belongs on, a [driver](Broadcaster) that publishes
//! it, and a [table of authorisers](ChannelRegistry) that a WebSocket server
//! consults before letting anyone subscribe.
//!
//! ```ignore
//! #[derive(Serialize)]
//! pub struct OrderShipped { pub order_id: u64, pub tracking: String }
//!
//! impl Broadcastable for OrderShipped {
//!     fn broadcast_on(&self) -> Vec<Channel> {
//!         vec![Channel::private(format!("orders.{}", self.order_id))]
//!     }
//! }
//!
//! Broadcast::instance().event(&OrderShipped { order_id: 7, tracking }).await?;
//! ```
//!
//! # Broadcast, event, notification
//!
//! Three things that all mean "tell someone", and the distinctions are worth
//! keeping straight because Rainier has all three:
//!
//! | | Reaches | Chosen by | Arrives |
//! |---|---|---|---|
//! | An **event** | listeners in this process | subscription, at boot | in-process, at once |
//! | A **notification** | one named recipient | `via()`, per recipient | email, SMS, a row |
//! | A **broadcast** | whoever is subscribed to a channel | the channel name | a WebSocket, now, or not at all |
//!
//! A broadcast is the only one of the three that is **best-effort and
//! ephemeral**. Nobody may be listening; a browser that reconnects a second
//! later has missed it, and nothing will replay it. That makes it right for
//! "the page should update" and wrong for anything that must have happened —
//! use a [notification](rainier_support) or a job for those, and broadcast in
//! addition if the screen should also move.
//!
//! # Publishing and delivering are separate, and only the first is required
//!
//! This crate is the **publish** side: an event happened, on a channel. What
//! turns that into bytes on a browser's socket is a deployment choice, and
//! there are two:
//!
//! | | Where the sockets live | When |
//! |---|---|---|
//! | A relay | another process — soketi, Pusher, Laravel Reverb | the connections should not be held by your web server, or something else already runs one |
//! | [`PusherServer`](pusher_server::PusherServer) | this process, behind the `pusher-server` feature | one less deployment, and the connection count is one your process can carry |
//!
//! Both consume the same publish, so moving between them changes no
//! application code — a publish goes to Redis either way, and whoever is
//! subscribed delivers it. The application's own half is the same in both
//! cases: publishing (over Redis with `redis`, Kafka with `kafka`, or a driver
//! you write) and [authorising](ChannelRegistry) subscriptions, including the
//! Pusher protocol's [HMAC](PusherAuth).
//!
//! # Not the same thing as `rainier-websocket`
//!
//! That crate is for a socket **you write both ends of** — your own message
//! shapes, two-way, no protocol to conform to. This one speaks the Pusher
//! protocol because the client is `pusher-js` or Laravel Echo and will not be
//! talked out of it. See its docs for the comparison in full.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod auth;
pub mod broadcaster;
pub mod channel;
pub mod event;
pub mod manager;
pub mod pusher;
#[cfg(feature = "pusher-server")]
pub mod pusher_server;

#[cfg(feature = "kafka")]
pub mod kafka;
#[cfg(feature = "redis")]
pub mod redis;

pub use auth::{ChannelAccess, ChannelParams, ChannelRegistry};
pub use broadcaster::{Broadcaster, LogBroadcaster, MemoryBroadcaster};
pub use channel::Channel;
pub use event::{Broadcast, Broadcastable};
pub use manager::Broadcasting;
pub use pusher::PusherAuth;

#[cfg(feature = "kafka")]
pub use kafka::{KafkaBroadcaster, KafkaRelay, RelayCursor, RelayedBroadcast};
#[cfg(feature = "redis")]
pub use redis::RedisBroadcaster;
