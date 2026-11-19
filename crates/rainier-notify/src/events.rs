//! What the notifier publishes about its own work on the event bus.
//!
//! [`NotificationSent`] is the bus's view of a `notification.send` span:
//! published for every notification the [`Notifier`](crate::Notifier) sends,
//! whether or not the crate's `telemetry` feature is on, with the same fields
//! the span carries.

use std::time::Duration;

use rainier_events::{Activity, Fields};
use rainier_support::{Error, ErrorKind};

/// A notification was fanned out across its channels.
///
/// Carries the notification's name and how many channels it went to — never
/// the recipient, and never what it said.
///
/// The send ran as an [`Operation`](rainier_events::Operation): the mail its
/// mail channel sent, and anything else a channel published, names
/// [`operation`](Activity::operation) as its parent.
#[derive(Debug, Clone)]
pub struct NotificationSent {
    /// [`Notification::notification_name`](crate::Notification::notification_name).
    pub notification: &'static str,
    /// How many channels the receipt has an outcome for; `None` when the
    /// send failed before it had a receipt.
    pub channels: Option<usize>,
    /// From the notifier taking the notification to the last channel
    /// answering.
    pub duration: Duration,
    /// The failure's kind, in `snake_case`, when the send failed.
    pub error_type: Option<&'static str>,
    /// The [`Operation`](rainier_events::Operation) id the send ran as.
    pub operation: Option<u64>,
}

impl Activity for NotificationSent {
    fn kind(&self) -> &'static str {
        "notification.send"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("notify {}", self.notification))
            .set("rainier.notification.name", self.notification)
            .maybe("rainier.notification.channels", self.channels);
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

    fn operation(&self) -> Option<u64> {
        self.operation
    }
}

/// An error's kind as a span's `error.type` records it.
pub(crate) fn error_type(error: &Error) -> &'static str {
    match error.kind() {
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
