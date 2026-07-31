//! Route handlers — how an ordinary `async fn` becomes something the router
//! can store and call.
//!
//! Handlers differ in shape (`async fn() -> Response`,
//! `async fn(Json<Post>, Path<u64>) -> Result<Response>`, …) but the router
//! needs one uniform type in its table. Two traits bridge that:
//!
//! - [`Handler`] is implemented for every async function whose parameters are
//!   [`FromRequest`] and whose return value is [`IntoResponse`]. Its `Args`
//!   type parameter is what lets one blanket impl per arity coexist — without
//!   it, `impl Handler for F where F: Fn(A)` and `impl Handler for F where
//!   F: Fn(A, B)` would overlap.
//! - [`RouteHandler`] is the object-safe form the router actually stores.
//!
//! ```
//! use rainier_http::{extract::{Json, Path}, IntoResponse, Response};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct NewComment { body: String }
//!
//! // Both of these are handlers, with no wrapper and no attribute macro.
//! async fn index() -> &'static str {
//!     "every post"
//! }
//!
//! async fn store(Path(post): Path<u64>, Json(comment): Json<NewComment>) -> Response {
//!     Response::text(format!("comment on {post}: {}", comment.body))
//! }
//! ```

use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use rainier_http::{FromRequest, IntoResponse, Request, Response};
use rainier_support::BoxedFuture;

/// A shared request, as handlers receive it.
///
/// Extractors each take a clone of this `Arc`, so a handler can ask for the
/// whole request *and* typed pieces of it without any of them fighting over
/// ownership.
pub type Req = Arc<Request>;

/// The object-safe handler the router stores.
pub trait RouteHandler: Send + Sync + 'static {
    /// Handle the request.
    fn call(&self, request: Request) -> BoxedFuture<Response>;
}

/// An `async fn` usable as a handler, with `Args` distinguishing its arity.
///
/// `Clone` is required because extraction is `async`: the handler has to
/// survive being moved into a `'static` future that awaits its extractors
/// before calling it. Function items and closures over `Clone` captures
/// satisfy this for free.
pub trait Handler<Args>: Clone + Send + Sync + 'static {
    /// Extract the arguments and run.
    fn handle(&self, request: Req) -> BoxedFuture<Response>;
}

/// Wraps a [`Handler`] as a [`RouteHandler`], erasing its `Args`.
pub struct HandlerService<H, Args> {
    handler: H,
    /// `fn() -> Args` rather than `Args`, so the wrapper is `Send + Sync`
    /// whatever the argument types are — it never holds one.
    _args: PhantomData<fn() -> Args>,
}

impl<H, Args> HandlerService<H, Args> {
    /// Wrap a handler.
    pub fn new(handler: H) -> Self {
        Self { handler, _args: PhantomData }
    }
}

impl<H, Args> RouteHandler for HandlerService<H, Args>
where
    H: Handler<Args>,
    Args: 'static,
{
    fn call(&self, request: Request) -> BoxedFuture<Response> {
        self.handler.handle(Arc::new(request))
    }
}

/// Turns anything handler-shaped into the stored form.
pub trait IntoRouteHandler<Args> {
    /// Erase into a [`RouteHandler`].
    fn into_route_handler(self) -> Arc<dyn RouteHandler>;
}

impl<H, Args> IntoRouteHandler<Args> for H
where
    H: Handler<Args>,
    Args: 'static,
{
    fn into_route_handler(self) -> Arc<dyn RouteHandler> {
        Arc::new(HandlerService::new(self))
    }
}

/// A handler taking no arguments.
impl<F, Fut, Res> Handler<()> for F
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Res> + Send + 'static,
    Res: IntoResponse + 'static,
{
    fn handle(&self, _request: Req) -> BoxedFuture<Response> {
        let handler = self.clone();
        Box::pin(async move { handler().await.into_response() })
    }
}

/// Implements [`Handler`] for one arity.
///
/// Extraction runs left to right and **short-circuits**: the first extractor
/// that fails becomes the response, and the later ones never run. That matters
/// for ordering — putting a `FormRequest` (which authorises) before an
/// expensive extractor means the expensive one is skipped on a rejection.
macro_rules! impl_handler {
    ($($arg:ident),+) => {
        #[allow(non_snake_case)]
        impl<F, Fut, Res, $($arg,)+> Handler<($($arg,)+)> for F
        where
            F: Fn($($arg,)+) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Res> + Send + 'static,
            Res: IntoResponse + 'static,
            $($arg: FromRequest,)+
        {
            fn handle(&self, request: Req) -> BoxedFuture<Response> {
                let handler = self.clone();
                Box::pin(async move {
                    $(
                        let $arg = match $arg::from_request(Arc::clone(&request)).await {
                            Ok(value) => value,
                            Err(e) => return e.into_response(),
                        };
                    )+
                    handler($($arg,)+).await.into_response()
                })
            }
        }
    };
}

impl_handler!(A1);
impl_handler!(A1, A2);
impl_handler!(A1, A2, A3);
impl_handler!(A1, A2, A3, A4);
impl_handler!(A1, A2, A3, A4, A5);
impl_handler!(A1, A2, A3, A4, A5, A6);
impl_handler!(A1, A2, A3, A4, A5, A6, A7);
impl_handler!(A1, A2, A3, A4, A5, A6, A7, A8);

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::extract::{Json, Path, Query};
    use rainier_http::StatusCode;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    struct NewPost {
        title: String,
    }

    #[derive(Deserialize)]
    struct Page {
        page: u32,
    }

    async fn body_of(response: Response) -> String {
        String::from_utf8(response.into_http().into_body().collect().await.unwrap().to_vec())
            .unwrap()
    }

    async fn run(handler: Arc<dyn RouteHandler>, request: Request) -> Response {
        handler.call(request).await
    }

    #[tokio::test]
    async fn a_nullary_handler_works() {
        async fn index() -> &'static str {
            "listing"
        }

        let response = run(index.into_route_handler(), Request::builder().build()).await;
        assert_eq!(body_of(response).await, "listing");
    }

    #[tokio::test]
    async fn a_handler_receives_the_whole_request() {
        async fn show(request: Req) -> String {
            request.path().to_string()
        }

        let response =
            run(show.into_route_handler(), Request::builder().uri("/here").build()).await;
        assert_eq!(body_of(response).await, "/here");
    }

    #[tokio::test]
    async fn extractors_are_filled_in_order() {
        async fn store(Path(id): Path<u64>, Json(post): Json<NewPost>) -> String {
            format!("{id}:{}", post.title)
        }

        let request =
            Request::builder().route_param("post", "7").json(&json!({ "title": "Hello" })).build();

        let response = run(store.into_route_handler(), request).await;
        assert_eq!(body_of(response).await, "7:Hello");
    }

    #[tokio::test]
    async fn a_failed_extraction_becomes_the_response() {
        async fn store(Json(post): Json<NewPost>) -> String {
            post.title
        }

        // No body at all, so `Json` fails with a 400.
        let response = run(store.into_route_handler(), Request::builder().build()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_failed_extraction_skips_the_later_ones() {
        static REACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

        struct Tripwire;
        impl FromRequest for Tripwire {
            fn from_request(_: Req) -> BoxedFuture<rainier_support::Result<Self>> {
                REACHED.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { Ok(Tripwire) })
            }
        }

        async fn store(Json(_): Json<NewPost>, _: Tripwire) -> &'static str {
            "ok"
        }

        let response = run(store.into_route_handler(), Request::builder().build()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            !REACHED.load(std::sync::atomic::Ordering::SeqCst),
            "extraction must stop at the first failure"
        );
    }

    #[tokio::test]
    async fn handlers_may_return_a_result() {
        async fn show() -> rainier_support::Result<String> {
            Err(rainier_support::Error::not_found("no such post"))
        }

        let response = run(show.into_route_handler(), Request::builder().build()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_closure_capturing_state_is_a_handler() {
        let greeting = Arc::new(String::from("hi"));
        let handler = move || {
            let greeting = Arc::clone(&greeting);
            async move { greeting.to_string() }
        };

        let response = run(handler.into_route_handler(), Request::builder().build()).await;
        assert_eq!(body_of(response).await, "hi");
    }

    #[tokio::test]
    async fn several_extractors_compose() {
        async fn search(Query(page): Query<Page>, request: Req) -> String {
            format!("{}#{}", request.path(), page.page)
        }

        let response =
            run(search.into_route_handler(), Request::builder().uri("/s?page=3").build()).await;
        assert_eq!(body_of(response).await, "/s#3");
    }
}
