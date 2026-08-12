//! Dispatching — the [`QueueManager`] facade accessor and the [`SyncQueue`]
//! driver.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rainier_cache::{LockGuard, LockManager};
use rainier_container::Container;
use rainier_support::{BoxFuture, Error, Result};

use crate::job::{Job, JobContext, JobRegistry, QueuedJob};
use crate::queue::Queue;

/// A queue that runs jobs **immediately**, on the thread that dispatched them.
///
/// Not really a queue: nothing is stored and nothing is deferred. It exists so
/// an application can be developed and tested without a worker process, which
/// is what `QUEUE_DRIVER=sync` selects. A job dispatched
/// this way fails the dispatching request if it fails, which is exactly the
/// behaviour queueing exists to avoid — so never use it in production.
pub struct SyncQueue {
    registry: Arc<JobRegistry>,
    container: Arc<Container>,
}

impl SyncQueue {
    /// Run jobs inline, resolving their dependencies from `container`.
    pub fn new(registry: Arc<JobRegistry>, container: Arc<Container>) -> Self {
        Self { registry, container }
    }
}

impl Queue for SyncQueue {
    fn name(&self) -> &str {
        "sync"
    }

    fn push<'a>(&'a self, mut job: QueuedJob) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            job.attempts = 1;
            let id = job.id.clone();

            let context = Arc::new(JobContext::new(
                Arc::clone(&self.container),
                job.id.clone(),
                job.queue.clone(),
                // One attempt: there is no queue to retry from, so it is also
                // the last, and the job's `failed` hook should run.
                1,
                1,
            ));

            self.registry.run(&job, context).await?;
            Ok(id)
        })
    }

    fn reserve<'a>(&'a self, _queue: &'a str) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        // Nothing is ever waiting: everything ran on push.
        Box::pin(async { Ok(None) })
    }

    fn acknowledge<'a>(&'a self, _job: &'a QueuedJob) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn release<'a>(&'a self, _job: &'a QueuedJob, _delay: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn fail<'a>(&'a self, _job: &'a QueuedJob, _error: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn size<'a>(&'a self, _queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn clear<'a>(&'a self, _queue: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async { Ok(0) })
    }
}

/// A job on its way to a queue, with the dispatch options applied.
///
/// The builder behind `pending(job)?.on_queue("mail").delay(…).send()`.
pub struct PendingDispatch<'a> {
    manager: &'a QueueManager,
    job: QueuedJob,
    /// Which declared connection to push to, or `None` for the default.
    ///
    /// A name rather than a resolved queue, because resolution belongs at the
    /// moment of sending: a builder that names a connection that is not
    /// declared must fail the *dispatch*, loudly, rather than be constructible
    /// and then quietly land somewhere.
    connection: Option<String>,
    /// The uniqueness key and its TTL, from [`Job::unique_id`].
    ///
    /// Captured here rather than claimed in `pending`, because the claim
    /// belongs at the moment of sending — a builder that is configured and then
    /// dropped must not leave a lock behind.
    unique: Option<(String, Duration)>,
}

impl<'a> PendingDispatch<'a> {
    /// Put the job on a named queue instead of its default.
    pub fn on_queue(mut self, queue: impl Into<String>) -> Self {
        self.job = self.job.on_queue(queue);
        self
    }

    /// Push the job to a named connection instead of the default one.
    ///
    /// Orthogonal to [`on_queue`](Self::on_queue), and the two answer different
    /// questions: a connection is **which backend** the job is stored in — a
    /// database, an SQS queue, a Kafka cluster — and a queue is **which named
    /// lane within it**. `on_connection("bulk").on_queue("mail")` is the `mail`
    /// queue of the `bulk` backend, and neither setting implies the other.
    ///
    /// The name must be declared on the manager (see
    /// [`Connections`](crate::Connections) or
    /// [`with_connection`](QueueManager::with_connection)). One that is not
    /// fails at [`send`](Self::send) rather than falling back to the default,
    /// because a fallback here is invisible: the push succeeds, an id comes
    /// back, and the job waits in a backend whose worker is not the one anybody
    /// is watching.
    pub fn on_connection(mut self, connection: impl Into<String>) -> Self {
        self.connection = Some(connection.into());
        self
    }

    /// Hold the job back for `delay`.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.job = self.job.delayed_by(delay);
        self
    }

    /// Override how many attempts the job gets.
    pub fn tries(mut self, tries: u32) -> Self {
        self.job.max_attempts = tries.max(1);
        self
    }

    /// Send it.
    ///
    /// `Ok(Some(id))` when it was queued. `Ok(None)` when the job declares a
    /// [`unique_id`](Job::unique_id) and an identical one is already pending —
    /// dropping it is the point, so it is not an error.
    ///
    /// # Errors
    ///
    /// When [`on_connection`](Self::on_connection) named a connection that is
    /// not declared, or when the backend refuses the push.
    pub async fn send(self) -> Result<Option<String>> {
        let mut job = self.job;

        // Resolved before the uniqueness claim rather than after, because the
        // claim deliberately outlives this call: taking one and *then* failing
        // to resolve would leave a lock nothing will ever release, and every
        // later dispatch of that job would be dropped as a duplicate until its
        // TTL lapsed.
        let destination = self.manager.resolve(self.connection.as_deref())?;

        let claim = match &self.unique {
            Some((key, ttl)) => {
                let claim = self.manager.claim(key, *ttl).await?;
                if matches!(claim, Claim::AlreadyQueued) {
                    tracing::debug!(key, "dropping a duplicate dispatch");
                    return Ok(None);
                }
                // Carried on the job so the worker can release it when the job
                // finishes — which is what makes the guarantee "one pending or
                // running" rather than "one every `UNIQUE_FOR`".
                if matches!(claim, Claim::Held(_)) {
                    job.unique_key = Some(key.clone());
                }
                claim
            }
            None => Claim::NotEnforced,
        };

        let id = self.manager.push_to(destination, job).await?;

        // The claim outlives this call on purpose. Releasing it here would
        // deduplicate nothing: the point is that it stands while the job is
        // queued.
        if let Claim::Held(guard) = claim {
            guard.keep();
        }

        Ok(Some(id))
    }
}

/// What happened when a uniqueness lock was asked for.
enum Claim {
    /// Taken. Must outlive the dispatch — see `PendingDispatch::send`.
    Held(LockGuard),
    /// An identical job is already pending or running.
    AlreadyQueued,
    /// The job wants uniqueness but no lock manager is wired, or it never
    /// wanted any.
    NotEnforced,
}

/// Dispatches jobs onto the configured queue.
///
/// The accessor behind the `Queue` facade.
///
/// One default connection, plus any number of named ones — the queue equivalent
/// of a filesystem `Storage` holding a default disk and a map of named ones, and
/// assembled the same way, from a [`Connections`](crate::Connections)
/// declaration.
pub struct QueueManager {
    queue: Arc<dyn Queue>,
    /// The connections a dispatch may name, by the name it names them with.
    ///
    /// A `BTreeMap` so an error that lists the declared connections reads the
    /// same each run. The default connection is in here too when it was
    /// declared — see [`Connections::build`](crate::Connections::build) — and
    /// is the *same* `Arc`, not a second backend built from the same
    /// declaration.
    connections: BTreeMap<String, Arc<dyn Queue>>,
    registry: Arc<JobRegistry>,
    /// `Some` while faking: jobs are recorded and never enqueued.
    recorded: Option<Mutex<Vec<QueuedJob>>>,
    /// Where [`Job::unique_id`] locks are taken, if uniqueness is wanted.
    ///
    /// `None` means a job declaring a `unique_id` is dispatched anyway rather
    /// than silently deduplicated by nothing — see
    /// [`with_locks`](QueueManager::with_locks).
    locks: Option<LockManager>,
    /// Which queues `queue:work` drains when nobody passes `--queue`.
    ///
    /// An application that puts its jobs on named queues has to say so
    /// somewhere, and the alternative to saying it here is saying it on every
    /// worker's command line — in a Dockerfile, in a chart, in a systemd unit
    /// and in whatever an operator types at three in the morning. Those drift,
    /// and the failure when they do is silent: a worker starts, drains a queue
    /// nothing is dispatched to, reports itself healthy and processes nothing.
    default_queues: Vec<String>,
}

impl QueueManager {
    /// Dispatch onto `queue`.
    ///
    /// No connection is named: `queue` is the default, and a dispatch that does
    /// not ask for one goes there. Names come from
    /// [`with_connection`](Self::with_connection).
    pub fn new(queue: Arc<dyn Queue>, registry: Arc<JobRegistry>) -> Self {
        Self {
            queue,
            connections: BTreeMap::new(),
            registry,
            recorded: None,
            locks: None,
            // The queue `Job::QUEUE` defaults to, so an application that has
            // never thought about queue names behaves exactly as before.
            default_queues: vec!["default".to_string()],
        }
    }

    /// Drain these queues when `queue:work` is given no `--queue`.
    ///
    /// In priority order, like the flag: earlier queues are emptied first,
    /// which is how a `high` queue gets ahead of `default`.
    ///
    /// State it once, here, rather than on every worker's command line. The
    /// two are not equivalent — a flag that is right in the Dockerfile and
    /// missing from the chart produces a worker that starts cleanly, drains a
    /// queue nothing is dispatched to, and reports itself healthy while
    /// processing nothing.
    ///
    /// An empty list is ignored rather than obeyed: a worker draining no
    /// queues at all is never what was meant, and the failure it produces is
    /// the same silent one.
    pub fn with_default_queues(
        mut self,
        queues: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let queues: Vec<String> = queues
            .into_iter()
            .map(Into::into)
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();

        if !queues.is_empty() {
            self.default_queues = queues;
        }
        self
    }

    /// The queues a worker drains when it is told nothing else.
    pub fn default_queues(&self) -> &[String] {
        &self.default_queues
    }

    /// Declare a connection reachable as `name`.
    ///
    /// The default connection is registered here too when it is declared, under
    /// its own name and as the **same** backend — one built twice would give
    /// `connection("primary")` a different store from the default even though
    /// both name one declaration.
    pub fn with_connection(mut self, name: impl Into<String>, queue: Arc<dyn Queue>) -> Self {
        self.connections.insert(name.into(), queue);
        self
    }

    /// The connection declared as `name`, or `None`.
    ///
    /// `None` rather than the default, which is the whole point: Laravel's
    /// `Queue::connection('sqs')` on a name nobody declared is a job accepted
    /// into a backend no worker drains, and that raises nothing, retries
    /// nothing and leaves no failed row. An `Option` makes the caller say what
    /// happens instead.
    pub fn connection(&self, name: &str) -> Option<&Arc<dyn Queue>> {
        self.connections.get(name)
    }

    /// Whether `name` is declared.
    pub fn has_connection(&self, name: &str) -> bool {
        self.connections.contains_key(name)
    }

    /// Every declared connection name, in a stable order.
    pub fn connection_names(&self) -> impl Iterator<Item = &str> {
        self.connections.keys().map(String::as_str)
    }

    /// The backend a dispatch naming `connection` goes to.
    ///
    /// `None` means the default. A name that is not declared is an error and
    /// never the default — see [`connection`](Self::connection).
    fn resolve(&self, connection: Option<&str>) -> Result<&Arc<dyn Queue>> {
        let Some(name) = connection else {
            return Ok(&self.queue);
        };

        self.connection(name).ok_or_else(|| {
            Error::internal(format!(
                "no queue connection named `{name}` is declared; declared connections are {}. \
                 Dispatching to the default instead would store the job in a backend nobody \
                 drains, which is accepted, never run, and reported nowhere",
                self.declared_connections()
            ))
        })
    }

    /// The declared names, backtick-quoted, for an error message.
    fn declared_connections(&self) -> String {
        if self.connections.is_empty() {
            let hint = if self.is_faking() {
                "none — this manager is faking, and a fake declares no connections until one is \
                 added with `QueueManager::fake().with_connection(…)`"
            } else {
                "none"
            };
            return hint.to_string();
        }
        self.connection_names().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
    }

    /// Enforce [`Job::unique_id`] with locks taken from `locks`.
    ///
    /// Without this a job that declares a `unique_id` is dispatched normally.
    /// That is the deliberate choice: quietly not deduplicating is a bug you
    /// find in production, and quietly deduplicating against a per-process
    /// cache is the same bug wearing a hat. Wiring it is one line, and
    /// `Rainier::boot` does it.
    ///
    /// A warning is logged the first time a unique job is dispatched without
    /// one, so the gap is visible rather than silent.
    pub fn with_locks(mut self, locks: LockManager) -> Self {
        self.locks = Some(locks);
        self
    }

    /// A manager that **records** dispatches instead of performing them.
    ///
    /// The test-double form of `Queue::fake()`: a test can assert that an
    /// action queued the right job without a worker ever running it.
    pub fn fake() -> Self {
        Self {
            queue: Arc::new(SyncQueue::new(
                Arc::new(JobRegistry::new()),
                Arc::new(Container::new()),
            )),
            connections: BTreeMap::new(),
            registry: Arc::new(JobRegistry::new()),
            default_queues: vec!["default".to_string()],
            recorded: Some(Mutex::new(Vec::new())),
            locks: None,
        }
    }

    /// Whether this manager is recording instead of dispatching.
    pub fn is_faking(&self) -> bool {
        self.recorded.is_some()
    }

    /// The underlying queue.
    pub fn queue(&self) -> &Arc<dyn Queue> {
        &self.queue
    }

    /// The job registry.
    pub fn registry(&self) -> &Arc<JobRegistry> {
        &self.registry
    }

    /// Queue `job` with its declared defaults.
    pub async fn dispatch<J: Job>(&self, job: J) -> Result<Option<String>> {
        self.pending(job)?.send().await
    }

    /// The uniqueness key for `job`, if it wants one and this manager can
    /// enforce it.
    fn unique_key<J: Job>(job: &J) -> Option<(String, Duration)> {
        // `NAME` in the key, so two job types returning the same id — `"7"` for
        // an invoice and `"7"` for a user — are not the same lock.
        job.unique_id().map(|id| (format!("queue:unique:{}:{id}", J::NAME), J::UNIQUE_FOR))
    }

    /// Take a uniqueness lock.
    async fn claim(&self, key: &str, ttl: Duration) -> Result<Claim> {
        let Some(locks) = &self.locks else {
            // Louder than a debug line: the application asked for uniqueness
            // and is not getting it.
            tracing::warn!(
                key,
                "a job declares `unique_id` but the queue has no lock manager;                  dispatching without deduplicating"
            );
            return Ok(Claim::NotEnforced);
        };

        Ok(match locks.lock(key, ttl).acquire().await? {
            Some(guard) => Claim::Held(guard),
            None => Claim::AlreadyQueued,
        })
    }

    /// Release the uniqueness lock for a job that has finished.
    ///
    /// Called by the worker. Releasing on completion rather than on dispatch is
    /// what makes the guarantee "one **pending or running** at a time" rather
    /// than "one every `UNIQUE_FOR`".
    ///
    /// A `force_release` rather than a token-checked one, and this is the one
    /// place that is right: the acquirer was the *dispatching* process, which
    /// is long gone and never handed its token over. Nothing else takes this
    /// key — dispatch is its only other user — and the job it belonged to is
    /// over.
    pub async fn release_uniqueness(&self, key: &str) -> Result<bool> {
        let Some(locks) = &self.locks else { return Ok(false) };

        locks.lock(key.to_string(), Duration::from_secs(1)).force_release().await
    }

    /// Start a dispatch that can be configured before sending.
    ///
    /// ```ignore
    /// queue.pending(SendInvoice { id })?.on_queue("billing").delay(TEN_MINUTES).send().await?;
    /// ```
    pub fn pending<J: Job>(&self, job: J) -> Result<PendingDispatch<'_>> {
        let unique = Self::unique_key(&job);
        Ok(PendingDispatch {
            manager: self,
            job: QueuedJob::from_job(&job)?,
            connection: None,
            unique,
        })
    }

    /// Queue `job` on a named queue.
    pub async fn dispatch_on<J: Job>(
        &self,
        queue: impl Into<String>,
        job: J,
    ) -> Result<Option<String>> {
        self.pending(job)?.on_queue(queue).send().await
    }

    /// Queue `job`, held back for `delay`.
    pub async fn dispatch_after<J: Job>(&self, delay: Duration, job: J) -> Result<Option<String>> {
        self.pending(job)?.delay(delay).send().await
    }

    /// Run `job` right now instead of queueing it.
    ///
    /// Bypasses the queue entirely, so failures surface to the caller. For a
    /// job the caller genuinely needs to have finished.
    pub async fn dispatch_now<J: Job>(&self, job: J, container: Arc<Container>) -> Result<()> {
        let queued = QueuedJob::from_job(&job)?;
        let context =
            Arc::new(JobContext::new(container, queued.id.clone(), queued.queue.clone(), 1, 1));
        self.registry.run(&queued, context).await
    }

    /// Push `job` to an already-resolved backend.
    ///
    /// Resolution happens in [`resolve`](Self::resolve), *before* this, and
    /// applies while faking too: a fake that accepted a connection name the
    /// real manager would reject is a test that passes and a production
    /// dispatch that lands nowhere, which is the failure the name check exists
    /// for.
    async fn push_to(&self, queue: &Arc<dyn Queue>, job: QueuedJob) -> Result<String> {
        if let Some(recorded) = &self.recorded {
            let id = job.id.clone();
            recorded.lock().expect("recorder lock poisoned").push(job);
            return Ok(id);
        }
        queue.push(job).await
    }

    // --- assertions (faking) -----------------------------------------------

    /// Every recorded dispatch of `J`. Always empty unless faking.
    pub fn pushed<J: Job>(&self) -> Vec<QueuedJob> {
        let Some(recorded) = &self.recorded else {
            return Vec::new();
        };
        recorded
            .lock()
            .expect("recorder lock poisoned")
            .iter()
            .filter(|job| job.name == J::NAME)
            .cloned()
            .collect()
    }

    /// Every recorded dispatch, of any type.
    pub fn all_pushed(&self) -> Vec<QueuedJob> {
        match &self.recorded {
            Some(recorded) => recorded.lock().expect("recorder lock poisoned").clone(),
            None => Vec::new(),
        }
    }

    /// Panic unless a `J` was dispatched.
    ///
    /// # Panics
    ///
    /// If none was, or the manager is not faking — which would otherwise make
    /// every assertion pass vacuously.
    pub fn assert_pushed<J: Job>(&self) {
        self.require_faking("assert_pushed");
        assert!(
            !self.pushed::<J>().is_empty(),
            "expected a `{}` job to have been queued. Queued: {:?}",
            J::NAME,
            self.all_pushed().iter().map(|job| job.name.clone()).collect::<Vec<_>>()
        );
    }

    /// Panic unless exactly `times` `J` jobs were dispatched.
    ///
    /// # Panics
    ///
    /// If the count differs, or the manager is not faking.
    pub fn assert_pushed_times<J: Job>(&self, times: usize) {
        self.require_faking("assert_pushed_times");
        let actual = self.pushed::<J>().len();
        assert_eq!(
            actual,
            times,
            "expected `{}` to be queued {times} time(s), but it was queued {actual}",
            J::NAME
        );
    }

    /// Panic if any `J` was dispatched.
    ///
    /// # Panics
    ///
    /// If one was, or the manager is not faking.
    pub fn assert_not_pushed<J: Job>(&self) {
        self.require_faking("assert_not_pushed");
        assert!(
            self.pushed::<J>().is_empty(),
            "expected no `{}` job to be queued, but one was",
            J::NAME
        );
    }

    /// Panic unless a `J` was dispatched onto `queue`.
    ///
    /// # Panics
    ///
    /// If none was, or the manager is not faking.
    pub fn assert_pushed_on<J: Job>(&self, queue: &str) {
        self.require_faking("assert_pushed_on");
        let queues: Vec<String> = self.pushed::<J>().iter().map(|job| job.queue.clone()).collect();
        assert!(
            queues.iter().any(|q| q == queue),
            "expected `{}` to be queued on `{queue}`, but it was queued on {queues:?}",
            J::NAME
        );
    }

    fn require_faking(&self, method: &str) {
        assert!(
            self.is_faking(),
            "`{method}` needs a faking manager — build it with `QueueManager::fake()`, \
             otherwise nothing is recorded and the assertion is meaningless"
        );
    }
}

impl std::fmt::Debug for QueueManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueManager")
            .field("driver", &self.queue.name())
            .field("connections", &self.connection_names().collect::<Vec<_>>())
            .field("jobs", &self.registry.names())
            .field("faking", &self.is_faking())
            .finish()
    }
}

/// Not `Sync` by default because of the `Mutex`? It is — `Mutex<Vec<..>>` is
/// `Sync` when its contents are. Asserted so a future field cannot silently
/// break the facade, which requires `Send + Sync`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<QueueManager>();
};

#[cfg(test)]
mod unique_tests {
    use super::*;
    use crate::queue::MemoryQueue;
    use rainier_cache::MemoryCache;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct RebuildIndex;

    #[async_trait::async_trait]
    impl Job for RebuildIndex {
        const NAME: &'static str = "search.rebuild";
        // One rebuild at a time, whoever asks.
        fn unique_id(&self) -> Option<String> {
            Some(String::new())
        }
        async fn handle(&self, _: &JobContext) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct SendInvoice {
        id: u64,
    }

    #[async_trait::async_trait]
    impl Job for SendInvoice {
        const NAME: &'static str = "billing.invoice";
        // One per invoice — two different invoices are two jobs.
        fn unique_id(&self) -> Option<String> {
            Some(self.id.to_string())
        }
        async fn handle(&self, _: &JobContext) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Ordinary;

    #[async_trait::async_trait]
    impl Job for Ordinary {
        const NAME: &'static str = "test.ordinary";
        async fn handle(&self, _: &JobContext) -> Result<()> {
            Ok(())
        }
    }

    fn manager() -> (QueueManager, Arc<MemoryQueue>) {
        let queue = Arc::new(MemoryQueue::new());
        let manager = QueueManager::new(Arc::clone(&queue) as Arc<_>, Arc::new(JobRegistry::new()))
            .with_locks(rainier_cache::LockManager::new(Arc::new(MemoryCache::new())));
        (manager, queue)
    }

    #[tokio::test]
    async fn a_second_identical_dispatch_is_dropped() {
        let (manager, queue) = manager();

        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_some());
        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_none());

        assert_eq!(queue.size("default").await.unwrap(), 1, "one copy, not two");
    }

    #[tokio::test]
    async fn different_ids_are_different_jobs() {
        let (manager, queue) = manager();

        assert!(manager.dispatch(SendInvoice { id: 1 }).await.unwrap().is_some());
        assert!(manager.dispatch(SendInvoice { id: 2 }).await.unwrap().is_some());
        assert!(manager.dispatch(SendInvoice { id: 1 }).await.unwrap().is_none());

        assert_eq!(queue.size("default").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn two_job_types_sharing_an_id_do_not_collide() {
        // `NAME` is in the key, so invoice 7 and index "" are separate locks —
        // and so would invoice "7" and user "7" be.
        let (manager, queue) = manager();

        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_some());
        assert!(manager.dispatch(SendInvoice { id: 0 }).await.unwrap().is_some());

        assert_eq!(queue.size("default").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn an_ordinary_job_is_never_deduplicated() {
        let (manager, queue) = manager();

        for _ in 0..3 {
            assert!(manager.dispatch(Ordinary).await.unwrap().is_some());
        }
        assert_eq!(queue.size("default").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn the_queued_job_carries_the_key_so_a_worker_can_release_it() {
        let (manager, queue) = manager();
        manager.dispatch(RebuildIndex).await.unwrap();

        let job = queue.reserve("default").await.unwrap().expect("a job");
        assert_eq!(job.unique_key.as_deref(), Some("queue:unique:search.rebuild:"));
    }

    #[tokio::test]
    async fn without_a_lock_manager_a_unique_job_is_dispatched_anyway() {
        // Degraded, and loudly — quietly deduplicating against nothing is the
        // same bug wearing a hat.
        let queue = Arc::new(MemoryQueue::new());
        let manager = QueueManager::new(Arc::clone(&queue) as Arc<_>, Arc::new(JobRegistry::new()));

        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_some());
        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_some());

        assert_eq!(queue.size("default").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn releasing_the_lock_lets_the_next_one_in() {
        let (manager, _queue) = manager();

        manager.dispatch(RebuildIndex).await.unwrap();
        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_none());

        manager.release_uniqueness("queue:unique:search.rebuild:").await.unwrap();

        assert!(
            manager.dispatch(RebuildIndex).await.unwrap().is_some(),
            "once the first has run, another may be queued"
        );
    }

    #[tokio::test]
    async fn a_configured_dispatch_is_deduplicated_too() {
        // The claim is taken at `send`, so the builder path gets it as well.
        let (manager, queue) = manager();

        assert!(manager
            .pending(RebuildIndex)
            .unwrap()
            .on_queue("high")
            .send()
            .await
            .unwrap()
            .is_some());
        assert!(manager
            .pending(RebuildIndex)
            .unwrap()
            .on_queue("high")
            .send()
            .await
            .unwrap()
            .is_none());

        assert_eq!(queue.size("high").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_dispatch_to_an_undeclared_connection_leaves_no_lock() {
        // The claim outlives a successful dispatch on purpose, so one taken
        // before a *failed* resolution would never be released — and every
        // later dispatch of that job would be dropped as a duplicate until the
        // TTL lapsed. Resolution therefore comes first.
        let (manager, queue) = manager();

        assert!(manager
            .pending(RebuildIndex)
            .unwrap()
            .on_connection("nowhere")
            .send()
            .await
            .is_err());

        assert!(
            manager.dispatch(RebuildIndex).await.unwrap().is_some(),
            "the failed dispatch must not have claimed the uniqueness key"
        );
        assert_eq!(queue.size("default").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_unique_job_is_deduplicated_across_connections() {
        // The key is the job's name and id, not the backend — two copies of one
        // rebuild are two rebuilds however they were routed.
        let queue = Arc::new(MemoryQueue::new());
        let bulk = Arc::new(MemoryQueue::new());
        let manager = QueueManager::new(Arc::clone(&queue) as Arc<_>, Arc::new(JobRegistry::new()))
            .with_connection("bulk", Arc::clone(&bulk) as Arc<_>)
            .with_locks(rainier_cache::LockManager::new(Arc::new(MemoryCache::new())));

        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_some());
        assert!(manager
            .pending(RebuildIndex)
            .unwrap()
            .on_connection("bulk")
            .send()
            .await
            .unwrap()
            .is_none());

        assert_eq!(queue.size("default").await.unwrap(), 1);
        assert_eq!(bulk.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_pending_dispatch_that_is_dropped_leaves_no_lock() {
        // The claim is at `send`, not at `pending`, so a builder that is
        // configured and then abandoned must not block the next dispatch.
        let (manager, _queue) = manager();

        let abandoned = manager.pending(RebuildIndex).unwrap().on_queue("high");
        drop(abandoned);

        assert!(manager.dispatch(RebuildIndex).await.unwrap().is_some());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_application_that_says_nothing_drains_default_exactly_as_before() {
        // The whole point of a default: adding this must not change what any
        // existing application does.
        let manager = QueueManager::fake();

        assert_eq!(manager.default_queues(), ["default"]);
    }

    #[test]
    fn declared_queues_are_kept_in_priority_order() {
        // Earlier first, like the flag — that ordering is how a `high` queue
        // gets ahead of `default`, and sorting or deduplicating would silently
        // change the priority the application asked for.
        let manager =
            QueueManager::fake().with_default_queues(["transcode-video", "transcode-image"]);

        assert_eq!(manager.default_queues(), ["transcode-video", "transcode-image"]);
    }

    #[test]
    fn blank_names_are_dropped_rather_than_drained() {
        // A trailing comma in a chart value is the realistic source, and a
        // queue named "" is one nothing is ever dispatched to.
        let manager = QueueManager::fake().with_default_queues(["high", "  ", "", " low "]);

        assert_eq!(manager.default_queues(), ["high", "low"]);
    }

    #[test]
    fn declaring_nothing_leaves_the_previous_default_standing() {
        // A worker draining no queues at all is never what was meant, and it
        // fails the same silent way the flag does: it starts, reports itself
        // healthy, and processes nothing.
        let manager = QueueManager::fake().with_default_queues(Vec::<String>::new());
        assert_eq!(manager.default_queues(), ["default"]);

        let manager =
            QueueManager::fake().with_default_queues(["high"]).with_default_queues(["", "   "]);
        assert_eq!(
            manager.default_queues(),
            ["high"],
            "a bad later call must not erase a good one"
        );
    }

    use super::*;
    use crate::queue::MemoryQueue;
    use rainier_support::Error;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// How many times `Ping` has run, resolved from the job's own container.
    ///
    /// Per-test rather than a `static`: these tests run concurrently in one
    /// process, so a shared counter is incremented by whichever other test
    /// happens to dispatch a `Ping` between a store and a load. That made
    /// `the_sync_driver_runs_the_job_immediately` flaky in a way that depended
    /// on the binary's test ordering.
    ///
    /// Going through the container also exercises `JobContext::resolve`, which
    /// is how a real job reaches its dependencies.
    type Runs = Arc<AtomicU32>;

    fn counting_container() -> (Arc<Container>, Runs) {
        let runs: Runs = Arc::new(AtomicU32::new(0));
        let container = Arc::new(Container::new());
        container.instance(Arc::clone(&runs));
        (container, runs)
    }

    #[derive(Serialize, Deserialize)]
    struct Ping;

    #[async_trait::async_trait]
    impl Job for Ping {
        const NAME: &'static str = "test.ping";
        async fn handle(&self, context: &JobContext) -> Result<()> {
            // A container without a counter means a test that does not count,
            // which is most of them.
            if let Ok(runs) = context.resolve::<Runs>() {
                runs.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Boom;

    #[async_trait::async_trait]
    impl Job for Boom {
        const NAME: &'static str = "test.boom";
        async fn handle(&self, _: &JobContext) -> Result<()> {
            Err(Error::internal("exploded"))
        }
    }

    fn registry() -> Arc<JobRegistry> {
        Arc::new(JobRegistry::new().with::<Ping>().with::<Boom>())
    }

    fn manager(queue: Arc<dyn Queue>) -> QueueManager {
        QueueManager::new(queue, registry())
    }

    #[tokio::test]
    async fn dispatching_enqueues_the_job() {
        let queue = Arc::new(MemoryQueue::new());
        let manager = manager(Arc::clone(&queue) as Arc<dyn Queue>);

        let id = manager.dispatch(Ping).await.unwrap();
        assert!(id.is_some_and(|id| !id.is_empty()));
        assert_eq!(queue.size("default").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn dispatch_options_apply() {
        let queue = Arc::new(MemoryQueue::new());
        let manager = manager(Arc::clone(&queue) as Arc<dyn Queue>);

        manager
            .pending(Ping)
            .unwrap()
            .on_queue("mail")
            .delay(Duration::from_secs(60))
            .tries(5)
            .send()
            .await
            .unwrap();

        let pending = queue.pending("mail");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].max_attempts, 5);
        assert!(!pending[0].is_available(), "the delay should hold it back");
    }

    #[tokio::test]
    async fn dispatch_on_and_after_are_shorthands() {
        let queue = Arc::new(MemoryQueue::new());
        let manager = manager(Arc::clone(&queue) as Arc<dyn Queue>);

        manager.dispatch_on("mail", Ping).await.unwrap();
        manager.dispatch_after(Duration::from_secs(30), Ping).await.unwrap();

        assert_eq!(queue.size("mail").await.unwrap(), 1);
        assert!(!queue.pending("default")[0].is_available());
    }

    #[tokio::test]
    async fn the_sync_driver_runs_the_job_immediately() {
        let (container, runs) = counting_container();
        let manager = manager(Arc::new(SyncQueue::new(registry(), container)));

        manager.dispatch(Ping).await.unwrap();

        assert_eq!(runs.load(Ordering::SeqCst), 1, "it ran during dispatch");
    }

    #[tokio::test]
    async fn the_sync_driver_surfaces_a_failure_to_the_dispatcher() {
        // The behaviour that makes it unsuitable for production, made explicit.
        let sync = Arc::new(SyncQueue::new(registry(), Arc::new(Container::new())));
        let manager = manager(sync);

        let err = manager.dispatch(Boom).await.unwrap_err();
        assert!(err.message().contains("exploded"), "{}", err.message());
    }

    #[tokio::test]
    async fn dispatch_now_bypasses_the_queue() {
        let (container, runs) = counting_container();
        let queue = Arc::new(MemoryQueue::new());
        let manager = manager(Arc::clone(&queue) as Arc<dyn Queue>);

        manager.dispatch_now(Ping, container).await.unwrap();

        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(queue.size("default").await.unwrap(), 0, "nothing was queued");
    }

    #[tokio::test]
    async fn a_fake_records_instead_of_queueing() {
        let manager = QueueManager::fake();

        manager.dispatch(Ping).await.unwrap();
        manager.dispatch_on("mail", Ping).await.unwrap();

        manager.assert_pushed::<Ping>();
        manager.assert_pushed_times::<Ping>(2);
        manager.assert_pushed_on::<Ping>("mail");
        manager.assert_not_pushed::<Boom>();
        assert_eq!(manager.all_pushed().len(), 2);
    }

    // --- connections --------------------------------------------------------

    /// A manager with a default and two named connections, all three distinct
    /// stores.
    fn multi_connection() -> (QueueManager, Arc<MemoryQueue>, Arc<MemoryQueue>, Arc<MemoryQueue>) {
        let default = Arc::new(MemoryQueue::new());
        let primary = Arc::new(MemoryQueue::new());
        let bulk = Arc::new(MemoryQueue::new());

        let manager = QueueManager::new(Arc::clone(&default) as Arc<_>, registry())
            .with_connection("primary", Arc::clone(&primary) as Arc<_>)
            .with_connection("bulk", Arc::clone(&bulk) as Arc<_>);

        (manager, default, primary, bulk)
    }

    #[tokio::test]
    async fn a_dispatch_lands_on_the_connection_it_named_and_on_no_other() {
        let (manager, default, primary, bulk) = multi_connection();

        manager.pending(Ping).unwrap().on_connection("bulk").send().await.unwrap();

        assert_eq!(bulk.size("default").await.unwrap(), 1);
        assert_eq!(primary.size("default").await.unwrap(), 0, "not the other named one");
        assert_eq!(default.size("default").await.unwrap(), 0, "and not the default");
    }

    #[tokio::test]
    async fn an_undeclared_connection_never_becomes_the_default() {
        // The silent failure this exists to prevent: a fallback here is a job
        // accepted into a backend nobody drains, which raises nothing.
        let (manager, default, primary, bulk) = multi_connection();

        let err = manager
            .pending(Ping)
            .unwrap()
            .on_connection("blk")
            .send()
            .await
            .err()
            .expect("`blk` is not declared");

        assert!(err.message().contains("`blk`"), "{}", err.message());
        assert!(err.message().contains("`bulk`"), "the declared ones: {}", err.message());
        assert!(err.message().contains("`primary`"), "the declared ones: {}", err.message());

        assert_eq!(default.size("default").await.unwrap(), 0, "nothing fell back");
        assert_eq!(primary.size("default").await.unwrap(), 0);
        assert_eq!(bulk.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_connection_and_a_queue_are_two_different_choices() {
        // Which backend, and which lane within it. Neither implies the other.
        let (manager, _default, _primary, bulk) = multi_connection();

        manager.pending(Ping).unwrap().on_connection("bulk").on_queue("mail").send().await.unwrap();

        assert_eq!(bulk.size("mail").await.unwrap(), 1);
        assert_eq!(bulk.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_dispatch_that_names_no_connection_still_goes_to_the_default() {
        // Every existing call site, unchanged: declaring connections must not
        // move work that never asked to be moved.
        let (manager, default, primary, bulk) = multi_connection();

        manager.dispatch(Ping).await.unwrap();
        manager.dispatch_on("mail", Ping).await.unwrap();
        manager.pending(Ping).unwrap().tries(5).send().await.unwrap();

        assert_eq!(default.size("default").await.unwrap(), 2);
        assert_eq!(default.size("mail").await.unwrap(), 1);
        assert_eq!(primary.size("default").await.unwrap(), 0);
        assert_eq!(bulk.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_undeclared_name_resolves_to_none_rather_than_the_default() {
        let (manager, _default, _primary, _bulk) = multi_connection();

        assert!(manager.connection("bulk").is_some());
        assert!(manager.connection("blk").is_none());
        assert!(!manager.has_connection("blk"));
        assert_eq!(manager.connection_names().collect::<Vec<_>>(), vec!["bulk", "primary"]);
    }

    #[tokio::test]
    async fn a_fake_checks_the_connection_name_as_the_real_manager_would() {
        // A fake that accepted a name the real manager rejects is a green test
        // and a production dispatch that lands nowhere.
        let manager = QueueManager::fake();

        let err = manager
            .pending(Ping)
            .unwrap()
            .on_connection("bulk")
            .send()
            .await
            .err()
            .expect("a fake declares no connections");
        assert!(err.message().contains("faking"), "{}", err.message());

        let declared = QueueManager::fake().with_connection("bulk", Arc::new(MemoryQueue::new()));
        declared.pending(Ping).unwrap().on_connection("bulk").send().await.unwrap();
        declared.assert_pushed::<Ping>();
    }

    #[tokio::test]
    #[should_panic(expected = "needs a faking manager")]
    async fn assertions_refuse_to_pass_vacuously() {
        let queue = Arc::new(MemoryQueue::new());
        let manager = manager(queue as Arc<dyn Queue>);
        manager.dispatch(Ping).await.unwrap();
        manager.assert_not_pushed::<Ping>();
    }

    #[tokio::test]
    #[should_panic(expected = "test.boom")]
    async fn a_missing_dispatch_reports_what_was_queued() {
        let manager = QueueManager::fake();
        manager.dispatch(Ping).await.unwrap();
        manager.assert_pushed::<Boom>();
    }
}
