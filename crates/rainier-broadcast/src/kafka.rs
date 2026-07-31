//! Publishing over Kafka — [`KafkaBroadcaster`], and [`KafkaRelay`] to read it
//! back.
//!
//! The same shape as the [Redis broadcaster](crate::redis): your application
//! publishes, and something else relays to the browsers. What changes is what
//! sits in the middle.
//!
//! ```text
//! your app ──produce broadcasts──▶ kafka ──▶ every replica ──ws──▶ browsers
//!                                    │
//!                                    └──▶ an audit consumer, next Tuesday
//! ```
//!
//! # Why bother, when Redis pub/sub already works
//!
//! Redis pub/sub is **fire and forget**: a subscriber that is not connected at
//! the instant of the publish never learns it happened, and nothing records
//! that it did. That is the right trade for "make the browser's list move", and
//! the wrong one when the same event is also something the business cares
//! about — an order shipping, a payment clearing, a document being signed.
//!
//! Kafka keeps it. The broadcast that moved the browser is still there for the
//! analytics consumer, the audit log, and the service somebody writes next
//! quarter — none of which have to be built now, or coordinated with, or even
//! known about. That is the actual reason a team with Kafka wants their
//! broadcasts on it: one event, many readers, and the readers are not each
//! other's problem.
//!
//! # Channels are keys, not topics
//!
//! Everything goes to **one topic**, keyed by channel name. A topic per
//! channel would be wrong twice over: Kafka topics are cluster-level objects
//! with partitions and replicas to manage, and `private-orders.7` is not a
//! thing anybody wants to provision.
//!
//! Keying by channel gets the property that matters anyway — every message for
//! a channel lands on one partition, so a browser sees them in the order they
//! were published.

use std::sync::Arc;

use rainier_drivers::kafka::{KafkaClient, KafkaMessage, KafkaOffset, KafkaPosition, KafkaRecord};
use rainier_support::{BoxFuture, Error, Result};
use serde_json::Value;

use crate::broadcaster::Broadcaster;
use crate::channel::Channel;
use crate::event::Broadcast;
use crate::pusher::PusherAuth;

/// The topic broadcasts go to unless the application says otherwise.
pub const DEFAULT_TOPIC: &str = "broadcasts";

/// The header carrying the channel a record was published on.
pub const CHANNEL_HEADER: &str = "channel";

/// The header carrying the event name.
pub const EVENT_HEADER: &str = "event";

/// The header carrying the socket id to skip, when there is one.
pub const SOCKET_HEADER: &str = "socket";

/// Publishes each broadcast to a Kafka topic, keyed by channel.
///
/// ```no_run
/// use std::sync::Arc;
/// use rainier_broadcast::{Broadcasting, KafkaBroadcaster};
/// use rainier_drivers::kafka::{KafkaClient, KafkaConnector};
///
/// # async fn wire() -> rainier_support::Result<()> {
/// let client = Arc::new(KafkaClient::connect(&KafkaConnector::parse("kafka:9092")).await?);
///
/// let broadcasting = Broadcasting::new(Arc::new(
///     KafkaBroadcaster::new(client).on_topic("broadcasts"),
/// ));
/// # let _ = broadcasting; Ok(()) }
/// ```
pub struct KafkaBroadcaster {
    client: Arc<KafkaClient>,
    topic: String,
    prefix: String,
    auth: Option<Arc<PusherAuth>>,
}

impl KafkaBroadcaster {
    /// Publish through `client`, to [`DEFAULT_TOPIC`].
    pub fn new(client: Arc<KafkaClient>) -> Self {
        Self { client, topic: DEFAULT_TOPIC.to_string(), prefix: String::new(), auth: None }
    }

    /// Publish to `topic` instead.
    pub fn on_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = topic.into();
        self
    }

    /// Prefix every channel name.
    ///
    /// The same knob the Redis broadcaster has, and it matters more here: two
    /// applications sharing a topic is normal, and without a prefix each one's
    /// relay would fan out the other's broadcasts to its own browsers.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Sign subscription requests with the Pusher protocol's HMAC.
    ///
    /// Only needed when what relays these to browsers is a Pusher-compatible
    /// server. A [`KafkaRelay`] inside your own process is not, because it has
    /// already authorised the subscription itself.
    pub fn with_pusher_auth(mut self, auth: PusherAuth) -> Self {
        self.auth = Some(Arc::new(auth));
        self
    }

    /// The topic being published to.
    pub fn topic(&self) -> &str {
        &self.topic
    }
}

/// The name a record is keyed by: the configured prefix, then the wire name.
fn prefixed(prefix: &str, channel: &Channel) -> String {
    format!("{prefix}{}", channel.wire_name())
}

/// The record one channel's copy of a broadcast becomes.
///
/// A free function so the shape of what goes on the wire can be tested without
/// a broker to connect to — the record is the contract, and it is decided here
/// rather than by whatever the client happens to do with it.
fn record_for(prefix: &str, broadcast: &Broadcast, channel: &Channel) -> Result<KafkaRecord> {
    let name = prefixed(prefix, channel);

    let body = serde_json::to_vec(&broadcast.wire_payload())
        .map_err(|e| Error::internal(format!("a broadcast must serialise: {e}")))?;

    // Keyed by channel: one partition per channel, so the order a browser sees
    // is the order things happened in.
    let mut record = KafkaRecord::new(body)
        .keyed(name.clone())
        .header(CHANNEL_HEADER, name)
        .header(EVENT_HEADER, broadcast.event.clone());

    if let Some(socket) = &broadcast.except {
        // In a header as well as in the body, so a relay can decide whether to
        // deliver without deserialising anything.
        record = record.header(SOCKET_HEADER, socket.clone());
    }

    Ok(record)
}

impl Broadcaster for KafkaBroadcaster {
    fn name(&self) -> &'static str {
        "kafka"
    }

    fn publish<'a>(&'a self, broadcast: &'a Broadcast) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let records = broadcast
                .channels
                .iter()
                .map(|channel| record_for(&self.prefix, broadcast, channel))
                .collect::<Result<Vec<_>>>()?;

            // One call for every channel: they are keyed differently and so may
            // land on different partitions, and the client batches per
            // partition anyway.
            let placed = self.client.produce(&self.topic, records).await?;

            tracing::debug!(
                topic = self.topic,
                records = placed.len(),
                event = broadcast.event,
                "broadcast produced"
            );
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
            None => Ok(match member {
                Some(member) => serde_json::json!({ "channel_data": member }),
                None => serde_json::json!({}),
            }),
        }
    }
}

/// One broadcast, read back off the topic.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayedBroadcast {
    /// The channel it was published on, prefix included.
    pub channel: String,
    /// The event name a client is listening for.
    pub event: String,
    /// The payload, as published.
    pub payload: Value,
    /// The socket that caused it, and should therefore not be told about it.
    pub except: Option<String>,
    /// Where it sat in the log.
    pub position: KafkaPosition,
}

impl RelayedBroadcast {
    /// Whether this should be delivered to `socket`.
    ///
    /// False only for the socket the broadcast asked to skip — the exclusion
    /// applied at the point of delivery because that is the only
    /// place that knows which socket it is talking to.
    pub fn should_reach(&self, socket: &str) -> bool {
        self.except.as_deref() != Some(socket)
    }

    /// The body to send to a browser: `{"event": …, "data": …}`.
    pub fn wire_payload(&self) -> Value {
        serde_json::json!({ "event": self.event, "data": self.payload })
    }

    /// Read one back out of a record.
    fn from_message(message: &KafkaMessage) -> Result<Self> {
        let body: Value = serde_json::from_slice(&message.value).map_err(|e| {
            Error::internal(format!("a broadcast on `{}` was not JSON: {e}", message.topic))
        })?;

        let channel = message
            .header(CHANNEL_HEADER)
            .map(str::to_string)
            // A record with no channel header was written by something else on
            // the same topic. The key is where the channel is anyway.
            .or_else(|| message.key.as_ref().and_then(|key| String::from_utf8(key.clone()).ok()))
            .ok_or_else(|| {
                Error::internal("a broadcast record carried neither a channel header nor a key")
            })?;

        Ok(Self {
            channel,
            event: body
                .get("event")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| message.header(EVENT_HEADER).map(str::to_string))
                .unwrap_or_default(),
            payload: body.get("data").cloned().unwrap_or(Value::Null),
            except: body.get("socket").and_then(Value::as_str).map(str::to_string),
            position: message.position(),
        })
    }
}

/// Reads broadcasts back off the topic, so a process holding sockets can fan
/// them out.
///
/// This is the half that makes [`rainier-websocket`](../rainier_websocket/index.html)
/// work behind a load balancer. A `Rooms` registry lives in one process's
/// memory, so two replicas have two sets of rooms and a broadcast from replica
/// A never reaches a browser on replica B. A relay in each replica fixes that
/// without a second server to run.
///
/// ```text
///  request on replica A ──▶ Broadcasting ──▶ kafka topic
///                                              │
///                              ┌───────────────┴───────────────┐
///                              ▼                               ▼
///                       relay on replica A              relay on replica B
///                              │                               │
///                          its sockets                     its sockets
/// ```
///
/// # It has no cursor, deliberately
///
/// A relay reads from the **end** of the log and never commits an offset, so
/// it behaves like pub/sub: every replica sees every message published while
/// it is running, and a replica that restarts does not replay yesterday's
/// broadcasts to whoever happens to be connected now.
///
/// That is the correct semantics for pushing to a browser and the wrong ones
/// for anything that must not be missed. If a message matters, it wants a
/// [queue](../rainier_queue/index.html) — this exists to move a list, not to
/// deliver an instruction.
pub struct KafkaRelay {
    client: Arc<KafkaClient>,
    topic: String,
    start: KafkaOffset,
    max_bytes: i32,
    max_wait: std::time::Duration,
}

impl KafkaRelay {
    /// Relay `topic` through `client`.
    pub fn new(client: Arc<KafkaClient>, topic: impl Into<String>) -> Self {
        Self {
            client,
            topic: topic.into(),
            start: KafkaOffset::Latest,
            max_bytes: 1024 * 1024,
            // Long-polling is what makes an idle relay free: the broker holds
            // the request open rather than answering "nothing" ten times a
            // second.
            max_wait: std::time::Duration::from_secs(1),
        }
    }

    /// Start from the oldest retained record rather than from the end.
    ///
    /// For a test, and for the rare consumer that genuinely wants the history.
    /// A relay pushing to browsers does not: it would deliver a backlog to
    /// people who were not there.
    pub fn from_earliest(mut self) -> Self {
        self.start = KafkaOffset::Earliest;
        self
    }

    /// How long a fetch waits for something to arrive.
    pub fn with_max_wait(mut self, max_wait: std::time::Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// The topic being relayed.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Open a cursor on every partition.
    ///
    /// Separate from [`poll`](Self::poll) so a caller owns the loop — and so a
    /// test can drive one turn of it.
    pub async fn subscribe(&self) -> Result<RelayCursor> {
        let partitions = self.client.partitions(&self.topic).await?.ok_or_else(|| {
            Error::service_unavailable(format!(
                "the Kafka topic `{}` does not exist, so there is nothing to relay.",
                self.topic
            ))
        })?;

        let mut at = Vec::with_capacity(partitions.len());
        for partition in partitions {
            let offset = self.client.offset(&self.topic, partition, self.start).await?;
            at.push((partition, offset));
        }

        Ok(RelayCursor { at })
    }

    /// Read whatever has arrived, advancing `cursor`.
    ///
    /// Every partition is polled **concurrently**. Doing them one at a time
    /// would add the poll interval per partition to the delivery latency of the
    /// last one, which on a twelve-partition topic is a browser waiting twelve
    /// seconds to see a change.
    pub async fn poll(&self, cursor: &mut RelayCursor) -> Result<Vec<RelayedBroadcast>> {
        let fetches = cursor.at.iter().map(|(partition, offset)| {
            self.client.fetch(&self.topic, *partition, *offset, self.max_bytes, self.max_wait)
        });

        let fetched = futures_util::future::try_join_all(fetches).await?;

        let mut relayed = Vec::new();
        for (slot, outcome) in cursor.at.iter_mut().zip(fetched) {
            let (partition, offset) = slot;

            let Some(fetch) = outcome else {
                // The cursor fell off the log — retention caught up with us,
                // or the topic was recreated. Rejoin at the end rather than
                // replaying whatever is left, which is what a relay is for.
                let end = self.client.offset(&self.topic, *partition, KafkaOffset::Latest).await?;
                tracing::warn!(
                    topic = self.topic,
                    partition,
                    from = *offset,
                    to = end,
                    "the relay's cursor is no longer in the log; rejoining at the end"
                );
                *offset = end;
                continue;
            };

            if let Some(next) = fetch.next_offset() {
                *offset = next;
            }

            for message in &fetch.messages {
                match RelayedBroadcast::from_message(message) {
                    Ok(broadcast) => relayed.push(broadcast),
                    // One unreadable record must not stop the relay: it would
                    // stop every browser, forever, over one bad write by
                    // something else on the topic.
                    Err(e) => tracing::warn!(
                        topic = self.topic,
                        partition = message.partition,
                        offset = message.offset,
                        error = %e,
                        "skipping a record that is not a broadcast"
                    ),
                }
            }
        }

        Ok(relayed)
    }
}

/// Where a relay has read up to, per partition.
#[derive(Debug, Clone, Default)]
pub struct RelayCursor {
    at: Vec<(i32, i64)>,
}

impl RelayCursor {
    /// The partitions being followed, and the next offset for each.
    pub fn positions(&self) -> &[(i32, i64)] {
        &self.at
    }

    /// How many partitions are being followed.
    pub fn len(&self) -> usize {
        self.at.len()
    }

    /// Whether it is following none.
    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_drivers::kafka::KafkaMessage;
    use std::collections::BTreeMap;

    fn message(headers: &[(&str, &str)], body: Value, key: Option<&str>) -> KafkaMessage {
        KafkaMessage {
            topic: "broadcasts".into(),
            partition: 2,
            offset: 91,
            key: key.map(|k| k.as_bytes().to_vec()),
            value: serde_json::to_vec(&body).unwrap(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
                .collect::<BTreeMap<_, _>>(),
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn the_prefix_goes_before_the_wire_name() {
        assert_eq!(prefixed("app_", &Channel::private("orders.7")), "app_private-orders.7");
        assert_eq!(prefixed("", &Channel::public("news")), "news");
    }

    #[test]
    fn a_record_is_keyed_by_channel_so_a_channel_keeps_its_order() {
        let channel = Channel::private("orders.7");
        let broadcast =
            Broadcast::new("OrderShipped", vec![channel.clone()], serde_json::json!({ "id": 7 }));

        let record = record_for("", &broadcast, &channel).unwrap();

        // The key is the whole ordering guarantee: same key, same partition,
        // and Kafka orders within a partition and nowhere else.
        assert_eq!(record.key.as_deref(), Some(&b"private-orders.7"[..]));
        assert_eq!(
            record.headers.get(CHANNEL_HEADER).map(Vec::as_slice),
            Some(&b"private-orders.7"[..])
        );
        assert_eq!(record.headers.get(EVENT_HEADER).map(Vec::as_slice), Some(&b"OrderShipped"[..]));
        assert!(!record.headers.contains_key(SOCKET_HEADER), "there was no socket to skip");
    }

    #[test]
    fn the_prefix_reaches_the_key_as_well_as_the_header() {
        // Two applications on one topic: without this, each relay fans out the
        // other's broadcasts to its own browsers.
        let channel = Channel::private("orders.7");
        let broadcast = Broadcast::new("OrderShipped", vec![channel.clone()], Value::Null);

        let record = record_for("checkout_", &broadcast, &channel).unwrap();

        assert_eq!(record.key.as_deref(), Some(&b"checkout_private-orders.7"[..]));
    }

    #[test]
    fn to_others_rides_in_a_header_and_in_the_body() {
        let channel = Channel::private("orders.7");
        let broadcast =
            Broadcast::new("OrderShipped", vec![channel.clone()], Value::Null).except("1234.5678");

        let record = record_for("", &broadcast, &channel).unwrap();

        assert_eq!(record.headers.get(SOCKET_HEADER).map(Vec::as_slice), Some(&b"1234.5678"[..]));

        let body: Value = serde_json::from_slice(&record.value).unwrap();
        assert_eq!(body["socket"], "1234.5678");
    }

    #[test]
    fn a_relayed_broadcast_is_read_back_out_of_a_record() {
        let relayed = RelayedBroadcast::from_message(&message(
            &[("channel", "private-orders.7"), ("event", "OrderShipped")],
            serde_json::json!({ "event": "OrderShipped", "data": { "id": 7 } }),
            Some("private-orders.7"),
        ))
        .unwrap();

        assert_eq!(relayed.channel, "private-orders.7");
        assert_eq!(relayed.event, "OrderShipped");
        assert_eq!(relayed.payload["id"], 7);
        assert_eq!(relayed.except, None);
        assert_eq!(relayed.position.offset, 91);
    }

    #[test]
    fn the_socket_to_skip_survives_the_round_trip() {
        let relayed = RelayedBroadcast::from_message(&message(
            &[("channel", "private-orders.7")],
            serde_json::json!({ "event": "OrderShipped", "data": {}, "socket": "1234.5678" }),
            None,
        ))
        .unwrap();

        assert_eq!(relayed.except.as_deref(), Some("1234.5678"));
        assert!(!relayed.should_reach("1234.5678"), "the socket that caused it is skipped");
        assert!(relayed.should_reach("9999.0000"), "everyone else still hears about it");
    }

    #[test]
    fn a_record_with_no_channel_header_falls_back_to_its_key() {
        // Something else wrote to the topic, or an older producer did. The key
        // is the channel anyway, so there is no need to give up.
        let relayed = RelayedBroadcast::from_message(&message(
            &[],
            serde_json::json!({ "event": "Ping", "data": {} }),
            Some("private-orders.7"),
        ))
        .unwrap();

        assert_eq!(relayed.channel, "private-orders.7");
    }

    #[test]
    fn a_record_that_is_not_a_broadcast_is_an_error_rather_than_a_panic() {
        let mut message = message(&[], serde_json::json!({}), Some("x"));
        message.value = b"not json".to_vec();

        assert!(RelayedBroadcast::from_message(&message).is_err());
    }

    #[test]
    fn the_wire_payload_is_what_a_browser_receives() {
        let relayed = RelayedBroadcast::from_message(&message(
            &[("channel", "news")],
            serde_json::json!({ "event": "Published", "data": { "id": 3 } }),
            None,
        ))
        .unwrap();

        let body = relayed.wire_payload();
        assert_eq!(body["event"], "Published");
        assert_eq!(body["data"]["id"], 3);
    }
}
