//! # rainier-http
//!
//! The HTTP message layer: [`Request`], [`Response`], [`Body`], [`Cookie`],
//! uploads, and the [`FromRequest`] extractors controller actions are written
//! against.
//!
//! It knows nothing about routing, middleware or the server — those are layered
//! on top. That is what lets a test drive a controller with a hand-built
//! [`Request`] and assert on the [`Response`], with no runtime and no socket:
//!
//! ```
//! use rainier_http::{IntoResponse, Request, Response};
//! use http::{Method, StatusCode};
//!
//! async fn show(request: &Request) -> Response {
//!     match request.input("id") {
//!         Some(id) => Response::json(&serde_json::json!({ "id": id })),
//!         None => Response::new(StatusCode::BAD_REQUEST),
//!     }
//! }
//!
//! # #[tokio::main] async fn main() {
//! let request = Request::builder().method(Method::GET).uri("/posts?id=7").build();
//! assert_eq!(show(&request).await.status(), StatusCode::OK);
//! # }
//! ```
//!
//! ## The buffered-request decision
//!
//! Request bodies arrive fully buffered, so `request.input("title")` is a plain
//! synchronous call.
//! Response bodies may stream. See [`body`] for why the asymmetry is
//! deliberate rather than an omission.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod body;
pub mod coerce;
pub mod cookie;
pub mod extract;
pub mod input;
pub mod request;
pub mod response;
pub mod upload;

pub use body::Body;
pub use cookie::{Cookie, SameSite};
pub use extract::FromRequest;
pub use request::{ClientIp, Request, RequestBuilder};
pub use response::{Html, IntoResponse, Json, Redirect, RenderedError, Response};
pub use upload::{Multipart, UploadedFile};

// Re-exported so downstream crates and applications reference one `http`
// version rather than accidentally depending on a second copy.
pub use http;
pub use http::{HeaderMap, Method, StatusCode, Uri, Version};
