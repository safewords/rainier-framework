//! Subscriptions — `/v1/subscriptions`.
//!
//! The operations a "plan" page needs, named for what the customer did
//! rather than for which field changes: swap, change quantity, cancel now,
//! cancel at the end of the period, resume.

use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::segment;
use crate::client::{List, Stripe};
use crate::form::Params;
use crate::subscription::SubscriptionSnapshot;

/// A Stripe subscription.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    /// `sub_…`.
    pub id: String,
    /// The customer, as an id — or an object, when expanded.
    #[serde(default)]
    pub customer: Option<Value>,
    /// `active`, `trialing`, `past_due`…
    #[serde(default)]
    pub status: Option<String>,
    /// Whether it ends when the current period does.
    #[serde(default)]
    pub cancel_at_period_end: bool,
    /// The items — one per price.
    #[serde(default)]
    pub items: List<SubscriptionItem>,
    /// Your keys.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Everything else Stripe sent — `trial_end`, `ended_at`, `cancel_at`…
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Subscription {
    /// The customer's id, whether or not it was expanded.
    pub fn customer_id(&self) -> Option<&str> {
        super::id_of(self.customer.as_ref())
    }

    /// The state the application should store — see [`SubscriptionSnapshot`].
    pub fn snapshot(&self) -> SubscriptionSnapshot {
        let value = serde_json::to_value(self).unwrap_or(Value::Null);
        SubscriptionSnapshot::from_object(&value)
    }

    fn first_item(&self) -> Result<&SubscriptionItem> {
        self.items.data.first().ok_or_else(|| {
            Error::bad_request(format!("subscription {} has no items to change", self.id))
        })
    }
}

/// One price on a subscription.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionItem {
    /// `si_…`.
    pub id: String,
    /// The price, as an object.
    #[serde(default)]
    pub price: Option<Value>,
    /// How many.
    #[serde(default)]
    pub quantity: Option<u64>,
    /// Everything else Stripe sent — on current API versions,
    /// `current_period_start` and `current_period_end` live here.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SubscriptionItem {
    /// The price's id.
    pub fn price_id(&self) -> Option<&str> {
        super::id_of(self.price.as_ref())
    }
}

/// How a change is prorated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Proration {
    /// Credit and charge the difference on the next invoice. Stripe's default.
    #[default]
    CreateProrations,
    /// Invoice the difference immediately.
    AlwaysInvoice,
    /// Do not prorate.
    None,
}

impl Proration {
    fn as_str(self) -> &'static str {
        match self {
            Self::CreateProrations => "create_prorations",
            Self::AlwaysInvoice => "always_invoice",
            Self::None => "none",
        }
    }
}

/// The subscription endpoints. From [`Stripe::subscriptions`].
#[derive(Debug, Clone, Copy)]
pub struct Subscriptions<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl Subscriptions<'_> {
    /// Create one directly — for a customer who already has a default payment
    /// method. Everybody else should go through Checkout.
    pub async fn create(
        &self,
        customer: &str,
        price: &str,
        params: Params,
    ) -> Result<Subscription> {
        let params = Params::new()
            .set("customer", customer)
            .set("items", json!([{ "price": price }]))
            .merge(params);
        self.stripe.call("POST", "/v1/subscriptions", params).await
    }

    /// Retrieve one.
    pub async fn retrieve(&self, id: &str) -> Result<Subscription> {
        self.stripe.call("GET", &path(id), Params::new()).await
    }

    /// A customer's subscriptions — every status, including canceled, which
    /// Stripe's list leaves out unless asked.
    pub async fn for_customer(&self, customer: &str) -> Result<Vec<Subscription>> {
        self.stripe
            .list_all(
                "/v1/subscriptions",
                Params::new().set("customer", customer).set("status", "all").set("limit", 100),
                1_000,
            )
            .await
    }

    /// Update with arbitrary parameters.
    pub async fn update(&self, id: &str, params: Params) -> Result<Subscription> {
        self.stripe.call("POST", &path(id), params).await
    }

    /// Move a single-price subscription to `price`, keeping its quantity.
    pub async fn swap(&self, id: &str, price: &str, proration: Proration) -> Result<Subscription> {
        let current = self.retrieve(id).await?;
        let item = current.first_item()?;
        let params = Params::new()
            .set("items", json!([{ "id": item.id, "price": price }]))
            .set("proration_behavior", proration.as_str())
            // Leaving a swap that cannot be paid in place would leave the
            // customer on the new plan without paying for it.
            .set("payment_behavior", "pending_if_incomplete");
        self.update(id, params).await
    }

    /// Change a single-price subscription's quantity — seats.
    pub async fn update_quantity(
        &self,
        id: &str,
        quantity: u64,
        proration: Proration,
    ) -> Result<Subscription> {
        let current = self.retrieve(id).await?;
        let item = current.first_item()?;
        let params = Params::new()
            .set("items", json!([{ "id": item.id, "quantity": quantity }]))
            .set("proration_behavior", proration.as_str());
        self.update(id, params).await
    }

    /// End it at the end of the period already paid for — the grace period.
    pub async fn cancel_at_period_end(&self, id: &str) -> Result<Subscription> {
        self.update(id, Params::new().set("cancel_at_period_end", true)).await
    }

    /// Undo [`cancel_at_period_end`](Self::cancel_at_period_end), while the
    /// grace period lasts. After it, the subscription is gone and a new one
    /// is the only way back.
    pub async fn resume(&self, id: &str) -> Result<Subscription> {
        self.update(id, Params::new().set("cancel_at_period_end", false)).await
    }

    /// End it now.
    pub async fn cancel_now(&self, id: &str) -> Result<Subscription> {
        self.cancel_now_with(id, Params::new()).await
    }

    /// End it now, with `invoice_now` / `prorate` and the like.
    pub async fn cancel_now_with(&self, id: &str, params: Params) -> Result<Subscription> {
        self.stripe.call("DELETE", &path(id), params).await
    }

    /// Pause collection, leaving the subscription in place.
    pub async fn pause_collection(&self, id: &str, behavior: &str) -> Result<Subscription> {
        self.update(id, Params::new().set("pause_collection", json!({ "behavior": behavior })))
            .await
    }

    /// Resume a **paused** subscription — `POST …/resume`, which is a
    /// different thing from [`resume`](Self::resume).
    pub async fn resume_paused(&self, id: &str) -> Result<Subscription> {
        self.stripe.call("POST", &format!("{}/resume", path(id)), Params::new()).await
    }
}

fn path(id: &str) -> String {
    format!("/v1/subscriptions/{}", segment(id))
}
