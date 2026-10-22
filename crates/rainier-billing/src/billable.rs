//! The model that pays — [`Billable`] and [`BillableExt`].
//!
//! Implemented on whatever owns a subscription: a user, a team, an
//! organisation. The trait asks for the three facts Stripe needs; the
//! extension methods are the calls every "billing" page makes, so they are
//! written once.
//!
//! ```ignore
//! impl Billable for User {
//!     fn stripe_customer_id(&self) -> Option<String> { self.stripe_id.clone() }
//!     fn billing_email(&self) -> Option<String> { Some(self.email.clone()) }
//!     fn billing_reference(&self) -> String { self.id.to_string() }
//! }
//!
//! let customer = user.create_or_get_customer(&stripe).await?;
//! if user.stripe_id.is_none() {
//!     users.set_stripe_id(user.id, &customer.id).await?;   // yours to persist
//! }
//! let session = user.checkout(&stripe, CheckoutParams::subscription(price, ok, back)).await?;
//! Ok(Redirect::see_other(session.url.unwrap_or_default()))
//! ```
//!
//! # Why this does not save the customer id
//!
//! The crate does not know your table, and an extension method that wrote
//! to it would need a repository, a column name and a transaction policy it
//! has no business choosing. So [`create_or_get_customer`](BillableExt::create_or_get_customer)
//! returns the customer and you store `customer.id`. The application's
//! reference rides along in the customer's metadata as `rainier_reference`,
//! so a webhook about a customer whose id was never saved can still be
//! traced back.

use rainier_support::{Error, Result};

use crate::client::Stripe;
use crate::form::Params;
use crate::resources::checkout::{CheckoutParams, CheckoutSession};
use crate::resources::customers::{Customer, CustomerParams};

/// A model that can be billed.
pub trait Billable: Send + Sync {
    /// The Stripe customer id, once there is one.
    fn stripe_customer_id(&self) -> Option<String>;

    /// The email Stripe sends receipts to.
    fn billing_email(&self) -> Option<String> {
        None
    }

    /// The name on invoices.
    fn billing_name(&self) -> Option<String> {
        None
    }

    /// The application's own identifier — stored in the customer's metadata
    /// and used as Checkout's `client_reference_id`.
    fn billing_reference(&self) -> String;
}

/// The calls a billing page makes. Every [`Billable`] has them.
#[async_trait::async_trait]
pub trait BillableExt: Billable {
    /// The customer, retrieving it when there is an id and creating it when
    /// there is not. **Persist the returned `id`** if you did not have one.
    ///
    /// Creation is keyed on the reference, so a double-click does not create
    /// two customers within Stripe's 24-hour idempotency window.
    async fn create_or_get_customer(&self, stripe: &Stripe) -> Result<Customer> {
        if let Some(id) = self.stripe_customer_id() {
            return stripe.customers().retrieve(&id).await;
        }

        let reference = self.billing_reference();
        let mut params = CustomerParams::new()
            .metadata("rainier_reference", reference.clone())
            .idempotency_key(format!("rainier-customer-{reference}"));
        if let Some(email) = self.billing_email() {
            params = params.email(email);
        }
        if let Some(name) = self.billing_name() {
            params = params.name(name);
        }
        stripe.customers().create(params).await
    }

    /// A Checkout session for this model: its customer when it has one, its
    /// email when it does not, and its reference either way.
    async fn checkout(&self, stripe: &Stripe, params: CheckoutParams) -> Result<CheckoutSession> {
        let mut params = params.client_reference_id(self.billing_reference());
        if !params.names_customer() {
            if let Some(customer) = self.stripe_customer_id() {
                params = params.customer(customer);
            } else if let Some(email) = self.billing_email() {
                params = params.customer_email(email);
            }
        }
        stripe.checkout().create(params).await
    }

    /// A customer-portal URL that returns to `return_url`. Create one per
    /// click — they expire.
    async fn billing_portal_url(&self, stripe: &Stripe, return_url: &str) -> Result<String> {
        let customer = self.stripe_customer_id().ok_or_else(|| {
            Error::bad_request("there is no billing account to manage yet — check out first")
        })?;
        Ok(stripe.billing_portal().create_with(&customer, return_url, Params::new()).await?.url)
    }
}

impl<B: Billable + ?Sized> BillableExt for B {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct User {
        id: u64,
        stripe_id: Option<String>,
    }

    impl Billable for User {
        fn stripe_customer_id(&self) -> Option<String> {
            self.stripe_id.clone()
        }
        fn billing_email(&self) -> Option<String> {
            Some("ada@example.com".into())
        }
        fn billing_name(&self) -> Option<String> {
            Some("Ada".into())
        }
        fn billing_reference(&self) -> String {
            self.id.to_string()
        }
    }

    #[tokio::test]
    async fn a_new_customer_is_created_once_with_the_reference() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, json!({"id":"cus_new"}).to_string());

        let user = User { id: 7, stripe_id: None };
        let customer = user.create_or_get_customer(&stripe).await.unwrap();
        assert_eq!(customer.id, "cus_new");

        let sent = &fake.recorded()[0];
        assert_eq!(sent.method(), "POST");
        assert_eq!(sent.header("idempotency-key"), Some("rainier-customer-7"));
        assert!(sent.body().contains("metadata%5Brainier_reference%5D=7"), "{}", sent.body());
        assert!(sent.body().contains("email=ada%40example.com"), "{}", sent.body());
    }

    #[tokio::test]
    async fn an_existing_customer_is_retrieved() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, json!({"id":"cus_1"}).to_string());

        let user = User { id: 7, stripe_id: Some("cus_1".into()) };
        user.create_or_get_customer(&stripe).await.unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(sent.method(), "GET");
        assert_eq!(sent.url(), "https://api.stripe.com/v1/customers/cus_1");
    }

    #[tokio::test]
    async fn checkout_names_the_customer_or_their_email_and_the_reference() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, json!({"id":"cs_1"}).to_string());

        let known = User { id: 7, stripe_id: Some("cus_1".into()) };
        known
            .checkout(
                &stripe,
                CheckoutParams::subscription("price", "https://a/ok", "https://a/no"),
            )
            .await
            .unwrap();
        let unknown = User { id: 8, stripe_id: None };
        unknown
            .checkout(
                &stripe,
                CheckoutParams::subscription("price", "https://a/ok", "https://a/no"),
            )
            .await
            .unwrap();

        let sent = fake.recorded();
        assert!(sent[0].body().contains("customer=cus_1"), "{}", sent[0].body());
        assert!(sent[0].body().contains("client_reference_id=7"), "{}", sent[0].body());
        assert!(sent[1].body().contains("customer_email=ada%40example.com"), "{}", sent[1].body());
        assert!(!sent[1].body().contains("customer="), "{}", sent[1].body());
    }

    #[tokio::test]
    async fn the_portal_needs_a_billing_account() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(
            200,
            json!({"id":"bps_1","url":"https://billing.stripe.com/p/x"}).to_string(),
        );

        let user = User { id: 7, stripe_id: Some("cus_1".into()) };
        assert_eq!(
            user.billing_portal_url(&stripe, "https://a/account").await.unwrap(),
            "https://billing.stripe.com/p/x"
        );

        let nobody = User { id: 8, stripe_id: None };
        assert!(nobody.billing_portal_url(&stripe, "https://a").await.is_err());
        assert_eq!(fake.count(), 1);
    }
}
