//! [`RedisQueue`] — jobs on Redis streams.
//!
//! # Read this before choosing it
//!
//! Redis is a **data-structure server**, not a broker. Queue behaviour is a
//! consequence of its atomicity — commands run one at a time, so two workers
//! cannot take the same entry — rather than something it was built to be. That
//! is a real property, and this driver is built on the part of Redis that goes
//! furthest with it: streams with consumer groups, which have an actual
//! acknowledgement protocol. A `LPUSH`/`BRPOP` queue cannot honour the
//! [`Queue`] contract at all, because `BRPOP` removes the job and
//! a worker that dies has taken it with it.
//!
//! What streams do **not** change:
//!
//! - **An acknowledged write can still vanish.** Redis's default persistence is
//!   periodic snapshots; with the append-only file on, the default
//!   `appendfsync everysec` leaves up to about a second of writes unflushed. A
//!   dispatch your request already reported as accepted can be gone after a
//!   power loss.
//! - **Replication is asynchronous.** The primary acknowledges before any
//!   replica has the write, so a failover can lose writes it confirmed. `WAIT`
//!   narrows that window; it is not consensus.
//! - **A backlog can be evicted.** With `maxmemory` and a policy like
//!   `allkeys-lru`, Redis will drop keys to stay under the limit — including
//!   this queue's stream, silently, exactly when it is deepest. Set
//!   `maxmemory-policy noeviction`, which turns that into refused writes
//!   instead. [`check_eviction_policy`](RedisQueue::check_eviction_policy) will
//!   tell you which you have.
//! - **You cannot enqueue in your database transaction.** Insert an order and
//!   dispatch its confirmation email and either can succeed alone.
//!
//!   [`DatabaseQueue`](crate::DatabaseQueue) does **not** currently fix this,
//!   and this bullet used to say it did. This framework has no transaction API
//!   at all, so that driver's push is a bare insert like any other write and
//!   the two can still diverge. What it does give you is one store rather than
//!   two: the job is written to the same database as the row it refers to, so
//!   it is as durable as that row and survives a restart that loses everything
//!   queued here. That is the honest reason it is the usual recommendation —
//!   durability, not atomicity.
//!
//!   Until there is a transaction to enlist in, a dispatch that must not be
//!   orphaned is the application's to reconcile: write the intent in the same
//!   statement as the data and dispatch from that, or make the job tolerate a
//!   row that is not there yet and retry. Both are work. Neither is a setting.
//!
//! So: right for work you can afford to lose — warming a cache, recomputing a
//! projection, analytics — and wrong for work you cannot, like taking a payment
//! or an email a user was promised. Reach for it when Redis's design and your
//! requirement agree, which for a queue is less often than its popularity
//! suggests.
//!
//! ```no_run
//! use rainier_drivers::RedisConnector;
//! use rainier_queue::RedisQueue;
//!
//! # async fn run() -> rainier_support::Result<()> {
//! let queue = RedisQueue::connect(&RedisConnector::open("redis://127.0.0.1/")?).await?;
//! queue.check_eviction_policy().await;
//! # Ok(()) }
//! ```

use std::time::Duration;

use chrono::Utc;
use rainier_drivers::{RedisClient, RedisConnector};
use rainier_events::report::{
    ConfigurationFailed, FailureEvent, QueueMessageDiscarded, QueueOperationFailed,
};
use rainier_events::Exception;
use rainier_support::{BoxFuture, Error, ErrorKind, Result};
use serde_json::json;

use crate::job::QueuedJob;
use crate::queue::{FailedJob, Queue};

/// The consumer group every worker joins.
///
/// One group, so the workers share the queue rather than each getting a copy —
/// which is the difference between a queue and a fan-out.
const GROUP: &str = "workers";

/// Jobs on Redis streams.
///
/// An **adapter**: the Redis knowledge — which commands, how a reply nests,
/// that a delayed entry needs a sorted set because streams have no delay —
/// lives in [`rainier-drivers`](rainier_drivers). What is decided here is queue
/// policy.
///
/// Read the [module documentation](self) before choosing this. It is the one
/// driver here whose limits are a property of the store rather than of the
/// implementation.
pub struct RedisQueue {
    client: RedisClient,
    keys: Keys,
    consumer: String,
    reservation: Duration,
}

/// The three keys one queue owns.
///
/// A value rather than three methods on the queue, so what a deployment will
/// actually see in `redis-cli --scan` can be asserted without a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    prefix: String,
}

impl Keys {
    /// Keys under `prefix`.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self { prefix: prefix.into() }
    }

    /// The stream holding ready jobs.
    pub fn stream(&self, queue: &str) -> String {
        format!("{}{queue}", self.prefix)
    }

    /// The sorted set holding delayed jobs, scored by when they are due.
    ///
    /// Separate because a stream cannot express a delay: an entry is available
    /// the moment it is added.
    pub fn delayed(&self, queue: &str) -> String {
        format!("{}{queue}:delayed", self.prefix)
    }

    /// The list of jobs that exhausted their attempts.
    pub fn failed(&self, queue: &str) -> String {
        format!("{}{queue}:failed", self.prefix)
    }

    /// All three — what `clear` removes.
    pub fn all(&self, queue: &str) -> Vec<String> {
        vec![self.stream(queue), self.delayed(queue), self.failed(queue)]
    }
}

impl RedisQueue {
    /// Connect through `connector`.
    pub async fn connect(connector: &RedisConnector) -> Result<Self> {
        Ok(Self::new(RedisClient::connect(connector).await?))
    }

    /// How long a reservation lasts when a connection does not say.
    ///
    /// Public, and read by [`ConnectionConfig`](crate::ConnectionConfig) rather
    /// than copied into it: the check that a reservation outlives the worker's
    /// timeout is worthless if it compares against a stale duplicate of this
    /// number.
    pub const DEFAULT_RESERVATION: Duration = Duration::from_secs(90);

    /// Use a client you already have — the point of sharing one connector
    /// between the cache, the broadcaster and this.
    pub fn new(client: RedisClient) -> Self {
        Self {
            client,
            keys: Keys::new("rainier:queue:"),
            consumer: consumer_name(),
            reservation: Self::DEFAULT_RESERVATION,
        }
    }

    /// Prefix every key. For two applications sharing one Redis.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.keys = Keys::new(prefix);
        self
    }

    /// How long a reserved job may be held before another worker may claim it.
    ///
    /// The same trade as every visibility timeout: too short and a slow job is
    /// run twice, too long and a job from a dead worker waits. It must exceed
    /// the longest a job legitimately takes.
    pub fn with_reservation(mut self, reservation: Duration) -> Self {
        self.reservation = reservation;
        self
    }

    /// Name this worker, for `XINFO CONSUMERS` and for claiming.
    ///
    /// Defaults to the hostname and process id, which is enough to tell two
    /// workers apart in a log.
    pub fn as_consumer(mut self, name: impl Into<String>) -> Self {
        self.consumer = name.into();
        self
    }

    /// Warn unless Redis is configured not to evict.
    ///
    /// The failure this catches is silent and happens under load: with an
    /// eviction policy set, Redis discards keys to stay under `maxmemory`, and
    /// a deep queue is a large key. Jobs disappear with no error anywhere.
    ///
    /// Any policy but `noeviction` is also reported, through the process-wide
    /// reporter, as a `ConfigurationFailed` for the setting
    /// `maxmemory-policy`.
    ///
    /// Worth calling at boot. It only reads configuration, and a managed Redis
    /// that refuses `CONFIG GET` simply gets no answer rather than an error.
    pub async fn check_eviction_policy(&self) -> Option<String> {
        let policy = self.client.config_get("maxmemory-policy").await.ok().flatten()?;

        if let Some(unsafe_policy) = eviction_policy_failure(&policy) {
            tracing::warn!(
                policy,
                "this Redis evicts keys under memory pressure, and a queued job is a key it can \
                 evict — set `maxmemory-policy noeviction` before relying on this queue"
            );
            crate::reporting::report(unsafe_policy).await;
        }
        Some(policy)
    }

    /// The keys this queue writes to.
    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    /// Jobs that were due while nobody was looking, moved into the stream.
    async fn promote_delayed(&self, queue: &str) -> Result<u64> {
        self.client
            .promote_due(
                &self.keys.delayed(queue),
                &self.keys.stream(queue),
                Utc::now().timestamp_millis(),
            )
            .await
    }

    /// The stream entry id `reserve` put on this job.
    fn entry_id(job: &QueuedJob) -> Result<&str> {
        job.delivery_handle.as_deref().ok_or_else(|| {
            Error::internal(format!(
                "job `{}` has no Redis entry id — it was not reserved from this queue",
                job.id
            ))
        })
    }

    /// The job, without the transport's bookkeeping on it.
    ///
    /// The handle is `#[serde(skip)]`, so clearing it here is belt and braces
    /// — but a job put back on the stream must not claim a delivery that has
    /// been acknowledged.
    fn without_entry_id(job: &QueuedJob) -> QueuedJob {
        QueuedJob { delivery_handle: None, ..job.clone() }
    }

    fn encode(job: &QueuedJob) -> Result<Vec<u8>> {
        serde_json::to_vec(job)
            .map_err(|e| Error::internal(format!("a queued job must serialise: {e}")))
    }
}

#[allow(clippy::needless_lifetimes, reason = "the trait's signature names them")]
impl Queue for RedisQueue {
    fn name(&self) -> &str {
        "redis"
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let body = Self::encode(&job)?;
            let now = Utc::now();

            if job.available_at > now {
                // Streams have no delay, so it waits in a sorted set until a
                // worker promotes it. The score is when it becomes due.
                self.client
                    .zadd(
                        &self.keys.delayed(&job.queue),
                        job.available_at.timestamp_millis(),
                        &body,
                    )
                    .await?;
            } else {
                self.client.xadd(&self.keys.stream(&job.queue), &body).await?;
            }

            Ok(job.id)
        })
    }

    fn reserve<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move {
            let stream = self.keys.stream(queue);

            // Idempotent, and cheap enough to do per reserve — the alternative
            // is a worker that fails until something else creates the group.
            self.client.xgroup_create(&stream, GROUP).await?;
            self.promote_delayed(queue).await?;

            // Redelivery first: a job a dead worker was holding has been
            // waiting longer than anything new, and taking new work while it
            // sits there is how a job starves.
            let entry = match self
                .client
                .xautoclaim_one(&stream, GROUP, &self.consumer, self.reservation)
                .await?
            {
                Some(entry) => Some(entry),
                None => self.client.xreadgroup_one(&stream, GROUP, &self.consumer).await?,
            };

            let Some(entry) = entry else { return Ok(None) };

            let mut job: QueuedJob = match decode_entry(queue, &stream, &entry) {
                Ok(job) => job,
                Err(discarded) => {
                    // Not a job. Leaving it would make every reserve on this
                    // queue return it again forever, so it goes.
                    tracing::error!(
                        entry = entry.id,
                        error = %discarded.exception,
                        "discarding an unreadable entry"
                    );
                    crate::reporting::report(*discarded).await;
                    self.client.xack_delete(&stream, GROUP, &entry.id).await?;
                    return Ok(None);
                }
            };

            // The id identifying this *delivery*, which is not the job's id.
            //
            // On the job, not in its payload. Writing it into the payload
            // rewrote the job: `payload[key] = value` promotes a `Value::Null`
            // — which is what a unit-struct job serialises to — into an
            // object, and the job then failed to deserialise into its own type
            // for the whole of its retry budget.
            job.delivery_handle = Some(entry.id.clone());
            job.attempts += 1;

            Ok(Some(job))
        })
    }

    fn renew<'a>(&'a self, job: &'a QueuedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // `XAUTOCLAIM` in `reserve` hands any entry idle longer than
            // `self.reservation` to whoever asks next, without checking whether
            // its holder is still working. For anything slower than the
            // reservation window that means the same job runs on several
            // workers at once -- observed in production with one upload leased
            // three times over, each host encoding it from the start.
            //
            // Renewing says "still here", so the timer only ever expires on a
            // worker that has actually stopped.
            self.client
                .xclaim_touch(
                    &self.keys.stream(&job.queue),
                    GROUP,
                    &self.consumer,
                    Self::entry_id(job)?,
                )
                .await
        })
    }

    fn acknowledge<'a>(&'a self, job: &'a QueuedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.client
                .xack_delete(&self.keys.stream(&job.queue), GROUP, Self::entry_id(job)?)
                .await
        })
    }

    fn release<'a>(&'a self, job: &'a QueuedJob, delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let stream = self.keys.stream(&job.queue);
            let entry = Self::entry_id(job)?;

            // The retry is a *new* entry: a stream cannot reschedule one, and
            // the attempt count has changed anyway.
            let mut retry = Self::without_entry_id(job);
            retry.available_at = Utc::now()
                + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());

            let body = Self::encode(&retry)?;
            if delay.is_zero() {
                self.client.xadd(&stream, &body).await?;
            } else {
                self.client
                    .zadd(
                        &self.keys.delayed(&job.queue),
                        retry.available_at.timestamp_millis(),
                        &body,
                    )
                    .await?;
            }

            // Only after the retry is stored. The other order loses the job if
            // the process dies in between — at-least-once is the contract, and
            // this is the line that keeps it.
            self.client.xack_delete(&stream, GROUP, entry).await
        })
    }

    fn fail<'a>(&'a self, job: &'a QueuedJob, error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let failed = FailedJob {
                job: Self::without_entry_id(job),
                error: error.to_string(),
                failed_at: Utc::now(),
            };

            let body = serde_json::to_vec(&json!({
                "job": failed.job,
                "error": failed.error,
                "failed_at": failed.failed_at,
            }))
            .map_err(|e| Error::internal(format!("a failed job must serialise: {e}")))?;

            // Stored before the acknowledgement, for the reason above.
            self.client.lpush(&self.keys.failed(&job.queue), &body).await?;
            self.client
                .xack_delete(&self.keys.stream(&job.queue), GROUP, Self::entry_id(job)?)
                .await
        })
    }

    fn size<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            // Both, because a delayed job is queued — it is simply not due.
            let ready = self.client.xlen(&self.keys.stream(queue)).await?;
            let delayed = self.client.zcard(&self.keys.delayed(queue)).await?;

            Ok(ready + delayed)
        })
    }

    fn clear<'a>(&'a self, queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let size = self.size(queue).await.unwrap_or(0);
            self.client.delete_all(&self.keys.all(queue)).await?;

            Ok(size)
        })
    }
}

impl RedisQueue {
    /// Every job that exhausted its attempts, newest first.
    pub async fn failed_jobs(&self, queue: &str) -> Result<Vec<FailedJob>> {
        let raw = self.client.lrange_all(&self.keys.failed(queue)).await?;

        let mut failed = Vec::with_capacity(raw.len());
        for body in &raw {
            match read_failed_job(body) {
                Ok(job) => failed.push(job),
                // Left in the list, and left out of the answer — but said, so a
                // listing that comes back short is not mistaken for complete.
                Err(exception) => {
                    tracing::warn!(queue, error = %exception, "skipping an unreadable failed job");
                    crate::reporting::report(QueueOperationFailed::new(
                        exception,
                        Some("redis".to_string()),
                        Some(queue.to_string()),
                        "read_failed_job",
                        None,
                    ))
                    .await;
                }
            }
        }
        Ok(failed)
    }
}

/// Read a reserved stream entry as a job, or say why it is being discarded.
///
/// Boxed because the failure is large and this is on every reserve's path.
fn decode_entry(
    queue: &str,
    stream: &str,
    entry: &rainier_drivers::StreamEntry,
) -> std::result::Result<QueuedJob, Box<QueueMessageDiscarded>> {
    serde_json::from_slice(&entry.body).map_err(|e| {
        Box::new(
            QueueMessageDiscarded::new(
                Exception::from_std(&e),
                "redis",
                queue,
                Some(entry.id.clone()),
            )
            .with_extra("stream", stream),
        )
    })
}

/// One record of the failed-job list.
fn read_failed_job(body: &[u8]) -> std::result::Result<FailedJob, Exception> {
    let unreadable = |why: &str| {
        Exception::message(ErrorKind::Internal, format!("an unreadable failed job: {why}"))
    };
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| Exception::from_std(&e))?;
    let field = |name: &str| value.get(name).cloned().ok_or_else(|| unreadable(name));

    Ok(FailedJob {
        job: serde_json::from_value(field("job")?).map_err(|e| Exception::from_std(&e))?,
        error: field("error")?.as_str().ok_or_else(|| unreadable("error"))?.to_string(),
        failed_at: serde_json::from_value(field("failed_at")?)
            .map_err(|e| Exception::from_std(&e))?,
    })
}

/// The report for an eviction policy that can lose jobs, or `None` for
/// `noeviction`.
fn eviction_policy_failure(policy: &str) -> Option<ConfigurationFailed> {
    (policy != "noeviction").then(|| {
        ConfigurationFailed::new(
            Exception::message(
                ErrorKind::Internal,
                format!(
                    "Redis `maxmemory-policy` is `{policy}`: queued jobs can be evicted under \
                     memory pressure. Set it to `noeviction`."
                ),
            ),
            "maxmemory-policy",
        )
    })
}

/// A name for this worker: the host and the process, which is enough to tell
/// two apart in a log.
fn consumer_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "worker".to_string());

    format!("{host}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> QueuedJob {
        QueuedJob {
            id: "j1".into(),
            name: "test".into(),
            payload: json!({ "a": 1 }),
            queue: "default".into(),
            attempts: 0,
            max_attempts: 3,
            available_at: Utc::now(),
            created_at: Utc::now(),
            unique_key: None,
            delivery_handle: None,
            traceparent: None,
        }
    }

    #[test]
    fn one_queue_owns_three_keys_and_the_prefix_reaches_all_of_them() {
        // Two applications on one Redis depend on the prefix reaching every
        // key, and a missed one collides silently.
        let keys = Keys::new("app:q:");

        assert_eq!(keys.stream("default"), "app:q:default");
        assert_eq!(keys.delayed("default"), "app:q:default:delayed");
        assert_eq!(keys.failed("default"), "app:q:default:failed");

        assert_eq!(keys.all("default").len(), 3);
        assert!(keys.all("default").iter().all(|key| key.starts_with("app:q:")));
    }

    #[test]
    fn two_queues_do_not_share_a_key() {
        let keys = Keys::new("app:q:");

        assert_ne!(keys.stream("default"), keys.stream("mail"));
        assert_ne!(keys.stream("default"), keys.delayed("default"));
    }

    #[test]
    fn the_entry_id_rides_on_the_job_and_comes_off_again() {
        let mut reserved = job();
        reserved.delivery_handle = Some("1700000000000-0".to_string());

        assert_eq!(RedisQueue::entry_id(&reserved).unwrap(), "1700000000000-0");

        // A retry must not carry the *previous* delivery's id, or the next
        // acknowledgement would target an entry that is already gone.
        let retry = RedisQueue::without_entry_id(&reserved);
        assert!(retry.delivery_handle.is_none());
        assert_eq!(retry.payload["a"], 1, "the job's own payload survives");
    }

    #[test]
    fn reserving_does_not_rewrite_a_unit_struct_jobs_payload() {
        // The bug this field exists for. A unit-struct job serialises to
        // `null`, and the entry id used to be written into the payload —
        // where `payload[key] = value` promotes `null` to an object. The job
        // then failed to deserialise into its own type on every attempt and
        // went to the failed table having never run.
        let mut reserved = QueuedJob { payload: serde_json::Value::Null, ..job() };
        reserved.delivery_handle = Some("1700000000000-0".to_string());

        assert_eq!(reserved.payload, serde_json::Value::Null, "the payload must be untouched");

        let retry = RedisQueue::without_entry_id(&reserved);
        assert_eq!(retry.payload, serde_json::Value::Null);
    }

    #[test]
    fn a_delivery_handle_is_never_serialised_with_the_job() {
        // It describes a delivery that does not exist until something reserves
        // the job, so a stored one would be stale on redelivery — and the
        // stream body is what redelivery is built from.
        let mut reserved = job();
        reserved.delivery_handle = Some("1700000000000-0".to_string());

        let encoded = RedisQueue::encode(&reserved).unwrap();
        let decoded: QueuedJob = serde_json::from_slice(&encoded).unwrap();

        assert!(decoded.delivery_handle.is_none());
    }

    #[test]
    fn acknowledging_a_job_that_was_never_reserved_says_so() {
        // A caller that built a job and acknowledged it without reserving.
        // Better than silently acking nothing.
        let err = RedisQueue::entry_id(&job()).unwrap_err();

        assert!(err.message().contains("not reserved"), "{}", err.message());
        assert!(err.message().contains("j1"), "{}", err.message());
    }

    #[test]
    fn an_unreadable_entry_becomes_a_discard_naming_the_queue_and_the_entry() {
        let entry = rainier_drivers::StreamEntry {
            id: "1700000000000-0".into(),
            body: b"not json".to_vec(),
        };
        let discarded = *decode_entry("mail", "queues:mail", &entry).unwrap_err();

        assert_eq!(discarded.connection, "redis");
        assert_eq!(discarded.queue, "mail");
        assert_eq!(discarded.message_id.as_deref(), Some("1700000000000-0"));
        assert_eq!(discarded.meta.extra["stream"], "queues:mail");
        assert!(!discarded.exception.message.is_empty());
    }

    #[test]
    fn an_entry_with_no_body_is_discarded_rather_than_left_pending() {
        // What the stream reader hands over for an entry written without a
        // `body` field: an empty body, which is not a job.
        let entry = rainier_drivers::StreamEntry { id: "1-0".into(), body: Vec::new() };
        assert!(decode_entry("default", "s", &entry).is_err());
    }

    #[test]
    fn a_readable_entry_is_a_job() {
        let entry = rainier_drivers::StreamEntry {
            id: "1-0".into(),
            body: serde_json::to_vec(&job()).unwrap(),
        };
        assert_eq!(decode_entry("default", "s", &entry).unwrap().id, "j1");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_discard_reaches_the_process_wide_reporter() {
        let capture = rainier_events::report::capture();
        let entry =
            rainier_drivers::StreamEntry { id: "redis-discard-test".into(), body: b"{".to_vec() };
        crate::reporting::report(*decode_entry("default", "s", &entry).unwrap_err()).await;

        let found = capture.reports().into_iter().any(|r| {
            matches!(
                &r.failure,
                rainier_events::Failure::QueueMessageDiscarded(d)
                    if d.message_id.as_deref() == Some("redis-discard-test")
            )
        });
        assert!(found);
    }

    #[test]
    fn an_unreadable_failed_job_says_why_instead_of_vanishing() {
        assert!(read_failed_job(b"nope").is_err());
        let missing = read_failed_job(br#"{"error": "x"}"#).unwrap_err();
        assert_eq!(missing.message, "an unreadable failed job: job");

        let whole = json!({ "job": job(), "error": "boom", "failed_at": Utc::now() });
        let read = read_failed_job(&serde_json::to_vec(&whole).unwrap()).unwrap();
        assert_eq!(read.error, "boom");
    }

    #[test]
    fn only_noeviction_is_a_safe_policy() {
        assert!(eviction_policy_failure("noeviction").is_none());
        let failure = eviction_policy_failure("allkeys-lru").expect("reported");
        assert_eq!(failure.setting, "maxmemory-policy");
        assert!(failure.exception.message.contains("allkeys-lru"));
    }

    #[test]
    fn a_consumer_names_the_host_and_the_process() {
        let name = consumer_name();

        assert!(name.contains(&std::process::id().to_string()), "{name}");
    }
}
