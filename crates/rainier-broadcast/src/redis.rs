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

#[cfg(test)]
mod tests {
    use super::*;

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
