//! Apache Kafka — a partitioned log, and the operations something built on one
//! needs.
//!
//! # Kafka is a log, not a queue
//!
//! Everything awkward about putting Kafka behind a queue-shaped port follows
//! from one fact: **a consumer does not remove what it reads.** A partition is
//! an append-only file and a reader is a cursor into it. There is no
//! "delete this message", no "make this one visible again", and no way to
//! acknowledge the third message while the second is still in flight — the
//! cursor is a single number.
//!
//! ```text
//!   topic "jobs"
//!   partition 0  ┌────┬────┬────┬────┬────┐
//!                │ 12 │ 13 │ 14 │ 15 │ 16 │  ← high watermark
//!                └────┴────┴────┴────┴────┘
//!                          ▲
//!                          └── one cursor. Not a set of in-flight messages.
//!   partition 1  ┌────┬────┐
//!                │ 40 │ 41 │
//!                └────┴────┘
//! ```
//!
//! What you get for that: retention independent of consumption (a message can
//! be read again, by a second consumer, next week), ordering within a
//! partition, and throughput that comes from partition count rather than from
//! locking.
//!
//! # What this client does, and what it leaves out
//!
//! It speaks the parts of the protocol Rainier needs: metadata, produce, fetch,
//! offsets, and topic creation. Records are produced with **`acks=all`**, so a
//! `produce` that returns has been accepted by every in-sync replica.
//!
//! It does **not** join a consumer group. There is no `JoinGroup`, no
//! heartbeat, and no broker-side rebalancing here, so nothing this crate reads
//! is visible to `kafka-consumer-groups.sh`. Partition ownership and cursors
//! are the *caller's* to arrange — [`rainier-queue`](../rainier_queue/index.html)
//! does it with the lock manager and the cache, which is infrastructure a
//! Rainier application already runs.
//!
//! That is a deliberate trade, and the honest version of it is: a
//! group-coordinating client is a state machine with a rebalance protocol, and
//! carrying one that is subtly wrong is worse than not carrying one at all.
//!
//! # No C toolchain
//!
//! The wire client is [`rskafka`], which is pure Rust. The obvious alternative
//! wraps `librdkafka` and needs a C compiler and CMake to build — on every
//! machine that compiles the workspace, including the ones that will never
//! speak to Kafka. TLS and SASL are still here; they are behind
//! [`rustls`](https://docs.rs/rustls) rather than OpenSSL for the same reason.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rainier_support::{Error, Result};
use rskafka::client::error::{Error as KafkaError, ProtocolError};
use rskafka::client::partition::{Compression, OffsetAt, PartitionClient, UnknownTopicHandling};
use rskafka::client::{Client, ClientBuilder};
use rskafka::record::Record;

/// The port a Kafka broker listens on when nobody said otherwise.
pub const DEFAULT_PORT: u16 = 9092;

/// How long an operation may spend retrying before it gives up.
///
/// The wire client's own default is **to retry forever**, sleeping up to eight
/// minutes between attempts. That is a reasonable choice for a data pipeline
/// and a terrible one inside a web request: a mistyped broker address stops
/// being an error and becomes a hang, which is the failure nobody can
/// diagnose because nothing is ever logged.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How a client proves who it is.
///
/// `Debug` prints the mechanism and the username and **not** the password —
/// connection details end up in logs, and a broker that refuses a login is
/// exactly when somebody turns the logging up.
#[derive(Clone)]
pub struct KafkaCredentials {
    mechanism: SaslMechanism,
    username: String,
    password: String,
}

impl KafkaCredentials {
    /// A username and password for `mechanism`.
    pub fn new(
        mechanism: SaslMechanism,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self { mechanism, username: username.into(), password: password.into() }
    }

    /// Which SASL mechanism these are for.
    pub fn mechanism(&self) -> SaslMechanism {
        self.mechanism
    }

    /// The username.
    pub fn username(&self) -> &str {
        &self.username
    }
}

impl std::fmt::Debug for KafkaCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaCredentials")
            .field("mechanism", &self.mechanism)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The SASL mechanisms a broker is likely to offer.
///
/// `PLAIN` sends the password in the clear, so it belongs inside TLS and
/// nowhere else — which is how every managed Kafka is configured. The SCRAM
/// mechanisms do a challenge-response instead and are what a self-hosted
/// cluster usually enables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaslMechanism {
    /// `PLAIN` — the password crosses the wire. Use TLS.
    Plain,
    /// `SCRAM-SHA-256`.
    ScramSha256,
    /// `SCRAM-SHA-512`.
    ScramSha512,
}

impl SaslMechanism {
    /// The name the broker knows it by — `"SCRAM-SHA-256"`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Plain => "PLAIN",
            Self::ScramSha256 => "SCRAM-SHA-256",
            Self::ScramSha512 => "SCRAM-SHA-512",
        }
    }
}

/// How to reach a cluster.
///
/// The brokers listed here are **bootstrap** brokers: the client asks one of
/// them for the cluster's metadata and then talks to whichever broker leads
/// each partition. Listing one is enough for the connection to work and not
/// enough for it to survive that broker being down, which is why the list is a
/// list.
///
/// ```
/// use rainier_drivers::kafka::KafkaConnector;
///
/// let connector = KafkaConnector::parse("kafka-1:9092, kafka-2:9092")
///     .with_client_id("checkout");
///
/// assert_eq!(connector.brokers(), ["kafka-1:9092", "kafka-2:9092"]);
/// ```
#[derive(Clone, Debug)]
pub struct KafkaConnector {
    brokers: Vec<String>,
    client_id: Option<String>,
    credentials: Option<KafkaCredentials>,
    tls: bool,
    max_message_size: Option<usize>,
    timeout: Duration,
}

impl KafkaConnector {
    /// Bootstrap from `brokers`.
    pub fn new(brokers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            brokers: brokers.into_iter().map(Into::into).map(|b| with_default_port(&b)).collect(),
            client_id: None,
            credentials: None,
            tls: false,
            max_message_size: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Bootstrap from a comma-separated list — what `KAFKA_BROKERS` holds.
    ///
    /// A bare host gets [`DEFAULT_PORT`], because `KAFKA_BROKERS=localhost` is
    /// what everybody writes first and failing to connect to port 0 is a poor
    /// way to explain it.
    pub fn parse(brokers: &str) -> Self {
        Self::new(brokers.split(',').map(str::trim).filter(|broker| !broker.is_empty()))
    }

    /// Identify this application to the broker.
    ///
    /// Shows up in the broker's own metrics and request logs, which is where
    /// "which service is hammering this topic" gets answered.
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    /// Authenticate with SASL.
    pub fn with_credentials(mut self, credentials: KafkaCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Connect over TLS.
    ///
    /// Needs the `kafka-tls` feature; without it a connector that asks for TLS
    /// fails at [`KafkaClient::connect`] rather than connecting in the clear,
    /// because quietly downgrading a transport is not a fallback.
    pub fn with_tls(mut self) -> Self {
        self.tls = true;
        self
    }

    /// Cap the size of a single request.
    ///
    /// The broker has its own cap (`message.max.bytes`, a megabyte by default)
    /// and the smaller of the two wins. Setting this above the broker's does
    /// not raise it; it moves where the rejection happens.
    pub fn with_max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = Some(bytes);
        self
    }

    /// How long an operation may spend retrying before giving up.
    ///
    /// Covers connecting, producing and fetching — every one of which the wire
    /// client would otherwise retry until the process was killed. See
    /// [`DEFAULT_TIMEOUT`].
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How long an operation may spend retrying.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The bootstrap brokers.
    pub fn brokers(&self) -> &[String] {
        &self.brokers
    }

    /// Whether this connector will use TLS.
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// The credentials, if any were configured.
    pub fn credentials(&self) -> Option<&KafkaCredentials> {
        self.credentials.as_ref()
    }
}

/// `host` unchanged if it already names a port, `host:9092` otherwise.
fn with_default_port(host: &str) -> String {
    // A bracketed IPv6 literal — `[::1]:9092` — has colons of its own, so the
    // test is for a port after the closing bracket rather than for any colon.
    let has_port = match host.rsplit_once(']') {
        Some((_, rest)) => rest.starts_with(':'),
        None => host.contains(':'),
    };

    if has_port {
        host.to_string()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    }
}

/// One record to produce.
#[derive(Debug, Clone, Default)]
pub struct KafkaRecord {
    /// The partitioning key. Records with the same key land on the same
    /// partition, which is the only ordering guarantee Kafka makes.
    pub key: Option<Vec<u8>>,
    /// The body.
    pub value: Vec<u8>,
    /// Headers — metadata a consumer can read without parsing the body.
    pub headers: BTreeMap<String, Vec<u8>>,
    /// When it happened. `None` means now.
    pub timestamp: Option<DateTime<Utc>>,
}

impl KafkaRecord {
    /// A record carrying `value`.
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self { value: value.into(), ..Default::default() }
    }

    /// Partition it by `key`.
    pub fn keyed(mut self, key: impl Into<Vec<u8>>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Add a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Timestamp it with something other than now.
    pub fn at(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = Some(timestamp);
        self
    }
}

/// One record read back.
#[derive(Debug, Clone)]
pub struct KafkaMessage {
    /// Which topic it came from.
    pub topic: String,
    /// Which partition.
    pub partition: i32,
    /// Its offset in that partition — the cursor value that reads it again.
    pub offset: i64,
    /// The partitioning key, if it had one.
    pub key: Option<Vec<u8>>,
    /// The body.
    pub value: Vec<u8>,
    /// Its headers.
    pub headers: BTreeMap<String, Vec<u8>>,
    /// When the producer said it happened.
    pub timestamp: DateTime<Utc>,
}

impl KafkaMessage {
    /// A header's value as UTF-8, if it is there and is UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| std::str::from_utf8(value).ok())
    }

    /// Where this message is, for a cursor to be stored against.
    pub fn position(&self) -> KafkaPosition {
        KafkaPosition { topic: self.topic.clone(), partition: self.partition, offset: self.offset }
    }
}

/// A place in a partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KafkaPosition {
    /// The topic.
    pub topic: String,
    /// The partition.
    pub partition: i32,
    /// The offset.
    pub offset: i64,
}

impl std::fmt::Display for KafkaPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.topic, self.partition, self.offset)
    }
}

impl std::str::FromStr for KafkaPosition {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        // From the right: a topic name may contain neither `:` nor a colon by
        // Kafka's own rules, but splitting from the right costs nothing and
        // survives a topic that somehow does.
        let (topic, rest) = value
            .rsplit_once(':')
            .and_then(|(head, offset)| head.rsplit_once(':').map(|(t, p)| (t, (p, offset))))
            .ok_or_else(|| Error::internal(format!("`{value}` is not a Kafka position")))?;

        let (partition, offset) = rest;
        Ok(Self {
            topic: topic.to_string(),
            partition: partition
                .parse()
                .map_err(|_| Error::internal(format!("`{partition}` is not a partition")))?,
            offset: offset
                .parse()
                .map_err(|_| Error::internal(format!("`{offset}` is not an offset")))?,
        })
    }
}

/// What one fetch returned.
#[derive(Debug, Clone)]
pub struct KafkaFetch {
    /// The records, in offset order.
    pub messages: Vec<KafkaMessage>,
    /// The offset the next produced record will get.
    ///
    /// The distance between this and where you are reading **is** the lag, and
    /// it is the number worth alerting on.
    pub high_watermark: i64,
}

impl KafkaFetch {
    /// Whether the fetch came back empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The offset to read from next.
    pub fn next_offset(&self) -> Option<i64> {
        self.messages.last().map(|message| message.offset + 1)
    }
}

/// Which end of a partition to ask about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KafkaOffset {
    /// The oldest record still retained — **not** the oldest ever written.
    Earliest,
    /// One past the newest record.
    Latest,
}

/// A connection to a Kafka cluster.
///
/// Cheap to share: one of these holds a connection per broker it has needed,
/// and a client per partition it has touched, so passing an `Arc` around is
/// how the cache, the queue and the broadcaster stay on one set of sockets.
pub struct KafkaClient {
    client: Client,
    /// The wall-clock budget for one operation.
    timeout: Duration,
    /// Leader connections, kept because discovering a partition's leader is a
    /// round trip and the answer changes only when the cluster does.
    partitions: Mutex<HashMap<(String, i32), Arc<PartitionClient>>>,
    /// Where the next keyless record goes, so they spread rather than piling
    /// onto partition 0.
    next_partition: AtomicUsize,
}

impl KafkaClient {
    /// Connect through `connector`.
    pub async fn connect(connector: &KafkaConnector) -> Result<Self> {
        if connector.brokers.is_empty() {
            return Err(Error::internal("Kafka needs at least one bootstrap broker."));
        }

        let mut builder =
            ClientBuilder::new(connector.brokers.clone()).backoff_config(rskafka::BackoffConfig {
                init_backoff: Duration::from_millis(100),
                // The client's own default is eight minutes, which inside a
                // request is indistinguishable from a hang.
                max_backoff: Duration::from_secs(2),
                base: 2.0,
                deadline: Some(connector.timeout),
            });

        if let Some(client_id) = &connector.client_id {
            builder = builder.client_id(client_id.clone());
        }
        if let Some(size) = connector.max_message_size {
            builder = builder.max_message_size(size);
        }
        if let Some(credentials) = &connector.credentials {
            builder = builder.sasl_config(sasl_config(credentials));
        }
        builder = with_tls(builder, connector)?;

        let brokers = connector.brokers.join(", ");

        let client =
            within(connector.timeout, format!("connecting to Kafka at {brokers}"), async {
                builder.build().await.map_err(|e| {
                    connection_error(format!("could not reach Kafka at {brokers}: {e}"))
                })
            })
            .await?;

        Ok(Self {
            client,
            timeout: connector.timeout,
            partitions: Mutex::new(HashMap::new()),
            next_partition: AtomicUsize::new(0),
        })
    }

    /// Every topic the cluster knows about.
    pub async fn topics(&self) -> Result<Vec<String>> {
        Ok(self.metadata().await?.into_iter().map(|topic| topic.name).collect())
    }

    /// The cluster's topic metadata, within the timeout.
    async fn metadata(&self) -> Result<Vec<rskafka::topic::Topic>> {
        within(self.timeout, "listing Kafka topics", async {
            self.client
                .list_topics()
                .await
                .map_err(|e| connection_error(format!("could not list topics: {e}")))
        })
        .await
    }

    /// The partitions of `topic`, in order.
    ///
    /// `None` when the topic does not exist, which is a different thing from a
    /// topic with no partitions and from a broker that will not answer.
    pub async fn partitions(&self, topic: &str) -> Result<Option<Vec<i32>>> {
        let topics = self.metadata().await?;

        Ok(topics
            .into_iter()
            .find(|candidate| candidate.name == topic)
            .map(|found| found.partitions.into_iter().collect()))
    }

    /// Create `topic`, returning whether this call is the one that made it.
    ///
    /// Idempotent by tolerating the "already exists" error: two replicas
    /// starting together both try, and only one can win.
    ///
    /// **A production cluster usually forbids this**, and should — topic
    /// creation is where partition counts and replication factors get decided,
    /// and a service that creates its own topics on boot decides them by
    /// accident. It is here for a development cluster and a test.
    pub async fn create_topic(
        &self,
        topic: &str,
        partitions: i32,
        replication_factor: i16,
    ) -> Result<bool> {
        let controller = self
            .client
            .controller_client()
            .map_err(|e| connection_error(format!("could not reach the controller: {e}")))?;

        let created = within(self.timeout, format!("creating the Kafka topic `{topic}`"), async {
            match controller.create_topic(topic, partitions, replication_factor, 5_000).await {
                Ok(()) => Ok(true),
                Err(e) if already_exists(&e) => Ok(false),
                Err(e) => Err(connection_error(format!("could not create `{topic}`: {e}"))),
            }
        })
        .await?;

        // The controller accepting the topic is not the cluster knowing about
        // it: metadata propagates, and a produce in the meantime is told the
        // topic does not exist. Returning before it is visible would make
        // "created" mean something a caller cannot act on — so wait for it.
        self.await_topic(topic).await?;

        Ok(created)
    }

    /// Wait until the cluster reports `topic`, within the timeout.
    async fn await_topic(&self, topic: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.timeout;

        loop {
            if self.partitions(topic).await?.is_some_and(|found| !found.is_empty()) {
                return Ok(());
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(connection_error(format!(
                    "`{topic}` was created but the cluster has not reported it within {:?}",
                    self.timeout
                )));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Produce `records`, choosing each one's partition from its key.
    ///
    /// Keyed records go to `murmur2(key) % partitions` — the same arithmetic
    /// the Java client uses, so a key written here lands where a JVM producer
    /// would put it and per-key ordering holds across a mixed fleet. Keyless
    /// ones go round-robin.
    ///
    /// Returns where each record landed, in the order they were given.
    pub async fn produce(
        &self,
        topic: &str,
        records: Vec<KafkaRecord>,
    ) -> Result<Vec<KafkaPosition>> {
        if records.is_empty() {
            return Ok(vec![]);
        }

        let partitions = self
            .partitions(topic)
            .await?
            .filter(|found| !found.is_empty())
            .ok_or_else(|| unknown_topic(topic))?;

        // Group by partition first: one produce request per partition beats one
        // per record, and a batch is also how Kafka's throughput happens.
        let mut batched: HashMap<i32, Vec<(usize, KafkaRecord)>> = HashMap::new();
        for (index, record) in records.into_iter().enumerate() {
            let partition = self.partition_for(&record, &partitions);
            batched.entry(partition).or_default().push((index, record));
        }

        let mut placed: Vec<Option<KafkaPosition>> =
            (0..batched.values().map(Vec::len).sum()).map(|_| None).collect();

        for (partition, batch) in batched {
            let (indexes, records): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
            let offsets = self.produce_to(topic, partition, records).await?;

            for (index, offset) in indexes.into_iter().zip(offsets) {
                placed[index] = Some(KafkaPosition { topic: topic.to_string(), partition, offset });
            }
        }

        Ok(placed.into_iter().flatten().collect())
    }

    /// Produce to one partition, chosen by the caller.
    ///
    /// For when the partition is a decision rather than a hash — a worker that
    /// owns partition 3 writing a retry back to partition 3, for instance.
    pub async fn produce_to(
        &self,
        topic: &str,
        partition: i32,
        records: Vec<KafkaRecord>,
    ) -> Result<Vec<i64>> {
        if records.is_empty() {
            return Ok(vec![]);
        }

        let client = self.partition_client(topic, partition).await?;
        let records = records.into_iter().map(Record::from).collect();

        client.produce(records, Compression::NoCompression).await.map_err(|e| {
            connection_error(format!("could not produce to `{topic}/{partition}`: {e}"))
        })
    }

    /// Read from `offset`, waiting up to `max_wait` for something to arrive.
    ///
    /// `Ok(None)` means the offset is no longer in the log: either it was
    /// retained away, or it is beyond the end because the topic was recreated.
    /// A caller decides what that means — Kafka's own clients call it
    /// `auto.offset.reset` and make you choose between the two ends.
    ///
    /// `max_wait` is what makes a poll loop cheap: the broker holds the request
    /// open until a record arrives or the time is up, so an idle consumer costs
    /// one parked request rather than a request per turn round the loop.
    pub async fn fetch(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_bytes: i32,
        max_wait: Duration,
    ) -> Result<Option<KafkaFetch>> {
        let client = self.partition_client(topic, partition).await?;

        // The budget is the timeout *plus* the wait the caller asked for: a
        // long poll is meant to sit there, and counting it as slowness would
        // make every idle consumer look like a broken one.
        let outcome =
            within(self.timeout + max_wait, format!("fetching `{topic}/{partition}`"), async {
                Ok(client
                    .fetch_records(
                        offset,
                        1..max_bytes.max(2),
                        max_wait.as_millis().min(i32::MAX as u128) as i32,
                    )
                    .await)
            })
            .await?;

        let (records, high_watermark) = match outcome {
            Ok(fetched) => fetched,
            Err(e) if out_of_range(&e) => return Ok(None),
            Err(e) => {
                return Err(connection_error(format!(
                    "could not fetch from `{topic}/{partition}` at {offset}: {e}"
                )))
            }
        };

        Ok(Some(KafkaFetch {
            messages: records
                .into_iter()
                .map(|found| KafkaMessage {
                    topic: topic.to_string(),
                    partition,
                    offset: found.offset,
                    key: found.record.key,
                    value: found.record.value.unwrap_or_default(),
                    headers: found.record.headers,
                    timestamp: found.record.timestamp,
                })
                .collect(),
            high_watermark,
        }))
    }

    /// Where a partition starts or ends right now.
    pub async fn offset(&self, topic: &str, partition: i32, at: KafkaOffset) -> Result<i64> {
        let client = self.partition_client(topic, partition).await?;
        let at = match at {
            KafkaOffset::Earliest => OffsetAt::Earliest,
            KafkaOffset::Latest => OffsetAt::Latest,
        };

        within(self.timeout, format!("reading the offsets of `{topic}/{partition}`"), async {
            client
                .get_offset(at)
                .await
                .map_err(|e| connection_error(format!("could not read `{topic}/{partition}`: {e}")))
        })
        .await
    }

    /// The partition this record belongs on.
    fn partition_for(&self, record: &KafkaRecord, partitions: &[i32]) -> i32 {
        match &record.key {
            Some(key) => partitions[partition_for_key(key, partitions.len())],
            // Round-robin rather than random: the same spread, and a test can
            // predict it.
            None => {
                partitions[self.next_partition.fetch_add(1, Ordering::Relaxed) % partitions.len()]
            }
        }
    }

    /// A cached client for one partition's leader.
    async fn partition_client(&self, topic: &str, partition: i32) -> Result<Arc<PartitionClient>> {
        let key = (topic.to_string(), partition);

        if let Some(client) = self.cached(&key) {
            return Ok(client);
        }

        let client =
            within(self.timeout, format!("finding the leader of `{topic}/{partition}`"), async {
                self.client
                    // `Error` rather than `Retry`: a topic that does not exist
                    // is a configuration mistake, and retrying it forever turns
                    // a clear failure into a hang.
                    .partition_client(topic.to_string(), partition, UnknownTopicHandling::Error)
                    .await
                    .map_err(|e| {
                        connection_error(format!(
                            "could not reach the leader of `{topic}/{partition}`: {e}"
                        ))
                    })
            })
            .await?;

        let client = Arc::new(client);
        self.partitions.lock().expect("kafka clients poisoned").insert(key, Arc::clone(&client));
        Ok(client)
    }

    fn cached(&self, key: &(String, i32)) -> Option<Arc<PartitionClient>> {
        self.partitions.lock().expect("kafka clients poisoned").get(key).map(Arc::clone)
    }
}

impl std::fmt::Debug for KafkaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaClient")
            .field("partition_clients", &self.partitions.lock().map(|p| p.len()).unwrap_or(0))
            .finish()
    }
}

impl From<KafkaRecord> for Record {
    fn from(record: KafkaRecord) -> Self {
        Record {
            key: record.key,
            value: Some(record.value),
            headers: record.headers,
            timestamp: record.timestamp.unwrap_or_else(Utc::now),
        }
    }
}

/// Which of `partitions` a key belongs on.
///
/// Kafka's default partitioner, arithmetic included: `murmur2` of the key,
/// forced positive by masking the sign bit, modulo the partition count. The
/// masking is not a detail — Java's `%` can return a negative, and a
/// partitioner that returns `-3` is the classic reimplementation bug.
///
/// Matching the Java client matters because partitioning is a *contract
/// between producers*: two services writing the same key must reach the same
/// partition or the ordering that key was chosen for does not exist.
///
/// ```
/// use rainier_drivers::kafka::partition_for_key;
///
/// // Same key, same partition. Every time, from any client.
/// assert_eq!(partition_for_key(b"user-7", 12), partition_for_key(b"user-7", 12));
/// assert!(partition_for_key(b"user-7", 12) < 12);
/// ```
pub fn partition_for_key(key: &[u8], partitions: usize) -> usize {
    assert!(partitions > 0, "a topic with no partitions cannot hold a record");
    (murmur2(key) & 0x7fff_ffff) as usize % partitions
}

/// Kafka's `Utils.murmur2`, byte for byte.
///
/// Transcribed from the Java client rather than taken from a hashing crate:
/// murmur2 has variants, and the one Kafka uses is pinned to its seed and its
/// exact tail handling. A general-purpose murmur2 that differs in either
/// produces a hash that is fine and a partition that is wrong.
fn murmur2(data: &[u8]) -> u32 {
    const SEED: u32 = 0x9747_b28c;
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;

    let length = data.len();
    let mut h = SEED ^ (length as u32);

    let blocks = length / 4;
    for block in 0..blocks {
        let i = block * 4;
        let mut k = (data[i] as u32)
            | ((data[i + 1] as u32) << 8)
            | ((data[i + 2] as u32) << 16)
            | ((data[i + 3] as u32) << 24);

        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);

        h = h.wrapping_mul(M);
        h ^= k;
    }

    // The tail, and it falls through in Java — case 3 also does 2 and 1.
    let tail = length & !3;
    match length % 4 {
        3 => {
            h ^= (data[tail + 2] as u32) << 16;
            h ^= (data[tail + 1] as u32) << 8;
            h ^= data[tail] as u32;
            h = h.wrapping_mul(M);
        }
        2 => {
            h ^= (data[tail + 1] as u32) << 8;
            h ^= data[tail] as u32;
            h = h.wrapping_mul(M);
        }
        1 => {
            h ^= data[tail] as u32;
            h = h.wrapping_mul(M);
        }
        _ => {}
    }

    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h
}

/// The SASL configuration rskafka wants.
fn sasl_config(credentials: &KafkaCredentials) -> rskafka::client::SaslConfig {
    use rskafka::client::{Credentials, SaslConfig};

    let credential = Credentials::new(credentials.username.clone(), credentials.password.clone());

    match credentials.mechanism {
        SaslMechanism::Plain => SaslConfig::Plain(credential),
        SaslMechanism::ScramSha256 => SaslConfig::ScramSha256(credential),
        SaslMechanism::ScramSha512 => SaslConfig::ScramSha512(credential),
    }
}

/// Add TLS to the builder, or explain that the feature is off.
#[cfg(feature = "kafka-tls")]
fn with_tls(builder: ClientBuilder, connector: &KafkaConnector) -> Result<ClientBuilder> {
    if !connector.tls {
        return Ok(builder);
    }

    let mut roots = rustls::RootCertStore::empty();
    let found = rustls_native_certs::load_native_certs();

    for certificate in found.certs {
        // A store with one unparseable certificate in it is still a usable
        // store, and refusing to start over one would be a poor trade.
        let _ = roots.add(certificate);
    }

    if roots.is_empty() {
        return Err(Error::internal(
            "Kafka was asked for TLS but this machine has no trusted root certificates.",
        ));
    }

    let config = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::internal(format!("TLS could not be configured: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();

    Ok(builder.tls_config(std::sync::Arc::new(config)))
}

/// Without the feature, asking for TLS is an error rather than a downgrade.
#[cfg(not(feature = "kafka-tls"))]
fn with_tls(builder: ClientBuilder, connector: &KafkaConnector) -> Result<ClientBuilder> {
    if connector.tls {
        return Err(Error::internal(
            "Kafka TLS needs the `kafka-tls` feature. Connecting without it would send \
             credentials in the clear, so this stops instead.",
        ));
    }
    Ok(builder)
}

/// Run `work`, giving up after `budget`.
///
/// The wire client retries with a backoff whose deadline counts **sleep time**
/// rather than elapsed time, so a broker that refuses a connection in two
/// seconds is retried a dozen times before the accumulated sleep reaches ten.
/// Measured on a machine with nothing listening, that turned a "give up after
/// ten seconds" into two and a half minutes.
///
/// A wall clock is what a caller actually meant, and inside a request it is the
/// difference between a `503` and a worker that never comes back.
async fn within<T>(
    budget: Duration,
    what: impl std::fmt::Display,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(budget, work).await {
        Ok(outcome) => outcome,
        Err(_) => Err(connection_error(format!("{what} timed out after {budget:?}"))),
    }
}

/// Whether a `create_topic` failure was somebody else winning the race.
fn already_exists(error: &KafkaError) -> bool {
    matches!(
        error,
        KafkaError::ServerError { protocol_error, .. }
            if matches!(protocol_error, ProtocolError::TopicAlreadyExists)
    )
}

/// Whether a fetch asked for an offset the log no longer holds.
fn out_of_range(error: &KafkaError) -> bool {
    matches!(
        error,
        KafkaError::ServerError { protocol_error, .. }
            if matches!(protocol_error, ProtocolError::OffsetOutOfRange)
    )
}

/// A topic that is not there.
fn unknown_topic(topic: &str) -> Error {
    Error::service_unavailable(format!(
        "the Kafka topic `{topic}` does not exist or has no partitions."
    ))
}

/// Every driver failure is a dependency outage, not a bug in the request.
///
/// The message is passed through, unlike the Redis driver's: a Kafka error
/// names brokers and topics, and the password — when there is one — lives in
/// the SASL configuration rather than in the connection string, so there is
/// nothing here to leak.
fn connection_error(message: String) -> Error {
    Error::service_unavailable(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_gets_the_default_port() {
        assert_eq!(with_default_port("localhost"), "localhost:9092");
        assert_eq!(with_default_port("localhost:19092"), "localhost:19092");
    }

    #[test]
    fn an_ipv6_literal_is_not_mistaken_for_a_port() {
        // `[::1]` is full of colons and has no port. Reading any colon as a
        // port separator produces a bootstrap address that cannot resolve.
        assert_eq!(with_default_port("[::1]"), "[::1]:9092");
        assert_eq!(with_default_port("[::1]:9092"), "[::1]:9092");
    }

    #[test]
    fn there_is_a_deadline_on_retrying() {
        // Without one the wire client retries forever: a mistyped broker
        // address becomes a hang rather than an error, and a test suite on a
        // machine with no Kafka never finishes rather than skipping.
        assert_eq!(KafkaConnector::parse("kafka:9092").timeout(), DEFAULT_TIMEOUT);
        assert_eq!(
            KafkaConnector::parse("kafka:9092").with_timeout(Duration::from_secs(2)).timeout(),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn a_broker_list_is_split_and_trimmed() {
        let connector = KafkaConnector::parse(" kafka-1:9092 , kafka-2 ,, ");
        assert_eq!(connector.brokers(), ["kafka-1:9092", "kafka-2:9092"]);
    }

    #[test]
    fn the_same_key_always_reaches_the_same_partition() {
        for partitions in [1usize, 3, 8, 12, 64] {
            let first = partition_for_key(b"orders-7", partitions);
            assert_eq!(first, partition_for_key(b"orders-7", partitions));
            assert!(first < partitions, "{first} is not a partition of {partitions}");
        }
    }

    #[test]
    fn murmur2_matches_the_java_client() {
        // `"21"` is the vector in Kafka's own `UtilsTest.testMurmur2`, which is
        // what makes this a check against the real thing rather than against a
        // second copy of my own arithmetic.
        assert_eq!(murmur2(b"21") as i32, -973_932_308);

        // The rest come from an independent transcription of `Utils.murmur2`,
        // and between them they catch what a Rust rewrite gets wrong: an
        // arithmetic shift where Java has `>>>`, a missing `wrapping_mul`, and
        // a tail `match` that does not fall through the way the Java `switch`
        // does. One length of each remainder is deliberate.
        assert_eq!(murmur2(b"") as i32, 275_646_681);
        assert_eq!(murmur2(b"a") as i32, -1_563_381_124);
        assert_eq!(murmur2(b"ab") as i32, 316_155_434);
        assert_eq!(murmur2(b"abc") as i32, 479_470_107);
        assert_eq!(murmur2(b"abcd") as i32, -1_323_649_548);
        assert_eq!(murmur2(b"abcde") as i32, 461_995_741);
        assert_eq!(murmur2(b"user-7") as i32, 696_778_364);
    }

    #[test]
    fn a_key_lands_where_the_java_producer_would_put_it() {
        // The contract that matters: a JVM service and this one writing
        // `user-7` must reach the same partition, or the ordering the key was
        // chosen for does not exist.
        assert_eq!(partition_for_key(b"user-7", 12), 8);
        assert_eq!(partition_for_key(b"orders.7", 12), 7);
        assert_eq!(partition_for_key(b"21", 12), 0);
    }

    #[test]
    fn a_key_never_lands_on_a_negative_partition() {
        // `murmur2` returns something with the sign bit set about half the
        // time. Without the mask this is where it shows up.
        for n in 0..500u32 {
            let key = format!("key-{n}");
            let partition = partition_for_key(key.as_bytes(), 7);
            assert!(partition < 7, "`{key}` landed on {partition}");
        }
    }

    #[test]
    fn keys_spread_across_the_partitions() {
        // Not a distribution test — a "did the modulo happen" test. One
        // partition receiving everything is what a broken hash looks like.
        let mut seen = std::collections::HashSet::new();
        for n in 0..200u32 {
            seen.insert(partition_for_key(format!("user-{n}").as_bytes(), 8));
        }
        assert!(seen.len() > 5, "200 keys reached only {} of 8 partitions", seen.len());
    }

    #[test]
    fn a_position_round_trips_through_its_string() {
        let position = KafkaPosition { topic: "jobs".into(), partition: 3, offset: 4_982 };

        assert_eq!(position.to_string(), "jobs:3:4982");
        assert_eq!("jobs:3:4982".parse::<KafkaPosition>().unwrap(), position);
    }

    #[test]
    fn a_position_that_is_not_one_is_an_error_rather_than_a_panic() {
        assert!("jobs:3".parse::<KafkaPosition>().is_err());
        assert!("jobs:everywhere:4".parse::<KafkaPosition>().is_err());
        assert!("".parse::<KafkaPosition>().is_err());
    }

    #[test]
    fn credentials_do_not_print_the_password() {
        let credentials = KafkaCredentials::new(SaslMechanism::Plain, "svc-checkout", "hunter2");

        let printed = format!("{credentials:?}");
        assert!(printed.contains("svc-checkout"));
        assert!(!printed.contains("hunter2"), "{printed}");
    }

    #[test]
    fn a_record_carries_its_key_and_headers() {
        let record = KafkaRecord::new("body").keyed("orders.7").header("event", "OrderShipped");

        assert_eq!(record.key.as_deref(), Some(&b"orders.7"[..]));
        assert_eq!(record.headers.get("event").map(Vec::as_slice), Some(&b"OrderShipped"[..]));
    }

    #[test]
    fn the_next_offset_is_one_past_the_last_message() {
        let fetch = KafkaFetch { messages: vec![message(10), message(11)], high_watermark: 12 };

        assert_eq!(fetch.next_offset(), Some(12));
        assert!(!fetch.is_empty());
    }

    #[test]
    fn an_empty_fetch_has_nowhere_to_advance_to() {
        let fetch = KafkaFetch { messages: vec![], high_watermark: 12 };

        assert_eq!(fetch.next_offset(), None, "advancing past nothing would skip a record");
        assert!(fetch.is_empty());
    }

    fn message(offset: i64) -> KafkaMessage {
        KafkaMessage {
            topic: "jobs".into(),
            partition: 0,
            offset,
            key: None,
            value: vec![],
            headers: BTreeMap::new(),
            timestamp: Utc::now(),
        }
    }
}
