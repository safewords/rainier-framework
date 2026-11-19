//! What the guards publish about their own work on the event bus.
//!
//! [`AuthAttempted`] is the bus's view of an `auth.attempt` span: published
//! for every login attempt a [`SessionGuard`](crate::SessionGuard) makes,
//! whether or not the crate's `telemetry` feature is on, with the same fields
//! the span carries.

use std::time::Duration;

use rainier_events::{Activity, Fields};

/// A guard tried a set of credentials.
///
/// Carries the guard and whether it worked — never the credentials, and never
/// who: a failed attempt's username is somebody else's, as often as not.
#[derive(Debug, Clone)]
pub struct AuthAttempted {
    /// The guard's name: `web`, `api`, …
    pub guard: String,
    /// `success` or `failure`; `None` when the attempt errored rather than
    /// answering (see [`error_type`](Self::error_type)).
    pub outcome: Option<&'static str>,
    /// From the guard taking the credentials to its answer, the login
    /// included.
    pub duration: Duration,
    /// The error's kind, in `snake_case`, when the attempt could not answer —
    /// the user provider failed, the session could not be written.
    pub error_type: Option<&'static str>,
}

impl Activity for AuthAttempted {
    fn kind(&self) -> &'static str {
        "auth.attempt"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", "auth attempt")
            .set("rainier.auth.guard", self.guard.clone())
            .maybe("rainier.auth.outcome", self.outcome);
        if let Some(error_type) = self.error_type {
            fields.set("otel.status_code", "ERROR").set("error.type", error_type);
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error_type.is_some()
    }
}
