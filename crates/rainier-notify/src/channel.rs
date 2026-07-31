//! Where a notification goes — the [`Channel`] port and the ones that ship.

use std::sync::{Arc, Mutex};

use rainier_mail::{Address, Mailer};
use rainier_support::{BoxFuture, Result};

use crate::notification::Delivery;

/// One way of reaching a recipient.
///
/// A port, so an application adds Slack, Vonage, APNs or a webhook without
/// anything here changing. A channel consumes whichever of the
/// [three renderings](crate::Notification) suits it.
pub trait Channel: Send + Sync + 'static {
    /// The channel's name.
    ///
    /// What [`Notifiable::route_for`](crate::Notifiable) is asked about and
    /// what a diagnostic prints. Not an identifier — channels are selected by
    /// type — so two channels sharing a name is untidy rather than broken.
    fn name(&self) -> &'static str;

    /// Send it.
    fn send<'a>(&'a self, delivery: &'a Delivery) -> BoxFuture<'a, Result<()>>;
}

/// Writes the notification to the log. The safe default.
///
/// Sends nothing anywhere, which is what makes it the right thing to have by
/// accident: a misconfigured deployment logs its notifications rather than
/// mailing real people from a copy of production data.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogChannel;

impl Channel for LogChannel {
    fn name(&self) -> &'static str {
        "log"
    }

    fn send<'a>(&'a self, delivery: &'a Delivery) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::info!(
                notification = delivery.notification,
                to = %format_args!("{} {}", delivery.recipient_type, delivery.recipient_id),
                body = delivery.text().unwrap_or("(no text form)"),
                "notification"
            );
            Ok(())
        })
    }
}

/// Keeps notifications in memory so a test can assert on them.
///
/// Never right outside a test — nothing is delivered and the vector grows until
/// the process ends.
#[derive(Debug, Default)]
pub struct MemoryChannel {
    sent: Mutex<Vec<Recorded>>,
}

/// One notification a [`MemoryChannel`] captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The notification's name.
    pub notification: &'static str,
    /// Who it was for.
    pub recipient_id: String,
    /// What kind of thing they are.
    pub recipient_type: &'static str,
    /// Its short-text form, if it had one.
    pub text: Option<String>,
}

impl MemoryChannel {
    /// An empty channel.
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything captured so far.
    pub fn sent(&self) -> Vec<Recorded> {
        self.lock().clone()
    }

    /// How many were captured.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing was.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `notification` was sent to `recipient_id`.
    pub fn sent_to(&self, notification: &str, recipient_id: &str) -> bool {
        self.lock().iter().any(|r| r.notification == notification && r.recipient_id == recipient_id)
    }

    /// Forget everything.
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Recorded>> {
        self.sent.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Channel for MemoryChannel {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn send<'a>(&'a self, delivery: &'a Delivery) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.lock().push(Recorded {
                notification: delivery.notification,
                recipient_id: delivery.recipient_id.clone(),
                recipient_type: delivery.recipient_type,
                text: delivery.text().map(str::to_string),
            });
            Ok(())
        })
    }
}

/// Sends the notification as an email.
///
/// Uses [`to_mail`](crate::Notification::to_mail), and addresses it to the
/// recipient's `route_for("mail")` — so the notification does not have to know
/// the address, which is the point of a notification having a recipient.
pub struct MailChannel {
    mailer: Arc<Mailer>,
}

impl MailChannel {
    /// A channel over `mailer`.
    pub fn new(mailer: Arc<Mailer>) -> Self {
        Self { mailer }
    }
}

impl Channel for MailChannel {
    fn name(&self) -> &'static str {
        "mail"
    }

    fn send<'a>(&'a self, delivery: &'a Delivery) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(mailable) = delivery.mail() else {
                return Err(delivery.nothing_to_send("mail", "email"));
            };

            // `render`, not `prepare`: the address goes on before the mailer's
            // defaults, so `always_to` records the real recipient rather than
            // an empty one. `deliver` applies them.
            let mut message = self.mailer.render(mailable)?;

            // The notification wrote the body; the recipient supplies the
            // address. A notification that hard-coded one would be a mailable.
            if message.envelope.to.is_empty() {
                let Some(route) = &delivery.route else {
                    return Err(delivery.no_route("mail"));
                };
                message.envelope.to.push(Address::new(route.clone()));
            }

            self.mailer.deliver(message).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::{Channels, Notifiable, Notification};
    use rainier_mail::{Envelope, Mailable, Message};
    use rainier_view::{MemoryEngine, ViewEngine};

    fn views() -> Arc<dyn ViewEngine> {
        Arc::new(MemoryEngine::new())
    }

    struct User {
        id: u64,
        email: Option<String>,
    }

    impl Notifiable for User {
        fn notifiable_id(&self) -> String {
            self.id.to_string()
        }
        fn notifiable_type(&self) -> &'static str {
            "User"
        }
        fn route_for(&self, channel: &str) -> Option<String> {
            match channel {
                "mail" => self.email.clone(),
                _ => Some(self.id.to_string()),
            }
        }
    }

    struct Welcome;

    impl Notification<User> for Welcome {
        fn notification_name(&self) -> &'static str {
            "user.welcome"
        }
        fn via(&self, _: &User) -> Channels {
            Channels::new().with::<MemoryChannel>()
        }
        fn to_text(&self, _: &User) -> Option<String> {
            Some("welcome".to_string())
        }
        fn to_mail(&self, _: &User) -> Option<Box<dyn Mailable>> {
            // Deliberately no `to` — the channel fills it from the recipient.
            let mut message = Message::new(Envelope::new("Welcome"));
            message.text = Some("welcome".into());
            Some(Box::new(message))
        }
    }

    #[tokio::test]
    async fn the_memory_channel_records_what_it_was_given() {
        let channel = MemoryChannel::new();
        let user = User { id: 7, email: None };

        channel.send(&Delivery::render(&Welcome, &user)).await.unwrap();

        assert_eq!(channel.len(), 1);
        assert!(channel.sent_to("user.welcome", "7"));
        assert_eq!(channel.sent()[0].text.as_deref(), Some("welcome"));
    }

    #[tokio::test]
    async fn the_log_channel_delivers_nothing_and_succeeds() {
        // The property that makes it a safe default.
        let user = User { id: 7, email: None };
        assert!(LogChannel.send(&Delivery::render(&Welcome, &user)).await.is_ok());
    }

    #[tokio::test]
    async fn the_mail_channel_addresses_from_the_recipient() {
        let mailer =
            Arc::new(Mailer::fake(views()).with_default_from(Address::new("noreply@example.com")));
        let channel = MailChannel::new(Arc::clone(&mailer));

        let user = User { id: 7, email: Some("ada@example.com".into()) };
        let delivery = Delivery::render(&Welcome, &user).routed_for("mail", &user);

        channel.send(&delivery).await.unwrap();

        let sent = mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].envelope.to[0].email, "ada@example.com");
    }

    #[tokio::test]
    async fn the_mail_channel_refuses_a_recipient_with_no_address() {
        // Distinct from "the notification produced no email" — the notifier
        // turns this one into a skip, not a failure.
        let mailer =
            Arc::new(Mailer::fake(views()).with_default_from(Address::new("noreply@example.com")));
        let channel = MailChannel::new(mailer);

        let user = User { id: 7, email: None };
        let delivery = Delivery::render(&Welcome, &user).routed_for("mail", &user);

        let err = channel.send(&delivery).await.unwrap_err();
        assert!(err.message().contains("no `mail` address"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_notification_can_send_a_view_backed_mailable() {
        // The reason `to_mail` returns a `Mailable` rather than a `Message`:
        // rendering needs the view engine, and only the mailer has one.
        struct Templated;

        impl rainier_mail::Mailable for Templated {
            fn envelope(&self) -> Envelope {
                Envelope::new("Your post is live")
            }
            fn content(&self) -> Result<rainier_mail::Content> {
                rainier_mail::Content::view("mail.welcome", serde_json::json!({ "name": "Ada" }))
            }
        }

        impl Notification<User> for Templated {
            fn notification_name(&self) -> &'static str {
                "test.templated"
            }
            fn via(&self, _: &User) -> Channels {
                Channels::new().with::<MailChannel>()
            }
            fn to_mail(&self, _: &User) -> Option<Box<dyn Mailable>> {
                Some(Box::new(Templated))
            }
        }

        let engine: Arc<dyn ViewEngine> =
            Arc::new(MemoryEngine::new().with("mail.welcome", "<p>Hi {{ name }}</p>"));
        let mailer =
            Arc::new(Mailer::fake(engine).with_default_from(Address::new("noreply@example.com")));
        let channel = MailChannel::new(Arc::clone(&mailer));

        let user = User { id: 7, email: Some("ada@example.com".into()) };
        channel.send(&Delivery::render(&Templated, &user).routed_for("mail", &user)).await.unwrap();

        let sent = mailer.sent();
        assert_eq!(sent[0].html.as_deref(), Some("<p>Hi Ada</p>"));
        assert_eq!(sent[0].envelope.to[0].email, "ada@example.com");
    }

    #[tokio::test]
    async fn a_redirected_notification_records_who_it_was_really_for() {
        // `always_to` is the staging safety net. Addressing the message from
        // the recipient has to happen *before* the redirect, or the header
        // that says who it was for records nothing.
        let mailer = Arc::new(
            Mailer::fake(views())
                .with_default_from(Address::new("noreply@example.com"))
                .always_to(Address::new("staging@example.com")),
        );
        let channel = MailChannel::new(Arc::clone(&mailer));

        let user = User { id: 7, email: Some("ada@example.com".into()) };
        channel.send(&Delivery::render(&Welcome, &user).routed_for("mail", &user)).await.unwrap();

        let sent = mailer.sent();
        assert_eq!(sent[0].envelope.to[0].email, "staging@example.com");
        assert!(
            sent[0].headers.iter().any(|(name, value)| name == rainier_mail::ORIGINAL_TO
                && value.contains("ada@example.com")),
            "{:?}",
            sent[0].headers
        );
    }

    #[tokio::test]
    async fn the_mail_channel_says_so_when_the_notification_wrote_no_email() {
        struct DataOnly;

        impl Notification<User> for DataOnly {
            fn notification_name(&self) -> &'static str {
                "test.data-only"
            }
            fn via(&self, _: &User) -> Channels {
                Channels::new()
            }
        }

        let mailer =
            Arc::new(Mailer::fake(views()).with_default_from(Address::new("noreply@example.com")));
        let channel = MailChannel::new(mailer);
        let user = User { id: 7, email: Some("a@b.c".into()) };

        let err = channel
            .send(&Delivery::render(&DataOnly, &user).routed_for("mail", &user))
            .await
            .unwrap_err();

        assert!(err.message().contains("no email"), "{}", err.message());
        assert!(err.message().contains("test.data-only"), "{}", err.message());
    }
}
