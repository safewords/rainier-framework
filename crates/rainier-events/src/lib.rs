//! # rainier-events
//!
//! Rainier's hook system: a typed event [`Dispatcher`], the [`Listener`] and
//! [`WildcardListener`] contracts, [`EventSubscriber`] for grouping a
//! feature's wiring, and [`Reporter`] / [`ExceptionReported`] for exception
//! reporting (see [`report`]).
//!
//! This is the *decoupling* seam of the framework. Where the container answers
//! "who provides this?", events answer "who cares that this happened?" — and
//! the answer can be nobody, or code the emitting component has never heard of.
//! Rainier itself hooks in at every lifecycle point through this bus: model
//! `saving`/`saved`, `JobProcessed`/`JobFailed`, `MessageSending`.
//!
//! ```
//! use rainier_events::Dispatcher;
//! use std::sync::Arc;
//!
//! struct UserRegistered { email: String }
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let events = Dispatcher::new();
//!
//! events.listen(|event: Arc<UserRegistered>| async move {
//!     println!("send a welcome mail to {}", event.email);
//!     Ok(())
//! });
//!
//! events.dispatch(UserRegistered { email: "ada@example.com".into() }).await?;
//! # Ok(()) }
//! ```
//!
//! ## Testing
//!
//! [`Dispatcher::fake`] swaps delivery for recording, so a test can assert an
//! action raised the right events without their listeners firing:
//!
//! ```
//! # use rainier_events::Dispatcher;
//! # struct UserRegistered;
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let events = Dispatcher::fake();
//! events.dispatch(UserRegistered).await?;
//! events.assert_dispatched_times::<UserRegistered>(1);
//! # Ok(()) }
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod dispatcher;
pub mod event;
pub mod report;

pub use dispatcher::Dispatcher;
pub use event::{Event, EventSubscriber, Listener, WildcardListener};
pub use report::{
    Exception, ExceptionReported, Failure, FailureEvent, FailureKind, ReportMeta, Reporter,
};
