//! [`KafkaQueue`] — the [`Queue`] port over a partitioned log.
//!
//! # Read this before choosing it
//!
//! Kafka is not a job queue, and a driver that pretended otherwise would be
//! lying about things that cost money. It is a log: a consumer holds a cursor,
//! and there is no per-message acknowledgement, no redelivery of one message,
//! and no delayed delivery. Every awkward decision below follows from that.
//!
//! | The port says | Kafka's answer | So this driver |
//! |---|---|---|
//! | reserve one job | a cursor into a partition | owns partitions with a **lock**, one job in flight each |
//! | acknowledge it | advance the cursor | commits `offset + 1` to the cache |
//! | release it for later | *nothing* | **re-produces** it, at the end of the topic |
//! | fail it | *nothing* | produces to `{topic}.failed` |
//! | count what is waiting | high watermark − cursor | reports the lag |
//! | clear the queue | you cannot delete | skips to the end, and says how many |
//!
//! **Concurrency is the partition count.** Two workers cannot share a
//! partition — a cursor is one number, so a second worker reading the same
//! partition would run every job twice. A topic with six partitions supports
//! six concurrent jobs, and a seventh worker sits idle waiting for one of the
//! others to stop. That is Kafka's model, not a limitation of this code.
//!
//! **A delayed job blocks its partition.** `release(job, 30s)` re-produces the
//! job at the end of the topic, and a worker that reaches a job which is not
//! due yet stops reading that partition until it is. In a queue the delayed job
//! steps aside; in a log there is nowhere to step aside to.
//!
//! # When it is the right choice anyway
//!
//! When the jobs are already events on a topic somebody else produces, and
//! having them in a queue would mean a bridge process that exists only to move
//! them. When the ordering per key matters more than the concurrency —
//! everything for one account, in order, forever. When the same stream also
//! feeds analytics and nobody wants it written twice.
//!
//! When none of that is true, the [database driver](crate::database) is a
//! better job queue and needs no new infrastructure.
//!
//! # Ownership and cursors live in the cache
//!
//! This client does not join a consumer group — see
//! [`rainier_drivers::kafka`] for why — so partition ownership is a
//! [`LockManager`] lock and the cursor is a cache entry. Both need the
//! **shared, lock-capable** store an application already runs for
//! `on_one_server`, and [`KafkaQueue::new`] refuses one that is not, because
//! the failure otherwise is every worker owning every partition and every job
//! running on every machine.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use rainier_cache::{Cache, CacheExt, LockGuard, LockManager};
use rainier_drivers::kafka::{KafkaClient, KafkaOffset, KafkaPosition, KafkaRecord};
use rainier_support::{BoxFuture, Error, Result};
use serde_json::json;

use crate::job::QueuedJob;
use crate::queue::Queue;

/// Where `reserve` stashes the log position so `acknowledge` can find it.
const POSITION: &str = "__kafka_position";

/// The suffix of the topic a job goes to when it is out of attempts.
pub const FAILED_SUFFIX: &str = ".failed";

/// How long a partition lease lasts before another worker may take it.
///
/// Public, and read by [`ConnectionConfig`](crate::ConnectionConfig) rather than
/// copied into it: the check that a lease outlives the worker's timeout is
/// worthless if it compares against a stale duplicate of this number.
///
/// Note that this is **shorter** than the worker's own default timeout, so a
/// Kafka connection that declares no `lease` and a worker that takes its default
/// `timeout` are already the misconfiguration
/// [`Connections::check_reservations`](crate::Connections::check_reservations)
/// exists to name.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(60);

/// How long the partition list is trusted before being asked for again.
const METADATA_TTL: Duration = Duration::from_secs(30);

/// How long an attempt count outlives the job it counts.
const ATTEMPTS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Jobs on a Kafka topic.
///
/// ```no_run
/// use std::sync::Arc;
/// use rainier_cache::LockManager;
/// use rainier_drivers::kafka::{KafkaClient, KafkaConnector};
/// use rainier_queue::KafkaQueue;
///
/// # async fn wire(locks: LockManager) -> rainier_support::Result<()> {
/// let client = Arc::new(KafkaClient::connect(&KafkaConnector::parse("kafka:9092")).await?);
///
/// let queue = KafkaQueue::new(client, locks)?
///     .in_group("checkout-workers")
///     .with_topic_prefix("jobs.");
/// # let _ = queue; Ok(()) }
/// ```
pub struct KafkaQueue {
    client: std::sync::Arc<KafkaClient>,
    locks: LockManager,
    cache: std::sync::Arc<dyn Cache>,
    group: String,
    prefix: String,
    lease: Duration,
    max_wait: Duration,
    max_bytes: i32,
    /// The leases this worker holds, so a reserve renews rather than re-takes.
    held: tokio::sync::Mutex<HashMap<(String, i32), LockGuard>>,
    /// The partition list, refreshed occasionally rather than per reserve.
    partitions: Mutex<HashMap<String, (Instant, Vec<i32>)>>,
}

impl KafkaQueue {
    /// Jobs on `client`, with `locks` deciding who owns which partition.
    ///
    /// # Errors
    ///
    /// When the lock manager is not backed by a shared store — see
    /// [`require_shared`].
    pub fn new(client: std::sync::Arc<KafkaClient>, locks: LockManager) -> Result<Self> {
        require_shared(&locks)?;

        let cache = std::sync::Arc::clone(locks.cache());

        Ok(Self {
            client,
            locks,
            cache,
            group: "rainier".to_string(),
            prefix: String::new(),
            lease: DEFAULT_LEASE,
            // Short rather than zero: it catches a record that arrives while we
            // are asking without holding the worker's loop open. The worker
            // does the waiting when everything is empty.
            max_wait: Duration::from_millis(100),
            max_bytes: 1024 * 1024,
            held: tokio::sync::Mutex::new(HashMap::new()),
            partitions: Mutex::new(HashMap::new()),
        })
    }

    /// Which set of cursors this worker shares.
    ///
    /// A consumer group by another name: two deployments reading the same topic
    /// under different groups each get every job, and under the same group they
    /// share it out. Changing it makes a worker start over from the beginning
    /// of the topic.
    pub fn in_group(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
    }

    /// Prefix the topic a queue name maps to.
    ///
    /// `with_topic_prefix("jobs.")` puts the `default` queue on `jobs.default`,
    /// which is how a cluster shared with other things stays legible.
    pub fn with_topic_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// How long a partition lease lasts.
    ///
    /// **Must exceed the longest job.** The lease is what stops a second worker
    /// reading the same partition; if it expires while a job is still running,
    /// another worker takes the partition and runs the next job from a cursor
    /// that has not moved — so the job in flight runs twice.
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// How long a fetch waits for a record before answering empty.
    pub fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// The topic a queue name maps to.
    pub fn topic_for(&self, queue: &str) -> String {
        topic_name(&self.prefix, queue)
    }

    /// The topic exhausted jobs are produced to.
    pub fn failed_topic_for(&self, queue: &str) -> String {
        failed_topic_name(&self.prefix, queue)
    }

    /// The group whose cursors this worker shares.
    pub fn group(&self) -> &str {
        &self.group
    }

    /// The client, for something this port does not expose.
    pub fn client(&self) -> &std::sync::Arc<KafkaClient> {
        &self.client
    }

    /// Give up every partition this worker holds.
    ///
    /// Worth calling on shutdown: without it the partitions stay leased until
    /// the TTL lapses, and a rolling deploy leaves them idle for a minute.
    pub async fn release_partitions(&self) -> Result<()> {
        let held = std::mem::take(&mut *self.held.lock().await);

        for ((topic, partition), guard) in held {
            if !guard.release().await? {
                tracing::warn!(
                    topic,
                    partition,
                    "the lease on this partition had already expired — jobs may have run twice"
                );
            }
        }
        Ok(())
    }

    /// The partitions of `topic`, from a short-lived cache.
    ///
    /// Metadata is a round trip and the answer changes only when somebody
    /// repartitions a topic, which is not something that happens between two
    /// reserves.
    async fn partitions_of(&self, topic: &str) -> Result<Vec<i32>> {
        if let Some(fresh) = self.cached_partitions(topic) {
            return Ok(fresh);
        }

        let partitions = self.client.partitions(topic).await?.ok_or_else(|| {
            Error::service_unavailable(format!(
                "the Kafka topic `{topic}` does not exist. Create it — a queue that creates its \
                 own topics decides their partition count by accident."
            ))
        })?;

        self.partitions
            .lock()
            .expect("kafka partitions poisoned")
            .insert(topic.to_string(), (Instant::now(), partitions.clone()));

        Ok(partitions)
    }

    fn cached_partitions(&self, topic: &str) -> Option<Vec<i32>> {
        self.partitions
            .lock()
            .expect("kafka partitions poisoned")
            .get(topic)
            .filter(|(read, _)| read.elapsed() < METADATA_TTL)
            .map(|(_, partitions)| partitions.clone())
    }

    /// Take or renew the lease on one partition. `false` if somebody else has it.
    async fn own(&self, topic: &str, partition: i32) -> Result<bool> {
        let key = (topic.to_string(), partition);
        let mut held = self.held.lock().await;

        if let Some(guard) = held.get(&key) {
            // Still ours: push the expiry out rather than taking it again.
            if guard.extend(self.lease).await? {
                return Ok(true);
            }

            // The lease lapsed and somebody else may now hold it. Whatever we
            // were doing on this partition, we were doing it alongside them.
            tracing::warn!(topic, partition, "lost the lease on this partition mid-flight");
            held.remove(&key);
        }

        let lock = self.locks.lock(lease_name(&self.group, topic, partition), self.lease);

        match lock.acquire().await? {
            Some(guard) => {
                held.insert(key, guard);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Where this group has read up to in a partition.
    ///
    /// `None` means never read, and the caller starts at the **earliest**
    /// retained record rather than the latest: a job pushed before the first
    /// worker started is still a job, and starting at the end would silently
    /// drop everything queued during a deploy.
    async fn cursor(&self, topic: &str, partition: i32) -> Result<Option<i64>> {
        let raw = self.cache.get_string(&cursor_key(&self.group, topic, partition)).await?;

        raw.map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| Error::internal(format!("`{value}` is not a Kafka cursor")))
        })
        .transpose()
    }

    /// Commit a cursor.
    async fn commit(&self, topic: &str, partition: i32, offset: i64) -> Result<()> {
        self.cache
            .put_string(&cursor_key(&self.group, topic, partition), &offset.to_string(), None)
            .await
    }

    /// Where to start reading a partition.
    async fn start_of(&self, topic: &str, partition: i32) -> Result<i64> {
        match self.cursor(topic, partition).await? {
            Some(offset) => Ok(offset),
            None => self.client.offset(topic, partition, KafkaOffset::Earliest).await,
        }
    }

    /// How many times the job at `position` has been handed to a worker.
    ///
    /// Kafka has no delivery counter, and without one a job that kills its
    /// worker is redelivered from the same cursor forever — the poison pill
    /// that takes a partition down and never reports why. Counting deliveries
    /// against the offset makes the fourth one the last one.
    async fn count_attempt(&self, position: &KafkaPosition) -> Result<u32> {
        let key = attempts_key(&self.group, position);
        let attempts = self.cache.increment(&key, 1).await?;

        // Only the first write needs the TTL; the rest are increments on a key
        // that already has one.
        if attempts == 1 {
            let _ = self.cache.put_string(&key, "1", Some(ATTEMPTS_TTL)).await;
        }

        Ok(attempts.max(1) as u32)
    }

    /// Forget the attempt count for a job that is done with.
    async fn forget_attempts(&self, position: &KafkaPosition) {
        let _ = self.cache.forget(&attempts_key(&self.group, position)).await;
    }

    /// Move past the job at `position`, whatever became of it.
    async fn advance_past(&self, position: &KafkaPosition) -> Result<()> {
        self.forget_attempts(position).await;
        self.commit(&position.topic, position.partition, position.offset + 1).await
    }
}

/// Refuse a lock store that cannot actually exclude another worker.
///
/// # Errors
///
/// When the store is process-local. Every worker would own every partition and
/// every job would run on every machine — the failure that looks like "the
/// queue is fast" until somebody notices the emails went out four times.
pub fn require_shared(locks: &LockManager) -> Result<()> {
    if locks.is_shared() {
        return Ok(());
    }

    Err(Error::internal(
        "a Kafka queue needs a shared lock store to decide which worker owns which partition.          With an in-memory one every worker owns every partition and every job runs on every          machine — configure Redis, or call `declared_shared()` if this store really is shared.",
    ))
}

/// The topic a queue name maps to.
fn topic_name(prefix: &str, queue: &str) -> String {
    format!("{prefix}{queue}")
}

/// The topic a queue's exhausted jobs are produced to.
fn failed_topic_name(prefix: &str, queue: &str) -> String {
    format!("{}{FAILED_SUFFIX}", topic_name(prefix, queue))
}

/// Which attempt this delivery is.
///
/// Two counts, added, and both halves are load-bearing:
///
/// `recorded` is what the record itself says, which is how a **retry** carries
/// its history — a released job is a *new record at a new offset*, so a count
/// kept only against the offset would reset to one and the job would retry
/// until the topic's retention removed it.
///
/// `delivered` is how many times this offset has been handed to a worker,
/// which is how a **crash** counts — Kafka redelivers from an unmoved cursor
/// and says nothing about having done so before.
fn attempt_number(recorded: u32, delivered: u32) -> u32 {
    recorded.saturating_add(delivered).max(1)
}

/// The lock name one partition's lease is held under.
fn lease_name(group: &str, topic: &str, partition: i32) -> String {
    format!("kafka:{group}:{topic}:{partition}")
}

/// The cache key one partition's cursor is stored under.
fn cursor_key(group: &str, topic: &str, partition: i32) -> String {
    format!("kafka-cursor:{group}:{topic}:{partition}")
}

/// The cache key one job's delivery count is stored under.
fn attempts_key(group: &str, position: &KafkaPosition) -> String {
    format!("kafka-attempts:{group}:{}:{}:{}", position.topic, position.partition, position.offset)
}

/// The record a job becomes.
///
/// Keyed by the job's uniqueness key when it declared one, and by its id
/// otherwise. That is not arbitrary: a key decides the partition, so jobs
/// sharing a `unique_id` run **in order, on one partition**, which is usually
/// exactly what a job about one account or one document wants.
fn record_for(job: &QueuedJob) -> Result<KafkaRecord> {
    let body = serde_json::to_vec(job)
        .map_err(|e| Error::internal(format!("a job must serialise: {e}")))?;

    let key = job.unique_key.clone().unwrap_or_else(|| job.id.clone());

    Ok(KafkaRecord::new(body)
        .keyed(key)
        .header("job", job.name.clone())
        .header("attempts", job.attempts.to_string()))
}

/// The position [`Queue::reserve`] stashed on a job.
fn position_of(job: &QueuedJob) -> Result<KafkaPosition> {
    job.payload
        .get(POSITION)
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            Error::internal("this job carries no Kafka position, so it was not reserved from Kafka")
        })?
        .parse()
}

impl Queue for KafkaQueue {
    fn name(&self) -> &str {
        "kafka"
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let topic = self.topic_for(&job.queue);
            let id = job.id.clone();

            self.client.produce(&topic, vec![record_for(&job)?]).await?;

            Ok(id)
        })
    }

    fn reserve<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move {
            let topic = self.topic_for(queue);

            for partition in self.partitions_of(&topic).await? {
                if !self.own(&topic, partition).await? {
                    continue;
                }

                let from = self.start_of(&topic, partition).await?;

                let Some(fetch) = self
                    .client
                    .fetch(&topic, partition, from, self.max_bytes, self.max_wait)
                    .await?
                else {
                    // The cursor is no longer in the log: retention removed
                    // what we had not read. Those jobs are gone whatever we do,
                    // and rejoining at the earliest surviving record is the
                    // only way to make progress.
                    let earliest =
                        self.client.offset(&topic, partition, KafkaOffset::Earliest).await?;

                    tracing::error!(
                        topic,
                        partition,
                        from,
                        earliest,
                        "jobs were lost to retention before a worker read them"
                    );
                    self.commit(&topic, partition, earliest).await?;
                    continue;
                };

                let Some(message) = fetch.messages.into_iter().next() else {
                    continue;
                };

                let position = message.position();

                let mut job: QueuedJob = match serde_json::from_slice(&message.value) {
                    Ok(job) => job,
                    Err(e) => {
                        // Not a job. Skipping it is the only option that keeps
                        // the partition moving, and a log keeps the record
                        // itself, so nothing is actually lost by doing so.
                        tracing::error!(
                            topic,
                            partition,
                            offset = position.offset,
                            error = %e,
                            "skipping a record on a job topic that is not a job"
                        );
                        self.advance_past(&position).await?;
                        continue;
                    }
                };

                if !job.is_available() {
                    // A log has no way to set one record aside, so the partition
                    // waits. Which is also the ordering guarantee working as
                    // intended: nothing behind a delayed job may overtake it.
                    tracing::debug!(
                        topic,
                        partition,
                        until = %job.available_at,
                        "the next job on this partition is not due yet"
                    );
                    continue;
                }

                job.attempts = attempt_number(job.attempts, self.count_attempt(&position).await?);
                job.payload[POSITION] = json!(position.to_string());

                return Ok(Some(job));
            }

            Ok(None)
        })
    }

    fn acknowledge<'a>(&'a self, job: &'a QueuedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.advance_past(&position_of(job)?).await })
    }

    fn release<'a>(&'a self, job: &'a QueuedJob, delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let position = position_of(job)?;

            // A cursor cannot go backwards for one record, so a retry is a new
            // record at the end of the topic. It keeps its attempt count and
            // its id; what it loses is its place in the order.
            let mut retry = job.clone();
            retry.payload.as_object_mut().map(|payload| payload.remove(POSITION));
            retry.available_at = Utc::now()
                + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());

            let topic = self.topic_for(&job.queue);
            let record = record_for(&retry)?.header("retry-of", position.to_string());

            self.client.produce(&topic, vec![record]).await?;

            // Only after the retry is safely on the topic: the other order
            // loses the job if this process dies in between.
            self.advance_past(&position).await
        })
    }

    fn fail<'a>(&'a self, job: &'a QueuedJob, error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let position = position_of(job)?;
            let failed = self.failed_topic_for(&job.queue);

            tracing::error!(
                job = %job.name,
                id = %job.id,
                attempts = job.attempts,
                topic = %failed,
                %error,
                "a job failed after its last attempt"
            );

            let record = record_for(job)?
                .header("error", error.to_string())
                .header("failed-at", Utc::now().to_rfc3339())
                .header("failed-from", position.to_string());

            // Best effort: a dead-letter topic that does not exist must not
            // stop the partition, or one failure wedges the queue.
            if let Err(e) = self.client.produce(&failed, vec![record]).await {
                tracing::error!(
                    topic = %failed,
                    error = %e,
                    "could not record a failed job — create the topic to keep them"
                );
            }

            self.advance_past(&position).await
        })
    }

    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let topic = self.topic_for(queue);
            let mut waiting = 0i64;

            for partition in self.partitions_of(&topic).await? {
                let end = self.client.offset(&topic, partition, KafkaOffset::Latest).await?;
                let at = self.start_of(&topic, partition).await?;
                waiting += (end - at).max(0);
            }

            Ok(waiting as u64)
        })
    }

    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let topic = self.topic_for(queue);
            let mut skipped = 0i64;

            // Nothing is deleted — a log cannot forget on request, and its
            // retention policy is the only thing that removes a record. What
            // this does is move every cursor to the end, so the jobs stop being
            // *this group's* problem. They are still there.
            for partition in self.partitions_of(&topic).await? {
                let end = self.client.offset(&topic, partition, KafkaOffset::Latest).await?;
                let at = self.start_of(&topic, partition).await?;

                skipped += (end - at).max(0);
                self.commit(&topic, partition, end).await?;
            }

            Ok(skipped as u64)
        })
    }
}

impl std::fmt::Debug for KafkaQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaQueue")
            .field("group", &self.group)
            .field("prefix", &self.prefix)
            .field("lease", &self.lease)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;
    use std::sync::Arc;

    fn locks() -> LockManager {
        // The memory cache is not shared, and `KafkaQueue::new` refuses one —
        // so a test that wants a queue says the store is shared on purpose.
        LockManager::new(Arc::new(MemoryCache::new())).declared_shared()
    }

    fn job() -> QueuedJob {
        QueuedJob {
            id: "job-1".into(),
            name: "send-invoice".into(),
            payload: json!({ "invoice": 4 }),
            queue: "default".into(),
            attempts: 0,
            max_attempts: 3,
            available_at: Utc::now(),
            created_at: Utc::now(),
            unique_key: None,
            delivery_handle: None,
        }
    }

    #[test]
    fn a_lock_store_that_is_not_shared_is_refused() {
        // The failure this prevents is silent and expensive: every worker owns
        // every partition, so every job runs on every machine.
        let plain = LockManager::new(Arc::new(MemoryCache::new()));

        let error = require_shared(&plain).unwrap_err();
        assert!(error.message().contains("shared lock store"), "{}", error.message());

        // And an application that knows better can say so.
        assert!(require_shared(&locks()).is_ok());
    }

    #[test]
    fn a_topic_is_the_prefix_and_the_queue_name() {
        assert_eq!(topic_name("jobs.", "emails"), "jobs.emails");
        assert_eq!(topic_name("", "default"), "default", "no prefix is the queue name itself");
    }

    #[test]
    fn the_cursor_and_lease_keys_name_the_group_topic_and_partition() {
        // Two groups on one topic must not share either, or one deployment's
        // progress becomes the other's.
        assert_eq!(
            cursor_key("checkout", "jobs.default", 3),
            "kafka-cursor:checkout:jobs.default:3"
        );
        assert_eq!(lease_name("checkout", "jobs.default", 3), "kafka:checkout:jobs.default:3");

        assert_ne!(
            cursor_key("checkout", "jobs.default", 3),
            cursor_key("analytics", "jobs.default", 3)
        );
    }

    #[test]
    fn an_attempts_key_is_per_offset() {
        let first = KafkaPosition { topic: "jobs".into(), partition: 0, offset: 10 };
        let second = KafkaPosition { topic: "jobs".into(), partition: 0, offset: 11 };

        assert_ne!(attempts_key("g", &first), attempts_key("g", &second));
        assert_eq!(attempts_key("g", &first), "kafka-attempts:g:jobs:0:10");
    }

    #[test]
    fn a_job_is_keyed_by_its_id_so_jobs_spread_across_partitions() {
        let record = record_for(&job()).unwrap();

        assert_eq!(record.key.as_deref(), Some(&b"job-1"[..]));
        assert_eq!(record.headers.get("job").map(Vec::as_slice), Some(&b"send-invoice"[..]));
    }

    #[test]
    fn a_unique_job_is_keyed_by_its_uniqueness_so_it_keeps_its_order() {
        // The useful consequence: every job for one account lands on one
        // partition and therefore runs in the order it was queued.
        let mut job = job();
        job.unique_key = Some("account-7".into());

        assert_eq!(record_for(&job).unwrap().key.as_deref(), Some(&b"account-7"[..]));
    }

    #[test]
    fn a_job_that_was_not_reserved_from_kafka_has_no_position() {
        let error = position_of(&job()).unwrap_err();

        assert!(error.message().contains("not reserved from Kafka"), "{}", error.message());
    }

    #[test]
    fn a_position_survives_the_round_trip_through_the_payload() {
        let mut job = job();
        let position = KafkaPosition { topic: "jobs.default".into(), partition: 2, offset: 91 };
        job.payload[POSITION] = json!(position.to_string());

        assert_eq!(position_of(&job).unwrap(), position);
    }

    #[tokio::test]
    async fn a_partition_can_only_be_leased_by_one_worker() {
        // The property the whole design rests on. Two workers, one partition:
        // the second must not get it, or every job runs twice.
        let manager = locks();
        let name = lease_name("checkout", "jobs.default", 0);

        let first = manager.lock(name.clone(), Duration::from_secs(60)).acquire().await.unwrap();
        assert!(first.is_some(), "the first worker takes the partition");

        let second = manager.lock(name.clone(), Duration::from_secs(60)).acquire().await.unwrap();
        assert!(second.is_none(), "the second worker must not");

        first.unwrap().release().await.unwrap();

        let third = manager.lock(name, Duration::from_secs(60)).acquire().await.unwrap();
        assert!(third.is_some(), "and can take it once the first lets go");
    }

    #[tokio::test]
    async fn a_cursor_is_read_back_as_the_number_it_was_written_as() {
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let key = cursor_key("checkout", "jobs.default", 1);

        assert_eq!(cache.get_string(&key).await.unwrap(), None, "never read is not offset zero");

        cache.put_string(&key, "4982", None).await.unwrap();
        assert_eq!(cache.get_string(&key).await.unwrap().unwrap().parse::<i64>().unwrap(), 4982);
    }

    #[tokio::test]
    async fn attempts_are_counted_per_offset_so_a_poison_pill_gives_up() {
        // Kafka redelivers from the same cursor after a crash and has no
        // delivery counter, so without this a job that kills its worker is
        // retried forever and the partition never moves.
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let position = KafkaPosition { topic: "jobs".into(), partition: 0, offset: 10 };
        let key = attempts_key("g", &position);

        assert_eq!(cache.increment(&key, 1).await.unwrap(), 1);
        assert_eq!(cache.increment(&key, 1).await.unwrap(), 2);
        assert_eq!(cache.increment(&key, 1).await.unwrap(), 3);

        cache.forget(&key).await.unwrap();
        assert_eq!(cache.increment(&key, 1).await.unwrap(), 1, "acknowledged, so it starts over");
    }

    #[test]
    fn an_attempt_counts_the_record_and_the_delivery() {
        // A fresh job, delivered once.
        assert_eq!(attempt_number(0, 1), 1);

        // The same record handed out again after a worker died holding it —
        // Kafka redelivers from a cursor that never moved.
        assert_eq!(attempt_number(0, 2), 2);

        // A retry: a new record at a new offset, so its own delivery count
        // starts at one. Without the recorded half this would say "attempt 1"
        // forever and the job would never reach its last one.
        assert_eq!(attempt_number(1, 1), 2);
        assert_eq!(attempt_number(2, 1), 3);
    }

    #[test]
    fn a_retry_loses_its_position_so_it_cannot_be_acknowledged_twice() {
        let mut job = job();
        job.payload[POSITION] = json!("jobs.default:0:5");

        let mut retry = job.clone();
        retry.payload.as_object_mut().map(|payload| payload.remove(POSITION));

        assert!(position_of(&retry).is_err(), "the retry is a new record, not the old one");
        assert!(position_of(&job).is_ok());
    }

    #[test]
    fn the_failed_topic_is_the_queues_topic_and_a_suffix() {
        assert_eq!(failed_topic_name("jobs.", "default"), "jobs.default.failed");
        assert_eq!(failed_topic_name("", "emails"), "emails.failed");
    }
}
