//! Customers — `/v1/customers`.

use rainier_support::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::segment;
use crate::client::{List, Stripe};
use crate::form::Params;

/// A Stripe customer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Customer {
    /// `cus_…`.
    pub id: String,
    /// The email on file.
    #[serde(default)]
    pub email: Option<String>,
    /// The name on file.
    #[serde(default)]
    pub name: Option<String>,
    /// Your keys, Stripe's storage.
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// Whether this object was deleted — what a `DELETE` answers, and what a
    /// retrieve of a deleted customer answers too.
    #[serde(default)]
    pub deleted: bool,
    /// Live or test mode.
    #[serde(default)]
    pub livemode: bool,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Parameters for creating or updating a customer.
#[derive(Debug, Clone, Default)]
pub struct CustomerParams {
    params: Params,
}

impl CustomerParams {
    /// Nothing set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The email.
    pub fn email(mut self, email: impl Into<String>) -> Self {
        self.params.insert("email", email.into());
        self
    }

    /// The name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.params.insert("name", name.into());
        self
    }

    /// A phone number.
    pub fn phone(mut self, phone: impl Into<String>) -> Self {
        self.params.insert("phone", phone.into());
        self
    }

    /// A description, for the dashboard.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.params.insert("description", description.into());
        self
    }

    /// The payment method invoices are charged to.
    pub fn default_payment_method(mut self, payment_method: impl Into<String>) -> Self {
        self.params.insert(
            "invoice_settings",
            serde_json::json!({ "default_payment_method": payment_method.into() }),
        );
        self
    }

    /// One metadata entry. The application's own user id belongs here — it
    /// is how a webhook about this customer finds its way back to a row.
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

    /// The parameters.
    pub fn into_params(self) -> Params {
        self.params
    }
}

/// The customer endpoints. From [`Stripe::customers`].
#[derive(Debug, Clone, Copy)]
pub struct Customers<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl Customers<'_> {
    /// Create a customer.
    pub async fn create(&self, params: CustomerParams) -> Result<Customer> {
        self.stripe.call("POST", "/v1/customers", params.into_params()).await
    }

    /// Retrieve one.
    pub async fn retrieve(&self, id: &str) -> Result<Customer> {
        self.stripe.call("GET", &format!("/v1/customers/{}", segment(id)), Params::new()).await
    }

    /// Update one.
    pub async fn update(&self, id: &str, params: CustomerParams) -> Result<Customer> {
        self.stripe
            .call("POST", &format!("/v1/customers/{}", segment(id)), params.into_params())
            .await
    }

    /// Delete one. Stripe cancels its subscriptions immediately.
    pub async fn delete(&self, id: &str) -> Result<Customer> {
        self.stripe.call("DELETE", &format!("/v1/customers/{}", segment(id)), Params::new()).await
    }

    /// One page of customers — `email` narrows it, which is the usual reason
    /// to list them.
    pub async fn list(&self, params: Params) -> Result<List<Customer>> {
        self.stripe.call("GET", "/v1/customers", params).await
    }
}
