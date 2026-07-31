//! The [`Mailer`] — where a mailable becomes a delivered message.

use std::sync::{Arc, Mutex};

use rainier_events::Dispatcher;
use rainier_support::{Error, Result};
use rainier_view::ViewEngine;

use crate::mailable::Mailable;
use crate::message::{Address, Message};
use crate::transport::Transport;

/// Fired before a message is handed to the transport. A listener returning
/// `Err` cancels the send — how a suppression list is enforced.
#[derive(Debug, Clone)]
pub struct MessageSending {
    /// The message about to be sent.
    pub message: Message,
}

/// Fired after a message is accepted by the transport.
#[derive(Debug, Clone)]
pub struct MessageSent {
    /// The message that was sent.
    pub message: Message,
}

/// Renders mailables and hands them to a transport.
pub struct Mailer {
    views: Arc<dyn ViewEngine>,
    transport: Arc<dyn Transport>,
    events: Option<Arc<Dispatcher>>,
    /// The sender applied to any message that does not name one.
    default_from: Option<Address>,
    /// When set, every message is redirected here instead of its recipients.
    always_to: Option<Address>,
    /// `Some` while faking: messages are recorded, never sent.
    recorded: Option<Mutex<Vec<Message>>>,
}

/// The header recording who a redirected message was really for.
pub const ORIGINAL_TO: &str = "X-Rainier-Original-To";

impl Mailer {
    /// A mailer rendering through `views` and delivering through `transport`.
    pub fn new(views: Arc<dyn ViewEngine>, transport: Arc<dyn Transport>) -> Self {
        Self { views, transport, events: None, default_from: None, always_to: None, recorded: None }
    }

    /// A mailer that **records** messages instead of sending them.
    pub fn fake(views: Arc<dyn ViewEngine>) -> Self {
        Self {
            views,
            transport: Arc::new(crate::transport::MemoryTransport::new()),
            events: None,
            default_from: None,
            always_to: None,
            recorded: Some(Mutex::new(Vec::new())),
        }
    }

    /// Fire [`MessageSending`] and [`MessageSent`] through `events`.
    pub fn with_events(mut self, events: Arc<Dispatcher>) -> Self {
        self.events = Some(events);
        self
    }

    /// The sender used when a mailable does not set one.
    pub fn with_default_from(mut self, from: Address) -> Self {
        self.default_from = Some(from);
        self
    }

    /// Redirect **every** message to one address.
    ///
    /// A staging safety valve: it is the difference between testing an email
    /// flow against production data and emailing every one of those customers.
    /// The original recipients are recorded in `X-Rainier-Original-To`.
    pub fn always_to(mut self, address: Address) -> Self {
        self.always_to = Some(address);
        self
    }

    /// Whether this mailer is recording instead of sending.
    pub fn is_faking(&self) -> bool {
        self.recorded.is_some()
    }

    /// The transport in use.
    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    /// Render and deliver a mailable.
    pub async fn send(&self, mailable: &dyn Mailable) -> Result<Message> {
        let message = self.prepare(mailable)?;
        self.deliver(message).await
    }

    /// Render a mailable without sending it — for previewing, and for
    /// asserting on the rendered output in a test.
    pub fn prepare(&self, mailable: &dyn Mailable) -> Result<Message> {
        Ok(self.apply_defaults(self.render(mailable)?))
    }

    /// Render a mailable **without** filling in the sender or applying
    /// `always_to`.
    ///
    /// For a caller that must still change the message before it goes — the
    /// notification mail channel addresses it from the recipient, and doing
    /// that after `always_to` had already rewritten `to` would record the
    /// redirect against an empty address. [`deliver`](Self::deliver) applies
    /// the defaults, so rendering here loses nothing.
    pub fn render(&self, mailable: &dyn Mailable) -> Result<Message> {
        mailable.build(self.views.as_ref())
    }

    /// Fill in the sender and apply `always_to`.
    ///
    /// Called by **both** entry points. It used to be only in `prepare`, which
    /// meant anything assembling its own `Message` and calling
    /// [`deliver`](Self::deliver) — a notification's mail channel, most
    /// obviously — silently bypassed both. The `always_to` half of that is the
    /// serious one: it is the staging safety net, and skipping it means mailing
    /// real customers from a copy of production data.
    ///
    /// Idempotent, so `send`'s prepare-then-deliver does not apply it twice.
    fn apply_defaults(&self, mut message: Message) -> Message {
        if message.envelope.from.is_none() {
            message.envelope.from = self.default_from.clone();
        }

        let Some(always) = &self.always_to else { return message };

        // The header is the marker: if it is there, this has already run and
        // the recipients on the message are the redirect, not the originals.
        if message.headers.iter().any(|(name, _)| name == ORIGINAL_TO) {
            return message;
        }

        let original: Vec<String> =
            message.envelope.recipients().iter().map(|a| a.email.clone()).collect();

        message.envelope.to = vec![always.clone()];
        message.envelope.cc.clear();
        message.envelope.bcc.clear();
        message.with_header(ORIGINAL_TO, original.join(", "))
    }

    /// Deliver an already-assembled message.
    ///
    /// Applies the configured sender and `always_to` first, so a caller that
    /// built its own `Message` gets the same treatment as one that went through
    /// a [`Mailable`].
    pub async fn deliver(&self, message: Message) -> Result<Message> {
        let message = self.apply_defaults(message);
        message.validate()?;

        // The `sending` hook runs before validation's counterpart — the
        // transport — so a listener can veto a message that would otherwise
        // have gone out.
        if let Some(events) = &self.events {
            events
                .dispatch(MessageSending { message: message.clone() })
                .await
                .map_err(|e| Error::internal(format!("the message was not sent: {e}")))?;
        }

        if let Some(recorded) = &self.recorded {
            recorded.lock().expect("recorder lock poisoned").push(message.clone());
            return Ok(message);
        }

        self.transport.send(&message).await?;

        if let Some(events) = &self.events {
            // Sent is past tense: the message has gone, so a failing listener
            // is logged rather than turned into a delivery failure.
            events.dispatch_quietly(MessageSent { message: message.clone() }).await;
        }

        Ok(message)
    }

    // --- assertions (faking) -----------------------------------------------

    /// Every recorded message. Always empty unless faking.
    pub fn sent(&self) -> Vec<Message> {
        match &self.recorded {
            Some(recorded) => recorded.lock().expect("recorder lock poisoned").clone(),
            None => Vec::new(),
        }
    }

    /// Recorded messages addressed to `email`.
    pub fn sent_to(&self, email: &str) -> Vec<Message> {
        self.sent()
            .into_iter()
            .filter(|message| {
                message.envelope.recipients().iter().any(|address| address.email == email)
            })
            .collect()
    }

    /// Panic unless a message was sent to `email`.
    ///
    /// # Panics
    ///
    /// If none was, or the mailer is not faking — which would otherwise make
    /// every assertion pass vacuously.
    pub fn assert_sent_to(&self, email: &str) {
        self.require_faking("assert_sent_to");
        assert!(
            !self.sent_to(email).is_empty(),
            "expected a message to `{email}`, but the recipients were {:?}",
            self.sent()
                .iter()
                .flat_map(|m| m.envelope.recipients().into_iter().map(|a| a.email.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// Panic unless exactly `times` messages were sent.
    ///
    /// # Panics
    ///
    /// If the count differs, or the mailer is not faking.
    pub fn assert_sent_times(&self, times: usize) {
        self.require_faking("assert_sent_times");
        let actual = self.sent().len();
        assert_eq!(actual, times, "expected {times} message(s) to be sent, but {actual} were");
    }

    /// Panic if anything was sent.
    ///
    /// # Panics
    ///
    /// If something was, or the mailer is not faking.
    pub fn assert_nothing_sent(&self) {
        self.require_faking("assert_nothing_sent");
        assert!(self.sent().is_empty(), "expected no mail, but {} was sent", self.sent().len());
    }

    fn require_faking(&self, method: &str) {
        assert!(
            self.is_faking(),
            "`{method}` needs a faking mailer — build it with `Mailer::fake()`, otherwise \
             nothing is recorded and the assertion is meaningless"
        );
    }
}

impl std::fmt::Debug for Mailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailer")
            .field("transport", &self.transport.name())
            .field("faking", &self.is_faking())
            .field("always_to", &self.always_to)
            .finish()
    }
}

#[cfg(test)]
mod deliver_defaults_tests {
    use super::*;
    use crate::message::{Address, Envelope, Message};
    use rainier_view::MemoryEngine;

    fn message_to(recipient: &str) -> Message {
        let mut envelope = Envelope::new("Subject");
        envelope.to.push(Address::new(recipient));

        let mut message = Message::new(envelope);
        message.text = Some("body".into());
        message
    }

    #[tokio::test]
    async fn deliver_fills_in_the_configured_sender() {
        // Anything that assembles its own `Message` — a notification's mail
        // channel — goes through `deliver` rather than `prepare`, and used to
        // miss this entirely.
        let mailer = Mailer::fake(Arc::new(MemoryEngine::new()))
            .with_default_from(Address::new("noreply@example.com"));

        mailer.deliver(message_to("ada@example.com")).await.unwrap();

        assert_eq!(mailer.sent()[0].envelope.from.as_ref().unwrap().email, "noreply@example.com");
    }

    #[tokio::test]
    async fn deliver_honours_always_to() {
        // The serious half. `always_to` is the staging safety net, and a
        // message that skipped it goes to the real customer in the copied
        // database.
        let mailer = Mailer::fake(Arc::new(MemoryEngine::new()))
            .with_default_from(Address::new("noreply@example.com"))
            .always_to(Address::new("dev@example.com"));

        mailer.deliver(message_to("customer@example.com")).await.unwrap();

        let sent = &mailer.sent()[0];
        assert_eq!(sent.envelope.to.len(), 1);
        assert_eq!(sent.envelope.to[0].email, "dev@example.com");
        assert!(
            sent.headers.iter().any(|(n, v)| n == ORIGINAL_TO && v == "customer@example.com"),
            "the real recipient should be recorded: {:?}",
            sent.headers
        );
    }

    #[tokio::test]
    async fn applying_the_defaults_twice_does_not_lose_the_original_recipient() {
        // `send` is prepare-then-deliver, so the redirect runs twice. Without
        // the guard the second pass would record the *redirected* address as
        // the original.
        let mailer = Mailer::fake(Arc::new(MemoryEngine::new()))
            .with_default_from(Address::new("noreply@example.com"))
            .always_to(Address::new("dev@example.com"));

        let once = mailer.apply_defaults(message_to("customer@example.com"));
        let twice = mailer.apply_defaults(once);

        let recorded: Vec<&String> =
            twice.headers.iter().filter(|(n, _)| n == ORIGINAL_TO).map(|(_, v)| v).collect();

        assert_eq!(recorded, vec!["customer@example.com"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Content, Envelope};
    use crate::transport::MemoryTransport;
    use rainier_view::MemoryEngine;

    struct Welcome {
        to: String,
        from: Option<String>,
    }

    impl Welcome {
        fn to(to: &str) -> Self {
            Self { to: to.into(), from: Some("app@example.com".into()) }
        }
        fn without_sender(to: &str) -> Self {
            Self { to: to.into(), from: None }
        }
    }

    impl Mailable for Welcome {
        fn envelope(&self) -> Envelope {
            let mut envelope = Envelope::new("Welcome").to(self.to.clone());
            if let Some(from) = &self.from {
                envelope = envelope.from(from.clone());
            }
            envelope
        }
        fn content(&self) -> Result<Content> {
            Ok(Content::text("Hello!"))
        }
    }

    fn views() -> Arc<dyn ViewEngine> {
        Arc::new(MemoryEngine::new())
    }

    fn mailer_with(transport: Arc<MemoryTransport>) -> Mailer {
        Mailer::new(views(), transport)
    }

    #[tokio::test]
    async fn sending_hands_the_message_to_the_transport() {
        let transport = Arc::new(MemoryTransport::new());
        let mailer = mailer_with(Arc::clone(&transport));

        mailer.send(&Welcome::to("ada@example.com")).await.unwrap();

        assert_eq!(transport.count(), 1);
        assert_eq!(transport.sent()[0].envelope.subject, "Welcome");
    }

    #[tokio::test]
    async fn the_default_sender_fills_in_when_a_mailable_omits_one() {
        let transport = Arc::new(MemoryTransport::new());
        let mailer = mailer_with(Arc::clone(&transport))
            .with_default_from(Address::named("noreply@example.com", "Rainier"));

        mailer.send(&Welcome::without_sender("ada@example.com")).await.unwrap();

        assert_eq!(
            transport.sent()[0].envelope.from.as_ref().unwrap().email,
            "noreply@example.com"
        );
    }

    #[tokio::test]
    async fn a_mailables_own_sender_wins_over_the_default() {
        let transport = Arc::new(MemoryTransport::new());
        let mailer = mailer_with(Arc::clone(&transport))
            .with_default_from(Address::new("noreply@example.com"));

        mailer.send(&Welcome::to("ada@example.com")).await.unwrap();
        assert_eq!(transport.sent()[0].envelope.from.as_ref().unwrap().email, "app@example.com");
    }

    #[tokio::test]
    async fn an_undeliverable_message_never_reaches_the_transport() {
        let transport = Arc::new(MemoryTransport::new());
        let mailer = mailer_with(Arc::clone(&transport));

        // No sender configured anywhere.
        let err = mailer.send(&Welcome::without_sender("ada@example.com")).await.unwrap_err();
        assert!(err.message().contains("sender"), "{}", err.message());
        assert_eq!(transport.count(), 0);
    }

    #[tokio::test]
    async fn a_transport_failure_surfaces() {
        let mailer = Mailer::new(views(), Arc::new(MemoryTransport::failing("smtp down")));
        let err = mailer.send(&Welcome::to("ada@example.com")).await.unwrap_err();
        assert!(err.message().contains("smtp down"), "{}", err.message());
    }

    #[tokio::test]
    async fn always_to_redirects_every_recipient_and_records_the_originals() {
        struct ManyRecipients;
        impl Mailable for ManyRecipients {
            fn envelope(&self) -> Envelope {
                Envelope::new("Bulk")
                    .from("app@example.com")
                    .to("a@example.com")
                    .cc("b@example.com")
                    .bcc("c@example.com")
            }
            fn content(&self) -> Result<Content> {
                Ok(Content::text("hi"))
            }
        }

        let transport = Arc::new(MemoryTransport::new());
        let mailer =
            mailer_with(Arc::clone(&transport)).always_to(Address::new("staging@example.com"));

        mailer.send(&ManyRecipients).await.unwrap();

        let sent = &transport.sent()[0];
        assert_eq!(sent.envelope.recipients().len(), 1);
        assert_eq!(sent.envelope.to[0].email, "staging@example.com");
        assert!(sent.envelope.cc.is_empty());
        assert!(sent.envelope.bcc.is_empty());

        let original = sent.headers.iter().find(|(name, _)| name == "X-Rainier-Original-To");
        let (_, value) = original.expect("the originals should be recorded");
        assert!(value.contains("a@example.com"), "{value}");
        assert!(value.contains("c@example.com"), "{value}");
    }

    #[tokio::test]
    async fn a_sending_listener_can_veto_the_message() {
        let transport = Arc::new(MemoryTransport::new());
        let events = Arc::new(Dispatcher::new());
        events.listen(|event: Arc<MessageSending>| async move {
            if event.message.envelope.to.iter().any(|a| a.email.ends_with("@blocked.test")) {
                return Err(Error::internal("recipient is suppressed"));
            }
            Ok(())
        });

        let mailer = mailer_with(Arc::clone(&transport)).with_events(events);

        let err = mailer.send(&Welcome::to("ada@blocked.test")).await.unwrap_err();
        assert!(err.message().contains("suppressed"), "{}", err.message());
        assert_eq!(transport.count(), 0, "a vetoed message must not be sent");

        assert!(mailer.send(&Welcome::to("ada@example.com")).await.is_ok());
        assert_eq!(transport.count(), 1);
    }

    #[tokio::test]
    async fn a_failing_sent_listener_does_not_fail_the_send() {
        // The message has already gone; there is nothing to undo.
        let transport = Arc::new(MemoryTransport::new());
        let events = Arc::new(Dispatcher::new());
        events.listen(|_: Arc<MessageSent>| async { Err(Error::internal("metrics down")) });

        let mailer = mailer_with(Arc::clone(&transport)).with_events(events);

        assert!(mailer.send(&Welcome::to("ada@example.com")).await.is_ok());
        assert_eq!(transport.count(), 1);
    }

    #[tokio::test]
    async fn prepare_renders_without_sending() {
        let transport = Arc::new(MemoryTransport::new());
        let mailer = mailer_with(Arc::clone(&transport));

        let message = mailer.prepare(&Welcome::to("ada@example.com")).unwrap();
        assert_eq!(message.text.as_deref(), Some("Hello!"));
        assert_eq!(transport.count(), 0);
    }

    #[tokio::test]
    async fn a_fake_records_instead_of_sending() {
        let mailer = Mailer::fake(views());

        mailer.send(&Welcome::to("ada@example.com")).await.unwrap();
        mailer.send(&Welcome::to("grace@example.com")).await.unwrap();

        mailer.assert_sent_times(2);
        mailer.assert_sent_to("ada@example.com");
        assert_eq!(mailer.sent_to("nobody@example.com").len(), 0);
    }

    #[tokio::test]
    async fn a_fake_still_validates_the_message() {
        let mailer = Mailer::fake(views());
        assert!(mailer.send(&Welcome::without_sender("ada@example.com")).await.is_err());
        mailer.assert_nothing_sent();
    }

    #[tokio::test]
    #[should_panic(expected = "needs a faking mailer")]
    async fn assertions_refuse_to_pass_vacuously() {
        let mailer = Mailer::new(views(), Arc::new(MemoryTransport::new()));
        mailer.send(&Welcome::to("ada@example.com")).await.unwrap();
        mailer.assert_nothing_sent();
    }
}
