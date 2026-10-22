# rainier-billing

Stripe for the [Rainier](https://github.com/safewords/rainier-framework)
framework:

- a client over the framework's HTTP transport port — pinned `Stripe-Version`,
  an `Idempotency-Key` on every write, typed `StripeError`s classified as
  retryable or not;
- customers, Checkout, the customer portal, subscriptions (swap, quantity,
  cancel, resume), payment and setup intents, refunds, invoices, prices and
  products — plus `get` / `post` / `call` / `list_all` for every other endpoint;
- webhooks that are verified (HMAC-SHA256, constant time, secret rotation),
  replay-protected, deduplicated through the cache, and dispatched onto the
  event bus as typed events;
- `SubscriptionSnapshot`, with `valid` / `on_trial` / `on_grace_period` /
  `ended`, so access is answered from a stored row rather than a call to Stripe.

Enable it through the framework: `rainier-framework = { features = ["billing-stripe"] }`.
See [`docs/billing.md`](../../docs/billing.md).
