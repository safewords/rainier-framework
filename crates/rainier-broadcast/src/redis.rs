//! Publishing over Redis pub/sub — [`RedisBroadcaster`].
//!
//! What soketi and other Pusher-protocol relays subscribe to. Your
//! application publishes a message per channel;
//! the WebSocket server, which is a separate process, relays it to the browsers
//! subscribed there.
//!
//! ```text
//! your app ──PUBLISH private-orders.7──▶ redis ──▶ soketi ──ws──▶ browser
//! ```
//!
//! Redis is the seam that lets the two be different processes, in different
//! languages, deployed separately — which is the point. Nothing here speaks
//! WebSocket.

use std::sync::Arc;

use rainier_drivers::redis::{RedisClient, RedisConnector};
use rainier_support::{BoxFuture, Error, Result};
use serde_json::Value;

use crate::broadcaster::Broadcaster;
use crate::channel::Channel;
use crate::event::Broadcast;
use crate::pusher::PusherAuth;

/// Publishes each broadcast to its channel over Redis pub/sub.
pub struct RedisBroadcaster {
    client: RedisClient,
    prefix: String,
    auth: Option<Arc<PusherAuth>>,
}

impl RedisBroadcaster {
    /// Connect through `connector`.
    pub async fn connect(connector: &RedisConnector) -> Result<Self> {
        Ok(Self::new(RedisClient::connect(connector).await?))
    }

    /// Use a client you already have — the point of sharing one connector
    /// between the cache, the queue and this.
    pub fn new(client: RedisClient) -> Self {
        Self { client, prefix: String::new(), auth: None }
    }

    /// Prefix every published channel name.
    ///
    /// Two applications sharing a
    /// Redis need it, and getting it wrong is silent: the publish succeeds and
    /// nobody is subscribed to what was published.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Sign subscription requests with the Pusher protocol's HMAC.
    ///
    /// Needed when the WebSocket server relaying these messages is
    /// Pusher-compatible — soketi is, and so is Pusher itself — because it
    /// will not let a browser subscribe to a private channel without a
    /// signature from you.
    pub fn with_pusher_auth(mut self, auth: PusherAuth) -> Self {
        self.auth = Some(Arc::new(auth));
        self
    }

    fn channel_name(&self, channel: &Channel) -> String {
        prefixed(&self.prefix, channel)
    }
}

/// The name a message is published under: the configured prefix, then the
/// channel's wire name.
fn prefixed(prefix: &str, channel: &Channel) -> String {
    format!("{prefix}{}", channel.wire_name())
}

impl Broadcaster for RedisBroadcaster {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn publish<'a>(&'a self, broadcast: &'a Broadcast) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_string(&broadcast.wire_payload())
                .map_err(|e| Error::internal(format!("a broadcast must serialise: {e}")))?;

            // One PUBLISH per channel. Redis has no multi-channel publish, and
            // batching through a pipeline would trade a round trip for a
            // partial failure that is harder to report.
            for channel in &broadcast.channels {
                let name = self.channel_name(channel);

                let received =
                    self.client.publish(&name, body.as_bytes()).await.map_err(|e| {
                        Error::internal(format!("could not publish to `{name}`: {e}"))
                    })?;

                // Nobody subscribed is not a failure — pub/sub has no queue —
                // but it is the symptom of a mismatched prefix, so it is worth
                // being able to see.
                tracing::debug!(channel = name, subscribers = received, "published");
            }
            Ok(())
        })
    }

    fn auth_response(
        &self,
        socket_id: &str,
        channel: &Channel,
        member: Option<&Value>,
    ) -> Result<Value> {
        match &self.auth {
            Some(auth) => auth.auth_response(socket_id, channel, member),
            // No credentials configured: the relay is not checking signatures,
            // so there is nothing honest to sign with.
            None => Ok(match member {
                Some(member) => serde_json::json!({ "channel_data": member }),
                None => serde_json::json!({}),
            }),
        }
    }
}

/// Reads broadcasts back off Redis pub/sub, so a process holding sockets can
/// deliver them — the Redis twin of [`KafkaRelay`](crate::kafka::KafkaRelay).
///
/// Every replica publishes through [`RedisBroadcaster`] and every replica
/// runs one of these, so a broadcast reaches a browser whichever replica it
/// is connected to:
///
/// ```text
///  request on replica A ──▶ Broadcasting ──PUBLISH──▶ redis
///                                                       │
///                              ┌────────────────────────┴───────────┐
///                              ▼                                    ▼
///                     relay on replica A                   relay on replica B
///                              │                                    │
///                         its sockets                          its sockets
/// ```
///
/// It hands each message on whole — the channel's wire name, with the
/// prefix taken off, and the body as published — and leaves delivery to the
/// caller: `rainier_framework::relay::SocketFanOut::deliver_published` for a
/// `rainier-websocket` `Rooms`, or `PusherServer::deliver_published` for
/// Pusher-protocol sockets.
///
/// # One node, not a cluster
///
/// Point publishers and relays at **the same single node**. Plain `PUBLISH`
/// is meant to reach a whole cluster, but in practice a sharded deployment can
/// keep it to the node it was sent to, and then a broadcast reaches only the
/// relays that happen to share that node — the symptom is an event that
/// arrives "sometimes, after a refresh". A comma-separated seed list is
/// trimmed to its first entry for that reason.
///
/// # Pub/sub keeps nothing
///
/// A message published while a relay is reconnecting is gone. That is right
/// for pushing to a browser and wrong for anything that must not be missed —
/// that wants [a job](https://rainierframework.io/docs/queues).
#[derive(Clone, Debug)]
pub struct RedisRelay {
    url: String,
    prefix: String,
}

impl RedisRelay {
    /// Relay from the Redis at `url` (`redis://…`, or a comma-separated
    /// seed list, of which the first entry is used).
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        let seed = url.split(',').next().unwrap_or(&url).trim().to_string();
        Self { url: seed, prefix: String::new() }
    }

    /// Only channels published under this prefix — the same prefix the
    /// [`RedisBroadcaster`] was given. Taken off the channel name before it
    /// is handed on.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The pattern subscribed to: the prefix, then everything.
    pub fn pattern(&self) -> String {
        format!("{}*", self.prefix)
    }

    /// Subscribe. Each [`RedisRelayStream::next`] is one published message.
    pub async fn subscribe(&self) -> Result<RedisRelayStream> {
        let connector = RedisConnector::open(&self.url)?;
        let subscription = connector.subscribe(&self.pattern()).await?;
        Ok(RedisRelayStream { subscription, prefix: self.prefix.clone() })
    }

    /// The channel's wire name a published one stands for, the prefix off;
    /// `None` for a channel outside it.
    pub fn channel_of<'a>(&self, published: &'a str) -> Option<&'a str> {
        published.strip_prefix(self.prefix.as_str())
    }
}

/// A live relay subscription.
pub struct RedisRelayStream {
    subscription: rainier_drivers::redis::RedisSubscription,
    prefix: String,
}

impl RedisRelayStream {
    /// The next message as `(channel, body)` — the channel's wire name with
    /// the prefix off, the body as published. `None` when the connection has
    /// ended, which is the signal to subscribe again. A message that is not
    /// text, or outside the prefix, is skipped.
    pub async fn next(&mut self) -> Option<(String, String)> {
        loop {
            let message = self.subscription.next_message().await?;
            let Some(channel) = message.channel.strip_prefix(self.prefix.as_str()) else {
                continue;
            };
            let Some(body) = message.text() else { continue };
            return Some((channel.to_string(), body.to_string()));
        }
    }
}

impl std::fmt::Debug for RedisRelayStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisRelayStream").field("prefix", &self.prefix).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_subscribes_under_the_prefix_and_takes_it_off() {
        let relay = RedisRelay::new("redis://a:6379, redis://b:6379").with_prefix("app_");
        assert_eq!(relay.pattern(), "app_*");
        assert_eq!(relay.url, "redis://a:6379", "one node: the first seed");
        assert_eq!(relay.channel_of("app_private-orders.7"), Some("private-orders.7"));
        assert_eq!(relay.channel_of("other_news"), None);
        assert_eq!(RedisRelay::new("redis://a").pattern(), "*");
    }

    #[test]
    fn the_prefix_goes_before_the_wire_name_not_after_it() {
        // Two applications sharing a Redis get this wrong silently: the
        // publish succeeds and nobody is subscribed to what was published.
        assert_eq!(prefixed("app_", &Channel::private("orders.7")), "app_private-orders.7");
        assert_eq!(prefixed("", &Channel::public("news")), "news");
    }

    #[test]
    fn the_body_is_what_echo_server_reads() {
        let broadcast = Broadcast::new(
            "OrderShipped",
            vec![Channel::private("orders.7")],
            serde_json::json!({ "id": 7 }),
        )
        .except("1.2");

        let body = broadcast.wire_payload();
        assert_eq!(body["event"], "OrderShipped");
        assert_eq!(body["data"]["id"], 7);
        assert_eq!(body["socket"], "1.2");
    }
}
