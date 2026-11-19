//! What the console publishes on the event bus: [`CommandRan`], the root
//! activity of a command's unit of work.
//!
//! [`Console::run`](crate::Console::run) runs each command inside a
//! [`Unit`] of kind [`UnitKind::Command`], named after the command, on the
//! application's [`Dispatcher`] when one is bound (else the process-wide
//! bus). `CommandRan` is published inside it when the command returns — or
//! panics, just before the panic is resumed.

use std::sync::Arc;
use std::time::Duration;

use rainier_events::{Activity, Dispatcher, Fields, Unit, UnitKind};
use rainier_support::ErrorKind;

/// A console command ran: the `console.command` activity.
///
/// Carries the command's name and exit code, never its arguments — which is
/// where a console takes a password, a token or a customer's email.
#[derive(Debug, Clone)]
pub struct CommandRan {
    /// The command's name, `queue:work`.
    pub name: String,
    /// Its exit code; `None` when it panicked.
    pub exit_code: Option<i32>,
    /// The `error.type` when it returned `Err` (its error kind) or panicked
    /// (`panic`). A non-zero `Ok` is the command's own answer, not an error.
    pub error: Option<&'static str>,
    /// How long it ran.
    pub duration: Duration,
}

impl Activity for CommandRan {
    fn kind(&self) -> &'static str {
        "console.command"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("command {}", self.name))
            .set("rainier.command.name", self.name.clone())
            .maybe("process.exit.code", self.exit_code);
        if let Some(error) = self.error {
            fields.set("otel.status_code", "ERROR").set("error.type", error);
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// Open the command's unit — on `events` when the application has a
/// dispatcher, else wherever [`Unit::open_or_join`] finds one — or `None`
/// when a unit is already current (a command run from inside another).
pub(crate) fn open_unit(
    events: Option<&Arc<Dispatcher>>,
    kind: UnitKind,
    name: &str,
) -> Option<Arc<Unit>> {
    match events {
        None => Unit::open_or_join(kind, Some(name.to_string()), None),
        Some(_) if rainier_events::Context::current_unit().is_some() => None,
        Some(events) => Some(Unit::on(events, kind, Some(name.to_string()), None)),
    }
}

/// The `error.type` an [`ErrorKind`] records as — the strings the span's
/// `record_error` uses.
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

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_events::Value;

    #[test]
    fn its_fields_are_the_command_spans() {
        let ran = CommandRan {
            name: "check".into(),
            exit_code: Some(3),
            error: None,
            duration: Duration::from_millis(5),
        };
        assert_eq!(ran.kind(), "console.command");
        let fields = ran.collect_fields();
        assert_eq!(fields.text("otel.name"), Some("command check"));
        assert_eq!(fields.text("rainier.command.name"), Some("check"));
        assert_eq!(fields.get("process.exit.code"), Some(&Value::I64(3)));
        assert_eq!(fields.get("otel.status_code"), None);
        assert!(!ran.failed(), "a non-zero Ok is the command's answer, not a failure");
    }

    #[test]
    fn an_error_is_marked_the_way_the_span_is() {
        let ran = CommandRan {
            name: "queue:work".into(),
            exit_code: Some(1),
            error: Some(error_type(ErrorKind::Internal)),
            duration: Duration::ZERO,
        };
        let fields = ran.collect_fields();
        assert!(ran.failed());
        assert_eq!(fields.text("otel.status_code"), Some("ERROR"));
        assert_eq!(fields.text("error.type"), Some("internal"));
        assert_eq!(fields.get("process.exit.code"), Some(&Value::I64(1)));
    }
}
