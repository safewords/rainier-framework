//! The typed endpoints — one module per Stripe resource.
//!
//! Every response struct is **lenient**: the fields worth naming are named
//! and optional, and everything else lands in `extra`. Stripe adds fields
//! constantly and moves a few between API versions; a struct that refused a
//! response it did not fully expect would turn every one of those into an
//! outage. What is not named is still there, one map lookup away.

pub mod checkout;
pub mod customers;
pub mod invoices;
pub mod payments;
pub mod subscriptions;

use serde_json::Value;

use crate::client::Stripe;

/// A path segment, escaped. Stripe ids are `[A-Za-z0-9_]`, but an id that came
/// from a request parameter is whatever the requester typed, and `../` in a
/// URL path is how one endpoint becomes another.
pub(crate) fn segment(id: &str) -> String {
    form_urlencoded::byte_serialize(id.as_bytes()).collect()
}

/// The id of an expandable field — a bare string, or an object with an `id`.
pub(crate) fn id_of(value: Option<&Value>) -> Option<&str> {
    match value? {
        Value::String(id) => Some(id),
        Value::Object(object) => object.get("id")?.as_str(),
        _ => None,
    }
}

impl Stripe {
    /// `/v1/customers`.
    pub fn customers(&self) -> customers::Customers<'_> {
        customers::Customers { stripe: self }
    }

    /// `/v1/checkout/sessions`.
    pub fn checkout(&self) -> checkout::Checkout<'_> {
        checkout::Checkout { stripe: self }
    }

    /// `/v1/billing_portal/sessions`.
    pub fn billing_portal(&self) -> checkout::BillingPortal<'_> {
        checkout::BillingPortal { stripe: self }
    }

    /// `/v1/subscriptions`.
    pub fn subscriptions(&self) -> subscriptions::Subscriptions<'_> {
        subscriptions::Subscriptions { stripe: self }
    }

    /// `/v1/payment_intents`.
    pub fn payment_intents(&self) -> payments::PaymentIntents<'_> {
        payments::PaymentIntents { stripe: self }
    }

    /// `/v1/setup_intents`.
    pub fn setup_intents(&self) -> payments::SetupIntents<'_> {
        payments::SetupIntents { stripe: self }
    }

    /// `/v1/refunds`.
    pub fn refunds(&self) -> payments::Refunds<'_> {
        payments::Refunds { stripe: self }
    }

    /// `/v1/invoices`.
    pub fn invoices(&self) -> invoices::Invoices<'_> {
        invoices::Invoices { stripe: self }
    }

    /// `/v1/prices` and `/v1/products`.
    pub fn catalog(&self) -> invoices::Catalog<'_> {
        invoices::Catalog { stripe: self }
    }
}

#[cfg(test)]
mod tests {
    use super::checkout::{CheckoutMode, CheckoutParams};
    use super::customers::CustomerParams;
    use super::payments::{PaymentIntentParams, RefundReason};
    use super::subscriptions::Proration;
    use crate::form::Params;
    use crate::Stripe;
    use serde_json::json;

    fn has(body: &str, pair: &str) -> bool {
        body.split('&').any(|p| p == pair)
    }

    #[test]
    fn a_hostile_id_cannot_walk_the_path() {
        assert_eq!(super::segment("cus_1/../../v1/charges"), "cus_1%2F..%2F..%2Fv1%2Fcharges");
    }

    #[tokio::test]
    async fn customers_round_trip_and_keep_unknown_fields() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(
            200,
            json!({"id":"cus_1","email":"ada@example.com","metadata":{"user_id":"7"},"tax_exempt":"none"})
                .to_string(),
        );

        let customer = stripe
            .customers()
            .create(
                CustomerParams::new().email("ada@example.com").name("Ada").metadata("user_id", "7"),
            )
            .await
            .unwrap();
        assert_eq!(customer.id, "cus_1");
        assert_eq!(customer.metadata["user_id"], "7");
        assert_eq!(customer.extra["tax_exempt"], "none");

        let body = fake.recorded()[0].body();
        assert!(has(&body, "email=ada%40example.com"), "{body}");
        assert!(has(&body, "name=Ada"), "{body}");
        assert!(has(&body, "metadata%5Buser_id%5D=7"), "{body}");

        stripe.customers().delete("cus_1").await.unwrap();
        let sent = &fake.recorded()[1];
        assert_eq!(sent.method(), "DELETE");
        assert_eq!(sent.url(), "https://api.stripe.com/v1/customers/cus_1");
    }

    #[tokio::test]
    async fn a_subscription_checkout_is_the_shape_stripe_documents() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(
            200,
            json!({"id":"cs_1","url":"https://checkout.stripe.com/c/pay/cs_1","customer":{"id":"cus_1"}})
                .to_string(),
        );

        let session = stripe
            .checkout()
            .create(
                CheckoutParams::subscription(
                    "price_pro",
                    "https://app.test/billing/done?session={CHECKOUT_SESSION_ID}",
                    "https://app.test/billing",
                )
                .customer("cus_1")
                .client_reference_id("user-7")
                .trial_days(14)
                .subscription_metadata("user_id", "7")
                .allow_promotion_codes(),
            )
            .await
            .unwrap();
        assert_eq!(session.url.as_deref(), Some("https://checkout.stripe.com/c/pay/cs_1"));
        assert_eq!(session.customer_id(), Some("cus_1"));

        let body = fake.recorded()[0].body();
        for pair in [
            "mode=subscription",
            "line_items%5B0%5D%5Bprice%5D=price_pro",
            "line_items%5B0%5D%5Bquantity%5D=1",
            "customer=cus_1",
            "client_reference_id=user-7",
            "subscription_data%5Btrial_period_days%5D=14",
            "subscription_data%5Bmetadata%5D%5Buser_id%5D=7",
            "allow_promotion_codes=true",
            "cancel_url=https%3A%2F%2Fapp.test%2Fbilling",
        ] {
            assert!(has(&body, pair), "missing {pair} in {body}");
        }
    }

    #[tokio::test]
    async fn a_payment_checkout_can_price_inline() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, json!({"id":"cs_2"}).to_string());
        stripe
            .checkout()
            .create(
                CheckoutParams::new(CheckoutMode::Payment, "https://a/ok", "https://a/no")
                    .inline_price("Donation", "USD", 500, 1)
                    .customer_email("ada@example.com"),
            )
            .await
            .unwrap();

        let body = fake.recorded()[0].body();
        assert!(has(&body, "mode=payment"), "{body}");
        assert!(has(&body, "line_items%5B0%5D%5Bprice_data%5D%5Bunit_amount%5D=500"), "{body}");
        assert!(
            has(&body, "line_items%5B0%5D%5Bprice_data%5D%5Bproduct_data%5D%5Bname%5D=Donation"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn the_portal_needs_a_customer() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(
            200,
            json!({"id":"bps_1","url":"https://billing.stripe.com/p/1"}).to_string(),
        );

        let portal = stripe.billing_portal().create("cus_1", "https://a/account").await.unwrap();
        assert_eq!(portal.url, "https://billing.stripe.com/p/1");
        assert!(has(&fake.recorded()[0].body(), "customer=cus_1"));

        assert!(stripe.billing_portal().create("", "https://a").await.is_err());
        assert_eq!(fake.count(), 1, "an empty customer is refused before a call");
    }

    fn subscription(quantity: u64) -> String {
        json!({
            "id": "sub_1",
            "customer": "cus_1",
            "status": "active",
            "items": {"data": [{"id": "si_1", "price": {"id": "price_basic"}, "quantity": quantity}]}
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_swap_finds_the_item_and_moves_its_price() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, subscription(3)).responding(200, subscription(3));

        stripe.subscriptions().swap("sub_1", "price_pro", Proration::AlwaysInvoice).await.unwrap();

        let sent = fake.recorded();
        assert_eq!(sent[0].method(), "GET");
        assert_eq!(sent[1].url(), "https://api.stripe.com/v1/subscriptions/sub_1");
        let body = sent[1].body();
        assert!(has(&body, "items%5B0%5D%5Bid%5D=si_1"), "{body}");
        assert!(has(&body, "items%5B0%5D%5Bprice%5D=price_pro"), "{body}");
        assert!(has(&body, "proration_behavior=always_invoice"), "{body}");
    }

    #[tokio::test]
    async fn quantity_cancel_and_resume_are_the_documented_calls() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, subscription(1));

        let subscriptions = stripe.subscriptions();
        subscriptions.update_quantity("sub_1", 5, Proration::None).await.unwrap();
        subscriptions.cancel_at_period_end("sub_1").await.unwrap();
        subscriptions.resume("sub_1").await.unwrap();
        subscriptions.cancel_now("sub_1").await.unwrap();

        let sent = fake.recorded();
        assert!(has(&sent[1].body(), "items%5B0%5D%5Bquantity%5D=5"));
        assert!(has(&sent[1].body(), "proration_behavior=none"));
        assert!(has(&sent[2].body(), "cancel_at_period_end=true"));
        assert!(has(&sent[3].body(), "cancel_at_period_end=false"));
        assert_eq!(sent[4].method(), "DELETE");
        assert_eq!(sent[4].url(), "https://api.stripe.com/v1/subscriptions/sub_1");
    }

    #[tokio::test]
    async fn a_customers_subscriptions_include_canceled_ones() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, json!({"data":[],"has_more":false}).to_string());

        stripe.subscriptions().for_customer("cus_1").await.unwrap();
        assert!(fake.recorded()[0].url_contains("status=all"));
    }

    #[tokio::test]
    async fn a_payment_intent_and_its_refund() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(
            200,
            json!({"id":"pi_1","amount":1999,"currency":"usd","status":"succeeded"}).to_string(),
        );

        let intent = stripe
            .payment_intents()
            .create(
                PaymentIntentParams::new(1999, "USD")
                    .customer("cus_1")
                    .confirm()
                    .off_session()
                    .idempotency_key("order-42"),
            )
            .await
            .unwrap();
        assert!(intent.succeeded());

        stripe
            .refunds()
            .create("pi_1", Some(500), Some(RefundReason::RequestedByCustomer), Params::new())
            .await
            .unwrap();

        let sent = fake.recorded();
        let body = sent[0].body();
        assert!(has(&body, "amount=1999"), "{body}");
        assert!(has(&body, "currency=usd"), "{body}");
        assert!(has(&body, "automatic_payment_methods%5Benabled%5D=true"), "{body}");
        assert!(has(&body, "off_session=true"), "{body}");
        assert_eq!(sent[0].header("idempotency-key"), Some("order-42"));

        let refund = sent[1].body();
        assert!(has(&refund, "payment_intent=pi_1"), "{refund}");
        assert!(has(&refund, "amount=500"), "{refund}");
        assert!(has(&refund, "reason=requested_by_customer"), "{refund}");
    }

    #[tokio::test]
    async fn payment_intent_actions_hit_their_endpoints() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, json!({"id":"pi_1"}).to_string());

        let intents = stripe.payment_intents();
        intents.retrieve("pi_1").await.unwrap();
        intents.confirm("pi_1", Params::new()).await.unwrap();
        intents.cancel("pi_1").await.unwrap();

        let urls: Vec<String> = fake.recorded().iter().map(|r| r.url().to_string()).collect();
        assert_eq!(
            urls,
            vec![
                "https://api.stripe.com/v1/payment_intents/pi_1",
                "https://api.stripe.com/v1/payment_intents/pi_1/confirm",
                "https://api.stripe.com/v1/payment_intents/pi_1/cancel",
            ]
        );
    }

    #[tokio::test]
    async fn invoices_and_the_catalogue() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(
            200,
            json!({"data":[{"id":"in_1","amount_due":500,"status":"open"}],"has_more":false})
                .to_string(),
        );

        let invoices = stripe.invoices().for_customer("cus_1", 10).await.unwrap();
        assert_eq!(invoices[0].amount_due, 500);
        assert!(fake.recorded()[0].url_contains("limit=10"));

        stripe.invoices().preview("cus_1", Params::new()).await.unwrap();
        assert_eq!(fake.recorded()[1].url(), "https://api.stripe.com/v1/invoices/create_preview");

        stripe.catalog().prices_by_lookup_key(&["pro_monthly"]).await.unwrap();
        assert!(fake.recorded()[2].url_contains("lookup_keys%5B0%5D=pro_monthly"));
    }
}
