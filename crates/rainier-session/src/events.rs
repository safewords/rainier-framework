//! What this crate publishes on the event bus: [`SessionOperation`], one per
//! session read from or written to its store by
//! [`StartSession`](crate::StartSession).

use std::sync::Arc;
use std::time::Duration;

use rainier_events::{Activity, Dispatcher, Fields};
use rainier_support::ErrorKind;

/// A session loaded from its store (kind `session.load`) or saved to it
/// (`session.save`), with the fields the span of the same name records.
///
/// Published whether or not the `telemetry` feature is on. Like the span, it
/// never carries the session id — a bearer credential for as long as the
/// session lives — nor the session's data. A client-side store has nothing to
/// load, so a cookie session publishes no `session.load`.
#[derive(Debug, Clone)]
pub struct SessionOperation {
    /// `session.load` or `session.save`.
    pub kind: &'static str,
    /// `rainier.session.store`: the store's name — `cache`, `database`,
    /// `cookie`, …
    pub store: String,
    /// `rainier.session.found`, for a load that succeeded: whether the
    /// cookie's session was in the store.
    pub found: Option<bool>,
    /// How long it took.
    pub duration: Duration,
    /// The error's kind, when it failed.
    pub error: Option<ErrorKind>,
}

impl SessionOperation {
    pub(crate) fn load<T>(
        store: &str,
        read: &rainier_support::Result<Option<T>>,
        duration: Duration,
    ) -> Self {
        Self {
            kind: "session.load",
            store: store.to_string(),
            found: read.as_ref().ok().map(Option::is_some),
            duration,
            error: read.as_ref().err().map(|error| error.kind()),
        }
    }

    pub(crate) fn save<T>(
        store: &str,
        saved: &rainier_support::Result<T>,
        duration: Duration,
    ) -> Self {
        Self {
            kind: "session.save",
            store: store.to_string(),
            found: None,
            duration,
            error: saved.as_ref().err().map(|error| error.kind()),
        }
    }
}

impl Activity for SessionOperation {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn fields(&self, fields: &mut Fields) {
        let otel_name = if self.kind == "session.load" { "session load" } else { "session save" };
        fields
            .set("otel.name", otel_name)
            .set("rainier.session.store", self.store.clone())
            .maybe("rainier.session.found", self.found);
        if let Some(kind) = self.error {
            fields.set("otel.status_code", "ERROR").set("error.type", kind.name());
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// The bus, when something on it would hear a [`SessionOperation`].
#[inline]
pub(crate) fn listening() -> Option<Arc<Dispatcher>> {
    // Asked first by borrowing the bus — no lock, no reference count — so
    // the answer "nobody" costs an atomic load; only a yes clones it.
    if !rainier_events::activity::has_activity_subscribers::<SessionOperation>() {
        return None;
    }
    rainier_events::bus()
}
