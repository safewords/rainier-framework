//! Cross-site request forgery protection — [`VerifyCsrfToken`].
//!
//! ```ignore
//! // Inside the session middleware: the token lives in the session.
//! MiddlewareStack::new()
//!     .push(StartSession::from_manager(&sessions))
//!     .push(VerifyCsrfToken::new().except(["webhooks/*"]));
//! ```
//!
//! # What it checks
//!
//! A request that changes something — anything but `GET`, `HEAD` and
//! `OPTIONS` — must prove it came from a page this application served. It
//! passes when **any** of these holds, in this order:
//!
//! 1. its path matches the except-list (`*` matches any run of characters);
//! 2. the browser says it is same-origin — `Sec-Fetch-Site: same-origin`, a
//!    header a page cannot set and every current browser sends;
//! 3. it carries the session's token: a `_token` form field, an
//!    `X-CSRF-TOKEN` header, or an `X-XSRF-TOKEN` header holding the
//!    `XSRF-TOKEN` cookie's value.
//!
//! Otherwise it is answered `419`, before the handler runs. The comparison is
//! constant-time.
//!
//! # The cookie
//!
//! Every response that passes sets `XSRF-TOKEN` — readable by script, not
//! `HttpOnly`, which is the point: a front end copies it into the
//! `X-XSRF-TOKEN` header, and most HTTP clients for the browser do that
//! without being asked. A page on another origin cannot read it, so it cannot
//! copy it.
//!
//! Its value is the token itself unless an [`XsrfCodec`] says otherwise — a
//! migration that shares cookies with an application sealing them can plug
//! that application's format in (`rainier-compat` has one).
//!
//! # Minting on reads
//!
//! The cookie needs a token, and the first `GET` of a fresh visitor has none,
//! so by default it mints one — which writes a session for that visitor. An
//! API that only takes bearer tokens should not be behind this middleware at
//! all; a site that would rather not store a session for every anonymous page
//! view can turn [`mint_on_read`](VerifyCsrfToken::mint_on_read) off, and the
//! cookie then appears from the first page that starts a session anyway.
//!
//! # Wiring
//!
//! It must run **inside** [`StartSession`](crate::StartSession). Without a
//! session there is no token to compare against, and it refuses every
//! changing request rather than waving them through.

use std::sync::Arc;

use async_trait::async_trait;
use rainier_http::{Cookie, Method, Request, Response, SameSite, StatusCode};
use rainier_middleware::{Middleware, Next};
use rainier_support::Result;
use serde_json::json;

use crate::middleware::SessionRequestExt;
use crate::session::Session;

/// The cookie a front end reads the token from.
pub const XSRF_COOKIE: &str = "XSRF-TOKEN";

/// How the `XSRF-TOKEN` cookie's value relates to the token.
pub trait XsrfCodec: Send + Sync + 'static {
    /// The cookie value carrying `token`.
    fn seal(&self, token: &str) -> Result<String>;

    /// The token an `X-XSRF-TOKEN` header carries, or `None` if it carries
    /// nothing this codec recognises.
    fn open(&self, header: &str) -> Option<String>;
}

/// The cookie **is** the token. The default.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlainXsrf;

impl XsrfCodec for PlainXsrf {
    fn seal(&self, token: &str) -> Result<String> {
        Ok(token.to_string())
    }

    fn open(&self, header: &str) -> Option<String> {
        Some(header.to_string())
    }
}

/// Refuses state-changing requests that cannot show the session's token.
#[derive(Clone)]
pub struct VerifyCsrfToken {
    except: Vec<String>,
    codec: Arc<dyn XsrfCodec>,
    cookie: bool,
    path: String,
    domain: Option<String>,
    secure: bool,
    same_site: SameSite,
    fetch_metadata: bool,
    allow_same_site: bool,
    mint_on_read: bool,
}

impl Default for VerifyCsrfToken {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifyCsrfToken {
    /// Verify every changing request, set a plain `XSRF-TOKEN` cookie, and
    /// trust `Sec-Fetch-Site: same-origin`.
    pub fn new() -> Self {
        Self {
            except: Vec::new(),
            codec: Arc::new(PlainXsrf),
            cookie: true,
            path: "/".into(),
            domain: None,
            secure: false,
            same_site: SameSite::Lax,
            fetch_metadata: true,
            allow_same_site: false,
            mint_on_read: true,
        }
    }

    /// Paths not to verify — `webhooks/*`, `/stripe/webhook`. A leading `/` is
    /// optional; `*` matches anything, including `/`.
    #[must_use]
    pub fn except<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.except.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Seal the cookie, and open the header, with `codec`.
    #[must_use]
    pub fn codec(mut self, codec: Arc<dyn XsrfCodec>) -> Self {
        self.codec = codec;
        self
    }

    /// Whether to set the `XSRF-TOKEN` cookie at all. On by default; a
    /// server-rendered site that only ever posts forms with `_token` in them
    /// can turn it off.
    #[must_use]
    pub fn cookie(mut self, enabled: bool) -> Self {
        self.cookie = enabled;
        self
    }

    /// The cookie's attributes — match the session cookie's.
    #[must_use]
    pub fn cookie_attributes(
        mut self,
        path: impl Into<String>,
        domain: Option<String>,
        secure: bool,
        same_site: SameSite,
    ) -> Self {
        self.path = path.into();
        self.domain = domain;
        self.secure = secure;
        self.same_site = same_site;
        self
    }

    /// Whether `Sec-Fetch-Site: same-origin` is proof enough. On by default:
    /// a page cannot forge the header, and a browser old enough not to send it
    /// falls through to the token, so nothing is weakened.
    #[must_use]
    pub fn trust_fetch_metadata(mut self, trust: bool) -> Self {
        self.fetch_metadata = trust;
        self
    }

    /// Also accept `Sec-Fetch-Site: same-site` — a sibling subdomain. Off by
    /// default: every subdomain is then as trusted as this one.
    #[must_use]
    pub fn allow_same_site(mut self, allow: bool) -> Self {
        self.allow_same_site = allow;
        self
    }

    /// Whether a read request with no token yet mints one so the cookie can be
    /// set. See the [module documentation](self#minting-on-reads).
    #[must_use]
    pub fn mint_on_read(mut self, mint: bool) -> Self {
        self.mint_on_read = mint;
        self
    }

    fn is_reading(request: &Request) -> bool {
        matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS)
    }

    fn is_excepted(&self, request: &Request) -> bool {
        let path = request.path().trim_start_matches('/');
        self.except.iter().any(|pattern| {
            let pattern =
                if pattern == "/" { pattern.as_str() } else { pattern.trim_start_matches('/') };
            if pattern == "/" {
                return path.is_empty();
            }
            wildcard(pattern, path)
        })
    }

    fn fetch_metadata_passes(&self, request: &Request) -> bool {
        if !self.fetch_metadata {
            return false;
        }
        match request.header("sec-fetch-site") {
            Some("same-origin") => true,
            Some("same-site") => self.allow_same_site,
            _ => false,
        }
    }

    /// The token the request presents, wherever it put it.
    fn presented(&self, request: &Request) -> Option<String> {
        if let Some(token) = request.input("_token").filter(|token| !token.is_empty()) {
            return Some(token);
        }
        if let Some(token) = request.header("x-csrf-token").filter(|token| !token.is_empty()) {
            return Some(token.to_string());
        }
        let header = request.header("x-xsrf-token").filter(|header| !header.is_empty())?;
        self.codec.open(header)
    }

    fn tokens_match(&self, request: &Request) -> bool {
        let Some(session) = request.session() else {
            tracing::error!(
                "VerifyCsrfToken ran without a session; refusing — put it inside StartSession"
            );
            return false;
        };
        let Some(presented) = self.presented(request) else {
            return false;
        };
        // Only compare against a token the session already has: minting one
        // here would compare against a value nobody could have presented.
        match session.string(crate::session::TOKEN_KEY) {
            Some(token) => constant_time_eq(token.as_bytes(), presented.as_bytes()),
            None => false,
        }
    }

    fn with_cookie(&self, session: &Session, response: Response) -> Response {
        if !self.cookie {
            return response;
        }
        if !self.mint_on_read && !session.has(crate::session::TOKEN_KEY) {
            return response;
        }

        let token = session.token();
        let value = match self.codec.seal(&token) {
            Ok(value) => value,
            Err(e) => {
                tracing::error!(error = %e, "could not seal the XSRF-TOKEN cookie");
                return response;
            }
        };

        let mut cookie = Cookie::new(XSRF_COOKIE, value)
            .path(&self.path)
            .http_only(false)
            .secure(self.secure)
            .same_site(self.same_site);
        if let Some(domain) = &self.domain {
            cookie = cookie.domain(domain);
        }
        response.with_cookie(&cookie)
    }
}

/// `419`, the status the convention has settled on for an expired or missing
/// anti-forgery token — distinct from `403` so a front end can tell "reload
/// the page" from "you may not".
pub fn mismatch(request: &Request) -> Response {
    let status = StatusCode::from_u16(419).expect("419 is a valid status code");
    if request.expects_json() {
        Response::json(&json!({ "message": "CSRF token mismatch." })).with_status(status)
    } else {
        Response::text("CSRF token mismatch.").with_status(status)
    }
}

#[async_trait]
impl Middleware for VerifyCsrfToken {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let passes = Self::is_reading(&request)
            || self.is_excepted(&request)
            || self.fetch_metadata_passes(&request)
            || self.tokens_match(&request);

        if !passes {
            return mismatch(&request);
        }

        // The cookie needs the session after the handler has run — a login
        // regenerates the token — so the request is kept for it.
        let session = request.session().cloned();
        let response = next.run(request).await;

        match session {
            Some(session) => self.with_cookie(&session, response),
            None => response,
        }
    }

    fn name(&self) -> &'static str {
        "VerifyCsrfToken"
    }
}

impl std::fmt::Debug for VerifyCsrfToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyCsrfToken")
            .field("except", &self.except)
            .field("cookie", &self.cookie)
            .field("fetch_metadata", &self.fetch_metadata)
            .finish_non_exhaustive()
    }
}

/// Equal, without returning early on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let difference = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(difference) == 0
}

/// `*` matches any run of characters, everything else itself.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemorySessionStore, SessionStore, StartSession};
    use rainier_middleware::Pipeline;

    fn pipeline(csrf: VerifyCsrfToken) -> (Pipeline, Arc<MemorySessionStore>) {
        let store = Arc::new(MemorySessionStore::default());
        let pipeline = Pipeline::new()
            .through(StartSession::new(Arc::clone(&store) as Arc<dyn SessionStore>))
            .through(csrf);
        (pipeline, store)
    }

    async fn run(csrf: &VerifyCsrfToken, request: Request) -> Response {
        let (pipeline, _) = pipeline(csrf.clone());
        pipeline.then(|_: Request| async { Response::text("handled") }).run(request).await
    }

    /// A session already holding `token`, and the cookie naming it.
    async fn session_with_token(store: &MemorySessionStore, token: &str) -> String {
        let mut data = crate::SessionData::default();
        data.values.insert(crate::TOKEN_KEY.into(), serde_json::Value::String(token.into()));
        let id = crate::generate_session_id();
        store.write(&id, &data).await.unwrap();
        id
    }

    async fn post(csrf: VerifyCsrfToken, build: impl FnOnce(String) -> Request) -> Response {
        let (pipeline, store) = pipeline(csrf);
        let id = session_with_token(&store, "the-token").await;
        pipeline.then(|_: Request| async { Response::text("handled") }).run(build(id)).await
    }

    fn cookie_header(id: &str) -> String {
        format!("rainier_session={id}")
    }

    #[tokio::test]
    async fn a_read_passes_and_gets_the_cookie() {
        let response = run(&VerifyCsrfToken::new(), Request::builder().uri("/").build()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookies: Vec<_> = response.headers().get_all("set-cookie").iter().collect();
        let xsrf = cookies.iter().find(|c| c.to_str().unwrap().starts_with("XSRF-TOKEN=")).unwrap();
        assert!(!xsrf.to_str().unwrap().contains("HttpOnly"), "script must be able to read it");
    }

    #[tokio::test]
    async fn a_post_without_a_token_is_419() {
        let response = post(VerifyCsrfToken::new(), |id| {
            Request::builder()
                .method(Method::POST)
                .uri("/posts")
                .header("cookie", &cookie_header(&id))
                .build()
        })
        .await;
        assert_eq!(response.status().as_u16(), 419);
    }

    #[tokio::test]
    async fn a_post_with_the_token_in_any_of_the_three_places_passes() {
        let field = post(VerifyCsrfToken::new(), |id| {
            Request::builder()
                .method(Method::POST)
                .uri("/posts")
                .header("cookie", &cookie_header(&id))
                .form(&[("_token", "the-token")])
                .build()
        })
        .await;
        assert_eq!(field.status(), StatusCode::OK);

        for header in ["x-csrf-token", "x-xsrf-token"] {
            let response = post(VerifyCsrfToken::new(), |id| {
                Request::builder()
                    .method(Method::POST)
                    .uri("/posts")
                    .header("cookie", &cookie_header(&id))
                    .header(header, "the-token")
                    .build()
            })
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{header}");
        }
    }

    #[tokio::test]
    async fn a_wrong_token_is_419_and_json_clients_get_json() {
        let response = post(VerifyCsrfToken::new(), |id| {
            Request::builder()
                .method(Method::DELETE)
                .uri("/posts/1")
                .header("cookie", &cookie_header(&id))
                .header("accept", "application/json")
                .header("x-csrf-token", "the-tokeN")
                .build()
        })
        .await;
        assert_eq!(response.status().as_u16(), 419);
        assert!(response.header("content-type").unwrap().starts_with("application/json"));
    }

    #[tokio::test]
    async fn a_session_with_no_token_yet_matches_nothing() {
        let response = run(
            &VerifyCsrfToken::new(),
            Request::builder().method(Method::POST).uri("/x").header("x-csrf-token", "").build(),
        )
        .await;
        assert_eq!(response.status().as_u16(), 419);
    }

    #[tokio::test]
    async fn same_origin_fetch_metadata_passes_and_cross_site_does_not() {
        let csrf = VerifyCsrfToken::new();
        let request = |site: &str| {
            Request::builder().method(Method::POST).uri("/x").header("sec-fetch-site", site).build()
        };

        assert_eq!(run(&csrf, request("same-origin")).await.status(), StatusCode::OK);
        assert_eq!(run(&csrf, request("cross-site")).await.status().as_u16(), 419);
        assert_eq!(run(&csrf, request("same-site")).await.status().as_u16(), 419);
        assert_eq!(
            run(&csrf.clone().allow_same_site(true), request("same-site")).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            run(&csrf.trust_fetch_metadata(false), request("same-origin")).await.status().as_u16(),
            419
        );
    }

    #[tokio::test]
    async fn excepted_paths_pass() {
        let csrf = VerifyCsrfToken::new().except(["/webhooks/*", "api/ping"]);
        let post = |uri: &str| Request::builder().method(Method::POST).uri(uri).build();

        assert_eq!(run(&csrf, post("/webhooks/stripe")).await.status(), StatusCode::OK);
        assert_eq!(run(&csrf, post("/api/ping")).await.status(), StatusCode::OK);
        assert_eq!(run(&csrf, post("/api/ping/more")).await.status().as_u16(), 419);
        assert_eq!(run(&csrf, post("/webhook")).await.status().as_u16(), 419);
    }

    #[tokio::test]
    async fn without_a_session_it_refuses_rather_than_waving_through() {
        let response = Pipeline::new()
            .through(VerifyCsrfToken::new())
            .then(|_: Request| async { Response::text("handled") })
            .run(
                Request::builder()
                    .method(Method::POST)
                    .uri("/x")
                    .header("x-csrf-token", "t")
                    .build(),
            )
            .await;
        assert_eq!(response.status().as_u16(), 419);
    }

    struct Reversed;

    impl XsrfCodec for Reversed {
        fn seal(&self, token: &str) -> Result<String> {
            Ok(token.chars().rev().collect())
        }

        fn open(&self, header: &str) -> Option<String> {
            Some(header.chars().rev().collect())
        }
    }

    #[tokio::test]
    async fn a_codec_seals_the_cookie_and_opens_the_header() {
        let csrf = VerifyCsrfToken::new().codec(Arc::new(Reversed));
        let response = post(csrf, |id| {
            Request::builder()
                .method(Method::POST)
                .uri("/x")
                .header("cookie", &cookie_header(&id))
                .header("x-xsrf-token", "nekot-eht")
                .build()
        })
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let set: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert!(set.iter().any(|c| c.starts_with("XSRF-TOKEN=nekot-eht")), "{set:?}");
    }

    #[tokio::test]
    async fn not_minting_on_reads_leaves_an_anonymous_visitor_without_a_session() {
        let response =
            run(&VerifyCsrfToken::new().mint_on_read(false), Request::builder().uri("/").build())
                .await;
        assert!(response.header("set-cookie").is_none());
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("a/*", "a/b/c"));
        assert!(wildcard("*", ""));
        assert!(wildcard("a*c", "abbbc"));
        assert!(!wildcard("a*c", "abbbd"));
        assert!(wildcard("a", "a"));
        assert!(!wildcard("a", "ab"));
    }

    #[test]
    fn constant_time_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
