//! The bus watchers: what Lookout hears on the event bus rather than reads off
//! spans.
//!
//! [`Lookout::listen`] registers:
//!
//! - **failures** — a listener for
//!   [`ExceptionReported`], which
//!   the framework dispatches for every failure it reports. Each becomes an
//!   [`Exception`](crate::EntryType::Exception) entry; see [`exceptions`].
//!   Failures ignore sample rates and scope: one with no batch around it gets
//!   a [`BatchKind::Other`](crate::BatchKind::Other) batch of its own.
//! - **model changes** — when the model watcher is on, a listener for
//!   [`ModelChanged`], which boot dispatches for every committed write through
//!   any of the application's connections. Each becomes a
//!   [`Model`](crate::EntryType::Model) entry; see [`models`]. The typed
//!   `Created<M>` / `Updated<M>` / `Deleted<M>` are not recorded: they fire
//!   only from a repository built `with_events`, and the same write is
//!   already a `ModelChanged`.
//! - **job outcomes** — with the model watcher on, a wildcard listener that
//!   reads event *names*: it marks a batch whose job fired `JobFailed` (the
//!   `queue.process` span's `rainier.job.outcome` and a final
//!   `JobAttemptFailed` report already do so without it) or `JobReleased`.
//!
//! **Correlation.** Listeners run inline on the dispatching task, so the
//! batch an event belongs to is the one whose span the dispatching code is
//! running in — the layer keeps a per-thread stack of those (see
//! [`recorder`](crate::recorder)). The one report that arrives after its span
//! closed — a failed request, reported once the kernel has its response — is
//! matched to the request batch the same thread just closed.
//!
//! **A cost of the wildcard listener:** with it registered, every event has a
//! listener, so with the `telemetry` feature every dispatch opens an
//! `event.dispatch` span, where unheard events otherwise open none. The layer
//! does not record `event.dispatch` for the framework's own (`rainier_*`)
//! events, but an exporter will see them. It also makes every repository
//! built `with_events` build and dispatch its typed hooks, since a wildcard
//! counts as a listener of them. Turn the model watcher off to avoid both.

use std::future::{ready, Ready};
use std::sync::Arc;

use rainier_database::ModelChanged;
use rainier_events::report::{ExceptionReported, Failure};
use rainier_events::Dispatcher;
use rainier_support::Result;

use crate::entry::EntryType;
use crate::recorder::Lookout;

pub mod exceptions;
pub mod models;

/// Register every bus watcher on `events`. A disabled Lookout registers
/// nothing.
pub(crate) fn listen(lookout: &Lookout, events: &Dispatcher) {
    if !lookout.is_enabled() {
        return;
    }

    let failures = lookout.clone();
    events.listen(move |reported: Arc<ExceptionReported>| -> Ready<Result<()>> {
        failures.record_failure(&reported.failure);
        ready(Ok(()))
    });

    if lookout.config().watcher(EntryType::Model).enabled {
        let changes = lookout.clone();
        events.listen(move |change: Arc<ModelChanged>| -> Ready<Result<()>> {
            changes.record_model_change(&change);
            ready(Ok(()))
        });

        let names = lookout.clone();
        events.listen_any(
            move |name: &'static str,
                  _event: Arc<dyn std::any::Any + Send + Sync>|
                  -> Ready<Result<()>> {
                names.record_named_event(name);
                ready(Ok(()))
            },
        );
    }
}

impl Lookout {
    /// Record a reported failure — what the `ExceptionReported` listener does.
    ///
    /// Public so a failure that never reaches the bus can still be recorded.
    pub fn record_failure(&self, failure: &Failure) {
        let inner = &self.inner;
        if !inner.active() || !inner.config.watcher(EntryType::Exception).enabled {
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
        let after_request = matches!(failure, Failure::HttpRequest(_));
        inner.record_failure(entry, failed_job, after_request);
    }

    /// Record an event by its type name — what the wildcard listener does.
    pub(crate) fn record_named_event(&self, name: &str) {
        let inner = &self.inner;
        if !inner.active() {
            return;
        }
        match models::job_event(name) {
            Some(models::JobEvent::Failed) => inner.mark_current(true, None),
            Some(models::JobEvent::Released) => inner.mark_current(false, Some("released")),
            None => {}
        }
    }
}
