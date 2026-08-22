//! The [`Worker`] loop, and the events it fires.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rainier_cache::LockManager;
use rainier_container::Container;
use rainier_events::Dispatcher;
use rainier_support::{Error, Result};

use crate::job::{JobContext, JobRegistry, QueuedJob};
use crate::queue::Queue;

/// Fired before a job runs.
#[derive(Debug, Clone)]
pub struct JobProcessing {
    /// The job about to run.
    pub job: QueuedJob,
}

/// Fired after a job succeeds.
#[derive(Debug, Clone)]
pub struct JobProcessed {
    /// The job that ran.
    pub job: QueuedJob,
    /// How long `handle` took.
    pub duration: Duration,
}

/// Fired when a job fails but has attempts left.
#[derive(Debug, Clone)]
pub struct JobReleased {
    /// The job going back on the queue.
    pub job: QueuedJob,
    /// How long before it may run again.
    pub delay: Duration,
    /// Why it failed.
    pub error: String,
}

/// Fired when a job fails on its last attempt.
#[derive(Debug, Clone)]
pub struct JobFailed {
    /// The job that gave up.
    pub job: QueuedJob,
    /// Why it failed.
    pub error: String,
}

/// What one turn of the loop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing was waiting.
    Idle,
    /// A job ran and succeeded.
    Processed,
    /// A job failed and went back on the queue.
    Released,
    /// A job failed for the last time.
    Failed,
}

/// How a worker behaves.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// The queues to serve, in priority order — earlier ones are drained
    /// first, which is how a `high` queue gets ahead of `default`.
    pub queues: Vec<String>,
    /// How long to wait before looking again when every queue is empty.
    pub sleep: Duration,
    /// Stop after this many jobs. `None` runs forever.
    pub max_jobs: Option<u64>,
    /// Stop as soon as the queues are empty, rather than waiting for more.
    pub stop_when_empty: bool,
    /// Give up on a job that runs longer than this.
    pub timeout: Option<Duration>,
    /// Stop once the worker has been running this long.
    ///
    /// A long-lived process accumulates whatever it leaks — memory,
    /// connections, file descriptors. Recycling on a clock is the blunt but
    /// reliable answer, and it pairs with `max_jobs`: whichever limit is
    /// reached first ends the run, and the supervisor starts a fresh worker.
    ///
    /// `None` runs until stopped.
    pub max_time: Option<Duration>,
    /// A floor on how many attempts a job gets.
    ///
    /// Individual jobs carry their own `max_attempts`, chosen when they are
    /// dispatched. This raises that for jobs that did not ask for more, so a
    /// worker can be told "retry anything up to three times" without every
    /// dispatch site having to say so.
    ///
    /// It only ever raises. A job that explicitly asked for more attempts than
    /// this keeps them — the worker is expressing a default, not a ceiling.
    pub tries: Option<u32>,
    /// How many **consecutive** broker failures to ride out before giving up.
    ///
    /// Reserving a job talks to the broker, and a broker has outages: a
    /// failover, a resharding, a shard whose master is briefly gone. None of
    /// those are the worker's fault and all of them end. Exiting on the first
    /// one hands the problem to the supervisor, which restarts into the same
    /// outage, backs off, and ends up running the worker for a smaller and
    /// smaller fraction of the time exactly when the backlog is growing.
    ///
    /// Consecutive, so this is not a budget that a long-lived worker slowly
    /// spends on unrelated blips; one success resets it.
    pub max_consecutive_errors: u32,
    /// The longest the wait between broker retries grows to.
    pub max_error_backoff: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            queues: vec!["default".to_string()],
            sleep: Duration::from_secs(1),
            max_jobs: None,
            stop_when_empty: false,
            timeout: Some(Duration::from_secs(60)),
            max_time: None,
            tries: None,
            // Around eight minutes of a broker being unreachable, given the
            // default one-second sleep and thirty-second ceiling. Long enough
            // to sit through a cluster failover without noticing; short
            // enough that a worker pointed at a broker that is never coming
            // back still exits and says so.
            max_consecutive_errors: 20,
            max_error_backoff: Duration::from_secs(30),
        }
    }
}

impl WorkerOptions {
    /// Serve these queues, in priority order.
    pub fn queues(mut self, queues: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.queues = queues.into_iter().map(Into::into).collect();
        self
    }

    /// Wait this long between polls of an empty queue.
    pub fn sleep(mut self, sleep: Duration) -> Self {
        self.sleep = sleep;
        self
    }

    /// Stop after `max` jobs — for a worker that should recycle periodically.
    pub fn max_jobs(mut self, max: u64) -> Self {
        self.max_jobs = Some(max);
        self
    }

    /// Return as soon as there is nothing left to do.
    pub fn stop_when_empty(mut self) -> Self {
        self.stop_when_empty = true;
        self
    }

    /// Abandon a job that exceeds this duration.
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Stop once the worker has run this long, so a supervisor can replace it.
    pub fn max_time(mut self, max_time: Duration) -> Self {
        self.max_time = Some(max_time);
        self
    }

    /// Give every job at least this many attempts.
    ///
    /// Raises a job's own `max_attempts` if it asked for fewer; never lowers
    /// one that asked for more.
    pub fn tries(mut self, tries: u32) -> Self {
        self.tries = Some(tries);
        self
    }

    /// Give up after this many back-to-back broker failures.
    ///
    /// One is the old behaviour: exit as soon as the broker refuses once.
    pub fn max_consecutive_errors(mut self, max: u32) -> Self {
        self.max_consecutive_errors = max.max(1);
        self
    }

    /// Cap the wait between broker retries.
    pub fn max_error_backoff(mut self, backoff: Duration) -> Self {
        self.max_error_backoff = backoff;
        self
    }

    /// How many attempts this job actually gets under these options.
    pub fn attempts_for(&self, job_max_attempts: u32) -> u32 {
        job_max_attempts.max(self.tries.unwrap_or(0)).max(1)
    }

    /// How long to wait after `consecutive` back-to-back broker failures.
    ///
    /// Doubles from [`sleep`](Self::sleep), capped at
    /// [`max_error_backoff`](Self::max_error_backoff). Backing off matters as
    /// much as not exiting: a worker that retried a dead shard in a tight loop
    /// would answer an outage with load, and every replica would do it at
    /// once.
    pub fn error_backoff(&self, consecutive: u32) -> Duration {
        let doublings = consecutive.saturating_sub(1).min(16);
        self.sleep.saturating_mul(1u32 << doublings).min(self.max_error_backoff)
    }
}

/// What a worker run did in total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Jobs that succeeded.
    pub processed: u64,
    /// Jobs that failed but were retried.
    pub released: u64,
    /// Jobs that gave up.
    pub failed: u64,
    /// Turns of the loop that found nothing.
    pub idles: u64,
    /// Turns of the loop the broker refused.
    ///
    /// Not a job outcome — nothing was reserved, so nothing ran. Counted
    /// because a run that processed a hundred jobs and swallowed nine hundred
    /// broker failures is not the healthy run its other three numbers
    /// describe.
    pub errors: u64,
}

impl WorkerStats {
    /// How many jobs finished, one way or the other.
    pub fn total(&self) -> u64 {
        self.processed + self.failed
    }
}

/// Pulls jobs off a queue and runs them.
pub struct Worker {
    queue: Arc<dyn Queue>,
    registry: Arc<JobRegistry>,
    container: Arc<Container>,
    events: Option<Arc<Dispatcher>>,
    options: WorkerOptions,
    stopping: AtomicBool,
    /// Where a finished job's [`unique_id`](crate::Job::unique_id) lock is
    /// released.
    ///
    /// `None` means uniqueness is not enforced here, and a job carrying a key
    /// simply keeps its lock until `UNIQUE_FOR` expires — degraded rather than
    /// broken.
    locks: Option<LockManager>,
}

impl Worker {
    /// Enforce [`unique_id`](crate::Job::unique_id) releases with `locks`.
    ///
    /// The same manager the dispatching side uses, or the release will not find
    /// the key. `Worker::from_manager` wires it for you.
    pub fn with_locks(mut self, locks: LockManager) -> Self {
        self.locks = Some(locks);
        self
    }

    /// A worker serving `queue`, running the jobs `registry` knows.
    pub fn new(
        queue: Arc<dyn Queue>,
        registry: Arc<JobRegistry>,
        container: Arc<Container>,
    ) -> Self {
        Self {
            queue,
            registry,
            container,
            events: None,
            locks: None,
            options: WorkerOptions::default(),
            stopping: AtomicBool::new(false),
        }
    }

    /// Fire lifecycle events through `events`.
    pub fn with_events(mut self, events: Arc<Dispatcher>) -> Self {
        self.events = Some(events);
        self
    }

    /// Configure the loop.
    pub fn with_options(mut self, options: WorkerOptions) -> Self {
        self.options = options;
        self
    }

    /// Ask the loop to stop after the job in flight.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Whether a stop has been requested.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Reserve and run one job from the first queue that has one.
    pub async fn run_next(&self) -> Result<Outcome> {
        for queue in &self.options.queues {
            if let Some(job) = self.queue.reserve(queue).await? {
                return self.process(job).await;
            }
        }
        Ok(Outcome::Idle)
    }

    /// Run until stopped, drained, or the job limit is reached.
    ///
    /// Between jobs the worker flushes the container's **scoped** bindings, so
    /// one job cannot leak per-request-shaped state into the next. That is the
    /// long-running-process hazard a per-request framework never has to think
    /// about.
    ///
    /// # A broker failure is not the end of the run
    ///
    /// Reserving a job can fail for reasons that have nothing to do with the
    /// worker or the job: the broker is failing over, a shard has no master,
    /// the network blinked. This used to propagate, which ended the process —
    /// and under an orchestrator that is a restart, into the same outage, with
    /// a backoff that grows each time. A queue whose consumers are in
    /// `CrashLoopBackOff` is not being consumed at all, and it stays that way
    /// for minutes after the broker itself is healthy again.
    ///
    /// So a failure here is logged, waited out (see
    /// [`error_backoff`](WorkerOptions::error_backoff)) and retried, and only
    /// [`max_consecutive_errors`](WorkerOptions::max_consecutive_errors)
    /// back-to-back failures end the run. It is the same judgement already
    /// made for a panicking job and for a uniqueness lock that will not
    /// release: what the worker can survive, it survives.
    pub async fn run(&self) -> Result<WorkerStats> {
        let mut stats = WorkerStats::default();
        let started = std::time::Instant::now();
        let mut consecutive_errors: u32 = 0;

        loop {
            if self.is_stopping() {
                break;
            }
            if self.options.max_jobs.is_some_and(|max| stats.total() >= max) {
                break;
            }
            // Checked between jobs, never mid-job: a worker that abandoned
            // work halfway through to meet a deadline would be a worse problem
            // than the leak the deadline exists to bound.
            if self.options.max_time.is_some_and(|max| started.elapsed() >= max) {
                tracing::info!(max_time = ?self.options.max_time, "worker reached its time limit");
                break;
            }

            match self.run_next().await {
                Ok(outcome) => {
                    consecutive_errors = 0;

                    match outcome {
                        Outcome::Idle => {
                            stats.idles += 1;
                            if self.options.stop_when_empty {
                                break;
                            }
                            tokio::time::sleep(self.options.sleep).await;
                        }
                        Outcome::Processed => stats.processed += 1,
                        Outcome::Released => stats.released += 1,
                        Outcome::Failed => stats.failed += 1,
                    }
                }
                Err(error) => {
                    stats.errors += 1;
                    consecutive_errors += 1;

                    // Still failing after the whole allowance: report it as
                    // the run's outcome rather than looping forever. A worker
                    // pointed at a broker that is genuinely gone should end
                    // and say why, not impersonate a healthy one.
                    if consecutive_errors >= self.options.max_consecutive_errors {
                        tracing::error!(
                            error = %error.message(),
                            consecutive = consecutive_errors,
                            "the broker has failed too many times in a row; giving up"
                        );
                        return Err(error);
                    }

                    let backoff = self.options.error_backoff(consecutive_errors);
                    tracing::warn!(
                        error = %error.message(),
                        consecutive = consecutive_errors,
                        ?backoff,
                        "could not reserve a job; retrying after a backoff"
                    );

                    // Scoped bindings are flushed here too. The turn is over
                    // either way, and anything a half-finished reserve bound
                    // has no business outliving it.
                    self.container.flush_scoped();
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            }

            self.container.flush_scoped();
        }

        Ok(stats)
    }

    async fn process(&self, job: QueuedJob) -> Result<Outcome> {
        self.dispatch(JobProcessing { job: job.clone() }).await;

        let context = Arc::new(JobContext::new(
            Arc::clone(&self.container),
            job.id.clone(),
            job.queue.clone(),
            job.attempts,
            job.max_attempts,
        ));

        let started = std::time::Instant::now();
        // The job's own limit wins.
        //
        // The worker's is a backstop for jobs that never said how long they
        // need; one that did know is the better authority, and overriding it
        // from the command line would make the declaration a decoy.
        let timeout = self.registry.timeout_for(&job.name).or(self.options.timeout);

        // Guarded, so a panic fails this job rather than the worker — see
        // [`CatchPanic`].
        let run = CatchPanic(self.registry.run(&job, Arc::clone(&context)));

        let outcome = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, run).await {
                Ok(result) => result,
                Err(_) => {
                    Ok(Err(Error::internal(format!("the job exceeded its {timeout:?} timeout"))))
                }
            },
            None => run.await,
        };

        let outcome = match outcome {
            Ok(result) => result,
            Err(panic) => {
                Err(Error::internal(format!("the job panicked: {}", panic_message(panic.as_ref()))))
            }
        };

        match outcome {
            Ok(()) => {
                self.queue.acknowledge(&job).await?;
                self.release_uniqueness(&job).await;

                let duration = started.elapsed();
                tracing::info!(job = %job.name, id = %job.id, ?duration, "job processed");
                self.dispatch(JobProcessed { job, duration }).await;
                Ok(Outcome::Processed)
            }
            Err(error) => self.handle_failure(job, error).await,
        }
    }

    /// Release a finished job's uniqueness lock, if it held one.
    ///
    /// Deliberately **not** released when a job is merely *released for
    /// retry* — it is still pending, and a duplicate dispatched in the gap is
    /// exactly what uniqueness is meant to drop.
    ///
    /// A failure here is logged rather than propagated: the job is done, and
    /// turning "the cache blinked" into "the job failed" would re-run work that
    /// already succeeded. The lock expires on its own.
    async fn release_uniqueness(&self, job: &QueuedJob) {
        let (Some(key), Some(locks)) = (&job.unique_key, &self.locks) else { return };

        // `force_release`, because the token belongs to the process that
        // dispatched this — long gone, and it never handed it over. Nothing
        // else takes this key.
        if let Err(e) = locks.lock(key.clone(), Duration::from_secs(1)).force_release().await {
            tracing::warn!(
                job = %job.name,
                key,
                error = %e.message(),
                "could not release the uniqueness lock; it will expire on its TTL"
            );
        }
    }

    async fn handle_failure(&self, job: QueuedJob, error: Error) -> Result<Outcome> {
        let message = error.to_string();

        if job.attempts >= self.options.attempts_for(job.max_attempts) {
            self.queue.fail(&job, &message).await?;

            // Released on final failure too. Holding the lock until
            // `UNIQUE_FOR` would mean one permanent failure blocks every later
            // dispatch of that id — including the one that fixes it.
            self.release_uniqueness(&job).await;

            tracing::error!(job = %job.name, id = %job.id, error = %message, "job failed");
            self.dispatch(JobFailed { job, error: message }).await;
            return Ok(Outcome::Failed);
        }

        // `attempts` counts what has been tried, so the backoff for the *next*
        // attempt is indexed by it directly.
        let delay = backoff_for(job.attempts);
        self.queue.release(&job, delay).await?;
        tracing::warn!(
            job = %job.name,
            id = %job.id,
            attempt = job.attempts,
            of = job.max_attempts,
            ?delay,
            error = %message,
            "job failed; retrying"
        );
        self.dispatch(JobReleased { job, delay, error: message }).await;
        Ok(Outcome::Released)
    }

    /// Worker events are informational, so a failing listener is logged and
    /// otherwise ignored — a broken metrics hook must not stop the queue.
    async fn dispatch<E: Send + Sync + 'static>(&self, event: E) {
        if let Some(events) = &self.events {
            events.dispatch_quietly(event).await;
        }
    }
}

/// The retry delay after `attempts` failures: 1s, 2s, 4s, …, capped at 64s.
///
/// The per-job [`Job::backoff`](crate::Job::backoff) cannot be consulted here —
/// the worker has only the job's *name*, not its type — so this mirrors the
/// same default curve.
fn backoff_for(attempts: u32) -> Duration {
    Duration::from_secs(1u64 << attempts.clamp(1, 6))
}

/// A future that turns a panic in the inner future into an `Err` instead of
/// unwinding into the caller.
///
/// # Why the queue needs this
///
/// A panic in one job kills the worker process, and with it every other job on
/// every queue that worker drains. That is the wrong blast radius: the other
/// jobs did nothing wrong, and the panicking one is usually a configuration
/// fault that a retry or a failed-job record would surface just as loudly and
/// far more cheaply.
///
/// It is not hypothetical. Facades panic by design when their service is not
/// bound — the reasoning being that a facade whose service was never
/// registered can never work, so it should say so rather than propagate an
/// error into every caller. That reasoning holds in a request handler and
/// inverts in a queue worker, where the "saying so" is the process dying. A
/// deployment configured `MAIL_DRIVER=smtp` without the matching cargo
/// feature, and the first job to send mail put the worker into
/// CrashLoopBackOff, taking the whole default queue with it.
///
/// So a panicking job becomes a failing job: it retries on the normal
/// schedule, lands in the failed-job table when its attempts run out, and the
/// worker carries on. The panic itself is still printed by the default hook,
/// so nothing is hidden.
struct CatchPanic<F>(F);

impl<F: std::future::Future> std::future::Future for CatchPanic<F> {
    type Output = std::result::Result<F::Output, Box<dyn std::any::Any + Send>>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: a pin projection to the single field. The inner future is
        // never moved out, and `CatchPanic` is only ever polled through this.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.0) };

        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(value)) => std::task::Poll::Ready(Ok(value)),
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    }
}

/// What a panic payload says, for the failed-job record.
///
/// `panic!` with a literal gives a `&str` and with a format gives a `String`;
/// anything else is a payload type this cannot read, and saying so beats
/// inventing a message.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic with a payload this worker cannot render".to_string()
    }
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("queue", &self.queue.name())
            .field("queues", &self.options.queues)
            .field("stopping", &self.is_stopping())
            .finish()
    }
}

#[cfg(test)]
// These tests hold `SERIAL` across their awaits on purpose: the `Flaky` job
// coordinates through process-wide statics, so two of them running at once
// would see each other's configuration. Safe here because `#[tokio::test]`
// runs on a current-thread runtime, so the guard never crosses a thread.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::job::{Job, QueuedJob};
    use crate::queue::MemoryQueue;
    use rainier_support::BoxFuture;

    #[test]
    fn tries_raises_a_jobs_attempts_but_never_lowers_them() {
        // A worker expresses a default, not a ceiling. A job dispatched asking
        // for five attempts has a reason to want five, and a worker started
        // with `--tries=3` must not quietly halve it.
        let options = WorkerOptions::default().tries(3);

        assert_eq!(options.attempts_for(1), 3, "a job that asked for one gets the floor");
        assert_eq!(options.attempts_for(5), 5, "a job that asked for more keeps them");
    }

    #[test]
    fn without_tries_a_jobs_own_attempts_stand() {
        let options = WorkerOptions::default();

        assert_eq!(options.attempts_for(1), 1);
        assert_eq!(options.attempts_for(7), 7);
    }

    #[test]
    fn attempts_are_never_zero() {
        // Zero would mean a job that can be reserved and never run: exhausted
        // before its first try.
        assert_eq!(WorkerOptions::default().attempts_for(0), 1);
    }
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    /// The `Flaky` job coordinates through process-wide statics, so the tests
    /// that configure it must not interleave.
    static SERIAL: Mutex<()> = Mutex::new(());
    static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
    static SUCCEED_FROM: AtomicU32 = AtomicU32::new(1);

    #[derive(Serialize, Deserialize)]
    struct Flaky;

    #[async_trait::async_trait]
    impl Job for Flaky {
        const NAME: &'static str = "test.flaky";
        const TRIES: u32 = 3;

        async fn handle(&self, context: &JobContext) -> Result<()> {
            ATTEMPTS.fetch_add(1, Ordering::SeqCst);
            if context.attempt() >= SUCCEED_FROM.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(Error::internal("not yet"))
            }
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Slow;

    #[async_trait::async_trait]
    impl Job for Slow {
        const NAME: &'static str = "test.slow";
        const TRIES: u32 = 1;

        async fn handle(&self, _: &JobContext) -> Result<()> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }

    /// Panics, the way a facade does when its service is not bound.
    #[derive(Serialize, Deserialize)]
    struct Exploding;

    #[async_trait::async_trait]
    impl Job for Exploding {
        const NAME: &'static str = "test.exploding";
        const TRIES: u32 = 1;

        async fn handle(&self, _: &JobContext) -> Result<()> {
            panic!("the `Mail` facade could not resolve `Mailer`");
        }
    }

    /// Takes longer than the worker's limit and says so.
    #[derive(Serialize, Deserialize)]
    struct Patient;

    #[async_trait::async_trait]
    impl Job for Patient {
        const NAME: &'static str = "test.patient";
        const TRIES: u32 = 1;
        const TIMEOUT: Option<Duration> = Some(Duration::from_secs(5));

        async fn handle(&self, _: &JobContext) -> Result<()> {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        }
    }

    fn registry() -> Arc<JobRegistry> {
        Arc::new(JobRegistry::new().with::<Flaky>().with::<Slow>().with::<Exploding>())
    }

    #[tokio::test]
    async fn a_stopped_worker_stops_reserving() {
        // What a shutdown signal does. Nothing called `stop` before this, so a
        // SIGTERM did nothing: the worker kept reserving and the process sat
        // out its whole termination grace period before being killed, holding
        // work it would be interrupted in the middle of.
        let queue = Arc::new(MemoryQueue::new());
        for _ in 0..3 {
            queue.push(QueuedJob::from_job(&Slow).unwrap()).await.unwrap();
        }

        let worker = Worker::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        );

        worker.stop();

        let stats = worker.run().await.expect("the loop returns rather than hanging");

        assert_eq!(stats.total(), 0, "a stopped worker takes nothing new");
        assert_eq!(queue.size("default").await.unwrap(), 3, "the work is left for somebody");
    }

    fn worker(queue: Arc<MemoryQueue>) -> Worker {
        Worker::new(queue, registry(), Arc::new(Container::new()))
            .with_options(WorkerOptions::default().stop_when_empty())
    }

    /// Take the serial lock and configure `Flaky` for this test. The guard
    /// must be bound, or the lock is released immediately.
    fn reset(succeed_from: u32) -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        ATTEMPTS.store(0, Ordering::SeqCst);
        SUCCEED_FROM.store(succeed_from, Ordering::SeqCst);
        guard
    }

    #[tokio::test]
    async fn an_empty_queue_is_idle() {
        let queue = Arc::new(MemoryQueue::new());
        assert_eq!(worker(queue).run_next().await.unwrap(), Outcome::Idle);
    }

    #[tokio::test]
    async fn a_successful_job_is_acknowledged() {
        let _serial = reset(1);
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();

        assert_eq!(worker(Arc::clone(&queue)).run_next().await.unwrap(), Outcome::Processed);
        assert_eq!(queue.size("default").await.unwrap(), 0);
        assert_eq!(queue.reserved_count(), 0);
        assert!(queue.failed_jobs().is_empty());
    }

    #[tokio::test]
    async fn a_failing_job_is_released_for_another_attempt() {
        let _serial = reset(99); // never succeeds
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();

        assert_eq!(worker(Arc::clone(&queue)).run_next().await.unwrap(), Outcome::Released);
        assert_eq!(queue.size("default").await.unwrap(), 1, "it is back in the queue");
        assert!(queue.failed_jobs().is_empty(), "not failed yet — it has attempts left");
    }

    #[tokio::test]
    async fn a_job_that_exhausts_its_attempts_fails() {
        let _serial = reset(99);
        let queue = Arc::new(MemoryQueue::new());
        let mut job = QueuedJob::from_job(&Flaky).unwrap();
        job.attempts = job.max_attempts - 1; // the next reserve makes it the last

        queue.push(job).await.unwrap();
        assert_eq!(worker(Arc::clone(&queue)).run_next().await.unwrap(), Outcome::Failed);

        let failed = queue.failed_jobs();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].error.contains("not yet"), "{}", failed[0].error);
        assert_eq!(queue.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_job_that_succeeds_on_retry_is_only_run_until_it_does() {
        let _serial = reset(2); // fails once, then succeeds
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();

        // Zero backoff so the retry is immediately available.
        let worker = Worker::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        .with_options(WorkerOptions::default().stop_when_empty());

        assert_eq!(worker.run_next().await.unwrap(), Outcome::Released);
        // The release carries a backoff, so it is not available yet.
        assert_eq!(worker.run_next().await.unwrap(), Outcome::Idle);
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_run_drains_the_queue_and_reports_what_it_did() {
        let _serial = reset(1);
        let queue = Arc::new(MemoryQueue::new());
        for _ in 0..3 {
            queue.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();
        }

        let stats = worker(Arc::clone(&queue)).run().await.unwrap();
        assert_eq!(stats.processed, 3);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.total(), 3);
    }

    #[tokio::test]
    async fn a_worker_stops_after_its_job_limit() {
        let _serial = reset(1);
        let queue = Arc::new(MemoryQueue::new());
        for _ in 0..5 {
            queue.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();
        }

        let worker = Worker::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        .with_options(WorkerOptions::default().stop_when_empty().max_jobs(2));

        assert_eq!(worker.run().await.unwrap().processed, 2);
        assert_eq!(queue.size("default").await.unwrap(), 3, "the rest are still waiting");
    }

    #[tokio::test]
    async fn queues_are_served_in_priority_order() {
        let _serial = reset(1);
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Flaky).unwrap().on_queue("low")).await.unwrap();
        queue.push(QueuedJob::from_job(&Flaky).unwrap().on_queue("high")).await.unwrap();

        let worker = Worker::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        .with_options(WorkerOptions::default().queues(["high", "low"]).stop_when_empty());

        worker.run_next().await.unwrap();
        assert_eq!(queue.size("high").await.unwrap(), 0, "the high queue drains first");
        assert_eq!(queue.size("low").await.unwrap(), 1);
    }

    /// A job that states its own limit is not held to the worker's.
    ///
    /// The worker's is the fallback for jobs that never said how long they
    /// need. Letting a command-line flag overrule a declaration would make
    /// the declaration a decoy — which is how a transcode ladder whose honest
    /// duration is minutes got killed at the web default of sixty seconds.
    #[tokio::test]
    async fn a_jobs_own_timeout_wins_over_the_workers() {
        let _serial = reset(1);
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Patient).unwrap()).await.unwrap();

        let worker = Worker::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            Arc::new(JobRegistry::new().with::<Patient>()),
            Arc::new(Container::new()),
        )
        // Far below what `Patient` takes. `Patient` declares its own, so this
        // must not be what applies.
        .with_options(
            WorkerOptions::default().stop_when_empty().timeout(Some(Duration::from_millis(1))),
        );

        let stats = worker.run().await.unwrap();

        assert_eq!(stats.failed, 0, "the job's own timeout should have applied");
        assert_eq!(stats.processed, 1);
    }

    #[tokio::test]
    async fn a_job_that_overruns_its_timeout_is_abandoned() {
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Slow).unwrap()).await.unwrap();

        let worker = Worker::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        .with_options(
            WorkerOptions::default().stop_when_empty().timeout(Some(Duration::from_millis(50))),
        );

        // `Slow` gets one attempt, so a timeout is terminal.
        assert_eq!(worker.run_next().await.unwrap(), Outcome::Failed);
        assert!(queue.failed_jobs()[0].error.contains("timeout"));
    }

    /// A panicking job used to take the process with it, and every other job
    /// on every queue that worker served. Facades panic when their service is
    /// not bound, so one misconfigured driver was enough.
    #[tokio::test]
    async fn a_panicking_job_fails_rather_than_killing_the_worker() {
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Exploding).unwrap()).await.unwrap();

        let worker = Worker::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        .with_options(WorkerOptions::default().stop_when_empty());

        // The worker is still here to answer, which is the point.
        assert_eq!(worker.run_next().await.unwrap(), Outcome::Failed);

        let failed = queue.failed_jobs();
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0].error.contains("panicked"),
            "the record says what happened: {}",
            failed[0].error
        );
        assert!(
            failed[0].error.contains("could not resolve"),
            "and carries the panic's own message: {}",
            failed[0].error
        );
    }

    #[tokio::test]
    async fn an_unknown_job_fails_rather_than_stalling_the_worker() {
        let queue = Arc::new(MemoryQueue::new());
        let mut job = QueuedJob::from_job(&Flaky).unwrap();
        job.name = "never.registered".into();
        job.max_attempts = 1;
        queue.push(job).await.unwrap();

        assert_eq!(worker(Arc::clone(&queue)).run_next().await.unwrap(), Outcome::Failed);
        assert!(queue.failed_jobs()[0].error.contains("never.registered"));
    }

    #[tokio::test]
    async fn lifecycle_events_are_fired() {
        let _serial = reset(1);
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();

        let events = Arc::new(Dispatcher::new());
        let log = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&log);
        events.listen(move |_: Arc<JobProcessing>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push("processing");
                Ok(())
            }
        });
        let sink = Arc::clone(&log);
        events.listen(move |_: Arc<JobProcessed>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push("processed");
                Ok(())
            }
        });

        worker(Arc::clone(&queue)).with_events(events).run_next().await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["processing", "processed"]);
    }

    #[tokio::test]
    async fn a_failing_event_listener_does_not_stop_the_queue() {
        let _serial = reset(1);
        let queue = Arc::new(MemoryQueue::new());
        queue.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();

        let events = Arc::new(Dispatcher::new());
        events.listen(|_: Arc<JobProcessed>| async { Err(Error::internal("metrics down")) });

        let outcome = worker(Arc::clone(&queue)).with_events(events).run_next().await.unwrap();
        assert_eq!(outcome, Outcome::Processed, "the job still succeeded");
    }

    #[tokio::test]
    async fn a_stopping_worker_leaves_the_loop() {
        let queue = Arc::new(MemoryQueue::new());
        let worker = worker(queue);
        worker.stop();

        assert!(worker.is_stopping());
        assert_eq!(worker.run().await.unwrap(), WorkerStats::default());
    }

    #[test]
    fn the_backoff_curve_grows_and_levels_off() {
        assert_eq!(backoff_for(1), Duration::from_secs(2));
        assert_eq!(backoff_for(2), Duration::from_secs(4));
        assert_eq!(backoff_for(6), Duration::from_secs(64));
        assert_eq!(backoff_for(50), Duration::from_secs(64), "capped");
    }

    // --- a broker that fails ------------------------------------------------

    /// A queue whose `reserve` fails the first `failures` times it is called
    /// and behaves normally afterwards — a broker failing over, in other
    /// words. Everything else delegates, so a job that does get reserved still
    /// runs through the real code.
    struct FlakyBroker {
        inner: MemoryQueue,
        remaining_failures: AtomicU32,
        reserve_calls: AtomicU32,
    }

    impl FlakyBroker {
        fn new(failures: u32) -> Self {
            Self {
                inner: MemoryQueue::new(),
                remaining_failures: AtomicU32::new(failures),
                reserve_calls: AtomicU32::new(0),
            }
        }

        fn reserve_calls(&self) -> u32 {
            self.reserve_calls.load(Ordering::SeqCst)
        }
    }

    impl Queue for FlakyBroker {
        fn name(&self) -> &str {
            "flaky-broker"
        }

        fn push<'q>(&'q self, job: QueuedJob) -> BoxFuture<'q, Result<String>> {
            self.inner.push(job)
        }

        fn reserve<'q>(&'q self, queue: &'q str) -> BoxFuture<'q, Result<Option<QueuedJob>>> {
            self.reserve_calls.fetch_add(1, Ordering::SeqCst);

            // The shape of the real one: a cluster shard with no master, which
            // refuses every command for its slots and does not recover on its
            // own.
            if self
                .remaining_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Box::pin(async {
                    Err(Error::service_unavailable(
                        "Redis: slot 11221 has no reachable node: its master is gone",
                    ))
                });
            }

            self.inner.reserve(queue)
        }

        fn acknowledge<'q>(&'q self, job: &'q QueuedJob) -> BoxFuture<'q, Result<()>> {
            self.inner.acknowledge(job)
        }

        fn release<'q>(&'q self, job: &'q QueuedJob, delay: Duration) -> BoxFuture<'q, Result<()>> {
            self.inner.release(job, delay)
        }

        fn fail<'q>(&'q self, job: &'q QueuedJob, error: &'q str) -> BoxFuture<'q, Result<()>> {
            self.inner.fail(job, error)
        }

        fn size<'q>(&'q self, queue: &'q str) -> BoxFuture<'q, Result<u64>> {
            self.inner.size(queue)
        }

        fn clear<'q>(&'q self, queue: &'q str) -> BoxFuture<'q, Result<u64>> {
            self.inner.clear(queue)
        }
    }

    /// Options that retry a broker briskly, so the tests do not sleep.
    fn brisk(max_consecutive: u32) -> WorkerOptions {
        WorkerOptions::default()
            .sleep(Duration::from_millis(1))
            .max_error_backoff(Duration::from_millis(2))
            .max_consecutive_errors(max_consecutive)
    }

    #[tokio::test]
    async fn a_broker_that_fails_and_recovers_does_not_end_the_run() {
        // The production failure this exists for. `reserve` used to propagate,
        // which ended the process; under an orchestrator that is a restart
        // into the same outage, and a queue whose workers are all in
        // CrashLoopBackOff is not being drained by anybody.
        let _serial = reset(1);
        let broker = Arc::new(FlakyBroker::new(3));
        broker.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();

        let worker = Worker::new(
            Arc::clone(&broker) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        .with_options(brisk(20).stop_when_empty());

        let stats = worker.run().await.expect("the run survives the outage");

        assert_eq!(stats.errors, 3, "each refusal is counted");
        assert_eq!(stats.processed, 1, "and the job still runs once the broker is back");
        assert_eq!(broker.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_broker_that_never_recovers_ends_the_run_and_says_why() {
        // The other half. Riding out an outage must not become pretending to
        // work forever: a worker pointed at a broker that is genuinely gone
        // should exit non-zero, so a supervisor and an operator both notice.
        let broker = Arc::new(FlakyBroker::new(u32::MAX));

        let worker = Worker::new(
            Arc::clone(&broker) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        .with_options(brisk(4));

        let error = worker.run().await.expect_err("it gives up rather than looping forever");

        assert!(error.message().contains("slot 11221"), "{}", error.message());
        assert_eq!(broker.reserve_calls(), 4, "it stops at the allowance, not before or after");
    }

    #[tokio::test]
    async fn a_success_resets_the_allowance() {
        // Consecutive, not cumulative. A worker alive for days accumulates
        // unrelated blips, and a budget it slowly spent would eventually make
        // it exit for no reason at all.
        let _serial = reset(1);
        let broker = Arc::new(FlakyBroker::new(2));
        broker.push(QueuedJob::from_job(&Flaky).unwrap()).await.unwrap();

        let worker = Worker::new(
            Arc::clone(&broker) as Arc<dyn Queue>,
            registry(),
            Arc::new(Container::new()),
        )
        // Three would not survive a cumulative count: two failures,
        // then a success, then the idle turn that drains the queue.
        .with_options(brisk(3).stop_when_empty());

        let stats = worker.run().await.expect("two failures then a success is not three failures");

        assert_eq!(stats.errors, 2);
        assert_eq!(stats.processed, 1);
    }

    #[tokio::test]
    async fn a_stop_is_honoured_while_backing_off_from_a_broker() {
        // A worker waiting out an outage must still answer SIGTERM, or a
        // rollout during one sits out the whole termination grace period.
        let broker = Arc::new(FlakyBroker::new(u32::MAX));

        let worker = Arc::new(
            Worker::new(
                Arc::clone(&broker) as Arc<dyn Queue>,
                registry(),
                Arc::new(Container::new()),
            )
            .with_options(
                WorkerOptions::default()
                    .sleep(Duration::from_millis(20))
                    .max_error_backoff(Duration::from_millis(20))
                    .max_consecutive_errors(1_000),
            ),
        );

        let signalled = Arc::clone(&worker);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            signalled.stop();
        });

        let stats = tokio::time::timeout(Duration::from_secs(5), worker.run())
            .await
            .expect("the loop notices the stop rather than backing off forever")
            .expect("stopping is not a failure");

        assert!(stats.errors > 0, "it was in the middle of an outage");
    }

    #[test]
    fn the_broker_backoff_doubles_from_sleep_and_is_capped() {
        let options = WorkerOptions::default()
            .sleep(Duration::from_secs(1))
            .max_error_backoff(Duration::from_secs(30));

        assert_eq!(options.error_backoff(1), Duration::from_secs(1), "the first wait is `sleep`");
        assert_eq!(options.error_backoff(2), Duration::from_secs(2));
        assert_eq!(options.error_backoff(5), Duration::from_secs(16));
        assert_eq!(options.error_backoff(6), Duration::from_secs(30), "capped");
        assert_eq!(options.error_backoff(u32::MAX), Duration::from_secs(30), "never overflows");
    }

    #[test]
    fn broker_errors_are_not_job_outcomes() {
        // `total()` drives `--max-jobs`, and a broker outage must not spend a
        // worker's job budget without a job ever having run.
        let stats = WorkerStats { errors: 500, ..WorkerStats::default() };

        assert_eq!(stats.total(), 0);
    }
}
