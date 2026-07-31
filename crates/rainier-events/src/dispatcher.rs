//! The event [`Dispatcher`] — Rainier's hook bus.
//!
//! Components raise events at their extension points and applications hook in
//! by listening, which is how a feature is extended without editing it. The
//! framework itself uses this for model lifecycle hooks (`saving`, `saved`),
//! queue hooks (`JobProcessed`, `JobFailed`), and mail hooks (`MessageSending`).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rainier_support::{BoxedFuture, Error, Result};

use crate::event::{Event, EventSubscriber, Listener, WildcardListener};

/// A type-erased listener invocation.
type StoredHandler =
    Arc<dyn Fn(Arc<dyn Any + Send + Sync>) -> BoxedFuture<Result<()>> + Send + Sync>;

/// A registered listener, with the ordering keys dispatch sorts by.
#[derive(Clone)]
struct Registration {
    handler: StoredHandler,
    priority: i32,
    /// Registration order, breaking priority ties.
    sequence: usize,
}

/// A wildcard listener with the same ordering keys.
type WildcardRegistration = (Arc<dyn WildcardListener>, i32, usize);

/// One dispatched event, retained while the dispatcher is
/// [faking](Dispatcher::fake).
struct Recorded {
    type_id: TypeId,
    name: &'static str,
    payload: Arc<dyn Any + Send + Sync>,
}

/// Registers listeners and dispatches events to them.
///
/// ```
/// # use rainier_events::Dispatcher;
/// # use std::sync::Arc;
/// # use std::sync::atomic::{AtomicU64, Ordering};
/// struct OrderShipped { order_id: u64 }
///
/// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
/// let events = Dispatcher::new();
/// let seen = Arc::new(AtomicU64::new(0));
///
/// let sink = Arc::clone(&seen);
/// events.listen(move |event: Arc<OrderShipped>| {
///     let sink = Arc::clone(&sink);
///     async move {
///         sink.store(event.order_id, Ordering::SeqCst);
///         Ok(())
///     }
/// });
///
/// events.dispatch(OrderShipped { order_id: 42 }).await?;
/// assert_eq!(seen.load(Ordering::SeqCst), 42);
/// # Ok(()) }
/// ```
#[derive(Default)]
pub struct Dispatcher {
    listeners: RwLock<HashMap<TypeId, Vec<Registration>>>,
    wildcards: RwLock<Vec<WildcardRegistration>>,
    sequence: AtomicUsize,
    /// `Some` while faking: events are recorded here and listeners are skipped.
    recorder: Option<Mutex<Vec<Recorded>>>,
}

impl Dispatcher {
    /// A dispatcher with no listeners.
    pub fn new() -> Self {
        Self::default()
    }

    /// A dispatcher that **records** events instead of delivering them.
    ///
    /// The dispatcher's test double: listeners never run, so a test can
    /// assert that an action raised the right events without their side
    /// effects firing.
    pub fn fake() -> Self {
        Self { recorder: Some(Mutex::new(Vec::new())), ..Self::default() }
    }

    /// Whether this dispatcher is recording instead of delivering.
    pub fn is_faking(&self) -> bool {
        self.recorder.is_some()
    }

    // --- registration ------------------------------------------------------

    /// Register a listener for events of type `E`, at the default priority.
    pub fn listen<E, L>(&self, listener: L)
    where
        E: Event,
        L: Listener<E>,
    {
        self.listen_with_priority(listener, 0);
    }

    /// Register a listener for events of type `E`.
    ///
    /// Higher `priority` runs first; listeners of equal priority run in
    /// registration order.
    pub fn listen_with_priority<E, L>(&self, listener: L, priority: i32)
    where
        E: Event,
        L: Listener<E>,
    {
        let listener = Arc::new(listener);
        let handler: StoredHandler = Arc::new(move |erased| {
            let listener = Arc::clone(&listener);
            match erased.downcast::<E>() {
                Ok(event) => listener.handle(event),
                Err(_) => {
                    // Unreachable: handlers are stored under the TypeId of the
                    // very type they downcast to. Surfaced as an error rather
                    // than an unwrap so a future untyped dispatch path cannot
                    // become a panic in application code.
                    let name = std::any::type_name::<E>();
                    Box::pin(async move {
                        Err(Error::internal(format!(
                            "listener for `{name}` received a different event type"
                        )))
                    })
                }
            }
        });

        let registration = Registration {
            handler,
            priority,
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst),
        };
        self.listeners
            .write()
            .expect("listeners lock poisoned")
            .entry(TypeId::of::<E>())
            .or_default()
            .push(registration);
    }

    /// Register a listener for events of type `E` written as a **synchronous**
    /// closure. Sugar for a listener whose body does no awaiting.
    pub fn listen_sync<E, F>(&self, listener: F)
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<()> + Send + Sync + 'static,
    {
        self.listen(move |event: Arc<E>| {
            let outcome = listener(event);
            async move { outcome }
        });
    }

    /// Register a listener that receives **every** event, with its name and
    /// type-erased payload.
    pub fn listen_any<L: WildcardListener>(&self, listener: L) {
        self.listen_any_with_priority(listener, 0);
    }

    /// [`listen_any`](Self::listen_any) at an explicit priority.
    pub fn listen_any_with_priority<L: WildcardListener>(&self, listener: L, priority: i32) {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        self.wildcards.write().expect("wildcards lock poisoned").push((
            Arc::new(listener),
            priority,
            sequence,
        ));
    }

    /// Let a [`EventSubscriber`] register its listeners.
    pub fn subscribe(&self, subscriber: &impl EventSubscriber) {
        subscriber.subscribe(self);
    }

    /// Whether anything listens for `E` (wildcards excluded).
    pub fn has_listeners<E: Event>(&self) -> bool {
        self.listeners
            .read()
            .expect("listeners lock poisoned")
            .get(&TypeId::of::<E>())
            .is_some_and(|l| !l.is_empty())
    }

    /// Drop every listener for `E`.
    pub fn forget<E: Event>(&self) {
        self.listeners.write().expect("listeners lock poisoned").remove(&TypeId::of::<E>());
    }

    /// Drop every listener, typed and wildcard.
    pub fn forget_all(&self) {
        self.listeners.write().expect("listeners lock poisoned").clear();
        self.wildcards.write().expect("wildcards lock poisoned").clear();
    }

    // --- dispatch ----------------------------------------------------------

    /// Dispatch `event` to its listeners, then to the wildcard listeners.
    ///
    /// **Stops at the first error** and returns it, leaving the remaining
    /// listeners unrun — a listener that fails vetoes the listeners behind
    /// it. Use
    /// [`dispatch_quietly`](Self::dispatch_quietly) when listeners are
    /// independent and one failing should not silence the rest.
    pub async fn dispatch<E: Event>(&self, event: E) -> Result<()> {
        self.dispatch_as(E::event_name(), event).await
    }

    /// [`dispatch`](Self::dispatch) under an explicit name, which is what
    /// wildcard listeners and the fake's records see.
    pub async fn dispatch_as<E: Event>(&self, name: &'static str, event: E) -> Result<()> {
        let payload: Arc<dyn Any + Send + Sync> = Arc::new(event);

        if let Some(recorder) = &self.recorder {
            recorder.lock().expect("recorder lock poisoned").push(Recorded {
                type_id: TypeId::of::<E>(),
                name,
                payload,
            });
            return Ok(());
        }

        for handler in self.handlers_for::<E>() {
            handler(Arc::clone(&payload)).await?;
        }
        for wildcard in self.wildcard_handlers() {
            wildcard.handle(name, Arc::clone(&payload)).await?;
        }
        Ok(())
    }

    /// Dispatch to **every** listener regardless of failures, logging each
    /// error. Returns how many listeners failed.
    ///
    /// For events where the listeners are independent notifications — one
    /// mailer being down should not stop the audit log from being written.
    pub async fn dispatch_quietly<E: Event>(&self, event: E) -> usize {
        let name = E::event_name();
        let payload: Arc<dyn Any + Send + Sync> = Arc::new(event);

        if let Some(recorder) = &self.recorder {
            recorder.lock().expect("recorder lock poisoned").push(Recorded {
                type_id: TypeId::of::<E>(),
                name,
                payload,
            });
            return 0;
        }

        let mut failures = 0;
        for handler in self.handlers_for::<E>() {
            if let Err(e) = handler(Arc::clone(&payload)).await {
                tracing::error!(event = name, error = %e, "event listener failed");
                failures += 1;
            }
        }
        for wildcard in self.wildcard_handlers() {
            if let Err(e) = wildcard.handle(name, Arc::clone(&payload)).await {
                tracing::error!(event = name, error = %e, "wildcard event listener failed");
                failures += 1;
            }
        }
        failures
    }

    /// This event's handlers, highest priority first, snapshotted so the lock
    /// is never held across an `await`.
    fn handlers_for<E: Event>(&self) -> Vec<StoredHandler> {
        let listeners = self.listeners.read().expect("listeners lock poisoned");
        let Some(registrations) = listeners.get(&TypeId::of::<E>()) else {
            return Vec::new();
        };
        let mut sorted = registrations.clone();
        drop(listeners);

        sorted.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.sequence.cmp(&b.sequence)));
        sorted.into_iter().map(|r| r.handler).collect()
    }

    fn wildcard_handlers(&self) -> Vec<Arc<dyn WildcardListener>> {
        let wildcards = self.wildcards.read().expect("wildcards lock poisoned");
        let mut sorted = wildcards.clone();
        drop(wildcards);

        sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
        sorted.into_iter().map(|(listener, _, _)| listener).collect()
    }

    // --- assertions (faking) -----------------------------------------------

    /// Every recorded event of type `E`, in dispatch order. Always empty
    /// unless this dispatcher came from [`fake`](Self::fake).
    pub fn dispatched<E: Event>(&self) -> Vec<Arc<E>> {
        let Some(recorder) = &self.recorder else {
            return Vec::new();
        };
        recorder
            .lock()
            .expect("recorder lock poisoned")
            .iter()
            .filter(|r| r.type_id == TypeId::of::<E>())
            .filter_map(|r| Arc::clone(&r.payload).downcast::<E>().ok())
            .collect()
    }

    /// The names of every recorded event, in dispatch order.
    pub fn dispatched_names(&self) -> Vec<&'static str> {
        let Some(recorder) = &self.recorder else {
            return Vec::new();
        };
        recorder.lock().expect("recorder lock poisoned").iter().map(|r| r.name).collect()
    }

    /// Panic unless at least one `E` was dispatched.
    ///
    /// # Panics
    ///
    /// If no `E` was recorded, or the dispatcher is not faking (which would
    /// otherwise make every assertion silently pass).
    pub fn assert_dispatched<E: Event>(&self) {
        self.require_faking("assert_dispatched");
        assert!(
            !self.dispatched::<E>().is_empty(),
            "expected `{}` to have been dispatched, but it was not. Dispatched: {:?}",
            E::event_name(),
            self.dispatched_names()
        );
    }

    /// Panic unless exactly `times` events of type `E` were dispatched.
    ///
    /// # Panics
    ///
    /// If the count differs, or the dispatcher is not faking.
    pub fn assert_dispatched_times<E: Event>(&self, times: usize) {
        self.require_faking("assert_dispatched_times");
        let actual = self.dispatched::<E>().len();
        assert_eq!(
            actual,
            times,
            "expected `{}` to have been dispatched {times} time(s), but it was dispatched {actual}",
            E::event_name(),
        );
    }

    /// Panic if any `E` was dispatched.
    ///
    /// # Panics
    ///
    /// If an `E` was recorded, or the dispatcher is not faking.
    pub fn assert_not_dispatched<E: Event>(&self) {
        self.require_faking("assert_not_dispatched");
        assert!(
            self.dispatched::<E>().is_empty(),
            "expected `{}` not to have been dispatched, but it was",
            E::event_name()
        );
    }

    fn require_faking(&self, method: &str) {
        assert!(
            self.is_faking(),
            "`{method}` needs a faking dispatcher — build it with `Dispatcher::fake()`, \
             otherwise nothing is recorded and the assertion is meaningless"
        );
    }
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("event_types", &self.listeners.read().map(|l| l.len()).unwrap_or(0))
            .field("wildcards", &self.wildcards.read().map(|w| w.len()).unwrap_or(0))
            .field("faking", &self.is_faking())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Shipped(u64);
    #[derive(Debug)]
    struct Cancelled;

    /// A shared log: one handle for the listener to write through, one for
    /// the test to read.
    type Log = Arc<Mutex<Vec<String>>>;

    fn recorder() -> (Log, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        (Arc::clone(&log), log)
    }

    #[tokio::test]
    async fn delivers_to_listeners_of_the_matching_type() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        events.listen(move |event: Arc<Shipped>| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push(format!("shipped:{}", event.0));
                Ok(())
            }
        });

        events.dispatch(Shipped(7)).await.unwrap();
        events.dispatch(Cancelled).await.unwrap();
        assert_eq!(*out.lock().unwrap(), vec!["shipped:7"]);
    }

    #[tokio::test]
    async fn several_listeners_run_in_registration_order() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        for tag in ["a", "b", "c"] {
            let log = Arc::clone(&log);
            events.listen(move |_: Arc<Shipped>| {
                let log = Arc::clone(&log);
                async move {
                    log.lock().unwrap().push(tag.to_string());
                    Ok(())
                }
            });
        }

        events.dispatch(Shipped(1)).await.unwrap();
        assert_eq!(*out.lock().unwrap(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn higher_priority_runs_first() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        for (tag, priority) in [("low", -10), ("normal", 0), ("high", 10)] {
            let log = Arc::clone(&log);
            events.listen_with_priority(
                move |_: Arc<Shipped>| {
                    let log = Arc::clone(&log);
                    async move {
                        log.lock().unwrap().push(tag.to_string());
                        Ok(())
                    }
                },
                priority,
            );
        }

        events.dispatch(Shipped(1)).await.unwrap();
        assert_eq!(*out.lock().unwrap(), vec!["high", "normal", "low"]);
    }

    #[tokio::test]
    async fn an_erroring_listener_halts_the_chain() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        events.listen(|_: Arc<Shipped>| async { Err(Error::internal("veto")) });
        events.listen(move |_: Arc<Shipped>| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push("ran".into());
                Ok(())
            }
        });

        let err = events.dispatch(Shipped(1)).await.unwrap_err();
        assert_eq!(err.message(), "veto");
        assert!(out.lock().unwrap().is_empty(), "the second listener must not have run");
    }

    #[tokio::test]
    async fn dispatch_quietly_runs_everything_and_counts_failures() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        events.listen(|_: Arc<Shipped>| async { Err(Error::internal("down")) });
        events.listen(move |_: Arc<Shipped>| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push("ran".into());
                Ok(())
            }
        });

        assert_eq!(events.dispatch_quietly(Shipped(1)).await, 1);
        assert_eq!(*out.lock().unwrap(), vec!["ran"]);
    }

    #[tokio::test]
    async fn sync_listeners_need_no_async_block() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        events.listen_sync(move |event: Arc<Shipped>| {
            log.lock().unwrap().push(event.0.to_string());
            Ok(())
        });

        events.dispatch(Shipped(3)).await.unwrap();
        assert_eq!(*out.lock().unwrap(), vec!["3"]);
    }

    #[tokio::test]
    async fn wildcard_listeners_see_every_event_by_name() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        events.listen_any(move |name: &'static str, _: Arc<dyn Any + Send + Sync>| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push(name.rsplit("::").next().unwrap().to_string());
                Ok(())
            }
        });

        events.dispatch(Shipped(1)).await.unwrap();
        events.dispatch(Cancelled).await.unwrap();
        assert_eq!(*out.lock().unwrap(), vec!["Shipped", "Cancelled"]);
    }

    #[tokio::test]
    async fn a_wildcard_listener_can_recover_the_concrete_event() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        events.listen_any(move |_: &'static str, payload: Arc<dyn Any + Send + Sync>| {
            let log = Arc::clone(&log);
            async move {
                if let Ok(shipped) = payload.downcast::<Shipped>() {
                    log.lock().unwrap().push(shipped.0.to_string());
                }
                Ok(())
            }
        });

        events.dispatch(Shipped(9)).await.unwrap();
        events.dispatch(Cancelled).await.unwrap();
        assert_eq!(*out.lock().unwrap(), vec!["9"]);
    }

    #[tokio::test]
    async fn dispatch_as_overrides_the_name_wildcards_see() {
        let (log, out) = recorder();
        let events = Dispatcher::new();

        events.listen_any(move |name: &'static str, _: Arc<dyn Any + Send + Sync>| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push(name.to_string());
                Ok(())
            }
        });

        events.dispatch_as("order.shipped", Shipped(1)).await.unwrap();
        assert_eq!(*out.lock().unwrap(), vec!["order.shipped"]);
    }

    #[tokio::test]
    async fn subscribers_register_a_group_of_listeners() {
        struct Orders;
        impl EventSubscriber for Orders {
            fn subscribe(&self, events: &Dispatcher) {
                events.listen(|_: Arc<Shipped>| async { Ok(()) });
                events.listen(|_: Arc<Cancelled>| async { Ok(()) });
            }
        }

        let events = Dispatcher::new();
        events.subscribe(&Orders);
        assert!(events.has_listeners::<Shipped>());
        assert!(events.has_listeners::<Cancelled>());
    }

    #[tokio::test]
    async fn forget_removes_listeners() {
        let events = Dispatcher::new();
        events.listen(|_: Arc<Shipped>| async { Ok(()) });
        assert!(events.has_listeners::<Shipped>());

        events.forget::<Shipped>();
        assert!(!events.has_listeners::<Shipped>());
    }

    #[tokio::test]
    async fn a_fake_records_instead_of_delivering() {
        let (log, out) = recorder();
        let events = Dispatcher::fake();

        events.listen(move |_: Arc<Shipped>| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push("should not run".into());
                Ok(())
            }
        });

        events.dispatch(Shipped(1)).await.unwrap();
        events.dispatch(Shipped(2)).await.unwrap();

        assert!(out.lock().unwrap().is_empty(), "listeners must not run while faking");
        events.assert_dispatched::<Shipped>();
        events.assert_dispatched_times::<Shipped>(2);
        events.assert_not_dispatched::<Cancelled>();

        let recorded = events.dispatched::<Shipped>();
        assert_eq!(recorded.iter().map(|s| s.0).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[tokio::test]
    #[should_panic(expected = "needs a faking dispatcher")]
    async fn assertions_refuse_to_pass_vacuously_on_a_real_dispatcher() {
        let events = Dispatcher::new();
        events.dispatch(Shipped(1)).await.unwrap();
        // Would otherwise "pass" simply because nothing is ever recorded.
        events.assert_not_dispatched::<Shipped>();
    }

    #[tokio::test]
    async fn dispatching_with_no_listeners_is_fine() {
        let events = Dispatcher::new();
        events.dispatch(Shipped(1)).await.unwrap();
        assert_eq!(events.dispatch_quietly(Shipped(1)).await, 0);
    }
}
