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
//! # What this crate is not
//!
//! **It is not a WebSocket server.** Broadcasting publishes; a separate
//! process — soketi, Pusher, any Pusher-protocol server — holds the sockets and
//! relays. That split is what lets the thing holding
//! ten thousand idle connections be neither your web server nor your language.
//!
//! What Rainier provides is the two halves an application owns: publishing
//! (over Redis with the `redis` feature, over Kafka with `kafka`, or a driver
//! you write) and
//! [authorising](ChannelRegistry) subscriptions, including the Pusher
//! protocol's [HMAC](PusherAuth).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod auth;
pub mod broadcaster;
pub mod channel;
pub mod event;
pub mod manager;
pub mod pusher;

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
