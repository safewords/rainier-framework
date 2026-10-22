//! Billing through Stripe, end to end, offline.
//!
//! What it shows:
//!
//! 1. a [`Stripe`] client over the fake transport, answering canned JSON;
//! 2. a `Billable` user, and `create_or_get_customer` for it;
//! 3. a Checkout session for a subscription with a trial;
//! 4. a customer-portal URL;
//! 5. a signed webhook fed through [`Webhooks::handle`], with a listener on
//!    `CustomerSubscriptionUpdated` that stores a [`SubscriptionSnapshot`] —
//!    and the access questions asked of what it stored.
//!
//! ```sh
//! cargo run -p rainier-framework --example billing_stripe --features billing-stripe
//! ```
//!
//! In an application the fake becomes the real transport, and the secrets
//! come from the environment:
//!
//! ```ignore
//! use rainier_framework::http_client::ReqwestTransport;
//!
//! let stripe = Stripe::new(Arc::new(ReqwestTransport::new()), env("STRIPE_SECRET"));
//! let verifier = WebhookVerifier::new(env("STRIPE_WEBHOOK_SECRET"));
//! ```

use std::sync::{Arc, Mutex};

use rainier_framework::billing::{
    sign_payload, Billable, BillableExt, CheckoutParams, CustomerSubscriptionUpdated, Stripe,
    SubscriptionSnapshot, WebhookVerifier, Webhooks,
};
use rainier_framework::events::Dispatcher;
use rainier_framework::http::{Method, Request};
use rainier_framework::http_client::{FakeTransport, Transport};
use serde_json::json;

/// The application's user. `stripe_id` is the column the application keeps.
struct User {
    id: u64,
    email: String,
    stripe_id: Option<String>,
}

impl Billable for User {
    fn stripe_customer_id(&self) -> Option<String> {
        self.stripe_id.clone()
    }

    fn billing_email(&self) -> Option<String> {
        Some(self.email.clone())
    }

    fn billing_reference(&self) -> String {
        self.id.to_string()
    }
}

const WEBHOOK_SECRET: &str = "whsec_example";

#[tokio::main]
async fn main() -> rainier_framework::support::Result<()> {
    // --- the client ------------------------------------------------------
    //
    // The fake answers in order; the last answer repeats. These are the
    // three calls the billing page below makes.
    let fake = Arc::new(FakeTransport::new());
    fake.responding(200, json!({ "id": "cus_123", "email": "ada@example.com" }).to_string())
        .responding(
            200,
            json!({ "id": "cs_123", "url": "https://checkout.stripe.com/c/pay/cs_123" })
                .to_string(),
        )
        .responding(
            200,
            json!({ "id": "bps_123", "url": "https://billing.stripe.com/p/session/bps_123" })
                .to_string(),
        );
    let stripe = Stripe::new(Arc::clone(&fake) as Arc<dyn Transport>, "sk_test_example");

    // --- the customer ------------------------------------------------------
    let mut user = User { id: 7, email: "ada@example.com".into(), stripe_id: None };

    let customer = user.create_or_get_customer(&stripe).await?;
    // Yours to persist: the crate does not know your table.
    user.stripe_id = Some(customer.id.clone());
    println!("customer:      {}", customer.id);

    // --- Checkout ------------------------------------------------------------
    let session = user
        .checkout(
            &stripe,
            CheckoutParams::subscription(
                "price_pro_monthly",
                "https://app.test/billing/done?session={CHECKOUT_SESSION_ID}",
                "https://app.test/billing",
            )
            .trial_days(14)
            .allow_promotion_codes(),
        )
        .await?;
    // In a handler: `Ok(Redirect::see_other(session.url.unwrap_or_default()))`.
    println!("checkout:      {}", session.url.unwrap_or_default());

    // --- the portal ----------------------------------------------------------
    let portal = user.billing_portal_url(&stripe, "https://app.test/account").await?;
    println!("portal:        {portal}");

    // What actually left the building:
    for request in fake.recorded() {
        println!("  sent {} {}", request.method(), request.url());
    }

    // --- webhooks ------------------------------------------------------------
    //
    // The listener keeps the application's copy of the subscription current.
    // Here it is a mutex; in an application it is a row.
    let events = Arc::new(Dispatcher::new());
    let stored: Arc<Mutex<Option<SubscriptionSnapshot>>> = Arc::new(Mutex::new(None));

    let store = Arc::clone(&stored);
    events.listen(move |event: Arc<CustomerSubscriptionUpdated>| {
        let store = Arc::clone(&store);
        async move {
            *store.lock().expect("snapshot lock") = Some(event.snapshot());
            Ok(())
        }
    });

    // Add `.deduplicate_with(cache)` with a shared store in production, so a
    // redelivery is handled once.
    let webhooks = Arc::new(Webhooks::new(WebhookVerifier::new(WEBHOOK_SECRET), events));

    // Mounted as a route — outside CSRF protection, since Stripe cannot send
    // a token:
    //
    //     router.post("/stripe/webhook", webhooks.handler()).name("stripe.webhook");

    let now = chrono::Utc::now().timestamp();
    let body = json!({
        "id": "evt_123",
        "type": "customer.subscription.updated",
        "created": now,
        "livemode": false,
        "data": {
            "object": {
                "id": "sub_123",
                "customer": "cus_123",
                "status": "trialing",
                "trial_end": now + 14 * 86_400,
                "cancel_at_period_end": false,
                "items": { "data": [{
                    "id": "si_123",
                    "price": { "id": "price_pro_monthly" },
                    "quantity": 1,
                    "current_period_end": now + 14 * 86_400
                }]}
            },
            "previous_attributes": { "status": "incomplete" }
        }
    })
    .to_string();

    // Signed the way Stripe signs it, so the real verifier accepts it.
    let signature = sign_payload(WEBHOOK_SECRET, now, body.as_bytes());
    let delivery = Request::builder()
        .method(Method::POST)
        .uri("/stripe/webhook")
        .header("Stripe-Signature", &signature)
        .body(body.clone())
        .build();
    println!("webhook:       {}", webhooks.handle(&delivery).await.status());

    // A forgery is refused before any listener runs.
    let forged = Request::builder()
        .method(Method::POST)
        .uri("/stripe/webhook")
        .header("Stripe-Signature", &sign_payload("whsec_attacker", now, body.as_bytes()))
        .body(body)
        .build();
    println!("forged:        {}", webhooks.handle(&forged).await.status());

    // --- access, from the stored copy ------------------------------------------
    let snapshot = stored.lock().expect("snapshot lock").clone().expect("the listener ran");
    let now = chrono::Utc::now();
    println!("subscription:  {} ({:?}) on {:?}", snapshot.id, snapshot.status, snapshot.prices);
    println!("  valid():           {}", snapshot.valid(now));
    println!("  on_trial():        {}", snapshot.on_trial(now));
    println!("  on_grace_period(): {}", snapshot.on_grace_period(now));

    Ok(())
}
