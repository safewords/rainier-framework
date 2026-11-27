//! [`LookoutMiddleware`]: what a request's batch learns from the request and
//! the response rather than from the bus.
//!
//! Global middleware runs inside the request's unit of work, which the server
//! opens, so the batch is there when it starts. Before the work it applies
//! the request-side scope — the dashboard's own path is never recorded — and
//! takes the opt-in captures (headers; the body, kept only if the batch ends
//! up with a failure). After the response it reads:
//!
//! - the matched route from the response's extensions
//!   ([`MatchedRoute`](rainier_routing::MatchedRoute), which the router puts
//!   there) — and runs the route stage of the scope;
//! - the status;
//! - the guard and user, through [`Lookout::identify_with`].
//!
//! Guard and user scope are evaluated when the batch closes.
//!
//! This runs on the request's own thread, and writes into the batch directly:
//! the batch is the unit's, whichever thread gets to it first opens it.
//!
//! **With no unit of work around it** — a kernel that opens none — the
//! middleware opens the request's unit itself, on the bus Lookout subscribed
//! to, and publishes its `http.server.request` activity when the response is
//! ready. Requests are then recorded from this middleware inward: whatever
//! global middleware runs before it is outside the batch.
//!
//! **Middleware groups** are not part of the scope here: nothing on the
//! request or response says which group a route was registered in.

use std::time::{Duration, Instant};

use rainier_events::{Activity, Context, Fields, Unit, UnitKind};
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

/// What a calling service said about keeping this trace, in its
/// `tracestate`: `Off` for a caller that does not take part.
fn asked_to_keep(request: &Request) -> rainier_events::TraceKeeping {
    request
        .header("tracestate")
        .map_or(rainier_events::TraceKeeping::Off, rainier_events::TraceKeeping::from_tracestate)
}

#[async_trait::async_trait]
impl Middleware for LookoutMiddleware {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let inner = &self.lookout.inner;
        if !inner.active() {
            return next.run(request).await;
        }

        if Context::current_unit().is_some() {
            inner.before_request(&request);
            let path = request.path().to_string();
            let asked = asked_to_keep(&request);
            let response = next.run(request).await;
            inner.after_response(&response);
            let response = inner.answer_trace(response, asked);
            return self.lookout.mark_for_rum(response, &path);
        }

        // No unit around us: open the request's, so it has a batch.
        let Some(events) = inner.events() else { return next.run(request).await };
        let method = request.method().as_str().to_string();
        let path = request.path().to_string();
        let unit = Unit::on(&events, UnitKind::Request, None, Some(path.clone()));
        let lookout = self.lookout.clone();
        let started = Instant::now();
        let work = async move {
            lookout.inner.before_request(&request);
            let asked = asked_to_keep(&request);
            let response = next.run(request).await;
            lookout.inner.after_response(&response);
            let response = lookout.inner.answer_trace(response, asked);
            let matched = response.extensions().get::<rainier_routing::MatchedRoute>();
            events.publish_activity(RequestHandled {
                method,
                path: path.clone(),
                route: matched.map(|m| m.uri.clone()),
                route_name: matched.and_then(|m| m.name.clone()),
                status: response.status().as_u16(),
                trace_id: Context::current_unit().and_then(|u| u.trace_id().map(str::to_string)),
                duration: started.elapsed(),
            });
            lookout.mark_for_rum(response, &path)
        };
        unit.scope(work).await
    }

    fn name(&self) -> &'static str {
        "Lookout"
    }
}

/// The `http.server.request` activity the middleware publishes for a request
/// whose unit it opened itself, with the fields the server's span carries.
#[derive(Debug, Clone)]
pub struct RequestHandled {
    /// The request method.
    pub method: String,
    /// The request path.
    pub path: String,
    /// The matched route's pattern.
    pub route: Option<String>,
    /// The matched route's name.
    pub route_name: Option<String>,
    /// The response status.
    pub status: u16,
    /// The trace, when the unit knew one.
    pub trace_id: Option<String>,
    /// How long the middleware and everything inside it took.
    pub duration: Duration,
}

impl Activity for RequestHandled {
    fn kind(&self) -> &'static str {
        "http.server.request"
    }

    fn fields(&self, fields: &mut Fields) {
        let name = match &self.route {
            Some(route) => format!("{} {route}", self.method),
            None => self.method.clone(),
        };
        fields
            .set("otel.name", name)
            .set("http.request.method", self.method.clone())
            .set("url.path", self.path.clone())
            .maybe("http.route", self.route.clone())
            .maybe("rainier.route.name", self.route_name.clone())
            .set("http.response.status_code", self.status)
            .maybe("trace_id", self.trace_id.clone());
        if self.status >= 500 {
            fields.set("otel.status_code", "ERROR").set("error.type", self.status.to_string());
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.status >= 500
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

    use crate::config::{Patterns, Scope};
    use crate::entry::{BatchKind, EntryType};
    use crate::store::memory::MemoryStore;
    use crate::{Lookout, LookoutConfig};

    fn get(uri: &str) -> Request {
        Request::builder().method(Method::GET).uri(uri).build()
    }

    /// A bus with Lookout subscribed to it, and a real kernel and router
    /// whose requests' units publish on it, with Lookout's middleware in the
    /// global stack.
    fn wired(lookout: &Lookout) -> (Kernel, Arc<Dispatcher>) {
        let events = Arc::new(Dispatcher::new());
        lookout.subscribe(&events);
        (kernel(lookout, Arc::clone(&events)), events)
    }

    /// Everything published so far heard, then written.
    async fn flushed(lookout: &Lookout, events: &Dispatcher) {
        events.settled().await;
        lookout.flush().await;
    }

    fn kernel(lookout: &Lookout, events: Arc<Dispatcher>) -> Kernel {
        let mut router = Router::new();
        router
            .get("/posts/{post}", || async {
                crate::testing::query("SELECT * FROM posts WHERE id = ?");
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
            .with_events(Arc::clone(&events))
            .with_reporter(Reporter::new(events))
    }

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

    /// A request from another service in trace [`TRACE`], answering its span
    /// `parent`, saying `state` about keeping it.
    fn called(uri: &str, parent: &str, state: Option<&str>) -> Request {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header("traceparent", &format!("00-{TRACE}-{parent}-01"));
        if let Some(state) = state {
            builder = builder.header("tracestate", state);
        }
        builder.build()
    }

    fn aggregating(rate: f64) -> LookoutConfig {
        let mut config = LookoutConfig::default().enabled(true).local(false);
        config.request_tracing.enabled = true;
        config.request_tracing.trace_sample_rate = rate;
        config
    }

    #[tokio::test]
    async fn with_request_tracing_a_caller_hears_whether_its_callee_kept_its_part() {
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(aggregating(0.0), store.clone());
        let (kernel, events) = wired(&lookout);

        let fine =
            kernel.handle_request(called("/posts/1", "000000000000000a", Some("rainier=on"))).await;
        let failed =
            kernel.handle_request(called("/boom", "000000000000000b", Some("rainier=on"))).await;
        let asked = kernel
            .handle_request(called("/posts/2", "000000000000000c", Some("rainier=keep")))
            .await;
        flushed(&lookout, &events).await;

        let answer = |response: &Response| {
            response.headers().get("traceresponse").map(|v| v.to_str().unwrap().to_string())
        };
        let fine = answer(&fine).expect("a participating caller is answered");
        assert!(fine.starts_with(&format!("00-{TRACE}-")) && fine.ends_with("-00"), "{fine}");
        assert!(answer(&failed).unwrap().ends_with("-01"), "a failure keeps its part");
        assert!(answer(&asked).unwrap().ends_with("-01"), "asked to keep, it keeps");

        let kept = store.all();
        let paths: Vec<Option<&str>> = kept.iter().map(|b| b.route_name.as_deref()).collect();
        assert_eq!(kept.len(), 2, "the fine one is let go: {paths:?}");
        for batch in &kept {
            assert_eq!(batch.trace_id, TRACE);
            assert!(batch.tags.iter().any(|t| t == "trace:kept"), "{:?}", batch.tags);
            assert!(batch.tags.iter().any(|t| t == "trace:distributed"), "{:?}", batch.tags);
            let root =
                batch.entries.iter().find(|e| e.span_id.as_deref() == Some(&batch.id)).unwrap();
            let parent = root.content["rainier.trace.parent_span_id"].as_str().unwrap();
            assert!(["000000000000000b", "000000000000000c"].contains(&parent), "{parent}");
        }
        // The answer names the batch it kept, so a caller can hang it under
        // its call.
        let failed_answer = answer(&failed).unwrap();
        assert!(kept.iter().any(|b| failed_answer.contains(&b.id)), "{failed_answer}");
    }

    #[tokio::test]
    async fn every_service_keeps_the_same_sampled_traces_and_says_nothing_unasked() {
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(aggregating(1.0), store.clone());
        let (kernel, events) = wired(&lookout);

        let response = kernel.handle_request(called("/posts/1", "000000000000000a", None)).await;
        flushed(&lookout, &events).await;

        assert!(response.headers().get("traceresponse").is_none(), "not asked, nothing said");
        assert_eq!(store.all().len(), 1, "in the sample: kept though fine");
    }

    /// Another service's kernel as a transport: what the client sends, it
    /// handles — on a task of its own, as another process would.
    struct InProcess(Arc<Kernel>);

    impl rainier_http_client::Transport for InProcess {
        fn send<'a>(
            &'a self,
            request: rainier_http_client::transport::OutboundRequest,
        ) -> rainier_support::BoxFuture<
            'a,
            rainier_support::Result<rainier_http_client::transport::RawResponse>,
        > {
            let kernel = Arc::clone(&self.0);
            rainier_http_client::telemetry::instrument(request, move |request| {
                Box::pin(async move {
                    let path = request.url.splitn(4, '/').nth(3).unwrap_or_default().to_string();
                    let mut builder =
                        Request::builder().method(Method::GET).uri(format!("/{path}"));
                    for (name, value) in &request.headers {
                        builder = builder.header(name, value);
                    }
                    let built = builder.build();
                    let response = tokio::spawn(async move { kernel.handle_request(built).await })
                        .await
                        .expect("the callee answers");
                    let headers = response
                        .headers()
                        .iter()
                        .filter_map(|(n, v)| {
                            Some((n.as_str().to_string(), v.to_str().ok()?.to_string()))
                        })
                        .collect();
                    Ok(rainier_http_client::transport::RawResponse {
                        status: response.status().as_u16(),
                        headers,
                        body: Vec::new(),
                    })
                })
            })
        }

        fn name(&self) -> &str {
            "in-process"
        }
    }

    #[tokio::test]
    async fn a_failure_two_services_down_keeps_the_whole_trace_and_it_fits_back_together() {
        // The callee: fails, so keeps its part and says so.
        let billing_store = Arc::new(MemoryStore::new(10));
        let billing = Lookout::new(aggregating(0.0), billing_store.clone());
        let (billing_kernel, billing_events) = wired(&billing);
        let billing_kernel = Arc::new(billing_kernel);

        // The caller: fine itself, and not in the sample — kept only because
        // the service it called kept its part.
        let orders_store = Arc::new(MemoryStore::new(10));
        let orders = Lookout::new(aggregating(0.0), orders_store.clone());
        let orders_events = Arc::new(Dispatcher::new());
        orders.subscribe(&orders_events);
        let mut router = Router::new();
        let transport: Arc<dyn rainier_http_client::Transport> =
            Arc::new(InProcess(billing_kernel));
        router
            .get("/checkout", move || {
                let transport = Arc::clone(&transport);
                async move {
                    let charged = rainier_http_client::Http::get("http://billing.test/boom")
                        .through(transport)
                        .send()
                        .await
                        .expect("sent");
                    format!("charged: {}", charged.status())
                }
            })
            .name("checkout");
        let compiled = router.compile(&rainier_container::Container::new()).unwrap();
        let orders_kernel = Kernel::with_middleware(compiled, vec![Arc::new(orders.middleware())])
            .with_events(Arc::clone(&orders_events));

        // From a browser: a trace, and nothing said about keeping it.
        let response =
            orders_kernel.handle_request(called("/checkout", "00000000000000ff", None)).await;
        assert_eq!(response.status().as_u16(), 200);
        flushed(&billing, &billing_events).await;
        flushed(&orders, &orders_events).await;

        let callee = billing_store.all();
        let caller = orders_store.all();
        assert_eq!((caller.len(), callee.len()), (1, 1), "both parts kept");
        let (caller, callee) = (&caller[0], &callee[0]);
        assert_eq!(caller.trace_id, TRACE);
        assert_eq!(callee.trace_id, TRACE, "one trace across both");
        assert!(caller.tags.iter().any(|t| t == "trace:kept"), "{:?}", caller.tags);

        // The stitch: the caller's call names the span the callee answered.
        let call = caller
            .entries
            .iter()
            .find(|e| e.entry_type == EntryType::HttpClient)
            .expect("the call is recorded");
        let sent = call.content["rainier.trace.span_id"].as_str().expect("the span it sent");
        let callee_root =
            callee.entries.iter().find(|e| e.span_id.as_deref() == Some(&callee.id)).unwrap();
        assert_eq!(callee_root.content["rainier.trace.parent_span_id"].as_str(), Some(sent));
        assert_eq!(call.content["rainier.trace.peer_kept"], true);
        assert_eq!(call.content["rainier.trace.peer_span_id"].as_str(), Some(callee.id.as_str()));

        // And the trace API says the same, without content.
        let shaped = crate::http::traces::shape(callee);
        assert_eq!(shaped["remote_parent_span_id"].as_str(), Some(sent));
        let shaped = crate::http::traces::shape(caller);
        let call = shaped["spans"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["type"] == "http_client")
            .unwrap();
        assert_eq!(call["call"]["span_id"].as_str(), Some(sent));
    }

    #[tokio::test]
    async fn without_request_tracing_nothing_changes() {
        let store = Arc::new(MemoryStore::new(10));
        let lookout =
            Lookout::new(LookoutConfig::default().enabled(true).local(false), store.clone());
        let (kernel, events) = wired(&lookout);

        let response = kernel
            .handle_request(called("/posts/1", "000000000000000a", Some("rainier=keep")))
            .await;
        flushed(&lookout, &events).await;

        assert!(response.headers().get("traceresponse").is_none());
        assert!(store.all().is_empty(), "fine, so not kept");
    }

    #[tokio::test]
    async fn every_request_is_counted_by_route_method_and_status_class_kept_or_not() {
        use crate::store::Store as _;
        // Production's rule: the fine requests are counted, not kept.
        let config = LookoutConfig::default().enabled(true).local(false);
        let store = Arc::new(MemoryStore::new(10));
        let lookout = Lookout::new(config, store.clone());
        let (kernel, events) = wired(&lookout);
        for uri in ["/health", "/health", "/posts/1", "/boom", "/nowhere"] {
            kernel.handle_request(get(uri)).await;
        }
        flushed(&lookout, &events).await;

        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        let rows = store.aggregate_rows(EntryType::RequestFacet, 60, since).await.unwrap();
        let mut facets: Vec<(String, u64)> =
            rows.iter().map(|r| (r.key.clone(), r.count)).collect();
        facets.sort();
        assert_eq!(
            facets,
            vec![
                ("GET 2xx health".to_string(), 2),
                ("GET 2xx posts.show".to_string(), 1),
                ("GET 4xx (no route)".to_string(), 1),
                ("GET 5xx /boom".to_string(), 1),
            ],
            "every request, kept or not, by its route's name or else its pattern"
        );
        assert!(store.all().len() < 5, "and most were not kept");
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
        let (kernel, events) = wired(&lookout);

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
        flushed(&lookout, &events).await;

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

        // The failure was reported by the kernel after the middleware had
        // seen the response, and joined its request's batch.
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
        let (kernel, events) = wired(&lookout);

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
        flushed(&lookout, &events).await;

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
        let (kernel, events) = wired(&lookout);

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
        flushed(&lookout, &events).await;
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
        let events = Arc::new(Dispatcher::new());
        lookout.subscribe(&events);
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
        )
        .with_events(Arc::clone(&events));

        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/hello")
            .header("cf-connecting-ip", "198.51.100.23")
            .build();
        // The socket's peer: the ingress, inside the cluster.
        request.extensions_mut().insert(rainier_http::ClientIp("10.42.2.7".parse().unwrap()));
        assert_eq!(kernel.handle_request(request).await.status().as_u16(), 200);
        flushed(&lookout, &events).await;

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
        let (kernel, events) = wired(&lookout);

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
        flushed(&lookout, &events).await;

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
        let (kernel, events) = wired(&lookout);
        kernel
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/sessions")
                    .json(&serde_json::json!({"email": "ada@example.com"}))
                    .build(),
            )
            .await;
        flushed(&lookout, &events).await;
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
        let (kernel, events) = wired(&lookout);
        kernel
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/sessions")
                    .json(&serde_json::json!({"email": "ada@example.com"}))
                    .build(),
            )
            .await;
        flushed(&lookout, &events).await;
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
        let (kernel, events) = wired(&lookout);
        kernel.handle_request(get("/posts/1")).await;
        kernel.handle_request(get("/missing")).await;
        flushed(&lookout, &events).await;
        let batches = store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].user_id.as_deref(), Some("42"));
        assert_eq!(batches[0].guard.as_deref(), Some("session"));
        assert!(batches[0].tags.contains(&"user:42".to_string()));
    }

    #[tokio::test]
    async fn without_a_unit_the_middleware_opens_one() {
        use rainier_middleware::Pipeline;

        let store = Arc::new(MemoryStore::new(10));
        let lookout =
            Lookout::new(LookoutConfig::default().enabled(true).local(true), store.clone());
        let events = Arc::new(Dispatcher::new());
        lookout.subscribe(&events);
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
        flushed(&lookout, &events).await;
        let batches = store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].summary, "GET / 200");
        let roots = batches[0].entries.iter().filter(|e| e.entry_type == EntryType::Request);
        assert_eq!(roots.count(), 1, "the middleware's own request entry, once");
        assert_eq!(batches[0].route_name.as_deref(), Some("home"));
    }
}
