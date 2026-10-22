//! # rainier-billing
//!
//! Billing through Stripe: a client,
//! verified webhooks, and a subscription state an application can gate
//! features on without asking Stripe on every request.
//!
//! | | |
//! |---|---|
//! | [`Stripe`] | the API client — pinned version, idempotency keys, typed errors |
//! | [`Params`] | Stripe's bracketed form encoding, and the request options |
//! | [`resources`] | customers, Checkout, the portal, subscriptions, payment and setup intents, refunds, invoices, prices and products |
//! | [`WebhookVerifier`] / [`Webhooks`] | signature checks, replay protection, dedupe, dispatch onto the event bus |
//! | [`SubscriptionSnapshot`] | what to store, and `valid` / `on_trial` / `on_grace_period` / … |
//! | [`Billable`] / [`BillableExt`] | the model that pays, and the calls its billing page makes |
//!
//! ```
//! use rainier_billing::{resources::checkout::CheckoutParams, Stripe};
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let (stripe, fake) = Stripe::fake();
//! fake.responding(200, r#"{"id":"cs_1","url":"https://checkout.stripe.com/c/pay/cs_1"}"#);
//!
//! let session = stripe
//!     .checkout()
//!     .create(
//!         CheckoutParams::subscription("price_pro", "https://app.test/done", "https://app.test/plans")
//!             .customer("cus_1")
//!             .trial_days(14),
//!     )
//!     .await?;
//!
//! assert_eq!(session.url.as_deref(), Some("https://checkout.stripe.com/c/pay/cs_1"));
//! fake.assert_sent(|request| request.body().contains("subscription_data%5Btrial_period_days%5D=14"));
//! # Ok(()) }
//! ```
//!
//! ## No SDK
//!
//! Stripe's API is a form-encoded POST and a JSON answer. A generated SDK
//! for it is hundreds of thousands of lines, pins its own HTTP stack and
//! TLS provider, and moves its types with every API version. This crate
//! rides the framework's own outbound [transport port](rainier_http_client::Transport),
//! so it shares the process's TLS setup, records into the same fake in
//! tests, and costs nothing to compile beyond what the framework already
//! does. The typed surface covers what an application built on
//! subscriptions and one-off payments uses; [`Stripe::get`],
//! [`Stripe::post`] and [`Stripe::call`] reach every other endpoint with
//! the same headers, keys and error handling.
//!
//! ## Money
//!
//! Amounts are `i64`s in the currency's smallest unit, as Stripe takes them.
//! Never a float.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod billable;
pub mod client;
pub mod form;
pub mod resources;
pub mod subscription;
pub mod webhook;

pub use billable::{Billable, BillableExt};
pub use client::{List, Stripe, StripeError, DEFAULT_API_VERSION, DEFAULT_BASE_URL};
pub use form::Params;
pub use resources::checkout::{
    BillingPortal, Checkout, CheckoutMode, CheckoutParams, CheckoutSession, PortalSession,
};
pub use resources::customers::{Customer, CustomerParams, Customers};
pub use resources::invoices::{Catalog, Invoice, Invoices, Price, Product};
pub use resources::payments::{
    PaymentIntent, PaymentIntentParams, PaymentIntents, Refund, RefundReason, Refunds, SetupIntent,
    SetupIntents,
};
pub use resources::subscriptions::{Proration, Subscription, SubscriptionItem, Subscriptions};
pub use subscription::{SubscriptionSnapshot, SubscriptionStatus};
pub use webhook::{
    sign_payload, ChargeRefunded, CheckoutSessionCompleted, CustomerDeleted,
    CustomerSubscriptionCreated, CustomerSubscriptionDeleted, CustomerSubscriptionUpdated,
    CustomerUpdated, EventData, InvoicePaid, InvoicePaymentFailed, Outcome,
    PaymentIntentPaymentFailed, PaymentIntentSucceeded, StripeEventReceived, WebhookEvent,
    WebhookVerifier, Webhooks, SIGNATURE_HEADER, TYPED_EVENTS,
};

// Re-exported so an implementor of `Billable` alongside other async traits
// needs no extra dependency.
pub use async_trait::async_trait;
