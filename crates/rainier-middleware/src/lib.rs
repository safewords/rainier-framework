//! # rainier-middleware
//!
//! Middleware as values: the [`Middleware`] contract, the [`Pipeline`] that
//! runs a chain of it, and the [`MiddlewareStack`] a route or a group attaches.
//!
//! ```
//! use rainier_http::{Request, Response, StatusCode};
//! use rainier_middleware::{Middleware, Next, Pipeline};
//!
//! struct RequireApiKey;
//!
//! #[async_trait::async_trait]
//! impl Middleware for RequireApiKey {
//!     async fn handle(&self, request: Request, next: Next) -> Response {
//!         match request.header("x-api-key") {
//!             Some("secret") => next.run(request).await,
//!             _ => Response::new(StatusCode::UNAUTHORIZED),
//!         }
//!     }
//! }
//!
//! # #[tokio::main] async fn main() {
//! let pipeline = Pipeline::new()
//!     .through(RequireApiKey)
//!     .then(|_: Request| async { Response::text("secrets") });
//!
//! let denied = pipeline.run(Request::builder().build()).await;
//! assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
//!
//! let allowed = pipeline.run(Request::builder().header("x-api-key", "secret").build()).await;
//! assert_eq!(allowed.status(), StatusCode::OK);
//! # }
//! ```
//!
//! ## No names anywhere
//!
//! A route attaches `.middleware(Authenticate::new(auth))`, not
//! `.middleware("auth")`. A name-keyed kernel needs the string because a
//! dynamic language has nowhere to put the type; Rust does not, and the
//! indirection costs more than it saves —
//! a misspelled alias is a route that runs unguarded, and only an integration
//! test notices.
//!
//! The router still does not depend on `rainier-auth` or `rainier-session`. It
//! only ever sees `dyn Middleware`, which is defined here; the **application**
//! names the concrete type, because the application is the crate that knows it.
//!
//! | The name-based equivalent | Rainier |
//! |---|---|
//! | a route-middleware alias map | nothing — attach the value |
//! | a named group in a map | a function returning a [`MiddlewareStack`] |
//! | the global list | [`MiddlewareRegistry::global`] |
//! | a throttle spelled as a string | `ThrottleRequests::per_minute(60)` |
//! | excluding middleware by name | `.without_middleware::<Authenticate<User>>()` |
//!
//! The one thing a name genuinely bought — middleware that cannot be built
//! until the container is populated — is [`MiddlewareStack::deferred`], which
//! runs a closure when the router compiles.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod builtin;
pub mod compression;
pub mod method_override;
pub mod pipeline;
pub mod proxy;
pub mod rate_limit;
pub mod registry;
pub mod stack;
pub mod timeout;

pub use builtin::{
    AddHeaders, AllowedOrigins, ConvertEmptyStringsToNull, HandleCors, SharedThrottle,
    ThrottleRequests, TrimStrings,
};
pub use compression::Compress;
pub use method_override::MethodOverride;
pub use pipeline::{ConcreteMiddleware, Destination, Middleware, Next, Pipeline, ReadyPipeline};
pub use proxy::{Cidr, TrustProxies, Trusted};
pub use rate_limit::{Hit, MemoryRateLimitStore, RateLimitStore};
pub use registry::MiddlewareRegistry;
pub use stack::{IntoMiddlewareStack, MiddlewareStack};
pub use timeout::Timeout;

// Re-exported so middleware implementors get the attribute macro without
// adding the dependency themselves.
pub use async_trait::async_trait;
