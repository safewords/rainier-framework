//! # rainier-routing
//!
//! Route declaration and dispatch: the [`Router`] DSL, [`Route`] patterns and
//! constraints, [`GroupAttributes`] for shared prefixes and middleware,
//! [`ResourceController`] for RESTful resources, and [`UrlGenerator`] for
//! building URLs back out of named routes.
//!
//! ```
//! use rainier_container::Container;
//! use rainier_http::{Method, Request};
//! use rainier_routing::{GroupAttributes, Router, UrlGenerator};
//!
//! async fn index() -> &'static str { "posts" }
//! async fn show(request: rainier_routing::Req) -> String {
//!     format!("post {}", request.route_param("post").unwrap_or("?"))
//! }
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let mut router = Router::new();
//!
//! router.group(GroupAttributes::new().prefix("api").name("api."), |router| {
//!     router.get("/posts", index).name("posts.index");
//!     router.get("/posts/{post}", show).name("posts.show").where_number("post");
//! });
//!
//! let compiled = router.compile(&Container::new())?;
//! let urls = UrlGenerator::from_routes(compiled.named_routes());
//!
//! assert_eq!(urls.route("api.posts.show", &[("post", "7")])?, "/api/posts/7");
//!
//! let response = compiled
//!     .dispatch(Request::builder().method(Method::GET).uri("/api/posts/7").build())
//!     .await;
//! assert_eq!(response.status(), 200);
//! # Ok(()) }
//! ```
//!
//! ## Two phases
//!
//! [`Router`] is the *declaration*; [`CompiledRouter`] is the *runtime*.
//! Compiling flattens each route's group and own middleware into one pipeline,
//! builds it once, and rejects duplicate route names at boot rather than on the
//! first request that hits them.
//!
//! Middleware is attached **by value** — `.middleware(Authenticate::new(auth))`,
//! not `.middleware("auth")`. There is no registry to look a name up in and
//! nothing to misspell; see
//! [`MiddlewareStack`](rainier_middleware::MiddlewareStack). Compiling is also
//! where a middleware that needs the container is built, so that failure lands
//! at boot too.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod handler;
pub mod resource;
pub mod route;
pub mod router;
pub mod url;

pub use handler::{Handler, IntoRouteHandler, Req, RouteHandler};
pub use resource::{ActionName, ControllerMiddleware, ResourceAction, ResourceController};
pub use route::{ParamConstraint, Route, Segment};
pub use router::{not_found, CompiledRouter, GroupAttributes, MatchedRoute, RouteSummary, Router};
pub use url::UrlGenerator;

// Re-exported so controllers get the attribute macro without adding the
// dependency themselves.
pub use async_trait::async_trait;
