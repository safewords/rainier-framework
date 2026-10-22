//! Checkout and the customer portal — `/v1/checkout/sessions` and
//! `/v1/billing_portal/sessions`.
//!
//! The two pages Stripe hosts so this application never touches a card
//! number: Checkout to start paying, the portal to change or stop. Both are
//! a session created server-side and a redirect to its `url`.

use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::segment;
use crate::client::Stripe;
use crate::form::Params;

/// What a Checkout session is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckoutMode {
    /// A one-off payment.
    Payment,
    /// Start a subscription.
    Subscription,
    /// Save a payment method for later, charging nothing now.
    Setup,
}

impl CheckoutMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Payment => "payment",
            Self::Subscription => "subscription",
            Self::Setup => "setup",
        }
    }
}

/// A Checkout session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckoutSession {
    /// `cs_…`.
    pub id: String,
    /// Where to send the customer. Absent once the session is complete.
    #[serde(default)]
    pub url: Option<String>,
    /// `payment`, `subscription` or `setup`.
    #[serde(default)]
    pub mode: Option<String>,
    /// `open`, `complete` or `expired`.
    #[serde(default)]
    pub status: Option<String>,
    /// `paid`, `unpaid` or `no_payment_required`.
    #[serde(default)]
    pub payment_status: Option<String>,
    /// The customer, as an id — or an object, when expanded.
    #[serde(default)]
    pub customer: Option<Value>,
    /// The subscription it started, in subscription mode.
    #[serde(default)]
    pub subscription: Option<Value>,
    /// The payment intent, in payment mode.
    #[serde(default)]
    pub payment_intent: Option<Value>,
    /// Your reference — typically your user or order id.
    #[serde(default)]
    pub client_reference_id: Option<String>,
    /// Your keys.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CheckoutSession {
    /// The customer's id, whether or not it was expanded.
    pub fn customer_id(&self) -> Option<&str> {
        super::id_of(self.customer.as_ref())
    }

    /// The subscription's id, whether or not it was expanded.
    pub fn subscription_id(&self) -> Option<&str> {
        super::id_of(self.subscription.as_ref())
    }
}

/// Parameters for a Checkout session.
#[derive(Debug, Clone)]
pub struct CheckoutParams {
    params: Params,
    line_items: Vec<Value>,
    subscription_data: Map<String, Value>,
}

impl CheckoutParams {
    /// A session in `mode` that returns to these URLs.
    ///
    /// `{CHECKOUT_SESSION_ID}` in the success URL is replaced by Stripe, which
    /// is how the page it lands on finds out which session finished — but do
    /// not grant anything from that page: grant from the
    /// `checkout.session.completed` webhook, which a customer closing the tab
    /// cannot skip.
    pub fn new(
        mode: CheckoutMode,
        success_url: impl Into<String>,
        cancel_url: impl Into<String>,
    ) -> Self {
        let params = Params::new()
            .set("mode", mode.as_str())
            .set("success_url", success_url.into())
            .set("cancel_url", cancel_url.into());
        Self { params, line_items: Vec::new(), subscription_data: Map::new() }
    }

    /// A subscription to `price`.
    pub fn subscription(
        price: impl Into<String>,
        success_url: impl Into<String>,
        cancel_url: impl Into<String>,
    ) -> Self {
        Self::new(CheckoutMode::Subscription, success_url, cancel_url).price(price, 1)
    }

    /// One line: `quantity` of an existing price.
    pub fn price(mut self, price: impl Into<String>, quantity: u64) -> Self {
        self.line_items.push(json!({ "price": price.into(), "quantity": quantity }));
        self
    }

    /// One line with an inline price — amount in the currency's smallest
    /// unit, for a payment whose amount is not in the catalogue.
    pub fn inline_price(
        mut self,
        name: impl Into<String>,
        currency: impl Into<String>,
        unit_amount: i64,
        quantity: u64,
    ) -> Self {
        self.line_items.push(json!({
            "price_data": {
                "currency": currency.into(),
                "unit_amount": unit_amount,
                "product_data": { "name": name.into() },
            },
            "quantity": quantity,
        }));
        self
    }

    /// Bill an existing customer.
    pub fn customer(mut self, customer: impl Into<String>) -> Self {
        self.params.insert("customer", customer.into());
        self
    }

    /// Prefill the email, for somebody with no customer yet.
    pub fn customer_email(mut self, email: impl Into<String>) -> Self {
        self.params.insert("customer_email", email.into());
        self
    }

    /// Your reference — it comes back on the session and its webhook.
    pub fn client_reference_id(mut self, reference: impl Into<String>) -> Self {
        self.params.insert("client_reference_id", reference.into());
        self
    }

    /// A free trial before the first charge.
    pub fn trial_days(mut self, days: u32) -> Self {
        self.subscription_data.insert("trial_period_days".into(), days.into());
        self
    }

    /// Metadata copied onto the subscription Checkout creates — where it is
    /// useful later, unlike the session's own, which nobody reads twice.
    pub fn subscription_metadata(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let entry = self.subscription_data.entry("metadata").or_insert_with(|| json!({}));
        if let Value::Object(map) = entry {
            map.insert(key.into(), Value::String(value.into()));
        }
        self
    }

    /// Let the customer type a promotion code.
    pub fn allow_promotion_codes(mut self) -> Self {
        self.params.insert("allow_promotion_codes", true);
        self
    }

    /// Apply a coupon or promotion code up front.
    pub fn discount_coupon(mut self, coupon: impl Into<String>) -> Self {
        self.params.insert("discounts", json!([{ "coupon": coupon.into() }]));
        self
    }

    /// Collect tax automatically, through Stripe Tax.
    pub fn automatic_tax(mut self) -> Self {
        self.params.insert("automatic_tax", json!({ "enabled": true }));
        self
    }

    /// One metadata entry on the session.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params = self.params.metadata(key, value);
        self
    }

    /// Any other parameter.
    pub fn set(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.params.insert(key, value);
        self
    }

    /// Use this idempotency key.
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.params = self.params.idempotency_key(key);
        self
    }

    /// Whether a customer or an email has been given.
    pub(crate) fn names_customer(&self) -> bool {
        self.params.get("customer").is_some() || self.params.get("customer_email").is_some()
    }

    /// The parameters.
    pub fn into_params(self) -> Params {
        let mut params = self.params;
        if !self.line_items.is_empty() {
            params.insert("line_items", self.line_items);
        }
        if !self.subscription_data.is_empty() {
            params.insert("subscription_data", self.subscription_data);
        }
        params
    }
}

/// The Checkout endpoints. From [`Stripe::checkout`].
#[derive(Debug, Clone, Copy)]
pub struct Checkout<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl Checkout<'_> {
    /// Create a session; redirect the customer to its `url`.
    pub async fn create(&self, params: CheckoutParams) -> Result<CheckoutSession> {
        self.stripe.call("POST", "/v1/checkout/sessions", params.into_params()).await
    }

    /// Retrieve one — with `expand` for its line items or subscription.
    pub async fn retrieve(&self, id: &str, params: Params) -> Result<CheckoutSession> {
        self.stripe.call("GET", &format!("/v1/checkout/sessions/{}", segment(id)), params).await
    }

    /// Expire an open session, so its URL stops working.
    pub async fn expire(&self, id: &str) -> Result<CheckoutSession> {
        self.stripe
            .call("POST", &format!("/v1/checkout/sessions/{}/expire", segment(id)), Params::new())
            .await
    }
}

/// A customer-portal session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PortalSession {
    /// `bps_…`.
    pub id: String,
    /// Where to send the customer. Short-lived: create one per click.
    pub url: String,
    /// Where the portal's "back" link goes.
    #[serde(default)]
    pub return_url: Option<String>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The customer-portal endpoint. From [`Stripe::billing_portal`].
#[derive(Debug, Clone, Copy)]
pub struct BillingPortal<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl BillingPortal<'_> {
    /// A portal session for `customer` that returns to `return_url`.
    pub async fn create(&self, customer: &str, return_url: &str) -> Result<PortalSession> {
        self.create_with(customer, return_url, Params::new()).await
    }

    /// The same, with more parameters — a `configuration`, a `flow_data`
    /// deep link straight to "cancel" or "update payment method".
    pub async fn create_with(
        &self,
        customer: &str,
        return_url: &str,
        params: Params,
    ) -> Result<PortalSession> {
        if customer.is_empty() {
            return Err(Error::bad_request("a billing portal session needs a customer"));
        }
        let params =
            Params::new().set("customer", customer).set("return_url", return_url).merge(params);
        self.stripe.call("POST", "/v1/billing_portal/sessions", params).await
    }
}
