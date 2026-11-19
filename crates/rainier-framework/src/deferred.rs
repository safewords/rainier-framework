//! Work that runs after a request without the response waiting on it —
//! [`defer`], and the [`Deferrer`] that tracks it.
//!
//! ```no_run
//! use rainier_framework::deferred::defer;
//!
//! # async fn touch_last_seen(_: u64) -> Result<(), rainier_framework::support::Error> { Ok(()) }
//! # fn handler(profile_id: u64) {
//! // The response does not wait for this; graceful shutdown does.
//! defer("profiles.touch_last_seen", async move { touch_last_seen(profile_id).await });
//! # }
//! ```
//!
//! What a bare `tokio::spawn` does not give you, and this does:
//!
//! | | `tokio::spawn` | [`defer`] |
//! |---|---|---|
//! | the request's span, trace id and facade application | lost | carried into the task |
//! | an `Err` | dropped unless you log it | logged at `warn` with the task's name |
//! | a panic | logged by tokio, unnamed | logged at `error` with the task's name |
//! | metrics | none | `deferred_tasks_total` and `deferred_task_duration_seconds` per name |
//! | a pod rolling | the task is dropped mid-write | `serve` waits for it, up to a bound |
//! | a slow database | tasks pile up without limit | past a ceiling new work is shed and counted |
//!
//! # Not a queue
//!
//! Deferred work lives in this process's memory. It is for work that is cheap
//! to lose in a crash and wrong to make a client wait for — touching a
//! `last_seen_at`, bumping a view counter, warming a cache. Anything that must
//! happen belongs on a [queue](rainier_queue), which survives the process and
//! retries. The difference is the price: dispatching a job is itself a round
//! trip to the queue's backend, which is what deferring avoids.
//!
//! # Without an application
//!
//! [`defer`] uses the application's [`Deferrer`] when one is bound (the
//! bootstrap binds one), and otherwise a process-wide one — so code that
//! defers runs unchanged in a unit test with no application. A test that needs
//! the deferred work to have happened before it asserts calls [`settle`].

use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rainier_container::{try_facade_application, with_facade_application, Application};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tracing::Instrument;

/// How deferred work ended, as a low-cardinality label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeferOutcome {
    /// It ran and returned `Ok` (or `()`).
    Completed,
    /// It returned `Err`.
    Failed,
    /// It panicked.
    Panicked,
    /// It ran past the [`Deferrer::timeout`] and was cancelled.
    TimedOut,
    /// It never ran: the [ceiling](Deferrer::max_in_flight) was reached, or
    /// there was no runtime to run it on.
    Shed,
}

impl DeferOutcome {
    /// The label value — `completed`, `failed`, `panicked`, `timed_out`, `shed`.
    pub fn as_str(self) -> &'static str {
        match self {
            DeferOutcome::Completed => "completed",
            DeferOutcome::Failed => "failed",
            DeferOutcome::Panicked => "panicked",
            DeferOutcome::TimedOut => "timed_out",
            DeferOutcome::Shed => "shed",
        }
    }
}

impl fmt::Display for DeferOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether [`defer`] took the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deferral {
    /// It was spawned and is tracked until it finishes.
    Spawned,
    /// It was dropped without running — see [`DeferOutcome::Shed`].
    Shed,
}

impl Deferral {
    /// Whether the work was spawned.
    pub fn is_spawned(self) -> bool {
        self == Deferral::Spawned
    }
}

/// What a deferred future may return.
///
/// `()` always succeeds; a `Result` fails on `Err`, which is logged with its
/// `Display`. The `Ok` value is discarded — there is nobody to give it to.
pub trait DeferredOutput: Send + 'static {
    /// `None` for success, or the error's message.
    fn into_failure(self) -> Option<String>;
}

impl DeferredOutput for () {
    fn into_failure(self) -> Option<String> {
        None
    }
}

impl<T: Send + 'static, E: fmt::Display + Send + 'static> DeferredOutput for Result<T, E> {
    fn into_failure(self) -> Option<String> {
        self.err().map(|e| e.to_string())
    }
}

/// Where a [`Deferrer`] reports what it measured.
///
/// Called on the task itself, so an implementation must be cheap and must not
/// block. `rainier-framework` wires [`DeferredMetrics`]
/// over the application's `Metrics` registry when one is bound at boot.
pub trait DeferObserver: Send + Sync + 'static {
    /// Work named `name` was spawned.
    fn started(&self, name: &str);

    /// Work named `name` ended. `elapsed` is zero for [`DeferOutcome::Shed`],
    /// which also has no matching [`started`](Self::started).
    fn finished(&self, name: &str, outcome: DeferOutcome, elapsed: Duration);
}

/// What [`Deferrer::drain`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drained {
    /// How many tasks were running when the drain began.
    pub waited_for: usize,
    /// How many were still running when it gave up. Zero means everything
    /// finished.
    pub abandoned: usize,
}

struct Inner {
    in_flight: AtomicUsize,
    idle: Notify,
    limit: Option<Arc<Semaphore>>,
    max_in_flight: Option<usize>,
    timeout: Option<Duration>,
    observer: Option<Arc<dyn DeferObserver>>,
}

/// Spawns deferred work and keeps count of it, so shutdown can wait.
///
/// Cheap to clone: clones share the count, the ceiling and the observer.
///
/// ```
/// use std::time::Duration;
/// use rainier_framework::deferred::Deferrer;
///
/// # #[tokio::main] async fn main() {
/// let deferrer = Deferrer::new().max_in_flight(256).timeout(Duration::from_secs(30));
///
/// deferrer.spawn("cache.warm", async { Ok::<_, std::io::Error>(()) });
///
/// let drained = deferrer.drain(Duration::from_secs(10)).await;
/// assert_eq!(drained.abandoned, 0);
/// # }
/// ```
#[derive(Clone)]
pub struct Deferrer {
    inner: Arc<Inner>,
}

impl Default for Deferrer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Deferrer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deferrer")
            .field("in_flight", &self.in_flight())
            .field("max_in_flight", &self.inner.max_in_flight)
            .field("timeout", &self.inner.timeout)
            .field("observed", &self.inner.observer.is_some())
            .finish()
    }
}

impl Deferrer {
    /// No ceiling, no timeout, no observer.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                in_flight: AtomicUsize::new(0),
                idle: Notify::new(),
                limit: None,
                max_in_flight: None,
                timeout: None,
                observer: None,
            }),
        }
    }

    fn rebuild(self, edit: impl FnOnce(&mut Inner)) -> Self {
        let mut inner = Inner {
            in_flight: AtomicUsize::new(self.in_flight()),
            idle: Notify::new(),
            limit: self.inner.limit.clone(),
            max_in_flight: self.inner.max_in_flight,
            timeout: self.inner.timeout,
            observer: self.inner.observer.clone(),
        };
        edit(&mut inner);
        Self { inner: Arc::new(inner) }
    }

    /// At most `max` tasks at once; past that, new work is
    /// [shed](DeferOutcome::Shed) — logged, counted, and not run. `0` means
    /// no ceiling.
    ///
    /// Shed rather than queued, because the ceiling exists for the case where
    /// the work is slow — a database that has stopped answering — and
    /// queueing it in memory is the unbounded pile-up the ceiling is there to
    /// prevent. Configure before spawning anything.
    pub fn max_in_flight(self, max: usize) -> Self {
        self.rebuild(|inner| {
            inner.max_in_flight = (max > 0).then_some(max);
            inner.limit = (max > 0).then(|| Arc::new(Semaphore::new(max)));
        })
    }

    /// Cancel a task that runs longer than `timeout`. `Duration::ZERO` means
    /// none.
    ///
    /// Worth setting alongside a ceiling: a task stuck on a dead connection
    /// holds its slot until it is cancelled.
    pub fn timeout(self, timeout: Duration) -> Self {
        self.rebuild(|inner| inner.timeout = (!timeout.is_zero()).then_some(timeout))
    }

    /// Report every task's outcome and duration to `observer`.
    pub fn with_observer(self, observer: Arc<dyn DeferObserver>) -> Self {
        self.rebuild(|inner| inner.observer = Some(observer))
    }

    /// How many tasks are running now.
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::Acquire)
    }

    /// Run `work` in the background, named `name`.
    ///
    /// Returns at once. The task runs inside a `deferred` span whose parent
    /// is the span current here — the request's, carrying its trace id — and
    /// with the current facade application in scope.
    ///
    /// `name` is a metric label, so it is a `&'static str` that names the
    /// *kind* of work (`profiles.touch_last_seen`), never anything per call.
    ///
    /// Outside a tokio runtime the work is shed with a warning rather than
    /// panicking, as [`tokio::spawn`] would.
    pub fn spawn<F>(&self, name: &'static str, work: F) -> Deferral
    where
        F: Future + Send + 'static,
        F::Output: DeferredOutput,
    {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(deferred = name, "deferred work shed: no tokio runtime");
            self.observe_shed(name);
            return Deferral::Shed;
        };

        let permit = match &self.inner.limit {
            Some(limit) => match Arc::clone(limit).try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    tracing::warn!(
                        deferred = name,
                        max_in_flight = self.inner.max_in_flight.unwrap_or_default(),
                        "deferred work shed: too many deferred tasks in flight"
                    );
                    self.observe_shed(name);
                    return Deferral::Shed;
                }
            },
            None => None,
        };

        let guard = InFlight::enter(Arc::clone(&self.inner), permit);
        if let Some(observer) = &self.inner.observer {
            observer.started(name);
        }

        let span = tracing::info_span!("deferred", deferred = name);
        let task = run(name, work, guard).instrument(span);

        match try_facade_application() {
            Some(app) => drop(runtime.spawn(with_facade_application(app, task))),
            None => drop(runtime.spawn(task)),
        }
        Deferral::Spawned
    }

    fn observe_shed(&self, name: &str) {
        if let Some(observer) = &self.inner.observer {
            observer.finished(name, DeferOutcome::Shed, Duration::ZERO);
        }
    }

    /// Wait until nothing is in flight.
    ///
    /// No bound — for tests. Shutdown uses [`drain`](Self::drain).
    pub async fn settle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            // Registered before the count is read, so a task finishing in
            // between still wakes us.
            notified.as_mut().enable();
            if self.in_flight() == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Wait up to `timeout` for in-flight work to finish, and say what was
    /// left.
    ///
    /// Work deferred during the drain is waited for too. Whatever is still
    /// running at the deadline is abandoned to the runtime — it is dropped
    /// when the process exits — and logged.
    pub async fn drain(&self, timeout: Duration) -> Drained {
        let waited_for = self.in_flight();
        if waited_for == 0 {
            return Drained { waited_for, abandoned: 0 };
        }

        tracing::info!(in_flight = waited_for, "waiting for deferred work to finish");
        let _ = tokio::time::timeout(timeout, self.settle()).await;

        let abandoned = self.in_flight();
        if abandoned > 0 {
            tracing::warn!(
                abandoned,
                timeout_ms = timeout.as_millis() as u64,
                "shutting down with deferred work still running"
            );
        }
        Drained { waited_for, abandoned }
    }
}

/// Counts a task in, and out again however it ends — including being dropped
/// by a runtime that is shutting down.
struct InFlight {
    inner: Arc<Inner>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl InFlight {
    fn enter(inner: Arc<Inner>, permit: Option<OwnedSemaphorePermit>) -> Self {
        inner.in_flight.fetch_add(1, Ordering::AcqRel);
        Self { inner, _permit: permit }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.inner.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.idle.notify_waiters();
        }
    }
}

async fn run<F>(name: &'static str, work: F, guard: InFlight)
where
    F: Future + Send + 'static,
    F::Output: DeferredOutput,
{
    let started = Instant::now();
    let caught = CatchUnwind(Box::pin(work));

    let outcome = match guard.inner.timeout {
        Some(limit) => tokio::time::timeout(limit, caught).await.ok(),
        None => Some(caught.await),
    };
    let elapsed = started.elapsed();
    let elapsed_ms = elapsed.as_millis() as u64;

    let outcome = match outcome {
        None => {
            tracing::warn!(
                deferred = name,
                elapsed_ms,
                "deferred work timed out and was cancelled"
            );
            DeferOutcome::TimedOut
        }
        Some(Err(panic)) => {
            tracing::error!(deferred = name, elapsed_ms, panic = %panic, "deferred work panicked");
            DeferOutcome::Panicked
        }
        Some(Ok(output)) => match output.into_failure() {
            Some(error) => {
                tracing::warn!(deferred = name, elapsed_ms, error = %error, "deferred work failed");
                DeferOutcome::Failed
            }
            None => DeferOutcome::Completed,
        },
    };

    if let Some(observer) = &guard.inner.observer {
        observer.finished(name, outcome, elapsed);
    }
    drop(guard);
}

/// A future that turns a panic in `poll` into an `Err` carrying its message.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let future = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| future.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Err(payload) => Poll::Ready(Err(panic_message(payload.as_ref()))),
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "a panic with a non-string payload".to_string()
    }
}

/// A [`DeferObserver`] over the application's `Metrics` registry.
///
/// | Metric | Type | Labels |
/// |---|---|---|
/// | `deferred_tasks_total` | counter | name, outcome |
/// | `deferred_task_duration_seconds` | histogram | name, outcome |
/// | `deferred_tasks_in_flight` | gauge | — |
///
/// `name` is the `&'static str` given to [`defer`], so the series count is
/// fixed by the code, not by traffic. Shed work is counted but not timed.
pub struct DeferredMetrics {
    metrics: Arc<rainier_metrics::Metrics>,
    in_flight: AtomicUsize,
}

/// From a millisecond `UPDATE` to a minute-long call to a slow provider.
const DURATION_BUCKETS: [f64; 13] =
    [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 60.0];

impl DeferredMetrics {
    /// Declare the deferred-work metrics on `metrics`.
    pub fn new(metrics: Arc<rainier_metrics::Metrics>) -> Self {
        metrics.counter("deferred_tasks_total", "Deferred tasks finished or shed, by outcome");
        metrics.histogram_with(
            "deferred_task_duration_seconds",
            "How long deferred tasks ran, by outcome",
            DURATION_BUCKETS,
        );
        metrics.gauge("deferred_tasks_in_flight", "Deferred tasks this process is running now");
        Self { metrics, in_flight: AtomicUsize::new(0) }
    }

    fn set_in_flight(&self, value: usize) {
        self.metrics.set("deferred_tasks_in_flight", rainier_metrics::Labels::new(), value as f64);
    }
}

impl DeferObserver for DeferredMetrics {
    fn started(&self, _name: &str) {
        self.set_in_flight(self.in_flight.fetch_add(1, Ordering::AcqRel) + 1);
    }

    fn finished(&self, name: &str, outcome: DeferOutcome, elapsed: Duration) {
        let labels = || rainier_metrics::labels([("name", name), ("outcome", outcome.as_str())]);
        self.metrics.increment("deferred_tasks_total", labels());
        if outcome == DeferOutcome::Shed {
            return;
        }
        self.metrics.observe("deferred_task_duration_seconds", labels(), elapsed.as_secs_f64());
        // Only `Shed` finishes without having started, and it returned above.
        let previous = self.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.set_in_flight(previous.saturating_sub(1));
    }
}

impl fmt::Debug for DeferredMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeferredMetrics").finish_non_exhaustive()
    }
}

/// The process-wide [`Deferrer`], for when no application has bound one.
pub fn global() -> &'static Deferrer {
    static GLOBAL: OnceLock<Deferrer> = OnceLock::new();
    GLOBAL.get_or_init(Deferrer::new)
}

/// The deferrer [`defer`] uses here: the facade application's, or
/// [`global`].
pub fn current() -> Deferrer {
    try_facade_application()
        .and_then(|app| app.try_resolve::<Deferrer>())
        .map(|deferrer| (*deferrer).clone())
        .unwrap_or_else(|| global().clone())
}

/// Run `work` in the background of the current request, named `name`.
///
/// See [`Deferrer::spawn`] for what is carried into the task and what
/// `name` should be, and the [module](self) for when not to use this.
pub fn defer<F>(name: &'static str, work: F) -> Deferral
where
    F: Future + Send + 'static,
    F::Output: DeferredOutput,
{
    current().spawn(name, work)
}

/// Wait until the current deferrer has nothing in flight. For tests.
pub async fn settle() {
    current().settle().await;
    // Work deferred with no application while one was bound — or the other
    // way round — is in the other one.
    global().settle().await;
}

/// Build the application's deferrer from configuration and bind it, unless
/// the application bound its own.
pub(crate) fn install(app: &Application) {
    if app.bound::<Deferrer>() {
        return;
    }
    let (max, timeout) = match app.try_resolve::<rainier_config::Config>() {
        Some(config) => (
            config.get(crate::keys::DEFER_MAX_IN_FLIGHT).unwrap_or(DEFAULT_MAX_IN_FLIGHT),
            config.get(crate::keys::DEFER_TIMEOUT_SECS).unwrap_or(0),
        ),
        None => (DEFAULT_MAX_IN_FLIGHT, 0),
    };

    let mut deferrer =
        Deferrer::new().max_in_flight(max as usize).timeout(Duration::from_secs(timeout));
    if let Some(metrics) = app.try_resolve::<rainier_metrics::Metrics>() {
        deferrer = deferrer.with_observer(Arc::new(DeferredMetrics::new(metrics)));
    }
    app.instance(deferrer);
}

/// Wait for the application's deferred work — and the process-wide
/// deferrer's — before the process exits, bounded by
/// `defer.shutdown_timeout_secs`.
pub async fn drain_on_shutdown(app: &Application) -> Drained {
    let seconds = app
        .try_resolve::<rainier_config::Config>()
        .and_then(|config| config.get(crate::keys::DEFER_SHUTDOWN_TIMEOUT_SECS))
        .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
    let deadline = Instant::now() + Duration::from_secs(seconds);

    let mut drained = match app.try_resolve::<Deferrer>() {
        Some(deferrer) => deferrer.drain(Duration::from_secs(seconds)).await,
        None => Drained { waited_for: 0, abandoned: 0 },
    };
    let rest = global().drain(deadline.saturating_duration_since(Instant::now())).await;
    drained.waited_for += rest.waited_for;
    drained.abandoned += rest.abandoned;
    drained
}

/// The ceiling when `defer.max_in_flight` is unset.
pub const DEFAULT_MAX_IN_FLIGHT: u64 = 1024;

/// How long shutdown waits when `defer.shutdown_timeout_secs` is unset —
/// inside Kubernetes' default 30-second grace period, with room for the rest
/// of shutdown.
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        started: Mutex<Vec<String>>,
        finished: Mutex<Vec<(String, DeferOutcome)>>,
    }

    impl DeferObserver for Recorder {
        fn started(&self, name: &str) {
            self.started.lock().unwrap().push(name.to_string());
        }
        fn finished(&self, name: &str, outcome: DeferOutcome, _: Duration) {
            self.finished.lock().unwrap().push((name.to_string(), outcome));
        }
    }

    fn observed() -> (Deferrer, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        (Deferrer::new().with_observer(recorder.clone()), recorder)
    }

    #[tokio::test]
    async fn the_caller_does_not_wait_and_settle_does() {
        let deferrer = Deferrer::new();
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let done = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&done);

        let spawned = deferrer.spawn("test.gated", async move {
            let _ = gate.await;
            seen.fetch_add(1, Ordering::SeqCst);
        });

        // Returned while the work is still blocked on the gate.
        assert!(spawned.is_spawned());
        assert_eq!(deferrer.in_flight(), 1);
        assert_eq!(done.load(Ordering::SeqCst), 0);

        release.send(()).unwrap();
        deferrer.settle().await;
        assert_eq!(done.load(Ordering::SeqCst), 1);
        assert_eq!(deferrer.in_flight(), 0);
    }

    #[tokio::test]
    async fn outcomes_are_reported_by_name() {
        let (deferrer, recorder) = observed();

        deferrer.spawn("ok", async {});
        deferrer.spawn("ok.result", async { Ok::<_, String>(1) });
        deferrer.spawn("err", async { Err::<(), _>("database is read-only") });
        deferrer.spawn("boom", async {
            if true {
                panic!("deferred panic");
            }
        });
        deferrer.settle().await;

        let mut finished = recorder.finished.lock().unwrap().clone();
        finished.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            finished,
            vec![
                ("boom".to_string(), DeferOutcome::Panicked),
                ("err".to_string(), DeferOutcome::Failed),
                ("ok".to_string(), DeferOutcome::Completed),
                ("ok.result".to_string(), DeferOutcome::Completed),
            ]
        );
        assert_eq!(recorder.started.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn past_the_ceiling_work_is_shed_and_counted() {
        let recorder = Arc::new(Recorder::default());
        let deferrer = Deferrer::new().max_in_flight(1).with_observer(recorder.clone());
        let (release, gate) = tokio::sync::oneshot::channel::<()>();

        assert_eq!(deferrer.spawn("slow", async move { drop(gate.await) }), Deferral::Spawned);
        assert_eq!(deferrer.spawn("slow", async {}), Deferral::Shed);
        assert_eq!(
            recorder.finished.lock().unwrap().as_slice(),
            &[("slow".to_string(), DeferOutcome::Shed)]
        );

        // The slot comes back when the first one finishes.
        release.send(()).unwrap();
        deferrer.settle().await;
        assert_eq!(deferrer.spawn("slow", async {}), Deferral::Spawned);
        deferrer.settle().await;
    }

    #[tokio::test]
    async fn a_task_past_its_timeout_is_cancelled() {
        let (deferrer, recorder) = observed();
        let deferrer = deferrer.timeout(Duration::from_millis(20));

        deferrer.spawn("stuck", std::future::pending::<()>());
        deferrer.settle().await;

        assert_eq!(
            recorder.finished.lock().unwrap().as_slice(),
            &[("stuck".to_string(), DeferOutcome::TimedOut)]
        );
    }

    #[tokio::test]
    async fn drain_waits_for_work_and_gives_up_at_the_deadline() {
        let deferrer = Deferrer::new();
        let finished = Arc::new(AtomicUsize::new(0));

        let seen = Arc::clone(&finished);
        deferrer.spawn("quick", async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            seen.fetch_add(1, Ordering::SeqCst);
        });
        deferrer.spawn("forever", std::future::pending::<()>());

        let drained = deferrer.drain(Duration::from_millis(300)).await;
        assert_eq!(drained, Drained { waited_for: 2, abandoned: 1 });
        assert_eq!(finished.load(Ordering::SeqCst), 1, "the quick one was waited for");
    }

    #[tokio::test]
    async fn drain_with_nothing_in_flight_returns_at_once() {
        let drained = Deferrer::new().drain(Duration::from_secs(3600)).await;
        assert_eq!(drained, Drained { waited_for: 0, abandoned: 0 });
    }

    #[test]
    fn outside_a_runtime_work_is_shed_not_a_panic() {
        let (deferrer, recorder) = observed();
        assert_eq!(deferrer.spawn("no.runtime", async {}), Deferral::Shed);
        assert_eq!(
            recorder.finished.lock().unwrap().as_slice(),
            &[("no.runtime".to_string(), DeferOutcome::Shed)]
        );
    }

    // That the task runs inside the caller's span is
    // `tests/deferred_span.rs`: a span test in this binary races the other
    // tests over tracing's per-callsite interest cache.

    #[tokio::test]
    async fn the_task_sees_the_callers_facade_application() {
        let app = Arc::new(Application::new("."));
        app.instance(7u8);
        let deferrer = Deferrer::new();
        let (tx, rx) = tokio::sync::oneshot::channel();

        with_facade_application(Arc::clone(&app), async {
            deferrer.spawn("facades", async move {
                let seen = try_facade_application().and_then(|a| a.try_resolve::<u8>());
                let _ = tx.send(seen.map(|v| *v));
            });
        })
        .await;

        assert_eq!(rx.await.unwrap(), Some(7));
    }

    #[tokio::test]
    async fn defer_without_an_application_uses_the_global_deferrer() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        assert!(defer("no.app", async move {
            let _ = tx.send(());
        })
        .is_spawned());
        settle().await;
        rx.await.unwrap();
    }

    #[tokio::test]
    async fn defer_uses_the_applications_deferrer_when_one_is_bound() {
        let app = Arc::new(Application::new("."));
        let (bound, recorder) = observed();
        app.instance(bound.clone());

        with_facade_application(Arc::clone(&app), async {
            defer("bound", async {});
        })
        .await;
        bound.settle().await;

        assert_eq!(recorder.started.lock().unwrap().as_slice(), &["bound".to_string()]);
    }

    #[tokio::test]
    async fn metrics_count_and_time_by_name_and_outcome() {
        let metrics = Arc::new(rainier_metrics::Metrics::new());
        let deferrer = Deferrer::new()
            .max_in_flight(1)
            .with_observer(Arc::new(DeferredMetrics::new(Arc::clone(&metrics))));
        let (release, gate) = tokio::sync::oneshot::channel::<()>();

        deferrer.spawn("touch", async move {
            let _ = gate.await;
            Err::<(), _>("database is read-only")
        });
        deferrer.spawn("touch", async {});
        assert_eq!(
            metrics.value("deferred_tasks_in_flight", &rainier_metrics::Labels::new()),
            Some(1.0)
        );
        release.send(()).unwrap();
        deferrer.settle().await;

        let by = |outcome| rainier_metrics::labels([("name", "touch"), ("outcome", outcome)]);
        assert_eq!(metrics.value("deferred_tasks_total", &by("failed")), Some(1.0));
        assert_eq!(metrics.value("deferred_tasks_total", &by("shed")), Some(1.0));
        assert_eq!(
            metrics.value("deferred_tasks_in_flight", &rainier_metrics::Labels::new()),
            Some(0.0)
        );
        assert!(metrics
            .render()
            .contains("deferred_task_duration_seconds_count{name=\"touch\",outcome=\"failed\"} 1"));
    }

    #[tokio::test]
    async fn drain_on_shutdown_waits_for_the_applications_work() {
        let app = Application::new(".");
        install(&app);
        let deferrer = app.try_resolve::<Deferrer>().expect("install binds one");
        let finished = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&finished);
        deferrer.spawn("shutdown", async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            seen.fetch_add(1, Ordering::SeqCst);
        });

        let drained = drain_on_shutdown(&app).await;
        assert_eq!(drained.abandoned, 0);
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }
}
