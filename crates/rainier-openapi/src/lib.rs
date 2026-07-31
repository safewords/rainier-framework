//! An OpenAPI 3.1 document, built from the router and the request contracts.
//!
//! ```ignore
//! // config/openapi.rs decides whether it is served at all
//! pub fn document() -> OpenApi {
//!     OpenApi::new("Rainier Sample", "1.0.0")
//!         .describe("api.posts.store", Endpoint::new()
//!             .summary("Create a draft")
//!             .accepts(StorePostRequest::rules())
//!             .returns(201, "The created post"))
//! }
//!
//! // routes/api.rs
//! router.get("/openapi.json", openapi::serve).name("openapi");
//! ```
//!
//! # Half generated, half declared, and the split is the point
//!
//! **Generated** from the compiled router: every path, every method, the path
//! parameters, and a `401` on anything behind authentication. That half cannot
//! drift, because it is read from the routes being served.
//!
//! **Declared** per endpoint: the summary, the tags, the request body and the
//! responses. Rust erases a handler's parameter types by the time the router
//! holds one, so there is nothing to introspect — and guessing would produce a
//! document that is confidently wrong, which is worse than one that is plainly
//! incomplete.
//!
//! The declared half points at a route by **name**, and
//! [`dangling`](OpenApi::dangling) reports descriptions whose route no longer
//! exists. That is the one way this rots, and catching it is a one-line test.
//!
//! # The request body comes from the validator's own rules
//!
//! This is the part worth having. A hand-written OpenAPI file describes what
//! the endpoint accepted the last time somebody updated it;
//! [`Endpoint::accepts`] describes what the validator will accept, because it
//! is handed the same [`RuleSet`](rainier_validation::RuleSet) the validator
//! runs:
//!
//! ```ignore
//! Endpoint::new().accepts(StorePostRequest::rules())
//! ```
//!
//! Add a rule and the document changes. Delete a field and it leaves both at
//! once.
//!
//! # What is not here
//!
//! **No UI.** Swagger UI and Redoc are large JavaScript bundles, and serving
//! one from a Rust binary means either vendoring a megabyte or loading it from
//! a CDN — a third party on your documentation page. Point either at
//! `/openapi.json`; both take a URL.
//!
//! **No response schemas.** A response is whatever the handler serialises, and
//! nothing here can see that type. Describe it in
//! [`returns`](Endpoint::returns).
//!
//! **No derive macro.** An attribute on a handler could carry the summary, but
//! the request contract is a parameter type the router has already erased — so
//! it would move half the declaration and split it across two files.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod document;
pub mod endpoint;
pub mod schema;

pub use document::{Endpoint, OpenApi};
pub use endpoint::{serve, Rendered};
pub use schema::schema_for;
