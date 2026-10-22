//! Invoices and the catalogue — `/v1/invoices`, `/v1/prices`, `/v1/products`.

use rainier_support::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::segment;
use crate::client::{List, Stripe};
use crate::form::Params;

/// An invoice.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Invoice {
    /// `in_…`. Absent on a preview, which is not a real invoice yet.
    #[serde(default)]
    pub id: Option<String>,
    /// The customer, as an id or an object.
    #[serde(default)]
    pub customer: Option<Value>,
    /// `draft`, `open`, `paid`, `uncollectible`, `void`.
    #[serde(default)]
    pub status: Option<String>,
    /// What is owed, in the smallest currency unit.
    #[serde(default)]
    pub amount_due: i64,
    /// What has been paid.
    #[serde(default)]
    pub amount_paid: i64,
    /// The total.
    #[serde(default)]
    pub total: i64,
    /// Lowercase ISO code.
    #[serde(default)]
    pub currency: Option<String>,
    /// The page a customer pays or views it on.
    #[serde(default)]
    pub hosted_invoice_url: Option<String>,
    /// The PDF.
    #[serde(default)]
    pub invoice_pdf: Option<String>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The invoice endpoints. From [`Stripe::invoices`].
#[derive(Debug, Clone, Copy)]
pub struct Invoices<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl Invoices<'_> {
    /// A customer's invoices, newest first, up to `max`.
    pub async fn for_customer(&self, customer: &str, max: usize) -> Result<Vec<Invoice>> {
        self.stripe
            .list_all(
                "/v1/invoices",
                Params::new().set("customer", customer).set("limit", max.clamp(1, 100)),
                max,
            )
            .await
    }

    /// One page, with any filters.
    pub async fn list(&self, params: Params) -> Result<List<Invoice>> {
        self.stripe.call("GET", "/v1/invoices", params).await
    }

    /// Retrieve one.
    pub async fn retrieve(&self, id: &str) -> Result<Invoice> {
        self.stripe.call("GET", &format!("/v1/invoices/{}", segment(id)), Params::new()).await
    }

    /// What the customer's next invoice will be — `POST
    /// /v1/invoices/create_preview`, which replaced the retired "upcoming
    /// invoice" endpoint. Add `subscription_details` to `params` to preview
    /// a plan change before making it.
    pub async fn preview(&self, customer: &str, params: Params) -> Result<Invoice> {
        let params = Params::new().set("customer", customer).merge(params);
        self.stripe.call("POST", "/v1/invoices/create_preview", params).await
    }

    /// Pay an open invoice now, with the default payment method.
    pub async fn pay(&self, id: &str) -> Result<Invoice> {
        self.stripe.call("POST", &format!("/v1/invoices/{}/pay", segment(id)), Params::new()).await
    }

    /// Void an open invoice.
    pub async fn void(&self, id: &str) -> Result<Invoice> {
        self.stripe.call("POST", &format!("/v1/invoices/{}/void", segment(id)), Params::new()).await
    }
}

/// A price — an amount and an interval for a product.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// `price_…`.
    pub id: String,
    /// The product, as an id or an object.
    #[serde(default)]
    pub product: Option<Value>,
    /// Whether it can be bought.
    #[serde(default)]
    pub active: bool,
    /// Lowercase ISO code.
    #[serde(default)]
    pub currency: Option<String>,
    /// In the smallest currency unit; absent for tiered prices.
    #[serde(default)]
    pub unit_amount: Option<i64>,
    /// A stable handle to look a price up by, rather than an id per
    /// environment.
    #[serde(default)]
    pub lookup_key: Option<String>,
    /// `{ interval, interval_count }` for a recurring price.
    #[serde(default)]
    pub recurring: Option<Value>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A product.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Product {
    /// `prod_…`.
    pub id: String,
    /// What the customer sees.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether it can be bought.
    #[serde(default)]
    pub active: bool,
    /// Its default price.
    #[serde(default)]
    pub default_price: Option<Value>,
    /// Everything else Stripe sent.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The catalogue endpoints. From [`Stripe::catalog`].
#[derive(Debug, Clone, Copy)]
pub struct Catalog<'a> {
    pub(crate) stripe: &'a Stripe,
}

impl Catalog<'_> {
    /// Every active price.
    pub async fn prices(&self) -> Result<Vec<Price>> {
        self.stripe
            .list_all(
                "/v1/prices",
                Params::new().set("active", true).set("limit", 100).expand("data.product"),
                10_000,
            )
            .await
    }

    /// Prices by lookup key — `pro_monthly` in every environment, rather
    /// than a `price_…` id that differs between test and live.
    pub async fn prices_by_lookup_key(&self, keys: &[&str]) -> Result<Vec<Price>> {
        let list: List<Price> = self
            .stripe
            .call("GET", "/v1/prices", Params::new().set("lookup_keys", keys).set("limit", 100))
            .await?;
        Ok(list.data)
    }

    /// Every active product.
    pub async fn products(&self) -> Result<Vec<Product>> {
        self.stripe
            .list_all("/v1/products", Params::new().set("active", true).set("limit", 100), 10_000)
            .await
    }
}
