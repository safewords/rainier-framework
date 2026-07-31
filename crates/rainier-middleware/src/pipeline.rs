//! The request pipeline — [`Middleware`], [`Next`] and [`Pipeline`].
//!
//! The middleware signature is `handle(request, next)`, and that shape is the
//! whole point: middleware sits *around* the rest of the chain rather than
//! merely before it, so one piece of middleware can inspect the response,
//! wrap the call in a transaction or a timer, or decline to call `next` at
//! all.
//!
//! ```
//! use rainier_http::{Request, Response, StatusCode};
//! use rainier_middleware::{Middleware, Next};
//!
//! struct BlockRobots;
//!
//! #[async_trait::async_trait]
//! impl Middleware for BlockRobots {
//!     async fn handle(&self, request: Request, next: Next) -> Response {
//!         if request.header("user-agent").is_some_and(|ua| ua.contains("bot")) {
//!             return Response::new(StatusCode::FORBIDDEN);   // short-circuit
//!         }
//!         let response = next.run(request).await;            // …or continue
//!         response.with_header("x-checked", "yes")           // …and adjust
//!     }
//! }
//! ```

use std::any::TypeId;
use std::sync::Arc;

use rainier_http::{Request, Response};
use rainier_support::BoxedFuture;

/// The concrete type behind a `dyn Middleware`.
///
/// A supertrait with a blanket impl rather than a defaulted method, because a
/// provided method returning `TypeId::of::<Self>()` needs `Self: Sized` and
/// would therefore not land in the vtable — where a `dyn Middleware` is the
/// only place it is ever wanted.
///
/// This is what makes `Route::without_middleware::<T>()` possible without
/// going back to comparing names.
///
/// The blanket impl is bound on [`Middleware`] rather than on
/// `Send + Sync + 'static`, and that is load-bearing. A wider bound would also
/// cover `Arc<dyn Middleware>`, and then `arc.concrete_type_id()` would resolve
/// to the `Arc`'s own type before auto-deref reached the trait object —
/// answering `TypeId::of::<Arc<dyn Middleware>>()` for every stage, so every
/// comparison matches everything. Narrowing it means that call cannot compile
/// against the wrapper and must go through the vtable, which is the only place
/// the real answer lives.
pub trait ConcreteMiddleware: Send + Sync + 'static {
    /// The `TypeId` of the implementing type.
    fn concrete_type_id(&self) -> TypeId;

    /// Its fully-qualified type name.
    fn concrete_type_name(&self) -> &'static str;
}

impl<T: Middleware> ConcreteMiddleware for T {
    fn concrete_type_id(&self) -> TypeId {
        TypeId::of::<T>()
    }

    fn concrete_type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }
}

/// A stage in the request pipeline.
#[async_trait::async_trait]
pub trait Middleware: ConcreteMiddleware {
    /// Handle the request, optionally calling `next`.
    ///
    /// Returning without calling `next.run(..)` short-circuits the pipeline —
    /// the handler and every later middleware are skipped. That is how
    /// authentication, rate limiting and CORS preflight all work.
    async fn handle(&self, request: Request, next: Next) -> Response;

    /// A label for diagnostics and `route:list`.
    ///
    /// Defaults to the type's short name. This is a **label**, not an
    /// identifier: nothing looks middleware up by it, so two stages sharing one
    /// is untidy rather than broken.
    fn name(&self) -> &'static str {
        short_type_name(self.concrete_type_name())
    }
}

/// `a::b::Thing<x::y::Z>` → `Thing<Z>`.
///
/// `route:list` is a table; a column of fully-qualified paths makes it
/// unreadable and tells you nothing the short name does not.
pub(crate) fn short_type_name(full: &'static str) -> &'static str {
    // Generic parameters are dropped along with the path, which is what keeps
    // `Authenticate<app::models::User>` from being wider than the terminal.
    let without_generics = full.split_once('<').map_or(full, |(head, _)| head);
    without_generics.rsplit("::").next().unwrap_or(without_generics)
}

/// The end of the pipeline — the thing that actually produces a response.
///
/// The router implements this; the pipeline itself has no idea what a route
/// is, which is what keeps this crate independent of `rainier-routing`.
pub trait Destination: Send + Sync + 'static {
    /// Produce the response.
    fn call(&self, request: Request) -> BoxedFuture<Response>;
}

/// Any async closure is a destination — useful for tests and for one-off
/// pipelines.
impl<F, Fut> Destination for F
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    fn call(&self, request: Request) -> BoxedFuture<Response> {
        Box::pin(self(request))
    }
}

/// The remainder of the pipeline, handed to each middleware.
pub struct Next {
    stages: Arc<[Arc<dyn Middleware>]>,
    index: usize,
    destination: Arc<dyn Destination>,
}

impl Next {
    /// Continue: run the next middleware, or the destination if this was the
    /// last one.
    pub fn run(self, request: Request) -> BoxedFuture<Response> {
        let Some(stage) = self.stages.get(self.index).cloned() else {
            return self.destination.call(request);
        };

        let next =
            Next { stages: self.stages, index: self.index + 1, destination: self.destination };

        // The `Arc` is moved into the future so the borrow of `*stage` that
        // `handle` takes stays alive for as long as the future does.
        Box::pin(async move { stage.handle(request, next).await })
    }

    /// How many stages are still to run, excluding the destination.
    pub fn remaining(&self) -> usize {
        self.stages.len().saturating_sub(self.index)
    }
}

impl std::fmt::Debug for Next {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Next").field("remaining", &self.remaining()).finish()
    }
}

/// Builds and runs a pipeline: a request, through some middleware, to a
/// destination.
///
/// ```
/// # use rainier_http::{Request, Response};
/// # use rainier_middleware::Pipeline;
/// # #[tokio::main] async fn main() {
/// let response = Pipeline::new()
///     .then(|_req: Request| async { Response::text("hello") })
///     .run(Request::builder().build())
///     .await;
///
/// assert_eq!(response.status(), 200);
/// # }
/// ```
#[derive(Default)]
pub struct Pipeline {
    stages: Vec<Arc<dyn Middleware>>,
}

impl Pipeline {
    /// An empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one middleware.
    pub fn through(mut self, middleware: impl Middleware) -> Self {
        self.stages.push(Arc::new(middleware));
        self
    }

    /// Append an already-shared middleware.
    pub fn through_arc(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.stages.push(middleware);
        self
    }

    /// Append several already-shared middleware, in order.
    pub fn through_all(
        mut self,
        middleware: impl IntoIterator<Item = Arc<dyn Middleware>>,
    ) -> Self {
        self.stages.extend(middleware);
        self
    }

    /// Close the pipeline with its destination.
    pub fn then(self, destination: impl Destination) -> ReadyPipeline {
        self.then_arc(Arc::new(destination))
    }

    /// [`then`](Self::then) with an already-shared destination.
    pub fn then_arc(self, destination: Arc<dyn Destination>) -> ReadyPipeline {
        ReadyPipeline { stages: self.stages.into(), destination }
    }

    /// How many stages are registered.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Whether the pipeline has no middleware.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

/// A pipeline with its destination attached, ready to run — and cheap to
/// re-run, which is what lets the router build one per route at boot rather
/// than per request.
#[derive(Clone)]
pub struct ReadyPipeline {
    stages: Arc<[Arc<dyn Middleware>]>,
    destination: Arc<dyn Destination>,
}

impl ReadyPipeline {
    /// Send a request through.
    pub async fn run(&self, request: Request) -> Response {
        Next {
            stages: Arc::clone(&self.stages),
            index: 0,
            destination: Arc::clone(&self.destination),
        }
        .run(request)
        .await
    }

    /// The names of the middleware, in order.
    pub fn stage_names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.name()).collect()
    }
}

impl std::fmt::Debug for ReadyPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyPipeline").field("stages", &self.stage_names()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::StatusCode;
    use std::sync::Mutex;

    /// Records when it runs, on the way in and on the way out.
    struct Trace {
        tag: &'static str,
        log: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Middleware for Trace {
        async fn handle(&self, request: Request, next: Next) -> Response {
            self.log.lock().unwrap().push(format!("{}:before", self.tag));
            let response = next.run(request).await;
            self.log.lock().unwrap().push(format!("{}:after", self.tag));
            response
        }

        fn name(&self) -> &'static str {
            "Trace"
        }
    }

    struct ShortCircuit;

    #[async_trait::async_trait]
    impl Middleware for ShortCircuit {
        async fn handle(&self, _request: Request, _next: Next) -> Response {
            Response::new(StatusCode::FORBIDDEN)
        }
    }

    fn log() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    #[tokio::test]
    async fn an_empty_pipeline_reaches_the_destination() {
        let response = Pipeline::new()
            .then(|_: Request| async { Response::text("hi") })
            .run(Request::builder().build())
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_wraps_the_destination_in_order() {
        let log = log();
        let recorded = Arc::clone(&log);

        let destination_log = Arc::clone(&log);
        let response = Pipeline::new()
            .through(Trace { tag: "outer", log: Arc::clone(&log) })
            .through(Trace { tag: "inner", log: Arc::clone(&log) })
            .then(move |_: Request| {
                let log = Arc::clone(&destination_log);
                async move {
                    log.lock().unwrap().push("handler".into());
                    Response::text("ok")
                }
            })
            .run(Request::builder().build())
            .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            *recorded.lock().unwrap(),
            vec!["outer:before", "inner:before", "handler", "inner:after", "outer:after"]
        );
    }

    #[tokio::test]
    async fn short_circuiting_skips_the_rest_and_the_handler() {
        let log = log();
        let recorded = Arc::clone(&log);

        let response = Pipeline::new()
            .through(Trace { tag: "outer", log: Arc::clone(&log) })
            .through(ShortCircuit)
            .through(Trace { tag: "never", log: Arc::clone(&log) })
            .then(|_: Request| async { Response::text("unreachable") })
            .run(Request::builder().build())
            .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        // The outer middleware still sees the response on the way out.
        assert_eq!(*recorded.lock().unwrap(), vec!["outer:before", "outer:after"]);
    }

    #[tokio::test]
    async fn middleware_can_rewrite_the_request_and_the_response() {
        struct Tag;

        #[async_trait::async_trait]
        impl Middleware for Tag {
            async fn handle(&self, mut request: Request, next: Next) -> Response {
                request.merge_input(serde_json::json!({ "injected": "yes" }));
                next.run(request).await.with_header("x-tagged", "1")
            }
        }

        let response =
            Pipeline::new()
                .through(Tag)
                .then(|request: Request| async move {
                    Response::text(request.input_or("injected", "no"))
                })
                .run(Request::builder().build())
                .await;

        assert_eq!(response.header("x-tagged"), Some("1"));
        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "yes");
    }

    #[tokio::test]
    async fn a_ready_pipeline_can_be_run_repeatedly() {
        let pipeline = Pipeline::new()
            .through(Trace { tag: "a", log: log() })
            .then(|_: Request| async { Response::text("ok") });

        for _ in 0..3 {
            assert_eq!(pipeline.run(Request::builder().build()).await.status(), StatusCode::OK);
        }
        assert_eq!(pipeline.stage_names(), vec!["Trace"]);
    }

    #[tokio::test]
    async fn next_reports_how_much_is_left() {
        struct CountRemaining(Arc<Mutex<Vec<usize>>>);

        #[async_trait::async_trait]
        impl Middleware for CountRemaining {
            async fn handle(&self, request: Request, next: Next) -> Response {
                self.0.lock().unwrap().push(next.remaining());
                next.run(request).await
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        Pipeline::new()
            .through(CountRemaining(Arc::clone(&seen)))
            .through(CountRemaining(Arc::clone(&seen)))
            .then(|_: Request| async { Response::no_content() })
            .run(Request::builder().build())
            .await;

        assert_eq!(*seen.lock().unwrap(), vec![1, 0]);
    }
}
