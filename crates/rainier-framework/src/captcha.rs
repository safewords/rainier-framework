//! Human checks — [`Captcha`] and the [`RequireCaptcha`] middleware, behind
//! the `captcha` feature.
//!
//! ```ignore
//! let captcha = Arc::new(Captcha::turnstile(http, env.string("CAPTCHA_SECRET", "")?)
//!     .expect_hostname("app.example.com")
//!     .expect_action("login"));
//!
//! router.post("/register", register).middleware(RequireCaptcha::new(captcha));
//! ```
//!
//! Cloudflare Turnstile, hCaptcha and reCAPTCHA all answer the same
//! question the same way: the browser widget hands the form a one-time token,
//! and the server POSTs it with a secret to a `siteverify` endpoint that says
//! whether a person produced it. One verifier speaks all three, because the
//! protocol is one protocol with three URLs and three field names.
//!
//! # It fails closed
//!
//! When the verification endpoint cannot be reached, the request is refused.
//! A check that waves everybody through whenever the provider is slow is a
//! check an attacker can disable by being patient — or by finding whatever
//! makes the call time out. [`fail_open`](Captcha::fail_open) is there for the
//! form whose availability matters more than the bots (a support form during
//! an outage), and it says so in the log every time it lets one through.
//!
//! # What is checked
//!
//! - `success`, from the provider — the token is real, unexpired and unspent.
//!   Tokens are single-use, so a replay fails here.
//! - The **hostname** the widget ran on, when you name one. Without it, a token
//!   solved on any site carrying your site key is accepted — the key is public.
//! - The **action**, when you name one: a token solved on the newsletter form
//!   should not open the login form.
//! - The client's IP is sent as `remoteip` when the request knows it, which
//!   lets the provider refuse a token solved somewhere else.
//!
//! # Testing
//!
//! The verifier speaks through the framework's [HTTP transport
//! port](rainier_http_client::Transport), so a test hands it a
//! [`FakeTransport`](rainier_http_client::FakeTransport) answering
//! `{"success": true}` — or, for a feature test that is not about the check,
//! [`Captcha::always_pass`].

use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use rainier_http::{IntoResponse, Request, Response};
use rainier_http_client::transport::OutboundRequest;
use rainier_http_client::Transport;
use rainier_middleware::{Middleware, Next};
use rainier_support::{Error, Result};
use serde_json::{json, Value};

/// Which provider's endpoint and form field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptchaProvider {
    /// Cloudflare Turnstile — field `cf-turnstile-response`.
    Turnstile,
    /// hCaptcha — field `h-captcha-response`.
    HCaptcha,
    /// Google reCAPTCHA (v2 or v3) — field `g-recaptcha-response`.
    ReCaptcha,
}

impl CaptchaProvider {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Turnstile => "https://challenges.cloudflare.com/turnstile/v0/siteverify",
            Self::HCaptcha => "https://api.hcaptcha.com/siteverify",
            Self::ReCaptcha => "https://www.google.com/recaptcha/api/siteverify",
        }
    }

    /// The form field the widget writes its token into.
    pub fn field(self) -> &'static str {
        match self {
            Self::Turnstile => "cf-turnstile-response",
            Self::HCaptcha => "h-captcha-response",
            Self::ReCaptcha => "g-recaptcha-response",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Turnstile => "turnstile",
            Self::HCaptcha => "hcaptcha",
            Self::ReCaptcha => "recaptcha",
        }
    }
}

/// What the provider said about one token.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// Whether the token was accepted, after the hostname and action checks.
    pub success: bool,
    /// The provider's error codes, plus ours (`hostname-mismatch`,
    /// `action-mismatch`, `missing-input-response`, `verification-unavailable`).
    pub error_codes: Vec<String>,
    /// The hostname the widget was solved on.
    pub hostname: Option<String>,
    /// The action the widget was rendered with.
    pub action: Option<String>,
    /// reCAPTCHA v3's score, 0.0 – 1.0. Absent elsewhere.
    pub score: Option<f64>,
    /// The whole answer.
    pub raw: Value,
}

impl Verdict {
    fn refused(code: &str) -> Self {
        Self {
            success: false,
            error_codes: vec![code.to_string()],
            hostname: None,
            action: None,
            score: None,
            raw: Value::Null,
        }
    }
}

/// Verifies widget tokens against a provider.
pub struct Captcha {
    http: Option<Arc<dyn Transport>>,
    provider: CaptchaProvider,
    secret: String,
    endpoint: String,
    hostnames: Vec<String>,
    action: Option<String>,
    min_score: Option<f64>,
    fail_open: bool,
    timeout: Duration,
}

impl Captcha {
    /// Cloudflare Turnstile with this secret key.
    pub fn turnstile(http: Arc<dyn Transport>, secret: impl Into<String>) -> Self {
        Self::new(CaptchaProvider::Turnstile, http, secret)
    }

    /// Any of the three providers.
    pub fn new(
        provider: CaptchaProvider,
        http: Arc<dyn Transport>,
        secret: impl Into<String>,
    ) -> Self {
        Self {
            http: Some(http),
            provider,
            secret: secret.into(),
            endpoint: provider.endpoint().to_string(),
            hostnames: Vec::new(),
            action: None,
            min_score: None,
            fail_open: false,
            timeout: Duration::from_secs(10),
        }
    }

    /// A verifier that accepts every token without a request — for feature
    /// tests that are about something else. Never in a deployment.
    pub fn always_pass() -> Self {
        Self { http: None, ..Self::new(CaptchaProvider::Turnstile, Arc::new(Unused), "") }
    }

    /// Only accept tokens solved on this hostname. Repeat for several.
    pub fn expect_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostnames.push(hostname.into().to_ascii_lowercase());
        self
    }

    /// Only accept tokens the widget was rendered for with this action.
    pub fn expect_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    /// reCAPTCHA v3: refuse a score below this. Ignored when no score comes back.
    pub fn min_score(mut self, score: f64) -> Self {
        self.min_score = Some(score);
        self
    }

    /// Accept requests when the provider cannot be reached. See the module
    /// documentation before using it.
    pub fn fail_open(mut self) -> Self {
        self.fail_open = true;
        self
    }

    /// Another verification URL — a proxy, a test double.
    pub fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = url.into();
        self
    }

    /// The wall clock on the verification. Ten seconds by default.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The provider this verifier speaks to.
    pub fn provider(&self) -> CaptchaProvider {
        self.provider
    }

    /// Verify `token`, solved by a client at `remote_ip`.
    ///
    /// Never an `Err`: an unreachable provider is a refusing verdict (or an
    /// accepting one under [`fail_open`](Self::fail_open)), so a caller cannot
    /// forget to handle it.
    pub async fn verify(&self, token: &str, remote_ip: Option<&str>) -> Verdict {
        let Some(http) = &self.http else {
            return Verdict { success: true, error_codes: Vec::new(), ..Verdict::refused("") };
        };
        if token.trim().is_empty() {
            return Verdict::refused("missing-input-response");
        }

        let mut form = vec![("secret", self.secret.as_str()), ("response", token)];
        if let Some(ip) = remote_ip {
            form.push(("remoteip", ip));
        }
        let body = form
            .iter()
            .map(|(key, value)| format!("{key}={}", utf8_percent_encode(value, NON_ALPHANUMERIC)))
            .collect::<Vec<_>>()
            .join("&");

        let request = OutboundRequest {
            method: "POST".into(),
            url: self.endpoint.clone(),
            headers: [
                ("content-type".to_string(), "application/x-www-form-urlencoded".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ]
            .into_iter()
            .collect(),
            body: Some(body.into_bytes()),
            timeout: Some(self.timeout),
        };

        let answer = match http.send(request).await {
            Ok(response) if (200..300).contains(&response.status) => {
                serde_json::from_slice::<Value>(&response.body).ok()
            }
            Ok(response) => {
                tracing::warn!(
                    provider = self.provider.name(),
                    status = response.status,
                    "captcha verification answered with an error status"
                );
                None
            }
            Err(e) => {
                tracing::warn!(provider = self.provider.name(), error = %e, "captcha verification failed");
                None
            }
        };

        let Some(answer) = answer else {
            if self.fail_open {
                tracing::warn!(
                    provider = self.provider.name(),
                    "captcha provider unreachable; letting the request through (fail_open)"
                );
                return Verdict { success: true, ..Verdict::refused("verification-unavailable") };
            }
            return Verdict::refused("verification-unavailable");
        };

        self.judge(answer)
    }

    /// The provider's answer, plus our own checks.
    fn judge(&self, raw: Value) -> Verdict {
        let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_string);
        let mut verdict = Verdict {
            success: raw.get("success").and_then(Value::as_bool).unwrap_or(false),
            error_codes: raw
                .get("error-codes")
                .and_then(Value::as_array)
                .map(|codes| codes.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            hostname: text("hostname"),
            action: text("action"),
            score: raw.get("score").and_then(Value::as_f64),
            raw: Value::Null,
        };

        if verdict.success && !self.hostnames.is_empty() {
            let solved_on = verdict.hostname.as_deref().unwrap_or("").to_ascii_lowercase();
            if !self.hostnames.contains(&solved_on) {
                verdict.success = false;
                verdict.error_codes.push("hostname-mismatch".into());
            }
        }
        if verdict.success {
            if let Some(expected) = &self.action {
                if verdict.action.as_deref() != Some(expected.as_str()) {
                    verdict.success = false;
                    verdict.error_codes.push("action-mismatch".into());
                }
            }
        }
        if verdict.success {
            if let (Some(min), Some(score)) = (self.min_score, verdict.score) {
                if score < min {
                    verdict.success = false;
                    verdict.error_codes.push("score-too-low".into());
                }
            }
        }

        verdict.raw = raw;
        verdict
    }
}

impl std::fmt::Debug for Captcha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret stays out of Debug for the same reason it stays out of logs.
        f.debug_struct("Captcha")
            .field("provider", &self.provider)
            .field("hostnames", &self.hostnames)
            .field("action", &self.action)
            .field("fail_open", &self.fail_open)
            .finish()
    }
}

/// The transport [`Captcha::always_pass`] holds and never calls.
struct Unused;

impl Transport for Unused {
    fn send<'a>(
        &'a self,
        _request: OutboundRequest,
    ) -> rainier_support::BoxFuture<'a, Result<rainier_http_client::transport::RawResponse>> {
        Box::pin(async { Err(Error::internal("Captcha::always_pass made a request")) })
    }

    fn name(&self) -> &str {
        "unused"
    }
}

/// Refuses a request whose widget token does not verify — `422`, with the
/// error on the provider's field, as a validation failure would be.
pub struct RequireCaptcha {
    captcha: Arc<Captcha>,
    field: String,
}

impl RequireCaptcha {
    /// Check the provider's own field (`cf-turnstile-response`, …).
    pub fn new(captcha: Arc<Captcha>) -> Self {
        let field = captcha.provider().field().to_string();
        Self { captcha, field }
    }

    /// Read the token from another input field or header-less JSON key.
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.field = field.into();
        self
    }
}

#[async_trait::async_trait]
impl Middleware for RequireCaptcha {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let token = request.input(&self.field).unwrap_or_default();
        let ip = request.ip().map(|ip| ip.to_string());

        let verdict = self.captcha.verify(&token, ip.as_deref()).await;
        if verdict.success {
            return next.run(request).await;
        }

        tracing::info!(codes = ?verdict.error_codes, "captcha refused");
        Error::validation(json!({
            self.field.as_str(): ["Please complete the human verification and try again."],
        }))
        .into_response()
    }

    fn name(&self) -> &'static str {
        "RequireCaptcha"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::{Method, StatusCode};
    use rainier_http_client::FakeTransport;
    use rainier_middleware::Pipeline;

    fn form(request: &rainier_http_client::RecordedRequest) -> Vec<(String, String)> {
        request
            .body()
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| {
                (k.to_string(), percent_encoding::percent_decode_str(v).decode_utf8_lossy().into())
            })
            .collect()
    }

    #[tokio::test]
    async fn a_token_is_posted_with_the_secret_and_the_client_ip() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(200, r#"{"success":true,"hostname":"app.test","action":"login"}"#);
        let captcha = Captcha::turnstile(Arc::clone(&fake) as _, "sec&ret");

        let verdict = captcha.verify("tok en", Some("203.0.113.9")).await;
        assert!(verdict.success);

        let sent = &fake.recorded()[0];
        assert_eq!(sent.url(), "https://challenges.cloudflare.com/turnstile/v0/siteverify");
        assert_eq!(sent.header("content-type"), Some("application/x-www-form-urlencoded"));
        assert_eq!(
            form(sent),
            vec![
                ("secret".into(), "sec&ret".into()),
                ("response".into(), "tok en".into()),
                ("remoteip".into(), "203.0.113.9".into()),
            ]
        );
    }

    #[tokio::test]
    async fn the_provider_refusing_is_a_refusal_with_its_codes() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(200, r#"{"success":false,"error-codes":["timeout-or-duplicate"]}"#);
        let captcha = Captcha::turnstile(fake as _, "s");

        let verdict = captcha.verify("replayed", None).await;
        assert!(!verdict.success);
        assert_eq!(verdict.error_codes, vec!["timeout-or-duplicate"]);
    }

    #[tokio::test]
    async fn hostname_and_action_must_match_when_named() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(200, r#"{"success":true,"hostname":"evil.example","action":"login"}"#);
        let captcha = Captcha::turnstile(Arc::clone(&fake) as _, "s").expect_hostname("app.test");
        let verdict = captcha.verify("t", None).await;
        assert!(!verdict.success);
        assert!(verdict.error_codes.contains(&"hostname-mismatch".to_string()));

        let fake = Arc::new(FakeTransport::new());
        fake.responding(200, r#"{"success":true,"hostname":"APP.test","action":"newsletter"}"#);
        let captcha =
            Captcha::turnstile(fake as _, "s").expect_hostname("app.test").expect_action("login");
        let verdict = captcha.verify("t", None).await;
        assert!(!verdict.success);
        assert_eq!(verdict.error_codes, vec!["action-mismatch"]);
    }

    #[tokio::test]
    async fn a_low_recaptcha_score_is_refused() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(200, r#"{"success":true,"score":0.2,"action":"login","hostname":"a"}"#);
        let captcha =
            Captcha::new(CaptchaProvider::ReCaptcha, Arc::clone(&fake) as _, "s").min_score(0.5);
        let verdict = captcha.verify("t", None).await;
        assert!(!verdict.success);
        assert_eq!(fake.recorded()[0].url(), "https://www.google.com/recaptcha/api/siteverify");
    }

    #[tokio::test]
    async fn an_empty_token_is_refused_without_a_request() {
        let fake = Arc::new(FakeTransport::new());
        let captcha = Captcha::turnstile(Arc::clone(&fake) as _, "s");
        let verdict = captcha.verify("  ", None).await;
        assert_eq!(verdict.error_codes, vec!["missing-input-response"]);
        fake.assert_nothing_sent();
    }

    #[tokio::test]
    async fn an_outage_fails_closed_unless_told_otherwise() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(503, "down");
        let closed = Captcha::turnstile(Arc::clone(&fake) as _, "s");
        let verdict = closed.verify("t", None).await;
        assert!(!verdict.success);
        assert_eq!(verdict.error_codes, vec!["verification-unavailable"]);

        let open = Captcha::turnstile(fake as _, "s").fail_open();
        assert!(open.verify("t", None).await.success);
    }

    #[tokio::test]
    async fn the_middleware_answers_422_on_the_field_or_lets_the_request_through() {
        let fake = Arc::new(FakeTransport::new());
        fake.responding(200, r#"{"success":false,"error-codes":["invalid-input-response"]}"#);
        fake.responding(200, r#"{"success":true}"#);
        let captcha = Arc::new(Captcha::turnstile(fake as _, "s"));

        let run = |token: &'static str| {
            let captcha = Arc::clone(&captcha);
            async move {
                Pipeline::new()
                    .through(RequireCaptcha::new(captcha))
                    .then(|_: Request| async { Response::text("in") })
                    .run(
                        Request::builder()
                            .method(Method::POST)
                            .uri("/register")
                            .form(&[("cf-turnstile-response", token)])
                            .build(),
                    )
                    .await
            }
        };

        let refused = run("bad").await;
        assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let accepted = run("good").await;
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn always_pass_makes_no_request() {
        assert!(Captcha::always_pass().verify("", None).await.success);
    }

    #[test]
    fn the_secret_stays_out_of_debug() {
        let captcha = Captcha::turnstile(Arc::new(FakeTransport::new()), "extremely-secret");
        assert!(!format!("{captcha:?}").contains("extremely-secret"));
    }
}
