//! The fire-and-forget side of the [`Dispatcher`]: [`publish`](Dispatcher::publish)
//! and the subscribers it repeats events to.
//!
//! `publish` is a repeater. It keeps nothing and decides nothing: it tells
//! every subscriber of an event that it happened, and in what [`Context`], and
//! returns. With nobody subscribed that is one atomic load and the event is
//! never built ([`publish_with`](Dispatcher::publish_with)). With subscribers
//! it is one lock-free push per subscriber — no lock, no syscall, no await, no
//! wait for room, and no event dropped.
//!
//! **Every subscriber lives on its own thread.** A [`Subscriber`] — Lookout,
//! an audit log, a one-line `on::<E>()` — gets a dedicated OS thread that
//! takes events off its inbox in the order they were published from any one
//! thread, and runs its handlers there: synchronous ones directly, async ones
//! on that thread's own runtime. One subscriber being slow, stuck or failing
//! touches no other subscriber and never the publisher. The thread starts
//! with the first event it is sent, and ends when its subscriber is
//! forgotten and its inbox is empty.
//!
//! **The publisher never wakes a subscriber.** Waking a sleeping thread is a
//! syscall, and the publisher must not pay one. A subscriber's thread looks
//! at its inbox itself: continuously while events arrive, then, once idle,
//! about every millisecond. So an event can reach a subscriber up to a
//! millisecond after a quiet spell — the cost is the subscriber's, never the
//! publisher's.
//!
//! ```
//! use rainier_events::{Context, Dispatcher};
//! use std::sync::Arc;
//!
//! struct OrderShipped { order_id: u64, total_cents: u64 }
//!
//! # #[tokio::main] async fn main() {
//! let events = Arc::new(Dispatcher::new());
//!
//! events
//!     .on::<OrderShipped>()
//!     .when(|e| e.total_cents > 10_000)
//!     .run(|e: Arc<OrderShipped>, _cx: Context| async move {
//!         println!("a big order shipped: {}", e.order_id);
//!         Ok(())
//!     });
//!
//! events.publish(OrderShipped { order_id: 7, total_cents: 25_000 }); // returns at once
//! events.settled().await; // tests only: wait for delivery
//! # }
//! ```

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use crossbeam_queue::SegQueue;
use rainier_support::{BoxedFuture, Result};

use crate::activity::{Activity, AsActivity};
use crate::context::Context;
use crate::event::Event;
use crate::Dispatcher;

type Payload = Arc<dyn Any + Send + Sync>;

/// What one handler makes of one event.
pub(crate) enum Delivery {
    /// Its filter said no.
    Skip,
    /// It ran synchronously, with this outcome.
    Done(Result<()>),
    /// It needs to run this future.
    Pending(BoxedFuture<Result<()>>),
}

/// A handler of one event type, type-erased.
pub(crate) type TypedHandler = Arc<dyn Fn(&Payload, &Context) -> Delivery + Send + Sync>;
/// A handler of every event.
pub(crate) type AnyHandler =
    Arc<dyn Fn(&'static str, &Payload, &Context) -> Delivery + Send + Sync>;
/// A handler of every [`Activity`].
pub(crate) type ActivityHandler = Arc<dyn Fn(&dyn Activity, &Context) -> Result<()> + Send + Sync>;

/// One event on its way to one subscriber.
struct Item {
    name: &'static str,
    type_id: TypeId,
    payload: Payload,
    cx: Context,
    /// Set when the event is an [`Activity`].
    activity: Option<AsActivity>,
}

/// One of a subscriber's threads: its inbox, and whether it is mid-event.
struct Shard {
    inbox: SegQueue<Item>,
    started: Once,
    /// Events taken off the inbox and not yet finished (async handlers
    /// included), plus one while the thread is looking. Only the subscriber's
    /// own threads touch it: the publisher's push is the push alone.
    busy: AtomicU64,
}

impl Shard {
    fn new() -> Self {
        Self { inbox: SegQueue::new(), started: Once::new(), busy: AtomicU64::new(0) }
    }
}

/// A subscriber: its handlers, its inboxes, and the threads that drain them.
pub(crate) struct Core {
    name: String,
    typed: HashMap<TypeId, Vec<TypedHandler>>,
    any: Vec<AnyHandler>,
    activities: Vec<ActivityHandler>,
    /// Registered with `listen`/`listen_any`: `dispatch` runs it inline, so
    /// a dispatched event is not also sent here.
    inline: bool,
    /// One per thread. Events of one unit of work always go to the same one.
    shards: Box<[Shard]>,
    /// Set when the subscriber is forgotten: its threads finish their inboxes
    /// and end.
    retired: AtomicBool,
    failed: AtomicU64,
}

impl Core {
    fn new(name: String, inline: bool) -> Self {
        Self {
            name,
            typed: HashMap::new(),
            any: Vec::new(),
            activities: Vec::new(),
            inline,
            shards: vec![Shard::new()].into_boxed_slice(),
            retired: AtomicBool::new(false),
            failed: AtomicU64::new(0),
        }
    }

    fn set_threads(&mut self, threads: usize) {
        self.shards = (0..threads.clamp(1, 64)).map(|_| Shard::new()).collect();
    }

    #[inline]
    fn send(self: &Arc<Self>, item: Item) {
        let index = match self.shards.len() {
            1 => 0,
            n => (item.cx.partition() % n as u64) as usize,
        };
        let shard = &self.shards[index];
        shard.inbox.push(item);
        if !shard.started.is_completed() {
            self.start(index);
        }
    }

    /// Spawn one of the subscriber's threads, once. Happens on the first
    /// event it is sent — the only time a publish does anything beyond a
    /// push.
    #[cold]
    fn start(self: &Arc<Self>, index: usize) {
        self.shards[index].started.call_once(|| {
            let core = Arc::clone(self);
            let name = match self.shards.len() {
                1 => format!("rainier-sub-{}", core.name),
                _ => format!("rainier-sub-{}-{index}", core.name),
            };
            let spawned = std::thread::Builder::new()
                .name(name.chars().take(60).collect())
                .spawn(move || core.run(index));
            if let Err(error) = spawned {
                tracing::error!(subscriber = %self.name, %error, "could not start a subscriber thread");
            }
        });
    }

    fn run(self: Arc<Self>, index: usize) {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(subscriber = %self.name, %error, "subscriber runtime failed");
                return;
            }
        };
        runtime.block_on(async move {
            let shard = &self.shards[index];
            let mut idle = 0u32;
            loop {
                let mut worked = false;
                loop {
                    // Counted busy before looking, so `settled` never sees an
                    // empty inbox and an idle thread between the pop and the
                    // handling.
                    shard.busy.fetch_add(1, Ordering::AcqRel);
                    let Some(item) = shard.inbox.pop() else {
                        shard.busy.fetch_sub(1, Ordering::AcqRel);
                        break;
                    };
                    worked = true;
                    self.handle(index, item);
                    shard.busy.fetch_sub(1, Ordering::AcqRel);
                }
                if worked {
                    idle = 0;
                    // Let async handlers this batch spawned make progress.
                    tokio::task::yield_now().await;
                    continue;
                }
                if self.retired.load(Ordering::Acquire) && shard.inbox.is_empty() {
                    break;
                }
                idle = idle.saturating_add(1);
                if idle < 64 {
                    tokio::task::yield_now().await;
                } else {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            // Let async handlers already running finish.
            while shard.busy.load(Ordering::Acquire) > 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
    }

    fn handle(self: &Arc<Self>, index: usize, item: Item) {
        let mut pending = Vec::new();
        let mut failures = Vec::new();
        if let Some(handlers) = self.typed.get(&item.type_id) {
            for handler in handlers {
                sort(handler(&item.payload, &item.cx), &mut pending, &mut failures);
            }
        }
        for handler in &self.any {
            sort(handler(item.name, &item.payload, &item.cx), &mut pending, &mut failures);
        }
        if !self.activities.is_empty() {
            if let Some(activity) = item.activity.and_then(|cast| cast(item.payload.as_ref())) {
                for handler in &self.activities {
                    if let Err(e) = handler(activity, &item.cx) {
                        failures.push(e);
                    }
                }
            }
        }
        if pending.is_empty() {
            self.finish(item, failures);
            return;
        }
        // On this subscriber's own runtime: a stalled handler stalls only
        // itself, and the next event is taken off the inbox meanwhile.
        self.shards[index].busy.fetch_add(1, Ordering::AcqRel);
        let core = Arc::clone(self);
        tokio::spawn(async move {
            for future in pending {
                if let Err(e) = future.await {
                    failures.push(e);
                }
            }
            core.finish(item, failures);
            core.shards[index].busy.fetch_sub(1, Ordering::AcqRel);
        });
    }

    fn finish(self: &Arc<Self>, item: Item, failures: Vec<rainier_support::Error>) {
        let Item { name, type_id, payload, cx, .. } = item;
        if !failures.is_empty() {
            self.failed.fetch_add(failures.len() as u64, Ordering::Relaxed);
            let reportable = !crate::report::is_failure_event(type_id);
            for error in failures {
                tracing::error!(event = name, subscriber = %self.name, error = %error, "event subscriber failed");
                if reportable {
                    tokio::spawn(crate::report::report(crate::report::ListenerFailed::new(
                        error, name,
                    )));
                }
            }
        }
        // The context — and with it the unit — is let go of only now, after
        // every handler: what makes `UnitCompleted` mean "all delivered".
        drop(cx);
        drop(payload);
    }

    fn settled(&self) -> bool {
        self.shards.iter().all(|s| s.inbox.is_empty() && s.busy.load(Ordering::Acquire) == 0)
    }
}

fn sort(
    delivery: Delivery,
    pending: &mut Vec<BoxedFuture<Result<()>>>,
    failures: &mut Vec<rainier_support::Error>,
) {
    match delivery {
        Delivery::Skip | Delivery::Done(Ok(())) => {}
        Delivery::Done(Err(e)) => failures.push(e),
        Delivery::Pending(future) => pending.push(future),
    }
}

/// Who hears what, swapped whole when a subscriber is added or forgotten.
#[derive(Default)]
pub(crate) struct Snapshot {
    /// Subscribers with a handler for the type and no wildcard handler.
    typed: HashMap<TypeId, Arc<[Arc<Core>]>>,
    /// Subscribers with a wildcard handler: they get every event.
    any: Arc<[Arc<Core>]>,
    /// Subscribers with an activity handler and no wildcard: they get every
    /// activity, and their typed handlers' events besides.
    activity: Arc<[Arc<Core>]>,
}

impl Snapshot {
    #[inline]
    pub(crate) fn hears(&self, type_id: TypeId) -> bool {
        !self.any.is_empty() || self.typed.contains_key(&type_id)
    }

    fn all(&self) -> Vec<Arc<Core>> {
        let mut all: Vec<Arc<Core>> = self.any.to_vec();
        all.extend(self.activity.iter().cloned());
        for list in self.typed.values() {
            for core in list.iter() {
                if !all.iter().any(|c| Arc::ptr_eq(c, core)) {
                    all.push(Arc::clone(core));
                }
            }
        }
        all
    }
}

/// The publish side's state on a [`Dispatcher`].
#[derive(Default)]
pub(crate) struct BusState {
    snapshot: arc_swap::ArcSwap<Snapshot>,
    /// Serialises rebuilding `snapshot` (registration only).
    building: std::sync::Mutex<()>,
    subscribers: AtomicU64,
}

impl Drop for BusState {
    /// The dispatcher is going: each subscriber thread finishes what it was
    /// sent, then ends.
    fn drop(&mut self) {
        for core in self.snapshot.load().all() {
            core.retired.store(true, Ordering::Release);
        }
    }
}

impl BusState {
    #[inline]
    pub(crate) fn hears(&self, type_id: TypeId) -> bool {
        self.snapshot.load().hears(type_id)
    }
}

impl Dispatcher {
    // --- subscribing ---------------------------------------------------------

    /// Subscribe to `E` with one handler, on a thread of its own: narrow it
    /// with [`when`](Subscription::when), then [`run`](Subscription::run) or
    /// [`run_sync`](Subscription::run_sync).
    ///
    /// A subscription is **always** deferred: it runs after `publish` (or
    /// `dispatch`) has returned, never on the raiser's path, and its `Err` is
    /// logged and reported, never returned to the raiser.
    pub fn on<E: Event>(&self) -> Subscription<'_, E> {
        Subscription { events: self, filters: Vec::new(), name: None }
    }

    /// Subscribe to **every** event — its name, payload and [`Context`] — with
    /// one handler, on a thread of its own.
    pub fn on_any(&self) -> AnySubscription<'_> {
        AnySubscription { events: self, filter: None, name: None }
    }

    /// Subscribe to every [`Activity`] — everything the framework publishes
    /// about its own work — with one synchronous handler, on a thread named
    /// `name`.
    pub fn on_activity(
        &self,
        name: impl Into<String>,
        handler: impl Fn(&dyn Activity, &Context) -> Result<()> + Send + Sync + 'static,
    ) {
        self.subscriber(name).on_activity(handler).start();
    }

    /// Declare a subscriber with several handlers that share one thread —
    /// what a component that watches many events (Lookout, an audit log)
    /// registers as. See [`Subscriber`].
    pub fn subscriber(&self, name: impl Into<String>) -> Subscriber<'_> {
        Subscriber { events: self, core: Core::new(name.into(), false) }
    }

    pub(crate) fn add_core(&self, core: Core) {
        let core = Arc::new(core);
        let _building = self.bus.building.lock().expect("bus lock poisoned");
        let current = self.bus.snapshot.load();
        let mut typed = current.typed.clone();
        let mut any = current.any.to_vec();
        let mut activity = current.activity.to_vec();
        if core.any.is_empty() {
            for type_id in core.typed.keys() {
                let mut list: Vec<_> = typed.get(type_id).map(|l| l.to_vec()).unwrap_or_default();
                list.push(Arc::clone(&core));
                typed.insert(*type_id, list.into());
            }
            if !core.activities.is_empty() {
                activity.push(Arc::clone(&core));
            }
        } else {
            any.push(Arc::clone(&core));
        }
        self.bus.subscribers.fetch_add(1, Ordering::Relaxed);
        self.bus.snapshot.store(Arc::new(Snapshot {
            typed,
            any: any.into(),
            activity: activity.into(),
        }));
    }

    pub(crate) fn forget_published<E: Event>(&self) {
        let _building = self.bus.building.lock().expect("bus lock poisoned");
        let current = self.bus.snapshot.load();
        let mut typed = current.typed.clone();
        if let Some(list) = typed.remove(&TypeId::of::<E>()) {
            for core in list.iter() {
                core.retired.store(true, Ordering::Release);
            }
        }
        self.bus.snapshot.store(Arc::new(Snapshot {
            typed,
            any: Arc::clone(&current.any),
            activity: Arc::clone(&current.activity),
        }));
    }

    pub(crate) fn forget_all_published(&self) {
        let _building = self.bus.building.lock().expect("bus lock poisoned");
        for core in self.bus.snapshot.load().all() {
            core.retired.store(true, Ordering::Release);
        }
        self.bus.snapshot.store(Arc::new(Snapshot::default()));
    }

    // --- publishing ----------------------------------------------------------

    /// Whether anything would hear `E` when published: a subscriber, a
    /// listener, or a wildcard. One atomic load.
    #[inline]
    pub fn has_subscribers<E: Event>(&self) -> bool {
        self.recorder.is_some() || self.bus.hears(TypeId::of::<E>())
    }

    /// Tell every subscriber of `E` — and every wildcard — that `event`
    /// happened, in the calling code's [`Context`]. Returns at once.
    ///
    /// Fire and forget: nothing a subscriber does can delay, fail or reach
    /// the caller. With nobody subscribed it costs one atomic load; build
    /// expensive events with [`publish_with`](Self::publish_with) so they are
    /// not built for nobody.
    #[inline]
    pub fn publish<E: Event>(&self, event: E) {
        self.publish_with(move || event);
    }

    /// [`publish`](Self::publish), building the event only if somebody will
    /// hear it.
    #[inline]
    pub fn publish_with<E: Event>(&self, build: impl FnOnce() -> E) {
        if !self.has_subscribers::<E>() {
            return;
        }
        self.publish_built(E::event_name(), Context::current(), Arc::new(build()), false);
    }

    /// [`publish`](Self::publish) in an explicit context — for an event
    /// raised on behalf of work that is not current here.
    pub fn publish_in<E: Event>(&self, cx: Context, event: E) {
        if !self.has_subscribers::<E>() {
            return;
        }
        self.publish_built(E::event_name(), cx, Arc::new(event), false);
    }

    /// Send a built event to every subscriber that hears it. `from_dispatch`
    /// is set by `dispatch`, which has run the inline listeners itself.
    pub(crate) fn publish_built<E: Event>(
        &self,
        name: &'static str,
        cx: Context,
        payload: Arc<E>,
        from_dispatch: bool,
    ) {
        if let Some(recorder) = &self.recorder {
            if !from_dispatch {
                recorder.lock().expect("recorder lock poisoned").push(
                    crate::dispatcher::Recorded { type_id: TypeId::of::<E>(), name, payload },
                );
            }
            return;
        }
        let type_id = TypeId::of::<E>();
        let payload: Payload = payload;
        let snapshot = self.bus.snapshot.load();
        let typed = snapshot.typed.get(&type_id);
        for core in typed.into_iter().flat_map(|l| l.iter()).chain(snapshot.any.iter()) {
            if from_dispatch && core.inline {
                continue;
            }
            core.send(Item {
                name,
                type_id,
                payload: Arc::clone(&payload),
                cx: cx.clone(),
                activity: None,
            });
        }
    }

    /// Whether any subscriber hears every activity.
    #[inline]
    pub(crate) fn hears_activities(&self) -> bool {
        let snapshot = self.bus.snapshot.load();
        !snapshot.activity.is_empty() || !snapshot.any.is_empty()
    }

    /// Send a built [`Activity`] to its type's subscribers, the wildcards and
    /// the activity subscribers — each subscriber once.
    pub(crate) fn publish_activity_built<E: Activity>(&self, cx: Context, payload: Arc<E>) {
        let name = E::event_name();
        if let Some(recorder) = &self.recorder {
            recorder.lock().expect("recorder lock poisoned").push(crate::dispatcher::Recorded {
                type_id: TypeId::of::<E>(),
                name,
                payload,
            });
            return;
        }
        let type_id = TypeId::of::<E>();
        let payload: Payload = payload;
        let cast: AsActivity = crate::activity::as_activity::<E>;
        let snapshot = self.bus.snapshot.load();
        let typed = snapshot.typed.get(&type_id);
        let typed_only =
            typed.into_iter().flat_map(|l| l.iter()).filter(|c| c.activities.is_empty());
        for core in typed_only.chain(snapshot.any.iter()).chain(snapshot.activity.iter()) {
            core.send(Item {
                name,
                type_id,
                payload: Arc::clone(&payload),
                cx: cx.clone(),
                activity: Some(cast),
            });
        }
    }

    // --- observing -----------------------------------------------------------

    /// Wait until every event published so far has been handled by every
    /// subscriber. For tests: production code never waits on the bus.
    pub async fn settled(&self) {
        loop {
            if self.bus.snapshot.load().all().iter().all(|core| core.settled()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// How many subscriber handler runs have returned `Err`.
    pub fn subscriber_failures(&self) -> u64 {
        self.bus.snapshot.load().all().iter().map(|c| c.failed.load(Ordering::Relaxed)).sum()
    }

    /// How many subscribers — each its own thread once it has heard an
    /// event — have been registered.
    pub fn subscriber_count(&self) -> u64 {
        self.bus.subscribers.load(Ordering::Relaxed)
    }

    /// A listener registered with `listen`, as the subscriber that runs it
    /// for published events.
    pub(crate) fn add_inline_typed(
        &self,
        type_id: TypeId,
        name: &'static str,
        handler: TypedHandler,
    ) {
        let mut core = Core::new(format!("listener-{}", short(name)), true);
        core.typed.insert(type_id, vec![handler]);
        self.add_core(core);
    }

    /// A wildcard registered with `listen_any`, likewise.
    pub(crate) fn add_inline_any(&self, handler: AnyHandler) {
        let mut core = Core::new("wildcard-listener".into(), true);
        core.any.push(handler);
        self.add_core(core);
    }
}

// --- subscriptions -----------------------------------------------------------

/// A subscriber with several handlers sharing one thread:
/// [`Dispatcher::subscriber`].
///
/// ```
/// # use rainier_events::Dispatcher;
/// # struct QueryRan; struct CacheHit;
/// let events = Dispatcher::new();
/// events
///     .subscriber("metrics")
///     .on(|_: &QueryRan, _cx| Ok(()))
///     .on(|_: &CacheHit, _cx| Ok(()))
///     .start();
/// ```
#[must_use = "a subscriber does nothing until `start` registers it"]
pub struct Subscriber<'a> {
    events: &'a Dispatcher,
    core: Core,
}

impl Subscriber<'_> {
    /// Handle `E` synchronously on the subscriber's thread.
    pub fn on<E: Event>(
        mut self,
        handler: impl Fn(&E, &Context) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        let handler: TypedHandler =
            Arc::new(move |payload, cx| match payload.downcast_ref::<E>() {
                Some(event) => Delivery::Done(handler(event, cx)),
                None => Delivery::Skip,
            });
        self.core.typed.entry(TypeId::of::<E>()).or_default().push(handler);
        self
    }

    /// Handle `E` with an async handler, run on the subscriber's own runtime.
    pub fn on_async<E, F, Fut>(mut self, handler: F) -> Self
    where
        E: Event,
        F: Fn(Arc<E>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let handler: TypedHandler =
            Arc::new(move |payload, cx| match Arc::clone(payload).downcast::<E>() {
                Ok(event) => Delivery::Pending(Box::pin(handler(event, cx.clone()))),
                Err(_) => Delivery::Skip,
            });
        self.core.typed.entry(TypeId::of::<E>()).or_default().push(handler);
        self
    }

    /// Handle every [`Activity`] — everything the framework publishes about
    /// its own work — synchronously, whatever its type.
    pub fn on_activity(
        mut self,
        handler: impl Fn(&dyn Activity, &Context) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.core.activities.push(Arc::new(handler));
        self
    }

    /// Handle every event synchronously.
    pub fn on_any(
        mut self,
        handler: impl Fn(&'static str, &(dyn Any + Send + Sync), &Context) -> Result<()>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.core.any.push(Arc::new(move |name, payload, cx| {
            Delivery::Done(handler(name, payload.as_ref(), cx))
        }));
        self
    }

    /// Run the subscriber on `threads` threads instead of one — for a
    /// subscriber with more work per event than one thread keeps up with.
    /// Every event of one unit of work goes to the same thread, in order, its
    /// [`UnitCompleted`](crate::UnitCompleted) last; events outside every
    /// unit are spread by the thread that published them. The publisher's
    /// cost is unchanged: one push.
    pub fn threads(mut self, threads: usize) -> Self {
        self.core.set_threads(threads);
        self
    }

    /// Register the subscriber. Its thread starts with the first event it is
    /// sent.
    pub fn start(self) {
        self.events.add_core(self.core);
    }
}

type Filter<E> = Box<dyn Fn(&E) -> bool + Send + Sync>;

/// A one-handler subscription to `E` being declared: [`Dispatcher::on`].
#[must_use = "a subscription does nothing until `run` or `run_sync` registers it"]
pub struct Subscription<'a, E: Event> {
    events: &'a Dispatcher,
    filters: Vec<Filter<E>>,
    name: Option<String>,
}

impl<E: Event> Subscription<'_, E> {
    /// Only events for which `predicate` is true. Several `when`s must all
    /// hold. Evaluated on the subscriber's thread, never the publisher's.
    pub fn when(mut self, predicate: impl Fn(&E) -> bool + Send + Sync + 'static) -> Self {
        self.filters.push(Box::new(predicate));
        self
    }

    /// Name the subscriber's thread (`rainier-sub-<name>`) for profilers and
    /// logs. Defaults to the event's type name.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn core(&self) -> Core {
        let name = self.name.clone().unwrap_or_else(|| short(E::event_name()).to_string());
        Core::new(name, false)
    }

    /// Register `handler`, an async closure taking the event and its
    /// [`Context`], run on the subscriber's own runtime.
    pub fn run<F, Fut>(self, handler: F)
    where
        F: Fn(Arc<E>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let mut core = self.core();
        let filters = self.filters;
        let handler: TypedHandler = Arc::new(move |payload, cx| {
            let Ok(event) = Arc::clone(payload).downcast::<E>() else { return Delivery::Skip };
            if !filters.iter().all(|f| f(&event)) {
                return Delivery::Skip;
            }
            Delivery::Pending(Box::pin(handler(event, cx.clone())))
        });
        core.typed.insert(TypeId::of::<E>(), vec![handler]);
        self.events.add_core(core);
    }

    /// Register `handler`, a synchronous closure run on the subscriber's
    /// thread.
    pub fn run_sync<F>(self, handler: F)
    where
        F: Fn(&E, &Context) -> Result<()> + Send + Sync + 'static,
    {
        let mut core = self.core();
        let filters = self.filters;
        let handler: TypedHandler = Arc::new(move |payload, cx| {
            let Some(event) = payload.downcast_ref::<E>() else { return Delivery::Skip };
            if !filters.iter().all(|f| f(event)) {
                return Delivery::Skip;
            }
            Delivery::Done(handler(event, cx))
        });
        core.typed.insert(TypeId::of::<E>(), vec![handler]);
        self.events.add_core(core);
    }
}

type AnyFilter = Box<dyn Fn(&'static str, &(dyn Any + Send + Sync)) -> bool + Send + Sync>;

/// A one-handler subscription to every event being declared:
/// [`Dispatcher::on_any`].
#[must_use = "a subscription does nothing until `run` or `run_sync` registers it"]
pub struct AnySubscription<'a> {
    events: &'a Dispatcher,
    filter: Option<AnyFilter>,
    name: Option<String>,
}

impl AnySubscription<'_> {
    /// Only events for which `predicate` — given the name and payload — is
    /// true.
    pub fn when(
        mut self,
        predicate: impl Fn(&'static str, &(dyn Any + Send + Sync)) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.filter = Some(Box::new(predicate));
        self
    }

    /// Name the subscriber's thread.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn core(&self) -> Core {
        Core::new(self.name.clone().unwrap_or_else(|| "any".into()), false)
    }

    /// Register an async `handler` for every event.
    pub fn run<F, Fut>(self, handler: F)
    where
        F: Fn(&'static str, Arc<dyn Any + Send + Sync>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let mut core = self.core();
        let filter = self.filter;
        core.any.push(Arc::new(move |name, payload, cx| {
            if filter.as_ref().is_some_and(|f| !f(name, payload.as_ref())) {
                return Delivery::Skip;
            }
            Delivery::Pending(Box::pin(handler(name, Arc::clone(payload), cx.clone())))
        }));
        self.events.add_core(core);
    }

    /// Register a synchronous `handler` for every event.
    pub fn run_sync<F>(self, handler: F)
    where
        F: Fn(&'static str, &(dyn Any + Send + Sync), &Context) -> Result<()>
            + Send
            + Sync
            + 'static,
    {
        let mut core = self.core();
        let filter = self.filter;
        core.any.push(Arc::new(move |name, payload, cx| {
            if filter.as_ref().is_some_and(|f| !f(name, payload.as_ref())) {
                return Delivery::Skip;
            }
            Delivery::Done(handler(name, payload.as_ref(), cx))
        }));
        self.events.add_core(core);
    }
}

fn short(name: &'static str) -> &'static str {
    let base = name.split('<').next().unwrap_or(name);
    base.rsplit("::").next().unwrap_or(base)
}

// --- the process-wide bus -----------------------------------------------------

static INSTALLED: arc_swap::ArcSwapOption<Dispatcher> = arc_swap::ArcSwapOption::const_empty();

thread_local! {
    static OVERRIDE: std::cell::RefCell<Vec<Arc<Dispatcher>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Make `events` the process-wide bus: what [`bus`] answers outside every
/// unit, and what framework components with no dispatcher of their own
/// publish on. Boot does this; the last call wins.
pub fn install(events: Arc<Dispatcher>) {
    INSTALLED.store(Some(events));
}

/// The installed process-wide bus, if any.
pub fn installed() -> Option<Arc<Dispatcher>> {
    INSTALLED.load_full()
}

/// Run `f` with the bus the calling code publishes on — the current unit's,
/// else this thread's [`set_default`], else the [`install`]ed one — borrowed:
/// no lock, and no reference count touched. What every hot-path publish goes
/// through.
#[inline]
pub fn with_bus<R>(f: impl FnOnce(&Dispatcher) -> R) -> Option<R> {
    let mut f = Some(f);
    if let Ok(result) = Context::with_unit_events(|events| (f.take().expect("called once"))(events))
    {
        return Some(result);
    }
    let f = f.expect("not called");
    if let Ok(Some(events)) = OVERRIDE.try_with(|o| o.borrow().last().cloned()) {
        return Some(f(&events));
    }
    let guard = INSTALLED.load();
    guard.as_ref().map(|events| f(events))
}

/// Make `events` the bus for this thread until the guard drops — a test's
/// way to keep its events apart from a parallel test's.
pub fn set_default(events: Arc<Dispatcher>) -> DefaultGuard {
    OVERRIDE.with(|o| o.borrow_mut().push(events));
    DefaultGuard { _not_send: std::marker::PhantomData }
}

/// Restores the previous bus when dropped: [`set_default`].
pub struct DefaultGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for DefaultGuard {
    fn drop(&mut self) {
        OVERRIDE.with(|o| {
            o.borrow_mut().pop();
        });
    }
}

impl std::fmt::Debug for DefaultGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DefaultGuard")
    }
}

/// The bus the calling code publishes on: the current unit's, else this
/// thread's [`set_default`], else the [`install`]ed one.
pub fn bus() -> Option<Arc<Dispatcher>> {
    if let Some(events) = Context::current_unit().and_then(|u| u.events()) {
        return Some(events);
    }
    if let Some(events) = OVERRIDE.with(|o| o.borrow().last().cloned()) {
        return Some(events);
    }
    installed()
}

/// [`Dispatcher::publish_with`] on [`bus`] — how framework code with no
/// dispatcher in reach (a query, a cache call) publishes. With no bus, or
/// nobody subscribed, the event is never built.
#[inline]
pub fn publish_with<E: Event>(build: impl FnOnce() -> E) {
    with_bus(|events| events.publish_with(build));
}

/// [`publish_with`] for an event already built.
#[inline]
pub fn publish<E: Event>(event: E) {
    publish_with(move || event);
}

/// Whether [`bus`] has a subscriber for `E` — what a publisher that must
/// gather data before building its event asks first.
#[inline]
pub fn has_subscribers<E: Event>() -> bool {
    with_bus(|events| events.has_subscribers::<E>()).unwrap_or(false)
}
