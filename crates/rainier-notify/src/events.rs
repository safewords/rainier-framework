//! What the notifier publishes about its own work on the event bus.
//!
//! [`NotificationSent`] is the bus's view of a `notification.send` span:
//! published for every notification the [`Notifier`](crate::Notifier) sends,
//! whether or not the crate's `telemetry` feature is on, with the same fields
//! the span carries.

use std::time::Duration;

use rainier_events::{Activity, Fields};

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
