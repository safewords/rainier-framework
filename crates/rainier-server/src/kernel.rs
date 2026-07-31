//! The HTTP [`Kernel`] — global middleware, dispatch, and error rendering.
//!
//! The kernel is where the global middleware stack lives and
//! where an exception becomes a response. It is one seam: everything
//! between "a request exists" and "a response exists", with the transport on
//! one side and the router on the other.

use std::sync::Arc;

use rainier_http::{http, RenderedError, Request, Response, StatusCode};
use rainier_middleware::{Destination, Middleware, MiddlewareRegistry, Pipeline, ReadyPipeline};
use rainier_routing::CompiledRouter;
use rainier_support::{BoxedFuture, Error, Result};

/// Turns an error into the response a client sees.
///
/// A port, because the right answer differs per application: an API returns
/// JSON, a monolith returns a styled error page, and both want to decide for
/// themselves what a 500 discloses.
pub trait ExceptionRenderer: Send + Sync + 'static {
    /// Render `error` for `request`.
    fn render(&self, request: &Request, error: &RenderedError, debug: bool) -> Response;
}

/// The renderer Rainier ships: JSON for API clients, a plain HTML page for
/// browsers.
#[derive(Debug, Default)]
pub struct DefaultExceptionRenderer;

impl ExceptionRenderer for DefaultExceptionRenderer {
    fn render(&self, request: &Request, error: &RenderedError, debug: bool) -> Response {
        let status =
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        // A 5xx message routinely contains a connection string, a file path or
        // a query. Outside debug mode the client gets a generic sentence and
        // the real one goes to the log.
        let message = if error.disclosable || debug {
            error.message.clone()
        } else {
            "Server Error".to_string()
        };

        if request.expects_json() {
            let mut payload = serde_json::Map::new();
            payload.insert("message".into(), serde_json::Value::String(message));
            if let Some(details) = &error.details {
                payload.insert("errors".into(), details.clone());
            }
            return Response::json(&serde_json::Value::Object(payload)).with_status(status);
        }

        let reason = status.canonical_reason().unwrap_or("Error");
        let body = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">\
             <title>{status} {reason}</title></head>\
             <body style=\"font-family:system-ui,sans-serif;margin:4rem auto;max-width:40rem\">\
             <h1>{status} {reason}</h1><p>{}</p></body></html>",
            rainier_view_escape(&message)
        );
        Response::html(body).with_status(status)
    }
}

/// Escape the characters that could break out of the error page's markup.
///
/// A local copy rather than a dependency on the view crate: the kernel must be
/// able to render an error even in an application that has no view layer.
fn rainier_view_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Dispatches into the compiled router. The pipeline's terminus.
struct RouterDestination(Arc<CompiledRouter>);

impl Destination for RouterDestination {
    fn call(&self, request: Request) -> BoxedFuture<Response> {
        let router = Arc::clone(&self.0);
        Box::pin(async move { router.dispatch(request).await })
    }
}

/// The HTTP kernel.
pub struct Kernel {
    pipeline: ReadyPipeline,
    renderer: Arc<dyn ExceptionRenderer>,
    debug: bool,
}

impl Kernel {
    /// A kernel serving `router`, with no global middleware.
    pub fn new(router: CompiledRouter) -> Self {
        Self::with_middleware(router, Vec::new())
    }

    /// A kernel serving `router` behind `global` middleware, outermost first.
    pub fn with_middleware(router: CompiledRouter, global: Vec<Arc<dyn Middleware>>) -> Self {
        Self::from_shared(Arc::new(router), global)
    }

    /// A kernel over a router that is already shared.
    ///
    /// `CompiledRouter` is not `Clone` — it owns each route's pipeline — so
    /// this is how the kernel and the container hold the *same* table rather
    /// than the application compiling it twice.
    pub fn from_shared(router: Arc<CompiledRouter>, global: Vec<Arc<dyn Middleware>>) -> Self {
        let destination: Arc<dyn Destination> = Arc::new(RouterDestination(router));
        Self {
            pipeline: Pipeline::new().through_all(global).then_arc(destination),
            renderer: Arc::new(DefaultExceptionRenderer),
            debug: false,
        }
    }

    /// A kernel taking its global middleware from a registry.
    ///
    /// Fallible because a global stage may be
    /// [resolved from the container](rainier_middleware::MiddlewareStack::resolved),
    /// and middleware that runs on every request is not something to carry on
    /// without.
    pub fn from_registry(
        router: CompiledRouter,
        registry: &MiddlewareRegistry,
        container: &rainier_container::Container,
    ) -> rainier_support::Result<Self> {
        Ok(Self::with_middleware(router, registry.global_stack(container)?))
    }

    /// Render errors with `renderer`.
    pub fn with_renderer(mut self, renderer: Arc<dyn ExceptionRenderer>) -> Self {
        self.renderer = renderer;
        self
    }

    /// Disclose internal error messages and panic details to the client.
    ///
    /// Never in production: the messages this reveals are exactly the ones an
    /// attacker would like to read.
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Whether debug mode is on.
    pub fn is_debug(&self) -> bool {
        self.debug
    }

    /// Handle one request.
    ///
    /// A panic in a handler is caught here and becomes a `500`. One request
    /// bringing down the process — or, worse, poisoning a shared lock and
    /// taking every later request with it — is not an acceptable failure mode
    /// for a web framework.
    pub async fn handle(&self, request: Request) -> Response {
        let expects_json = request.expects_json();
        let path = request.path().to_string();

        let outcome = std::panic::AssertUnwindSafe(self.pipeline.run(request));
        let response = match futures_catch_unwind(outcome).await {
            Ok(response) => response,
            Err(panic) => {
                let detail = panic_message(&panic);
                tracing::error!(path = %path, panic = %detail, "a handler panicked");

                let error = RenderedError {
                    status: 500,
                    message: if self.debug {
                        format!("panic: {detail}")
                    } else {
                        "Server Error".to_string()
                    },
                    details: None,
                    disclosable: false,
                };
                // The request was consumed by the pipeline, so render against
                // a stand-in that preserves only what the renderer needs.
                let stand_in = if expects_json {
                    Request::builder().header("accept", "application/json").build()
                } else {
                    Request::builder().build()
                };
                return self.renderer.render(&stand_in, &error, self.debug);
            }
        };

        response
    }

    /// Handle a request, re-rendering framework errors through the renderer.
    ///
    /// Split from [`handle`](Self::handle) because the re-render needs the
    /// request, which the pipeline consumed. The server calls this one.
    pub async fn handle_request(&self, request: Request) -> Response {
        // Snapshot what the renderer needs before the pipeline takes the
        // request.
        let expects_json = request.expects_json();
        let response = self.handle(request).await;

        let Some(error) = response.extensions().get::<RenderedError>().cloned() else {
            return response;
        };

        if error.disclosable && expects_json {
            // Already JSON, already safe to show: nothing to re-render.
            return response;
        }

        let stand_in = if expects_json {
            Request::builder().header("accept", "application/json").build()
        } else {
            Request::builder().build()
        };

        if !error.disclosable {
            tracing::error!(status = error.status, error = %error.message, "server error");
        }

        let rendered = self.renderer.render(&stand_in, &error, self.debug);
        carry_over_headers(&response, rendered)
    }
}

/// Copy headers from the original error response onto the re-rendered one,
/// without overwriting anything the renderer set.
///
/// Re-rendering replaces the whole response, and some headers are part of the
/// error's *meaning* rather than its body: a `405` is required to carry
/// `Allow`, and a throttled `429` is useless without `Retry-After`. Dropping
/// them would turn a correct response into a subtly broken one.
fn carry_over_headers(original: &Response, mut rendered: Response) -> Response {
    for (name, value) in original.headers() {
        // The renderer decides the body, so it decides these.
        if name == http::header::CONTENT_TYPE || name == http::header::CONTENT_LENGTH {
            continue;
        }
        if rendered.headers().contains_key(name) {
            continue;
        }
        rendered.headers_mut().append(name.clone(), value.clone());
    }
    rendered
}

impl std::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernel")
            .field("middleware", &self.pipeline.stage_names())
            .field("debug", &self.debug)
            .finish()
    }
}

/// `catch_unwind` for a future.
///
/// `std::panic::catch_unwind` wraps a closure, not a future, so a panic after
/// the first `await` would escape it. Polling inside the guard is what makes
/// the whole future covered.
async fn futures_catch_unwind<F>(
    future: F,
) -> std::result::Result<F::Output, Box<dyn std::any::Any + Send>>
where
    F: std::future::Future,
{
    use std::pin::pin;
    use std::task::Poll;

    let mut future = pin!(future);
    std::future::poll_fn(move |context| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            future.as_mut().poll(context)
        })) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic) => Poll::Ready(Err(panic)),
        }
    })
    .await
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = panic.downcast_ref::<String>() {
        return message.clone();
    }
    "a handler panicked".to_string()
}

/// Read a request body into memory, refusing anything over `limit`.
///
/// The limit is the only thing standing between the server and a client that
/// streams gigabytes at it, because request bodies are buffered (see
/// [`rainier_http::body`]).
pub async fn read_body<B>(body: B, limit: usize) -> Result<bytes::Bytes>
where
    B: hyper::body::Body,
    B::Error: std::fmt::Display,
{
    use http_body_util::BodyExt;

    // Refuse up front when the sender declared a size over the limit, rather
    // than reading it all to find out.
    let hint = body.size_hint();
    if hint.lower() > limit as u64 {
        return Err(Error::new(
            rainier_support::ErrorKind::PayloadTooLarge,
            format!("the request body exceeds the {limit}-byte limit"),
        ));
    }

    let collected = body
        .collect()
        .await
        .map_err(|e| Error::bad_request(format!("could not read the request body: {e}")))?;
    let bytes = collected.to_bytes();

    // And again after reading: a chunked body can lie about its size.
    if bytes.len() > limit {
        return Err(Error::new(
            rainier_support::ErrorKind::PayloadTooLarge,
            format!("the request body exceeds the {limit}-byte limit"),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::Method;
    use rainier_middleware::Next;
    use rainier_routing::Router;

    async fn body_of(response: Response) -> String {
        String::from_utf8(response.into_http().into_body().collect().await.unwrap().to_vec())
            .unwrap()
    }

    fn kernel_for(router: Router) -> Kernel {
        Kernel::new(router.compile(&rainier_container::Container::new()).expect("compiles"))
    }

    fn get(uri: &str) -> Request {
        Request::builder().method(Method::GET).uri(uri).build()
    }

    fn api_get(uri: &str) -> Request {
        Request::builder().method(Method::GET).uri(uri).header("accept", "application/json").build()
    }

    #[tokio::test]
    async fn dispatches_to_a_route() {
        let mut router = Router::new();
        router.get("/hello", || async { "world" });

        let response = kernel_for(router).handle_request(get("/hello")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "world");
    }

    #[tokio::test]
    async fn global_middleware_wraps_every_route() {
        struct Tag;
        #[async_trait::async_trait]
        impl Middleware for Tag {
            async fn handle(&self, request: Request, next: Next) -> Response {
                next.run(request).await.with_header("x-global", "1")
            }
        }

        let mut router = Router::new();
        router.get("/a", || async { "a" });
        let compiled = router.compile(&rainier_container::Container::new()).unwrap();

        let kernel = Kernel::with_middleware(compiled, vec![Arc::new(Tag)]);
        assert_eq!(kernel.handle_request(get("/a")).await.header("x-global"), Some("1"));
    }

    #[tokio::test]
    async fn a_missing_route_renders_html_for_a_browser() {
        let response = kernel_for(Router::new()).handle_request(get("/nope")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.header("content-type"), Some("text/html; charset=utf-8"));
        assert!(body_of(response).await.contains("404"));
    }

    #[tokio::test]
    async fn a_missing_route_renders_json_for_an_api_client() {
        let response = kernel_for(Router::new()).handle_request(api_get("/nope")).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.header("content-type"), Some("application/json; charset=utf-8"));
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert!(body["message"].as_str().unwrap().contains("/nope"));
    }

    #[tokio::test]
    async fn an_internal_error_does_not_leak_its_message() {
        let mut router = Router::new();
        router.get("/boom", || async {
            Err::<&'static str, Error>(Error::internal(
                "postgres://user:hunter2@db.internal/app is unreachable",
            ))
        });

        let response = kernel_for(router).handle_request(api_get("/boom")).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = body_of(response).await;
        assert!(!body.contains("hunter2"), "the connection string leaked: {body}");
        assert!(body.contains("Server Error"), "{body}");
    }

    #[tokio::test]
    async fn debug_mode_discloses_the_internal_message() {
        let mut router = Router::new();
        router.get("/boom", || async {
            Err::<&'static str, Error>(Error::internal("the real reason"))
        });

        let kernel = kernel_for(router).with_debug(true);
        let body = body_of(kernel.handle_request(api_get("/boom")).await).await;
        assert!(body.contains("the real reason"), "{body}");
    }

    #[tokio::test]
    async fn a_client_error_message_is_always_shown() {
        // A 4xx says what the *client* did wrong; hiding it helps nobody.
        let mut router = Router::new();
        router.get("/bad", || async {
            Err::<&'static str, Error>(Error::bad_request("the `page` parameter must be a number"))
        });

        let body = body_of(kernel_for(router).handle_request(api_get("/bad")).await).await;
        assert!(body.contains("must be a number"), "{body}");
    }

    #[tokio::test]
    async fn validation_details_survive_the_re_render() {
        let mut router = Router::new();
        router.get("/form", || async {
            Err::<&'static str, Error>(Error::validation(
                serde_json::json!({ "email": ["is required"] }),
            ))
        });

        let response = kernel_for(router).handle_request(api_get("/form")).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(body["errors"]["email"][0], "is required");
    }

    // Named functions with explicit return types: an `async` block whose body
    // only diverges has output type `!`, which cannot satisfy `IntoResponse`.
    async fn panics_immediately() -> &'static str {
        panic!("something went very wrong")
    }

    async fn panics_after_awaiting() -> &'static str {
        tokio::task::yield_now().await;
        panic!("after the await")
    }

    #[tokio::test]
    async fn a_panicking_handler_becomes_a_500_rather_than_taking_the_process_down() {
        let mut router = Router::new();
        router.get("/panic", panics_immediately);

        // Silence the default panic output so the test log stays readable.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let response = kernel_for(router).handle_request(api_get("/panic")).await;
        std::panic::set_hook(previous);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_of(response).await;
        assert!(!body.contains("very wrong"), "a panic message must not leak: {body}");
    }

    #[tokio::test]
    async fn a_panic_after_an_await_is_still_caught() {
        // The reason `catch_unwind` has to wrap each poll rather than the
        // whole call: a panic after the first suspension point would otherwise
        // escape it entirely.
        let mut router = Router::new();
        router.get("/panic", panics_after_awaiting);

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let response = kernel_for(router).handle_request(api_get("/panic")).await;
        std::panic::set_hook(previous);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn re_rendering_an_error_keeps_the_headers_that_carry_its_meaning() {
        // Regression guard: a 405 is required to carry `Allow`, and the
        // re-render used to replace the whole response and lose it.
        let mut router = Router::new();
        router.post("/login", || async { "ok" });

        let request = Request::builder().method(Method::DELETE).uri("/login").build();
        let response = kernel_for(router).handle_request(request).await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            response.header("allow").is_some_and(|allow| allow.contains("POST")),
            "the Allow header must survive the re-render"
        );
    }

    #[tokio::test]
    async fn the_renderer_still_decides_the_content_type() {
        let mut router = Router::new();
        router.get("/bad", || async { Err::<&'static str, Error>(Error::bad_request("nope")) });

        // The error response was JSON; a browser must still get HTML.
        let response = kernel_for(router).handle_request(get("/bad")).await;
        assert_eq!(response.header("content-type"), Some("text/html; charset=utf-8"));
    }

    #[tokio::test]
    async fn a_successful_response_passes_through_untouched() {
        let mut router = Router::new();
        router.get("/ok", || async { Response::json(&serde_json::json!({ "a": 1 })) });

        let response = kernel_for(router).handle_request(api_get("/ok")).await;
        assert_eq!(body_of(response).await, r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn a_custom_renderer_takes_over() {
        struct Terse;
        impl ExceptionRenderer for Terse {
            fn render(&self, _: &Request, error: &RenderedError, _: bool) -> Response {
                Response::text(format!("oops {}", error.status))
                    .with_status(StatusCode::from_u16(error.status).unwrap())
            }
        }

        let kernel = kernel_for(Router::new()).with_renderer(Arc::new(Terse));
        assert_eq!(body_of(kernel.handle_request(get("/nope")).await).await, "oops 404");
    }

    #[tokio::test]
    async fn html_error_pages_escape_their_message() {
        let mut router = Router::new();
        router.get("/bad", || async {
            Err::<&'static str, Error>(Error::bad_request("<script>alert(1)</script>"))
        });

        let body = body_of(kernel_for(router).handle_request(get("/bad")).await).await;
        assert!(!body.contains("<script>"), "{body}");
        assert!(body.contains("&lt;script&gt;"), "{body}");
    }

    // --- body reading ------------------------------------------------------

    fn full_body(bytes: &'static str) -> http_body_util::Full<bytes::Bytes> {
        http_body_util::Full::new(bytes::Bytes::from_static(bytes.as_bytes()))
    }

    #[tokio::test]
    async fn reads_a_body_within_the_limit() {
        let bytes = read_body(full_body("hello"), 1024).await.unwrap();
        assert_eq!(bytes, bytes::Bytes::from("hello"));
    }

    #[tokio::test]
    async fn refuses_a_body_over_the_limit() {
        let err = read_body(full_body("hello world"), 4).await.unwrap_err();
        assert_eq!(err.status(), 413);
        assert!(err.message().contains("limit"), "{}", err.message());
    }

    #[tokio::test]
    async fn an_empty_body_is_fine() {
        assert!(read_body(full_body(""), 1024).await.unwrap().is_empty());
    }
}
