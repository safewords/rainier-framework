//! Sending — [`Notifier`].

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use std::time::Instant;

use rainier_events::activity::{has_activity_subscribers, publish_activity_with};
use rainier_events::report::{report, Exception, NotificationFailed};
use rainier_events::Operation;
use rainier_support::{Error, ErrorKind, Result};

use crate::channel::Channel;
use crate::events::{error_type, NotificationSent};
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
    ///
    /// Publishes [`NotificationSent`], and runs as an [`Operation`] when
    /// somebody hears it, so the mail a channel sends nests under it.
    pub async fn send<N, T>(&self, to: &N, notification: &T) -> Result<Receipt>
    where
        N: Notifiable + ?Sized,
        T: Notification<N> + ?Sized,
    {
        let started = Instant::now();
        let op = has_activity_subscribers::<NotificationSent>().then(Operation::new);
        let send = self.send_traced(to, notification);
        let result = match &op {
            Some(op) => op.scope(send).await,
            None => send.await,
        };
        // Outside the operation: the notification is the parent of what its
        // channels published, not of itself.
        publish_activity_with(|| NotificationSent {
            notification: notification.notification_name(),
            channels: result.as_ref().ok().map(|receipt| receipt.outcomes.len()),
            duration: started.elapsed(),
            error_type: result.as_ref().err().map(error_type),
            operation: op.as_ref().map(Operation::id),
        });
        result
    }

    /// The send, in a `notification.send` span when `telemetry` is on.
    async fn send_traced<N, T>(&self, to: &N, notification: &T) -> Result<Receipt>
    where
        N: Notifiable + ?Sized,
        T: Notification<N> + ?Sized,
    {
        // The notification's name and how many channels it went to — never
        // the recipient, and never what it said.
        #[cfg(feature = "telemetry")]
        {
            let span = rainier_support::span!(
                "notification.send",
                otel.name = %format!("notify {}", notification.notification_name()),
                rainier.notification.name = notification.notification_name(),
                rainier.notification.channels = tracing::field::Empty,
            );
            let traced = span.clone();
            return tracing::Instrument::instrument(
                async move {
                    let result = self.send_untraced(to, notification).await;
                    if let Ok(receipt) = &result {
                        traced.record("rainier.notification.channels", receipt.outcomes.len());
                    }
                    rainier_support::telemetry::record_result(&traced, result)
                },
                span,
            )
            .await;
        }

        #[cfg(not(feature = "telemetry"))]
        self.send_untraced(to, notification).await
    }

    async fn send_untraced<N, T>(&self, to: &N, notification: &T) -> Result<Receipt>
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
                report(NotificationFailed::new(
                    Exception::message(
                        ErrorKind::Internal,
                        format!("the notification selected `{type_name}`, which is not registered"),
                    ),
                    type_name,
                    rendered.notification,
                ))
                .await;
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
                    report(NotificationFailed::new(snapshot(&e), name, rendered.notification))
                        .await;
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

/// A report's copy of an error the receipt keeps: everything but the error
/// value itself, which is not `Clone`.
fn snapshot(error: &Error) -> Exception {
    Exception {
        details: error.details().cloned(),
        sources: error
            .source_error()
            .map(|source| source.chain().map(ToString::to_string).collect())
            .unwrap_or_default(),
        backtrace: error.backtrace_text().map(str::to_string),
        ..Exception::message(error.kind(), error.message())
    }
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

    /// Records the operation each delivery runs inside.
    #[derive(Default)]
    struct Watching(std::sync::Mutex<Vec<Option<u64>>>);

    impl Channel for Watching {
        fn name(&self) -> &'static str {
            "memory"
        }
        fn send<'a>(&'a self, _: &'a Delivery) -> BoxFuture<'a, Result<()>> {
            self.0.lock().unwrap().push(rainier_events::Context::current().parent());
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn a_send_publishes_notification_sent_and_its_channels_run_inside_it() {
        use rainier_events::{Activity, Dispatcher, Value};

        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::set_default(Arc::clone(&events));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        events.on::<NotificationSent>().run_sync(move |sent, cx| {
            s.lock().unwrap().push(((*sent).clone(), cx.parent()));
            Ok(())
        });
        let watching = Arc::new(Watching::default());
        let notifier = Notifier::new().with(LogChannel).with_arc(Arc::clone(&watching));

        notifier
            .send(&user(), &Welcome(Channels::new().with::<LogChannel>().with::<Watching>()))
            .await
            .unwrap();
        events.settled().await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (sent, parent) = &seen[0];
        assert_eq!(*parent, None);
        assert_eq!(sent.kind(), "notification.send");
        assert!(!sent.failed());
        let fields = sent.collect_fields();
        assert_eq!(fields.text("otel.name"), Some("notify user.welcome"));
        assert_eq!(fields.text("rainier.notification.name"), Some("user.welcome"));
        assert_eq!(fields.get("rainier.notification.channels"), Some(&Value::U64(2)));
        assert!(sent.operation.is_some());
        assert_eq!(*watching.0.lock().unwrap(), vec![sent.operation]);
    }

    #[tokio::test]
    async fn nobody_listening_opens_no_operation() {
        let events = Arc::new(rainier_events::Dispatcher::new());
        let _bus = rainier_events::set_default(events);
        let watching = Arc::new(Watching::default());
        let notifier = Notifier::new().with_arc(Arc::clone(&watching));

        notifier.send(&user(), &Welcome(Channels::new().with::<Watching>())).await.unwrap();

        assert_eq!(*watching.0.lock().unwrap(), vec![None]);
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

    /// The `NotificationFailed` reports for one notification name.
    fn notification_failures(
        capture: &rainier_events::report::Capture,
        notification: &str,
    ) -> Vec<NotificationFailed> {
        use rainier_events::report::{Failure, FailureKind};
        capture
            .of_kind(FailureKind::Notification)
            .into_iter()
            .filter_map(|f| match f {
                Failure::Notification(n) if n.notification == notification => Some(n),
                _ => None,
            })
            .collect()
    }

    struct Reported(Channels);

    impl Notification<User> for Reported {
        fn notification_name(&self) -> &'static str {
            "notify-test.reported"
        }
        fn via(&self, _: &User) -> Channels {
            self.0.clone()
        }
        fn to_text(&self, _: &User) -> Option<String> {
            Some("hello".into())
        }
    }

    #[tokio::test]
    async fn a_failing_or_unregistered_channel_is_reported_on_the_bus() {
        let capture = rainier_events::report::capture();
        let notifier = Notifier::new().with(BrokenChannel);

        notifier
            .send(
                &user(),
                &Reported(Channels::new().with::<BrokenChannel>().with::<MemoryChannel>()),
            )
            .await
            .unwrap();

        let failures = notification_failures(&capture, "notify-test.reported");
        assert_eq!(failures.len(), 2, "{failures:?}");
        let broken = failures.iter().find(|f| f.channel == "broken").unwrap();
        assert_eq!(broken.exception.message, "the pigeon did not come back");
        assert!(
            failures.iter().any(|f| f.channel.ends_with("MemoryChannel")),
            "the unregistered channel is named by its type: {failures:?}"
        );
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
