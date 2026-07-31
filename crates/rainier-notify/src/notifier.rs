//! Sending — [`Notifier`].

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use rainier_support::Result;

use crate::channel::Channel;
use crate::notification::{Delivery, Notifiable, Notification, Receipt, Sent};

/// Holds the configured channels and fans a notification out across them.
///
/// Bound in the container; resolve it, or reach it through the `Notify`
/// facade.
///
/// ```
/// # use rainier_notify::{Channels, MemoryChannel, Notifiable, Notification, Notifier};
/// # use std::sync::Arc;
/// struct User(u64);
///
/// impl Notifiable for User {
///     fn notifiable_id(&self) -> String { self.0.to_string() }
///     fn notifiable_type(&self) -> &'static str { "User" }
///     fn route_for(&self, _: &str) -> Option<String> { Some(self.0.to_string()) }
/// }
///
/// struct Welcome;
///
/// impl Notification<User> for Welcome {
///     fn notification_name(&self) -> &'static str { "user.welcome" }
///     fn via(&self, _: &User) -> Channels { Channels::new().with::<MemoryChannel>() }
///     fn to_text(&self, _: &User) -> Option<String> { Some("hello".into()) }
/// }
///
/// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
/// let captured = Arc::new(MemoryChannel::new());
/// let notifier = Notifier::new().with_arc(Arc::clone(&captured));
///
/// notifier.send(&User(7), &Welcome).await?;
///
/// assert!(captured.sent_to("user.welcome", "7"));
/// # Ok(()) }
/// ```
#[derive(Default)]
pub struct Notifier {
    channels: HashMap<TypeId, Arc<dyn Channel>>,
    /// Insertion order, so a receipt reads the way the channels were
    /// registered rather than however the map happened to hash them.
    order: Vec<TypeId>,
}

impl Notifier {
    /// A notifier with no channels.
    ///
    /// Sending through it delivers nothing and says so per channel — see
    /// [`Sent::NotRegistered`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a channel.
    pub fn with<C: Channel>(self, channel: C) -> Self {
        self.with_arc(Arc::new(channel))
    }

    /// Register an already-shared channel.
    ///
    /// For one a test holds a handle to — a [`MemoryChannel`](crate::MemoryChannel)
    /// it will assert on.
    pub fn with_arc<C: Channel>(mut self, channel: Arc<C>) -> Self {
        let id = TypeId::of::<C>();
        if self.channels.insert(id, channel).is_none() {
            self.order.push(id);
        }
        self
    }

    /// Whether a channel of this type is registered.
    pub fn has<C: Channel>(&self) -> bool {
        self.channels.contains_key(&TypeId::of::<C>())
    }

    /// The registered channels' names, in registration order.
    pub fn channel_names(&self) -> Vec<&'static str> {
        self.order.iter().filter_map(|id| self.channels.get(id)).map(|c| c.name()).collect()
    }

    /// Send `notification` to `to`.
    ///
    /// Every channel the notification selected is tried. **A channel failing
    /// does not stop the others** — an SMTP outage should not also cost the
    /// database row that puts the notification in the recipient's bell menu.
    ///
    /// The [`Receipt`] says what happened on each. `Ok` unless *every* channel
    /// failed; call [`Receipt::into_result`] to make "nothing got through" an
    /// error at the call site.
    pub async fn send<N, T>(&self, to: &N, notification: &T) -> Result<Receipt>
    where
        N: Notifiable + ?Sized,
        T: Notification<N> + ?Sized,
    {
        let channels = notification.via(to);
        if channels.is_empty() {
            tracing::debug!(
                notification = notification.notification_name(),
                "no channels selected; nothing sent"
            );
            return Ok(Receipt::default());
        }

        // Rendered once, shared across every channel. A notification that
        // implements all three forms should not pay for them per channel.
        let rendered = Delivery::render(notification, to);
        let mut receipt = Receipt::default();

        for (id, type_name) in channels.iter() {
            let Some(channel) = self.channels.get(&id) else {
                // Louder than a skip: the notification asked for a channel the
                // application never wired, and quietly dropping it is how a
                // password reset goes nowhere.
                tracing::error!(
                    notification = rendered.notification,
                    channel = type_name,
                    "the notification selected a channel that is not registered"
                );
                receipt.outcomes.push((type_name, Sent::NotRegistered));
                continue;
            };

            // The channel's own name, from the configured instance — that is
            // what `route_for` is asked about, and what a receipt should say.
            let name = channel.name();

            let route = to.route_for(name);
            if route.is_none() && wants_a_route(name) {
                // A user with no phone number should still get the email.
                tracing::debug!(
                    notification = rendered.notification,
                    channel = name,
                    "the recipient has no address on this channel; skipping"
                );
                receipt.outcomes.push((name, Sent::NoRoute));
                continue;
            }

            let delivery = rendered.with_route(route);

            match channel.send(&delivery).await {
                Ok(()) => receipt.outcomes.push((name, Sent::Delivered)),
                Err(e) => {
                    tracing::error!(
                        notification = rendered.notification,
                        channel = name,
                        error = %e.message(),
                        "the notification failed on this channel"
                    );
                    receipt.outcomes.push((name, Sent::Failed(e)));
                }
            }
        }

        Ok(receipt)
    }

    /// Send the same notification to several recipients.
    ///
    /// One receipt each, in order. A failure for one recipient does not stop
    /// the rest — the same reasoning as a failing channel, one level up.
    pub async fn send_to_many<N, T>(&self, to: &[&N], notification: &T) -> Vec<Result<Receipt>>
    where
        N: Notifiable + ?Sized,
        T: Notification<N> + ?Sized,
    {
        let mut receipts = Vec::with_capacity(to.len());
        for recipient in to {
            receipts.push(self.send(*recipient, notification).await);
        }
        receipts
    }
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier").field("channels", &self.channel_names()).finish()
    }
}

/// Whether a channel is useless without an address.
///
/// The `log`, `memory` and `database` channels address the recipient by their
/// **id**, which every notifiable has. Mail, SMS and a webhook need something
/// the recipient may simply not have, and missing it is a skip rather than a
/// failure.
fn wants_a_route(channel: &str) -> bool {
    !matches!(channel, "log" | "memory" | "database" | "broadcast")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{LogChannel, MemoryChannel};
    use crate::notification::Channels;
    use rainier_support::{BoxFuture, Error};

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
                "mail" | "broken" => self.email.clone(),
                _ => Some(self.id.to_string()),
            }
        }
    }

    /// A channel that always fails, to prove one does not stop the others.
    #[derive(Default)]
    struct BrokenChannel;

    impl Channel for BrokenChannel {
        fn name(&self) -> &'static str {
            "broken"
        }
        fn send<'a>(&'a self, _: &'a Delivery) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Err(Error::internal("the pigeon did not come back")) })
        }
    }

    struct Welcome(Channels);

    impl Notification<User> for Welcome {
        fn notification_name(&self) -> &'static str {
            "user.welcome"
        }
        fn via(&self, _: &User) -> Channels {
            self.0.clone()
        }
        fn to_text(&self, _: &User) -> Option<String> {
            Some("hello".into())
        }
    }

    fn user() -> User {
        User { id: 7, email: Some("ada@example.com".into()) }
    }

    #[tokio::test]
    async fn it_sends_on_every_selected_channel() {
        let captured = Arc::new(MemoryChannel::new());
        let notifier = Notifier::new().with(LogChannel).with_arc(Arc::clone(&captured));

        let receipt = notifier
            .send(&user(), &Welcome(Channels::new().with::<LogChannel>().with::<MemoryChannel>()))
            .await
            .unwrap();

        assert_eq!(receipt.outcomes.len(), 2);
        assert!(receipt.delivered_anywhere());
        assert!(captured.sent_to("user.welcome", "7"));
    }

    #[tokio::test]
    async fn one_channel_failing_does_not_stop_the_others() {
        // An SMTP outage must not also cost the database row that puts the
        // notification in the recipient's bell menu.
        let captured = Arc::new(MemoryChannel::new());
        let notifier = Notifier::new().with(BrokenChannel).with_arc(Arc::clone(&captured));

        let receipt = notifier
            .send(
                &user(),
                &Welcome(Channels::new().with::<BrokenChannel>().with::<MemoryChannel>()),
            )
            .await
            .unwrap();

        assert_eq!(receipt.failures(), vec!["broken"]);
        assert!(captured.sent_to("user.welcome", "7"), "the other channel still ran");
        assert!(receipt.into_result().is_ok(), "one failure is not a failed send");
    }

    #[tokio::test]
    async fn every_channel_failing_is_a_failed_send() {
        let notifier = Notifier::new().with(BrokenChannel);

        let receipt = notifier
            .send(&user(), &Welcome(Channels::new().with::<BrokenChannel>()))
            .await
            .unwrap();

        assert!(!receipt.delivered_anywhere());
        assert!(receipt.into_result().is_err());
    }

    #[tokio::test]
    async fn a_recipient_with_no_address_is_skipped_not_failed() {
        let notifier = Notifier::new().with(BrokenChannel);
        let unreachable = User { id: 8, email: None };

        let receipt = notifier
            .send(&unreachable, &Welcome(Channels::new().with::<BrokenChannel>()))
            .await
            .unwrap();

        assert!(matches!(receipt.outcomes[0], ("broken", Sent::NoRoute)));
        assert!(receipt.failures().is_empty(), "a skip is not a failure");
    }

    #[tokio::test]
    async fn a_channel_addressed_by_id_needs_no_route() {
        // `log`, `memory` and `database` address the recipient by their id,
        // which every notifiable has.
        let captured = Arc::new(MemoryChannel::new());
        let notifier = Notifier::new().with_arc(Arc::clone(&captured));
        let unreachable = User { id: 8, email: None };

        notifier
            .send(&unreachable, &Welcome(Channels::new().with::<MemoryChannel>()))
            .await
            .unwrap();

        assert!(captured.sent_to("user.welcome", "8"));
    }

    #[tokio::test]
    async fn selecting_an_unregistered_channel_is_reported_not_ignored() {
        // Quietly dropping it is how a password reset goes nowhere.
        let notifier = Notifier::new().with(LogChannel);

        let receipt = notifier
            .send(&user(), &Welcome(Channels::new().with::<MemoryChannel>()))
            .await
            .unwrap();

        // The *type* name, because an unregistered channel has no instance to
        // ask for its own.
        assert!(matches!(receipt.outcomes[0], ("MemoryChannel", Sent::NotRegistered)));
        assert!(receipt.into_result().is_err());
    }

    #[tokio::test]
    async fn a_notification_that_selects_nothing_sends_nothing() {
        let notifier = Notifier::new().with(LogChannel);

        let receipt = notifier.send(&user(), &Welcome(Channels::new())).await.unwrap();

        assert!(receipt.outcomes.is_empty());
        assert!(receipt.into_result().is_ok(), "opting out of everything is not an error");
    }

    #[tokio::test]
    async fn several_recipients_get_one_receipt_each() {
        let captured = Arc::new(MemoryChannel::new());
        let notifier = Notifier::new().with_arc(Arc::clone(&captured));

        let (a, b) = (User { id: 1, email: None }, User { id: 2, email: None });
        let receipts = notifier
            .send_to_many(&[&a, &b], &Welcome(Channels::new().with::<MemoryChannel>()))
            .await;

        assert_eq!(receipts.len(), 2);
        assert!(captured.sent_to("user.welcome", "1"));
        assert!(captured.sent_to("user.welcome", "2"));
    }

    #[test]
    fn the_registered_channels_keep_their_order() {
        let notifier = Notifier::new().with(LogChannel).with(MemoryChannel::new());
        assert_eq!(notifier.channel_names(), vec!["log", "memory"]);
        assert!(notifier.has::<LogChannel>());
        assert!(!notifier.has::<BrokenChannel>());
    }
}
