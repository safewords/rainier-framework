//! The recorder: the [`Lookout`] handle, the in-progress batches, what happens
//! when one closes, and the background writer.
//!
//! # The life of a batch
//!
//! 1. **Open.** The [layer](crate::layer) sees a unit-of-work span start —
//!    `http.server.request`, `queue.process`, `console.command`,
//!    `schedule.task` — with no batch around it, and opens one. Scope and
//!    sampling that can be decided now are decided now: the request's path
//!    against the dashboard's own, whether jobs / commands / tasks are watched
//!    at all, and the sample rate. A rejected batch still exists — so its
//!    failures have somewhere to go — but buffers nothing else.
//! 2. **Fill.** Every span inside it that Lookout knows (`db.query`,
//!    `cache.*`, …) becomes an [`Entry`] when it closes; the bus watchers add
//!    failures and model changes; the middleware adds the route, the status
//!    and who was signed in. As soon as the router names the route, the route
//!    stage of [`Scope`](crate::config::Scope) runs — before the handler, so
//!    an excluded route's queries are never buffered.
//! 3. **Close.** When the root span closes, `finalize` runs the rest: guard
//!    and user scope, per-watcher rules, slow thresholds, N+1 detection, the
//!    keep rule. A kept batch is `try_send`-ed into a bounded channel; when
//!    the channel is full the batch is dropped and [`Lookout::dropped`]
//!    counts it. Nothing on the request path waits for the store.
//! 4. **Write.** The writer task ([`Lookout::start_writer`]) drains the
//!    channel every `flush_interval_ms`, or as soon as `flush_batch_size`
//!    batches are waiting, and hands them to [`Store::store`] inside a span
//!    marked `rainier.lookout.internal`, which the layer ignores — the
//!    database store's own queries are not recorded.
//!
//! # Correlating the bus with a batch
//!
//! Event listeners run inline, on the task that dispatched the event. The
//! layer keeps a per-thread stack of the batches whose spans are entered on
//! that thread (pushed in `on_enter`, popped in `on_exit`), so a listener asks
//! "which batch is the code that called me in?" and gets the innermost one.
//! No task-local is needed, and it works in any code that runs inside an
//! instrumented future.
//!
//! One report arrives *after* its span: the HTTP kernel reports a failed
//! request once `handle` has returned, and the server span closed with it. So
//! a request batch that closes with a status of 400 or more is **parked** for
//! a moment instead of finalized, and remembered on the closing thread; the
//! `HttpRequestFailed` that follows on the same thread joins it and releases
//! it. Parked batches nobody claims are released by the writer's next tick
//! (or [`Lookout::flush`]).
//!
//! # Work outside any unit of work
//!
//! Spans that are not inside a batch — a query at boot, a cache read in a
//! background task — are **not recorded**. A batch per stray span would bury
//! the batches people look for under a heap of one-entry ones. Failures are
//! the exception: a failure with no batch around it becomes a
//! [`BatchKind::Other`] batch holding just that failure, so nothing reported
//! is lost to scoping or sampling.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rainier_http::Response;
use tokio::sync::{mpsc, Notify};

use crate::config::{glob, LookoutConfig};
use crate::entry::{Batch, BatchKind, Entry, EntryType};
use crate::layer::LookoutLayer;
use crate::middleware::LookoutMiddleware;
use crate::store::Store;

/// The span field that marks Lookout's own work. Anything inside a span that
/// declares it is not recorded.
pub const INTERNAL_FIELD: &str = "rainier.lookout.internal";

/// The most entries one batch holds. A long command that runs a million
/// queries keeps the first ones and counts the rest in the batch's
/// `truncated:<n>` tag.
pub const MAX_ENTRIES_PER_BATCH: usize = 2_000;

/// How long a failed request's batch waits for its failure report.
const PARK_FOR: Duration = Duration::from_millis(250);

/// How many batches may be parked at once; past it, the oldest is released.
const MAX_PARKED: usize = 256;

/// Decides who a response was for: `(guard, user id)`. See
/// [`Lookout::identify_with`].
pub type IdentifyHook = Arc<dyn Fn(&Response) -> Option<(String, Option<String>)> + Send + Sync>;

/// Lookout: the handle an application holds.
///
/// Cheap to clone; every clone shares the same buffers, channel and counters.
///
/// ```no_run
/// use std::sync::Arc;
/// use rainier_lookout::{Lookout, LookoutConfig, MemoryStore};
/// use tracing_subscriber::layer::SubscriberExt;
///
/// # async fn boot(events: &rainier_events::Dispatcher) {
/// let lookout = Lookout::new(LookoutConfig::from_env().unwrap(), Arc::new(MemoryStore::new(500)));
/// let subscriber = tracing_subscriber::registry().with(lookout.layer());
/// tracing::subscriber::set_global_default(subscriber).unwrap();
/// lookout.listen(events);
/// let writer = lookout.start_writer();
/// // … serve, with `lookout.middleware()` in the global stack …
/// writer.shutdown().await;
/// # }
/// ```
#[derive(Clone)]
pub struct Lookout {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Lookout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lookout")
            .field("enabled", &self.inner.config.enabled)
            .field("store", &self.inner.store.name())
            .field("paused", &self.is_paused())
            .field("dropped", &self.dropped())
            .finish()
    }
}

impl Lookout {
    /// A recorder for `config`, writing to `store`.
    ///
    /// With `config.enabled` off every part of it — the layer, the
    /// middleware, the listeners, the writer — does nothing.
    pub fn new(config: LookoutConfig, store: Arc<dyn Store>) -> Self {
        let (tx, rx) = mpsc::channel(config.ingest.channel_capacity.max(1));
        let log_level = parse_level(&config.log_level);
        Self {
            inner: Arc::new(Inner {
                config,
                store,
                log_level,
                host: std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()),
                paused: AtomicBool::new(false),
                dropped: AtomicU64::new(0),
                queued: AtomicUsize::new(0),
                tx,
                rx: tokio::sync::Mutex::new(rx),
                wake: Notify::new(),
                writer_started: AtomicBool::new(false),
                open: Mutex::new(HashMap::new()),
                parked: Mutex::new(Vec::new()),
                identify: RwLock::new(None),
                last_store_error: Mutex::new(None),
            }),
        }
    }

    /// The configuration it was built with.
    pub fn config(&self) -> &LookoutConfig {
        &self.inner.config
    }

    /// Where batches go.
    pub fn store(&self) -> &Arc<dyn Store> {
        &self.inner.store
    }

    /// Whether it records anything at all.
    pub fn is_enabled(&self) -> bool {
        self.inner.config.enabled
    }

    /// The `tracing` layer that turns spans into entries. Add it to the
    /// application's subscriber.
    pub fn layer(&self) -> LookoutLayer {
        LookoutLayer::new(Arc::clone(&self.inner))
    }

    /// The global middleware that attaches the route, status and user to a
    /// request's batch. Put it anywhere in the global stack.
    pub fn middleware(&self) -> LookoutMiddleware {
        LookoutMiddleware::new(self.clone())
    }

    /// Register the bus watchers on `events`: failures, and — when the model
    /// watcher is on — model and job lifecycle events. See
    /// [`watchers`](crate::watchers).
    pub fn listen(&self, events: &rainier_events::Dispatcher) {
        crate::watchers::listen(self, events);
    }

    /// Decide the guard and user of a request from its response.
    ///
    /// The framework's authentication middleware leaves a marker on the
    /// response; this is where it is read. Without a hook, batches have no
    /// guard or user, and guard / user scope sees every request as
    /// unauthenticated.
    pub fn identify_with(
        &self,
        hook: impl Fn(&Response) -> Option<(String, Option<String>)> + Send + Sync + 'static,
    ) {
        *self.inner.identify.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    /// Spawn the background writer on the current tokio runtime.
    ///
    /// Only the first call starts one; later calls return a handle that does
    /// nothing. Disabled, nothing is spawned. Dropping the handle stops the
    /// writer after a last flush; [`WriterHandle::shutdown`] does the same
    /// and waits for it.
    pub fn start_writer(&self) -> WriterHandle {
        if !self.inner.config.enabled || self.inner.writer_started.swap(true, Ordering::SeqCst) {
            return WriterHandle { stop: None, task: None };
        }
        let inner = Arc::clone(&self.inner);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let every = Duration::from_millis(inner.config.ingest.flush_interval_ms.max(1));
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = inner.wake.notified() => {}
                    _ = &mut stop_rx => {
                        inner.release_parked(Duration::ZERO);
                        inner.drain_and_store().await;
                        break;
                    }
                }
                inner.release_parked(PARK_FOR);
                inner.drain_and_store().await;
            }
        });
        WriterHandle { stop: Some(stop_tx), task: Some(task) }
    }

    /// Write everything closed so far, now: parked batches are released and
    /// the channel is drained into the store. For tests and shutdown.
    pub async fn flush(&self) {
        if !self.inner.config.enabled {
            return;
        }
        self.inner.release_parked(Duration::ZERO);
        self.inner.drain_and_store().await;
    }

    /// How many closed batches were dropped because the channel was full.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Stop opening batches until [`resume`](Self::resume). Batches already
    /// open finish normally.
    pub fn pause(&self) {
        self.inner.paused.store(true, Ordering::SeqCst);
    }

    /// Start opening batches again.
    pub fn resume(&self) {
        self.inner.paused.store(false, Ordering::SeqCst);
    }

    /// Whether [`pause`](Self::pause) is in force.
    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::SeqCst)
    }

    /// Tag a batch that is still open: the one with id `batch` (its own id or
    /// its trace id), or with `None` the one the calling code is running in.
    ///
    /// A tag in [`Scope::monitored_tags`](crate::config::Scope::monitored_tags)
    /// keeps the batch whatever the sampling and keep rule. Returns whether a
    /// batch was found.
    pub fn tag(&self, batch: Option<&str>, tag: impl Into<String>) -> bool {
        let cell = match batch {
            None => self.inner.current_batch(),
            Some(id) => self
                .inner
                .open
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .and_then(Weak::upgrade),
        };
        match cell {
            Some(cell) => {
                let mut state = cell.lock();
                match state.as_mut() {
                    Some(pending) => {
                        pending.tags.insert(tag.into());
                        true
                    }
                    None => false,
                }
            }
            None => false,
        }
    }

    /// The id of the batch the calling code is running in, if any — its
    /// trace id once one is known.
    pub fn current_batch_id(&self) -> Option<String> {
        let cell = self.inner.current_batch()?;
        let state = cell.lock();
        state.as_ref().map(|p| p.trace_id.clone().unwrap_or_else(|| p.id.clone()))
    }
}

/// The running writer. Dropping it asks the writer to flush and stop;
/// [`shutdown`](Self::shutdown) also waits for it.
#[derive(Debug)]
pub struct WriterHandle {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl WriterHandle {
    /// Whether this handle owns a running writer (the first
    /// [`Lookout::start_writer`] of an enabled Lookout).
    pub fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Flush what is waiting, stop the writer and wait for it.
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

// --- shared state ------------------------------------------------------------

/// Why a batch stopped buffering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// Out of scope: a route, path, guard or user excluded it, or the kind of
    /// work is not watched.
    Scope,
    /// The sample rate said no.
    Sampled,
}

/// An open batch, shared by every span inside it.
pub(crate) struct BatchCell {
    /// `None` once closed.
    state: Mutex<Option<Pending>>,
    /// A fast "do not bother building entries" flag, mirrored from
    /// `Pending::rejected`.
    rejected: AtomicBool,
    /// What started it.
    pub(crate) kind: BatchKind,
}

impl BatchCell {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn is_rejected(&self) -> bool {
        self.rejected.load(Ordering::Relaxed)
    }
}

/// What an open batch has gathered.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub(crate) id: String,
    pub(crate) kind: BatchKind,
    pub(crate) started: Instant,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) duration_us: Option<u64>,
    pub(crate) trace_id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) method: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) route_name: Option<String>,
    pub(crate) route_pattern: Option<String>,
    pub(crate) status: Option<u16>,
    pub(crate) guard: Option<String>,
    pub(crate) user_id: Option<String>,
    pub(crate) tags: BTreeSet<String>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) truncated: usize,
    pub(crate) rejected: Option<Rejection>,
    /// The sample rate said no, but monitored tags are configured, so the
    /// batch buffers anyway and is decided at close.
    pub(crate) sampled_out: bool,
    pub(crate) failed_job: bool,
    pub(crate) request_headers: Option<serde_json::Value>,
    pub(crate) request_body: Option<Vec<u8>>,
}

impl Pending {
    pub(crate) fn new(kind: BatchKind) -> Self {
        Self {
            id: new_id(),
            kind,
            started: Instant::now(),
            started_at: Utc::now(),
            duration_us: None,
            trace_id: None,
            name: None,
            method: None,
            path: None,
            route_name: None,
            route_pattern: None,
            status: None,
            guard: None,
            user_id: None,
            tags: BTreeSet::new(),
            entries: Vec::new(),
            truncated: 0,
            rejected: None,
            sampled_out: false,
            failed_job: false,
            request_headers: None,
            request_body: None,
        }
    }

    /// Add an entry, within [`MAX_ENTRIES_PER_BATCH`]. Exceptions always fit.
    pub(crate) fn push(&mut self, entry: Entry) {
        if entry.entry_type != EntryType::Exception && self.entries.len() >= MAX_ENTRIES_PER_BATCH {
            self.truncated += 1;
            return;
        }
        self.entries.push(entry);
    }
}

pub(crate) struct Inner {
    pub(crate) config: LookoutConfig,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) log_level: Option<tracing::Level>,
    host: Option<String>,
    paused: AtomicBool,
    dropped: AtomicU64,
    queued: AtomicUsize,
    tx: mpsc::Sender<Batch>,
    rx: tokio::sync::Mutex<mpsc::Receiver<Batch>>,
    wake: Notify,
    writer_started: AtomicBool,
    /// Open batches by id (and trace id), for [`Lookout::tag`].
    open: Mutex<HashMap<String, Weak<BatchCell>>>,
    /// Closed request batches waiting for their failure report.
    parked: Mutex<Vec<(Instant, Pending)>>,
    identify: RwLock<Option<IdentifyHook>>,
    last_store_error: Mutex<Option<Instant>>,
}

thread_local! {
    /// The batches whose spans are entered on this thread, innermost last,
    /// tagged with the `Inner` they belong to.
    static CURRENT: std::cell::RefCell<Vec<(usize, Arc<BatchCell>)>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// The request batch this thread parked last: `(inner, batch id)`.
    static RECENT: std::cell::RefCell<Option<(usize, String)>> =
        const { std::cell::RefCell::new(None) };
}

impl Inner {
    fn key(&self) -> usize {
        self as *const Inner as usize
    }

    /// Whether to do anything at all right now.
    pub(crate) fn active(&self) -> bool {
        self.config.enabled && !self.paused.load(Ordering::Relaxed)
    }

    pub(crate) fn enter(&self, cell: &Arc<BatchCell>) {
        let key = self.key();
        CURRENT.with(|stack| stack.borrow_mut().push((key, Arc::clone(cell))));
    }

    pub(crate) fn exit(&self, cell: &Arc<BatchCell>) {
        let key = self.key();
        CURRENT.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(at) = stack.iter().rposition(|(k, c)| *k == key && Arc::ptr_eq(c, cell)) {
                stack.remove(at);
            }
        });
    }

    /// The innermost open batch of this Lookout the calling code runs in.
    pub(crate) fn current_batch(&self) -> Option<Arc<BatchCell>> {
        let key = self.key();
        CURRENT.with(|stack| {
            stack.borrow().iter().rev().find(|(k, _)| *k == key).map(|(_, c)| Arc::clone(c))
        })
    }

    // --- opening ---------------------------------------------------------

    /// Open a batch for a root span. `path` and `name` are what the root span
    /// carried at creation.
    pub(crate) fn open_batch(
        &self,
        kind: BatchKind,
        path: Option<&str>,
        name: Option<&str>,
    ) -> Arc<BatchCell> {
        let mut pending = Pending::new(kind);
        pending.path = path.map(str::to_string);
        pending.name = name.map(str::to_string);
        let scope = &self.config.scope;
        let (watched, rate) = match kind {
            BatchKind::Request => {
                (path.is_none_or(|p| !self.is_dashboard_path(p)), self.config.request_sample_rate)
            }
            BatchKind::Job => (scope.jobs, self.config.job_sample_rate),
            BatchKind::Command => (
                scope.commands && name.is_none_or(|n| !n.starts_with("lookout:")),
                self.config.command_sample_rate,
            ),
            BatchKind::ScheduledTask => (scope.scheduled_tasks, self.config.command_sample_rate),
            _ => (true, 1.0),
        };
        if !watched {
            pending.rejected = Some(Rejection::Scope);
        } else if !sample(rate) {
            if scope.monitored_tags.is_empty() {
                pending.rejected = Some(Rejection::Sampled);
            } else {
                pending.sampled_out = true;
            }
        }
        let rejected = pending.rejected.is_some();
        let id = pending.id.clone();
        let cell = Arc::new(BatchCell {
            state: Mutex::new(Some(pending)),
            rejected: AtomicBool::new(rejected),
            kind,
        });
        self.open.lock().unwrap_or_else(|e| e.into_inner()).insert(id, Arc::downgrade(&cell));
        cell
    }

    fn is_dashboard_path(&self, path: &str) -> bool {
        let dashboard = self.config.dashboard.path.trim_end_matches('/');
        !dashboard.is_empty()
            && (path == dashboard
                || path.strip_prefix(dashboard).is_some_and(|rest| rest.starts_with('/')))
    }

    /// Stop buffering into `cell` and drop what it buffered — except
    /// failures, which are always kept.
    pub(crate) fn reject(&self, cell: &BatchCell, why: Rejection) {
        let mut state = cell.lock();
        if let Some(pending) = state.as_mut() {
            if pending.rejected.is_none() {
                pending.rejected = Some(why);
                pending.entries.retain(|e| e.entry_type == EntryType::Exception);
            }
        }
        cell.rejected.store(true, Ordering::Relaxed);
    }

    /// The root span recorded its trace id.
    pub(crate) fn set_trace_id(&self, cell: &Arc<BatchCell>, trace_id: &str) {
        if let Some(pending) = cell.lock().as_mut() {
            pending.trace_id = Some(trace_id.to_string());
        }
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(trace_id.to_string(), Arc::downgrade(cell));
    }

    /// The route became known: the route stage of the scope.
    pub(crate) fn set_route(&self, cell: &BatchCell, name: Option<&str>, pattern: Option<&str>) {
        {
            let mut state = cell.lock();
            let Some(pending) = state.as_mut() else { return };
            if let Some(name) = name {
                pending.route_name = Some(name.to_string());
            }
            if let Some(pattern) = pattern {
                pending.route_pattern = Some(pattern.to_string());
            }
        }
        if !self.route_in_scope(cell) {
            self.reject(cell, Rejection::Scope);
        }
    }

    /// Routes are matched by name, or by pattern for an unnamed route;
    /// requests that matched no route by their path.
    fn route_in_scope(&self, cell: &BatchCell) -> bool {
        let state = cell.lock();
        state.as_ref().is_none_or(|pending| self.pending_in_scope(pending))
    }

    fn pending_in_scope(&self, pending: &Pending) -> bool {
        let scope = &self.config.scope;
        match (&pending.route_name, &pending.route_pattern) {
            (Some(name), _) => scope.routes.allows(name),
            (None, Some(pattern)) => scope.routes.allows(pattern),
            (None, None) => match &pending.path {
                Some(path) => !self.is_dashboard_path(path) && scope.paths.allows(path),
                None => true,
            },
        }
    }

    // --- the request, from the middleware --------------------------------

    /// Before the request runs: path checks and the opt-in captures.
    pub(crate) fn before_request(&self, request: &rainier_http::Request) {
        let Some(cell) = self.current_batch() else { return };
        if cell.kind != BatchKind::Request {
            return;
        }
        let path = request.path().to_string();
        if self.is_dashboard_path(&path) {
            self.reject(&cell, Rejection::Scope);
            return;
        }
        let capture = &self.config.capture;
        let headers = capture.request_headers.then(|| {
            crate::redact::redact_headers(
                request
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.as_str(), v))),
                &self.config.redaction,
            )
        });
        let body = capture.request_body_on_exception.then(|| {
            let body = request.body();
            body[..body.len().min(64 * 1024)].to_vec()
        });
        let mut state = cell.lock();
        if let Some(pending) = state.as_mut() {
            pending.method.get_or_insert_with(|| request.method().as_str().to_string());
            pending.path.get_or_insert(path);
            pending.request_headers = headers;
            pending.request_body = body;
        }
    }

    /// After the response: the matched route, the status, who it was for.
    pub(crate) fn after_response(&self, response: &Response) {
        let Some(cell) = self.current_batch() else { return };
        if cell.kind != BatchKind::Request {
            return;
        }
        let identity = self
            .identify
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|hook| hook(response));
        {
            let mut state = cell.lock();
            let Some(pending) = state.as_mut() else { return };
            pending.status = Some(response.status().as_u16());
            if let Some((guard, user)) = identity {
                pending.guard = Some(guard);
                pending.user_id = user;
            }
        }
        let matched = response.extensions().get::<rainier_routing::MatchedRoute>();
        self.set_route(
            &cell,
            matched.and_then(|m| m.name.as_deref()),
            matched.map(|m| m.uri.as_str()),
        );
    }

    // --- filling ----------------------------------------------------------

    /// Add an entry to `cell`, unless it stopped buffering.
    pub(crate) fn push(&self, cell: &BatchCell, entry: Entry) {
        if cell.is_rejected() && entry.entry_type != EntryType::Exception {
            return;
        }
        if let Some(pending) = cell.lock().as_mut() {
            if pending.rejected.is_some() && entry.entry_type != EntryType::Exception {
                return;
            }
            pending.push(entry);
        }
    }

    /// Mark the current batch as a failed job.
    pub(crate) fn mark_current(&self, failed_job: bool, tag: Option<&str>) {
        let Some(cell) = self.current_batch() else { return };
        let mut state = cell.lock();
        if let Some(pending) = state.as_mut() {
            pending.failed_job |= failed_job;
            if let Some(tag) = tag {
                pending.tags.insert(tag.to_string());
            }
        }
    }

    /// A failure was reported. Into the batch it happened in; or the request
    /// batch this thread just parked; or a batch of its own.
    pub(crate) fn record_failure(&self, entry: Entry, failed_job: bool, after_request: bool) {
        if let Some(cell) = self.current_batch() {
            if let Some(pending) = cell.lock().as_mut() {
                pending.failed_job |= failed_job;
                pending.push(entry);
                return;
            }
        }
        if after_request {
            let key = self.key();
            let recent = RECENT.with(|r| r.borrow_mut().take());
            if let Some((k, id)) = recent {
                if k == key {
                    let claimed = {
                        let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
                        parked.iter().position(|(_, p)| p.id == id).map(|at| parked.remove(at).1)
                    };
                    if let Some(mut pending) = claimed {
                        pending.push(entry);
                        self.finish(pending);
                        return;
                    }
                }
            }
        }
        let mut pending = Pending::new(BatchKind::Other);
        pending.failed_job = failed_job;
        pending.started_at = entry.occurred_at;
        pending.duration_us = Some(0);
        pending.push(entry);
        self.finish(pending);
    }

    // --- closing ----------------------------------------------------------

    /// The root span closed with `root` as its entry.
    pub(crate) fn close_batch(&self, cell: &Arc<BatchCell>, root: Option<Entry>) {
        let Some(mut pending) = cell.lock().take() else { return };
        {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            open.remove(&pending.id);
            if let Some(trace) = &pending.trace_id {
                open.remove(trace);
            }
        }
        if let Some(root) = root {
            pending.duration_us = root.duration_us;
            pending.started_at = root.occurred_at;
            fill_from_root(&mut pending, &root);
            pending.entries.insert(0, root);
        }
        // The route stage again, for requests whose route (or lack of one)
        // only the root span's final fields say.
        if pending.kind == BatchKind::Request
            && pending.rejected.is_none()
            && !self.pending_in_scope(&pending)
        {
            pending.rejected = Some(Rejection::Scope);
        }
        if pending.kind == BatchKind::Request && pending.status.is_some_and(|s| s >= 400) {
            let key = self.key();
            let id = pending.id.clone();
            let overflow = {
                let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
                parked.push((Instant::now(), pending));
                (parked.len() > MAX_PARKED).then(|| parked.remove(0).1)
            };
            RECENT.with(|r| *r.borrow_mut() = Some((key, id)));
            if let Some(oldest) = overflow {
                self.finish(oldest);
            }
            return;
        }
        self.finish(pending);
    }

    /// Release parked batches older than `older_than`.
    fn release_parked(&self, older_than: Duration) {
        let due: Vec<Pending> = {
            let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let (due, keep): (Vec<_>, Vec<_>) =
                parked.drain(..).partition(|(at, _)| now.duration_since(*at) >= older_than);
            *parked = keep;
            due.into_iter().map(|(_, p)| p).collect()
        };
        for pending in due {
            self.finish(pending);
        }
    }

    /// Finalize and hand to the writer.
    fn finish(&self, pending: Pending) {
        if let Some(batch) = finalize(&self.config, pending, self.host.clone()) {
            self.send(batch);
        }
    }

    /// `try_send`; full means dropped and counted.
    fn send(&self, batch: Batch) {
        match self.tx.try_send(batch) {
            Ok(()) => {
                let queued = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
                if queued >= self.config.ingest.flush_batch_size {
                    self.wake.notify_one();
                }
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // --- writing ----------------------------------------------------------

    /// Drain the channel into the store, `flush_batch_size` at a time.
    async fn drain_and_store(&self) {
        let mut waiting = Vec::new();
        {
            let mut rx = self.rx.lock().await;
            while let Ok(batch) = rx.try_recv() {
                waiting.push(batch);
            }
        }
        if waiting.is_empty() {
            return;
        }
        let _ = self.queued.try_update(Ordering::Relaxed, Ordering::Relaxed, |q| {
            Some(q.saturating_sub(waiting.len()))
        });
        let size = self.config.ingest.flush_batch_size.max(1);
        let span = tracing::info_span!("lookout.write", rainier.lookout.internal = true);
        while !waiting.is_empty() {
            let rest = waiting.split_off(waiting.len().min(size));
            let chunk = std::mem::replace(&mut waiting, rest);
            let result =
                tracing::Instrument::instrument(self.store.store(chunk), span.clone()).await;
            if let Err(error) = result {
                self.log_store_error(&span, &error);
            }
        }
    }

    /// Log a store failure at most once per flush interval. Never reported on
    /// the bus: reporting it would be recorded, written, fail, and report.
    fn log_store_error(&self, span: &tracing::Span, error: &rainier_support::Error) {
        let every = Duration::from_millis(self.config.ingest.flush_interval_ms.max(1));
        let mut last = self.last_store_error.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| at.elapsed() < every) {
            return;
        }
        *last = Some(Instant::now());
        span.in_scope(|| {
            tracing::warn!(store = self.store.name(), error = %error, "lookout could not store batches");
        });
    }
}

/// Copy what the root span said onto the batch, where the middleware did not.
fn fill_from_root(pending: &mut Pending, root: &Entry) {
    let content = &root.content;
    let text = |key: &str| content.get(key).and_then(|v| v.as_str()).map(str::to_string);
    match pending.kind {
        BatchKind::Request => {
            if pending.method.is_none() {
                pending.method = text("http.request.method");
            }
            if pending.path.is_none() {
                pending.path = text("url.path");
            }
            if pending.route_pattern.is_none() {
                pending.route_pattern = text("http.route");
            }
            if pending.route_name.is_none() {
                pending.route_name = text("rainier.route.name");
            }
            if pending.status.is_none() {
                pending.status = content
                    .get("http.response.status_code")
                    .and_then(|v| v.as_u64())
                    .and_then(|s| u16::try_from(s).ok());
            }
        }
        BatchKind::Job => {
            pending.name = text("rainier.job.name").or(pending.name.take());
            if let Some(outcome) = text("rainier.job.outcome") {
                if matches!(outcome.as_str(), "failed" | "timed_out" | "error") {
                    pending.failed_job = true;
                }
                pending.tags.insert(format!("outcome:{outcome}"));
            }
        }
        BatchKind::Command => {
            pending.name = text("rainier.command.name").or(pending.name.take());
        }
        BatchKind::ScheduledTask => {
            pending.name = text("rainier.task.name").or(pending.name.take());
        }
        _ => {}
    }
    if pending.trace_id.is_none() {
        pending.trace_id = text("trace_id");
    }
}

// --- finalizing --------------------------------------------------------------

/// Everything that happens when a batch closes, in one pure function: scope,
/// watchers, slow, N+1, the keep rule. `None` means "not kept".
pub(crate) fn finalize(
    config: &LookoutConfig,
    mut pending: Pending,
    host: Option<String>,
) -> Option<Batch> {
    let scope = &config.scope;
    let has_exception = pending.entries.iter().any(|e| e.entry_type == EntryType::Exception);

    // Guard and user scope, now that authentication has run.
    let mut out_of_scope = pending.rejected == Some(Rejection::Scope);
    if !out_of_scope && pending.kind == BatchKind::Request {
        let guard_ok = match &pending.guard {
            Some(guard) => scope.guards.allows(guard),
            None => scope.guards.include.is_empty(),
        };
        let user_ok = scope.users.is_empty()
            || pending.user_id.as_ref().is_some_and(|u| scope.users.contains(u));
        out_of_scope = !guard_ok || !user_ok;
    }

    // Tags: the batch's own, the route, the user.
    let mut tags = std::mem::take(&mut pending.tags);
    if let Some(route) = &pending.route_name {
        tags.insert(format!("route:{route}"));
    }
    let user_tag = pending.user_id.as_ref().map(|u| format!("user:{u}"));
    if config.capture.user_id {
        if let Some(tag) = &user_tag {
            tags.insert(tag.clone());
        }
    }
    if pending.failed_job {
        tags.insert("failed".to_string());
    }
    if pending.truncated > 0 {
        tags.insert(format!("truncated:{}", pending.truncated));
    }
    let monitored = !scope.monitored_tags.is_empty()
        && (tags.iter().any(|t| scope.monitored_tags.contains(t))
            || user_tag.as_ref().is_some_and(|t| scope.monitored_tags.contains(t))
            || pending
                .entries
                .iter()
                .any(|e| e.tags.iter().any(|t| scope.monitored_tags.contains(t))));

    let sampled_out = pending.sampled_out || pending.rejected == Some(Rejection::Sampled);
    let reduced = out_of_scope || (sampled_out && !monitored);
    if reduced {
        // Failures are never lost to scope or sampling: keep them, with the
        // root entry for context, and nothing else.
        if !has_exception {
            return None;
        }
        let root_type = root_entry_type(pending.kind);
        let mut seen_root = false;
        pending.entries.retain(|e| {
            if e.entry_type == EntryType::Exception {
                return true;
            }
            if Some(e.entry_type) == root_type && !seen_root {
                seen_root = true;
                return true;
            }
            false
        });
        tags.insert(if out_of_scope { "out_of_scope" } else { "sampled_out" }.to_string());
    } else {
        // A watcher turned off removes its entries — including the root's,
        // though the batch itself stays.
        pending.entries.retain(|e| config.watcher(e.entry_type).enabled);
        derive_slow(config, &mut pending.entries);
        derive_n_plus_one(config, &mut pending.entries);
    }

    pending.entries.sort_by_key(|e| e.occurred_at);
    // Unique per batch: a job shares its trace with the request that queued
    // it, and a store replaces on equal ids.
    let id = pending.id.clone();
    let trace_id = pending.trace_id.clone().unwrap_or_else(|| pending.id.clone());
    for entry in &mut pending.entries {
        entry.batch_id = id.clone();
        for tag in &entry.tags {
            if matches!(tag.as_str(), "slow" | "failed" | "n_plus_one") {
                tags.insert(tag.clone());
            }
        }
    }
    let has_slow = pending.entries.iter().any(|e| e.tags.iter().any(|t| t == "slow"));

    // The request body, only now that we know there was a failure.
    if has_exception {
        if let Some(body) = pending.request_body.take() {
            if let Some(root) =
                pending.entries.iter_mut().find(|e| e.entry_type == EntryType::Request)
            {
                if let Some(map) = root.content.as_object_mut() {
                    map.insert(
                        "request.body".into(),
                        crate::redact::redact_body(&body, &config.redaction),
                    );
                }
            }
        }
    }
    if let Some(headers) = pending.request_headers.take() {
        if let Some(root) = pending.entries.iter_mut().find(|e| e.entry_type == EntryType::Request)
        {
            if let Some(map) = root.content.as_object_mut() {
                map.insert("request.headers".into(), headers);
            }
        }
    }

    let summary = batch_summary(&pending);
    let batch = Batch {
        id,
        trace_id,
        kind: pending.kind,
        summary,
        route_name: pending.route_name,
        route_pattern: pending.route_pattern,
        method: pending.method,
        status: pending.status,
        guard: pending.guard,
        user_id: if config.capture.user_id { pending.user_id } else { None },
        duration_us: pending.duration_us.unwrap_or_else(|| {
            u64::try_from(pending.started.elapsed().as_micros()).unwrap_or(u64::MAX)
        }),
        has_exception,
        has_slow,
        host,
        started_at: pending.started_at,
        tags: tags.into_iter().collect(),
        entries: pending.entries,
    };

    let keep = reduced
        || monitored
        || pending.kind == BatchKind::Other
        || match &config.keep {
            Some(rule) => rule(&batch),
            None => config.local || has_exception || has_slow || pending.failed_job,
        };
    keep.then_some(batch)
}

/// The entry type a batch's root span records as.
fn root_entry_type(kind: BatchKind) -> Option<EntryType> {
    match kind {
        BatchKind::Request => Some(EntryType::Request),
        BatchKind::Job => Some(EntryType::Job),
        BatchKind::Command => Some(EntryType::Command),
        BatchKind::ScheduledTask => Some(EntryType::ScheduledTask),
        _ => None,
    }
}

fn batch_summary(pending: &Pending) -> String {
    match pending.kind {
        BatchKind::Request => {
            let method = pending.method.as_deref().unwrap_or("?");
            let target =
                pending.route_pattern.as_deref().or(pending.path.as_deref()).unwrap_or("?");
            match pending.status {
                Some(status) => format!("{method} {target} {status}"),
                None => format!("{method} {target}"),
            }
        }
        BatchKind::Job => format!("job {}", pending.name.as_deref().unwrap_or("?")),
        BatchKind::Command => format!("command {}", pending.name.as_deref().unwrap_or("?")),
        BatchKind::ScheduledTask => format!("task {}", pending.name.as_deref().unwrap_or("?")),
        _ => pending
            .entries
            .iter()
            .find(|e| e.entry_type == EntryType::Exception)
            .map(|e| e.summary.clone())
            .unwrap_or_else(|| "other".to_string()),
    }
}

/// The slow threshold for an entry: the most specific `slow_overrides`
/// pattern matching its family key, else the watcher's `slow_ms`.
pub(crate) fn slow_threshold_ms(config: &LookoutConfig, entry: &Entry) -> Option<u64> {
    let watcher = config.watchers.get(&entry.entry_type)?;
    let key = entry.family_hash.as_deref().unwrap_or(&entry.summary);
    watcher
        .slow_overrides
        .iter()
        .filter(|(pattern, _)| glob(pattern, key))
        .max_by_key(|(pattern, _)| pattern.len())
        .map(|(_, ms)| *ms)
        .or(watcher.slow_ms)
}

/// Tag entries over their threshold `slow`, and add a derived
/// [`EntryType::Slow`] entry for each.
fn derive_slow(config: &LookoutConfig, entries: &mut Vec<Entry>) {
    let make_derived = config.watcher(EntryType::Slow).enabled;
    let mut derived = Vec::new();
    for entry in entries.iter_mut() {
        let Some(duration) = entry.duration_us else { continue };
        let Some(threshold) = slow_threshold_ms(config, entry) else { continue };
        if duration < threshold.saturating_mul(1000) {
            continue;
        }
        if !entry.tags.iter().any(|t| t == "slow") {
            entry.tags.push("slow".to_string());
        }
        if make_derived {
            let family = entry.family_hash.clone().unwrap_or_else(|| entry.summary.clone());
            derived.push(Entry {
                id: new_entry_id(),
                batch_id: entry.batch_id.clone(),
                entry_type: EntryType::Slow,
                span_id: entry.span_id.clone(),
                parent_span_id: entry.parent_span_id.clone(),
                family_hash: Some(format!("{}:{family}", entry.entry_type)),
                summary: format!(
                    "slow {}: {} ({} ms, threshold {} ms)",
                    entry.entry_type,
                    entry.summary,
                    duration / 1000,
                    threshold
                ),
                occurred_at: entry.occurred_at,
                duration_us: Some(duration),
                tags: vec!["slow".to_string()],
                content: serde_json::json!({
                    "entry_id": entry.id,
                    "entry_type": entry.entry_type,
                    "family": family,
                    "duration_ms": duration / 1000,
                    "threshold_ms": threshold,
                }),
            });
        }
    }
    entries.extend(derived);
}

/// Runs of the same sanitised SQL in one batch: at least `min_repeats` of
/// them, taking at least `min_total_ms` together. Not only consecutive ones —
/// the classic N+1 interleaves its repeated query with others.
fn derive_n_plus_one(config: &LookoutConfig, entries: &mut Vec<Entry>) {
    let rule = config.n_plus_one;
    if !rule.enabled || !config.watcher(EntryType::NPlusOne).enabled {
        return;
    }
    let mut runs: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.entry_type != EntryType::Query {
            continue;
        }
        if let Some(sql) = &entry.family_hash {
            runs.entry(sql.clone()).or_default().push(i);
        }
    }
    let mut derived = Vec::new();
    for (sql, indexes) in runs {
        if indexes.len() < rule.min_repeats.max(2) {
            continue;
        }
        let total: u64 = indexes.iter().filter_map(|&i| entries[i].duration_us).sum();
        if total < rule.min_total_ms.saturating_mul(1000) {
            continue;
        }
        let ids: Vec<String> = indexes.iter().map(|&i| entries[i].id.clone()).collect();
        let first = &entries[indexes[0]];
        derived.push(Entry {
            id: new_entry_id(),
            batch_id: first.batch_id.clone(),
            entry_type: EntryType::NPlusOne,
            span_id: None,
            parent_span_id: first.parent_span_id.clone(),
            family_hash: Some(sql.clone()),
            summary: format!("{}× {}", indexes.len(), first.summary),
            occurred_at: first.occurred_at,
            duration_us: Some(total),
            tags: vec!["n_plus_one".to_string()],
            content: serde_json::json!({
                "sql": sql,
                "count": indexes.len(),
                "total_ms": total / 1000,
                "entry_ids": ids,
            }),
        });
        for &i in &indexes {
            if !entries[i].tags.iter().any(|t| t == "n_plus_one") {
                entries[i].tags.push("n_plus_one".to_string());
            }
        }
    }
    entries.extend(derived);
}

// --- small things ------------------------------------------------------------

fn parse_level(level: &str) -> Option<tracing::Level> {
    match level.to_ascii_lowercase().as_str() {
        "error" => Some(tracing::Level::ERROR),
        "warn" => Some(tracing::Level::WARN),
        "info" => Some(tracing::Level::INFO),
        "debug" => Some(tracing::Level::DEBUG),
        "trace" => Some(tracing::Level::TRACE),
        _ => None,
    }
}

thread_local! {
    static RNG: std::cell::Cell<u64> = std::cell::Cell::new({
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(Instant::now().elapsed().as_nanos() as u64);
        h.finish() | 1
    });
}

/// A fast, non-cryptographic random number (xorshift64*), per thread.
pub(crate) fn random_u64() -> u64 {
    RNG.with(|cell| {
        let mut x = cell.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        cell.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

/// Whether to keep one sample at `rate`.
pub(crate) fn sample(rate: f64) -> bool {
    if rate >= 1.0 {
        return true;
    }
    if rate <= 0.0 {
        return false;
    }
    ((random_u64() >> 11) as f64 / (1u64 << 53) as f64) < rate
}

/// A 32-hex-digit id, the shape of a trace id.
pub(crate) fn new_id() -> String {
    format!("{:016x}{:016x}", random_u64(), random_u64())
}

/// A 16-hex-digit id, unique in the process, for entries and spans.
pub(crate) fn new_entry_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    static SALT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let salt = *SALT.get_or_init(|| random_u64() & 0xffff_ff00_0000_0000);
    format!("{:016x}", salt ^ NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Patterns, Scope, WatcherConfig};
    use crate::store::memory::MemoryStore;
    use rainier_events::report::{ApplicationFailed, ExceptionReported, HttpRequestFailed};
    use rainier_events::Dispatcher;
    use rainier_support::Error;
    use tracing::field::Empty;
    use tracing_subscriber::layer::SubscriberExt;

    /// An enabled Lookout over a fresh memory store, its layer installed as
    /// this thread's subscriber for as long as the guard lives.
    struct Rig {
        lookout: Lookout,
        store: Arc<MemoryStore>,
        _guard: tracing::subscriber::DefaultGuard,
    }

    fn rig(config: LookoutConfig) -> Rig {
        let store = Arc::new(MemoryStore::new(100));
        let lookout = Lookout::new(config, store.clone());
        let subscriber = tracing_subscriber::registry().with(lookout.layer());
        let guard = tracing::subscriber::set_default(subscriber);
        Rig { lookout, store, _guard: guard }
    }

    fn local() -> LookoutConfig {
        LookoutConfig::default().enabled(true).local(true)
    }

    fn production() -> LookoutConfig {
        LookoutConfig::default().enabled(true).local(false)
    }

    fn request_span(path: &str) -> tracing::Span {
        tracing::info_span!(
            "http.server.request",
            otel.name = "GET",
            http.request.method = "GET",
            url.path = path,
            http.route = Empty,
            rainier.route.name = Empty,
            http.response.status_code = Empty,
            trace_id = Empty,
        )
    }

    fn query(sql: &str) {
        let span = tracing::info_span!(
            "db.query",
            db.system.name = "sqlite",
            db.query.text = sql,
            otel.status_code = Empty,
        );
        let _entered = span.enter();
    }

    /// One request: route `posts.show`, status `status`, `work` inside it.
    fn request(path: &str, status: u16, work: impl FnOnce()) {
        let span = request_span(path);
        {
            let _entered = span.enter();
            span.record("trace_id", "4bf92f3577b34da6a3ce929d0e0e4736");
            span.record("http.route", "/posts/{post}");
            span.record("rainier.route.name", "posts.show");
            work();
        }
        span.record("http.response.status_code", status);
    }

    #[tokio::test]
    async fn a_request_becomes_a_batch_with_its_queries_nested() {
        let rig = rig(local());
        request("/posts/1", 200, || {
            query("SELECT * FROM posts WHERE id = ?");
            let cache = tracing::info_span!(
                "cache.get",
                cache.store = "memory",
                cache.operation = "get",
                cache.hit = true,
                cache.key = "posts:1",
            );
            let _e = cache.enter();
            query("SELECT * FROM users WHERE id = ?");
        });
        rig.lookout.flush().await;

        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736", "the trace id");
        assert_ne!(batch.id, batch.trace_id, "ids are unique per batch");
        assert_eq!(batch.kind, BatchKind::Request);
        assert_eq!(batch.summary, "GET /posts/{post} 200");
        assert_eq!(batch.route_name.as_deref(), Some("posts.show"));
        assert_eq!(batch.route_pattern.as_deref(), Some("/posts/{post}"));
        assert_eq!(batch.method.as_deref(), Some("GET"));
        assert_eq!(batch.status, Some(200));
        assert!(batch.tags.contains(&"route:posts.show".to_string()));

        let types: Vec<EntryType> = batch.entries.iter().map(|e| e.entry_type).collect();
        assert_eq!(
            types,
            vec![EntryType::Request, EntryType::Query, EntryType::Cache, EntryType::Query]
        );
        let root = &batch.entries[0];
        let cache = &batch.entries[2];
        assert_eq!(root.family_hash.as_deref(), Some("posts.show"));
        assert_eq!(batch.entries[1].parent_span_id, root.span_id, "query under the request");
        assert_eq!(batch.entries[3].parent_span_id, cache.span_id, "query under the cache read");
        assert_eq!(
            batch.entries[1].family_hash.as_deref(),
            Some("SELECT * FROM posts WHERE id = ?")
        );
        assert_eq!(cache.family_hash.as_deref(), Some("get (memory)"));
        assert_eq!(cache.summary, "cache get hit");
        assert!(cache.content.get("cache.key").is_none(), "keys are opt-in");
        assert!(batch.entries.iter().all(|e| e.batch_id == batch.id));
        assert!(batch.entries.iter().all(|e| e.duration_us.is_some()));
    }

    #[tokio::test]
    async fn cache_keys_are_kept_only_when_opted_in() {
        let mut config = local();
        config.capture.cache_keys = true;
        let rig = rig(config);
        request("/posts/1", 200, || {
            let cache =
                tracing::info_span!("cache.get", cache.operation = "get", cache.key = "posts:1");
            let _e = cache.enter();
        });
        rig.lookout.flush().await;
        let cache = &rig.store.all()[0].entries[1];
        assert_eq!(cache.content["cache.key"], "posts:1");
        assert_eq!(cache.summary, "cache get posts:1");
    }

    #[tokio::test]
    async fn slow_entries_are_tagged_and_derived_and_kept_in_production() {
        let config = production()
            .watch(EntryType::Query, |w| WatcherConfig { slow_ms: Some(0), ..w })
            .watch(EntryType::Request, |w| WatcherConfig { slow_ms: Some(60_000), ..w });
        let rig = rig(config);
        request("/posts/1", 200, || query("SELECT 1"));
        rig.lookout.flush().await;

        let batches = rig.store.all();
        assert_eq!(batches.len(), 1, "slow keeps the batch outside local");
        let batch = &batches[0];
        assert!(batch.has_slow);
        assert!(batch.tags.contains(&"slow".to_string()));
        let query = batch.entries.iter().find(|e| e.entry_type == EntryType::Query).unwrap();
        assert!(query.tags.contains(&"slow".to_string()));
        let request = &batch.entries[0];
        assert!(!request.tags.contains(&"slow".to_string()), "under its own threshold");
        let slow = batch.entries.iter().find(|e| e.entry_type == EntryType::Slow).unwrap();
        assert_eq!(slow.content["entry_id"], query.id.as_str());
        assert_eq!(slow.family_hash.as_deref(), Some("query:SELECT 1"));
    }

    #[tokio::test]
    async fn slow_overrides_glob_over_the_family_key() {
        let config = production().watch(EntryType::Query, |mut w| {
            w.slow_ms = Some(0);
            w.slow_overrides.insert("SELECT * FROM reports*".into(), 60_000);
            w
        });
        let rig = rig(config);
        request("/r", 200, || query("SELECT * FROM reports WHERE year = ?"));
        rig.lookout.flush().await;
        assert!(rig.store.all().is_empty(), "the override raised the bar: nothing slow, not kept");
    }

    #[tokio::test]
    async fn a_run_of_the_same_query_is_an_n_plus_one() {
        let mut config = local();
        config.n_plus_one.min_total_ms = 0;
        let rig = rig(config);
        request("/posts", 200, || {
            query("SELECT * FROM posts");
            for _ in 0..5 {
                query("SELECT * FROM users WHERE id = ?");
                query("SELECT * FROM avatars WHERE user_id = ?");
            }
            query("SELECT * FROM tags WHERE id = ?");
        });
        rig.lookout.flush().await;

        let batch = &rig.store.all()[0];
        let n1: Vec<&Entry> =
            batch.entries.iter().filter(|e| e.entry_type == EntryType::NPlusOne).collect();
        assert_eq!(n1.len(), 2, "users and avatars, each 5 times: {n1:#?}");
        let users = n1
            .iter()
            .find(|e| e.family_hash.as_deref() == Some("SELECT * FROM users WHERE id = ?"))
            .unwrap();
        assert_eq!(users.content["count"], 5);
        let ids: Vec<String> = serde_json::from_value(users.content["entry_ids"].clone()).unwrap();
        assert_eq!(ids.len(), 5);
        for id in &ids {
            let q = batch.entries.iter().find(|e| &e.id == id).expect("points at a query");
            assert_eq!(q.entry_type, EntryType::Query);
            assert!(q.tags.contains(&"n_plus_one".to_string()));
        }
        assert!(batch.tags.contains(&"n_plus_one".to_string()));
    }

    #[tokio::test]
    async fn fewer_repeats_than_the_rule_is_not_an_n_plus_one() {
        let mut config = local();
        config.n_plus_one.min_total_ms = 0;
        let rig = rig(config);
        request("/posts", 200, || (0..4).for_each(|_| query("SELECT 1")));
        rig.lookout.flush().await;
        let batch = &rig.store.all()[0];
        assert!(!batch.entries.iter().any(|e| e.entry_type == EntryType::NPlusOne));
    }

    #[tokio::test]
    async fn an_excluded_route_records_nothing_and_buffers_nothing_after_the_match() {
        let config = local().scope(Scope {
            routes: Patterns { include: vec![], exclude: vec!["posts.*".into()] },
            ..LookoutConfig::default().scope
        });
        let rig = rig(config);
        let probe = rig.lookout.clone();
        request("/posts/1", 200, || {
            query("SELECT 1");
            let cell = probe.inner.current_batch().expect("inside the batch");
            assert!(cell.is_rejected(), "rejected as soon as the route was recorded");
            assert!(cell.lock().as_ref().unwrap().entries.is_empty(), "nothing buffered");
        });
        rig.lookout.flush().await;
        assert!(rig.store.all().is_empty());
    }

    #[tokio::test]
    async fn unmatched_requests_are_scoped_by_path_and_the_dashboard_is_never_recorded() {
        let config = local().scope(Scope {
            paths: Patterns { include: vec![], exclude: vec!["/assets/*".into()] },
            ..LookoutConfig::default().scope
        });
        let rig = rig(config);
        for path in ["/assets/app.js", "/_lookout", "/_lookout/api/batches", "/missing"] {
            let span = request_span(path);
            {
                let _e = span.enter();
                query("SELECT 1");
            }
            span.record("http.response.status_code", 404);
        }
        rig.lookout.flush().await;
        let paths: Vec<String> = rig
            .store
            .all()
            .iter()
            .map(|b| b.entries[0].content["url.path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, vec!["/missing"]);
    }

    #[tokio::test]
    async fn a_zero_sample_rate_records_nothing() {
        let rig = rig(LookoutConfig { request_sample_rate: 0.0, ..local() });
        request("/posts/1", 200, || query("SELECT 1"));
        rig.lookout.flush().await;
        assert!(rig.store.all().is_empty());
    }

    #[tokio::test]
    async fn failures_are_kept_even_when_sampled_out() {
        let rig = rig(LookoutConfig { request_sample_rate: 0.0, ..production() });
        let events = Arc::new(Dispatcher::new());
        rig.lookout.listen(&events);
        let span = request_span("/posts/1");
        async {
            query("SELECT 1");
            events
                .dispatch(ExceptionReported {
                    failure: ApplicationFailed::new(Error::internal("disk full")).into(),
                })
                .await
                .unwrap();
        }
        .instrument_in(span.clone())
        .await;
        span.record("http.response.status_code", 200);
        drop(span);
        rig.lookout.flush().await;

        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert!(batch.has_exception);
        assert!(batch.tags.contains(&"sampled_out".to_string()));
        let types: Vec<EntryType> = batch.entries.iter().map(|e| e.entry_type).collect();
        assert_eq!(types, vec![EntryType::Request, EntryType::Exception], "no query");
        assert_eq!(batch.entries[1].summary, "Internal: disk full");
    }

    #[tokio::test]
    async fn a_failure_outside_any_batch_gets_one_of_its_own() {
        let rig = rig(production());
        let events = Arc::new(Dispatcher::new());
        rig.lookout.listen(&events);
        events
            .dispatch(ExceptionReported {
                failure: ApplicationFailed::new(Error::internal("cron 12 broke")).into(),
            })
            .await
            .unwrap();
        rig.lookout.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].kind, BatchKind::Other);
        assert_eq!(batches[0].summary, "Internal: cron 12 broke");
        assert_eq!(
            batches[0].entries[0].family_hash.as_deref(),
            Some("Application|Internal|cron N broke")
        );
    }

    #[tokio::test]
    async fn a_failed_request_reported_after_its_span_joins_its_batch() {
        let rig = rig(production());
        let events = Arc::new(Dispatcher::new());
        rig.lookout.listen(&events);
        request("/posts/1", 500, || query("SELECT 1"));
        // What the kernel does once `handle` has returned: no span around it.
        events
            .dispatch(ExceptionReported {
                failure: HttpRequestFailed::new(
                    Error::internal("boom"),
                    "GET",
                    "/posts/1",
                    Some("/posts/{post}".to_string()),
                )
                .into(),
            })
            .await
            .unwrap();
        rig.lookout.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1, "{batches:#?}");
        let batch = &batches[0];
        assert_eq!(batch.kind, BatchKind::Request);
        assert!(batch.has_exception);
        assert_eq!(batch.entries.len(), 3, "request, query, exception");
    }

    #[tokio::test]
    async fn a_parked_batch_nobody_claims_is_released_on_flush() {
        let rig = rig(local());
        request("/posts/1", 404, || {});
        assert!(rig.store.all().is_empty());
        rig.lookout.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn production_keeps_only_interesting_batches_and_monitored_tags() {
        let config = production().scope(Scope {
            monitored_tags: ["vip".to_string()].into(),
            ..LookoutConfig::default().scope
        });
        let rig = rig(config);
        let probe = rig.lookout.clone();
        request("/posts/1", 200, || query("SELECT 1"));
        request("/posts/2", 200, || {
            assert!(probe.tag(None, "vip"));
            query("SELECT 2");
        });
        rig.lookout.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].tags.contains(&"vip".to_string()));
        assert_eq!(batches[0].entries.len(), 2);
    }

    #[tokio::test]
    async fn monitored_tags_rescue_a_sampled_out_batch_whole() {
        let config = LookoutConfig {
            request_sample_rate: 0.0,
            ..production().scope(Scope {
                monitored_tags: ["vip".to_string()].into(),
                ..LookoutConfig::default().scope
            })
        };
        let rig = rig(config);
        let probe = rig.lookout.clone();
        request("/posts/1", 200, || query("SELECT 1"));
        request("/posts/2", 200, || {
            let id = probe.current_batch_id().expect("a batch");
            assert!(probe.tag(Some(&id), "vip"), "by its trace id");
            query("SELECT 2");
        });
        rig.lookout.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].entries.len(), 2, "with its query");
    }

    #[tokio::test]
    async fn a_custom_keep_rule_replaces_the_default() {
        let rig = rig(production().keep_batch_if(|b| b.status == Some(200)));
        request("/posts/1", 200, || {});
        rig.lookout.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn a_full_channel_drops_and_counts() {
        let mut config = local();
        config.ingest.channel_capacity = 1;
        let rig = rig(config);
        for _ in 0..3 {
            request("/posts/1", 200, || {});
        }
        assert_eq!(rig.lookout.dropped(), 2);
        rig.lookout.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn log_lines_are_recorded_and_redacted() {
        let rig = rig(LookoutConfig { log_level: "warn".into(), ..local() });
        request("/login", 200, || {
            tracing::warn!(password = "hunter2", user = "ada", "login 3 failed");
            tracing::info!("below the level");
        });
        rig.lookout.flush().await;
        let batch = &rig.store.all()[0];
        let logs: Vec<&Entry> =
            batch.entries.iter().filter(|e| e.entry_type == EntryType::Log).collect();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].summary, "login 3 failed");
        assert_eq!(logs[0].content["password"], crate::redact::REDACTED);
        assert_eq!(logs[0].content["user"], "ada");
        assert_eq!(logs[0].family_hash.as_deref(), Some("warn login N failed"));
    }

    #[tokio::test]
    async fn the_writer_flushes_on_its_interval_and_on_shutdown() {
        let mut config = local();
        config.ingest.flush_interval_ms = 10;
        let rig = rig(config);
        let writer = rig.lookout.start_writer();
        assert!(writer.is_running());
        assert!(!rig.lookout.start_writer().is_running(), "only one writer");

        request("/posts/1", 200, || query("SELECT 1"));
        for _ in 0..200 {
            if !rig.store.all().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(rig.store.all().len(), 1, "flushed by the interval");

        request("/posts/2", 200, || {});
        writer.shutdown().await;
        assert_eq!(rig.store.all().len(), 2, "flushed on shutdown");
    }

    /// A store whose writes record what spans are current — and run a query
    /// of their own, as the database store does.
    struct Noisy(MemoryStore);

    impl Store for Noisy {
        fn name(&self) -> &'static str {
            "noisy"
        }
        fn store(
            &self,
            batches: Vec<Batch>,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<()>> {
            query("INSERT INTO lookout_batches VALUES (?)");
            self.0.store(batches)
        }
        fn batches(
            &self,
            q: crate::store::BatchQuery,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<Vec<crate::entry::BatchSummary>>>
        {
            self.0.batches(q)
        }
        fn batch(
            &self,
            id: &str,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<Option<Batch>>> {
            self.0.batch(id)
        }
        fn entries(
            &self,
            t: EntryType,
            b: Option<DateTime<Utc>>,
            l: usize,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<Vec<Entry>>> {
            self.0.entries(t, b, l)
        }
        fn aggregates(
            &self,
            q: crate::store::AggregateQuery,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<Vec<crate::entry::Aggregate>>>
        {
            self.0.aggregates(q)
        }
        fn prune(
            &self,
            a: DateTime<Utc>,
            b: DateTime<Utc>,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<u64>> {
            self.0.prune(a, b)
        }
        fn clear(&self) -> rainier_support::BoxFuture<'_, rainier_support::Result<()>> {
            self.0.clear()
        }
    }

    #[tokio::test]
    async fn lookouts_own_work_is_not_recorded() {
        // The writer's span: a request-shaped span inside it opens no batch.
        let rig = rig(local());
        let internal = tracing::info_span!("lookout.write", rainier.lookout.internal = true);
        internal.in_scope(|| request("/inside", 200, || query("SELECT 1")));
        rig.lookout.flush().await;
        assert!(rig.store.all().is_empty());

        // And a store that queries while writing does not feed itself.
        let store = Arc::new(Noisy(MemoryStore::new(10)));
        let lookout = Lookout::new(local(), store.clone());
        let subscriber = tracing_subscriber::registry().with(lookout.layer());
        let _guard = tracing::subscriber::set_default(subscriber);
        request("/posts/1", 200, || {});
        lookout.flush().await;
        lookout.flush().await;
        let all = store.0.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].entries.len(), 1, "just the request");
    }

    #[tokio::test]
    async fn disabled_records_nothing() {
        let rig = rig(LookoutConfig::default().local(true));
        let events = Arc::new(Dispatcher::new());
        rig.lookout.listen(&events);
        assert!(!events.has_listeners::<ExceptionReported>(), "no listeners registered");
        request("/posts/1", 200, || query("SELECT 1"));
        rig.lookout.record_failure(&ApplicationFailed::new(Error::internal("x")).into());
        rig.lookout.flush().await;
        assert!(!rig.lookout.start_writer().is_running());
        assert!(rig.store.all().is_empty());
    }

    #[tokio::test]
    async fn paused_opens_no_batches() {
        let rig = rig(local());
        rig.lookout.pause();
        request("/posts/1", 200, || {});
        rig.lookout.resume();
        request("/posts/2", 200, || {});
        rig.lookout.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn a_job_inside_a_worker_command_is_its_own_batch_and_its_failure_keeps_it() {
        let rig = rig(production());
        let command = tracing::info_span!("console.command", rainier.command.name = "queue:work");
        command.in_scope(|| {
            for outcome in ["processed", "failed"] {
                let job = tracing::info_span!(
                    "queue.process",
                    rainier.job.name = "SendInvoice",
                    rainier.job.outcome = Empty,
                    otel.status_code = Empty,
                );
                job.in_scope(|| query("UPDATE invoices SET sent = 1"));
                job.record("rainier.job.outcome", outcome);
            }
        });
        drop(command);
        rig.lookout.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1, "only the failed job is interesting: {batches:#?}");
        let job = &batches[0];
        assert_eq!(job.kind, BatchKind::Job);
        assert_eq!(job.summary, "job SendInvoice");
        assert!(job.tags.contains(&"failed".to_string()));
        assert_eq!(job.entries.len(), 2, "the job and its query");
    }

    #[tokio::test]
    async fn jobs_and_commands_can_be_scoped_out() {
        let rig = rig(local().scope(Scope { jobs: false, ..LookoutConfig::default().scope }));
        tracing::info_span!("queue.process", rainier.job.name = "J").in_scope(|| query("SELECT 1"));
        tracing::info_span!("console.command", rainier.command.name = "lookout:prune")
            .in_scope(|| query("DELETE"));
        tracing::info_span!("console.command", rainier.command.name = "migrate")
            .in_scope(|| query("CREATE"));
        rig.lookout.flush().await;
        let summaries: Vec<String> = rig.store.all().iter().map(|b| b.summary.clone()).collect();
        assert_eq!(summaries, vec!["command migrate"]);
    }

    #[tokio::test]
    async fn model_events_are_recorded_by_name() {
        let rig = rig(local());
        let probe = rig.lookout.clone();
        request("/posts", 201, || {
            probe.record_named_event("rainier_database::model::Created<shop::models::Post>");
            probe.record_named_event("rainier_database::model::Creating<shop::models::Post>");
        });
        rig.lookout.flush().await;
        let batch = &rig.store.all()[0];
        let models: Vec<&str> = batch
            .entries
            .iter()
            .filter(|e| e.entry_type == EntryType::Model)
            .map(|e| e.summary.as_str())
            .collect();
        assert_eq!(models, vec!["Post created"]);
    }

    #[tokio::test]
    async fn a_disabled_watcher_drops_its_entries() {
        let rig = rig(local().disable(EntryType::Query));
        request("/posts/1", 200, || query("SELECT 1"));
        rig.lookout.flush().await;
        let batch = &rig.store.all()[0];
        assert_eq!(batch.entries.len(), 1);
        assert_eq!(batch.entries[0].entry_type, EntryType::Request);
    }

    #[tokio::test]
    async fn guard_and_user_scope_run_at_close() {
        let config = local()
            .scope(Scope { users: ["42".to_string()].into(), ..LookoutConfig::default().scope });
        let rig = rig(config);
        let probe = rig.lookout.clone();
        for user in ["7", "42"] {
            request("/posts/1", 200, || {
                let cell = probe.inner.current_batch().unwrap();
                let mut state = cell.lock();
                let pending = state.as_mut().unwrap();
                pending.guard = Some("session".into());
                pending.user_id = Some(user.into());
            });
        }
        rig.lookout.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].guard.as_deref(), Some("session"));
        assert_eq!(batches[0].user_id, None, "ids are kept only with capture.user_id");
    }

    /// `Instrument`, named so it does not read like the layer's own calls.
    trait InstrumentIn: std::future::Future + Sized {
        fn instrument_in(self, span: tracing::Span) -> tracing::instrument::Instrumented<Self> {
            tracing::Instrument::instrument(self, span)
        }
    }
    impl<F: std::future::Future> InstrumentIn for F {}
}
