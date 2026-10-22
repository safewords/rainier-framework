# Billing

Taking money through Stripe — subscriptions, one-off payments, refunds — and
knowing, on every request, whether somebody has paid.

```rust
let session = user
    .checkout(&stripe, CheckoutParams::subscription("price_pro", success, cancel).trial_days(14))
    .await?;

Ok(Redirect::see_other(session.url.unwrap_or_default()))
```

The shape: a client, hosted Checkout and portal pages so
the application never sees a card number, webhooks that keep a local copy of
each subscription current, and predicates — `valid`, `on_trial`,
`on_grace_period` — that answer the access question from that copy.

Enable it with the framework's `billing-stripe` feature, which also turns on
the `http-client` transport. The crate is `rainier-billing`, re-exported as
`rainier_framework::billing`.

## Quick start

A runnable version of everything below, offline against the fake transport:
[`crates/rainier-framework/examples/billing_stripe.rs`](../crates/rainier-framework/examples/billing_stripe.rs).

```sh
cargo run -p rainier-framework --example billing_stripe --features billing-stripe
```

**1. The feature.**

```toml
[dependencies]
rainier-framework = { version = "2", features = ["billing-stripe"] }
```

**2. The secrets.** Both come from the Stripe dashboard; neither belongs in
the repository.

```sh
STRIPE_SECRET=sk_test_…          # Developers → API keys
STRIPE_WEBHOOK_SECRET=whsec_…    # Developers → Webhooks → your endpoint
```

**3. The client and the receiver**, built once at boot:

```rust
use rainier_framework::billing::{Stripe, WebhookVerifier, Webhooks};
use rainier_framework::http_client::ReqwestTransport;

let stripe = Stripe::new(Arc::new(ReqwestTransport::new()), std::env::var("STRIPE_SECRET")?);

let webhooks = Arc::new(
    Webhooks::new(WebhookVerifier::new(std::env::var("STRIPE_WEBHOOK_SECRET")?), events.clone())
        // A shared store, so a redelivery is handled once across replicas.
        .deduplicate_with(cache.store().clone()),
);
```

**4. The route** — outside CSRF protection, since Stripe cannot send a token:

```rust
router.post("/stripe/webhook", webhooks.handler()).name("stripe.webhook");
```

**5. The listener** that keeps your copy of each subscription current:

```rust
use rainier_framework::billing::CustomerSubscriptionUpdated;

events.listen(move |event: Arc<CustomerSubscriptionUpdated>| {
    let subscriptions = subscriptions.clone();
    async move { subscriptions.save(&event.snapshot()).await }
});
```

Listen to `CustomerSubscriptionCreated` and `CustomerSubscriptionDeleted` the
same way, and grant one-off purchases from `CheckoutSessionCompleted`.

**6. The gate**, from the stored snapshot — no call to Stripe:

```rust
if !snapshot.valid(chrono::Utc::now()) {
    return Err(Error::unauthorized("This needs an active subscription."));
}
```

Locally, `stripe listen --forward-to localhost:8000/stripe/webhook` prints a
`whsec_…` for `STRIPE_WEBHOOK_SECRET` and forwards test-mode events.

## The client

```rust
let stripe = Stripe::new(Arc::new(ReqwestTransport::new()), env("STRIPE_SECRET"));
```

`Stripe` is cheap to clone; bind one in the container. Every request carries:

| | |
|---|---|
| `Authorization: Bearer sk_…` | the secret key, never in `Debug` output |
| `Stripe-Version` | pinned to `DEFAULT_API_VERSION`; `.api_version(…)` moves it |
| `Idempotency-Key` | on every `POST` — generated, or yours via `.idempotency_key(…)` |
| `Stripe-Account` | when acting for a connected account: `stripe.for_account("acct_…")` |

**The version is pinned on purpose.** Without the header, Stripe answers in
whatever version the account is set to in the dashboard, so a click in a
browser changes the shape of every response this code parses. Pinned, a
version change is a code change.

**Generated idempotency keys protect one HTTP call, not a job.** A queued job
that creates a charge and is retried generates a new key on the retry and
charges twice. Derive the key from your own record:

```rust
stripe.payment_intents().create(
    PaymentIntentParams::new(order.total_cents, "usd")
        .customer(customer)
        .confirm()
        .off_session()
        .idempotency_key(format!("order-{}-charge", order.id)),
).await?;
```

### Errors

A refusal becomes a framework `Error` that quotes Stripe — type, code, decline
code, parameter, message — and carries it as details:

```rust
match stripe.payment_intents().create(params).await {
    Err(error) => match StripeError::from_error(&error) {
        Some(card) if card.is_card_error() => show_customer(card.message),
        _ => return Err(error),
    },
    Ok(intent) => …
}
```

| status | kind | |
|---|---|---|
| 429, 5xx, transport failure | `ServiceUnavailable` | retryable — a queued job's retry policy keys on it |
| 400, 402 | `BadRequest` | this request or this card |
| 404 | `NotFound` | |
| 409 | `Conflict` | an idempotency key reused with different parameters |
| 401, 403, other | `Internal` | the key is wrong — ours to fix |

### The typed surface

| | |
|---|---|
| `stripe.customers()` | create, retrieve, update, delete, list |
| `stripe.checkout()` | create (payment, subscription, setup), retrieve, expire |
| `stripe.billing_portal()` | a portal session |
| `stripe.subscriptions()` | create, retrieve, `for_customer`, `swap`, `update_quantity`, `cancel_at_period_end`, `resume`, `cancel_now`, pause and `resume_paused` |
| `stripe.payment_intents()` | create, retrieve, update, confirm, capture, cancel |
| `stripe.setup_intents()` | create, retrieve, cancel |
| `stripe.refunds()` | create, retrieve |
| `stripe.invoices()` | `for_customer`, list, retrieve, `preview`, pay, void |
| `stripe.catalog()` | active prices and products, prices by lookup key |

Response structs name the fields worth naming and keep the rest in `extra`, so
a field Stripe adds next month is one map lookup away rather than a parse
failure.

### Everything else

```rust
let balance = stripe.get("/v1/balance", Params::new()).await?;
let coupon: Value = stripe.post("/v1/coupons", json!({ "percent_off": 25, "duration": "once" })).await?;
let items: Vec<Value> = stripe.list_all("/v1/charges", Params::new().set("limit", 100), 5_000).await?;
```

Same headers, keys and errors. `Params` encodes nested values the way Stripe
expects — `line_items[0][price]`, `metadata[key]`, `expand[0]` — and `null`
or an empty collection is how Stripe spells "unset".

## The billable model

```rust
impl Billable for User {
    fn stripe_customer_id(&self) -> Option<String> { self.stripe_id.clone() }
    fn billing_email(&self) -> Option<String> { Some(self.email.clone()) }
    fn billing_reference(&self) -> String { self.id.to_string() }
}
```

`BillableExt` gives it `create_or_get_customer`, `checkout` and
`billing_portal_url`. The crate does not write to your table: persist
`customer.id` yourself. The reference is stored in the customer's metadata
as `rainier_reference` and sent as Checkout's `client_reference_id`, so a
webhook can always be traced back to a row.

## Webhooks

Webhooks are where billing actually happens. A redirect from Checkout can be
skipped by closing the tab; a renewal, a failed card, a cancellation in the
portal or a refund from the dashboard produce no redirect at all.

```rust
let webhooks = Arc::new(
    Webhooks::new(WebhookVerifier::new(env("STRIPE_WEBHOOK_SECRET")), events.clone())
        .deduplicate_with(cache.store().clone()),
);
router.post("/stripe/webhook", webhooks.handler()).name("stripe.webhook");
```

A delivery is:

1. **verified** — HMAC-SHA256 over the exact bytes received, constant-time,
   any of several `v1` signatures, any of several secrets during a rotation;
2. **fresh** — the signed timestamp within five minutes, so a captured
   delivery cannot be replayed;
3. **deduplicated** — the event id is claimed atomically in the cache, and a
   redelivery answers `200` without dispatching;
4. **dispatched** — `StripeEventReceived` for every event, then a typed event
   for the ones that matter;
5. **retried on failure** — a listener error releases the claim and answers
   `500`, so Stripe tries again. Answering `200` would tell Stripe an event
   was handled when it was dropped.

The route must sit **outside CSRF protection** (Stripe cannot send a token)
and before anything that re-serialises the body (the signature is over the
bytes). Use a **shared** cache store for deduplication; a per-process one
deduplicates nothing across replicas.

| typed event | Stripe type |
|---|---|
| `CheckoutSessionCompleted` | `checkout.session.completed` |
| `CustomerSubscriptionCreated` / `Updated` / `Deleted` | `customer.subscription.*` |
| `InvoicePaid` / `InvoicePaymentFailed` | `invoice.paid`, `invoice.payment_failed` |
| `PaymentIntentSucceeded` / `PaymentIntentPaymentFailed` | `payment_intent.*` |
| `ChargeRefunded` | `charge.refunded` |
| `CustomerUpdated` / `CustomerDeleted` | `customer.*` |

```rust
events.listen(move |event: Arc<CustomerSubscriptionUpdated>| {
    let subscriptions = subscriptions.clone();
    async move { subscriptions.save(&event.snapshot()).await }
});
```

Grant purchases from `CheckoutSessionCompleted`, never from the page Checkout
redirects to.

## Subscription state

Asking Stripe "has this user paid?" on every request would be slow,
rate-limited, and would lock every customer out during a Stripe outage. Store
a `SubscriptionSnapshot` per subscription — from the webhooks above — and ask
it:

| | true when |
|---|---|
| `on_trial(now)` | `trialing` and the trial has not ended |
| `canceled()` | told to end, now or at the period's end |
| `on_grace_period(now)` | canceled, still inside the paid-for period |
| `ended(now)` | canceled and past the grace period |
| `active(now)` | `active` or `trialing`, and not ended |
| `valid(now)` | active, on trial, or on a grace period — **gate features on this** |

`past_due` is deliberately not active; write `active(now) || past_due()` where
you mean to be generous. A status this version does not know parses as
`Unknown` and grants nothing. The snapshot reads `current_period_end` from the
subscription or from its first item, because API versions from 2025-03-31
moved it.

## Testing

```rust
let (stripe, fake) = Stripe::fake();
fake.responding(200, r#"{"id":"cs_1","url":"https://checkout.stripe.com/x"}"#);

start_checkout(&stripe, &user).await?;

fake.assert_sent(|request| request.url_contains("/v1/checkout/sessions"));
```

And a webhook the real verifier accepts:

```rust
let body = json!({ "id": "evt_1", "type": "invoice.paid", "data": { "object": {} } }).to_string();
let signature = sign_payload("whsec_test", chrono::Utc::now().timestamp(), body.as_bytes());

let response = webhooks.handle(
    &Request::builder().method(Method::POST).header("Stripe-Signature", &signature).body(body).build(),
).await;
```

## What is not here

- **Persistence.** No migration and no subscriptions table: the snapshot is
  the row's content, and the table is yours.
- **Stripe Elements.** The browser side of a custom payment form is
  Stripe.js; this crate creates the intents it confirms.
- **Retries inside the client.** A failure is classified, not retried, so the
  retry happens where the idempotency key is known — the job.
- **Other processors.** The crate is Stripe's; nothing here pretends a second
  processor would have the same shape.
