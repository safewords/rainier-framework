//! [`ServiceProvider`] — the unit of framework wiring.
//!
//! Every Rainier component ships a provider rather than a `setup()` function,
//! because registration and booting have to happen in
//! two separate passes across *all* providers, or a provider could resolve a
//! service that a later provider has not bound yet.
//!
//! - [`register`](ServiceProvider::register) binds factories into the
//!   container. It must **not** resolve anything, because the providers after
//!   it have not registered yet.
//! - [`boot`](ServiceProvider::boot) runs once every provider has registered,
//!   so by then it may resolve freely. It is `async`, which is what lets a
//!   provider open a database pool or warm a cache during boot.

use std::any::type_name;

use rainier_support::{BoxFuture, Result};

use crate::application::Application;

/// A bundle of related bindings and their boot-time wiring.
///
/// ```
/// # use rainier_container::{Application, Container, ServiceProvider};
/// # use rainier_support::Result;
/// struct Clock;
/// struct ClockProvider;
///
/// impl ServiceProvider for ClockProvider {
///     fn register(&self, app: &Application) -> Result<()> {
///         app.singleton(|_: &Container| Ok(Clock));
///         Ok(())
///     }
/// }
/// ```
pub trait ServiceProvider: Send + Sync + 'static {
    /// A label for diagnostics and `provider:list`. Defaults to the Rust type
    /// name.
    fn name(&self) -> &'static str {
        type_name::<Self>()
    }

    /// Bind services into the container.
    ///
    /// Runs for every provider before any provider boots. Do not resolve here.
    fn register(&self, app: &Application) -> Result<()> {
        let _ = app;
        Ok(())
    }

    /// Wire things together once every provider has registered.
    ///
    /// Safe to resolve from. Returns a boxed future so the trait stays
    /// object-safe — providers are held as `Arc<dyn ServiceProvider>`.
    fn boot<'a>(&'a self, app: &'a Application) -> BoxFuture<'a, Result<()>> {
        let _ = app;
        Box::pin(async { Ok(()) })
    }
}

/// Implements [`ServiceProvider::boot`] from an `async fn`, so a provider can
/// be written with ordinary async syntax instead of hand-boxing the future.
///
/// ```
/// # use rainier_container::{boot_provider, Application, Container, ServiceProvider};
/// # use rainier_support::Result;
/// struct Pool;
/// struct DatabaseProvider;
///
/// impl ServiceProvider for DatabaseProvider {
///     fn register(&self, app: &Application) -> Result<()> {
///         app.singleton(|_: &Container| Ok(Pool));
///         Ok(())
///     }
///
///     boot_provider!(async |self, app| {
///         let _pool = app.resolve::<Pool>()?;
///         Ok(())
///     });
/// }
/// ```
#[macro_export]
macro_rules! boot_provider {
    (async |$self:ident, $app:ident| $body:block) => {
        fn boot<'boot>(
            &'boot $self,
            $app: &'boot $crate::Application,
        ) -> $crate::__private::BoxFuture<'boot, $crate::__private::Result<()>> {
            ::std::boxed::Box::pin(async move { $body })
        }
    };
}
