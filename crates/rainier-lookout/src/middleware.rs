//! [`LookoutMiddleware`]: what a request's batch learns from the request and
//! the response rather than from spans.
//!
//! Global middleware runs inside the kernel's `http.server.request` span, so
//! the batch is already open when it starts. Before the work it applies the
//! request-side scope — the dashboard's own path is never recorded — and takes
//! the opt-in captures (headers; the body, kept only if the batch ends up with
//! a failure). After the response it reads:
//!
//! - the matched route from the response's extensions
//!   ([`MatchedRoute`](rainier_routing::MatchedRoute), which the router puts
//!   there) — and runs the route stage of the scope, if the router's span
//!   record had not already;
//! - the status;
//! - the guard and user, through [`Lookout::identify_with`].
//!
//! Guard and user scope are evaluated when the batch closes.
//!
//! **Without the server's `telemetry` feature** there is no server span, so
//! the middleware opens an `http.server.request` span itself, with the same
//! fields. Requests are then recorded from this middleware inward: whatever
//! global middleware runs before it is outside the batch.
//!
//! **Middleware groups** are not part of the scope here: nothing on the
//! request or response says which group a route was registered in.

use rainier_http::{Request, Response};
use rainier_middleware::{Middleware, Next};

use crate::recorder::Lookout;

/// The middleware. Get one from [`Lookout::middleware`].
#[derive(Debug, Clone)]
pub struct LookoutMiddleware {
    lookout: Lookout,
}

impl LookoutMiddleware {
    pub(crate) fn new(lookout: Lookout) -> Self {
        Self { lookout }
    }
}

#[async_trait::async_trait]
impl Middleware for LookoutMiddleware {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let inner = &self.lookout.inner;
        if !inner.active() {
            return next.run(request).await;
        }

        if inner.current_batch().is_some() {
            inner.before_request(&request);
            let path = request.path().to_string();
            let response = next.run(request).await;
            inner.after_response(&response);
            return self.lookout.mark_for_rum(response, &path);
        }

        // No server span around us: open one, so the request has a batch.
        let method = request.method().as_str().to_string();
        let span = tracing::info_span!(
            "http.server.request",
            otel.name = %method,
            http.request.method = %method,
            url.path = %request.path(),
            http.route = tracing::field::Empty,
            rainier.route.name = tracing::field::Empty,
            http.response.status_code = tracing::field::Empty,
            trace_id = tracing::field::Empty,
        );
        let lookout = self.lookout.clone();
        let work = async move {
            lookout.inner.before_request(&request);
            let path = request.path().to_string();
            let response = next.run(request).await;
            lookout.inner.after_response(&response);
            lookout.mark_for_rum(response, &path)
        };
        let response = tracing::Instrument::instrument(work, span.clone()).await;
        span.record("http.response.status_code", response.status().as_u16());
        response
    }

    fn name(&self) -> &'static str {
        "Lookout"
    }
}

/// The client's address, once the application has decided which one to
/// believe. Get one from [`Lookout::client_middleware`].
///
/// [`LookoutMiddleware`] runs outermost, so it sees every request — including
/// those a throttle turns away — but it also sees the socket's peer, which
/// behind a proxy is the proxy. The application's own `TrustProxies` replaces
/// that with the visitor further in. This runs innermost of the global
/// middleware, after it, and hands Lookout the address the application
/// settled on. A request turned away before reaching it keeps the peer.
#[derive(Debug, Clone)]
pub struct LookoutClientMiddleware {
    lookout: Lookout,
}

impl LookoutClientMiddleware {
    pub(crate) fn new(lookout: Lookout) -> Self {
        Self { lookout }
    }
}

#[async_trait::async_trait]
impl Middleware for LookoutClientMiddleware {
    async fn handle(&self, request: Request, next: Next) -> Response {
        self.lookout.inner.refine_client(&request);
        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "LookoutClient"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rainier_events::{Dispatcher, Reporter};
    use rainier_http::{Method, Request, Response};
    use rainier_routing::Router;
    use rainier_server::Kernel;
    use tracing_subscriber::layer::SubscriberExt;

    use crate::config::{Patterns, Scope};
    use crate::entry::{BatchKind, EntryType};
    use crate::store::memory::MemoryStore;
    use crate::{Lookout, LookoutConfig};

    fn get(uri: &str) -> Request {
        Request::builder().method(Method::GET).uri(uri).build()
    }

    /// A real kernel and router, with the server's `telemetry` spans on, and
    /// Lookout's middleware in the global stack.
    fn kernel(lookout: &Lookout, events: Arc<Dispatcher>) -> Kernel {
        let mut router = Router::new();
        router
            .get("/posts/{post}", || async {
                let span = tracing::info_span!(
                    "db.query",
                    db.query.text = "SELECT * FROM posts WHERE id = ?"
                );
                let _e = span.enter();
                "post"
            })
            .name("posts.show");
        router.get("/health", || async { "ok" }).name("health");
        {
            // What an application's authentication path does: say who it is.
            let lookout = lookout.clone();
            router
                .get("/me", move || {
                    let lookout = lookout.clone();
                    async move {
                        lookout.describe_user(
                            None,
                            crate::LookoutUser::new("42")
                                .name("Ada Lovelace")
                                .email("ada@example.com")
                                .with("plan", "pro"),
                        );
                        "me"
                    }
                })
                .name("me");
        }
        {
            // What an application does once it knows what the session is:
            // here, the profile the visitor is acting as.
            let lookout = lookout.clone();
            router
                .get("/acting", move || {
                    let lookout = lookout.clone();
                    async move {
                        lookout.describe_session(None, "profile", "ada");
                        "acting"
                    }
                })
                .name("acting");
        }
        router
            .post("/sessions", || async {
                Response::json(&serde_json::json!({
                    "user": {"name": "ada"},
                    "access_token": "abc",
                    "session_ticket": "Zq8mN2pL7vR4tY9wK3xB6cD1eF0gH5jT",
                    "opaque": "Zq8mN2pL7vR4tY9wK3xB6cD1eF0gH5jT",
                }))
            })
            .name("sessions.store");
        router.get("/boom", || async {
            Err::<Response, _>(rainier_support::Error::internal("db password is hunter2"))
        });
        let compiled =
            router.compile(&rainier_container::Container::new()).expect("routes compile");
        Kernel::with_middleware(compiled, vec![Arc::new(lookout.middleware())])
            .with_reporter(Reporter::new(events))
    }

    #[tokio::test]
    async fn the_middleware_attaches_route_status_and_user_through_a_real_kernel() {
        let mut config = LookoutConfig::default().enabled(true).local(true).scope(Scope {
            routes: Patterns { include: vec![], exclude: vec!["health".into()] },
            ..LookoutConfig::default().scope
        });
        config.capture.user_id = true;
        config.capture.request_headers = true;
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config, store.clone());
        lookout.identify_with(|response| {
            response
                .headers()
                .get("x-user")
                .map(|u| ("session".to_string(), u.to_str().ok().map(str::to_string)))
        });
        let events = Arc::new(Dispatcher::new());
        lookout.listen(&events);
        let kernel = kernel(&lookout, Arc::clone(&events));
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));

        let ok = kernel
            .handle_request(
                Request::builder()
                    .method(Method::GET)
                    .uri("/posts/42")
                    .header("authorization", "Bearer secret")
                    .header("accept", "text/html")
                    .build(),
            )
            .await;
        assert_eq!(ok.status().as_u16(), 200);
        kernel.handle_request(get("/health")).await;
        let failed = kernel.handle_request(get("/boom")).await;
        assert_eq!(failed.status().as_u16(), 500);
        lookout.flush().await;

        let batches = store.all();
        let summaries: Vec<&str> = batches.iter().map(|b| b.summary.as_str()).collect();
        assert_eq!(summaries, vec!["GET /posts/{post} 200", "GET /boom 500"], "{batches:#?}");

        let post = &batches[0];
        assert_eq!(post.kind, BatchKind::Request);
        assert_eq!(post.route_name.as_deref(), Some("posts.show"));
        assert_eq!(post.status, Some(200));
        let types: Vec<EntryType> = post.entries.iter().map(|e| e.entry_type).collect();
        assert_eq!(types, vec![EntryType::Request, EntryType::Query]);
        let headers = &post.entries[0].content["request.headers"];
        assert_eq!(headers["authorization"], crate::redact::REDACTED);
        assert_eq!(headers["accept"], "text/html");

        // The failure was reported after the server span closed, and joined
        // its request's batch anyway.
        let boom = &batches[1];
        assert!(boom.has_exception);
        assert_eq!(boom.route_pattern.as_deref(), Some("/boom"));
        let exception =
            boom.entries.iter().find(|e| e.entry_type == EntryType::Exception).expect("exception");
        assert_eq!(exception.content["type"], "HttpRequest");
    }

    #[tokio::test]
    async fn who_and_what_made_a_request_and_how_its_browser_saw_it() {
        let mut config = LookoutConfig::default().enabled(true).local(true);
        config.capture.user_id = true;
        config.capture.user = true;
        config.capture.client = true;
        config.rum.enabled = true;
        config.rum.origins = vec!["https://app.example.com".into()];
        config.rum.secret = b"test-key".to_vec();
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config.clone(), store.clone());
        let kernel = kernel(&lookout, Arc::new(Dispatcher::new()));
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));

        let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1";
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/me")
            .header("user-agent", ua)
            .header("origin", "https://app.example.com")
            .header("referer", "https://app.example.com/settings?tab=security")
            .header("accept-language", "en-GB,en;q=0.8")
            .build();
        request.extensions_mut().insert(rainier_http::ClientIp("203.0.113.77".parse().unwrap()));
        let answered = kernel.handle_request(request).await;
        assert_eq!(answered.status().as_u16(), 200);
        lookout.flush().await;

        // The response names its batch, signed, for the browser to report on.
        let timing = answered.header("server-timing").expect("a Server-Timing header").to_string();
        assert_eq!(answered.header("timing-allow-origin"), Some("https://app.example.com"));
        // `lookout;dur=…;desc="<token>", lookout-route;desc="me"`: the
        // server's own time and the route ride along with the token.
        assert!(timing.starts_with("lookout;dur="), "{timing}");
        assert!(timing.contains(", lookout-route;desc=\"me\""), "{timing}");
        // And who it was for, signed, so the browser's reports name them.
        assert!(
            timing.contains(&format!(
                ", lookout-user;desc=\"{}\"",
                crate::rum::user_token(b"test-key", "42")
            )),
            "{timing}"
        );
        let token = timing
            .split("desc=\"")
            .nth(1)
            .and_then(|t| t.split('"').next())
            .expect("the lookout metric")
            .to_string();

        let batches = store.all();
        let me = &batches[0];
        assert_eq!(me.user_id.as_deref(), Some("42"), "the described user is the batch's user");
        assert_eq!(crate::rum::verify(b"test-key", &token).as_deref(), Some(me.trace_id.as_str()));
        let root = &me.entries[0].content;
        assert_eq!(root["user"]["name"], "Ada Lovelace", "{root}");
        assert_eq!(root["user"]["email"], "ada@example.com");
        assert_eq!(root["user"]["extra"]["plan"], "pro");
        let client = &root["client"];
        assert_eq!(client["ip"], "203.0.113.0/24", "{client}");
        assert_eq!(client["browser"]["name"], "Safari");
        assert_eq!(client["os"]["name"], "iOS");
        assert_eq!(client["device"], "mobile");
        assert_eq!(client["referer"], "https://app.example.com/settings");
        assert_eq!(client["language"], "en-GB");

        // The page reports how the browser saw it, and the report joins the
        // request's trace as a browser batch.
        let mut router = Router::new();
        router.merge(crate::routes(
            store.clone(),
            Arc::new(config),
            crate::LookoutGate::deny_all(),
            "production",
        ));
        let router = router.compile(&rainier_container::Container::new()).unwrap();
        let script = router.dispatch(get("/_lookout/rum.js")).await;
        assert_eq!(script.status().as_u16(), 200, "public, though the dashboard is gated");
        // Asked about on every page load, and answered with no body while
        // unchanged: a CDN must not keep an old script for days.
        assert_eq!(script.header("cache-control"), Some("private, no-cache"));
        let etag = script.header("etag").expect("an etag").to_string();
        let again = router
            .dispatch(
                Request::builder()
                    .method(Method::GET)
                    .uri("/_lookout/rum.js")
                    .header("if-none-match", &etag)
                    .build(),
            )
            .await;
        assert_eq!(again.status().as_u16(), 304);
        let report = serde_json::json!({
            "page": { "url": "https://app.example.com/settings" },
            "device": { "connection": { "type": "4g" } },
            "calls": [{ "token": token, "url": "https://api.example.com/me", "duration": 412.5,
                        "waiting": 180.0, "protocol": "h2", "status": 200 }],
        })
        .to_string();
        let post = |origin: &str| {
            Request::builder()
                .method(Method::POST)
                .uri("/_lookout/rum")
                .header("origin", origin)
                .header("content-type", "text/plain")
                .body(report.clone())
                .build()
        };
        assert_eq!(router.dispatch(post("https://evil.example")).await.status().as_u16(), 204);
        // Lookout's own responses carry no token: the script would report
        // its reports otherwise, one after another.
        let own = kernel.handle_request(get("/_lookout/rum.js")).await;
        assert!(own.header("server-timing").is_none(), "no token on Lookout's own path");
        assert_eq!(store.all().len(), 1, "an origin not listed stores nothing");
        assert_eq!(router.dispatch(post("https://app.example.com")).await.status().as_u16(), 204);
        let all = store.all();
        let browser = all.iter().find(|b| b.kind == BatchKind::Browser).expect("a browser batch");
        assert_eq!(browser.trace_id, me.trace_id, "on the request's trace");
        assert_eq!(browser.duration_us, 412_500);
        assert_eq!(browser.entries[0].content["timing"]["waiting"], 180.0);
    }

    #[tokio::test]
    async fn a_browser_report_pairs_with_the_request_it_describes_in_its_session_and_context() {
        use crate::store::Store as _;
        // Production's rule: only slow or failed requests are kept.
        let mut config = LookoutConfig::default().enabled(true).local(false);
        config.capture.user_id = true;
        config.rum.enabled = true;
        config.rum.origins = vec!["https://app.example.com".into()];
        config.rum.secret = b"test-key".to_vec();
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config.clone(), store.clone());
        let kernel = kernel(&lookout, Arc::new(Dispatcher::new()));
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));

        let answered = kernel
            .handle_request(
                Request::builder()
                    .method(Method::GET)
                    .uri("/acting")
                    .header("origin", "https://app.example.com")
                    .header("cookie", "app_session=s3cr3t-cookie-value")
                    .build(),
            )
            .await;
        lookout.flush().await;
        assert!(store.all().is_empty(), "fast and fine: not kept");

        let timing = answered.header("server-timing").unwrap().to_string();
        let token =
            timing.split("desc=\"").nth(1).and_then(|t| t.split('"').next()).unwrap().to_string();
        let context = timing
            .split("lookout-context;desc=\"")
            .nth(1)
            .and_then(|t| t.split('"').next())
            .expect("the session's context rides along, signed")
            .to_string();
        assert!(context.starts_with("profile=ada."), "{context}");
        assert!(
            !timing.contains("lookout-session"),
            "the session is the browser's, not the server's"
        );
        assert!(!timing.contains("s3cr3t"), "never the cookie");
        // The browser's own session, as the script keeps it.
        let session = "fedcba9876543210fedcba9876543210";
        let counted_before: u64 = store
            .aggregate_rows(EntryType::Request, 60, chrono::Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap()
            .iter()
            .map(|r| r.count)
            .sum();
        assert_eq!(counted_before, 1, "counted though not kept");

        // The page reports the call: the held request is kept and paired.
        let mut router = Router::new();
        router.merge(crate::routes(
            store.clone(),
            Arc::new(config),
            crate::LookoutGate::deny_all(),
            "production",
        ));
        let router = router.compile(&rainier_container::Container::new()).unwrap();
        let report = serde_json::json!({
            "view": "0123456789abcdef0123456789abcdef",
            "session": session,
            "context": context,
            "page": { "url": "https://app.example.com/acting" },
            "calls": [{ "token": token, "url": "https://api.example.com/acting", "duration": 90.0, "status": 200 }],
        })
        .to_string();
        let posted = router
            .dispatch(
                Request::builder()
                    .method(Method::POST)
                    .uri("/_lookout/rum")
                    .header("origin", "https://app.example.com")
                    .body(report)
                    .build(),
            )
            .await;
        assert_eq!(posted.status().as_u16(), 204);

        let all = store.all();
        let server =
            all.iter().find(|b| b.kind == BatchKind::Request).expect("the request, kept now");
        let browser =
            all.iter().find(|b| b.kind == BatchKind::Browser).expect("the browser's report");
        assert_eq!(server.trace_id, browser.trace_id, "one trace: each links to the other");
        assert!(server.tags.contains(&crate::rum::TAG_PAIRED.to_string()));
        let session_tag = format!("session:{session}");
        assert!(
            server.tags.contains(&session_tag) && browser.tags.contains(&session_tag),
            "one session"
        );
        assert!(
            server.tags.contains(&"profile:ada".to_string())
                && browser.tags.contains(&"profile:ada".to_string()),
            "labelled with the profile, on both sides"
        );
        let counted_after: u64 = store
            .aggregate_rows(EntryType::Request, 60, chrono::Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap()
            .iter()
            .map(|r| r.count)
            .sum();
        assert_eq!(counted_after, 1, "paired, not counted twice");
    }

    #[tokio::test]
    async fn the_client_is_the_visitor_the_application_trusts_not_its_proxy() {
        let mut config = LookoutConfig::default().enabled(true).local(true);
        config.capture.client = true;
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config, store.clone());
        let mut router = Router::new();
        router.get("/hello", || async { "hi" }).name("hello");
        let compiled = router.compile(&rainier_container::Container::new()).unwrap();
        // The order the framework builds: Lookout outermost, the
        // application's proxy trust, then Lookout's client hook innermost.
        let kernel = Kernel::with_middleware(
            compiled,
            vec![
                Arc::new(lookout.middleware()),
                Arc::new(
                    rainier_middleware::TrustProxies::these(["10.0.0.0/8"])
                        .header("cf-connecting-ip"),
                ),
                Arc::new(lookout.client_middleware()),
            ],
        );
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));

        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/hello")
            .header("cf-connecting-ip", "198.51.100.23")
            .build();
        // The socket's peer: the ingress, inside the cluster.
        request.extensions_mut().insert(rainier_http::ClientIp("10.42.2.7".parse().unwrap()));
        assert_eq!(kernel.handle_request(request).await.status().as_u16(), 200);
        lookout.flush().await;

        let batches = store.all();
        let client = &batches[0].entries[0].content["client"];
        assert_eq!(client["ip"], "198.51.100.0/24", "the visitor, not the ingress: {client}");
    }

    #[tokio::test]
    async fn request_and_response_bodies_are_kept_redacted_when_captured() {
        let mut config = LookoutConfig::default().enabled(true).local(true);
        config.capture.request_body = true;
        config.capture.response_body = true;
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config, store.clone());
        let kernel = kernel(&lookout, Arc::new(Dispatcher::new()));
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));

        let answered = kernel
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/sessions")
                    .json(
                        &serde_json::json!({"email": "ada@example.com", "new_password": "hunter2"}),
                    )
                    .build(),
            )
            .await;
        assert_eq!(answered.status().as_u16(), 200);
        lookout.flush().await;

        let batches = store.all();
        let root = &batches[0].entries[0].content;
        let sent = &root["request.body"];
        assert_eq!(sent["email"], "ada@example.com", "{root}");
        assert_eq!(sent["new_password"], crate::redact::REDACTED, "a pattern hides it");
        let got = &root["response.body"];
        assert_eq!(got["user"]["name"], "ada", "{root}");
        assert_eq!(got["access_token"], crate::redact::REDACTED);
        assert_eq!(got["session_ticket"], crate::redact::REDACTED);
        assert!(got["opaque"].as_str().unwrap().starts_with("Zq8m***"), "masked: {got}");
    }

    #[tokio::test]
    async fn a_route_named_in_bodies_except_keeps_no_body() {
        let mut config = LookoutConfig::default().enabled(true).local(true);
        config.capture.request_body = true;
        config.capture.response_body = true;
        config.capture.bodies_except = vec!["sessions.*".into()];
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config, store.clone());
        let kernel = kernel(&lookout, Arc::new(Dispatcher::new()));
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));
        kernel
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/sessions")
                    .json(&serde_json::json!({"email": "ada@example.com"}))
                    .build(),
            )
            .await;
        lookout.flush().await;
        let batch = &store.all()[0];
        assert_eq!(batch.route_name.as_deref(), Some("sessions.store"));
        let root = &batch.entries[0].content;
        assert!(
            root.get("request.body").is_none() && root.get("response.body").is_none(),
            "{root}"
        );
    }

    #[tokio::test]
    async fn bodies_are_not_kept_unless_asked_for() {
        let config = LookoutConfig::default().enabled(true).local(true);
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config, store.clone());
        let kernel = kernel(&lookout, Arc::new(Dispatcher::new()));
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));
        kernel
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/sessions")
                    .json(&serde_json::json!({"email": "ada@example.com"}))
                    .build(),
            )
            .await;
        lookout.flush().await;
        let root = &store.all()[0].entries[0].content;
        assert!(
            root.get("request.body").is_none() && root.get("response.body").is_none(),
            "{root}"
        );
    }

    #[tokio::test]
    async fn the_identify_hook_feeds_user_scope() {
        let mut config = LookoutConfig::default()
            .enabled(true)
            .local(true)
            .scope(Scope { users: ["42".to_string()].into(), ..LookoutConfig::default().scope });
        config.capture.user_id = true;
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config, store.clone());
        lookout.identify_with(|response| {
            let user = if response.status().as_u16() == 200 { "42" } else { "7" };
            Some(("session".to_string(), Some(user.to_string())))
        });
        let kernel = kernel(&lookout, Arc::new(Dispatcher::new()));
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));
        kernel.handle_request(get("/posts/1")).await;
        kernel.handle_request(get("/missing")).await;
        lookout.flush().await;
        let batches = store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].user_id.as_deref(), Some("42"));
        assert_eq!(batches[0].guard.as_deref(), Some("session"));
        assert!(batches[0].tags.contains(&"user:42".to_string()));
    }

    #[tokio::test]
    async fn without_a_server_span_the_middleware_opens_one() {
        use rainier_middleware::Pipeline;

        let store = Arc::new(MemoryStore::new(10));
        let lookout =
            Lookout::new(LookoutConfig::default().enabled(true).local(true), store.clone());
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(lookout.layer()));
        let pipeline = Pipeline::new().through(lookout.middleware()).then(|_request: Request| {
            Box::pin(async {
                let mut response = Response::new(rainier_http::StatusCode::OK);
                response.extensions_mut().insert(rainier_routing::MatchedRoute {
                    name: Some("home".into()),
                    uri: "/".into(),
                    methods: vec![Method::GET],
                });
                response
            }) as rainier_support::BoxedFuture<Response>
        });
        pipeline.run(get("/")).await;
        lookout.flush().await;
        let batches = store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].summary, "GET / 200");
        assert_eq!(batches[0].route_name.as_deref(), Some("home"));
    }
}
