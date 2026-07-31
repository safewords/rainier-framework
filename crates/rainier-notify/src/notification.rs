//! What is sent, and to whom — [`Notification`], [`Notifiable`], [`Channels`].

use std::any::TypeId;
use std::sync::Arc;

use rainier_mail::Mailable;
use rainier_support::Result;
use serde_json::Value;

use crate::channel::Channel;

/// Something that can be sent a notification.
///
/// A user, usually. Also an on-call rota, a Slack channel, a webhook endpoint —
/// anything with an identity and an address.
pub trait Notifiable: Send + Sync {
    /// A stable identifier for this recipient.
    ///
    /// Stored by the database channel and printed in logs, so it has to mean
    /// the same thing across restarts — a primary key, not a pointer.
    fn notifiable_id(&self) -> String;

    /// What kind of thing this is — `"User"`, `"Team"`.
    ///
    /// Stored alongside the id, because id `7` is only unique within a type.
    fn notifiable_type(&self) -> &'static str;

    /// Where to reach this recipient on `channel`, or `None` if you cannot.
    ///
    /// `"mail"` gives an email address,
    /// `"sms"` a phone number, `"slack"` a webhook URL.
    ///
    /// Returning `None` **skips that channel** rather than failing the send: a
    /// user with no phone number should still get the email.
    ///
    /// ```ignore
    /// impl Notifiable for User {
    ///     fn notifiable_id(&self) -> String { self.id.to_string() }
    ///     fn notifiable_type(&self) -> &'static str { "User" }
    ///
    ///     fn route_for(&self, channel: &str) -> Option<String> {
    ///         match channel {
    ///             "mail" => Some(self.email.clone()),
    ///             "sms" => self.phone.clone(),
    ///             _ => None,
    ///         }
    ///     }
    /// }
    /// ```
    fn route_for(&self, channel: &str) -> Option<String>;
}

/// A message to a [`Notifiable`], rendered differently per channel.
///
/// The thing an event is not: it has a **recipient**, and it
/// knows **how** it should reach them.
///
/// ```ignore
/// pub struct InvoicePaid {
///     pub invoice: u64,
///     pub amount: Money,
/// }
///
/// impl Notification<User> for InvoicePaid {
///     fn notification_name(&self) -> &'static str {
///         "billing.invoice-paid"
///     }
///
///     fn via(&self, user: &User) -> Channels {
///         let channels = Channels::new().with::<DatabaseChannel>();
///
///         // The recipient's own preference decides, which is the other half
///         // of what makes this a notification rather than an event.
///         if user.wants_email {
///             channels.with::<MailChannel>()
///         } else {
///             channels
///         }
///     }
///
///     fn to_mail(&self, user: &User) -> Option<Box<dyn Mailable>> {
///         Some(Box::new(InvoicePaidMail { invoice: self.invoice }))
///     }
///
///     fn to_data(&self, _: &User) -> Option<Value> {
///         Some(json!({ "invoice": self.invoice }))
///     }
/// }
/// ```
///
/// # Three renderings, not one per channel
///
/// A PHP framework can give a notification one method per channel —
/// `toMail`, `toDatabase`, `toSlack` — dispatched by `__call`. Rust has no
/// `__call`, and a trait that named every channel would make the set of
/// channels closed.
///
/// So the **representations** are closed and the channels are open. There are
/// three shapes a notification really takes, and every channel consumes one:
///
/// | | Used by |
/// |---|---|
/// | [`to_mail`](Self::to_mail) — a full email | the mail channel |
/// | [`to_text`](Self::to_text) — one short line | SMS, Slack, a webhook, the log |
/// | [`to_data`](Self::to_data) — structured | the database channel, a broadcast |
///
/// A channel you write consumes whichever fits. Nothing here has to change for
/// it to exist.
pub trait Notification<N: Notifiable + ?Sized>: Send + Sync {
    /// A stable name for this notification.
    ///
    /// Stored by the database channel — a row saying which notification it was
    /// — and used in logs. Permanent once rows exist, like a job's `NAME`.
    fn notification_name(&self) -> &'static str;

    /// Which channels to send on, for **this** recipient.
    ///
    /// Given the recipient, so the answer can depend on them: their
    /// preferences, their plan, whether they have a phone number.
    fn via(&self, to: &N) -> Channels;

    /// As an email.
    ///
    /// A [`Mailable`], so a notification gets the same view templates,
    /// attachments and headers a mailable does. An already-assembled
    /// [`Message`](rainier_mail::Message) is itself a `Mailable`, so the
    /// three-lines-of-text case needs no separate type:
    ///
    /// ```ignore
    /// fn to_mail(&self, user: &User) -> Option<Box<dyn Mailable>> {
    ///     Some(Box::new(InvoicePaidMail { name: user.name.clone() }))
    /// }
    /// ```
    ///
    /// Leave the envelope's `to` empty: the **recipient** supplies the
    /// address, via [`Notifiable::route_for`]. A `to_mail` that hard-codes one
    /// has written a mailable, not a notification.
    fn to_mail(&self, to: &N) -> Option<Box<dyn Mailable>> {
        let _ = to;
        None
    }

    /// As one short line — an SMS, a Slack message, a log line.
    fn to_text(&self, to: &N) -> Option<String> {
        let _ = to;
        None
    }

    /// As structured data — a database row, a broadcast payload.
    fn to_data(&self, to: &N) -> Option<Value> {
        let _ = to;
        None
    }
}

/// The channels one notification goes out on.
///
/// Selected **by type**, not by name, for the same reason
/// middleware is: a misspelled `"mial"` is a notification
/// that silently goes nowhere, and deleting a channel should break every
/// notification that used it in the compiler.
///
/// ```
/// # use rainier_notify::{Channels, LogChannel, MemoryChannel};
/// let channels = Channels::new().with::<LogChannel>().with::<MemoryChannel>();
///
/// assert_eq!(channels.len(), 2);
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Channels {
    /// `(TypeId, type name)`.
    ///
    /// The id selects the registered channel. The type name is only a
    /// **diagnostic** label, for the one case where the id resolves to nothing
    /// — a channel the application never registered, which has no instance to
    /// ask for its real name.
    selected: Vec<(TypeId, &'static str)>,
}

impl Channels {
    /// No channels. A notification returning this is not sent anywhere, which
    /// is a legitimate answer — "this user has opted out of everything".
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a channel.
    ///
    /// By type only — no instance and no `Default` bound. Selecting a channel
    /// and *configuring* one are different things: a `MailChannel` needs a
    /// mailer, and the notification has no business holding one.
    pub fn with<C: Channel>(mut self) -> Self {
        self.push(TypeId::of::<C>(), short_type_name(std::any::type_name::<C>()));
        self
    }

    fn push(&mut self, id: TypeId, name: &'static str) {
        // Selecting the same channel twice would send twice. Almost always a
        // paste rather than an intention.
        if !self.selected.iter().any(|(existing, _)| *existing == id) {
            self.selected.push((id, name));
        }
    }

    /// How many channels are selected.
    pub fn len(&self) -> usize {
        self.selected.len()
    }

    /// Whether none are.
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// The selected channels, in order.
    pub fn iter(&self) -> impl Iterator<Item = (TypeId, &'static str)> + '_ {
        self.selected.iter().copied()
    }

    /// Their type names, for diagnostics.
    ///
    /// A channel's *own* [`name`](Channel::name) — the one `route_for` is asked
    /// about — comes from the configured instance, which only the notifier
    /// has.
    pub fn names(&self) -> Vec<&'static str> {
        self.selected.iter().map(|(_, name)| *name).collect()
    }
}

/// `a::b::MailChannel` → `MailChannel`.
fn short_type_name(full: &'static str) -> &'static str {
    full.rsplit("::").next().unwrap_or(full)
}

/// One notification, rendered, on its way to one channel.
///
/// What a [`Channel`] is handed. The renderings are done once per send and
/// cloned per channel, so a notification that implements all three does not pay
/// for rendering them more than once.
#[derive(Clone)]
pub struct Delivery {
    /// The notification's stable name.
    pub notification: &'static str,
    /// The recipient's id.
    pub recipient_id: String,
    /// The recipient's type.
    pub recipient_type: &'static str,
    /// Where to reach them on **this** channel, from
    /// [`Notifiable::route_for`].
    pub route: Option<String>,
    /// `Arc`, not `Box`: one render is shared by every channel, and `Delivery`
    /// is cloned per channel.
    mail: Option<Arc<dyn Mailable>>,
    text: Option<String>,
    data: Option<Value>,
}

impl Delivery {
    /// Render `notification` for `to`.
    pub fn render<N, T>(notification: &T, to: &N) -> Self
    where
        N: Notifiable + ?Sized,
        T: Notification<N> + ?Sized,
    {
        Self {
            notification: notification.notification_name(),
            recipient_id: to.notifiable_id(),
            recipient_type: to.notifiable_type(),
            route: None,
            mail: notification.to_mail(to).map(Arc::from),
            text: notification.to_text(to),
            data: notification.to_data(to),
        }
    }

    /// The same delivery, routed for `channel`.
    pub fn routed_for(mut self, channel: &str, to: &(impl Notifiable + ?Sized)) -> Self {
        self.route = to.route_for(channel);
        self
    }

    /// A copy of this delivery addressed to `route`.
    ///
    /// What the notifier hands each channel: the renderings are shared, only
    /// the address differs.
    pub fn with_route(&self, route: Option<String>) -> Self {
        Self { route, ..self.clone() }
    }

    /// The email form, if the notification produced one.
    ///
    /// Unrendered — the mail channel has the view engine, and this does not.
    pub fn mail(&self) -> Option<&dyn Mailable> {
        self.mail.as_deref()
    }

    /// The short-text form.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// The structured form.
    pub fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }

    /// The structured form, or the text form wrapped as `{"message": …}`.
    ///
    /// For a channel that wants data and will settle for a line of it, which is
    /// most of them.
    pub fn data_or_text(&self) -> Option<Value> {
        self.data
            .clone()
            .or_else(|| self.text.as_ref().map(|text| serde_json::json!({ "message": text })))
    }

    /// The error a channel returns when the notification rendered nothing it
    /// can use.
    pub fn nothing_to_send(&self, channel: &str, wanted: &str) -> rainier_support::Error {
        rainier_support::Error::internal(format!(
            "`{}` selected the `{channel}` channel but produced no {wanted}",
            self.notification
        ))
    }

    /// The error a channel returns when the recipient has no address on it.
    pub fn no_route(&self, channel: &str) -> rainier_support::Error {
        rainier_support::Error::internal(format!(
            "`{} {}` has no `{channel}` address",
            self.recipient_type, self.recipient_id
        ))
    }
}

impl std::fmt::Debug for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Delivery")
            .field("notification", &self.notification)
            .field("to", &format_args!("{} {}", self.recipient_type, self.recipient_id))
            .field("route", &self.route)
            .finish()
    }
}

/// What happened on one channel.
#[derive(Debug)]
pub enum Sent {
    /// It went.
    Delivered,
    /// The recipient has no address on that channel, so it was skipped.
    ///
    /// Not a failure: a user with no phone number should still get the email.
    NoRoute,
    /// The channel is not registered on the notifier.
    NotRegistered,
    /// The channel tried and failed.
    Failed(rainier_support::Error),
}

impl Sent {
    /// Whether it was delivered.
    pub fn is_delivered(&self) -> bool {
        matches!(self, Sent::Delivered)
    }

    /// Whether it failed — as opposed to being skipped.
    pub fn is_failure(&self) -> bool {
        matches!(self, Sent::Failed(_) | Sent::NotRegistered)
    }
}

impl std::fmt::Display for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sent::Delivered => f.write_str("delivered"),
            Sent::NoRoute => f.write_str("no address on that channel"),
            Sent::NotRegistered => f.write_str("the channel is not registered"),
            Sent::Failed(e) => write!(f, "failed: {}", e.message()),
        }
    }
}

/// What happened across every channel of one send.
#[derive(Debug, Default)]
pub struct Receipt {
    /// One entry per channel the notification selected.
    pub outcomes: Vec<(&'static str, Sent)>,
}

impl Receipt {
    /// Whether it reached at least one channel.
    pub fn delivered_anywhere(&self) -> bool {
        self.outcomes.iter().any(|(_, sent)| sent.is_delivered())
    }

    /// The channels it was delivered on.
    pub fn delivered(&self) -> Vec<&'static str> {
        self.outcomes
            .iter()
            .filter(|(_, sent)| sent.is_delivered())
            .map(|(name, _)| *name)
            .collect()
    }

    /// The channels it failed on.
    pub fn failures(&self) -> Vec<&'static str> {
        self.outcomes.iter().filter(|(_, sent)| sent.is_failure()).map(|(name, _)| *name).collect()
    }

    /// An error if **every** selected channel failed, `Ok` otherwise.
    ///
    /// The shape a caller usually wants: one channel being down is not a reason
    /// to fail the request that triggered the notification, but every channel
    /// being down probably is.
    pub fn into_result(self) -> Result<Self> {
        if self.outcomes.is_empty() || self.delivered_anywhere() {
            return Ok(self);
        }

        let reasons: Vec<String> =
            self.outcomes.iter().map(|(name, sent)| format!("{name}: {sent}")).collect();

        Err(rainier_support::Error::internal(format!(
            "the notification reached nothing — {}",
            reasons.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LogChannel, MemoryChannel};

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
            Channels::new().with::<LogChannel>()
        }
        fn to_text(&self, user: &User) -> Option<String> {
            Some(format!("welcome, user {}", user.id))
        }
    }

    #[test]
    fn channels_keep_their_order() {
        let channels = Channels::new().with::<LogChannel>().with::<MemoryChannel>();
        assert_eq!(channels.names(), vec!["LogChannel", "MemoryChannel"]);
    }

    #[test]
    fn selecting_a_channel_twice_only_sends_once() {
        // Almost always a paste rather than an intention, and the symptom
        // otherwise is a user getting two of everything.
        let channels = Channels::new().with::<LogChannel>().with::<LogChannel>();
        assert_eq!(channels.len(), 1);
    }

    #[test]
    fn a_notification_can_choose_no_channels() {
        // "This user has opted out of everything" is a real answer, not an
        // error.
        assert!(Channels::new().is_empty());
    }

    #[test]
    fn rendering_happens_once_and_carries_the_recipients_identity() {
        let user = User { id: 7, email: Some("a@b.c".into()) };
        let delivery = Delivery::render(&Welcome, &user);

        assert_eq!(delivery.notification, "user.welcome");
        assert_eq!(delivery.recipient_id, "7");
        assert_eq!(delivery.recipient_type, "User");
        assert_eq!(delivery.text(), Some("welcome, user 7"));
        assert!(delivery.mail().is_none(), "it did not implement `to_mail`");
    }

    #[test]
    fn routing_asks_the_recipient_per_channel() {
        let user = User { id: 7, email: Some("a@b.c".into()) };

        let mail = Delivery::render(&Welcome, &user).routed_for("mail", &user);
        assert_eq!(mail.route.as_deref(), Some("a@b.c"));

        let none = User { id: 8, email: None };
        let mail = Delivery::render(&Welcome, &none).routed_for("mail", &none);
        assert_eq!(mail.route, None, "no address is a skip, not a failure");
    }

    #[test]
    fn data_falls_back_to_wrapping_the_text() {
        // So a channel that wants structured data works with a notification
        // that only wrote a line.
        let user = User { id: 7, email: None };
        let delivery = Delivery::render(&Welcome, &user);

        assert_eq!(
            delivery.data_or_text(),
            Some(serde_json::json!({ "message": "welcome, user 7" }))
        );
    }

    #[test]
    fn a_receipt_is_ok_when_anything_got_through() {
        let receipt = Receipt {
            outcomes: vec![
                ("mail", Sent::Failed(rainier_support::Error::internal("smtp down"))),
                ("database", Sent::Delivered),
            ],
        };

        assert!(receipt.delivered_anywhere());
        assert_eq!(receipt.failures(), vec!["mail"]);
        assert!(receipt.into_result().is_ok(), "one channel down is not a failed send");
    }

    #[test]
    fn a_receipt_fails_when_nothing_got_through() {
        let receipt = Receipt {
            outcomes: vec![("mail", Sent::Failed(rainier_support::Error::internal("smtp down")))],
        };

        let err = receipt.into_result().err().expect("should fail");
        assert!(err.message().contains("smtp down"), "{}", err.message());
    }

    #[test]
    fn sending_nowhere_at_all_is_not_a_failure() {
        // A notification that selected no channels did what it was asked.
        assert!(Receipt::default().into_result().is_ok());
    }
}
