//! The middleware Rainier ships with — what a fresh application's HTTP kernel
//! expects to find already written.

use std::sync::Arc;
use std::time::Duration;

use rainier_http::{IntoResponse, Method, Request, Response, StatusCode};
use serde_json::Value;

use crate::pipeline::{Middleware, Next};
use crate::rate_limit::{MemoryRateLimitStore, RateLimitStore};

/// Trims leading and trailing whitespace from every string input.
///
/// A stray space at the end of an email field is the single most common cause
/// of a "that address is already taken" that the user cannot see, so this is on
/// by default — with an exception list, because trimming a password silently
/// changes a credential the user typed on purpose.
#[derive(Debug, Clone)]
pub struct TrimStrings {
    except: Vec<String>,
}

impl Default for TrimStrings {
    fn default() -> Self {
        Self { except: vec!["password".into(), "password_confirmation".into()] }
    }
}

impl TrimStrings {
    /// Trim everything except `password` and `password_confirmation`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the exception list.
    pub fn except(mut self, keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.except = keys.into_iter().map(Into::into).collect();
        self
    }

    fn apply(&self, value: Value) -> Value {
        match value {
            Value::String(s) => Value::String(s.trim().to_string()),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| self.apply(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, item)| {
                        let trimmed =
                            if self.except.contains(&key) { item } else { self.apply(item) };
                        (key, trimmed)
                    })
                    .collect(),
            ),
            other => other,
        }
    }
}

#[async_trait::async_trait]
impl Middleware for TrimStrings {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        request.transform_input(|input| self.apply(input));
        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "TrimStrings"
    }
}

/// Turns empty string inputs into `null`.
///
/// An unfilled HTML text input submits `""`, not nothing at all. Without this,
/// every optional field in an application would need `Option<String>` plus an
/// is-it-blank check, and a `Option<u32>` field could never parse. Paired with
/// [`TrimStrings`], a whitespace-only field also becomes `null`.
#[derive(Debug, Clone, Default)]
pub struct ConvertEmptyStringsToNull;

impl ConvertEmptyStringsToNull {
    fn apply(value: Value) -> Value {
        match value {
            Value::String(s) if s.is_empty() => Value::Null,
            Value::Array(items) => Value::Array(items.into_iter().map(Self::apply).collect()),
            Value::Object(map) => {
                Value::Object(map.into_iter().map(|(k, v)| (k, Self::apply(v))).collect())
            }
            other => other,
        }
    }
}

#[async_trait::async_trait]
impl Middleware for ConvertEmptyStringsToNull {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        request.transform_input(Self::apply);
        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "ConvertEmptyStringsToNull"
    }
}

/// Adds fixed headers to every response — security headers, a server tag.
#[derive(Debug, Clone, Default)]
pub struct AddHeaders {
    headers: Vec<(String, String)>,
}

impl AddHeaders {
    /// No headers yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A conservative set of security headers suitable for an HTML app.
    ///
    /// Deliberately excludes `Strict-Transport-Security` and a
    /// `Content-Security-Policy`: HSTS is unsafe to set before HTTPS is
    /// definitely working everywhere, and a CSP that is not tailored to the app
    /// either breaks it or is too loose to help.
    pub fn security_defaults() -> Self {
        Self::new()
            .with("x-content-type-options", "nosniff")
            .with("x-frame-options", "SAMEORIGIN")
            .with("referrer-policy", "strict-origin-when-cross-origin")
    }

    /// Add a header.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[async_trait::async_trait]
impl Middleware for AddHeaders {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let mut response = next.run(request).await;
        for (name, value) in &self.headers {
            response = response.with_header(name, value);
        }
        response
    }

    fn name(&self) -> &'static str {
        "AddHeaders"
    }
}

/// Which origins a [`HandleCors`] policy allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowedOrigins {
    /// Every origin, echoed back. Cannot be combined with credentials.
    Any,
    /// Only these exact origins.
    List(Vec<String>),
}

/// Cross-Origin Resource Sharing.
///
/// Answers preflight `OPTIONS` requests itself (short-circuiting the pipeline,
/// which is why a preflight never reaches a route) and adds the response
/// headers to everything else.
///
/// # Register it **globally**, not on a route group
///
/// This is the one placement decision that decides whether the policy works at
/// all, and getting it wrong fails silently:
///
/// ```ignore
/// registry.global(HandleCors::for_origins(["https://app.example"]).allow_credentials(true));
/// ```
///
/// A browser asks permission before sending a cross-origin request that is not
/// "simple" — anything carrying `Authorization`, and any `POST` of JSON — and it
/// asks with `OPTIONS` against the same path. No route declares `OPTIONS`, so a
/// router matches the path, rejects the method and answers `405` **before**
/// entering the route's pipeline. Middleware attached to a route or a group
/// lives in that pipeline, so it never runs, and the preflight is refused with
/// no CORS headers on the refusal.
///
/// The requests that survive are exactly the ones needing no preflight — a
/// plain `GET` — which is why a group-mounted policy looks like it works while
/// the entire authenticated surface is unreachable from a browser.
///
/// Global middleware wraps the router rather than living inside it, so it sees
/// the preflight first and answers it. It also puts the headers on `404`s and
/// `405`s, which matters: without them a browser reports a mistyped URL as a
/// CORS failure.
///
/// The bootstrap says so at boot when it finds this on a route pipeline and not
/// in the global stack.
///
/// # `*` is not the permissive end of this setting
///
/// There are three reachable policies, and two of them look like settings:
///
/// | Built as | Answers | Means |
/// |---|---|---|
/// | [`any_origin`](Self::any_origin) | `Access-Control-Allow-Origin: *` | public reads work from anywhere; **no browser client can authenticate** |
/// | `any_origin().allow_credentials(true)` | the caller's own origin, reflected | **every** page on the internet may make authenticated calls and read the answers |
/// | [`for_origins`](Self::for_origins)`.allow_credentials(true)` | the caller's origin if it is listed | what an application usually means |
///
/// A browser will not attach a cookie to a cross-origin request whose response
/// omits `Access-Control-Allow-Credentials`, and will not accept that header
/// beside `Access-Control-Allow-Origin: *`. So the origin list and the
/// credentials flag are one decision: naming origins is what makes credentials
/// possible, and credentials are what make the cookie arrive.
///
/// Row two is what discovering row one invites, and it does not fail — see
/// [`allowed_origin_for`](Self::allowed_origin_for), which reflects rather than
/// answering `*`. It works for everybody, which is the problem.
#[derive(Debug, Clone)]
pub struct HandleCors {
    origins: AllowedOrigins,
    methods: Vec<String>,
    headers: Vec<String>,
    exposed: Vec<String>,
    credentials: bool,
    max_age: u32,
}

impl Default for HandleCors {
    fn default() -> Self {
        Self {
            origins: AllowedOrigins::Any,
            methods: ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
                .iter()
                .map(|m| m.to_string())
                .collect(),
            headers: ["content-type", "authorization", "x-requested-with"]
                .iter()
                .map(|h| h.to_string())
                .collect(),
            exposed: Vec::new(),
            credentials: false,
            max_age: 86_400,
        }
    }
}

impl HandleCors {
    /// A public policy: any origin, and therefore **no credentials**.
    ///
    /// Right for an API that serves the same public data to anyone and
    /// authenticates nobody from a browser. Wrong for anything a browser logs
    /// in to — see the table on [`HandleCors`], and reach for
    /// [`for_origins`](Self::for_origins) instead.
    pub fn any_origin() -> Self {
        Self::default()
    }

    /// A policy for named origins — the constructor to start from when a
    /// browser authenticates against this application.
    ///
    /// ```ignore
    /// HandleCors::for_origins(["https://app.example", "http://localhost:5173"])
    ///     .allow_credentials(true)
    /// ```
    ///
    /// The same policy as `any_origin().allow_origins(..)`, spelled so the
    /// starting point is not the one it is narrowing away from.
    pub fn for_origins(origins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::default().allow_origins(origins)
    }

    /// Restrict to an explicit origin list.
    pub fn allow_origins(mut self, origins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.origins = AllowedOrigins::List(origins.into_iter().map(Into::into).collect());
        self
    }

    /// Set the allowed methods.
    pub fn allow_methods(mut self, methods: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.methods = methods.into_iter().map(Into::into).collect();
        self
    }

    /// Set the allowed request headers.
    pub fn allow_headers(mut self, headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.headers = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Expose extra response headers to the browser.
    pub fn expose_headers(mut self, headers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exposed = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Allow credentialed requests — cookies, and `Authorization` on a
    /// same-site cookie flow.
    ///
    /// Browsers reject `Access-Control-Allow-Credentials: true` alongside
    /// `Access-Control-Allow-Origin: *`, so enabling this with
    /// [`AllowedOrigins::Any`] would produce a policy that silently never
    /// works. [`allowed_origin_for`](Self::allowed_origin_for) therefore
    /// reflects the request's origin instead of `*` when credentials are on.
    ///
    /// **That reflection is a fallback, not a feature.** It keeps the pair
    /// working rather than making it correct: a policy that reflects every
    /// origin *and* allows credentials tells every page on the internet it may
    /// call this application as whoever is logged in, and read the reply. Name
    /// the origins with [`for_origins`](Self::for_origins) and the reflection
    /// never applies — [`reflects_every_origin`](Self::reflects_every_origin)
    /// is how a test asserts it does not.
    pub fn allow_credentials(mut self, allow: bool) -> Self {
        self.credentials = allow;
        self
    }

    /// Whether this policy hands a credentialed answer to **any** origin that
    /// asks.
    ///
    /// True only for `any_origin().allow_credentials(true)` — the middle row of
    /// the table on [`HandleCors`], and the one worth an assertion in an
    /// application's own tests, because nothing about it fails at runtime.
    pub fn reflects_every_origin(&self) -> bool {
        self.credentials && matches!(self.origins, AllowedOrigins::Any)
    }

    /// How long a browser may cache the preflight result.
    pub fn max_age(mut self, seconds: u32) -> Self {
        self.max_age = seconds;
        self
    }

    /// The `Access-Control-Allow-Origin` value for a request from `origin`, or
    /// `None` if the origin is not allowed.
    pub fn allowed_origin_for(&self, origin: Option<&str>) -> Option<String> {
        match &self.origins {
            AllowedOrigins::Any if self.credentials => origin.map(str::to_string),
            AllowedOrigins::Any => Some("*".to_string()),
            AllowedOrigins::List(allowed) => {
                let origin = origin?;
                allowed.iter().any(|a| a == origin).then(|| origin.to_string())
            }
        }
    }

    fn decorate(&self, response: Response, allowed_origin: Option<String>) -> Response {
        let Some(origin) = allowed_origin else {
            return response;
        };

        let mut response = response.with_header("access-control-allow-origin", &origin);
        if origin != "*" {
            // Caches must not serve one origin's response to another.
            response = response.with_added_header("vary", "Origin");
        }
        if self.credentials {
            response = response.with_header("access-control-allow-credentials", "true");
        }
        if !self.exposed.is_empty() {
            response =
                response.with_header("access-control-expose-headers", &self.exposed.join(", "));
        }
        response
    }
}

#[async_trait::async_trait]
impl Middleware for HandleCors {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let origin = request.header("origin").map(str::to_string);
        let allowed = self.allowed_origin_for(origin.as_deref());

        // A preflight is answered here and never reaches a route — there is no
        // route for `OPTIONS /whatever` and the browser only wants headers.
        if request.method() == Method::OPTIONS
            && request.header("access-control-request-method").is_some()
        {
            let response = Response::new(StatusCode::NO_CONTENT)
                .with_header("access-control-allow-methods", &self.methods.join(", "))
                .with_header("access-control-allow-headers", &self.headers.join(", "))
                .with_header("access-control-max-age", &self.max_age.to_string());
            return self.decorate(response, allowed);
        }

        let response = next.run(request).await;
        self.decorate(response, allowed)
    }

    fn name(&self) -> &'static str {
        "HandleCors"
    }
}

/// A fixed-window rate limiter.
///
/// ```ignore
/// // The default: this process's memory, keyed by token or address.
/// router.get("/api/posts", index).middleware(ThrottleRequests::per_minute(60));
///
/// // A credential limiter: keyed by what was submitted, counted in the shared
/// // cache, under its own name.
/// router.post("/login", login).middleware(
///     ThrottleRequests::per_minute(5)
///         .named("login")
///         .keyed_by(|request| request.input("email"))
///         .stored_in(Arc::clone(&limits)),
/// );
/// ```
///
/// # What to key on
///
/// The default is the bearer token if there is one and the address otherwise,
/// which is right for an API: one caller behind a shared NAT cannot exhaust
/// everyone else's allowance.
///
/// It is the **wrong** key for a login form. There is no token yet, so every
/// attempt counts against an address — and an attacker spraying one password
/// across ten thousand accounts from a botnet never trips it, while a whole
/// office behind one NAT locks itself out. Keying on the submitted address
/// instead limits attempts *per account*, which is the thing being protected.
///
/// Both are worth having, on the same route, under two names. Which is why
/// [`named`](Self::named) exists.
#[derive(Clone)]
pub struct ThrottleRequests {
    max_attempts: u32,
    window: Duration,
    /// Namespaces the keys, so two limiters on one route do not share a
    /// counter.
    name: Option<String>,
    /// How a request becomes a key. `None` is the default token-or-address
    /// rule.
    key_by: Option<KeyFn>,
    store: Arc<dyn RateLimitStore>,
}

/// Turns a request into the key it should be counted under.
///
/// Returning `None` means **do not count this request** — it has nothing to
/// key on, which for a login limiter is a request with no email in it and is
/// somebody else's `422` rather than this middleware's `429`.
type KeyFn = Arc<dyn Fn(&Request) -> Option<String> + Send + Sync>;

impl ThrottleRequests {
    /// `max_attempts` per `window`, counted in this process.
    pub fn new(max_attempts: u32, window: Duration) -> Self {
        Self {
            max_attempts,
            window,
            name: None,
            key_by: None,
            store: Arc::new(MemoryRateLimitStore::new()),
        }
    }

    /// `max_attempts` per minute.
    pub fn per_minute(max_attempts: u32) -> Self {
        Self::new(max_attempts, Duration::from_secs(60))
    }

    /// `max_attempts` per hour.
    pub fn per_hour(max_attempts: u32) -> Self {
        Self::new(max_attempts, Duration::from_secs(3600))
    }

    /// `max_attempts` per day.
    pub fn per_day(max_attempts: u32) -> Self {
        Self::new(max_attempts, Duration::from_secs(86_400))
    }

    /// Count in `store` rather than in this process.
    ///
    /// Pass the one bound at boot, so every limiter in the application shares
    /// a counter and a deployment changes where they all live at once.
    #[must_use = "this returns a configured throttle rather than configuring in place"]
    pub fn stored_in(mut self, store: Arc<dyn RateLimitStore>) -> Self {
        self.store = store;
        self
    }

    /// Namespace this limiter's keys.
    ///
    /// Two limiters without names, on one route, count the same request twice
    /// against the same key — so a `5/min` and a `100/hour` become a `5/min`
    /// that also spends the hourly allowance. A name keeps them apart.
    ///
    /// Also what keeps `/login` and `/password/reset` from sharing an
    /// allowance when both are keyed by email.
    #[must_use = "this returns a configured throttle rather than configuring in place"]
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Count against something the request carries.
    ///
    /// ```ignore
    /// ThrottleRequests::per_minute(5).keyed_by(|request| request.input("email"))
    /// ```
    ///
    /// Returning `None` skips the limiter for that request entirely — see
    /// [`KeyFn`](self).
    #[must_use = "this returns a configured throttle rather than configuring in place"]
    pub fn keyed_by<F>(mut self, key: F) -> Self
    where
        F: Fn(&Request) -> Option<String> + Send + Sync + 'static,
    {
        self.key_by = Some(Arc::new(key));
        self
    }

    /// Whether the counters behind this limiter are shared between instances.
    ///
    /// `false` means a limit of `n` is really `n × replicas`.
    pub fn is_shared(&self) -> bool {
        self.store.is_shared()
    }

    /// The limit.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Its name, if it has one.
    pub fn limiter_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The key a request is counted under, namespaced.
    fn key(&self, request: &Request) -> Option<String> {
        let key = match &self.key_by {
            Some(key_by) => key_by(request)?,
            None => match request.bearer_token() {
                Some(token) => format!("token:{token}"),
                None => match request.ip() {
                    Some(ip) => format!("ip:{ip}"),
                    None => "anonymous".to_string(),
                },
            },
        };

        Some(match &self.name {
            Some(name) => format!("throttle:{name}:{key}"),
            None => format!("throttle:{key}"),
        })
    }
}

impl std::fmt::Debug for ThrottleRequests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThrottleRequests")
            .field("max_attempts", &self.max_attempts)
            .field("window", &self.window)
            .field("name", &self.name)
            .field("store", &self.store.name())
            .field("keyed", &if self.key_by.is_some() { "custom" } else { "token-or-ip" })
            .finish()
    }
}

#[async_trait::async_trait]
impl Middleware for ThrottleRequests {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let Some(key) = self.key(&request) else {
            // Nothing to count against. Not this middleware's problem to
            // report — a login with no email in it is a validation failure,
            // and answering `429` here would say something untrue.
            return next.run(request).await;
        };

        let hit = match self.store.hit(&key, self.window).await {
            Ok(hit) => hit,
            // The counter is unreachable. Fail **open**: a Redis outage that
            // turned every request into a 429 would be a much larger incident
            // than the one the limiter exists to prevent, and it would take the
            // login page down with it.
            Err(e) => {
                tracing::error!(
                    error = %e.message(),
                    limiter = self.name.as_deref().unwrap_or("unnamed"),
                    "the rate limit store is unreachable; allowing the request"
                );
                return next.run(request).await;
            }
        };

        let limit = self.max_attempts.to_string();

        if hit.count > self.max_attempts {
            let retry_after = hit.resets_in.as_secs() + 1;

            return rainier_support::Error::too_many_requests("Too many requests.")
                .into_response()
                .with_header("retry-after", &retry_after.to_string())
                .with_header("x-ratelimit-limit", &limit)
                .with_header("x-ratelimit-remaining", "0");
        }

        let remaining = self.max_attempts - hit.count;

        next.run(request)
            .await
            .with_header("x-ratelimit-limit", &limit)
            .with_header("x-ratelimit-remaining", &remaining.to_string())
    }

    fn name(&self) -> &'static str {
        "ThrottleRequests"
    }
}
/// A shared [`ThrottleRequests`], so several routes can be limited against one
/// counter.
pub type SharedThrottle = Arc<ThrottleRequests>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;
    use rainier_http::ClientIp;
    use serde_json::json;

    async fn echo_input(request: Request) -> Response {
        Response::json(&request.all())
    }

    async fn body_json(response: Response) -> Value {
        let bytes = response.into_http().into_body().collect().await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn trim_strings_trims_recursively() {
        let request = Request::builder()
            .method(Method::POST)
            .json(&json!({ "name": "  ada  ", "tags": [" a ", "b"], "nested": { "x": " y " } }))
            .build();

        let response =
            Pipeline::new().through(TrimStrings::new()).then(echo_input).run(request).await;

        assert_eq!(
            body_json(response).await,
            json!({ "name": "ada", "tags": ["a", "b"], "nested": { "x": "y" } })
        );
    }

    #[tokio::test]
    async fn trim_strings_leaves_passwords_alone() {
        let request = Request::builder()
            .method(Method::POST)
            .json(&json!({ "password": "  spaces matter  ", "email": " a@b.c " }))
            .build();

        let response =
            Pipeline::new().through(TrimStrings::new()).then(echo_input).run(request).await;

        assert_eq!(
            body_json(response).await,
            json!({ "password": "  spaces matter  ", "email": "a@b.c" })
        );
    }

    #[tokio::test]
    async fn empty_strings_become_null() {
        let request = Request::builder()
            .method(Method::POST)
            .json(&json!({ "nickname": "", "name": "ada", "list": [""] }))
            .build();

        let response =
            Pipeline::new().through(ConvertEmptyStringsToNull).then(echo_input).run(request).await;

        assert_eq!(
            body_json(response).await,
            json!({ "nickname": null, "name": "ada", "list": [null] })
        );
    }

    #[tokio::test]
    async fn trimming_then_nulling_turns_blank_into_null() {
        let request =
            Request::builder().method(Method::POST).json(&json!({ "nickname": "   " })).build();

        let response = Pipeline::new()
            .through(TrimStrings::new())
            .through(ConvertEmptyStringsToNull)
            .then(echo_input)
            .run(request)
            .await;

        assert_eq!(body_json(response).await, json!({ "nickname": null }));
    }

    #[tokio::test]
    async fn add_headers_decorates_the_response() {
        let response = Pipeline::new()
            .through(AddHeaders::security_defaults())
            .then(|_: Request| async { Response::text("ok") })
            .run(Request::builder().build())
            .await;

        assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(response.header("x-frame-options"), Some("SAMEORIGIN"));
        assert!(response.header("strict-transport-security").is_none());
    }

    #[tokio::test]
    async fn cors_answers_a_preflight_without_reaching_the_route() {
        let request = Request::builder()
            .method(Method::OPTIONS)
            .header("origin", "https://app.example")
            .header("access-control-request-method", "POST")
            .build();

        let response = Pipeline::new()
            .through(HandleCors::any_origin())
            .then(|_: Request| async { panic!("a preflight must not reach the route") })
            .run(request)
            .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.header("access-control-allow-origin"), Some("*"));
        assert!(response.header("access-control-allow-methods").unwrap().contains("POST"));
        assert_eq!(response.header("access-control-max-age"), Some("86400"));
    }

    #[tokio::test]
    async fn cors_decorates_an_ordinary_response() {
        let request = Request::builder().header("origin", "https://app.example").build();
        let response = Pipeline::new()
            .through(HandleCors::any_origin())
            .then(|_: Request| async { Response::text("ok") })
            .run(request)
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.header("access-control-allow-origin"), Some("*"));
    }

    #[test]
    fn cors_origin_matching() {
        let any = HandleCors::any_origin();
        assert_eq!(any.allowed_origin_for(Some("https://a.example")).as_deref(), Some("*"));
        assert_eq!(any.allowed_origin_for(None).as_deref(), Some("*"));

        let listed = HandleCors::any_origin().allow_origins(["https://a.example"]);
        assert_eq!(
            listed.allowed_origin_for(Some("https://a.example")).as_deref(),
            Some("https://a.example")
        );
        assert_eq!(listed.allowed_origin_for(Some("https://evil.example")), None);
        assert_eq!(listed.allowed_origin_for(None), None);
    }

    #[test]
    fn credentials_never_pair_with_a_wildcard_origin() {
        let policy = HandleCors::any_origin().allow_credentials(true);
        // Echoes the origin instead of `*`, which browsers would reject.
        assert_eq!(
            policy.allowed_origin_for(Some("https://a.example")).as_deref(),
            Some("https://a.example")
        );
    }

    #[tokio::test]
    async fn a_restricted_origin_response_varies_on_origin() {
        let request = Request::builder().header("origin", "https://a.example").build();
        let response = Pipeline::new()
            .through(HandleCors::any_origin().allow_origins(["https://a.example"]))
            .then(|_: Request| async { Response::text("ok") })
            .run(request)
            .await;

        assert_eq!(response.header("vary"), Some("Origin"));
    }

    fn from_ip(n: u8) -> Request {
        Request::builder().build().with_extension(ClientIp(std::net::IpAddr::from([127, 0, 0, n])))
    }

    #[tokio::test]
    async fn throttling_allows_up_to_the_limit_then_rejects() {
        let throttle: SharedThrottle = Arc::new(ThrottleRequests::per_minute(2));

        for expected_remaining in ["1", "0"] {
            let response = Pipeline::new()
                .through_arc(Arc::clone(&throttle) as Arc<dyn Middleware>)
                .then(|_: Request| async { Response::text("ok") })
                .run(from_ip(1))
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.header("x-ratelimit-remaining"), Some(expected_remaining));
        }

        let blocked = Pipeline::new()
            .through_arc(Arc::clone(&throttle) as Arc<dyn Middleware>)
            .then(|_: Request| async { panic!("must not reach the route once throttled") })
            .run(from_ip(1))
            .await;

        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(blocked.header("retry-after").is_some());
    }

    #[tokio::test]
    async fn throttling_counts_each_client_separately() {
        let throttle: SharedThrottle = Arc::new(ThrottleRequests::per_minute(1));

        for ip in [1, 2] {
            let response = Pipeline::new()
                .through_arc(Arc::clone(&throttle) as Arc<dyn Middleware>)
                .then(|_: Request| async { Response::text("ok") })
                .run(from_ip(ip))
                .await;
            assert_eq!(response.status(), StatusCode::OK, "ip {ip} should have its own allowance");
        }
    }

    #[tokio::test]
    async fn throttling_keys_authenticated_requests_by_token() {
        let throttle = ThrottleRequests::per_minute(1);

        let with_token = |token: &str| {
            Request::builder().header("authorization", &format!("Bearer {token}")).build()
        };

        assert_eq!(throttle.key(&with_token("a")).as_deref(), Some("throttle:token:a"));
        assert_ne!(throttle.key(&with_token("a")), throttle.key(&with_token("b")));
    }

    #[tokio::test]
    async fn a_name_namespaces_the_key() {
        // Two limiters on one route would otherwise count the same request
        // twice against the same key.
        let plain = ThrottleRequests::per_minute(1);
        let named = ThrottleRequests::per_minute(1).named("login");

        let request = Request::builder().header("authorization", "Bearer a").build();

        assert_eq!(named.key(&request).as_deref(), Some("throttle:login:token:a"));
        assert_ne!(plain.key(&request), named.key(&request));
    }

    #[tokio::test]
    async fn a_custom_key_counts_what_was_submitted() {
        let throttle =
            ThrottleRequests::per_minute(1).named("login").keyed_by(|r| r.input("email"));

        let request = Request::builder()
            .method(Method::POST)
            .uri("/login")
            .json(&json!({ "email": "ada@example.com" }))
            .build();

        assert_eq!(throttle.key(&request).as_deref(), Some("throttle:login:ada@example.com"));
    }

    #[tokio::test]
    async fn a_request_with_nothing_to_key_on_is_not_counted() {
        // A login with no email is a validation failure, and answering 429
        // here would say something untrue.
        let throttle = ThrottleRequests::per_minute(1).keyed_by(|r| r.input("email"));
        let request = Request::builder().method(Method::POST).uri("/login").build();

        assert_eq!(throttle.key(&request), None);

        // And it reaches the handler however many times it is repeated.
        for _ in 0..5 {
            let response = Pipeline::new()
                .through(ThrottleRequests::per_minute(1).keyed_by(|r| r.input("email")))
                .then(|_| async { Response::ok("through") })
                .run(Request::builder().method(Method::POST).uri("/login").build())
                .await;

            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn the_limit_is_enforced_and_the_window_resets() {
        let throttle = Arc::new(ThrottleRequests::new(2, Duration::from_millis(40)));

        let send = || {
            let throttle = Arc::clone(&throttle);
            async move {
                Pipeline::new()
                    .through_arc(throttle as Arc<dyn Middleware>)
                    .then(|_| async { Response::ok("through") })
                    .run(Request::builder().header("authorization", "Bearer a").build())
                    .await
            }
        };

        let first = send().await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.header("x-ratelimit-remaining"), Some("1"));

        assert_eq!(send().await.header("x-ratelimit-remaining"), Some("0"));

        let refused = send().await;
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(refused.header("retry-after").is_some());

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(send().await.status(), StatusCode::OK, "the window should have rolled over");
    }

    #[tokio::test]
    async fn an_unreachable_store_fails_open() {
        // A Redis outage that turned every request into a 429 would be a much
        // larger incident than the one the limiter prevents — and it would
        // take the login page down with it.
        struct Broken;

        impl RateLimitStore for Broken {
            fn hit<'a>(
                &'a self,
                _key: &'a str,
                _window: Duration,
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<crate::rate_limit::Hit>>
            {
                Box::pin(async { Err(rainier_support::Error::internal("connection refused")) })
            }

            fn peek<'a>(
                &'a self,
                _key: &'a str,
            ) -> rainier_support::BoxFuture<
                'a,
                rainier_support::Result<Option<crate::rate_limit::Hit>>,
            > {
                Box::pin(async { Ok(None) })
            }

            fn clear<'a>(
                &'a self,
                _key: &'a str,
            ) -> rainier_support::BoxFuture<'a, rainier_support::Result<()>> {
                Box::pin(async { Ok(()) })
            }

            fn is_shared(&self) -> bool {
                true
            }

            fn name(&self) -> &str {
                "broken"
            }
        }

        let response = Pipeline::new()
            .through(ThrottleRequests::per_minute(1).stored_in(Arc::new(Broken)))
            .then(|_| async { Response::ok("through") })
            .run(Request::builder().header("authorization", "Bearer a").build())
            .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn a_memory_backed_throttle_says_it_is_not_shared() {
        assert!(!ThrottleRequests::per_minute(1).is_shared());
        assert_eq!(ThrottleRequests::per_minute(5).max_attempts(), 5);
        assert_eq!(ThrottleRequests::per_minute(5).named("x").limiter_name(), Some("x"));
    }

    #[tokio::test]
    async fn a_throttled_response_is_a_framework_error_body() {
        let response = rainier_support::Error::new(
            rainier_support::ErrorKind::TooManyRequests,
            "Too many requests.",
        )
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
