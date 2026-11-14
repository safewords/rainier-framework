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
            let response = next.run(request).await;
            inner.after_response(&response);
            return response;
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
            let response = next.run(request).await;
            lookout.inner.after_response(&response);
            response
        };
        let response = tracing::Instrument::instrument(work, span.clone()).await;
        span.record("http.response.status_code", response.status().as_u16());
        response
    }

    fn name(&self) -> &'static str {
        "Lookout"
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
