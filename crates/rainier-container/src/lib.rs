//! # rainier-container
//!
//! Rainier's inversion-of-control layer: the [`Container`] every component
//! registers into, the [`ServiceProvider`] two-phase lifecycle that wires them
//! together, the [`Application`] that drives it, and the [`Facade`] machinery
//! that makes services reachable as static proxies.
//!
//! These four live in one crate because they are one idea,
//! and because splitting them would only produce three crates that always
//! travel together.
//!
//! ```
//! use rainier_container::{Application, Container, ServiceProvider};
//! use rainier_support::Result;
//!
//! struct Greeter { greeting: String }
//! struct GreeterProvider;
//!
//! impl ServiceProvider for GreeterProvider {
//!     fn register(&self, app: &Application) -> Result<()> {
//!         app.singleton(|_: &Container| Ok(Greeter { greeting: "hello".into() }));
//!         Ok(())
//!     }
//! }
//!
//! # #[tokio::main] async fn main() -> Result<()> {
//! let app = Application::new(".");
//! app.register(GreeterProvider)?;
//! app.boot().await?;
//!
//! assert_eq!(app.resolve::<Greeter>()?.greeting, "hello");
//! # Ok(()) }
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod application;
pub mod container;
pub mod facade;
pub mod provider;

pub use application::{Application, LifecycleHook};
pub use container::Container;
pub use facade::{
    clear_facade_application, facade_application, scope_facade_application,
    scoped_facade_application, set_facade_application, spawn_with_facades, task_facade_application,
    try_facade_application, with_facade_application, Facade, FacadeScope,
};
pub use provider::ServiceProvider;

/// Re-exports the `boot_provider!` macro expands to. Not a stable API.
#[doc(hidden)]
pub mod __private {
    pub use rainier_support::{BoxFuture, Result};
}
