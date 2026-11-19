//! What the queue publishes on the event bus about its own work:
//! [`JobQueued`] for every dispatch and [`JobRan`] for every attempt.
//!
//! Both are [`Activity`]s under the names of the spans the queue emits with
//! the `telemetry` feature — `queue.publish` and `queue.process` — with the
//! same fields, and both are published whether or not that feature is on.
//!
//! # A job's attempt is a unit of work
//!
//! A worker runs each attempt inside a [`Unit`] of kind [`UnitKind::Job`],
//! named after the job, whose trace is the one the job carries
//! ([`QueuedJob::traceparent`]) — so a request, the jobs it queued and the
//! jobs those queued share one trace. [`JobRan`] is the unit's root
//! activity, published inside it when the attempt ends.
//!
//! A job run **inside a request** — the `sync` driver, `dispatch_now`, a
//! worker driven from a test — joins the request's unit instead, inside an
//! [`Operation`](rainier_events::Operation) that its `JobRan` names
//! ([`Activity::operation`]), so what the job did nests under it.
//!
//! The worker's lifecycle events — [`JobProcessing`](crate::JobProcessing),
//! [`JobProcessed`](crate::JobProcessed), [`JobReleased`](crate::JobReleased),
//! [`JobFailed`](crate::JobFailed) — are informational and **published**, not
//! dispatched: their listeners run after the worker has moved on, and a
//! failing one is logged and reported, never seen by the job.

use std::sync::Arc;
use std::time::Duration;

use rainier_events::{Activity, Context, Dispatcher, Fields, Unit, UnitKind};
use rainier_support::{Error, ErrorKind};

/// A job was pushed to a queue: the `queue.publish` activity.
#[derive(Debug, Clone)]
pub struct JobQueued {
    /// The job's [`Job::NAME`](crate::Job::NAME).
    pub name: String,
    /// The queue it went to.
    pub queue: String,
    /// The queued instance's id.
    pub id: String,
    /// The driver it went to, as [`Queue::name`](crate::Queue::name) says.
    pub driver: String,
    /// How long the push took.
    pub duration: Duration,
    /// Why the push failed, when it did.
    pub error: Option<ErrorKind>,
}

impl Activity for JobQueued {
    fn kind(&self) -> &'static str {
        "queue.publish"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("publish {}", self.queue))
            .set("otel.kind", "producer")
            .set("messaging.system", crate::telemetry::messaging_system(&self.driver).to_string())
            .set("messaging.operation.type", "send")
            .set("messaging.destination.name", self.queue.clone())
            .set("messaging.message.id", self.id.clone())
            .set("rainier.job.name", self.name.clone());
        if let Some(kind) = self.error {
            fields.set("otel.status_code", "ERROR").set("error.type", error_type(kind));
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// One attempt at a job ended: the `queue.process` activity, root of the
/// attempt's unit of work.
#[derive(Debug, Clone)]
pub struct JobRan {
    /// The job's [`Job::NAME`](crate::Job::NAME).
    pub name: String,
    /// The queue it was reserved from.
    pub queue: String,
    /// The queued instance's id.
    pub id: String,
    /// The driver, as [`Queue::name`](crate::Queue::name) says.
    pub driver: String,
    /// Which attempt this was, counting from one.
    pub attempt: u32,
    /// How many attempts the job gets.
    pub max_attempts: u32,
    /// How it ended: `processed`, `released`, `failed`, `timed_out`, or
    /// `error` when the worker could not settle it with the broker.
    pub outcome: &'static str,
    /// Where it ran: `in_process`, or `kubernetes`.
    pub runner: &'static str,
    /// How long the driver took to hand it over; zero when nothing reserved
    /// it (a job run inline).
    pub reserve: Duration,
    /// How long it waited between becoming runnable and being reserved,
    /// when that can be known.
    pub wait: Option<Duration>,
    /// The backoff before the next attempt, for a released job.
    pub backoff: Option<Duration>,
    /// For an `error` outcome, the broker's error kind.
    pub error: Option<ErrorKind>,
    /// The trace the attempt ran in, when known.
    pub trace_id: Option<String>,
    /// The attempt's whole run, settling included.
    pub duration: Duration,
    /// The [`Operation`](rainier_events::Operation) the job ran as, when it
    /// ran inside another unit of work (a request).
    pub operation: Option<u64>,
}

impl JobRan {
    /// Whether the attempt failed: released, failed, timed out, or not
    /// settled.
    pub fn is_failure(&self) -> bool {
        self.outcome != "processed"
    }
}

impl Activity for JobRan {
    fn kind(&self) -> &'static str {
        "queue.process"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("process {}", self.name))
            .set("otel.kind", "consumer")
            .set("messaging.system", crate::telemetry::messaging_system(&self.driver).to_string())
            .set("messaging.operation.type", "process")
            .set("messaging.destination.name", self.queue.clone())
            .set("messaging.message.id", self.id.clone())
            .set("rainier.job.name", self.name.clone())
            .set("rainier.job.attempt", self.attempt)
            .set("rainier.job.max_attempts", self.max_attempts)
            .set("rainier.queue.reserve_ms", millis(self.reserve))
            .maybe("messaging.queue.wait_ms", self.wait.map(millis))
            .set("rainier.job.runner", self.runner)
            .set("rainier.job.outcome", self.outcome)
            .maybe("rainier.job.backoff_ms", self.backoff.map(millis))
            .maybe("trace_id", self.trace_id.clone());
        match (self.outcome, self.error) {
            ("processed", _) => {}
            ("error", kind) => {
                fields
                    .set("otel.status_code", "ERROR")
                    .set("error.type", kind.map_or("internal", error_type));
            }
            (outcome, _) => {
                fields.set("otel.status_code", "ERROR").set(
                    "error.type",
                    if outcome == "timed_out" { "timeout" } else { "job_failed" },
                );
            }
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.is_failure()
    }

    fn operation(&self) -> Option<u64> {
        self.operation
    }
}

/// Whole milliseconds, as the span records them.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The `error.type` an [`ErrorKind`] records as — the same strings the
/// span's `record_error` uses.
pub(crate) fn error_type(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::BadRequest => "bad_request",
        ErrorKind::Unauthenticated => "unauthenticated",
        ErrorKind::Unauthorized => "unauthorized",
        ErrorKind::NotFound => "not_found",
        ErrorKind::MethodNotAllowed => "method_not_allowed",
        ErrorKind::Conflict => "conflict",
        ErrorKind::PayloadTooLarge => "payload_too_large",
        ErrorKind::Validation => "validation",
        ErrorKind::TooManyRequests => "too_many_requests",
        ErrorKind::Internal => "internal",
        ErrorKind::ServiceUnavailable => "service_unavailable",
        ErrorKind::Status(_) => "status",
    }
}

/// The error kind of a result, for an activity's `error.type`.
pub(crate) fn error_of<T>(result: &rainier_support::Result<T>) -> Option<ErrorKind> {
    result.as_ref().err().map(Error::kind)
}

/// Open the unit a job's attempt runs in — on `events` when the worker has
/// the application's dispatcher, else wherever [`Unit::open_or_join`] finds
/// one — or `None` when it joins a request already current.
pub(crate) fn open_job_unit(
    events: Option<&Arc<Dispatcher>>,
    name: &str,
    traceparent: Option<&str>,
) -> Option<Arc<Unit>> {
    let unit = match events {
        None => Unit::open_or_join(UnitKind::Job, Some(name.to_string()), None)?,
        Some(events) => {
            // `open_or_join`'s rule: a job inside a request is part of it;
            // inside anything else (`queue:work`) it is a unit of its own.
            if Context::current_unit().is_some_and(|cur| cur.kind() == UnitKind::Request) {
                return None;
            }
            Unit::on(events, UnitKind::Job, Some(name.to_string()), None)
        }
    };
    unit.set_trace(None, traceparent);
    Some(unit)
}

/// Run `work` as a job attempt: inside its own unit, or — joined into the
/// request current here — inside an operation whose id `work` is handed to
/// put on its [`JobRan`].
pub(crate) async fn in_job_unit<F, Fut, T>(
    events: Option<&Arc<Dispatcher>>,
    name: &str,
    traceparent: Option<&str>,
    work: F,
) -> T
where
    F: FnOnce(Option<u64>) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    match open_job_unit(events, name, traceparent) {
        Some(unit) => unit.scope(work(None)).await,
        None if Context::current_unit().is_some() => {
            let operation = rainier_events::Operation::new();
            let id = operation.id();
            operation.scope(work(Some(id))).await
        }
        None => work(None).await,
    }
}

/// Run `job` here and now — the `sync` driver, `dispatch_now` — as an
/// attempt: in its own unit, or joined into the request current here, with
/// its [`JobRan`].
pub(crate) async fn run_inline(
    registry: &crate::JobRegistry,
    job: &crate::QueuedJob,
    context: Arc<crate::JobContext>,
    driver: &'static str,
) -> rainier_support::Result<()> {
    in_job_unit(None, &job.name, job.traceparent.as_deref(), |operation| async move {
        let started = std::time::Instant::now();
        let result = registry.run(job, context).await;
        rainier_events::activity::publish_activity_with(|| JobRan {
            name: job.name.clone(),
            queue: job.queue.clone(),
            id: job.id.clone(),
            driver: driver.to_string(),
            attempt: 1,
            max_attempts: 1,
            // One attempt, and the last: a failure is final.
            outcome: if result.is_ok() { "processed" } else { "failed" },
            runner: "in_process",
            reserve: Duration::ZERO,
            wait: None,
            backoff: None,
            error: None,
            trace_id: current_trace_id(),
            duration: started.elapsed(),
            operation,
        });
        result
    })
    .await
}

/// The `traceparent` a job queued here should carry: the current unit's —
/// the one it continues, or one naming the unit itself inside its trace.
pub(crate) fn current_traceparent() -> Option<String> {
    let unit = Context::current_unit()?;
    if let Some(traceparent) = unit.traceparent() {
        return Some(traceparent.to_string());
    }
    unit.trace_id().map(|trace| format!("00-{trace}-{:016x}-01", unit.id().max(1)))
}

/// The trace id of the unit current here.
pub(crate) fn current_trace_id() -> Option<String> {
    Context::current_unit().and_then(|unit| unit.trace_id().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(outcome: &'static str) -> JobRan {
        JobRan {
            name: "send-invoice".into(),
            queue: "billing".into(),
            id: "j-1".into(),
            driver: "sqs".into(),
            attempt: 2,
            max_attempts: 3,
            outcome,
            runner: "in_process",
            reserve: Duration::from_millis(4),
            wait: Some(Duration::from_millis(1500)),
            backoff: (outcome == "released").then_some(Duration::from_secs(10)),
            error: None,
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
            duration: Duration::from_millis(20),
            operation: None,
        }
    }

    #[test]
    fn job_ran_carries_the_process_spans_fields() {
        let event = ran("processed");
        assert_eq!(event.kind(), "queue.process");
        let fields = event.collect_fields();
        assert_eq!(fields.text("otel.name"), Some("process send-invoice"));
        assert_eq!(fields.text("otel.kind"), Some("consumer"));
        assert_eq!(fields.text("messaging.system"), Some("aws_sqs"));
        assert_eq!(fields.text("messaging.operation.type"), Some("process"));
        assert_eq!(fields.text("messaging.destination.name"), Some("billing"));
        assert_eq!(fields.text("messaging.message.id"), Some("j-1"));
        assert_eq!(fields.text("rainier.job.name"), Some("send-invoice"));
        assert_eq!(fields.get("rainier.job.attempt"), Some(&rainier_events::Value::U64(2)));
        assert_eq!(fields.get("rainier.job.max_attempts"), Some(&rainier_events::Value::U64(3)));
        assert_eq!(fields.get("rainier.queue.reserve_ms"), Some(&rainier_events::Value::U64(4)));
        assert_eq!(fields.get("messaging.queue.wait_ms"), Some(&rainier_events::Value::U64(1500)));
        assert_eq!(fields.text("rainier.job.runner"), Some("in_process"));
        assert_eq!(fields.text("rainier.job.outcome"), Some("processed"));
        assert_eq!(fields.text("trace_id"), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(fields.get("otel.status_code"), None);
        assert!(!event.failed());
    }

    #[test]
    fn a_failed_attempt_is_marked_the_way_the_span_is() {
        let released = ran("released");
        let fields = released.collect_fields();
        assert!(released.failed());
        assert_eq!(fields.text("otel.status_code"), Some("ERROR"));
        assert_eq!(fields.text("error.type"), Some("job_failed"));
        assert_eq!(fields.get("rainier.job.backoff_ms"), Some(&rainier_events::Value::U64(10_000)));

        let timed_out = ran("timed_out").collect_fields();
        assert_eq!(timed_out.text("error.type"), Some("timeout"));

        let mut unsettled = ran("error");
        unsettled.error = Some(ErrorKind::ServiceUnavailable);
        assert_eq!(unsettled.collect_fields().text("error.type"), Some("service_unavailable"));
    }

    #[test]
    fn job_queued_carries_the_publish_spans_fields() {
        let mut queued = JobQueued {
            name: "send-invoice".into(),
            queue: "billing".into(),
            id: "j-1".into(),
            driver: "database".into(),
            duration: Duration::from_millis(2),
            error: None,
        };
        assert_eq!(queued.kind(), "queue.publish");
        let fields = queued.collect_fields();
        assert_eq!(fields.text("otel.name"), Some("publish billing"));
        assert_eq!(fields.text("otel.kind"), Some("producer"));
        assert_eq!(fields.text("messaging.system"), Some("database"));
        assert_eq!(fields.text("messaging.operation.type"), Some("send"));
        assert_eq!(fields.text("messaging.destination.name"), Some("billing"));
        assert_eq!(fields.text("messaging.message.id"), Some("j-1"));
        assert_eq!(fields.text("rainier.job.name"), Some("send-invoice"));
        assert!(!queued.failed());

        queued.error = Some(ErrorKind::Internal);
        let fields = queued.collect_fields();
        assert_eq!(fields.text("otel.status_code"), Some("ERROR"));
        assert_eq!(fields.text("error.type"), Some("internal"));
    }
}
