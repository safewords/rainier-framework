//! # rainier-support
//!
//! The primitives every other Rainier crate shares, and nothing else. It sits
//! at the bottom of the dependency graph and depends on no other Rainier crate,
//! which is what lets the components above it stay independent of each other.
//!
//! - [`Error`] / [`Result`] — one error type, carrying the status and the
//!   structured details the HTTP layer needs to render any failure.
//! - [`Extensions`] — the type-keyed bag behind request attributes and
//!   container bindings.
//! - [`str`](mod@str) — the inflection vocabulary resource routing and the code
//!   generators are written against.
//! - [`Setting`] / [`setting_enum!`] — a closed set of config values, so a
//!   driver name is an enum rather than a string nobody validates.
//! - [`BoxFuture`] — the boxed future the framework's `dyn`-safe async traits
//!   return.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod build_info;
pub mod error;
pub mod extensions;
pub mod setting;
pub mod str;

pub use build_info::BuildInfo;
pub use error::{Context, Error, ErrorKind, Result};
pub use extensions::Extensions;
pub use setting::Setting;

/// What [`setting_enum!`] expands to. Not a stable API.
///
/// Re-exporting `serde` here is what lets a crate declare a setting without
/// naming serde in its own manifest — the macro reaches it through `$crate`.
#[doc(hidden)]
pub mod __private {
    pub use serde;
}

use std::future::Future;
use std::pin::Pin;

/// A heap-allocated, `Send` future — the return type of every `dyn`-safe async
/// method in the framework.
///
/// Rust's `async fn` in traits produces an unnameable opaque type, which cannot
/// be put behind `dyn`. Every Rainier contract that needs to be a trait object
/// (middleware, queue drivers, mail transports, auth guards) therefore returns
/// this instead. Where a contract does *not* need to be a trait object it uses
/// plain `async fn` and stays allocation-free.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A `'static` [`BoxFuture`], for futures that outlive the call that made them
/// (a spawned job, a queued listener).
pub type BoxedFuture<T> = BoxFuture<'static, T>;

/// A boxed future that is **not** `Send`, and so cannot cross threads.
///
/// Used for the few contracts that wrap something inherently thread-bound —
/// notably the escape hatch onto Rainier ORM's own `Executor` API, whose futures
/// hold `sea_query` statements (and therefore `Rc`) across their awaits. Code
/// on the request path should never need this; a handler's future must be
/// `Send` for the server to spawn it.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Box a future into a [`BoxFuture`]. Sugar for `Box::pin`, named so call
/// sites read as intent rather than allocation.
pub fn boxed<'a, F>(future: F) -> BoxFuture<'a, F::Output>
where
    F: Future + Send + 'a,
{
    Box::pin(future)
}

/// The framework's prelude: what nearly every module wants in scope.
pub mod prelude {
    pub use crate::error::{Context as _, Error, ErrorKind, Result};
    pub use crate::extensions::Extensions;
    pub use crate::setting::Setting as _;
    pub use crate::{boxed, BoxFuture, BoxedFuture, LocalBoxFuture};
}
