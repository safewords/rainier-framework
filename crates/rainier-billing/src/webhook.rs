//! Stripe webhooks — [`WebhookVerifier`], [`WebhookEvent`] and [`Webhooks`].
//!
//! Webhooks are where billing actually happens. A redirect back from
//! Checkout can be skipped by closing the tab; a renewal, a failed card, a
//! cancellation from the customer portal or a refund from the dashboard
//! produce no redirect at all. The only record of any of them reaching this
//! application is a `POST` from Stripe — so that endpoint has to be right.
//!
//! # What right means
//!
//! 1. **Verified.** The endpoint is public; anybody can `POST` a
//!    `customer.subscription.updated` saying they are now on the top plan.
//!    [`WebhookVerifier`] checks Stripe's HMAC-SHA256 signature over the
//!    **exact bytes** received — which is why the route must sit before
//!    anything that parses and re-serialises the body, and outside CSRF
//!    protection, which a server-to-server call cannot satisfy.
//! 2. **Fresh.** The signed timestamp must be within the tolerance (five
//!    minutes), so a captured delivery cannot be replayed next week.
//! 3. **Once.** Stripe delivers *at least* once. With a cache configured,
//!    [`Webhooks`] claims each event id atomically before dispatching it and
//!    answers a repeat with `200` and nothing else.
//! 4. **Retried on failure.** A listener that fails releases the claim and
//!    answers `500`, so Stripe tries again later — rather than `200`, which
//!    would tell Stripe the event was handled when it was dropped.
//!
//! # Wiring
//!
//! ```ignore
//! let webhooks = Arc::new(
//!     Webhooks::new(WebhookVerifier::new(env("STRIPE_WEBHOOK_SECRET")), events.clone())
//!         .deduplicate_with(cache.store().clone()),
//! );
//! router.post("/stripe/webhook", webhooks.handler()).name("stripe.webhook");
//!
//! events.listen(|event: Arc<CustomerSubscriptionUpdated>| async move {
//!     subscriptions.save(&event.snapshot()).await
//! });
//! ```
//!
//! # Rotating the secret
//!
//! Rolling a signing secret in the dashboard keeps the old one valid for a
//! while. [`WebhookVerifier::with_secret`] accepts both during that window;
//! a signature matching either is accepted.

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use rainier_cache::Cache;
use rainier_events::Dispatcher;
use rainier_http::{Request, Response, StatusCode};
use rainier_support::{BoxFuture, Error, ErrorKind, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::subscription::SubscriptionSnapshot;

type HmacSha256 = Hmac<Sha256>;

/// The header Stripe signs with.
pub const SIGNATURE_HEADER: &str = "stripe-signature";

/// Verifies `Stripe-Signature`.
#[derive(Clone)]
pub struct WebhookVerifier {
    secrets: Vec<String>,
    tolerance: Duration,
}

impl WebhookVerifier {
    /// Verify against this endpoint secret (`whsec_…`).
    pub fn new(secret: impl Into<String>) -> Self {
        Self { secrets: vec![secret.into()], tolerance: Duration::from_secs(300) }
    }

    /// Also accept this secret — for the window while one is being rotated.
    pub fn with_secret(mut self, secret: impl Into<String>) -> Self {
        self.secrets.push(secret.into());
        self
    }

    /// How old a signed timestamp may be. Five minutes by default, which is
    /// Stripe's own; zero turns the check off, which you should not do.
    pub fn tolerance(mut self, tolerance: Duration) -> Self {
        self.tolerance = tolerance;
        self
    }

    /// Verify `payload` against `header` at the current time.
    pub fn verify(&self, payload: &[u8], header: &str) -> Result<()> {
        self.verify_at(payload, header, chrono::Utc::now().timestamp())
    }

    /// Verify at `now` (Unix seconds) — for tests.
    pub fn verify_at(&self, payload: &[u8], header: &str, now: i64) -> Result<()> {
        let (timestamp, signatures) = parse_header(header)?;

        if signatures.is_empty() {
            return Err(refused("the signature header has no v1 signature"));
        }

        let matched = self.secrets.iter().any(|secret| {
            let expected = sign(secret, timestamp, payload);
            signatures.iter().any(|candidate| {
                decode_hex(candidate)
                    .is_some_and(|candidate| bool::from(candidate.ct_eq(expected.as_slice())))
            })
        });
        if !matched {
            return Err(refused("no signature matched the payload"));
        }

        // After the signature, not before: an unsigned timestamp is not
        // worth an opinion, and reporting "expired" for a forgery would tell
        // the forger which half to fix.
        if !self.tolerance.is_zero() {
            let age = now.saturating_sub(timestamp).unsigned_abs();
            if age > self.tolerance.as_secs() {
                return Err(refused("the signed timestamp is outside the tolerance"));
            }
        }

        Ok(())
    }

    /// Verify and parse — what a receiver does.
    pub fn construct_event(&self, payload: &[u8], header: &str) -> Result<WebhookEvent> {
        self.verify(payload, header)?;
        WebhookEvent::parse(payload)
    }
}

impl std::fmt::Debug for WebhookVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookVerifier")
            .field("secrets", &self.secrets.len())
            .field("tolerance", &self.tolerance)
            .finish()
    }
}

/// A `Stripe-Signature` value for `payload`, signed with `secret` at
/// `timestamp` — so a test can deliver a webhook the real verifier accepts.
pub fn sign_payload(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let signature: String =
        sign(secret, timestamp, payload).iter().map(|byte| format!("{byte:02x}")).collect();
    format!("t={timestamp},v1={signature}")
}

fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> Vec<u8> {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    mac.finalize().into_bytes().to_vec()
}

/// `t=…,v1=…,v1=…,v0=…` → the timestamp and every `v1`. Other schemes
/// (`v0` is test-mode only) are ignored, never accepted.
fn parse_header(header: &str) -> Result<(i64, Vec<&str>)> {
    let mut timestamp = None;
    let mut signatures = Vec::new();

    for part in header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            return Err(refused("the signature header is malformed"));
        };
        match key {
            "t" => {
                timestamp = Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| refused("the signature timestamp is not a number"))?,
                )
            }
            "v1" => signatures.push(value),
            _ => {}
        }
    }

    let timestamp = timestamp.ok_or_else(|| refused("the signature header has no timestamp"))?;
    Ok((timestamp, signatures))
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn refused(why: &str) -> Error {
    Error::bad_request(format!("stripe webhook refused: {why}"))
}

/// A webhook delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// `evt_…` — the deduplication key.
    pub id: String,
    /// `customer.subscription.updated`, …
    #[serde(rename = "type")]
    pub kind: String,
    /// When Stripe created it, Unix seconds.
    #[serde(default)]
    pub created: i64,
    /// Live or test mode.
    #[serde(default)]
    pub livemode: bool,
    /// The API version the object is rendered in — the endpoint's, which
    /// may differ from the one the client pins.
    #[serde(default)]
    pub api_version: Option<String>,
    /// The object, and what changed.
    pub data: EventData,
    /// The connected account, for a Connect webhook.
    #[serde(default)]
    pub account: Option<String>,
}

/// An event's payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventData {
    /// The object as it is now.
    pub object: Value,
    /// The fields an `*.updated` event changed, with their old values.
    #[serde(default)]
    pub previous_attributes: Option<Value>,
}

impl WebhookEvent {
    /// Parse a payload. Does **not** verify it — see
    /// [`WebhookVerifier::construct_event`].
    pub fn parse(payload: &[u8]) -> Result<Self> {
        serde_json::from_slice(payload)
            .map_err(|e| Error::bad_request(format!("stripe webhook is not an event: {e}")))
    }

    /// The object, deserialised.
    pub fn object<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.data.object.clone()).map_err(|e| {
            Error::internal(format!("stripe event {} held an unexpected object: {e}", self.id))
        })
    }

    /// The object's `id`.
    pub fn object_id(&self) -> Option<&str> {
        self.data.object["id"].as_str()
    }

    /// Whether `field` changed in this update.
    pub fn changed(&self, field: &str) -> bool {
        self.data.previous_attributes.as_ref().is_some_and(|previous| previous.get(field).is_some())
    }
}

/// Every verified event, before its typed counterpart — for a listener that
/// wants all of them (an audit log, a generic sync).
#[derive(Debug, Clone)]
pub struct StripeEventReceived(pub Arc<WebhookEvent>);

macro_rules! typed_events {
    ($( $(#[$doc:meta])* $name:ident => $kind:literal ),+ $(,)?) => {
        $(
            $(#[$doc])*
            #[doc = concat!("\n\nDispatched for `", $kind, "`.")]
            #[derive(Debug, Clone)]
            pub struct $name(pub Arc<WebhookEvent>);

            impl $name {
                /// The Stripe event type this is dispatched for.
                pub const TYPE: &'static str = $kind;

                /// The event.
                pub fn event(&self) -> &WebhookEvent {
                    &self.0
                }
            }
        )+

        /// Dispatch the typed event for `event`, if there is one.
        async fn dispatch_typed(events: &Dispatcher, event: &Arc<WebhookEvent>) -> Result<()> {
            match event.kind.as_str() {
                $( $kind => events.dispatch($name(Arc::clone(event))).await, )+
                _ => Ok(()),
            }
        }

        /// Every event type that has a typed counterpart.
        pub const TYPED_EVENTS: &[&str] = &[$($kind),+];
    };
}

typed_events! {
    /// A Checkout session finished — the moment to grant a purchase.
    CheckoutSessionCompleted => "checkout.session.completed",
    /// A subscription was created.
    CustomerSubscriptionCreated => "customer.subscription.created",
    /// A subscription changed — plan, quantity, status, cancellation.
    CustomerSubscriptionUpdated => "customer.subscription.updated",
    /// A subscription ended.
    CustomerSubscriptionDeleted => "customer.subscription.deleted",
    /// An invoice was paid — a renewal went through.
    InvoicePaid => "invoice.paid",
    /// An invoice payment failed — the moment to tell the customer.
    InvoicePaymentFailed => "invoice.payment_failed",
    /// A payment intent succeeded.
    PaymentIntentSucceeded => "payment_intent.succeeded",
    /// A payment intent failed.
    PaymentIntentPaymentFailed => "payment_intent.payment_failed",
    /// A charge was refunded, wholly or partly.
    ChargeRefunded => "charge.refunded",
    /// A customer's details changed.
    CustomerUpdated => "customer.updated",
    /// A customer was deleted.
    CustomerDeleted => "customer.deleted",
}

impl CustomerSubscriptionCreated {
    /// The subscription's state, ready to store.
    pub fn snapshot(&self) -> SubscriptionSnapshot {
        SubscriptionSnapshot::from_object(&self.0.data.object)
    }
}

impl CustomerSubscriptionUpdated {
    /// The subscription's state, ready to store.
    pub fn snapshot(&self) -> SubscriptionSnapshot {
        SubscriptionSnapshot::from_object(&self.0.data.object)
    }
}

impl CustomerSubscriptionDeleted {
    /// The subscription's state, ready to store.
    pub fn snapshot(&self) -> SubscriptionSnapshot {
        SubscriptionSnapshot::from_object(&self.0.data.object)
    }
}

impl CheckoutSessionCompleted {
    /// The session.
    pub fn session(&self) -> Result<crate::resources::checkout::CheckoutSession> {
        self.0.object()
    }
}

/// What became of one delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Verified and dispatched.
    Handled(String),
    /// Verified, and already handled once.
    Duplicate(String),
}

/// Receives webhook deliveries: verify, deduplicate, dispatch.
pub struct Webhooks {
    verifier: WebhookVerifier,
    events: Arc<Dispatcher>,
    dedupe: Option<Arc<dyn Cache>>,
    dedupe_ttl: Duration,
}

impl Webhooks {
    /// Verify with `verifier` and dispatch onto `events`.
    pub fn new(verifier: WebhookVerifier, events: Arc<Dispatcher>) -> Self {
        Self { verifier, events, dedupe: None, dedupe_ttl: Duration::from_secs(3 * 24 * 60 * 60) }
    }

    /// Remember handled event ids in `cache`, so a redelivery is not handled
    /// twice. Use a **shared** store: a per-process one deduplicates nothing
    /// across replicas.
    pub fn deduplicate_with(mut self, cache: Arc<dyn Cache>) -> Self {
        self.dedupe = Some(cache);
        self
    }

    /// How long a handled id is remembered. Three days by default — Stripe
    /// retries an undelivered event for up to three days in live mode.
    pub fn remember_for(mut self, ttl: Duration) -> Self {
        self.dedupe_ttl = ttl;
        self
    }

    /// Verify, deduplicate and dispatch one delivery.
    ///
    /// A bad signature is a `BadRequest`; a listener's failure is whatever
    /// the listener returned, after the claim on the id is released.
    pub async fn receive(&self, payload: &[u8], signature: Option<&str>) -> Result<Outcome> {
        let signature =
            signature.ok_or_else(|| refused("the request has no Stripe-Signature header"))?;
        let event = Arc::new(self.verifier.construct_event(payload, signature)?);
        let key = format!("stripe:webhook:{}", event.id);

        if let Some(cache) = &self.dedupe {
            if !cache.add(&key, b"1", Some(self.dedupe_ttl)).await? {
                tracing::debug!(event = %event.id, kind = %event.kind, "stripe webhook already handled");
                return Ok(Outcome::Duplicate(event.id.clone()));
            }
        }

        if let Err(error) = self.dispatch(&event).await {
            if let Some(cache) = &self.dedupe {
                // Released so Stripe's retry is handled rather than skipped.
                if let Err(release) = cache.forget(&key).await {
                    tracing::error!(event = %event.id, error = %release, "could not release a stripe webhook claim");
                }
            }
            return Err(error);
        }

        Ok(Outcome::Handled(event.id.clone()))
    }

    async fn dispatch(&self, event: &Arc<WebhookEvent>) -> Result<()> {
        self.events.dispatch(StripeEventReceived(Arc::clone(event))).await?;
        dispatch_typed(&self.events, event).await
    }

    /// Handle a request: `200` handled or duplicate, `400` refused, `500`
    /// when a listener failed so Stripe retries.
    pub async fn handle(&self, request: &Request) -> Response {
        match self.receive(request.body(), request.header(SIGNATURE_HEADER)).await {
            Ok(Outcome::Handled(id)) => {
                Response::json(&serde_json::json!({ "received": true, "id": id }))
            }
            Ok(Outcome::Duplicate(id)) => Response::json(
                &serde_json::json!({ "received": true, "id": id, "duplicate": true }),
            ),
            Err(error) if error.kind() == ErrorKind::BadRequest => {
                tracing::warn!(error = %error.message(), "stripe webhook refused");
                Response::json(&serde_json::json!({ "error": error.message() }))
                    .with_status(StatusCode::BAD_REQUEST)
            }
            Err(error) => {
                tracing::error!(error = %error.message(), "stripe webhook listener failed");
                // The listener's message stays in the log: Stripe's dashboard
                // shows this body to whoever looks.
                Response::json(&serde_json::json!({ "error": "webhook handling failed" }))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }

    /// A route handler — `router.post("/stripe/webhook", webhooks.handler())`.
    pub fn handler(
        self: &Arc<Self>,
    ) -> impl Fn(Arc<Request>) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static {
        let webhooks = Arc::clone(self);
        move |request: Arc<Request>| {
            let webhooks = Arc::clone(&webhooks);
            Box::pin(async move { webhooks.handle(&request).await })
        }
    }
}

impl std::fmt::Debug for Webhooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhooks")
            .field("verifier", &self.verifier)
            .field("deduplicates", &self.dedupe.is_some())
            .field("ttl", &self.dedupe_ttl)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;
    use serde_json::json;

    const SECRET: &str = "whsec_test";

    fn payload(id: &str, kind: &str) -> Vec<u8> {
        json!({
            "id": id,
            "type": kind,
            "created": 1_800_000_000,
            "livemode": false,
            "api_version": "2025-09-30.clover",
            "data": {
                "object": {"id": "sub_1", "customer": "cus_1", "status": "active",
                           "items": {"data": [{"price": {"id": "price_pro"}, "quantity": 1}]}},
                "previous_attributes": {"status": "trialing"}
            }
        })
        .to_string()
        .into_bytes()
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn a_signature_stripe_made_is_accepted() {
        let body = payload("evt_1", "invoice.paid");
        let header = sign_payload(SECRET, 1_000, &body);
        WebhookVerifier::new(SECRET).verify_at(&body, &header, 1_010).unwrap();
    }

    #[test]
    fn a_tampered_payload_is_refused() {
        let body = payload("evt_1", "invoice.paid");
        let header = sign_payload(SECRET, 1_000, &body);
        let mut tampered = body.clone();
        tampered[10] ^= 1;

        let err = WebhookVerifier::new(SECRET).verify_at(&tampered, &header, 1_000).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::BadRequest);
        assert!(err.message().contains("no signature matched"), "{}", err.message());
    }

    #[test]
    fn the_wrong_secret_is_refused() {
        let body = payload("evt_1", "invoice.paid");
        let header = sign_payload("whsec_other", 1_000, &body);
        assert!(WebhookVerifier::new(SECRET).verify_at(&body, &header, 1_000).is_err());
    }

    #[test]
    fn an_old_delivery_is_refused_as_a_replay() {
        let body = payload("evt_1", "invoice.paid");
        let header = sign_payload(SECRET, 1_000, &body);

        let err = WebhookVerifier::new(SECRET).verify_at(&body, &header, 1_000 + 301).unwrap_err();
        assert!(err.message().contains("tolerance"), "{}", err.message());

        WebhookVerifier::new(SECRET)
            .tolerance(Duration::from_secs(600))
            .verify_at(&body, &header, 1_000 + 301)
            .unwrap();
    }

    #[test]
    fn a_forgery_is_not_told_its_timestamp_was_the_problem() {
        let body = payload("evt_1", "invoice.paid");
        let header = sign_payload("whsec_other", 1_000, &body);
        let err = WebhookVerifier::new(SECRET).verify_at(&body, &header, 99_999).unwrap_err();
        assert!(err.message().contains("no signature matched"), "{}", err.message());
    }

    #[test]
    fn a_rotated_secret_is_accepted_alongside_the_new_one() {
        let body = payload("evt_1", "invoice.paid");
        let verifier = WebhookVerifier::new("whsec_new").with_secret("whsec_old");

        verifier.verify_at(&body, &sign_payload("whsec_old", 5, &body), 5).unwrap();
        verifier.verify_at(&body, &sign_payload("whsec_new", 5, &body), 5).unwrap();
    }

    #[test]
    fn any_of_several_v1_signatures_may_match() {
        let body = payload("evt_1", "invoice.paid");
        let good = sign_payload(SECRET, 5, &body);
        let good_sig = good.split("v1=").nth(1).unwrap();
        let header = format!("t=5,v1={},v1={good_sig},v0=deadbeef", "00".repeat(32));
        WebhookVerifier::new(SECRET).verify_at(&body, &header, 5).unwrap();
    }

    #[test]
    fn malformed_headers_are_refused_with_a_reason() {
        let body = payload("evt_1", "invoice.paid");
        let verifier = WebhookVerifier::new(SECRET);

        for (header, reason) in [
            ("", "malformed"),
            ("garbage", "malformed"),
            ("v1=abcd", "no timestamp"),
            ("t=abc,v1=abcd", "not a number"),
            ("t=5", "no v1 signature"),
            ("t=5,v0=abcd", "no v1 signature"),
            ("t=5,v1=zz", "no signature matched"),
        ] {
            let err = verifier.verify_at(&body, header, 5).unwrap_err();
            assert!(err.message().contains(reason), "{header:?}: {}", err.message());
        }
    }

    #[test]
    fn an_event_parses_and_says_what_changed() {
        let event =
            WebhookEvent::parse(&payload("evt_1", "customer.subscription.updated")).unwrap();
        assert_eq!(event.kind, "customer.subscription.updated");
        assert_eq!(event.object_id(), Some("sub_1"));
        assert!(event.changed("status"));
        assert!(!event.changed("quantity"));
    }

    fn request(body: Vec<u8>, signature: Option<String>) -> Request {
        let mut builder =
            Request::builder().method(rainier_http::Method::POST).uri("/stripe/webhook");
        if let Some(signature) = signature {
            builder = builder.header("Stripe-Signature", &signature);
        }
        builder.body(body).build()
    }

    #[tokio::test]
    async fn a_delivery_dispatches_the_generic_and_the_typed_event() {
        let events = Arc::new(Dispatcher::fake());
        let webhooks = Webhooks::new(WebhookVerifier::new(SECRET), Arc::clone(&events));

        let body = payload("evt_1", "customer.subscription.updated");
        let signature = sign_payload(SECRET, now(), &body);
        let response = webhooks.handle(&request(body, Some(signature))).await;

        assert_eq!(response.status(), StatusCode::OK);
        events.assert_dispatched_times::<StripeEventReceived>(1);
        let typed = events.dispatched::<CustomerSubscriptionUpdated>();
        assert_eq!(typed.len(), 1);
        let snapshot = typed[0].snapshot();
        assert_eq!(snapshot.id, "sub_1");
        assert!(snapshot.has_price("price_pro"));
    }

    #[tokio::test]
    async fn an_event_with_no_typed_counterpart_still_reaches_the_generic_listener() {
        let events = Arc::new(Dispatcher::fake());
        let webhooks = Webhooks::new(WebhookVerifier::new(SECRET), Arc::clone(&events));

        let body = payload("evt_1", "product.created");
        let outcome = webhooks.receive(&body, Some(&sign_payload(SECRET, now(), &body))).await;

        assert_eq!(outcome.unwrap(), Outcome::Handled("evt_1".into()));
        events.assert_dispatched_times::<StripeEventReceived>(1);
        assert_eq!(events.dispatched_names().len(), 1);
    }

    #[tokio::test]
    async fn a_bad_or_missing_signature_is_a_400_and_dispatches_nothing() {
        let events = Arc::new(Dispatcher::fake());
        let webhooks = Webhooks::new(WebhookVerifier::new(SECRET), Arc::clone(&events));

        let body = payload("evt_1", "invoice.paid");
        let forged = sign_payload("whsec_attacker", now(), &body);

        assert_eq!(
            webhooks.handle(&request(body.clone(), Some(forged))).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(webhooks.handle(&request(body, None)).await.status(), StatusCode::BAD_REQUEST);
        events.assert_not_dispatched::<StripeEventReceived>();
    }

    #[tokio::test]
    async fn a_redelivery_is_handled_once() {
        let events = Arc::new(Dispatcher::fake());
        let webhooks = Webhooks::new(WebhookVerifier::new(SECRET), Arc::clone(&events))
            .deduplicate_with(Arc::new(MemoryCache::new()));

        let body = payload("evt_1", "invoice.paid");
        let signature = sign_payload(SECRET, now(), &body);

        let first = webhooks.receive(&body, Some(&signature)).await.unwrap();
        let second = webhooks.receive(&body, Some(&signature)).await.unwrap();

        assert_eq!(first, Outcome::Handled("evt_1".into()));
        assert_eq!(second, Outcome::Duplicate("evt_1".into()));
        events.assert_dispatched_times::<InvoicePaid>(1);
    }

    #[tokio::test]
    async fn a_failing_listener_is_a_500_and_releases_the_claim_so_the_retry_runs() {
        let events = Arc::new(Dispatcher::new());
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        events.listen(move |_: Arc<InvoicePaymentFailed>| {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Err(Error::internal("the mailer is down"))
                } else {
                    Ok(())
                }
            }
        });

        let webhooks = Arc::new(
            Webhooks::new(WebhookVerifier::new(SECRET), events)
                .deduplicate_with(Arc::new(MemoryCache::new())),
        );
        let handler = webhooks.handler();

        let body = payload("evt_9", "invoice.payment_failed");
        let signature = sign_payload(SECRET, now(), &body);

        let failed = handler(Arc::new(request(body.clone(), Some(signature.clone())))).await;
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let retried = handler(Arc::new(request(body, Some(signature)))).await;
        assert_eq!(retried.status(), StatusCode::OK);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn every_typed_event_is_listed() {
        assert_eq!(TYPED_EVENTS.len(), 11);
        assert!(TYPED_EVENTS.contains(&CheckoutSessionCompleted::TYPE));
        assert!(TYPED_EVENTS.contains(&"charge.refunded"));
    }
}
