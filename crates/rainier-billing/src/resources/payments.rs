//! One-off money — payment intents, setup intents and refunds.
//!
//! Amounts are integers in the currency's **smallest unit** (cents, yen),
//! because that is what Stripe takes and because a float is not a sum of
//! money.

use rainier_support::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::segment;
use crate::client::Stripe;
use crate::form::Params;

/// A payment intent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaymentIntent {
    /// `pi_…`.
    pub id: String,
    /// In the smallest currency unit.
    #[serde(default)]
    pub amount: i64,
    /// Lowercase ISO code.
    #[serde(default)]
    pub currency: Option<String>,
    /// `requires_payment_method`, `requires_action`, `succeeded`…
    #[serde(default)]
    pub status: Option<String>,
    /// What a browser needs to confirm it with Stripe.js. Not a secret from
    /// the customer; a secret from everyone else.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// The customer, as an id or an object.
    #[serde(default)]
    pub customer: Option<Value>,
    /// Your keys.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl PaymentIntent {
    /// Whether the money moved.
    pub fn succeeded(&self) -> bool {
        self.status.as_deref() == Some("succeeded")
    }

    /// Whether the customer has to do something — 3-D Secure, usually.
    pub fn requires_action(&self) -> bool {
        self.status.as_deref() == Some("requires_action")
    }
}

/// Parameters for a payment intent.
#[derive(Debug, Clone)]
pub struct PaymentIntentParams {
    params: Params,
}

impl PaymentIntentParams {
    /// `amount` of `currency`, with automatic payment methods on — Stripe
    /// offers whatever the dashboard enables, rather than a list in code.
    pub fn new(amount: i64, currency: impl Into<String>) -> Self {
        Self {
            params: Params::new()
                .set("amount", amount)
                .set("currency", currency.into().to_ascii_lowercase())
                .set("automatic_payment_methods", json!({ "enabled": true })),
        }
    }

    /// Charge this customer.
    pub fn customer(mut self, customer: impl Into<String>) -> Self {
        self.params.insert("customer", customer.into());
        self
    }

    /// With this payment method.
    pub fn payment_method(mut self, payment_method: impl Into<String>) -> Self {
        self.params.insert("payment_method", payment_method.into());
        self
    }

    /// Confirm on creation — a server-side charge of a saved card.
    pub fn confirm(mut self) -> Self {
        self.params.insert("confirm", true);
        self
    }

    /// The customer is not present — a saved card charged in the background.
    pub fn off_session(mut self) -> Self {
        self.params.insert("off_session", true);
        self
    }

    /// Keep the payment method for future off-session use.
    pub fn setup_future_usage(mut self, usage: impl Into<String>) -> Self {
        self.params.insert("setup_future_usage", usage.into());
        self
    }

    /// Authorise now, capture later.
    pub fn capture_manually(mut self) -> Self {
        self.params.insert("capture_method", "manual");
        self
    }

    /// What appears in the dashboard.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.params.insert("description", description.into());
        self
    }

    /// Where a receipt goes.
    pub fn receipt_email(mut self, email: impl Into<String>) -> Self {
        self.params.insert("receipt_email", email.into());
        self
    }

    /// One metadata entry.
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params = self.params.metadata(key, value);
        self
    }

    /// Any other parameter.
    pub fn set(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.params.insert(key, value);
        self
    }

    /// Use this idempotency key — strongly advised for a charge made from a
    /// queued job; see [`Params::idempotency_key`].
    pub fn idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.params = self.params.idempotency_key(key);
        self
    }

    /// The parameters.
    pub fn into_params(self) -> Params {
        self.params
    }
}

/// The payment-intent endpoints. From [`Stripe::payment_intents`].
#[derive(Debug, Clone, Copy)]
pub struct PaymentIntents<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl PaymentIntents<'_> {
    /// Create one.
    pub async fn create(&self, params: PaymentIntentParams) -> Result<PaymentIntent> {
        self.stripe.call("POST", "/v1/payment_intents", params.into_params()).await
    }

    /// Retrieve one.
    pub async fn retrieve(&self, id: &str) -> Result<PaymentIntent> {
        self.stripe.call("GET", &path(id, ""), Params::new()).await
    }

    /// Update one before it is confirmed.
    pub async fn update(&self, id: &str, params: Params) -> Result<PaymentIntent> {
        self.stripe.call("POST", &path(id, ""), params).await
    }

    /// Confirm one.
    pub async fn confirm(&self, id: &str, params: Params) -> Result<PaymentIntent> {
        self.stripe.call("POST", &path(id, "/confirm"), params).await
    }

    /// Capture an authorised one — all of it, or `amount_to_capture` in
    /// `params`.
    pub async fn capture(&self, id: &str, params: Params) -> Result<PaymentIntent> {
        self.stripe.call("POST", &path(id, "/capture"), params).await
    }

    /// Cancel one that has not succeeded.
    pub async fn cancel(&self, id: &str) -> Result<PaymentIntent> {
        self.stripe.call("POST", &path(id, "/cancel"), Params::new()).await
    }
}

fn path(id: &str, action: &str) -> String {
    format!("/v1/payment_intents/{}{action}", segment(id))
}

/// A setup intent — saving a payment method without charging it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SetupIntent {
    /// `seti_…`.
    pub id: String,
    /// `requires_payment_method`, `succeeded`…
    #[serde(default)]
    pub status: Option<String>,
    /// What Stripe.js needs.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// The payment method it saved, once it has.
    #[serde(default)]
    pub payment_method: Option<Value>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The setup-intent endpoints. From [`Stripe::setup_intents`].
#[derive(Debug, Clone, Copy)]
pub struct SetupIntents<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl SetupIntents<'_> {
    /// Start saving a payment method for `customer`, for off-session use.
    pub async fn create(&self, customer: &str, params: Params) -> Result<SetupIntent> {
        let params = Params::new()
            .set("customer", customer)
            .set("usage", "off_session")
            .set("automatic_payment_methods", json!({ "enabled": true }))
            .merge(params);
        self.stripe.call("POST", "/v1/setup_intents", params).await
    }

    /// Retrieve one.
    pub async fn retrieve(&self, id: &str) -> Result<SetupIntent> {
        self.stripe.call("GET", &format!("/v1/setup_intents/{}", segment(id)), Params::new()).await
    }

    /// Cancel one.
    pub async fn cancel(&self, id: &str) -> Result<SetupIntent> {
        self.stripe
            .call("POST", &format!("/v1/setup_intents/{}/cancel", segment(id)), Params::new())
            .await
    }
}

/// A refund.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Refund {
    /// `re_…`.
    pub id: String,
    /// In the smallest currency unit.
    #[serde(default)]
    pub amount: i64,
    /// `pending`, `succeeded`, `failed`, `canceled`.
    #[serde(default)]
    pub status: Option<String>,
    /// The payment intent it refunds.
    #[serde(default)]
    pub payment_intent: Option<Value>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Why a refund was made — Stripe uses it for fraud signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefundReason {
    /// The customer asked.
    RequestedByCustomer,
    /// A charge made twice.
    Duplicate,
    /// A charge the cardholder did not make.
    Fraudulent,
}

impl RefundReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::RequestedByCustomer => "requested_by_customer",
            Self::Duplicate => "duplicate",
            Self::Fraudulent => "fraudulent",
        }
    }
}

/// The refund endpoints. From [`Stripe::refunds`].
#[derive(Debug, Clone, Copy)]
pub struct Refunds<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl Refunds<'_> {
    /// Refund a payment intent — all of it, or `amount`.
    ///
    /// Pass an idempotency key in `params` derived from your own refund
    /// record. Two clicks on "refund" are two refunds otherwise.
    pub async fn create(
        &self,
        payment_intent: &str,
        amount: Option<i64>,
        reason: Option<RefundReason>,
        params: Params,
    ) -> Result<Refund> {
        let params = Params::new()
            .set("payment_intent", payment_intent)
            .set_opt("amount", amount)
            .set_opt("reason", reason.map(RefundReason::as_str))
            .merge(params);
        self.stripe.call("POST", "/v1/refunds", params).await
    }

    /// Retrieve one.
    pub async fn retrieve(&self, id: &str) -> Result<Refund> {
        self.stripe.call("GET", &format!("/v1/refunds/{}", segment(id)), Params::new()).await
    }
}
