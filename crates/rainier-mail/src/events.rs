//! What the mailer publishes about its own work on the event bus.
//!
//! [`MailSent`] is the bus's view of a `mail.send` span: published for every
//! delivery the [`Mailer`](crate::Mailer) attempts, whether or not the
//! crate's `telemetry` feature is on, with the same fields the span carries.
//! It is an [`Activity`], so a subscriber that hears every activity —
//! Lookout, a metrics exporter — sees it without knowing the type.

use std::time::Duration;

use rainier_events::{Activity, Fields};

/// A message was handed to a transport — or the attempt failed.
///
/// Carries how many recipients and attachments, and through which transport;
/// never who, and never what. An address is personal data, and a subject line
/// very often is too.
///
/// The send ran as an [`Operation`](rainier_events::Operation): a view
/// rendered for it, or anything else it published, names
/// [`operation`](Activity::operation) as its parent.
#[derive(Debug, Clone)]
pub struct MailSent {
    /// The transport's name: `smtp`, `ses`, `log`, `memory`, …
    pub transport: String,
    /// To, cc and bcc together, as the message arrived at the mailer.
    pub recipients: usize,
    /// How many attachments.
    pub attachments: usize,
    /// From the mailer taking the message to the transport answering.
    pub duration: Duration,
    /// The failure's kind, in `snake_case`, when the send failed — a
    /// `MessageSending` listener's veto, an invalid message, the transport.
    pub error_type: Option<&'static str>,
    /// The [`Operation`](rainier_events::Operation) id the send ran as.
    pub operation: Option<u64>,
}

impl Activity for MailSent {
    fn kind(&self) -> &'static str {
        "mail.send"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", format!("mail {}", self.transport))
            .set("mail.transport", self.transport.clone())
            .set("mail.recipients", self.recipients)
            .set("mail.attachments", self.attachments);
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
