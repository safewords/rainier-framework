//! Job performance — spans, timings and a [`QueueObserver`] — behind the
//! `telemetry` feature.
//!
//! "The job is slow" has four different causes, and a single span per job
//! cannot tell them apart:
//!
//! | Where the time went | What it looks like here |
//! |---|---|
//! | the job **waited** — too few workers, a backlog | `messaging.queue.wait` / `queue_job_wait_seconds` |
//! | the **driver** round-trip — a slow `BRPOP`, an SQS long poll | the `queue.reserve` span, `rainier.queue.reserve_ms` |
//! | **decoding** the payload | the `queue.deserialize` span |
//! | the **handler** itself | the `queue.handle` span |
//! | acknowledging — a slow `XACK`, a contended `DELETE` | `queue.ack` / `queue.release` / `queue.fail` |
//!
//! So each is its own span under one `queue.process` root, and the root carries
//! the wait and the outcome as attributes.
//!
//! # Wait time is measured from `available_at`, not from the push
//!
//! A job delayed by ten minutes did not *wait* ten minutes — it was not due.
//! And a job released for a retry was not waiting during its backoff. Every
//! driver writes the moment a job becomes runnable into
//! [`QueuedJob::available_at`] (the push time for an ordinary dispatch, the
//! delay's end for a delayed one, the backoff's end for a release), so the
//! wait is `reserved − available_at`, and that is already on every envelope
//! ever written — including the ones sitting in a queue across the deploy
//! that turned this on. There is no new stamp to be missing.
//!
//! One driver cannot honour the retry case: SQS releases a message by changing
//! its visibility, and the body — `available_at` included — is not rewritten.
//! A retried SQS job's wait therefore includes its backoff and the failed
//! attempt before it.
//!
//! An `available_at` in the future (a producer whose clock runs ahead) records
//! a zero wait rather than a negative one, and one at or before the Unix
//! epoch's first day — a foreign producer that never set it — records none.
//!
//! # The trace crosses the queue
//!
//! At dispatch the `traceparent` in force is written onto the envelope
//! ([`QueuedJob::traceparent`]), and the worker's `queue.process` span adopts
//! it as its parent: the job joins the trace of the request that queued it,
//! minutes and processes later. An envelope without one starts a trace of its
//! own. The database driver stores jobs as columns and has none for it, so
//! its jobs start fresh traces — the wait and the timings are unaffected.
//!
//! # Never the payload
//!
//! No span carries a job's payload or anything derived from it. A payload is
//! application data — an email address, an order — and a span goes to a
//! system with other readers and other retention. The job's **type name** is
//! what identifies it, in spans and in metric labels alike; its id is on the
//! span (one value per span is fine) and never on a metric (one series per
//! job id is not).

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::job::QueuedJob;

/// How one job ended, as a low-cardinality label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobOutcome {
    /// It ran and succeeded.
    Processed,
    /// It failed and went back on the queue for another attempt.
    Released,
    /// It failed on its last attempt.
    Failed,
    /// It ran past its timeout — whether or not it has attempts left.
    ///
    /// Its own outcome because a timeout is a different problem from an
    /// error: the fix is a bigger limit or a smaller job, not a code change.
    TimedOut,
    /// The worker could not record what happened — the driver failed to
    /// acknowledge, release or fail it. The job will be redelivered when its
    /// reservation lapses, so this is neither a success nor a failure yet.
    Error,
}

impl JobOutcome {
    /// The label value — `processed`, `released`, `failed`, `timed_out`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobOutcome::Processed => "processed",
            JobOutcome::Released => "released",
            JobOutcome::Failed => "failed",
            JobOutcome::TimedOut => "timed_out",
            JobOutcome::Error => "error",
        }
    }
}

/// Everything measured about one job, handed to a [`QueueObserver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTiming {
    /// The queue it was reserved from.
    pub queue: String,
    /// The driver, as [`Queue::name`](crate::Queue::name) reports it.
    pub driver: String,
    /// The job's type name — [`Job::NAME`](crate::Job::NAME).
    pub job: String,
    /// The attempt this was, counting from one.
    pub attempt: u32,
    /// How it ended.
    pub outcome: JobOutcome,
    /// How long it waited between becoming runnable and being reserved, when
    /// that can be known.
    pub wait: Option<Duration>,
    /// How long the driver took to hand it over.
    pub reserve: Duration,
    /// How long it ran — the handler, or the pod — not counting the
    /// acknowledgement.
    pub handle: Duration,
    /// The backoff before the next attempt, for a released job.
    pub backoff: Option<Duration>,
    /// For a job run as a Kubernetes pod: from creating the `batch/v1 Job`
    /// to its container starting — scheduling, image pull, init containers.
    pub pod_start: Option<Duration>,
    /// For a job run as a Kubernetes pod: how long its container ran.
    pub pod_run: Option<Duration>,
}

/// Where a worker reports what it measured.
///
/// The worker does not know about Prometheus: `rainier-queue` sits below
/// `rainier-metrics` in the graph and stays there. `rainier-framework` wires
/// an implementation over the application's `Metrics` registry into
/// `queue:work`; anything else — a StatsD client, a test — implements this.
///
/// Called on the job's own task, after the job is done, so an implementation
/// must be cheap and must not block: increment a counter, observe a
/// histogram. It must never make a network call per job.
pub trait QueueObserver: Send + Sync + 'static {
    /// A job was reserved and is about to run.
    fn started(&self, queue: &str, job: &str);

    /// A job finished, however it ended.
    fn finished(&self, timing: &JobTiming);

    /// A sampled depth: how many jobs are waiting on `queue`.
    ///
    /// From [`Queue::size`](crate::Queue::size), on the worker's own sampling
    /// interval — never per job.
    fn depth(&self, queue: &str, depth: u64) {
        let _ = (queue, depth);
    }
}

/// How long `job` waited, reserved at `reserved_at` — see the module docs.
pub fn wait_time(job: &QueuedJob, reserved_at: DateTime<Utc>) -> Option<Duration> {
    // Before the epoch's first day is not a time anybody enqueued anything;
    // it is a producer that never set the field.
    if job.available_at.timestamp() <= 86_400 {
        return None;
    }

    Some((reserved_at - job.available_at).to_std().unwrap_or(Duration::ZERO))
}

/// The `messaging.system` value for a driver name.
pub(crate) fn messaging_system(driver: &str) -> &str {
    match driver {
        "sqs" => "aws_sqs",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_available_at(at: DateTime<Utc>) -> QueuedJob {
        QueuedJob::foreign("default", "ping", serde_json::Value::Null).available_at(at)
    }

    #[test]
    fn the_wait_runs_from_when_the_job_became_runnable() {
        let now = Utc::now();
        let job = job_available_at(now - chrono::Duration::seconds(42));

        assert_eq!(wait_time(&job, now), Some(Duration::from_secs(42)));
    }

    #[test]
    fn a_delayed_job_waits_from_its_delay_not_its_push() {
        let now = Utc::now();
        let mut job = job_available_at(now - chrono::Duration::seconds(3));
        job.created_at = now - chrono::Duration::minutes(10);

        assert_eq!(wait_time(&job, now), Some(Duration::from_secs(3)));
    }

    #[test]
    fn a_clock_running_ahead_is_a_zero_wait_not_a_negative_one() {
        let now = Utc::now();
        let job = job_available_at(now + chrono::Duration::seconds(5));

        assert_eq!(wait_time(&job, now), Some(Duration::ZERO));
    }

    #[test]
    fn a_producer_that_never_set_the_time_records_no_wait() {
        let job = job_available_at(DateTime::<Utc>::UNIX_EPOCH);
        assert_eq!(wait_time(&job, Utc::now()), None);
    }

    #[test]
    fn an_envelope_written_before_tracing_existed_still_reads() {
        // A job sitting in a queue across the deploy that turned this on.
        let old = serde_json::json!({
            "id": "1", "name": "ping", "payload": null, "queue": "default",
            "attempts": 0, "max_attempts": 3,
            "available_at": "2026-10-01T00:00:00Z", "created_at": "2026-10-01T00:00:00Z",
        });
        let job: QueuedJob = serde_json::from_value(old).unwrap();

        assert_eq!(job.traceparent, None);
        let reserved = "2026-10-01T00:00:07Z".parse().unwrap();
        assert_eq!(wait_time(&job, reserved), Some(Duration::from_secs(7)));
    }

    #[test]
    fn outcomes_are_stable_labels() {
        let labels: Vec<_> = [
            JobOutcome::Processed,
            JobOutcome::Released,
            JobOutcome::Failed,
            JobOutcome::TimedOut,
            JobOutcome::Error,
        ]
        .iter()
        .map(|outcome| outcome.as_str())
        .collect();
        assert_eq!(labels, ["processed", "released", "failed", "timed_out", "error"]);
    }
}
