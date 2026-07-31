//! The IoC [`Container`] — Rainier's service locator and the thing every
//! component registers itself into.
//!
//! A PHP container can resolve services by class name via reflection. Rust has
//! no reflection, so the key here is [`TypeId`] and the "constructor injection" is
//! an explicit factory closure that receives the container and pulls out what
//! it needs:
//!
//! ```
//! # use rainier_container::Container;
//! # use rainier_support::Result;
//! struct Config { url: String }
//! struct Client { url: String }
//!
//! let c = Container::new();
//! c.instance(Config { url: "postgres://…".into() });
//! c.singleton(|c: &Container| {
//!     let config = c.resolve::<Config>()?;
//!     Ok(Client { url: config.url.clone() })
//! });
//!
//! let client = c.resolve::<Client>().unwrap();
//! assert_eq!(client.url, "postgres://…");
//! ```
//!
//! The binding lifetimes:
//!
//! | Method | Lifetime |
//! |---|---|
//! | [`bind`](Container::bind) | a fresh value per resolution |
//! | [`singleton`](Container::singleton) | built once, shared forever |
//! | [`scoped`](Container::scoped) | built once per scope, dropped by [`flush_scoped`](Container::flush_scoped) |
//! | [`instance`](Container::instance) | supplied ready-made |

use std::any::{type_name, Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use rainier_support::{Error, Result};

/// A type-erased service instance.
type Shared = Arc<dyn Any + Send + Sync>;

/// A factory that builds a type-erased service from the container.
type Factory = Arc<dyn Fn(&Container) -> Result<Shared> + Send + Sync>;

/// How a bound type is produced, and how long the result lives.
#[derive(Clone)]
enum Binding {
    /// Rebuilt on every resolution.
    Transient(Factory),
    /// Built at most once; the instance is memoised in the `Mutex`.
    Shared { factory: Factory, instance: Arc<Mutex<Option<Shared>>>, scoped: bool },
}

/// A registry of services keyed by their Rust type.
///
/// Cheap to share: put it in an [`Arc`] and clone the handle. All methods take
/// `&self` — bindings are registered through interior mutability, because
/// service providers register into a container that is already shared.
#[derive(Default)]
pub struct Container {
    bindings: RwLock<HashMap<TypeId, Binding>>,
}

thread_local! {
    /// The chain of types currently being resolved on this thread, so a
    /// dependency cycle is reported as an error instead of deadlocking on the
    /// memoisation mutex.
    static RESOLVING: RefCell<Vec<(TypeId, &'static str)>> = const { RefCell::new(Vec::new()) };
}

impl Container {
    /// An empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a factory that runs on **every** resolution.
    pub fn bind<T, F>(&self, factory: F)
    where
        T: Send + Sync + 'static,
        F: Fn(&Container) -> Result<T> + Send + Sync + 'static,
    {
        let factory: Factory = Arc::new(move |c| Ok(Arc::new(factory(c)?) as Shared));
        self.insert::<T>(Binding::Transient(factory));
    }

    /// Bind a factory that runs **once**; every resolution afterwards hands
    /// back the same [`Arc`].
    pub fn singleton<T, F>(&self, factory: F)
    where
        T: Send + Sync + 'static,
        F: Fn(&Container) -> Result<T> + Send + Sync + 'static,
    {
        self.shared::<T, F>(factory, false);
    }

    /// Like [`singleton`](Self::singleton), but [`flush_scoped`] drops the
    /// memoised instance so the next resolution rebuilds it.
    ///
    /// This is the binding a long-running worker wants for per-job state: the
    /// queue worker flushes scoped bindings between jobs so one job cannot leak
    /// state into the next.
    ///
    /// [`flush_scoped`]: Self::flush_scoped
    pub fn scoped<T, F>(&self, factory: F)
    where
        T: Send + Sync + 'static,
        F: Fn(&Container) -> Result<T> + Send + Sync + 'static,
    {
        self.shared::<T, F>(factory, true);
    }

    fn shared<T, F>(&self, factory: F, scoped: bool)
    where
        T: Send + Sync + 'static,
        F: Fn(&Container) -> Result<T> + Send + Sync + 'static,
    {
        let factory: Factory = Arc::new(move |c| Ok(Arc::new(factory(c)?) as Shared));
        self.insert::<T>(Binding::Shared { factory, instance: Arc::new(Mutex::new(None)), scoped });
    }

    /// Register an already-built value. The container never constructs it.
    pub fn instance<T: Send + Sync + 'static>(&self, value: T) {
        self.instance_arc(Arc::new(value));
    }

    /// Register an already-built value that is already behind an [`Arc`] —
    /// useful when the caller must keep a handle to it too.
    pub fn instance_arc<T: Send + Sync + 'static>(&self, value: Arc<T>) {
        let instance: Shared = value;
        self.insert::<T>(Binding::Shared {
            // Never called: the instance slot is pre-filled.
            factory: Arc::new(|_| Err(Error::internal("instance binding has no factory"))),
            instance: Arc::new(Mutex::new(Some(instance))),
            scoped: false,
        });
    }

    fn insert<T: 'static>(&self, binding: Binding) {
        let mut bindings = self.bindings.write().expect("container lock poisoned");
        bindings.insert(TypeId::of::<T>(), binding);
    }

    /// Resolve `T`, building it if necessary.
    ///
    /// Fails when `T` was never bound, when its factory fails, or when
    /// resolving it re-enters itself (a dependency cycle).
    pub fn resolve<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        let binding = {
            let bindings = self.bindings.read().expect("container lock poisoned");
            bindings.get(&TypeId::of::<T>()).cloned()
        };

        let Some(binding) = binding else {
            return Err(Error::internal(format!(
                "nothing is bound for `{}` — register it in a service provider before resolving",
                type_name::<T>()
            )));
        };

        let value = self.build::<T>(binding)?;
        value.downcast::<T>().map_err(|_| {
            // Unreachable: bindings are keyed by the TypeId of the value the
            // factory produces. Reported rather than unwrapped so a future
            // untyped registration path cannot turn into a panic.
            Error::internal(format!("binding for `{}` produced the wrong type", type_name::<T>()))
        })
    }

    fn build<T: Send + Sync + 'static>(&self, binding: Binding) -> Result<Shared> {
        // The cycle guard wraps *everything*, including the "already built?"
        // check. It has to: that check takes the memoisation mutex, and
        // `std::sync::Mutex` is not reentrant, so a factory for `T` that
        // re-resolves `T` would deadlock on its own lock before any cycle could
        // be detected. Checking the guard first turns that into an error.
        let _guard = ResolutionGuard::enter::<T>()?;

        match binding {
            Binding::Transient(factory) => factory(self),
            Binding::Shared { factory, instance, .. } => {
                let mut slot = instance.lock().expect("container lock poisoned");
                if let Some(existing) = slot.clone() {
                    return Ok(existing);
                }
                let built = factory(self)?;
                *slot = Some(built.clone());
                Ok(built)
            }
        }
    }

    /// Resolve `T`, or `None` if it is unbound or its factory failed.
    pub fn try_resolve<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.resolve::<T>().ok()
    }

    /// Resolve `T`, panicking with the resolution error if it is not
    /// available. For bootstrap code and facades, where an unbound service is
    /// a configuration bug rather than a runtime condition.
    pub fn expect_resolve<T: Send + Sync + 'static>(&self) -> Arc<T> {
        match self.resolve::<T>() {
            Ok(value) => value,
            Err(e) => panic!("{e}"),
        }
    }

    /// Whether anything is bound for `T`.
    pub fn bound<T: 'static>(&self) -> bool {
        self.bindings.read().expect("container lock poisoned").contains_key(&TypeId::of::<T>())
    }

    /// Drop the binding for `T`, if any.
    pub fn forget<T: 'static>(&self) {
        self.bindings.write().expect("container lock poisoned").remove(&TypeId::of::<T>());
    }

    /// Drop the memoised instance of every [`scoped`](Self::scoped) binding.
    /// The bindings survive; only the built values are discarded.
    pub fn flush_scoped(&self) {
        let bindings = self.bindings.read().expect("container lock poisoned");
        for binding in bindings.values() {
            if let Binding::Shared { instance, scoped: true, .. } = binding {
                *instance.lock().expect("container lock poisoned") = None;
            }
        }
    }

    /// How many types are bound.
    pub fn len(&self) -> usize {
        self.bindings.read().expect("container lock poisoned").len()
    }

    /// Whether nothing is bound.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for Container {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Container").field("bindings", &self.len()).finish()
    }
}

/// Marks a type as "being resolved on this thread" for as long as it lives.
///
/// Detects same-thread dependency cycles. A cycle spread across two threads
/// (thread A builds `T` which needs `U`, thread B builds `U` which needs `T`)
/// still blocks on the memoisation mutexes — that is a genuine deadlock in the
/// application's dependency graph, and no per-thread bookkeeping can see it.
struct ResolutionGuard;

impl ResolutionGuard {
    fn enter<T: 'static>() -> Result<Self> {
        let id = TypeId::of::<T>();
        let name = type_name::<T>();

        RESOLVING.with(|stack| {
            let mut stack = stack.borrow_mut();
            if stack.iter().any(|(seen, _)| *seen == id) {
                let mut chain: Vec<&str> = stack.iter().map(|(_, n)| *n).collect();
                chain.push(name);
                return Err(Error::internal(format!(
                    "circular dependency while resolving: {}",
                    chain.join(" -> ")
                )));
            }
            stack.push((id, name));
            Ok(())
        })?;

        Ok(Self)
    }
}

impl Drop for ResolutionGuard {
    fn drop(&mut self) {
        // Popping on drop rather than after the call keeps the stack correct
        // when a factory panics and unwinds; otherwise a stale entry would make
        // every later resolution of that type look like a cycle.
        RESOLVING.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct Counter(usize);
    #[derive(Debug)]
    struct Dependent(usize);

    #[test]
    fn instance_bindings_come_back_as_given() {
        let c = Container::new();
        c.instance(Counter(42));
        assert_eq!(c.resolve::<Counter>().unwrap().0, 42);
    }

    #[test]
    fn unbound_types_report_their_name() {
        let c = Container::new();
        let err = c.resolve::<Counter>().unwrap_err();
        assert!(err.message().contains("Counter"), "{}", err.message());
    }

    #[test]
    fn bind_rebuilds_every_time() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let c = Container::new();
        c.bind(|_| Ok(Counter(BUILDS.fetch_add(1, Ordering::SeqCst))));

        assert_eq!(c.resolve::<Counter>().unwrap().0, 0);
        assert_eq!(c.resolve::<Counter>().unwrap().0, 1);
        assert_eq!(BUILDS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn singleton_builds_once_and_shares() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let c = Container::new();
        c.singleton(|_| {
            BUILDS.fetch_add(1, Ordering::SeqCst);
            Ok(Counter(7))
        });

        let a = c.resolve::<Counter>().unwrap();
        let b = c.resolve::<Counter>().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn factories_can_resolve_their_dependencies() {
        let c = Container::new();
        c.instance(Counter(5));
        c.singleton(|c: &Container| Ok(Dependent(c.resolve::<Counter>()?.0 * 2)));
        assert_eq!(c.resolve::<Dependent>().unwrap().0, 10);
    }

    #[test]
    fn a_failing_factory_surfaces_its_error() {
        let c = Container::new();
        c.singleton(|_| Err::<Counter, _>(Error::internal("no database")));
        assert_eq!(c.resolve::<Counter>().unwrap_err().message(), "no database");
    }

    #[test]
    fn dependency_cycles_error_instead_of_deadlocking() {
        let c = Container::new();
        // Counter needs Dependent, Dependent needs Counter.
        c.singleton(|c: &Container| Ok(Counter(c.resolve::<Dependent>()?.0)));
        c.singleton(|c: &Container| Ok(Dependent(c.resolve::<Counter>()?.0)));

        let err = c.resolve::<Counter>().unwrap_err();
        assert!(err.message().contains("circular dependency"), "{}", err.message());
    }

    #[test]
    fn a_singleton_that_resolves_itself_errors_rather_than_self_deadlocking() {
        // Regression guard: the memoisation mutex is not reentrant, so this
        // must be caught by the cycle guard *before* the lock is taken.
        let c = Container::new();
        c.singleton(|c: &Container| Ok(Counter(c.resolve::<Counter>()?.0)));

        let err = c.resolve::<Counter>().unwrap_err();
        assert!(err.message().contains("circular dependency"), "{}", err.message());
    }

    #[test]
    fn a_panicking_factory_does_not_poison_later_resolutions() {
        let c = Arc::new(Container::new());
        c.bind(|_| -> Result<Counter> { panic!("boom") });

        let panicked = {
            let c = Arc::clone(&c);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _ = c.resolve::<Counter>();
            }))
        };
        assert!(panicked.is_err());

        // The resolution stack must have unwound with the panic; if it had not,
        // this second attempt would be misreported as a cycle.
        c.forget::<Counter>();
        c.instance(Counter(1));
        assert_eq!(c.resolve::<Counter>().unwrap().0, 1);
    }

    #[test]
    fn scoped_bindings_rebuild_after_a_flush() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let c = Container::new();
        c.scoped(|_| Ok(Counter(BUILDS.fetch_add(1, Ordering::SeqCst))));

        assert_eq!(c.resolve::<Counter>().unwrap().0, 0);
        assert_eq!(c.resolve::<Counter>().unwrap().0, 0);
        c.flush_scoped();
        assert_eq!(c.resolve::<Counter>().unwrap().0, 1);
    }

    #[test]
    fn flush_scoped_leaves_singletons_alone() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let c = Container::new();
        c.singleton(|_| Ok(Counter(BUILDS.fetch_add(1, Ordering::SeqCst))));

        assert_eq!(c.resolve::<Counter>().unwrap().0, 0);
        c.flush_scoped();
        assert_eq!(c.resolve::<Counter>().unwrap().0, 0);
    }

    #[test]
    fn bound_and_forget_track_the_registry() {
        let c = Container::new();
        assert!(!c.bound::<Counter>());
        c.instance(Counter(1));
        assert!(c.bound::<Counter>());
        c.forget::<Counter>();
        assert!(!c.bound::<Counter>());
    }

    #[test]
    fn resolution_is_safe_across_threads() {
        let c = Arc::new(Container::new());
        c.singleton(|_| Ok(Counter(99)));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || c.resolve::<Counter>().unwrap())
            })
            .collect();

        let first = c.resolve::<Counter>().unwrap();
        for handle in handles {
            assert!(Arc::ptr_eq(&first, &handle.join().unwrap()));
        }
    }
}
