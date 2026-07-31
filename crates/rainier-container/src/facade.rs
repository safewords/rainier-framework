//! Facades — static proxies onto container-resolved services.
//!
//! In PHP, a facade can intercept an unknown static call and
//! forward it to an object pulled from the container. Rust has no such hook, so
//! a Rainier facade is a zero-sized type with inherent methods that each begin
//! by resolving the underlying service:
//!
//! ```ignore
//! pub struct Queue;
//! impl Facade for Queue { type Accessor = QueueManager; }
//!
//! impl Queue {
//!     pub async fn push(job: impl Job) -> Result<JobId> {
//!         Self::instance().push(job).await     // <- resolve, then delegate
//!     }
//! }
//! ```
//!
//! The convenience is the same and so is the cost: a facade reaches into
//! **global** state, which makes the dependency invisible at the call site.
//! Prefer taking the service as a constructor argument in anything you intend
//! to unit-test; reach for the facade in application code and route closures,
//! where the ergonomics win.
//!
//! ## Testing
//!
//! There is no separate `swap` mechanism, because there does not need to be:
//! facades resolve through the container on *every* call, so rebinding the
//! accessor swaps what every facade call sees from that point on.
//!
//! ```ignore
//! app.instance(MailManager::fake());   // every `Mail::…` call now hits the fake
//! ```

use std::any::type_name;
use std::sync::{Arc, RwLock};

use std::cell::RefCell;
use std::marker::PhantomData;

use crate::application::Application;

/// The application backing every facade in this process.
static FACADE_APP: RwLock<Option<Arc<Application>>> = RwLock::new(None);

thread_local! {
    /// Applications scoped to *this thread*, innermost last.
    ///
    /// A stack rather than an `Option` so scopes nest, and so dropping an
    /// inner one restores the outer rather than clearing everything.
    static SCOPED_APPS: RefCell<Vec<Arc<Application>>> = const { RefCell::new(Vec::new()) };
}

tokio::task_local! {
    /// The application scoped to *this task*.
    ///
    /// A task-local rather than a second thread-local, because a task is the
    /// thing that outlives a thread: tokio may resume a future on a different
    /// worker between polls, and a thread-local does not follow it.
    static TASK_APP: Arc<Application>;
}

/// A facade scope, undone when it is dropped.
///
/// Held by whoever called [`scope_facade_application`]. Dropping it is the only
/// way to leave the scope, so a panicking test cannot leak its application into
/// the next one.
#[must_use = "the scope ends the moment this is dropped"]
pub struct FacadeScope {
    /// Not `Send`: the scope is a thread-local, and moving the guard to another
    /// thread would pop a stack it was never pushed onto.
    _not_send: PhantomData<*const ()>,
}

impl Drop for FacadeScope {
    fn drop(&mut self) {
        SCOPED_APPS.with(|apps| {
            apps.borrow_mut().pop();
        });
    }
}

/// Resolve facades through `app` **on this thread**, until the guard drops.
///
/// The answer to a process-global container: two tests can each boot their own
/// application and each see their own, instead of racing to install one
/// globally and then resolving out of whichever won.
///
/// ```
/// # use std::sync::Arc;
/// # use rainier_container::{scope_facade_application, try_facade_application, Application};
/// let app = Arc::new(Application::new("."));
/// {
///     let _scope = scope_facade_application(Arc::clone(&app));
///     assert!(try_facade_application().is_some());
/// }
/// # let _ = app;
/// ```
///
/// # Where a thread scope reaches, and where it stops
///
/// It covers everything that runs **on this thread**, which is more than it
/// sounds: `block_on` drives a future on the calling thread, so the body of a
/// `#[tokio::test]` stays inside the scope even on a `multi_thread` runtime,
/// across as many `.await`s as it likes.
///
/// It stops at [`tokio::spawn`] and `spawn_blocking`. A spawned task is a new
/// task on some other thread, and it resolves through the process-wide
/// application instead — silently. That is what
/// [`spawn_with_facades`] and [`with_facade_application`] are for.
///
/// This does not replace [`set_facade_application`]; it layers over it.
/// Application code installs one globally at boot; a test scopes its own on
/// top.
pub fn scope_facade_application(app: Arc<Application>) -> FacadeScope {
    SCOPED_APPS.with(|apps| apps.borrow_mut().push(app));
    FacadeScope { _not_send: PhantomData }
}

/// The innermost application scoped to this thread, if any.
pub fn scoped_facade_application() -> Option<Arc<Application>> {
    SCOPED_APPS.with(|apps| apps.borrow().last().cloned())
}

/// Resolve facades through `app` for the duration of `future` — **wherever it
/// runs**.
///
/// A task-local scope rather than a thread-local one, so it follows the future
/// when tokio resumes it on a different worker thread. That is the difference
/// that matters for anything spawned:
///
/// ```
/// # use std::sync::Arc;
/// # use rainier_container::{with_facade_application, facade_application, Application};
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let app = Arc::new(Application::new("."));
/// let same = with_facade_application(Arc::clone(&app), async {
///     facade_application()
/// })
/// .await;
///
/// assert!(Arc::ptr_eq(&same, &app));
/// # }
/// ```
///
/// It nests: an inner scope wins for its own duration and the outer one
/// resumes after.
///
/// A task-local is **not inherited** by a task this future goes on to spawn —
/// nothing in tokio propagates them — so a spawn inside still needs
/// [`spawn_with_facades`].
pub async fn with_facade_application<F>(app: Arc<Application>, future: F) -> F::Output
where
    F: std::future::Future,
{
    TASK_APP.scope(app, future).await
}

/// The application scoped to this task, if any.
pub fn task_facade_application() -> Option<Arc<Application>> {
    TASK_APP.try_with(Arc::clone).ok()
}

/// [`tokio::spawn`], carrying the current facade application into the new task.
///
/// The gap this closes: a spawned task starts with no thread scope and no task
/// scope, so it resolves through the process-wide application — which in a test
/// is whichever one booted last, and in a multi-tenant process is the wrong
/// one. Nothing warns, because from inside the task there is nothing to notice.
///
/// ```
/// # use std::sync::Arc;
/// # use rainier_container::{scope_facade_application, spawn_with_facades, facade_application, Application};
/// # #[tokio::main(flavor = "multi_thread")] async fn main() {
/// let app = Arc::new(Application::new("."));
/// let _scope = scope_facade_application(Arc::clone(&app));
///
/// let seen = spawn_with_facades(async { facade_application() }).await.unwrap();
///
/// assert!(Arc::ptr_eq(&seen, &app), "the spawned task saw the same application");
/// # }
/// ```
///
/// With no application installed at all this is exactly [`tokio::spawn`].
///
/// # Panics
///
/// Outside a tokio runtime, like [`tokio::spawn`].
pub fn spawn_with_facades<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match try_facade_application() {
        Some(app) => tokio::spawn(with_facade_application(app, future)),
        None => tokio::spawn(future),
    }
}

/// Install the application that facades resolve through.
///
/// Called once during bootstrap. Calling it again replaces the binding, which
/// is what lets a test harness stand up a fresh application per test.
pub fn set_facade_application(app: Arc<Application>) {
    *FACADE_APP.write().expect("facade application lock poisoned") = Some(app);
}

/// The application facades resolve through, if one is installed.
///
/// Three places are consulted, nearest first:
///
/// 1. the [task](with_facade_application) scope — follows a future across
///    threads;
/// 2. the [thread](scope_facade_application) scope — covers everything running
///    on this thread;
/// 3. the [process](set_facade_application) binding — what the bootstrap
///    installed.
///
/// Nearest-first is what lets a test have its own without the application code
/// under test knowing anything about it.
pub fn try_facade_application() -> Option<Arc<Application>> {
    if let Some(scoped) = task_facade_application() {
        return Some(scoped);
    }
    if let Some(scoped) = scoped_facade_application() {
        return Some(scoped);
    }
    FACADE_APP.read().expect("facade application lock poisoned").clone()
}
// The application facades resolve through.
///
/// # Panics
///
/// If no application has been installed. That is a bootstrap bug — a facade
/// was used before (or without) [`set_facade_application`] — so it fails loudly
/// rather than returning an error every call site would have to unwrap.
pub fn facade_application() -> Arc<Application> {
    try_facade_application().expect(
        "no application is bound to the facades — call `set_facade_application` during bootstrap \
         (or use the service directly instead of the facade)",
    )
}

/// Forget the installed application. Mainly for tests that assert on the
/// unbound behaviour.
pub fn clear_facade_application() {
    *FACADE_APP.write().expect("facade application lock poisoned") = None;
}

/// A static proxy onto a container-resolved service.
///
/// Implementors are zero-sized marker types; all the behaviour lives in the
/// [`Accessor`](Facade::Accessor) they resolve.
pub trait Facade {
    /// The service this facade forwards to.
    type Accessor: Send + Sync + 'static;

    /// Resolve the underlying service.
    ///
    /// # Panics
    ///
    /// If no application is installed, or the accessor is not bound in it.
    /// Both are configuration bugs: a facade whose service was never registered
    /// can never work, so it says so rather than propagating an error into
    /// every caller.
    fn instance() -> Arc<Self::Accessor> {
        let app = facade_application();
        match app.resolve::<Self::Accessor>() {
            Ok(service) => service,
            Err(e) => panic!(
                "the `{}` facade could not resolve `{}`: {e}",
                type_name::<Self>(),
                type_name::<Self::Accessor>()
            ),
        }
    }

    /// Resolve the underlying service, or `None` if the application or the
    /// binding is missing. For code that must degrade rather than panic.
    fn try_instance() -> Option<Arc<Self::Accessor>> {
        try_facade_application()?.try_resolve::<Self::Accessor>()
    }
}

/// Declare a facade type and its [`Facade`] impl.
///
/// ```
/// # use rainier_container::{facade, Application, Facade};
/// # use std::sync::Arc;
/// pub struct Clock {
///     pub now: u64,
/// }
///
/// facade!(
///     /// Reads the application clock.
///     Time => Clock
/// );
///
/// # fn main() {
/// let app = Application::new(".");
/// app.instance(Clock { now: 1 });
/// rainier_container::set_facade_application(Arc::new(app));
///
/// assert_eq!(Time::instance().now, 1);
/// # rainier_container::clear_facade_application();
/// # }
/// ```
#[macro_export]
macro_rules! facade {
    ($(#[$meta:meta])* $name:ident => $accessor:ty) => {
        $(#[$meta])*
        #[doc = ""]
        #[doc = concat!("A [`Facade`](", stringify!($crate), "::Facade) over [`", stringify!($accessor), "`].")]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct $name;

        impl $crate::Facade for $name {
            type Accessor = $accessor;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Container;

    struct Clock(u64);
    struct Missing;

    struct Time;
    impl Facade for Time {
        type Accessor = Clock;
    }

    struct Nowhere;
    impl Facade for Nowhere {
        type Accessor = Missing;
    }

    // The facade application is process-global, so these tests must not run
    // concurrently with one another.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_app(build: impl FnOnce(&Application)) -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let app = Application::new(".");
        build(&app);
        set_facade_application(Arc::new(app));
        guard
    }

    #[test]
    fn a_facade_resolves_through_the_container() {
        let _guard = with_app(|app| app.instance(Clock(5)));
        assert_eq!(Time::instance().0, 5);
        clear_facade_application();
    }

    #[test]
    fn rebinding_the_accessor_swaps_what_the_facade_sees() {
        let _guard = with_app(|app| app.instance(Clock(1)));
        assert_eq!(Time::instance().0, 1);

        // No `swap` API needed: the facade re-resolves on every call.
        facade_application().instance(Clock(2));
        assert_eq!(Time::instance().0, 2);
        clear_facade_application();
    }

    #[test]
    fn try_instance_is_none_for_an_unbound_accessor() {
        let _guard = with_app(|_| {});
        assert!(Nowhere::try_instance().is_none());
        clear_facade_application();
    }

    #[test]
    fn try_instance_is_none_with_no_application() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        clear_facade_application();
        assert!(Time::try_instance().is_none());
    }

    #[test]
    #[should_panic(expected = "could not resolve")]
    fn instance_panics_for_an_unbound_accessor() {
        let _guard = with_app(|_| {});
        let _ = Nowhere::instance();
    }

    #[test]
    fn a_transient_binding_is_re_resolved_each_call() {
        let _guard = with_app(|app| {
            app.bind(|_: &Container| Ok(Clock(7)));
        });
        let a = Time::instance();
        let b = Time::instance();
        assert_eq!(a.0, b.0);
        assert!(!Arc::ptr_eq(&a, &b));
        clear_facade_application();
    }

    #[test]
    fn a_scope_wins_over_the_process_wide_application() {
        // The whole point: a test can have its own container without racing
        // whatever else installed one globally.
        //
        // This one has to hold `SERIAL` to make its own claim, which is the
        // irony of the thing it is testing: it asserts about the *process*
        // slot, and there is one of those. Without the guard another test's
        // `with_app` lands between the two assertions and this fails, seeing
        // that test's application instead of `global`.
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

        let global = Arc::new(Application::new("global"));
        let scoped = Arc::new(Application::new("scoped"));

        set_facade_application(Arc::clone(&global));
        assert_eq!(facade_application().base_path(), global.base_path());

        {
            let _scope = scope_facade_application(Arc::clone(&scoped));
            assert_eq!(facade_application().base_path(), scoped.base_path());
        }

        assert_eq!(facade_application().base_path(), global.base_path(), "the scope is undone");
        clear_facade_application();
    }

    #[test]
    fn scopes_nest_and_unwind_in_order() {
        let outer = Arc::new(Application::new("outer"));
        let inner = Arc::new(Application::new("inner"));

        let _outer = scope_facade_application(Arc::clone(&outer));
        {
            let _inner = scope_facade_application(Arc::clone(&inner));
            assert_eq!(facade_application().base_path(), inner.base_path());
        }
        assert_eq!(facade_application().base_path(), outer.base_path());
    }

    #[test]
    fn a_scope_that_panicked_does_not_leak_into_the_next_test() {
        // `Drop` runs while unwinding, which is why the scope is a guard and
        // not a pair of calls.
        let scoped = Arc::new(Application::new("panicky"));

        let outcome = std::panic::catch_unwind(|| {
            let _scope = scope_facade_application(Arc::clone(&scoped));
            panic!("as a test would");
        });

        assert!(outcome.is_err());
        assert!(scoped_facade_application().is_none(), "the stack should be empty again");
    }
}
