//! Jobs — the [`Job`] contract, the serialised [`QueuedJob`] payload, and the
//! [`JobRegistry`] that turns one back into the other.
//!
//! A queued job crosses a process boundary: it is written by a web request and
//! read, perhaps minutes later, by a worker that may be a different process on
//! a different machine. So a job is defined by two things — a **payload** that
//! serialises, and a **name** that survives serialisation.
//!
//! The name is explicit rather than derived from the Rust type path, because
//! the type path is not a stable identifier: renaming a struct or moving it
//! between modules would strand every job already in the queue.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rainier_container::Container;
use rainier_support::{BoxedFuture, Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// What a job can reach while it runs.
pub struct JobContext {
    container: Arc<Container>,
    attempt: u32,
    max_attempts: u32,
    id: String,
    queue: String,
}

impl JobContext {
    /// Build a context.
    pub fn new(
        container: Arc<Container>,
        id: impl Into<String>,
        queue: impl Into<String>,
        attempt: u32,
        max_attempts: u32,
    ) -> Self {
        Self { container, attempt, max_attempts, id: id.into(), queue: queue.into() }
    }

    /// The service container, for resolving whatever the job needs.
    ///
    /// A job cannot capture its dependencies — it was serialised — so it
    /// resolves them here instead.
    pub fn container(&self) -> &Arc<Container> {
        &self.container
    }

    /// Resolve a service.
    pub fn resolve<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.container.resolve::<T>()
    }

    /// Which attempt this is, starting at 1.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// How many attempts the job gets in total.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Whether this is the last attempt — a job that wants to record a
    /// permanent failure can check this before returning `Err`.
    pub fn is_last_attempt(&self) -> bool {
        self.attempt >= self.max_attempts
    }

    /// The queued job's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The queue it came from.
    pub fn queue(&self) -> &str {
        &self.queue
    }
}

impl std::fmt::Debug for JobContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobContext")
            .field("id", &self.id)
            .field("queue", &self.queue)
            .field("attempt", &format_args!("{}/{}", self.attempt, self.max_attempts))
            .finish()
    }
}

/// Work to be done later.
///
/// ```
/// use rainier_queue::{Job, JobContext};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct SendWelcomeEmail {
///     user_id: u64,
/// }
///
/// #[async_trait::async_trait]
/// impl Job for SendWelcomeEmail {
///     const NAME: &'static str = "mail.welcome";
///
///     async fn handle(&self, _context: &JobContext) -> rainier_support::Result<()> {
///         println!("emailing user {}", self.user_id);
///         Ok(())
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// The job's stable name on the wire.
    ///
    /// Must not change once jobs of this type exist in a queue, and must be
    /// unique across the application.
    const NAME: &'static str;

    /// Do the work.
    ///
    /// Returning `Err` releases the job for another attempt, until
    /// [`TRIES`](Job::TRIES) is exhausted and it is marked failed.
    async fn handle(&self, context: &JobContext) -> Result<()>;

    /// How many times to attempt the job before giving up.
    const TRIES: u32 = 3;

    /// The queue this job goes on by default.
    const QUEUE: &'static str = "default";

    /// The queue this job goes on, resolved when it is dispatched.
    ///
    /// Defaults to [`QUEUE`](Self::QUEUE), so a job that names a constant queue
    /// says nothing more and behaves exactly as before.
    ///
    /// It exists because a `const` cannot express a queue name the application
    /// computes — one carrying an environment prefix, a driver-dependent shape,
    /// or anything else known only at run time. Without this, such an
    /// application has to name the queue at every call site, and the constant
    /// on the job becomes a decoy: it reads like the job's queue, it is not
    /// what the job is dispatched to, and the first plain `dispatch` written
    /// against it sends the job to a queue that may have no worker.
    ///
    /// That failure is silent. A job on a queue nobody drains is not an error
    /// anywhere — it is accepted, stored, and never run.
    fn queue(&self) -> String {
        Self::QUEUE.to_string()
    }

    /// How long this job may run before it is abandoned.
    ///
    /// `None` defers to the worker's own limit, which is where every job's
    /// timeout used to come from. That default is chosen for the shortest
    /// work in the application, so a job whose honest duration is minutes had
    /// no way to say so and was killed partway through — the transcoder's
    /// ladder published its smallest rendition and lost the rest, reported
    /// only as "the job exceeded its 60s timeout" on the worker.
    ///
    /// State it on the job rather than on the deployment. How long the work
    /// legitimately takes is a property of the work, and a flag on the worker
    /// command has to be kept in step with every job that worker might drain
    /// — including ones added later, by someone who will not think to look.
    const TIMEOUT: Option<Duration> = None;

    /// How long to wait before retrying after `attempt` failures.
    ///
    /// Exponential by default — 1s, 2s, 4s, … — because the usual reason a job
    /// fails is a dependency that is briefly unavailable, and retrying at full
    /// speed makes that worse.
    fn backoff(attempt: u32) -> Duration {
        Duration::from_secs(1u64 << attempt.min(6))
    }

    /// Called after the final attempt fails. For recording the failure
    /// somewhere the application will notice.
    async fn failed(&self, context: &JobContext, error: &Error) {
        let _ = (context, error);
    }

    /// What makes two of these job "the same one", or `None` for jobs that are
    /// never duplicates.
    ///
    /// Returning `Some` makes dispatch take an
    /// [atomic lock](rainier_cache::Lock) on this id, so a second dispatch
    /// while the first is still pending is **dropped rather than queued**.
    ///
    /// ```ignore
    /// impl Job for RebuildSearchIndex {
    ///     const NAME: &'static str = "search.rebuild";
    ///
    ///     // One rebuild at a time, whatever asks for it.
    ///     fn unique_id(&self) -> Option<String> {
    ///         Some(String::new())
    ///     }
    /// }
    ///
    /// impl Job for SendInvoice {
    ///     const NAME: &'static str = "billing.invoice";
    ///
    ///     // One per invoice, but two different invoices are two jobs.
    ///     fn unique_id(&self) -> Option<String> {
    ///         Some(self.invoice_id.to_string())
    ///     }
    /// }
    /// ```
    ///
    /// The lock key is `NAME` **and** this id, so two job types returning the
    /// same id do not collide.
    ///
    /// # This deduplicates dispatch, not execution
    ///
    /// The lock is released when the job finishes, so it stops a *queue* filling
    /// with a hundred copies of the same work. It does not stop two workers
    /// running one job twice — that is the queue's reservation, a different
    /// mechanism — and it does not survive the lock expiring under a job that
    /// overran [`UNIQUE_FOR`](Job::UNIQUE_FOR).
    fn unique_id(&self) -> Option<String> {
        None
    }

    /// How long the uniqueness lock is held if the job never finishes.
    ///
    /// The safety net for a worker that dies mid-job: without it, one crash
    /// would block that job's id forever. Set it longer than the job takes.
    const UNIQUE_FOR: Duration = Duration::from_secs(3600);
}

/// A job serialised for the queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedJob {
    /// A unique id for this queued instance.
    pub id: String,
    /// The [`Job::NAME`] the registry resolves.
    pub name: String,
    /// The job's serialised body.
    pub payload: serde_json::Value,
    /// Which queue it is on.
    pub queue: String,
    /// How many times it has been attempted.
    pub attempts: u32,
    /// How many attempts it gets.
    pub max_attempts: u32,
    /// The earliest it may run — how delays are expressed.
    pub available_at: DateTime<Utc>,
    /// When it was enqueued.
    pub created_at: DateTime<Utc>,
    /// The uniqueness lock this job holds, if it declared a
    /// [`unique_id`](Job::unique_id).
    ///
    /// Carried on the job rather than kept by the dispatcher, because the
    /// process that releases it is a **worker**, in another process, minutes
    /// later. `None` for the ordinary case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_key: Option<String>,

    /// A driver's handle on **this delivery**, set when the job is reserved.
    ///
    /// Not the job's id: a job redelivered after a worker died is the same job
    /// with a new handle. Only the driver that set it knows what it means —
    /// for Redis it is the stream entry id that `XACK` needs.
    ///
    /// `#[serde(skip)]` on purpose, twice over. It does not belong in the
    /// stored job, because it does not exist until something reserves it; and
    /// a stored one would be stale the moment it was redelivered.
    ///
    /// It exists because the alternative was writing it into
    /// [`payload`](Self::payload), which corrupted the job. `payload` is
    /// `Value::Null` for a unit-struct job — 17 of them in the first
    /// application to run this — and `payload[key] = value` on a `Null`
    /// silently promotes it to an object. Round-tripping then yielded `{}`,
    /// and `serde_json::from_value::<UnitStruct>({})` fails with "invalid
    /// type: map, expected unit struct". The job retried to its limit and went
    /// to the failed table, having never run. Nothing could fix that from the
    /// other end either: `{}` is also what an empty named struct serialises
    /// to, so a driver stripping the key cannot know whether to restore `Null`.
    #[serde(skip)]
    pub delivery_handle: Option<String>,
}

impl QueuedJob {
    /// Serialise `job` for the queue.
    pub fn from_job<J: Job>(job: &J) -> Result<Self> {
        let now = Utc::now();
        Ok(Self {
            id: generate_id(),
            name: J::NAME.to_string(),
            payload: serde_json::to_value(job)?,
            // `job.queue()`, not `J::QUEUE` — so an application that computes
            // its queue names is dispatched to the queue it actually drains.
            queue: job.queue(),
            attempts: 0,
            max_attempts: J::TRIES,
            unique_key: None,
            delivery_handle: None,
            available_at: now,
            created_at: now,
        })
    }

    /// An envelope around a payload this framework did not serialise.
    ///
    /// For work handed to a consumer that is not this application's worker —
    /// another service, another language — but which still travels as a
    /// `QueuedJob` because something in between reserves it through
    /// [`Queue::reserve`](crate::Queue::reserve) and forwards the envelope
    /// whole.
    ///
    /// # Not the same as [`push_raw`](crate::Queue::push_raw)
    ///
    /// `push_raw` is for a consumer reading the queue **directly**, where an
    /// envelope it has never heard of is indistinguishable from corruption.
    /// This is the opposite case: the consumer never sees the queue, and
    /// something reserving on its behalf can only reserve a `QueuedJob`. Using
    /// `push_raw` there fails at the driver — a stream entry that is not an
    /// envelope cannot be reserved as one.
    ///
    /// `max_attempts` is 1. Redelivery is the forwarder's business — it is the
    /// only party that knows whether the far end ever received the work — and
    /// a second attempt minted here would race whatever it does about the
    /// first.
    pub fn foreign(
        queue: impl Into<String>,
        name: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: generate_id(),
            name: name.into(),
            payload,
            queue: queue.into(),
            attempts: 0,
            max_attempts: 1,
            unique_key: None,
            delivery_handle: None,
            available_at: now,
            created_at: now,
        }
    }

    /// Put it on a different queue.
    pub fn on_queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = queue.into();
        self
    }

    /// Hold it back for `delay`.
    pub fn delayed_by(mut self, delay: Duration) -> Self {
        let delay = chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());
        self.available_at = Utc::now() + delay;
        self
    }

    /// Make it available no earlier than `at`.
    pub fn available_at(mut self, at: DateTime<Utc>) -> Self {
        self.available_at = at;
        self
    }

    /// Whether it may run now.
    pub fn is_available(&self) -> bool {
        self.available_at <= Utc::now()
    }

    /// Whether every attempt has been used.
    pub fn is_exhausted(&self) -> bool {
        self.attempts >= self.max_attempts
    }
}

/// A unique id for a queued job: a timestamp for rough ordering plus
/// randomness for uniqueness.
///
/// Not a UUID, to avoid the dependency — the requirement is only that two ids
/// minted in the same process at the same moment differ.
fn generate_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let micros = Utc::now().timestamp_micros();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{micros:x}-{sequence:x}")
}

/// Runs a [`QueuedJob`] whose concrete type has been erased.
type Runner =
    Arc<dyn Fn(serde_json::Value, Arc<JobContext>) -> BoxedFuture<Result<()>> + Send + Sync>;

/// Maps a job's wire name back to code that can run it.
///
/// A worker reads `{"name": "mail.welcome", …}` and has no idea what type that
/// is; the registry is what closes the gap. Every job type an application
/// dispatches must be registered, or its jobs will fail as unknown.
#[derive(Default, Clone)]
pub struct JobRegistry {
    runners: HashMap<String, Runner>,
    /// Each job's own timeout, for the ones that declare one.
    ///
    /// Held here because the worker deserialises a job by name and never has
    /// the type: without this the trait's constant would be unreachable from
    /// the only place that could act on it.
    timeouts: HashMap<String, Duration>,
    /// The queues the registered jobs declare, in registration order.
    ///
    /// Kept because a worker draining a queue no registered job uses is doing
    /// nothing, and a queue a registered job uses that no worker drains is
    /// work that never runs. Both are silent. Recording what was registered
    /// means `queue:work` can answer "which queues" from the binary itself
    /// rather than from a flag somebody has to keep in step with it.
    ///
    /// Registration order, not sorted: it is the order the application listed
    /// its jobs, which is the only ordering that carries intent. Sorting would
    /// silently reprioritise.
    queues: Vec<String>,
}

impl JobRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `J` so the worker can run it.
    pub fn register<J: Job>(&mut self) -> &mut Self {
        let runner: Runner = Arc::new(|payload, context| {
            Box::pin(async move {
                let job: J = serde_json::from_value(payload).map_err(|e| {
                    Error::internal(format!(
                        "could not deserialise a `{}` job — its payload and its type have \
                         diverged: {e}",
                        J::NAME
                    ))
                })?;

                let outcome = job.handle(&context).await;
                if let Err(error) = &outcome {
                    if context.is_last_attempt() {
                        job.failed(&context, error).await;
                    }
                }
                outcome
            })
        });

        self.runners.insert(J::NAME.to_string(), runner);

        if let Some(timeout) = J::TIMEOUT {
            self.timeouts.insert(J::NAME.to_string(), timeout);
        }

        // `J::QUEUE` is a compile-time constant, so this cannot disagree with
        // where the job is actually dispatched. Two jobs sharing a queue
        // record it once.
        if !self.queues.iter().any(|queue| queue == J::QUEUE) {
            self.queues.push(J::QUEUE.to_string());
        }

        self
    }

    /// Builder form of [`register`](Self::register).
    pub fn with<J: Job>(mut self) -> Self {
        self.register::<J>();
        self
    }

    /// What `name` declared as its own timeout, if anything.
    ///
    /// `None` means the job did not state one and the worker's limit applies.
    pub fn timeout_for(&self, name: &str) -> Option<Duration> {
        self.timeouts.get(name).copied()
    }

    /// Whether `name` is registered.
    pub fn knows(&self, name: &str) -> bool {
        self.runners.contains_key(name)
    }

    /// Every queue the registered jobs declare, in registration order.
    ///
    /// What a worker should drain if nobody says otherwise: exactly the queues
    /// this binary has something to run. A list that came from configuration
    /// instead can drift from the binary in both directions, and both are
    /// silent — a queue nothing is registered for looks like an idle worker,
    /// and a registered job whose queue nobody drains looks like a queue that
    /// is merely slow.
    pub fn queues(&self) -> &[String] {
        &self.queues
    }

    /// Every registered job name, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.runners.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    /// Run a queued job.
    pub async fn run(&self, job: &QueuedJob, context: Arc<JobContext>) -> Result<()> {
        let runner = self.runners.get(&job.name).ok_or_else(|| {
            Error::internal(format!(
                "no job is registered as `{}` — register it on the JobRegistry, or the worker \
                 cannot run it. Known jobs: {:?}",
                job.name,
                self.names()
            ))
        })?;

        runner(job.payload.clone(), context).await
    }
}

impl std::fmt::Debug for JobRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRegistry").field("jobs", &self.names()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    static RAN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static FAILED: AtomicU32 = AtomicU32::new(0);

    #[derive(Serialize, Deserialize)]
    struct SendEmail {
        to: String,
    }

    #[async_trait::async_trait]
    impl Job for SendEmail {
        const NAME: &'static str = "mail.send";

        async fn handle(&self, _: &JobContext) -> Result<()> {
            RAN.lock().unwrap().push(self.to.clone());
            Ok(())
        }
    }

    #[derive(Serialize, Deserialize)]
    struct AlwaysFails;

    #[async_trait::async_trait]
    impl Job for AlwaysFails {
        const NAME: &'static str = "test.fails";
        const TRIES: u32 = 2;
        const QUEUE: &'static str = "slow";

        async fn handle(&self, _: &JobContext) -> Result<()> {
            Err(Error::internal("nope"))
        }

        async fn failed(&self, _: &JobContext, _: &Error) {
            FAILED.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn context(attempt: u32, max_attempts: u32) -> Arc<JobContext> {
        Arc::new(JobContext::new(
            Arc::new(Container::new()),
            "job-1",
            "default",
            attempt,
            max_attempts,
        ))
    }

    #[test]
    fn a_job_serialises_with_its_name_and_defaults() {
        let queued = QueuedJob::from_job(&SendEmail { to: "ada@example.com".into() }).unwrap();

        assert_eq!(queued.name, "mail.send");
        assert_eq!(queued.queue, "default");
        assert_eq!(queued.max_attempts, 3);
        assert_eq!(queued.attempts, 0);
        assert_eq!(queued.payload["to"], "ada@example.com");
        assert!(queued.is_available());
        assert!(!queued.is_exhausted());
    }

    #[test]
    fn a_job_can_declare_its_own_queue_and_tries() {
        let queued = QueuedJob::from_job(&AlwaysFails).unwrap();
        assert_eq!(queued.queue, "slow");
        assert_eq!(queued.max_attempts, 2);
    }

    #[derive(Serialize, Deserialize)]
    struct Prefixed;

    #[async_trait::async_trait]
    impl Job for Prefixed {
        const NAME: &'static str = "test.prefixed";
        const QUEUE: &'static str = "reports";

        // The case the constant cannot express: a name assembled at run time.
        fn queue(&self) -> String {
            format!("app-production-{}", Self::QUEUE)
        }

        async fn handle(&self, _: &JobContext) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_computed_queue_name_is_what_the_job_is_dispatched_to() {
        // Not `"reports"`. A job whose application prefixes its queue names is
        // dispatched to the prefixed one, so it lands where the worker draining
        // that name will find it. Taking the constant here instead would put it
        // on a queue nobody reads — accepted, stored, and never run, with
        // nothing reported anywhere.
        let queued = QueuedJob::from_job(&Prefixed).unwrap();
        assert_eq!(queued.queue, "app-production-reports");
    }

    #[test]
    fn a_job_that_does_not_override_still_uses_its_constant() {
        // The default has to stay exactly as it was: every existing job relies
        // on the constant alone, and this trait method is additive only if
        // saying nothing keeps meaning what it meant.
        assert_eq!(QueuedJob::from_job(&AlwaysFails).unwrap().queue, AlwaysFails::QUEUE);
        assert_eq!(
            QueuedJob::from_job(&SendEmail { to: "a@b.c".into() }).unwrap().queue,
            "default"
        );
    }

    #[test]
    fn a_delay_pushes_availability_into_the_future() {
        let queued = QueuedJob::from_job(&SendEmail { to: "a@b.c".into() })
            .unwrap()
            .delayed_by(Duration::from_secs(60));

        assert!(!queued.is_available());
        assert!(queued.available_at > Utc::now());
    }

    #[test]
    fn queued_ids_are_unique() {
        let a = QueuedJob::from_job(&SendEmail { to: "a".into() }).unwrap();
        let b = QueuedJob::from_job(&SendEmail { to: "b".into() }).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn backoff_grows_and_then_levels_off() {
        assert_eq!(SendEmail::backoff(0), Duration::from_secs(1));
        assert_eq!(SendEmail::backoff(1), Duration::from_secs(2));
        assert_eq!(SendEmail::backoff(3), Duration::from_secs(8));
        // Capped, so a long-failing job does not schedule itself years out.
        assert_eq!(SendEmail::backoff(20), Duration::from_secs(64));
    }

    #[tokio::test]
    async fn the_registry_runs_a_job_by_name() {
        RAN.lock().unwrap().clear();
        let registry = JobRegistry::new().with::<SendEmail>();

        let queued = QueuedJob::from_job(&SendEmail { to: "ada@example.com".into() }).unwrap();
        registry.run(&queued, context(1, 3)).await.unwrap();

        assert_eq!(*RAN.lock().unwrap(), vec!["ada@example.com"]);
    }

    #[tokio::test]
    async fn an_unregistered_job_names_itself_and_what_is_known() {
        let registry = JobRegistry::new().with::<SendEmail>();
        let queued = QueuedJob::from_job(&AlwaysFails).unwrap();

        let err = registry.run(&queued, context(1, 2)).await.unwrap_err();
        assert!(err.message().contains("test.fails"), "{}", err.message());
        assert!(err.message().contains("mail.send"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_payload_that_no_longer_fits_its_type_is_reported_clearly() {
        let registry = JobRegistry::new().with::<SendEmail>();
        let mut queued = QueuedJob::from_job(&SendEmail { to: "a".into() }).unwrap();
        queued.payload = serde_json::json!({ "recipient": "a" });

        let err = registry.run(&queued, context(1, 3)).await.unwrap_err();
        assert!(err.message().contains("diverged"), "{}", err.message());
    }

    #[tokio::test]
    async fn the_failed_hook_only_fires_on_the_last_attempt() {
        FAILED.store(0, Ordering::SeqCst);
        let registry = JobRegistry::new().with::<AlwaysFails>();
        let queued = QueuedJob::from_job(&AlwaysFails).unwrap();

        assert!(registry.run(&queued, context(1, 2)).await.is_err());
        assert_eq!(FAILED.load(Ordering::SeqCst), 0, "not on the first attempt");

        assert!(registry.run(&queued, context(2, 2)).await.is_err());
        assert_eq!(FAILED.load(Ordering::SeqCst), 1, "but yes on the last");
    }

    #[test]
    fn the_registry_lists_what_it_knows() {
        let registry = JobRegistry::new().with::<SendEmail>().with::<AlwaysFails>();
        assert_eq!(registry.names(), vec!["mail.send", "test.fails"]);
        assert!(registry.knows("mail.send"));
        assert!(!registry.knows("nope"));
    }

    #[test]
    fn the_context_reports_the_attempt_window() {
        let context = context(3, 3);
        assert_eq!(context.attempt(), 3);
        assert!(context.is_last_attempt());
        assert!(!self::context(1, 3).is_last_attempt());
    }
}
