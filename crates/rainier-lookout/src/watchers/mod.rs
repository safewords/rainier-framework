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
//! - **models and job outcomes** — when the model watcher is on, a wildcard
//!   listener that reads event *names*. Model events are generic
//!   (`Created<M>`, `Updated<M>`, `Deleted<M>`), so no single typed listener
//!   hears all of them; their type names do say what happened and to which
//!   model, which is all an entry needs. See [`models`]. The same listener
//!   marks a batch whose job fired `JobFailed` (the `queue.process` span's
//!   `rainier.job.outcome` and a final `JobAttemptFailed` report already do
//!   so without it).
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
//! events, but an exporter will see them. Turn the model watcher off to avoid
//! it.

use std::future::{ready, Ready};
use std::sync::Arc;

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
        if let Some(event) = models::parse(name) {
            let Some(cell) = inner.current_batch() else { return };
            let watcher = inner.config.watcher(EntryType::Model);
            if !watcher.enabled || !crate::recorder::sample(watcher.sample_rate) {
                return;
            }
            let entry = models::entry(&event);
            if watcher.ignore.iter().any(|p| crate::config::glob(p, &entry.summary)) {
                return;
            }
            inner.push(&cell, entry);
            return;
        }
        match models::job_event(name) {
            Some(models::JobEvent::Failed) => inner.mark_current(true, None),
            Some(models::JobEvent::Released) => inner.mark_current(false, Some("released")),
            None => {}
        }
    }
}
