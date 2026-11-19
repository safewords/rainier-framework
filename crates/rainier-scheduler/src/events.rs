//! What the scheduler publishes on the event bus: [`ScheduledTaskRan`], the
//! root activity of a due task's unit of work.
//!
//! [`Schedule::run_due`](crate::Schedule::run_due) runs each due task inside
//! a [`Unit`] of kind [`UnitKind::ScheduledTask`], named after the task, on
//! the application's [`Dispatcher`] when one is bound (else the process-wide
//! bus) — a unit of its own even inside `schedule:work`, which is a command.
//! A task run inside a request joins the request instead.
//! `ScheduledTaskRan` is published inside it when the task ends, including
//! when it was skipped.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rainier_events::{Activity, Context, Dispatcher, Fields, Unit, UnitKind};
use rainier_support::ErrorKind;

use crate::task::Outcome;

/// A due task ran, failed, or was skipped: the `schedule.task` activity.
#[derive(Debug, Clone)]
pub struct ScheduledTaskRan {
    /// The task's name.
    pub name: String,
    /// How it ended: `ran`, `failed` or `skipped`.
    pub outcome: &'static str,
    /// Why it was skipped: `still_running`, `another_server`,
    /// `condition_false`.
    pub skipped: Option<&'static str>,
    /// Its error's kind, when it failed.
    pub error: Option<ErrorKind>,
    /// How long it took — for a skipped task, the lock check.
    pub duration: Duration,
}

impl Activity for ScheduledTaskRan {
    fn kind(&self) -> &'static str {
        "schedule.task"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("schedule {}", self.name))
            .set("rainier.task.name", self.name.clone())
            .set("rainier.task.outcome", self.outcome)
            .maybe("rainier.task.skipped", self.skipped);
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

/// Open a due task's unit — on `events` when the application has a
/// dispatcher, else wherever [`Unit::open_or_join`] finds one — or `None`
/// when it joins a request already current.
pub(crate) fn open_unit(events: Option<&Arc<Dispatcher>>, name: &str) -> Option<Arc<Unit>> {
    match events {
        None => Unit::open_or_join(UnitKind::ScheduledTask, Some(name.to_string()), None),
        Some(_) if Context::current_unit().is_some_and(|cur| cur.kind() == UnitKind::Request) => {
            None
        }
        Some(events) => {
            Some(Unit::on(events, UnitKind::ScheduledTask, Some(name.to_string()), None))
        }
    }
}

/// `run`, publishing its [`ScheduledTaskRan`] when it ends — inside whatever
/// unit it is polled in.
pub(crate) async fn published(
    name: &str,
    started: Instant,
    run: impl Future<Output = Outcome>,
) -> Outcome {
    let outcome = run.await;
    rainier_events::activity::publish_activity_with(|| ScheduledTaskRan {
        name: name.to_string(),
        outcome: match &outcome {
            Outcome::Ran => "ran",
            Outcome::Failed(_) => "failed",
            Outcome::Skipped(_) => "skipped",
        },
        skipped: match &outcome {
            Outcome::Skipped(why) => Some(why.label()),
            _ => None,
        },
        error: match &outcome {
            Outcome::Failed(e) => Some(e.kind()),
            _ => None,
        },
        duration: started.elapsed(),
    });
    outcome
}

/// The `error.type` an [`ErrorKind`] records as — the strings the span's
/// `record_error` uses.
fn error_type(kind: ErrorKind) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_fields_are_the_task_spans() {
        let ran = ScheduledTaskRan {
            name: "prune".into(),
            outcome: "skipped",
            skipped: Some("still_running"),
            error: None,
            duration: Duration::ZERO,
        };
        assert_eq!(ran.kind(), "schedule.task");
        let fields = ran.collect_fields();
        assert_eq!(fields.text("otel.name"), Some("schedule prune"));
        assert_eq!(fields.text("rainier.task.name"), Some("prune"));
        assert_eq!(fields.text("rainier.task.outcome"), Some("skipped"));
        assert_eq!(fields.text("rainier.task.skipped"), Some("still_running"));
        assert_eq!(fields.get("otel.status_code"), None);
        assert!(!ran.failed());

        let failed = ScheduledTaskRan {
            outcome: "failed",
            skipped: None,
            error: Some(ErrorKind::ServiceUnavailable),
            ..ran
        };
        let fields = failed.collect_fields();
        assert!(failed.failed());
        assert_eq!(fields.text("otel.status_code"), Some("ERROR"));
        assert_eq!(fields.text("error.type"), Some("service_unavailable"));
    }
}
