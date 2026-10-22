//! The Stripe API client — [`Stripe`], [`StripeError`] and [`List`].
//!
//! One request shape over the framework's
//! [HTTP transport port](rainier_http_client::Transport): a bearer secret
//! key, a pinned `Stripe-Version`, an `Idempotency-Key` on every write, and a
//! form-encoded body. Being a shape rather than a socket is what lets a test
//! hand [`Stripe::fake`] a [`FakeTransport`] and assert on exactly what would
//! have left the building.
//!
//! # The API version is pinned, deliberately
//!
//! Without `Stripe-Version`, a request is answered in whatever version the
//! **account** is set to in the dashboard — so somebody clicking "upgrade" in
//! a browser changes the shape of every response this code parses, with no
//! deploy. Pinning it here means a version change is a code change, reviewed
//! and tested like any other. [`Stripe::api_version`] moves it.
//!
//! # Errors
//!
//! Stripe answers a failure with a JSON `error` object. It is parsed into a
//! [`StripeError`], attached to the framework [`Error`] as details (so
//! [`StripeError::from_error`] gets it back), and quoted in the message, so
//! a log line says *which* card was declined and why. The kind follows what
//! a caller should do about it:
//!
//! | status | kind | why |
//! |---|---|---|
//! | 429, 5xx | `ServiceUnavailable` | Stripe having a bad minute — retryable, which is what a queued job's retry policy keys on |
//! | 400, 402 | `BadRequest` | this request, or this card; retrying changes nothing |
//! | 404 | `NotFound` | |
//! | 409 | `Conflict` | an idempotency key reused with different parameters |
//! | 401, 403, anything else | `Internal` | the secret key is wrong, which is ours to fix, not the user's |

use std::sync::Arc;
use std::time::Duration;

use rainier_http_client::transport::{OutboundRequest, RawResponse};
use rainier_http_client::{FakeTransport, Transport};
use rainier_support::{Error, ErrorKind, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::form::Params;

/// The API version every request is pinned to unless
/// [`api_version`](Stripe::api_version) says otherwise.
pub const DEFAULT_API_VERSION: &str = "2025-09-30.clover";

/// Where Stripe lives.
pub const DEFAULT_BASE_URL: &str = "https://api.stripe.com";

/// A Stripe API client.
///
/// Cheap to clone — the transport and key are shared — so it is fine to hold
/// one in the container and clone it into every handler.
#[derive(Clone)]
pub struct Stripe {
    http: Arc<dyn Transport>,
    secret: Arc<str>,
    base: String,
    version: String,
    account: Option<String>,
    timeout: Duration,
}

impl Stripe {
    /// A client with this secret key (`sk_live_…`, `sk_test_…` or a
    /// restricted `rk_…`).
    pub fn new(http: Arc<dyn Transport>, secret: impl Into<String>) -> Self {
        Self {
            http,
            secret: Arc::from(secret.into()),
            base: DEFAULT_BASE_URL.to_string(),
            version: DEFAULT_API_VERSION.to_string(),
            account: None,
            timeout: Duration::from_secs(80),
        }
    }

    /// A client over a [`FakeTransport`], and the fake — for tests.
    ///
    /// ```
    /// # use rainier_billing::Stripe;
    /// let (stripe, fake) = Stripe::fake();
    /// fake.responding(200, r#"{"id":"cus_1","object":"customer"}"#);
    /// ```
    pub fn fake() -> (Self, Arc<FakeTransport>) {
        let fake = Arc::new(FakeTransport::new());
        (Self::new(Arc::clone(&fake) as Arc<dyn Transport>, "sk_test_fake"), fake)
    }

    /// Talk to a different base URL — `stripe-mock`, a proxy, a test double.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Pin a different `Stripe-Version`.
    pub fn api_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// The pinned version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The wall clock on each call. Eighty seconds by default, which is
    /// Stripe's own client default — a payment confirmation that waits on a
    /// card network can take a while, and timing it out client-side leaves
    /// you not knowing whether it happened.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// A client acting for a connected account — every call carries
    /// `Stripe-Account`. A per-call [`Params::on_account`] wins over this.
    pub fn for_account(&self, account: impl Into<String>) -> Self {
        let mut scoped = self.clone();
        scoped.account = Some(account.into());
        scoped
    }

    /// Whether the key is a live-mode key. Worth asserting at boot in
    /// production, and refusing in every other environment.
    pub fn is_live(&self) -> bool {
        self.secret.starts_with("sk_live_") || self.secret.starts_with("rk_live_")
    }

    // --- the escape hatch ---------------------------------------------------

    /// `GET path` with `params` as the query string.
    pub async fn get(&self, path: &str, params: impl Into<Params>) -> Result<Value> {
        self.request("GET", path, params.into()).await
    }

    /// `POST path` with `params` as the form body.
    pub async fn post(&self, path: &str, params: impl Into<Params>) -> Result<Value> {
        self.request("POST", path, params.into()).await
    }

    /// `DELETE path`.
    pub async fn delete(&self, path: &str, params: impl Into<Params>) -> Result<Value> {
        self.request("DELETE", path, params.into()).await
    }

    /// Any call, deserialised into `T`.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        params: impl Into<Params>,
    ) -> Result<T> {
        let value = self.request(method, path, params.into()).await?;
        serde_json::from_value(value).map_err(|e| {
            Error::internal(format!(
                "stripe answered {method} {path} with an unexpected shape: {e}"
            ))
        })
    }

    /// Every item of a list endpoint, following `has_more` / `starting_after`.
    ///
    /// Bounded by `max_items`, because "every invoice this customer ever had"
    /// is a number nobody chose. Pass `limit` in `params` to size the pages.
    pub async fn list_all<T: DeserializeOwned>(
        &self,
        path: &str,
        params: impl Into<Params>,
        max_items: usize,
    ) -> Result<Vec<T>> {
        let base = params.into();
        let mut out = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let mut page_params = base.clone();
            if let Some(cursor) = &after {
                page_params.insert("starting_after", cursor);
            }
            let page: List<Value> = self.call("GET", path, page_params).await?;
            let last_id = page.data.last().and_then(|item| item["id"].as_str()).map(String::from);

            for item in page.data {
                if out.len() >= max_items {
                    return Ok(out);
                }
                out.push(serde_json::from_value(item).map_err(|e| {
                    Error::internal(format!("stripe list {path} held an unexpected item: {e}"))
                })?);
            }

            match (page.has_more, last_id) {
                (true, Some(id)) if out.len() < max_items => after = Some(id),
                _ => return Ok(out),
            }
        }
    }

    // --- the plumbing -------------------------------------------------------

    async fn request(&self, method: &str, path: &str, params: Params) -> Result<Value> {
        let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
        let mut url = format!("{}{path}", self.base);

        let mut headers = std::collections::BTreeMap::new();
        headers.insert("authorization".to_string(), format!("Bearer {}", self.secret));
        headers.insert("stripe-version".to_string(), self.version.clone());
        headers.insert("accept".to_string(), "application/json".to_string());
        if let Some(account) = params.account.as_ref().or(self.account.as_ref()) {
            headers.insert("stripe-account".to_string(), account.clone());
        }

        let encoded = params.encode();
        let body = if method == "POST" {
            headers.insert(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            );
            // Every write, not only the ones that create money: a retried
            // `update` is harmless, but deciding per endpoint is how the one
            // that is not gets missed.
            let key = params.idempotency_key.clone().unwrap_or_else(generate_idempotency_key);
            headers.insert("idempotency-key".to_string(), key);
            Some(encoded.into_bytes())
        } else {
            if !encoded.is_empty() {
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&encoded);
            }
            None
        };

        let request = OutboundRequest {
            method: method.to_string(),
            url,
            headers,
            body,
            timeout: Some(self.timeout),
        };

        let response = self.http.send(request).await.map_err(|e| {
            Error::service_unavailable(format!("stripe {method} {path} did not complete: {e}"))
        })?;
        verdict(method, &path, response)
    }
}

impl std::fmt::Debug for Stripe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret key stays out of Debug for the same reason it stays out
        // of logs. Whether it is live is useful, and is not a secret.
        f.debug_struct("Stripe")
            .field("base", &self.base)
            .field("version", &self.version)
            .field("account", &self.account)
            .field("live", &self.is_live())
            .finish()
    }
}

/// A fresh key from the OS random source.
fn generate_idempotency_key() -> String {
    use rand::RngExt as _;

    let mut rng = rand::rng();
    (0..32).map(|_| format!("{:x}", rng.random_range(0..16u8))).collect()
}

/// A 2xx is the object; anything else is Stripe's refusal.
fn verdict(method: &str, path: &str, response: RawResponse) -> Result<Value> {
    if (200..300).contains(&response.status) {
        if response.body.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_slice(&response.body).map_err(|e| {
            Error::internal(format!("stripe answered {method} {path} with something not JSON: {e}"))
        });
    }

    let mut error = StripeError::parse(response.status, &response.body);
    error.request_id = response.headers.get("request-id").cloned();

    let kind = match response.status {
        429 | 500..=599 => ErrorKind::ServiceUnavailable,
        400 | 402 => ErrorKind::BadRequest,
        404 => ErrorKind::NotFound,
        409 => ErrorKind::Conflict,
        _ => ErrorKind::Internal,
    };
    let message = format!("stripe answered {} for {method} {path}: {}", response.status, error);
    let details = serde_json::to_value(&error).unwrap_or(Value::Null);

    Err(Error::new(kind, message).with_details(serde_json::json!({ "stripe": details })))
}

/// What Stripe said went wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripeError {
    /// The HTTP status.
    pub status: u16,
    /// `card_error`, `invalid_request_error`, `api_error`, `idempotency_error`…
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// A machine-readable code — `card_declined`, `resource_missing`.
    pub code: Option<String>,
    /// For a declined card, the issuer's reason — `insufficient_funds`.
    pub decline_code: Option<String>,
    /// The parameter at fault.
    pub param: Option<String>,
    /// The human-readable explanation. Safe to show a customer for a
    /// `card_error`; not necessarily for anything else.
    pub message: Option<String>,
    /// Stripe's `Request-Id`, which is what their support will ask for.
    pub request_id: Option<String>,
}

impl StripeError {
    /// Parse an error body. A body that is not Stripe's shape (a proxy's HTML
    /// page) still yields an error, with an excerpt as its message.
    pub fn parse(status: u16, body: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Envelope {
            error: StripeError,
        }

        match serde_json::from_slice::<Envelope>(body) {
            Ok(Envelope { error }) => Self { status, ..error },
            Err(_) => Self {
                status,
                message: Some(excerpt(&String::from_utf8_lossy(body))),
                ..Self::default()
            },
        }
    }

    /// The Stripe error behind a framework error, if it came from here.
    pub fn from_error(error: &Error) -> Option<Self> {
        let details = error.details()?.get("stripe")?;
        serde_json::from_value(details.clone()).ok()
    }

    /// Whether this is a card being declined — the error worth showing the
    /// customer, as opposed to one worth paging somebody about.
    pub fn is_card_error(&self) -> bool {
        self.kind.as_deref() == Some("card_error")
    }
}

impl std::fmt::Display for StripeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if let Some(kind) = &self.kind {
            parts.push(kind.clone());
        }
        if let Some(code) = &self.code {
            parts.push(format!("code={code}"));
        }
        if let Some(decline) = &self.decline_code {
            parts.push(format!("decline_code={decline}"));
        }
        if let Some(param) = &self.param {
            parts.push(format!("param={param}"));
        }
        let head = parts.join(" ");
        match (&self.message, head.is_empty()) {
            (Some(message), true) => write!(f, "{message}"),
            (Some(message), false) => write!(f, "{head}: {message}"),
            (None, true) => write!(f, "(no error body)"),
            (None, false) => write!(f, "{head}"),
        }
    }
}

/// Enough of an error body to act on, flattened to one log line.
fn excerpt(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "(empty body)".into();
    }
    if flat.chars().count() <= 300 {
        return flat;
    }
    let cut: String = flat.chars().take(300).collect();
    format!("{cut}…")
}

/// One page of a list endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct List<T> {
    /// The items.
    #[serde(default = "Vec::new")]
    pub data: Vec<T>,
    /// Whether there is another page after this one.
    #[serde(default)]
    pub has_more: bool,
    /// The endpoint this page came from.
    #[serde(default)]
    pub url: Option<String>,
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self { data: Vec::new(), has_more: false, url: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_write_is_form_encoded_and_carries_the_pinned_headers() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, r#"{"id":"cus_1"}"#);

        let answer = stripe
            .post(
                "/v1/customers",
                Params::new().set("email", "ada@example.com").metadata("id", "7"),
            )
            .await
            .unwrap();
        assert_eq!(answer["id"], "cus_1");

        let sent = &fake.recorded()[0];
        assert_eq!(sent.method(), "POST");
        assert_eq!(sent.url(), "https://api.stripe.com/v1/customers");
        assert_eq!(sent.header("authorization"), Some("Bearer sk_test_fake"));
        assert_eq!(sent.header("stripe-version"), Some(DEFAULT_API_VERSION));
        assert_eq!(sent.header("content-type"), Some("application/x-www-form-urlencoded"));
        assert_eq!(sent.body(), "email=ada%40example.com&metadata%5Bid%5D=7");
        assert_eq!(sent.header("stripe-account"), None);
    }

    #[tokio::test]
    async fn every_write_gets_its_own_idempotency_key_unless_one_is_given() {
        let (stripe, fake) = Stripe::fake();

        stripe.post("/v1/customers", Params::new()).await.unwrap();
        stripe.post("/v1/customers", Params::new()).await.unwrap();
        stripe.post("/v1/refunds", Params::new().idempotency_key("order-42-refund")).await.unwrap();

        let sent = fake.recorded();
        let first = sent[0].header("idempotency-key").unwrap().to_string();
        let second = sent[1].header("idempotency-key").unwrap().to_string();
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
        assert_eq!(sent[2].header("idempotency-key"), Some("order-42-refund"));
    }

    #[tokio::test]
    async fn a_read_puts_its_params_in_the_query_and_has_no_body_or_key() {
        let (stripe, fake) = Stripe::fake();

        stripe
            .get(
                "/v1/subscriptions",
                Params::new().set("customer", "cus_1").expand("data.customer"),
            )
            .await
            .unwrap();

        let sent = &fake.recorded()[0];
        assert_eq!(
            sent.url(),
            "https://api.stripe.com/v1/subscriptions?customer=cus_1&expand%5B0%5D=data.customer"
        );
        assert_eq!(sent.body(), "");
        assert_eq!(sent.header("idempotency-key"), None);
    }

    #[tokio::test]
    async fn a_connected_account_rides_a_header_and_a_per_call_one_wins() {
        let (stripe, fake) = Stripe::fake();
        let connected = stripe.for_account("acct_a");

        connected.get("/v1/balance", Params::new()).await.unwrap();
        connected.get("/v1/balance", Params::new().on_account("acct_b")).await.unwrap();

        let sent = fake.recorded();
        assert_eq!(sent[0].header("stripe-account"), Some("acct_a"));
        assert_eq!(sent[1].header("stripe-account"), Some("acct_b"));
    }

    #[tokio::test]
    async fn the_version_and_base_can_be_moved() {
        let (stripe, fake) = Stripe::fake();
        let stripe = stripe.api_version("2024-06-20").with_base_url("http://localhost:12111/");

        stripe.get("v1/charges", Params::new()).await.unwrap();
        let sent = &fake.recorded()[0];
        assert_eq!(sent.url(), "http://localhost:12111/v1/charges");
        assert_eq!(sent.header("stripe-version"), Some("2024-06-20"));
    }

    #[tokio::test]
    async fn a_decline_is_a_bad_request_that_quotes_stripe_and_carries_the_details() {
        let (stripe, fake) = Stripe::fake();
        fake.responding_with_header(
            402,
            r#"{"error":{"type":"card_error","code":"card_declined","decline_code":"insufficient_funds","message":"Your card has insufficient funds.","param":"payment_method"}}"#,
            "Request-Id",
            "req_123",
        );

        let err = stripe.post("/v1/payment_intents", Params::new()).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::BadRequest);
        assert!(err.message().contains("stripe answered 402"), "{}", err.message());
        assert!(err.message().contains("insufficient_funds"), "{}", err.message());
        assert!(err.message().contains("Your card has insufficient funds."), "{}", err.message());

        let stripe_error = StripeError::from_error(&err).unwrap();
        assert!(stripe_error.is_card_error());
        assert_eq!(stripe_error.status, 402);
        assert_eq!(stripe_error.code.as_deref(), Some("card_declined"));
        assert_eq!(stripe_error.param.as_deref(), Some("payment_method"));
        assert_eq!(stripe_error.request_id.as_deref(), Some("req_123"));
    }

    #[tokio::test]
    async fn an_outage_or_throttle_is_retryable_and_a_bad_key_is_ours() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(503, "upstream down")
            .responding(429, r#"{"error":{"type":"rate_limit_error","message":"slow"}}"#)
            .responding(401, r#"{"error":{"type":"invalid_request_error","message":"Invalid API Key"}}"#)
            .responding(404, r#"{"error":{"code":"resource_missing","message":"No such customer"}}"#)
            .responding(409, r#"{"error":{"type":"idempotency_error","message":"Keys for idempotent requests can only be used with the same parameters"}}"#);

        let kinds: Vec<ErrorKind> = {
            let mut kinds = Vec::new();
            for _ in 0..5 {
                kinds.push(stripe.get("/v1/x", Params::new()).await.unwrap_err().kind());
            }
            kinds
        };
        assert_eq!(
            kinds,
            vec![
                ErrorKind::ServiceUnavailable,
                ErrorKind::ServiceUnavailable,
                ErrorKind::Internal,
                ErrorKind::NotFound,
                ErrorKind::Conflict,
            ]
        );
    }

    #[test]
    fn a_body_that_is_not_stripes_still_explains_itself() {
        let error = StripeError::parse(502, b"<html>\n  Bad Gateway\n</html>");
        assert_eq!(error.message.as_deref(), Some("<html> Bad Gateway </html>"));
        assert_eq!(error.to_string(), "<html> Bad Gateway </html>");
    }

    #[tokio::test]
    async fn list_all_follows_the_cursor_and_stops_at_the_bound() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(
            200,
            json!({"data":[{"id":"in_1"},{"id":"in_2"}],"has_more":true}).to_string(),
        )
        .responding(200, json!({"data":[{"id":"in_3"}],"has_more":false}).to_string());

        let all: Vec<Value> =
            stripe.list_all("/v1/invoices", Params::new().set("limit", 2), 100).await.unwrap();
        assert_eq!(all.len(), 3);

        let sent = fake.recorded();
        assert_eq!(sent.len(), 2);
        assert!(sent[1].url_contains("starting_after=in_2"), "{}", sent[1].url());
        assert!(sent[1].url_contains("limit=2"), "{}", sent[1].url());
    }

    #[tokio::test]
    async fn list_all_respects_max_items_without_fetching_more() {
        let (stripe, fake) = Stripe::fake();
        fake.responding(200, json!({"data":[{"id":"a"},{"id":"b"}],"has_more":true}).to_string());

        let some: Vec<Value> = stripe.list_all("/v1/invoices", Params::new(), 1).await.unwrap();
        assert_eq!(some.len(), 1);
        assert_eq!(fake.count(), 1);
    }

    #[test]
    fn the_secret_key_stays_out_of_debug_output() {
        let (stripe, _) = Stripe::fake();
        let debug = format!("{stripe:?}");
        assert!(!debug.contains("sk_test_fake"), "{debug}");
        assert!(debug.contains("live: false"), "{debug}");
    }

    #[test]
    fn live_keys_are_recognised() {
        let fake = Arc::new(FakeTransport::new());
        assert!(Stripe::new(fake.clone(), "sk_live_x").is_live());
        assert!(Stripe::new(fake.clone(), "rk_live_x").is_live());
        assert!(!Stripe::new(fake, "sk_test_x").is_live());
    }
}
