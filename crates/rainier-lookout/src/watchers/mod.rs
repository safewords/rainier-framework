//! What Lookout hears on the event bus besides activities.
//!
//! [`Lookout::subscribe`] registers one subscriber, `lookout`, on its own
//! thread, with a handler for each of:
//!
//! - **activities** — every [`Activity`](rainier_events::Activity): queries,
//!   cache calls, the request itself. See [`subscriber`](crate::subscriber).
//! - **the end of a unit of work** — [`UnitCompleted`], which closes the
//!   unit's batch.
//! - **failures** — [`ExceptionReported`], which the framework publishes for
//!   every failure it reports. Each becomes an
//!   [`Exception`](crate::EntryType::Exception) entry; see [`exceptions`].
//!   Failures ignore sample rates and scope: one with no unit around it gets
//!   a [`BatchKind::Other`](crate::BatchKind::Other) batch of its own.
//! - **model changes** — when the model watcher is on, [`ModelChanged`],
//!   which boot publishes for every committed write through any of the
//!   application's connections. Each becomes a
//!   [`Model`](crate::EntryType::Model) entry; see [`models`]. The typed
//!   `Created<M>` / `Updated<M>` / `Deleted<M>` are not recorded: they fire
//!   only from a repository built `with_events`, and the same write is
//!   already a `ModelChanged`.
//!
//! **Correlation.** Every event carries the unit of work it was published in,
//! so the batch it belongs to is that unit's — whichever thread published it,
//! and whenever the subscriber gets to it. A request's failure reported after
//! its unit is over is matched to the request batch with its method and path.
//!
//! **Job outcomes** come from the `queue.process` activity's
//! `rainier.job.outcome`: `failed`, `timed_out` and `error` mark the batch a
//! failed job, and `released` tags it `released`. No wildcard listener is
//! registered, so events nobody else listens to are still never built.

use std::sync::Arc;

use rainier_database::ModelChanged;
use rainier_events::report::{ExceptionReported, Failure};
use rainier_events::{Dispatcher, Unit, UnitCompleted};

use crate::entry::EntryType;
use crate::recorder::Lookout;

pub mod exceptions;
pub mod models;

/// Register Lookout's one subscriber on `events`.
pub(crate) fn subscribe(lookout: &Lookout, events: &Arc<Dispatcher>) {
    let activities = lookout.clone();
    let completed = lookout.clone();
    let failures = lookout.clone();
    let mut subscriber = events
        .subscriber("lookout")
        .threads(lookout.config().ingest.threads)
        .on_activity(move |activity, cx| {
            activities.inner.on_activity(activity, cx);
            Ok(())
        })
        .on(move |done: &UnitCompleted, _cx| {
            completed.inner.on_unit_completed(done);
            Ok(())
        })
        .on(move |reported: &ExceptionReported, cx| {
            failures.record_failure_in(cx.unit(), &reported.failure);
            Ok(())
        });
    if lookout.config().watcher(EntryType::Model).enabled {
        let changes = lookout.clone();
        subscriber = subscriber.on(move |change: &ModelChanged, cx| {
            changes.record_model_change_in(cx.unit(), change);
            Ok(())
        });
    }
    subscriber.start();
}

impl Lookout {
    /// Record a reported failure in the unit of work the calling code runs
    /// in — what the `ExceptionReported` handler does for a failure heard on
    /// the bus.
    ///
    /// Public so a failure that never reaches the bus can still be recorded.
    pub fn record_failure(&self, failure: &Failure) {
        let unit = rainier_events::Context::current_unit();
        self.record_failure_in(unit.as_ref(), failure);
    }

    /// Record a failure reported in `unit` (`None`: outside every unit).
    pub(crate) fn record_failure_in(&self, unit: Option<&Arc<Unit>>, failure: &Failure) {
        let inner = &self.inner;
        if !inner.config.enabled || !inner.config.watcher(EntryType::Exception).enabled {
            return;
        }
        // Paused: only a failure inside a batch already open is recorded.
        if !inner.active() && unit.is_none() {
            return;
        }
        // A 404 or a 422 is the client's mistake, not a failure of the
        // application — unless it was asked for. A socket whose handler
        // refused the client (a revoked token, a channel it may not join) is
        // the same refusal over a WebSocket, and became a case per refusal
        // before it was treated as one.
        if matches!(failure, Failure::HttpRequest(_) | Failure::WebSocket(_))
            && failure.exception().is_client_error()
            && !inner.config.capture.client_errors
        {
            return;
        }
        let entry = exceptions::entry(&inner.config, failure);
        let failed_job = matches!(failure, Failure::JobAttempt(f) if f.final_attempt);
        let request = match failure {
            Failure::HttpRequest(f) => Some((f.method.as_str(), f.path.as_str())),
            _ => None,
        };
        inner.record_failure(entry, failed_job, unit, request);
    }
}
