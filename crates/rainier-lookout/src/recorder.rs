//! The recorder: the [`Lookout`] handle, the in-progress batches, what happens
//! when one closes, and the background writer.
//!
//! # The life of a batch
//!
//! A batch is one [`Unit`] of work — a request, a job's run, a console
//! command, a scheduled task — as the event bus names it. Lookout hears about
//! it on its [subscriber](crate::subscriber), on a thread of its own.
//!
//! 1. **Open.** The first thing heard about a unit opens its batch: an
//!    activity published in it, a failure reported in it, or the middleware
//!    on the request's own thread. Its id is the unit's
//!    [`id_string`](Unit::id_string). Scope and sampling that can be decided
//!    now are decided now, from the unit's kind, name and path: the request's
//!    path against the dashboard's own, whether jobs / commands / tasks are
//!    watched at all, and the sample rate. A rejected batch still exists — so
//!    its failures have somewhere to go — but buffers nothing else.
//! 2. **Fill.** Every activity Lookout knows (`db.query`, `cache.*`, …)
//!    becomes an [`Entry`]; failures and model changes heard on the same
//!    subscriber are added; the middleware adds the route, the status and who
//!    was signed in. When the route is known — from the middleware, or the
//!    root activity — the route stage of [`Scope`](crate::config::Scope) runs
//!    and an excluded route's batch drops what it buffered.
//! 3. **Close.** When the bus says the unit is complete
//!    ([`UnitCompleted`](rainier_events::UnitCompleted), published only
//!    after every event of the unit has been handled), the batch is handed
//!    on. `finalize` runs the rest on the writer: guard and user scope,
//!    per-watcher rules, slow thresholds, N+1 detection, the keep rule. A
//!    closed batch is `try_send`-ed into a bounded channel; when the channel
//!    is full the batch is dropped and [`Lookout::dropped`] counts it.
//! 4. **Write.** The writer task ([`Lookout::start_writer`]) drains the
//!    channel every `flush_interval_ms`, or as soon as `flush_batch_size`
//!    batches are waiting, and hands them to [`Store::store`] **outside every
//!    unit** — so what the database store's own queries publish lands in no
//!    batch.
//!
//! # A failure reported after its request
//!
//! The HTTP kernel may report a failed request once its unit is over. So a
//! request batch that closes with a status of 500 or more (400 with
//! `capture.client_errors`, which records client errors) and no failure in it
//! yet is **parked** for a moment instead of finalized; an `HttpRequestFailed`
//! with no unit, for the same method and path, joins it and releases it.
//! Parked batches nobody claims are released by the writer's next tick (or
//! [`Lookout::flush`]).
//!
//! # Work outside any unit of work
//!
//! Activities that are not inside a unit — a query at boot, a cache read in a
//! background task — are **not recorded**. A batch per stray query would bury
//! the batches people look for under a heap of one-entry ones. Failures are
//! the exception: a failure with no unit around it becomes a
//! [`BatchKind::Other`] batch holding just that failure, so nothing reported
//! is lost to scoping or sampling.

use std::backtrace::Backtrace;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rainier_events::{Context, Dispatcher, Unit, UnitCompleted, UnitKind};
use rainier_http::Response;
use tokio::sync::{mpsc, Notify};

use crate::config::{glob, LookoutConfig};
use crate::entry::{Batch, BatchKind, Entry, EntryType};
use crate::middleware::LookoutMiddleware;
use crate::store::Store;

/// The span field Lookout's stores mark their own work with. Lookout's
/// writer runs outside every unit of work, so nothing it does is recorded;
/// the field still tells a log reader whose work a line was.
pub const INTERNAL_FIELD: &str = "rainier.lookout.internal";

/// The most entries one batch holds. A long command that runs a million
/// queries keeps the first ones and counts the rest in the batch's
/// `truncated:<n>` tag.
pub const MAX_ENTRIES_PER_BATCH: usize = 2_000;

/// The most code traces one batch captures. A request that runs the same
/// slow query five hundred times is told where it came from by a few of them;
/// five hundred stack walks would only make it slower.
pub const MAX_CODE_TRACES_PER_BATCH: usize = 16;

/// How often the writer writes the counts of batches it did not keep, when no
/// kept batch has carried them sooner. Two seconds, so the dashboard's live
/// numbers (the stream's `pulse`) trail the traffic by about that much; the
/// store is still not rewritten on every request, only once per interval in
/// which something was counted.
const COUNTS_EVERY: Duration = Duration::from_secs(2);

/// Combine the waiting counts once they grow past this many rows, so a burst
/// between writes holds one row per family rather than one per request.
const COUNTS_COMBINE_AT: usize = 4096;

/// How long a failed request's batch waits for its failure report.
const PARK_FOR: Duration = Duration::from_millis(250);

/// How many batches may be parked at once; past it, the oldest is released.
const MAX_PARKED: usize = 256;

/// Decides who a response was for: `(guard, user id)`. See
/// [`Lookout::identify_with`].
pub type IdentifyHook = Arc<dyn Fn(&Response) -> Option<(String, Option<String>)> + Send + Sync>;

/// What the application said a session is: `(name, value)` pairs, from
/// [`Lookout::describe_session`].
type SessionContext = Vec<(String, String)>;

/// Lookout: the handle an application holds.
///
/// Cheap to clone; every clone shares the same buffers, channel and counters.
///
/// ```no_run
/// use std::sync::Arc;
/// use rainier_lookout::{Lookout, LookoutConfig, MemoryStore};
///
/// # async fn boot(events: &Arc<rainier_events::Dispatcher>) {
/// let lookout = Lookout::new(LookoutConfig::from_env().unwrap(), Arc::new(MemoryStore::new(500)));
/// lookout.subscribe(events);
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
    /// With `config.enabled` off every part of it — the subscriber, the
    /// middleware, the writer — does nothing.
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
                counts: Mutex::new(Vec::new()),
                counts_written: Mutex::new(Instant::now()),
                events: OnceLock::new(),
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

    /// Subscribe to `events`: one subscriber, `lookout`, on a thread of its
    /// own, hearing every activity the framework publishes about its work,
    /// the failures it reports, model changes and the end of each unit of
    /// work. See [`subscriber`](crate::subscriber) and
    /// [`watchers`](crate::watchers).
    ///
    /// Also asks publishers for what only they can capture: a code trace for
    /// work over the lowest slow threshold of the traced watchers (with
    /// [`Capture::code_traces`](crate::config::Capture::code_traces) on), and
    /// log lines at or above [`log_level`](LookoutConfig::log_level).
    ///
    /// A disabled Lookout subscribes to nothing, so the bus costs the
    /// application what it costs with no subscriber at all. Subscribe once:
    /// a second call registers a second subscriber that records everything
    /// again.
    pub fn subscribe(&self, events: &Arc<Dispatcher>) {
        if !self.inner.config.enabled {
            return;
        }
        let _ = self.inner.events.set(Arc::downgrade(events));
        if self.inner.config.capture.code_traces {
            if let Some(threshold) = crate::subscriber::code_trace_threshold(&self.inner.config) {
                rainier_events::code_trace::request_code_traces(threshold);
            }
        }
        if let Some(level) = self.inner.log_level {
            if self.inner.config.watcher(EntryType::Log).enabled {
                rainier_events::log_layer::request_level(level);
            }
        }
        crate::watchers::subscribe(self, events);
    }

    /// The global middleware that attaches the route, status and user to a
    /// request's batch. Put it anywhere in the global stack.
    pub fn middleware(&self) -> LookoutMiddleware {
        LookoutMiddleware::new(self.clone())
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
                        inner.sweep();
                        inner.release_parked(Duration::ZERO);
                        inner.drain_and_store(true).await;
                        break;
                    }
                }
                inner.sweep();
                inner.release_parked(PARK_FOR);
                inner.drain_and_store(false).await;
            }
        });
        WriterHandle { stop: Some(stop_tx), task: Some(task) }
    }

    /// Write everything closed so far, now: parked batches are released and
    /// the channel is drained into the store. For tests and shutdown.
    ///
    /// A batch closes when the subscriber hears its unit is complete, on its
    /// own thread; a test that wants everything published so far written
    /// waits for the bus first (`events.settled().await`).
    pub async fn flush(&self) {
        if !self.inner.config.enabled {
            return;
        }
        self.inner.sweep();
        self.inner.release_parked(Duration::ZERO);
        self.inner.drain_and_store(true).await;
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
        let cell = self.inner.find(batch);
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

    /// Say who the request the calling code is running in was made by — or,
    /// with `batch`, the open batch with that id.
    ///
    /// Kept on the request when [`Capture::user`](crate::config::Capture::user)
    /// is on, and shown on its page; ignored otherwise, so an application can
    /// describe its users unconditionally and leave the decision to
    /// configuration. Call it where the request is authenticated, with what is
    /// already loaded there: Lookout looks nothing up. If the batch has no
    /// user id yet, this one becomes it. Returns whether a batch was found.
    ///
    /// ```no_run
    /// # fn after_authenticating(lookout: &rainier_lookout::Lookout) {
    /// use rainier_lookout::LookoutUser;
    ///
    /// lookout.describe_user(None, LookoutUser::new("42").name("Ada").email("ada@example.com"));
    /// # }
    /// ```
    pub fn describe_user(&self, batch: Option<&str>, user: crate::user::LookoutUser) -> bool {
        if !self.inner.config.capture.user && !self.inner.config.capture.user_id {
            return false;
        }
        let cell = self.inner.find(batch);
        let Some(cell) = cell else { return false };
        let mut state = cell.lock();
        let Some(pending) = state.as_mut() else { return false };
        if pending.user_id.is_none() {
            pending.user_id = Some(user.id.clone());
        }
        if self.inner.config.capture.user {
            pending.user = Some(user);
        }
        true
    }

    /// Say what the visitor's session is, in the application's own terms —
    /// the profile they are acting as, the workspace they are in — for the
    /// request the calling code is running in, or with `batch`, the open
    /// batch with that id.
    ///
    /// A Lookout session is always the browser's: one tab, renewed after 30
    /// idle minutes. This does not change which session a request is in; it
    /// labels it. The request is tagged `name:value`, and with real user
    /// monitoring on the label rides back to the browser signed, so the page
    /// views, API calls and replays it reports are tagged too — a browser
    /// cannot label its own reports.
    ///
    /// `name` is lower-case letters, digits, `_` and `-`, at most 32, and not
    /// one of Lookout's own tag prefixes (`session`, `user`, `pageview`, …);
    /// `value` is printable ASCII without quotes, backslashes, `,` or `=`, at
    /// most 64. Saying the same `name` again replaces its value. Call it where
    /// the request already knows the answer: Lookout looks nothing up.
    /// Returns whether a batch was found and the label accepted.
    ///
    /// ```no_run
    /// # fn after_authenticating(lookout: &rainier_lookout::Lookout) {
    /// lookout.describe_session(None, "profile", "ada");
    /// # }
    /// ```
    pub fn describe_session(
        &self,
        batch: Option<&str>,
        name: &str,
        value: impl Into<String>,
    ) -> bool {
        let value = value.into();
        if !crate::rum::valid_context_name(name) || !crate::rum::valid_context_value(&value) {
            return false;
        }
        let cell = self.inner.find(batch);
        let Some(cell) = cell else { return false };
        let mut state = cell.lock();
        let Some(pending) = state.as_mut() else { return false };
        let prefix = format!("{name}:");
        pending.tags.retain(|t| !t.starts_with(&prefix));
        pending.tags.insert(format!("{name}:{value}"));
        match pending.context.iter_mut().find(|(n, _)| n == name) {
            Some(held) => held.1 = value,
            None => pending.context.push((name.to_string(), value)),
        }
        true
    }

    /// The middleware that tells Lookout the client's address once the
    /// application has decided which to believe: put it **after** the
    /// application's `TrustProxies`, innermost of the global middleware. The
    /// framework does. See [`LookoutClientMiddleware`](crate::middleware::LookoutClientMiddleware).
    pub fn client_middleware(&self) -> crate::middleware::LookoutClientMiddleware {
        crate::middleware::LookoutClientMiddleware::new(self.clone())
    }

    /// With real user monitoring on, add the headers that let the browser
    /// report on `response`: its batch's signed trace id in `Server-Timing`,
    /// and `Timing-Allow-Origin` for the frontends. See [`crate::rum`].
    ///
    /// Not Lookout's own responses: the script's reports, and the script,
    /// would otherwise each be reported in turn — a report about the last
    /// report, every few seconds, for as long as the page is open.
    pub(crate) fn mark_for_rum(&self, mut response: Response, path: &str) -> Response {
        let rum = &self.inner.config.rum;
        if !rum.enabled || self.inner.is_dashboard_path(path) {
            return response;
        }
        let Some((trace_id, server, context, user)) = self.rum_context() else { return response };
        let mut headers = crate::rum::response_headers(rum, &trace_id, Some(&server));
        if let Some(first) = headers.first_mut() {
            // The session's context and the signed user ride along, so the
            // browser's reports say what the session is and name the right
            // person. The session itself is the browser's.
            if let Some(context) = crate::rum::context_token(&rum.secret, &context) {
                first.1.push_str(&format!(", {};desc=\"{context}\"", crate::rum::CONTEXT_METRIC));
            }
            if let Some(user) = user.filter(|u| crate::rum::safe_header_text(u)) {
                first.1.push_str(&format!(
                    ", {};desc=\"{}\"",
                    crate::rum::USER_METRIC,
                    crate::rum::user_token(&rum.secret, &user)
                ));
            }
        }
        for (name, value) in headers {
            // `Server-Timing` is a list: an application's own metrics stay.
            let value = match response.header(name) {
                Some(existing) if name == "server-timing" => format!("{existing}, {value}"),
                _ => value,
            };
            response = response.with_header(name, &value);
        }
        response
    }

    /// The current batch's trace id, with its time so far and its route.
    fn rum_context(
        &self,
    ) -> Option<(String, crate::rum::ServerSide, SessionContext, Option<String>)> {
        let unit = Context::current_unit()?;
        let cell = self.inner.cell_for(&unit)?;
        if cell.ignored {
            return None;
        }
        if let Some(trace) = unit.trace_id() {
            cell.learn_trace(trace);
        }
        let state = cell.lock();
        let pending = state.as_ref()?;
        Some((
            pending.trace_id.clone().unwrap_or_else(|| pending.id.clone()),
            crate::rum::ServerSide {
                duration_ms: unit.elapsed().as_secs_f64() * 1000.0,
                route: pending.route_name.clone(),
            },
            pending.context.clone(),
            pending.user_id.clone(),
        ))
    }

    /// The id of the batch the calling code is running in, if any — its
    /// trace id once one is known.
    ///
    /// Answered from the unit of work the calling code runs in, without
    /// waiting on the subscriber: a batch's id is its unit's
    /// [`id_string`](Unit::id_string).
    pub fn current_batch_id(&self) -> Option<String> {
        if !self.inner.active() {
            return None;
        }
        let unit = Context::current_unit()?;
        Some(match unit.trace_id() {
            Some(trace) => trace.to_string(),
            None => unit.id_string(),
        })
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

/// An open batch: one unit of work's, shared by the subscriber's thread and
/// the request's (the middleware, `describe_user`, …).
pub(crate) struct BatchCell {
    /// `None` once closed.
    state: Mutex<Option<Pending>>,
    /// A fast "do not bother building entries" flag, mirrored from
    /// `Pending::rejected`.
    rejected: AtomicBool,
    /// The family hashes code traces were captured for, against
    /// [`MAX_CODE_TRACES_PER_BATCH`]. One per family: a loop running the same
    /// slow query twenty times captures its stack once.
    traces: Mutex<HashSet<u64>>,
    /// What started it.
    pub(crate) kind: BatchKind,
    /// The unit's id.
    pub(crate) unit: u64,
    /// The unit itself, not kept alive: a unit that ends without the bus
    /// saying so (one opened with no bus) is closed once it is gone.
    unit_ref: Weak<Unit>,
    /// The root entry's span id: what entries with no other parent hang off.
    pub(crate) root_span: String,
    /// Opened while Lookout was paused: records nothing.
    pub(crate) ignored: bool,
    /// Whether the root activity has been heard.
    root_taken: AtomicBool,
}

impl BatchCell {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Option<Pending>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn is_rejected(&self) -> bool {
        self.rejected.load(Ordering::Relaxed)
    }

    /// Claim the root: `true` for the first activity of the unit's own kind.
    pub(crate) fn take_root(&self) -> bool {
        !self.root_taken.swap(true, Ordering::Relaxed)
    }

    /// The unit's trace id, once known. The first one wins.
    pub(crate) fn learn_trace(&self, trace: &str) {
        if let Some(pending) = self.lock().as_mut() {
            if pending.trace_id.is_none() {
                pending.trace_id = Some(trace.to_string());
            }
        }
    }

    /// Operation `op` ran inside `parent` (`None`: directly in the unit).
    pub(crate) fn note_operation(&self, op: String, parent: Option<String>) {
        if let Some(pending) = self.lock().as_mut() {
            pending.operations.insert(op, parent);
        }
    }

    /// Claim the batch's code trace for `family`: `false` if one was already
    /// taken for it, or once [`MAX_CODE_TRACES_PER_BATCH`] are used up.
    pub(crate) fn claim_trace(&self, family: &str) -> bool {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        family.hash(&mut hasher);
        let mut taken = self.traces.lock().unwrap_or_else(|e| e.into_inner());
        taken.len() < MAX_CODE_TRACES_PER_BATCH && taken.insert(hasher.finish())
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
    /// Raw, cut to `max_body_bytes`: a reference to the request's own
    /// buffer, not a copy. Parsed and redacted on the writer.
    pub(crate) request_body: Option<bytes::Bytes>,
    /// The response's body, the same way — or a note of what it was when it
    /// was not a buffered one.
    pub(crate) response_body: Option<CapturedBody>,
    /// Stacks captured for entries' code traces, by entry id: walked on the
    /// request's thread, symbolised later by the writer.
    pub(crate) traces: Vec<(String, Arc<Backtrace>)>,
    /// Who made it, as the application described them.
    pub(crate) user: Option<crate::user::LookoutUser>,
    /// What made it: header values copied at the start, read on the writer.
    pub(crate) client: Option<crate::client::ClientRaw>,
    /// What the application said about the visitor's session — `profile`
    /// and the like — through [`Lookout::describe_session`].
    pub(crate) context: Vec<(String, String)>,
    /// From a listed frontend, so held for a browser report to pair with if
    /// it would otherwise be dropped.
    pub(crate) rum_hold: bool,
    /// The root entry, once its activity is heard. It joins the entries when
    /// the batch closes.
    pub(crate) root: Option<Entry>,
    /// Operation span ids, and the span id each ran inside — for entries
    /// whose parent is an operation that was not itself recorded.
    pub(crate) operations: HashMap<String, Option<String>>,
}

impl Pending {
    /// An open batch for `unit`: its id is the unit's.
    fn for_unit(kind: BatchKind, unit: &Unit) -> Self {
        let mut pending = Self::new(kind);
        pending.id = unit.id_string();
        pending.started_at = DateTime::<Utc>::from(unit.started_at());
        pending.started = Instant::now().checked_sub(unit.elapsed()).unwrap_or_else(Instant::now);
        pending.trace_id = unit.trace_id().map(str::to_string);
        pending
    }

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
            response_body: None,
            traces: Vec::new(),
            user: None,
            client: None,
            context: Vec::new(),
            rum_hold: false,
            root: None,
            operations: HashMap::new(),
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
    tx: mpsc::Sender<Closed>,
    rx: tokio::sync::Mutex<mpsc::Receiver<Closed>>,
    wake: Notify,
    writer_started: AtomicBool,
    /// Open batches by unit id.
    pub(crate) open: Mutex<HashMap<u64, Arc<BatchCell>>>,
    /// Closed request batches waiting for their failure report.
    parked: Mutex<Vec<(Instant, Pending)>>,
    identify: RwLock<Option<IdentifyHook>>,
    last_store_error: Mutex<Option<Instant>>,
    /// Aggregates of batches counted but not kept, waiting to be written.
    counts: Mutex<Vec<crate::entry::Aggregate>>,
    /// When `counts` was last written.
    counts_written: Mutex<Instant>,
    /// The bus [`Lookout::subscribe`] subscribed to: where the middleware
    /// opens a request's unit when nothing else has.
    events: OnceLock<Weak<Dispatcher>>,
}

impl Inner {
    /// Whether to do anything at all right now.
    pub(crate) fn active(&self) -> bool {
        self.config.enabled && !self.paused.load(Ordering::Relaxed)
    }

    /// The bus Lookout subscribed to, while it lives.
    pub(crate) fn events(&self) -> Option<Arc<Dispatcher>> {
        self.events.get().and_then(Weak::upgrade)
    }

    /// The open batch of the unit of work the calling code runs in.
    pub(crate) fn current_batch(&self) -> Option<Arc<BatchCell>> {
        let unit = Context::current_unit()?;
        self.cell_for(&unit).filter(|cell| !cell.ignored)
    }

    /// The open batch with id `batch` (its own id or its trace id), or with
    /// `None` the one the calling code runs in.
    pub(crate) fn find(&self, batch: Option<&str>) -> Option<Arc<BatchCell>> {
        let Some(id) = batch else { return self.current_batch() };
        if let Some(unit) = Context::current_unit() {
            if unit.trace_id() == Some(id) || unit.id_string() == id {
                return self.current_batch();
            }
        }
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if id.len() == 16 {
            if let Some(cell) = u64::from_str_radix(id, 16).ok().and_then(|u| open.get(&u)) {
                return Some(Arc::clone(cell)).filter(|cell| !cell.ignored);
            }
        }
        open.values()
            .filter(|cell| !cell.ignored)
            .find(|cell| {
                cell.lock().as_ref().is_some_and(|p| p.trace_id.as_deref() == Some(id))
                    || cell.unit_ref.upgrade().is_some_and(|u| u.trace_id() == Some(id))
            })
            .cloned()
    }

    // --- opening ---------------------------------------------------------

    /// The open batch of `unit`, opened now if this is the first Lookout has
    /// heard of it. `None` when Lookout is off.
    ///
    /// Scope and sampling that the unit's kind, name and path decide are
    /// decided here, once.
    pub(crate) fn cell_for(&self, unit: &Arc<Unit>) -> Option<Arc<BatchCell>> {
        if !self.config.enabled {
            return None;
        }
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cell) = open.get(&unit.id()) {
            return Some(Arc::clone(cell));
        }
        let cell = Arc::new(self.open_batch(unit));
        open.insert(unit.id(), Arc::clone(&cell));
        Some(cell)
    }

    /// A new batch for `unit`.
    fn open_batch(&self, unit: &Arc<Unit>) -> BatchCell {
        let kind = match unit.kind() {
            UnitKind::Request => BatchKind::Request,
            UnitKind::Job => BatchKind::Job,
            UnitKind::Command => BatchKind::Command,
            UnitKind::ScheduledTask => BatchKind::ScheduledTask,
            _ => BatchKind::Other,
        };
        let path = unit.path();
        let name = unit.name();
        let mut pending = Pending::for_unit(kind, unit);
        pending.path = path.map(str::to_string);
        if kind != BatchKind::Request {
            pending.name = name.map(str::to_string);
        }
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
        // Paused: the batch exists, so the unit's events find it, but it
        // records nothing.
        let ignored = self.paused.load(Ordering::Relaxed);
        let rejected = pending.rejected.is_some() || ignored;
        BatchCell {
            root_span: pending.id.clone(),
            state: Mutex::new(Some(pending)),
            rejected: AtomicBool::new(rejected),
            traces: Mutex::new(HashSet::new()),
            kind,
            unit: unit.id(),
            unit_ref: Arc::downgrade(unit),
            ignored,
            root_taken: AtomicBool::new(false),
        }
    }

    /// Close the open batches whose unit is gone without the bus saying so —
    /// a unit opened with no bus never publishes `UnitCompleted`. Once a unit
    /// is gone every event of it has been handled, so nothing is lost.
    pub(crate) fn sweep(&self) {
        let gone: Vec<Arc<BatchCell>> = {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<u64> = open
                .iter()
                .filter(|(_, cell)| cell.unit_ref.strong_count() == 0)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter().filter_map(|id| open.remove(&id)).collect()
        };
        for cell in gone {
            self.close_batch(&cell, None);
        }
    }

    pub(crate) fn is_dashboard_path(&self, path: &str) -> bool {
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

    /// The root activity was heard: kept aside until the batch closes.
    pub(crate) fn set_root(&self, cell: &BatchCell, root: Entry) {
        if let Some(pending) = cell.lock().as_mut() {
            pending.root = Some(root);
        }
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
        // A slice of the request's own buffer — a reference count, not a
        // copy. Nothing is parsed here: that is the writer's job, and only
        // for batches it keeps.
        let body = (capture.request_body || capture.request_body_on_exception)
            .then(|| {
                let body = request.body();
                body.slice(..body.len().min(capture.max_body_bytes))
            })
            .filter(|body| !body.is_empty());
        let client = capture.client.then(|| crate::client::ClientRaw::of(request));
        let rum = &self.config.rum;
        let rum_hold = if rum.enabled && !rum.secret.is_empty() {
            let from_frontend = request.header("origin").is_some_and(|origin| {
                let origin = origin.trim_end_matches('/');
                rum.origins.iter().any(|allowed| allowed == origin)
            });
            from_frontend && rum.hold_seconds > 0
        } else {
            false
        };
        let mut state = cell.lock();
        if let Some(pending) = state.as_mut() {
            pending.rum_hold = rum_hold;
            pending.method.get_or_insert_with(|| request.method().as_str().to_string());
            pending.path.get_or_insert(path);
            pending.request_headers = headers;
            pending.request_body = body;
            pending.client = client;
        }
    }

    /// The client's address as the application settled on it (after its
    /// `TrustProxies`), in place of the peer [`before_request`] saw.
    pub(crate) fn refine_client(&self, request: &rainier_http::Request) {
        if !self.config.capture.client {
            return;
        }
        let Some(ip) = request.ip() else { return };
        let Some(cell) = self.current_batch() else { return };
        let mut state = cell.lock();
        if let Some(client) = state.as_mut().and_then(|p| p.client.as_mut()) {
            client.ip = Some(ip);
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
        let body = self
            .config
            .capture
            .response_body
            .then(|| CapturedBody::of(response, self.config.capture.max_body_bytes));
        {
            let mut state = cell.lock();
            let Some(pending) = state.as_mut() else { return };
            pending.status = Some(response.status().as_u16());
            if let Some(body) = body.flatten() {
                pending.response_body = Some(body);
            }
            if let Some((guard, user)) = identity {
                pending.guard = Some(guard);
                if user.is_some() {
                    pending.user_id = user;
                }
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
        self.push_traced(cell, entry, None);
    }

    /// [`push`](Self::push), with the stack its code trace is made from.
    pub(crate) fn push_traced(
        &self,
        cell: &BatchCell,
        entry: Entry,
        trace: Option<Arc<Backtrace>>,
    ) {
        if cell.is_rejected() && entry.entry_type != EntryType::Exception {
            return;
        }
        if let Some(pending) = cell.lock().as_mut() {
            if pending.rejected.is_some() && entry.entry_type != EntryType::Exception {
                return;
            }
            let id = trace.is_some().then(|| entry.id.clone());
            let before = pending.entries.len();
            pending.push(entry);
            if let (Some(id), Some(trace)) = (id, trace) {
                if pending.entries.len() > before {
                    pending.traces.push((id, trace));
                }
            }
        }
    }

    /// A failure was reported, in `unit` or outside every unit. Into the
    /// unit's batch; or, for a request's failure reported after its unit,
    /// that request's batch (`request`: its method and path); or a batch of
    /// its own.
    pub(crate) fn record_failure(
        &self,
        entry: Entry,
        failed_job: bool,
        unit: Option<&Arc<Unit>>,
        request: Option<(&str, &str)>,
    ) {
        if let Some(cell) = unit.and_then(|u| self.cell_for(u)).filter(|c| !c.ignored) {
            if let Some(pending) = cell.lock().as_mut() {
                pending.failed_job |= failed_job;
                pending.push(entry);
                return;
            }
        }
        if unit.is_none() {
            if let Some((method, path)) = request {
                // Parked: the request's batch closed, waiting for this.
                let claimed = {
                    let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
                    parked
                        .iter()
                        .rposition(|(_, p)| request_is(p, method, path))
                        .map(|at| parked.remove(at).1)
                };
                if let Some(mut pending) = claimed {
                    pending.push(entry);
                    self.finish(pending);
                    return;
                }
                // Still open: its unit is over but not yet completed.
                let open: Vec<Arc<BatchCell>> = self
                    .open
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .filter(|c| c.kind == BatchKind::Request && !c.ignored)
                    .cloned()
                    .collect();
                for cell in open {
                    let mut state = cell.lock();
                    if let Some(pending) = state.as_mut() {
                        if pending.root.is_some() && request_is(pending, method, path) {
                            pending.push(entry);
                            return;
                        }
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

    /// The batch's unit is over: `done` when the bus said so, `None` when
    /// the unit went without a word ([`sweep`](Self::sweep)).
    pub(crate) fn close_batch(&self, cell: &Arc<BatchCell>, done: Option<&UnitCompleted>) {
        let Some(mut pending) = cell.lock().take() else { return };
        self.open.lock().unwrap_or_else(|e| e.into_inner()).remove(&cell.unit);
        if cell.ignored {
            return;
        }
        if let Some(done) = done {
            if pending.trace_id.is_none() {
                pending.trace_id = done.trace_id.clone();
            }
            if pending.name.is_none() && pending.kind != BatchKind::Request {
                pending.name = done.name.clone();
            }
        }
        match pending.root.take() {
            Some(root) => {
                pending.duration_us = root.duration_us;
                pending.started_at = root.occurred_at;
                fill_from_root(&mut pending, &root);
                pending.entries.insert(0, root);
            }
            None => {
                if let Some(done) = done {
                    pending.duration_us =
                        Some(u64::try_from(done.duration.as_micros()).unwrap_or(u64::MAX));
                    pending.started_at = DateTime::<Utc>::from(done.started_at);
                }
            }
        }
        // A command's name may only have come with its root: Lookout's own
        // commands are never recorded.
        if pending.kind == BatchKind::Command
            && pending.rejected.is_none()
            && pending.name.as_deref().is_some_and(|n| n.starts_with("lookout:"))
        {
            pending.rejected = Some(Rejection::Scope);
        }
        // The route stage again, for requests whose route (or lack of one)
        // only the root span's final fields say.
        if pending.kind == BatchKind::Request
            && pending.rejected.is_none()
            && !self.pending_in_scope(&pending)
        {
            pending.rejected = Some(Rejection::Scope);
        }
        // Only a status whose failure will be recorded is worth waiting for:
        // client errors are not, unless `capture.client_errors` says so.
        let awaited = if self.config.capture.client_errors { 400 } else { 500 };
        let reported = pending.entries.iter().any(|e| {
            e.entry_type == EntryType::Exception
                && e.content.get("type").and_then(|t| t.as_str()) == Some("HttpRequest")
        });
        if pending.kind == BatchKind::Request
            && !reported
            && pending.status.is_some_and(|s| s >= awaited)
        {
            let overflow = {
                let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
                parked.push((Instant::now(), pending));
                (parked.len() > MAX_PARKED).then(|| parked.remove(0).1)
            };
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

    /// Hand to the writer, as it is.
    ///
    /// All a request or job pays at its end: deciding whether to keep it,
    /// sorting, summarising and rolling it into the aggregates all happen on
    /// the writer, after the response has gone. Its duration is fixed here,
    /// so time spent waiting in the channel is not counted as its own.
    fn finish(&self, mut pending: Pending) {
        if pending.duration_us.is_none() {
            pending.duration_us =
                Some(u64::try_from(pending.started.elapsed().as_micros()).unwrap_or(u64::MAX));
        }
        self.send(Closed { pending })
    }

    /// `try_send`; full means dropped and counted.
    fn send(&self, closed: Closed) {
        match self.tx.try_send(closed) {
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
    /// Drain the channel into the store, `flush_batch_size` at a time.
    ///
    /// Counts of the batches not kept are added up here and written every
    /// [`COUNTS_EVERY`], with a kept batch when there is one, or when
    /// `force` (a flush, shutdown) says now — so a busy service writes its
    /// aggregates a few times a minute, not once per tick.
    async fn drain_and_store(&self, force: bool) {
        let mut waiting = Vec::new();
        {
            let mut rx = self.rx.lock().await;
            while let Ok(batch) = rx.try_recv() {
                waiting.push(batch);
            }
        }
        let _ = self.queued.try_update(Ordering::Relaxed, Ordering::Relaxed, |q| {
            Some(q.saturating_sub(waiting.len()))
        });
        let size = self.config.ingest.flush_batch_size.max(1);
        // Lookout's own work: outside every unit of work (`Context::none`),
        // so what the store's queries publish lands in no batch, even when
        // `flush` is called from inside a request or a command.
        let span = tracing::info_span!("lookout.write", rainier.lookout.internal = true);
        let mut kept_any = false;
        while !waiting.is_empty() {
            let rest = waiting.split_off(waiting.len().min(size));
            let chunk = std::mem::replace(&mut waiting, rest);
            // On the writer's task, after the requests and jobs have ended.
            let (kept, counted, held) = sort_closed(&self.config, &self.host, chunk);
            if !held.is_empty() {
                let ttl = Duration::from_secs(self.config.rum.hold_seconds);
                let result = tracing::Instrument::instrument(
                    Context::none().scope(async { self.store.hold(held, ttl).await }),
                    span.clone(),
                )
                .await;
                if let Err(error) = result {
                    self.log_store_error(&span, &error);
                }
            }
            {
                let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
                counts.extend(counted);
                if counts.len() > COUNTS_COMBINE_AT {
                    let all = std::mem::take(&mut *counts);
                    *counts = crate::aggregate::combine(all);
                }
            }
            if kept.is_empty() {
                continue;
            }
            kept_any = true;
            let kept = resolve_code_traces(kept).await;
            let counted = self.take_counts();
            self.write(&span, kept, counted).await;
        }
        let due = {
            let last = self.counts_written.lock().unwrap_or_else(|e| e.into_inner());
            last.elapsed() >= COUNTS_EVERY
        };
        if !kept_any && (force || due) {
            let counted = self.take_counts();
            if !counted.is_empty() {
                self.write(&span, Vec::new(), counted).await;
            }
        }
    }

    /// The counts added up since the last write, combined, and the clock reset.
    fn take_counts(&self) -> Vec<crate::entry::Aggregate> {
        *self.counts_written.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        let all = std::mem::take(&mut *self.counts.lock().unwrap_or_else(|e| e.into_inner()));
        crate::aggregate::combine(all)
    }

    async fn write(
        &self,
        span: &tracing::Span,
        kept: Vec<Batch>,
        counted: Vec<crate::entry::Aggregate>,
    ) {
        let result = tracing::Instrument::instrument(
            Context::none().scope(async { self.store.store_counting(kept, counted).await }),
            span.clone(),
        )
        .await;
        if let Err(error) = result {
            self.log_store_error(span, &error);
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

/// A finished request or job on its way to the writer, not yet finalized:
/// whether it is kept, and its summary, are decided there.
pub(crate) struct Closed {
    pending: Pending,
}

/// A kept batch with the stacks of its code traces still to be symbolised.
struct Kept {
    batch: Batch,
    traces: Vec<(String, Arc<Backtrace>)>,
}

/// Finalize what came off the channel: the batches to keep, with their
/// traces, and the roll-up of the entries of the ones not kept.
fn sort_closed(
    config: &LookoutConfig,
    host: &Option<String>,
    closed: Vec<Closed>,
) -> (Vec<Kept>, Vec<crate::entry::Aggregate>, Vec<Batch>) {
    let mut kept = Vec::new();
    let mut held = Vec::new();
    let mut counted: Vec<Entry> = Vec::new();
    for Closed { mut pending } in closed {
        let mut traces = std::mem::take(&mut pending.traces);
        match finalize(config, pending, host.clone()) {
            Finalized::Kept(batch) => {
                let batch = *batch;
                if !traces.is_empty() {
                    let ids: HashSet<&str> = batch.entries.iter().map(|e| e.id.as_str()).collect();
                    traces.retain(|(id, _)| ids.contains(id.as_str()));
                }
                kept.push(Kept { batch, traces });
            }
            Finalized::Held(batch) => {
                // Counted now, as any dropped request is: if a report claims
                // it later it is stored without being counted again.
                counted.extend(batch.entries.iter().cloned());
                held.push(*batch);
            }
            Finalized::Counted(entries) => counted.extend(entries),
        }
    }
    (kept, crate::aggregate::rollup_entries(&counted), held)
}

/// Turn each batch's captured stacks into `code.trace` frames.
///
/// Symbolising a stack — finding the function, file and line of every
/// address in it — costs several times what walking it did, so it happens
/// here, on the writer's side of the channel and on a blocking thread, rather
/// than in the request that captured it.
async fn resolve_code_traces(closed: Vec<Kept>) -> Vec<Batch> {
    if closed.iter().all(|c| c.traces.is_empty()) {
        return closed.into_iter().map(|c| c.batch).collect();
    }
    let resolve = |closed: Vec<Kept>| -> Vec<Batch> {
        closed
            .into_iter()
            .map(|Kept { mut batch, traces }| {
                if traces.is_empty() {
                    return batch;
                }
                let at: HashMap<&str, usize> =
                    batch.entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
                let found: Vec<(usize, Arc<Backtrace>)> = traces
                    .into_iter()
                    .filter_map(|(id, trace)| at.get(id.as_str()).map(|&i| (i, trace)))
                    .collect();
                for (i, trace) in found {
                    let frames = crate::watchers::exceptions::app_frames(
                        &trace.to_string(),
                        CODE_TRACE_FRAMES,
                    );
                    if let (false, serde_json::Value::Object(content)) =
                        (frames.is_empty(), &mut batch.entries[i].content)
                    {
                        content.insert("code.trace".into(), serde_json::Value::Array(frames));
                    }
                }
                batch
            })
            .collect()
    };
    // Only a panic in the resolver loses these batches, and it has none to
    // throw: the frames are parsed from text, not unwrapped.
    tokio::task::spawn_blocking(move || resolve(closed)).await.unwrap_or_default()
}

/// How many of the application's frames a code trace keeps.
const CODE_TRACE_FRAMES: usize = 12;

/// Whether `pending` is the request `method path`.
fn request_is(pending: &Pending, method: &str, path: &str) -> bool {
    let root = pending
        .root
        .as_ref()
        .or_else(|| pending.entries.iter().find(|e| e.entry_type == EntryType::Request));
    let field = |key: &str| root.and_then(|r| r.content.get(key)).and_then(|v| v.as_str());
    let m = pending.method.as_deref().or_else(|| field("http.request.method"));
    let p = pending.path.as_deref().or_else(|| field("url.path"));
    m.is_some_and(|m| m.eq_ignore_ascii_case(method)) && p == Some(path)
}

/// Copy what the root activity said onto the batch, where the middleware did not.
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
                pending.status = content.get("http.response.status_code").and_then(|v| {
                    v.as_u64()
                        .and_then(|s| u16::try_from(s).ok())
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                });
            }
        }
        BatchKind::Job => {
            pending.name = text("rainier.job.name").or(pending.name.take());
            if let Some(outcome) = text("rainier.job.outcome") {
                if matches!(outcome.as_str(), "failed" | "timed_out" | "error") {
                    pending.failed_job = true;
                }
                // A job put back on the queue for another attempt.
                if outcome == "released" {
                    pending.tags.insert("released".to_string());
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
/// A response body as captured on the request's thread: the bytes, cheaply,
/// when it was buffered, and only a note otherwise.
#[derive(Debug, Clone)]
pub(crate) enum CapturedBody {
    /// A buffered body, cut to `max_body_bytes`, with its content type.
    Bytes { bytes: bytes::Bytes, content_type: Option<String>, total: usize },
    /// A streamed body: never read, so never kept.
    Streamed { content_type: Option<String> },
}

impl CapturedBody {
    fn of(response: &rainier_http::Response, max: usize) -> Option<Self> {
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        match response.body() {
            rainier_http::Body::Empty => None,
            rainier_http::Body::Bytes(bytes) if bytes.is_empty() => None,
            rainier_http::Body::Bytes(bytes) => Some(Self::Bytes {
                bytes: bytes.slice(..bytes.len().min(max)),
                content_type,
                total: bytes.len(),
            }),
            rainier_http::Body::Stream(_) => Some(Self::Streamed { content_type }),
        }
    }

    /// As something safe to keep: JSON and forms parsed and redacted, text
    /// truncated, anything else described by its type and size.
    fn redacted(self, redaction: &crate::config::Redaction) -> serde_json::Value {
        match self {
            Self::Streamed { content_type } => serde_json::json!({
                "streamed": true,
                "content_type": content_type,
            }),
            Self::Bytes { bytes, content_type, total } => {
                let textual = content_type.as_deref().is_none_or(|t| {
                    let t = t.to_ascii_lowercase();
                    t.contains("json")
                        || t.starts_with("text/")
                        || t.contains("x-www-form-urlencoded")
                        || t.contains("xml")
                        || t.contains("javascript")
                });
                if textual && std::str::from_utf8(&bytes).is_ok() {
                    crate::redact::redact_body(&bytes, redaction)
                } else {
                    serde_json::json!({ "bytes": total, "content_type": content_type })
                }
            }
        }
    }
}

/// What became of a finished request or job.
pub(crate) enum Finalized {
    /// Kept: stored whole, and rolled into the aggregates. Boxed: one is
    /// moved per finished batch, and most are the small variant.
    Kept(Box<Batch>),
    /// Not kept, but counted — and held a while for a browser report to pair
    /// with: a request from a listed frontend. Stored for good only if a
    /// report claims it. See [`crate::rum`].
    Held(Box<Batch>),
    /// Not kept, but counted: these entries are rolled into the aggregates,
    /// so a route's numbers cover every request and not only the slow ones.
    /// Empty for a batch out of scope, which is counted nowhere.
    Counted(Vec<Entry>),
}

pub(crate) fn finalize(
    config: &LookoutConfig,
    mut pending: Pending,
    host: Option<String>,
) -> Finalized {
    let scope = &config.scope;
    let hold = pending.rum_hold;
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
            if out_of_scope {
                return Finalized::Counted(Vec::new());
            }
            // Sampled out: whatever it did record is still traffic.
            pending.entries.retain(|e| config.watcher(e.entry_type).enabled);
            return Finalized::Counted(pending.entries);
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
        // Most production requests end here: the default keep rule drops a
        // batch with nothing slow, failed or monitored in it, so nothing
        // below — the N+1 search, the sort, the summary — would be kept.
        // Except one held for a browser report: it is built whole, in case
        // the report comes and keeps it.
        if config.keep.is_none()
            && !hold
            && !config.local
            && !monitored
            && pending.kind != BatchKind::Other
            && !has_exception
            && !pending.failed_job
            && !pending.entries.iter().any(|e| e.tags.iter().any(|t| t == "slow"))
        {
            return Finalized::Counted(pending.entries);
        }
        derive_n_plus_one(config, &mut pending.entries);
    }

    resolve_parents(&mut pending);
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

    // The bodies: the request's when every one is captured, or when this one
    // failed; the response's when those are captured. Parsed and redacted
    // here, on the writer, and only for a batch that is kept.
    // A route the application named as never having its bodies kept loses
    // them here, before they are parsed — on a failure too.
    let route = pending.route_name.as_deref().or(pending.route_pattern.as_deref());
    let bodies_allowed = !route.is_some_and(|route| {
        config.capture.bodies_except.iter().any(|p| crate::config::glob(p, route))
    });
    let request_body = pending
        .request_body
        .take()
        .filter(|_| bodies_allowed && (config.capture.request_body || has_exception))
        .map(|body| crate::redact::redact_body(&body, &config.redaction));
    let response_body = pending
        .response_body
        .take()
        .filter(|_| bodies_allowed)
        .map(|body| body.redacted(&config.redaction));
    if request_body.is_some() || response_body.is_some() {
        if let Some(root) = pending.entries.iter_mut().find(|e| e.entry_type == EntryType::Request)
        {
            if let Some(map) = root.content.as_object_mut() {
                if let Some(body) = request_body {
                    map.insert("request.body".into(), body);
                }
                if let Some(body) = response_body {
                    map.insert("response.body".into(), body);
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
    // Who and what made it — read here, on the writer, for kept batches only.
    let user = pending.user.take().filter(|_| config.capture.user);
    let client = pending
        .client
        .take()
        .filter(|_| config.capture.client)
        .map(|raw| raw.describe(config.capture.client_ip));
    if user.is_some() || client.is_some() {
        if let Some(root) = pending.entries.iter_mut().find(|e| e.entry_type == EntryType::Request)
        {
            if let Some(map) = root.content.as_object_mut() {
                if let Some(user) = user.and_then(|u| serde_json::to_value(u).ok()) {
                    map.insert("user".into(), user);
                }
                if let Some(client) = client {
                    map.insert("client".into(), client);
                }
            }
        }
    }

    let summary = batch_summary(&pending);
    // A model change says which unit of work made it, so a list of one
    // model's changes can say "by `POST /posts`" and link to its trace without
    // loading every batch.
    let made_by = serde_json::json!({
        "kind": pending.kind,
        "summary": summary,
        "trace_id": trace_id,
    });
    for entry in pending.entries.iter_mut().filter(|e| e.entry_type == EntryType::Model) {
        if let Some(map) = entry.content.as_object_mut() {
            map.insert("batch".into(), made_by.clone());
        }
    }
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
    if keep {
        Finalized::Kept(Box::new(batch))
    } else if hold {
        Finalized::Held(Box::new(batch))
    } else {
        Finalized::Counted(batch.entries)
    }
}

/// Point every entry whose parent is an operation that was not recorded —
/// its watcher off, sampled out — at the nearest enclosing one that was, or
/// at the root.
fn resolve_parents(pending: &mut Pending) {
    if pending.operations.is_empty() {
        return;
    }
    let root = pending.id.clone();
    let known: HashSet<String> = pending.entries.iter().filter_map(|e| e.span_id.clone()).collect();
    let operations = std::mem::take(&mut pending.operations);
    for entry in &mut pending.entries {
        let Some(mut parent) = entry.parent_span_id.clone() else { continue };
        let mut hops = 0;
        while parent != root && !known.contains(&parent) && hops < 64 {
            parent = match operations.get(&parent) {
                Some(Some(up)) => up.clone(),
                _ => root.clone(),
            };
            hops += 1;
        }
        entry.parent_span_id = Some(parent);
    }
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
    // Borrowed while grouping: only the runs that qualify copy their SQL.
    let mut runs: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.entry_type != EntryType::Query {
            continue;
        }
        if let Some(sql) = &entry.family_hash {
            runs.entry(sql.as_str()).or_default().push(i);
        }
    }
    let runs: Vec<(String, Vec<usize>)> = runs
        .into_iter()
        .filter(|(_, indexes)| indexes.len() >= rule.min_repeats.max(2))
        .filter(|(_, indexes)| {
            let total: u64 = indexes.iter().filter_map(|&i| entries[i].duration_us).sum();
            total >= rule.min_total_ms.saturating_mul(1000)
        })
        .map(|(sql, indexes)| (sql.to_string(), indexes))
        .collect();
    let mut derived = Vec::new();
    for (sql, indexes) in runs {
        let total: u64 = indexes.iter().filter_map(|&i| entries[i].duration_us).sum();
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

pub(crate) fn parse_level(level: &str) -> Option<tracing::Level> {
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
fn random_u64() -> u64 {
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
    use crate::testing::{inside, query, served, timed, Did, TRACE};
    use rainier_events::report::{ApplicationFailed, ExceptionReported, HttpRequestFailed};
    use rainier_events::{Operation, UnitKind};
    use rainier_support::Error;

    /// An enabled Lookout over a fresh memory store, subscribed to a bus of
    /// its own.
    struct Rig {
        lookout: Lookout,
        store: Arc<MemoryStore>,
        events: Arc<Dispatcher>,
    }

    impl Rig {
        /// Wait for the subscriber to hear everything published so far, then
        /// write what closed.
        async fn flush(&self) {
            self.events.settled().await;
            self.lookout.flush().await;
        }
    }

    fn rig(config: LookoutConfig) -> Rig {
        let store = Arc::new(MemoryStore::new(100));
        let lookout = Lookout::new(config, store.clone());
        let events = Arc::new(Dispatcher::new());
        lookout.subscribe(&events);
        Rig { lookout, store, events }
    }

    fn local() -> LookoutConfig {
        LookoutConfig::default().enabled(true).local(true)
    }

    fn production() -> LookoutConfig {
        LookoutConfig::default().enabled(true).local(false)
    }

    /// A request unit for `path`, in trace [`TRACE`], on the rig's bus.
    fn request_unit(rig: &Rig, path: &str) -> Arc<Unit> {
        let unit = Unit::on(&rig.events, UnitKind::Request, None, Some(path.to_string()));
        unit.set_trace(Some(TRACE), None);
        unit
    }

    /// One request: route `posts.show`, status `status`, `work` inside it.
    fn request(rig: &Rig, path: &str, status: u16, work: impl FnOnce()) {
        let unit = request_unit(rig, path);
        let _entered = unit.enter();
        // The server's clock starts before the work inside it.
        let started = Instant::now();
        timed(work);
        served(path, Some(status), Some(("/posts/{post}", "posts.show")), started.elapsed())
            .publish();
    }

    /// A request with no route, `work` inside it.
    fn unrouted(rig: &Rig, path: &str, status: u16, work: impl FnOnce()) {
        let unit = request_unit(rig, path);
        let _entered = unit.enter();
        let started = Instant::now();
        timed(work);
        served(path, Some(status), None, started.elapsed()).publish();
    }

    #[tokio::test]
    async fn a_request_becomes_a_batch_with_its_queries_nested() {
        let rig = rig(local());
        request(&rig, "/posts/1", 200, || {
            query("SELECT * FROM posts WHERE id = ?");
            // A cache read that ran a query of its own: an operation, which
            // the query names as its parent.
            let op = Operation::new();
            let ((), took) = timed(|| inside(&op, || query("SELECT * FROM users WHERE id = ?")));
            Did::new("cache.get")
                .with("cache.store", "memory")
                .with("cache.operation", "get")
                .with("cache.hit", true)
                .with("cache.key", "posts:1")
                .for_operation(&op)
                .took(took)
                .publish();
        });
        rig.flush().await;

        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.trace_id, TRACE, "the trace id");
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
    async fn the_batch_id_is_the_unit_id() {
        let rig = rig(local());
        let unit = request_unit(&rig, "/posts/1");
        let id = unit.id_string();
        {
            let _entered = unit.enter();
            assert_eq!(rig.lookout.current_batch_id().as_deref(), Some(TRACE), "its trace");
            served("/posts/1", Some(200), None, Duration::from_millis(1)).publish();
        }
        drop(unit);
        rig.flush().await;
        assert_eq!(rig.store.all()[0].id, id);
    }

    #[tokio::test]
    async fn an_entry_under_an_operation_that_is_not_recorded_hangs_off_the_nearest_that_is() {
        // The cache watcher off: the query ran inside a cache read that is
        // not an entry, so it hangs off the request.
        let rig = rig(local().disable(EntryType::Cache));
        request(&rig, "/posts/1", 200, || {
            let op = Operation::new();
            inside(&op, || query("SELECT 1"));
            Did::new("cache.get").with("cache.operation", "get").for_operation(&op).publish();
        });
        rig.flush().await;
        let batch = &rig.store.all()[0];
        let root = &batch.entries[0];
        let query = batch.entries.iter().find(|e| e.entry_type == EntryType::Query).unwrap();
        assert_eq!(query.parent_span_id, root.span_id);
    }

    #[tokio::test]
    async fn cache_keys_are_kept_only_when_opted_in() {
        let mut config = local();
        config.capture.cache_keys = true;
        let rig = rig(config);
        request(&rig, "/posts/1", 200, || {
            Did::new("cache.get")
                .with("cache.operation", "get")
                .with("cache.key", "posts:1")
                .publish();
        });
        rig.flush().await;
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
        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        rig.flush().await;

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
    async fn ordinary_requests_are_counted_though_not_kept() {
        // Production: nothing slow or failed, so the keep rule stores none of
        // them — but every one is in the counts, or a route's percentiles
        // would describe only its slow runs.
        use crate::store::Store as _;
        let rig = rig(production());
        for _ in 0..3 {
            request(&rig, "/posts/1", 200, || query("SELECT 1"));
        }
        rig.flush().await;

        assert!(rig.store.all().is_empty(), "none kept");
        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        let count = |rows: Vec<crate::entry::Aggregate>| rows.iter().map(|r| r.count).sum::<u64>();
        let requests = rig.store.aggregate_rows(EntryType::Request, 60, since).await.unwrap();
        assert_eq!(count(requests.clone()), 3, "every request counted: {requests:?}");
        assert!(requests.iter().all(|r| !r.histogram.is_empty()), "and timed");
        let queries = rig.store.aggregate_rows(EntryType::Query, 60, since).await.unwrap();
        assert_eq!(count(queries), 3, "and every query in them");
    }

    #[tokio::test]
    async fn out_of_scope_requests_are_neither_kept_nor_counted() {
        use crate::store::Store as _;
        let config = production().scope(Scope {
            routes: Patterns { include: vec![], exclude: vec!["posts.*".into()] },
            ..LookoutConfig::default().scope
        });
        let rig = rig(config);
        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        rig.flush().await;

        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        assert!(rig.store.aggregate_rows(EntryType::Request, 60, since).await.unwrap().is_empty());
        assert!(rig.store.aggregate_rows(EntryType::Query, 60, since).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn slow_overrides_glob_over_the_family_key() {
        let config = production().watch(EntryType::Query, |mut w| {
            w.slow_ms = Some(0);
            w.slow_overrides.insert("SELECT * FROM reports*".into(), 60_000);
            w
        });
        let rig = rig(config);
        request(&rig, "/r", 200, || query("SELECT * FROM reports WHERE year = ?"));
        rig.flush().await;
        assert!(rig.store.all().is_empty(), "the override raised the bar: nothing slow, not kept");
    }

    #[tokio::test]
    async fn a_run_of_the_same_query_is_an_n_plus_one() {
        let mut config = local();
        config.n_plus_one.min_total_ms = 0;
        let rig = rig(config);
        request(&rig, "/posts", 200, || {
            query("SELECT * FROM posts");
            for _ in 0..5 {
                query("SELECT * FROM users WHERE id = ?");
                query("SELECT * FROM avatars WHERE user_id = ?");
            }
            query("SELECT * FROM tags WHERE id = ?");
        });
        rig.flush().await;

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
        request(&rig, "/posts", 200, || (0..4).for_each(|_| query("SELECT 1")));
        rig.flush().await;
        let batch = &rig.store.all()[0];
        assert!(!batch.entries.iter().any(|e| e.entry_type == EntryType::NPlusOne));
    }

    #[tokio::test]
    async fn an_excluded_route_records_nothing_and_buffers_nothing_once_its_route_is_known() {
        let config = local().scope(Scope {
            routes: Patterns { include: vec![], exclude: vec!["posts.*".into()] },
            ..LookoutConfig::default().scope
        });
        let rig = rig(config);
        let unit = request_unit(&rig, "/posts/1");
        {
            let _entered = unit.enter();
            query("SELECT 1");
            served("/posts/1", Some(200), Some(("/posts/{post}", "posts.show")), Duration::ZERO)
                .publish();
            query("SELECT 2");
        }
        // The unit is still open: its batch is there to look at.
        rig.events.settled().await;
        {
            let cell = rig.lookout.inner.open.lock().unwrap().get(&unit.id()).cloned();
            let cell = cell.expect("open until its unit completes");
            assert!(cell.is_rejected(), "rejected as soon as the route was known");
            assert!(cell.lock().as_ref().unwrap().entries.is_empty(), "nothing buffered");
        }
        drop(unit);
        rig.flush().await;
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
            unrouted(&rig, path, 404, || query("SELECT 1"));
        }
        rig.flush().await;
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
        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        rig.flush().await;
        assert!(rig.store.all().is_empty());
    }

    #[tokio::test]
    async fn failures_are_kept_even_when_sampled_out() {
        let rig = rig(LookoutConfig { request_sample_rate: 0.0, ..production() });
        let unit = request_unit(&rig, "/posts/1");
        let events = Arc::clone(&rig.events);
        unit.scope(async {
            query("SELECT 1");
            events
                .dispatch(ExceptionReported {
                    failure: ApplicationFailed::new(Error::internal("disk full")).into(),
                })
                .await
                .unwrap();
            served("/posts/1", Some(200), None, Duration::from_millis(1)).publish();
        })
        .await;
        drop(unit);
        rig.flush().await;

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
        rig.events
            .dispatch(ExceptionReported {
                failure: ApplicationFailed::new(Error::internal("cron 12 broke")).into(),
            })
            .await
            .unwrap();
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].kind, BatchKind::Other);
        assert_eq!(batches[0].summary, "Internal: cron 12 broke");
        assert_eq!(
            batches[0].entries[0].family_hash.as_deref(),
            Some("Application|Internal|cron N broke")
        );
    }

    fn boom() -> ExceptionReported {
        ExceptionReported {
            failure: HttpRequestFailed::new(
                Error::internal("boom"),
                "GET",
                "/posts/1",
                Some("/posts/{post}".to_string()),
            )
            .into(),
        }
    }

    #[tokio::test]
    async fn a_failed_request_reported_after_its_unit_joins_its_batch() {
        let rig = rig(production());
        request(&rig, "/posts/1", 500, || query("SELECT 1"));
        // What the kernel may do once `handle` has returned: no unit around it.
        rig.events.dispatch(boom()).await.unwrap();
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1, "{batches:#?}");
        let batch = &batches[0];
        assert_eq!(batch.kind, BatchKind::Request);
        assert!(batch.has_exception);
        assert_eq!(batch.entries.len(), 3, "request, query, exception");
    }

    #[tokio::test]
    async fn a_failed_request_reported_after_its_batch_was_parked_joins_it() {
        let rig = rig(production());
        request(&rig, "/posts/1", 500, || query("SELECT 1"));
        // The batch has closed and is waiting: the report comes later still.
        rig.events.settled().await;
        assert_eq!(rig.lookout.inner.parked.lock().unwrap().len(), 1, "parked");
        rig.events.dispatch(boom()).await.unwrap();
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1, "{batches:#?}");
        assert!(batches[0].has_exception);
        assert_eq!(batches[0].entries.len(), 3, "request, query, exception");
    }

    #[tokio::test]
    async fn a_failed_request_reported_inside_its_unit_is_not_parked() {
        let rig = rig(production());
        let unit = request_unit(&rig, "/posts/1");
        let events = Arc::clone(&rig.events);
        unit.scope(async {
            served("/posts/1", Some(500), None, Duration::from_millis(1)).publish();
            events.dispatch(boom()).await.unwrap();
        })
        .await;
        drop(unit);
        rig.events.settled().await;
        assert!(rig.lookout.inner.parked.lock().unwrap().is_empty(), "nothing to wait for");
        rig.lookout.flush().await;
        assert_eq!(rig.store.all().len(), 1);
        assert!(rig.store.all()[0].has_exception);
    }

    #[tokio::test]
    async fn a_client_error_is_not_a_failure_unless_asked_for() {
        let not_found = || ExceptionReported {
            failure: HttpRequestFailed::new(
                Error::not_found("no such post"),
                "GET",
                "/posts/9",
                Some("/posts/{post}".to_string()),
            )
            .into(),
        };

        let quiet = rig(local());
        request(&quiet, "/posts/9", 404, || {});
        quiet.events.dispatch(not_found()).await.unwrap();
        quiet.flush().await;
        let batches = quiet.store.all();
        assert_eq!(batches.len(), 1, "{batches:#?}");
        assert!(!batches[0].has_exception, "a 404 is not a failure");
        assert!(batches[0].entries.iter().all(|e| e.entry_type != EntryType::Exception));

        let mut config = local();
        config.capture.client_errors = true;
        let loud = rig(config);
        request(&loud, "/posts/9", 404, || {});
        loud.events.dispatch(not_found()).await.unwrap();
        loud.flush().await;
        let batches = loud.store.all();
        assert_eq!(batches.len(), 1, "{batches:#?}");
        assert!(batches[0].has_exception, "recorded when asked for");
    }

    #[tokio::test]
    async fn a_websocket_refusing_its_client_is_not_a_failure_unless_asked_for() {
        use rainier_events::report::WebSocketFailed;

        let refused = || -> rainier_events::report::Failure {
            WebSocketFailed::new(Error::unauthorized("revoked"), "1", "/socket", "connect").into()
        };

        let quiet = rig(local());
        quiet.lookout.record_failure(&refused());
        quiet.flush().await;
        assert!(quiet.store.all().is_empty(), "a refused client is not a failure");

        let mut config = local();
        config.capture.client_errors = true;
        let loud = rig(config);
        loud.lookout.record_failure(&refused());
        loud.flush().await;
        let batches = loud.store.all();
        assert_eq!(batches.len(), 1, "{batches:#?}");
        assert!(batches[0].has_exception, "recorded when asked for");

        // A socket that failed on the server's side is still a failure.
        let rig = rig(local());
        let broken: rainier_events::report::Failure =
            WebSocketFailed::new(Error::internal("boom"), "2", "/socket", "message").into();
        rig.lookout.record_failure(&broken);
        rig.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn a_parked_batch_nobody_claims_is_released_on_flush() {
        let rig = rig(local());
        request(&rig, "/posts/1", 500, || {});
        rig.events.settled().await;
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
        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        request(&rig, "/posts/2", 200, || {
            assert!(probe.tag(None, "vip"));
            query("SELECT 2");
        });
        rig.flush().await;
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
        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        request(&rig, "/posts/2", 200, || {
            let id = probe.current_batch_id().expect("a batch");
            assert!(probe.tag(Some(&id), "vip"), "by its trace id");
            query("SELECT 2");
        });
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].entries.len(), 2, "with its query");
    }

    #[tokio::test]
    async fn an_open_batch_is_found_by_its_id_from_anywhere() {
        let config = production().scope(Scope {
            monitored_tags: ["vip".to_string()].into(),
            ..LookoutConfig::default().scope
        });
        let rig = rig(config);
        let unit = Unit::on(&rig.events, UnitKind::Request, None, Some("/x".into()));
        {
            let _entered = unit.enter();
            query("SELECT 1");
        }
        // Not from inside the unit: by its id alone, once the subscriber has
        // opened the batch.
        rig.events.settled().await;
        assert!(rig.lookout.tag(Some(&unit.id_string()), "vip"));
        {
            let _entered = unit.enter();
            served("/x", Some(200), None, Duration::from_millis(1)).publish();
        }
        drop(unit);
        rig.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn a_custom_keep_rule_replaces_the_default() {
        let rig = rig(production().keep_batch_if(|b| b.status == Some(200)));
        request(&rig, "/posts/1", 200, || {});
        rig.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn a_full_channel_drops_and_counts() {
        let mut config = local();
        config.ingest.channel_capacity = 1;
        let rig = rig(config);
        for _ in 0..3 {
            request(&rig, "/posts/1", 200, || {});
        }
        rig.events.settled().await;
        assert_eq!(rig.lookout.dropped(), 2);
        rig.lookout.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    #[tokio::test]
    async fn log_lines_are_recorded_and_redacted() {
        let rig = rig(LookoutConfig { log_level: "warn".into(), ..local() });
        request(&rig, "/login", 200, || {
            Did::new("log")
                .with("level", "WARN")
                .with("target", "app::auth")
                .with("message", "login 3 failed")
                .with("password", "hunter2")
                .with("user", "ada")
                .publish();
            Did::new("log")
                .with("level", "INFO")
                .with("target", "app::auth")
                .with("message", "below the level")
                .publish();
        });
        rig.flush().await;
        let batch = &rig.store.all()[0];
        let logs: Vec<&Entry> =
            batch.entries.iter().filter(|e| e.entry_type == EntryType::Log).collect();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].summary, "login 3 failed");
        assert_eq!(logs[0].content["level"], "warn");
        assert_eq!(logs[0].content["target"], "app::auth");
        assert_eq!(logs[0].content["password"], crate::redact::REDACTED);
        assert_eq!(logs[0].content["user"], "ada");
        assert_eq!(logs[0].family_hash.as_deref(), Some("warn login N failed"));
        assert_eq!(logs[0].tags, vec!["warn".to_string()]);
    }

    #[tokio::test]
    async fn the_writer_flushes_on_its_interval_and_on_shutdown() {
        let mut config = local();
        config.ingest.flush_interval_ms = 10;
        let rig = rig(config);
        let writer = rig.lookout.start_writer();
        assert!(writer.is_running());
        assert!(!rig.lookout.start_writer().is_running(), "only one writer");

        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        for _ in 0..200 {
            if !rig.store.all().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(rig.store.all().len(), 1, "flushed by the interval");

        request(&rig, "/posts/2", 200, || {});
        rig.events.settled().await;
        writer.shutdown().await;
        assert_eq!(rig.store.all().len(), 2, "flushed on shutdown");
    }

    /// A store whose writes run a query of their own, as the database store
    /// does — published on the bus Lookout listens to.
    struct Noisy(MemoryStore, Arc<Dispatcher>);

    impl Noisy {
        fn query(&self) {
            self.1.publish_activity(
                Did::new("db.query")
                    .with("db.query.text", "INSERT INTO lookout_batches VALUES (?)"),
            );
        }
    }

    impl Store for Noisy {
        fn name(&self) -> &'static str {
            "noisy"
        }
        fn store(
            &self,
            batches: Vec<Batch>,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<()>> {
            self.query();
            self.0.store(batches)
        }
        fn store_counting(
            &self,
            batches: Vec<Batch>,
            counted: Vec<crate::entry::Aggregate>,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<()>> {
            self.query();
            self.0.store_counting(batches, counted)
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
        fn aggregate_rows(
            &self,
            t: EntryType,
            p: u32,
            s: chrono::DateTime<chrono::Utc>,
        ) -> rainier_support::BoxFuture<'_, rainier_support::Result<Vec<crate::entry::Aggregate>>>
        {
            self.0.aggregate_rows(t, p, s)
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
        let events = Arc::new(Dispatcher::new());
        let store = Arc::new(Noisy(MemoryStore::new(10), Arc::clone(&events)));
        let lookout = Lookout::new(local(), store.clone());
        lookout.subscribe(&events);
        let rig = Rig { lookout: lookout.clone(), store: Arc::new(MemoryStore::new(1)), events };

        // A request, written by a store that queries while it writes — and
        // written from inside a command, whose batch must not gain the
        // store's query either.
        request(&rig, "/posts/1", 200, || {});
        rig.events.settled().await;
        let command = Unit::on(&rig.events, UnitKind::Command, Some("deploy".into()), None);
        let flushing = lookout.clone();
        command
            .scope(async move {
                flushing.flush().await;
                flushing.flush().await;
            })
            .await;
        {
            let _entered = command.enter();
            Did::new("console.command").with("rainier.command.name", "deploy").publish();
        }
        drop(command);
        rig.flush().await;
        rig.flush().await;

        let all = store.0.all();
        assert_eq!(all.len(), 2, "the request and the command: {all:#?}");
        for batch in &all {
            assert_eq!(batch.entries.len(), 1, "just the root: {batch:#?}");
        }
    }

    #[tokio::test]
    async fn disabled_records_nothing() {
        let rig = rig(LookoutConfig::default().local(true));
        assert!(!rig.events.has_listeners::<ExceptionReported>(), "no subscriber registered");
        assert_eq!(rig.events.subscriber_count(), 0);
        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        rig.lookout.record_failure(&ApplicationFailed::new(Error::internal("x")).into());
        rig.flush().await;
        assert!(!rig.lookout.start_writer().is_running());
        assert!(rig.store.all().is_empty());
    }

    #[tokio::test]
    async fn paused_opens_no_batches() {
        let rig = rig(local());
        rig.lookout.pause();
        request(&rig, "/posts/1", 200, || {});
        rig.events.settled().await;
        rig.lookout.resume();
        request(&rig, "/posts/2", 200, || {});
        rig.flush().await;
        assert_eq!(rig.store.all().len(), 1);
    }

    /// One run of a job inside whatever unit is current — a worker command.
    /// Inside a request it joins the request's unit, as the queue's `sync`
    /// driver does; anywhere else it opens its own on the rig's bus.
    fn job(rig: &Rig, outcome: &'static str, trace: Option<&str>) {
        let _bus = rainier_events::set_default(Arc::clone(&rig.events));
        let job = Unit::open_or_join(UnitKind::Job, Some("SendInvoice".into()), None);
        let _entered = job.as_ref().map(|job| job.enter());
        query("UPDATE invoices SET sent = 1");
        // The trace is learned after the unit opened, from the job's
        // stored `traceparent`.
        if let Some(job) = &job {
            job.set_trace(trace, None);
        }
        Did::new("queue.process")
            .with("rainier.job.name", "SendInvoice")
            .with("rainier.job.outcome", outcome)
            .publish();
    }

    #[tokio::test]
    async fn a_job_inside_a_worker_command_is_its_own_batch_and_its_failure_keeps_it() {
        let rig = rig(production());
        let command = Unit::on(&rig.events, UnitKind::Command, Some("queue:work".into()), None);
        {
            let _entered = command.enter();
            for outcome in ["processed", "failed"] {
                job(&rig, outcome, None);
            }
        }
        drop(command);
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1, "only the failed job is interesting: {batches:#?}");
        let job = &batches[0];
        assert_eq!(job.kind, BatchKind::Job);
        assert_eq!(job.summary, "job SendInvoice");
        assert!(job.tags.contains(&"failed".to_string()));
        assert_eq!(job.entries.len(), 2, "the job and its query");
    }

    #[tokio::test]
    async fn a_released_job_is_tagged_so() {
        let rig = rig(local());
        job(&rig, "released", None);
        rig.flush().await;
        let batches = rig.store.all();
        assert!(batches[0].tags.contains(&"released".to_string()), "{:?}", batches[0].tags);
        assert!(!batches[0].tags.contains(&"failed".to_string()));
    }

    #[tokio::test]
    async fn a_job_inside_a_request_is_part_of_it() {
        let rig = rig(local());
        request(&rig, "/posts", 200, || job(&rig, "processed", None));
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        let types: Vec<EntryType> = batches[0].entries.iter().map(|e| e.entry_type).collect();
        assert_eq!(types, vec![EntryType::Request, EntryType::Query, EntryType::Job]);
    }

    #[tokio::test]
    async fn a_job_joins_the_trace_its_unit_learns_after_opening() {
        for config in [local(), local().disable(EntryType::Job)] {
            let rig = rig(config);
            job(&rig, "processed", Some(TRACE));
            rig.flush().await;
            let batches = rig.store.all();
            assert_eq!(batches.len(), 1);
            assert_eq!(batches[0].kind, BatchKind::Job);
            assert_eq!(batches[0].trace_id, TRACE);
        }
    }

    #[tokio::test]
    async fn jobs_and_commands_can_be_scoped_out() {
        let rig = rig(local().scope(Scope { jobs: false, ..LookoutConfig::default().scope }));
        let run = |kind: UnitKind, name: &str, root: Did, sql: &'static str| {
            let unit = Unit::on(&rig.events, kind, Some(name.to_string()), None);
            let _entered = unit.enter();
            query(sql);
            root.publish();
        };
        run(
            UnitKind::Job,
            "J",
            Did::new("queue.process").with("rainier.job.name", "J"),
            "SELECT 1",
        );
        run(
            UnitKind::Command,
            "lookout:prune",
            Did::new("console.command").with("rainier.command.name", "lookout:prune"),
            "DELETE",
        );
        run(
            UnitKind::Command,
            "migrate",
            Did::new("console.command").with("rainier.command.name", "migrate"),
            "CREATE",
        );
        rig.flush().await;
        let summaries: Vec<String> = rig.store.all().iter().map(|b| b.summary.clone()).collect();
        assert_eq!(summaries, vec!["command migrate"]);
    }

    #[tokio::test]
    async fn a_unit_with_no_bus_is_closed_once_it_is_gone() {
        // A unit opened without a bus never says it completed; its batch is
        // closed when the unit is gone.
        let rig = rig(local());
        let unit = Unit::new(UnitKind::Request, None, Some("/quiet".into()));
        {
            let _bus = rainier_events::set_default(Arc::clone(&rig.events));
            let _entered = unit.enter();
            query("SELECT 1");
            served("/quiet", Some(200), None, Duration::from_millis(1)).publish();
        }
        drop(unit);
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].entries.len(), 2);
    }

    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk)]
        id: u64,
        title: String,
    }

    impl rainier_database::Model for Post {}

    /// A request whose `work` runs inside its unit, across awaits.
    async fn request_async<F: std::future::Future>(rig: &Rig, path: &str, work: F) -> F::Output {
        let unit = request_unit(rig, path);
        let started = std::time::Instant::now();
        let out = unit
            .scope(async {
                let out = work.await;
                served(path, Some(200), None, started.elapsed()).publish();
                out
            })
            .await;
        drop(unit);
        out
    }

    /// A fake database whose writes go to the bus Lookout subscribed to —
    /// what boot wires for every connection.
    fn on_the_bus(rig: &Rig) -> rainier_database::Database {
        use rainier_database::testing::{fake_database, MemoryConnection};
        let (db, _) = fake_database(MemoryConnection::new(rainier_orm::Dialect::Sqlite));
        db.dispatch_changes_to(Arc::clone(&rig.events));
        db
    }

    fn models(batch: &Batch) -> Vec<&Entry> {
        batch.entries.iter().filter(|e| e.entry_type == EntryType::Model).collect()
    }

    #[tokio::test]
    async fn a_write_through_a_plain_repository_is_a_model_entry() {
        use rainier_database::{Criteria, EntityRepository, Repository};

        let rig = rig(local());
        let db = on_the_bus(&rig);
        // No `with_events`: the typed `Created<Post>` never fires. The
        // write is on the bus as a `ModelChanged` all the same.
        let posts = EntityRepository::<Post>::new(db.clone());

        request_async(&rig, "/posts", async {
            posts.create(Post { id: 7, title: "Hello".into() }).await.expect("create");
            posts
                .update_matching(
                    Criteria::new().where_eq("title", "Hello"),
                    vec![("title".into(), "Hi".into())],
                )
                .await
                .expect("bulk update");
            posts.delete(7u64.into()).await.expect("delete");
        })
        .await;
        // Outside every unit: Lookout's own store, a background task.
        posts.delete(8u64.into()).await.expect("delete");

        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        let summaries: Vec<&str> = models(batch).iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, vec!["Post created #7", "Post updated", "Post deleted #7"]);

        let created = models(batch)[0];
        assert_eq!(created.batch_id, batch.id);
        assert_eq!(created.family_key(), "Post created");
        assert_eq!(created.content["keys"], serde_json::json!(["7"]));
        assert!(created.content["model_type"].as_str().is_some_and(|t| t.ends_with("::Post")));
        // Who made it, for a list of one model's changes.
        assert_eq!(created.content["batch"]["trace_id"], TRACE);
        assert_eq!(created.content["batch"]["kind"], "request");
        assert_eq!(created.content["batch"]["summary"], batch.summary);
        assert_eq!(models(batch)[1].content["keys"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn model_changes_are_counted_when_their_batch_is_not_kept() {
        let rig = rig(production());
        let probe = rig.lookout.clone();
        request(&rig, "/posts", 200, || {
            probe.record_model_change(&rainier_database::ModelChanged {
                model: "Post".into(),
                model_type: Some("shop::Post"),
                table: "posts".into(),
                action: rainier_database::ModelAction::Updated,
                keys: Some(vec!["7".into()]),
            });
        });
        rig.flush().await;
        assert!(rig.store.all().is_empty(), "an ordinary request is not kept in production");
        let counted = rig
            .store
            .aggregate_rows(EntryType::Model, 3600, Utc::now() - chrono::Duration::hours(1))
            .await
            .expect("rows");
        assert_eq!(counted.len(), 1);
        assert_eq!((counted[0].key.as_str(), counted[0].count), ("Post updated", 1));
    }

    #[tokio::test]
    async fn the_model_watcher_off_records_no_model_change() {
        use rainier_database::{EntityRepository, Repository};

        let rig = rig(local().disable(EntryType::Model));
        let db = on_the_bus(&rig);
        let posts = EntityRepository::<Post>::new(db);
        request_async(&rig, "/posts", async {
            posts.create(Post { id: 7, title: "Hello".into() }).await.expect("create");
        })
        .await;
        rig.flush().await;
        assert!(models(&rig.store.all()[0]).is_empty());
    }

    #[tokio::test]
    async fn a_disabled_watcher_drops_its_entries() {
        let rig = rig(local().disable(EntryType::Query));
        request(&rig, "/posts/1", 200, || query("SELECT 1"));
        rig.flush().await;
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
            request(&rig, "/posts/1", 200, || {
                let cell = probe.inner.current_batch().unwrap();
                let mut state = cell.lock();
                let pending = state.as_mut().unwrap();
                pending.guard = Some("session".into());
                pending.user_id = Some(user.into());
            });
        }
        rig.flush().await;
        let batches = rig.store.all();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].guard.as_deref(), Some("session"));
        assert_eq!(batches[0].user_id, None, "ids are kept only with capture.user_id");
    }
}
