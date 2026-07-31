//! The schedule — [`Schedule`] and what running it produces.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rainier_cache::LockManager;
use rainier_container::Application;
use rainier_support::Result;

use crate::task::{ClosureTask, Outcome, ScheduledTask, Skipped, Task};

/// Every task an application schedules.
///
/// Declared in one function, bound in
/// the container, and read by `schedule:run`.
///
/// ```
/// # use rainier_scheduler::Schedule;
/// # use std::time::Duration;
/// pub fn schedule(schedule: &mut Schedule) {
///     schedule
///         .call("prune-sessions", |_| Box::pin(async { Ok(()) }))
///         .daily_at("03:00")
///         .without_overlapping(Duration::from_secs(1800));
///
///     schedule
///         .call("send-digest", |_| Box::pin(async { Ok(()) }))
///         .weekly_on(1, "09:00")
///         .on_one_server();
/// }
/// # let mut s = Schedule::new();
/// # schedule(&mut s);
/// # assert_eq!(s.len(), 2);
/// ```
#[derive(Default)]
pub struct Schedule {
    tasks: Vec<ScheduledTask>,
}

impl Schedule {
    /// An empty schedule.
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule a [`Task`].
    ///
    /// The general form. [`call`](Self::call) is the convenient one, and the
    /// framework adds `job` and `command` on top of this.
    pub fn add(&mut self, task: impl Task) -> &mut ScheduledTask {
        self.add_arc(Arc::new(task))
    }

    /// Schedule an already-shared task.
    pub fn add_arc(&mut self, task: Arc<dyn Task>) -> &mut ScheduledTask {
        self.tasks.push(ScheduledTask::new(task));
        self.tasks.last_mut().expect("just pushed")
    }

    /// Schedule a closure, under `name`.
    ///
    /// The name is the **lock key** for `without_overlapping` and
    /// `on_one_server`, so it has to be stable across machines and restarts.
    pub fn call<F>(&mut self, name: impl Into<String>, run: F) -> &mut ScheduledTask
    where
        F: for<'a> Fn(&'a Application) -> rainier_support::BoxFuture<'a, Result<()>>
            + Send
            + Sync
            + 'static,
    {
        self.add(ClosureTask::new(name, run))
    }

    /// Every scheduled task.
    pub fn tasks(&self) -> &[ScheduledTask] {
        &self.tasks
    }

    /// How many are scheduled.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether nothing is scheduled.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// The tasks whose expression fires at `at`.
    pub fn due(&self, at: DateTime<Utc>) -> Vec<&ScheduledTask> {
        self.tasks.iter().filter(|task| task.is_due(at)).collect()
    }

    /// Tasks whose schedule did not parse, with the message.
    ///
    /// The builders are lenient — `.daily_at("nope")` keeps the previous
    /// expression rather than panicking mid-chain — so this is how a boot check
    /// turns that leniency back into a failure.
    pub fn errors(&self) -> Vec<(String, String)> {
        self.tasks
            .iter()
            .filter_map(|task| task.error().map(|e| (task.name(), e.to_string())))
            .collect()
    }

    /// Any two tasks sharing a name.
    ///
    /// Worth checking at boot. A name is a lock key, so two tasks called the
    /// same thing with `without_overlapping` between them will block *each
    /// other* — which looks like a task that mysteriously never runs, and is
    /// nothing of the sort.
    pub fn duplicate_names(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeMap::<String, usize>::new();
        for task in &self.tasks {
            *seen.entry(task.name()).or_insert(0) += 1;
        }

        seen.into_iter().filter(|(_, count)| *count > 1).map(|(name, _)| name).collect()
    }

    /// The tasks whose guarantees need the lock to be shared between machines.
    ///
    /// Every task using `without_overlapping` or `on_one_server`. Pair it with
    /// [`LockManager::is_shared`](rainier_cache::LockManager::is_shared) at
    /// boot: if these are non-empty and the lock is not shared, the flags are
    /// decoration. Each machine takes its own lock, gets it, and runs — which
    /// is the exact situation the flags were written to prevent, arriving
    /// without a single line of output to say so.
    pub fn tasks_needing_shared_locks(&self) -> Vec<String> {
        self.tasks.iter().filter(|t| t.needs_shared_locks()).map(|t| t.name()).collect()
    }

    /// Run everything due at `at`.
    ///
    /// Sequentially, in declaration order. Two reasons: a scheduler that
    /// fans out is a scheduler that can start twenty database-heavy tasks in
    /// the same second, and sequential ordering means a task declared after
    /// another can rely on it having finished.
    ///
    /// A task that fails is logged and the rest still run — one broken report
    /// must not stop the backups.
    pub async fn run_due(
        &self,
        app: &Application,
        locks: &LockManager,
        at: DateTime<Utc>,
    ) -> RunSummary {
        let mut summary = RunSummary::default();

        for task in self.due(at) {
            let name = task.name();
            let started = std::time::Instant::now();

            match task.run(app, locks, at).await {
                Outcome::Ran => {
                    tracing::info!(task = %name, elapsed_ms = started.elapsed().as_millis() as u64, "scheduled task ran");
                    summary.ran.push(name);
                }
                Outcome::Failed(e) => {
                    tracing::error!(task = %name, error = %e.message(), "scheduled task failed");
                    summary.failed.push((name, e.message().to_string()));
                }
                Outcome::Skipped(why) => {
                    tracing::debug!(task = %name, %why, "scheduled task skipped");
                    summary.skipped.push((name, why));
                }
            }
        }

        summary
    }
}

impl std::fmt::Debug for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.tasks.iter().map(|t| t.name())).finish()
    }
}

/// What one pass of the scheduler did.
#[derive(Debug, Default)]
pub struct RunSummary {
    /// Tasks that ran and succeeded.
    pub ran: Vec<String>,
    /// Tasks that ran and failed, with the message.
    pub failed: Vec<(String, String)>,
    /// Tasks that were due but did not run, and why.
    pub skipped: Vec<(String, Skipped)>,
}

impl RunSummary {
    /// How many were due.
    pub fn considered(&self) -> usize {
        self.ran.len() + self.failed.len() + self.skipped.len()
    }

    /// Whether anything failed.
    ///
    /// What `schedule:run` turns into its exit code, so a supervisor notices.
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rainier_cache::MemoryCache;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use std::time::Duration;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 27, h, m, 0).unwrap()
    }

    fn locks() -> LockManager {
        LockManager::new(Arc::new(MemoryCache::new()))
    }

    /// A task that counts its runs.
    fn counting(
        counter: StdArc<AtomicUsize>,
    ) -> impl for<'a> Fn(&'a Application) -> rainier_support::BoxFuture<'a, Result<()>> {
        move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(futures_stub::Ready)
        }
    }

    /// A minimal `Future` that is immediately ready, so the tests need no
    /// futures dependency.
    mod futures_stub {
        use rainier_support::Result;
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        pub struct Ready;

        impl Future for Ready {
            type Output = Result<()>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(Ok(()))
            }
        }
    }

    #[test]
    fn a_task_defaults_to_every_minute() {
        let mut schedule = Schedule::new();
        schedule.call("tick", |_| Box::pin(futures_stub::Ready));

        assert_eq!(schedule.tasks()[0].expression().source(), "* * * * *");
        assert!(schedule.tasks()[0].is_due(at(3, 17)));
    }

    #[test]
    fn only_due_tasks_are_returned() {
        let mut schedule = Schedule::new();
        schedule.call("nightly", |_| Box::pin(futures_stub::Ready)).daily_at("03:00");
        schedule.call("hourly", |_| Box::pin(futures_stub::Ready)).hourly();

        let due: Vec<String> = schedule.due(at(3, 0)).iter().map(|t| t.name()).collect();
        assert_eq!(due, vec!["nightly", "hourly"]);

        let due: Vec<String> = schedule.due(at(4, 0)).iter().map(|t| t.name()).collect();
        assert_eq!(due, vec!["hourly"]);

        assert!(schedule.due(at(4, 30)).is_empty());
    }

    #[tokio::test]
    async fn running_a_due_task_runs_it_once() {
        let app = Application::new(".");
        let counter = StdArc::new(AtomicUsize::new(0));

        let mut schedule = Schedule::new();
        schedule.call("tick", counting(StdArc::clone(&counter)));

        let summary = schedule.run_due(&app, &locks(), at(0, 0)).await;

        assert_eq!(summary.ran, vec!["tick"]);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failing_task_does_not_stop_the_others() {
        // One broken report must not stop the backups.
        let app = Application::new(".");
        let counter = StdArc::new(AtomicUsize::new(0));

        let mut schedule = Schedule::new();
        schedule
            .call("broken", |_| Box::pin(async { Err(rainier_support::Error::internal("boom")) }));
        schedule.call("fine", counting(StdArc::clone(&counter)));

        let summary = schedule.run_due(&app, &locks(), at(0, 0)).await;

        assert_eq!(summary.failed.len(), 1);
        assert_eq!(summary.ran, vec!["fine"]);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(summary.has_failures());
    }

    #[tokio::test]
    async fn a_false_condition_skips_before_any_lock_is_taken() {
        let app = Application::new(".");
        let counter = StdArc::new(AtomicUsize::new(0));

        let mut schedule = Schedule::new();
        schedule
            .call("switched-off", counting(StdArc::clone(&counter)))
            .when(|| false)
            .without_overlapping(Duration::from_secs(60));

        let summary = schedule.run_due(&app, &locks(), at(0, 0)).await;

        assert_eq!(summary.skipped, vec![("switched-off".to_string(), Skipped::ConditionFalse)]);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn without_overlapping_skips_while_a_previous_run_holds_the_lock() {
        let app = Application::new(".");
        let locks = locks();

        let mut schedule = Schedule::new();
        schedule
            .call("slow", |_| Box::pin(futures_stub::Ready))
            .without_overlapping(Duration::from_secs(600));

        // Stand in for a run that is still going.
        let _held = locks
            .lock("schedule:overlap:slow", Duration::from_secs(600))
            .acquire()
            .await
            .unwrap()
            .unwrap();

        let summary = schedule.run_due(&app, &locks, at(0, 0)).await;

        assert_eq!(summary.skipped, vec![("slow".to_string(), Skipped::StillRunning)]);
        assert!(summary.ran.is_empty());
    }

    #[tokio::test]
    async fn the_overlap_lock_is_released_so_the_next_run_proceeds() {
        let app = Application::new(".");
        let locks = locks();
        let counter = StdArc::new(AtomicUsize::new(0));

        let mut schedule = Schedule::new();
        schedule
            .call("tick", counting(StdArc::clone(&counter)))
            .without_overlapping(Duration::from_secs(600));

        schedule.run_due(&app, &locks, at(0, 0)).await;
        schedule.run_due(&app, &locks, at(0, 1)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 2, "the lock must not outlive the run");
    }

    #[tokio::test]
    async fn on_one_server_lets_exactly_one_machine_run_the_minute() {
        // Two schedules sharing one cache stand in for two machines.
        let app = Application::new(".");
        let cache: Arc<dyn rainier_cache::Cache> = Arc::new(MemoryCache::new());
        let first = LockManager::new(Arc::clone(&cache));
        let second = LockManager::new(Arc::clone(&cache));

        let counter = StdArc::new(AtomicUsize::new(0));

        let mut a = Schedule::new();
        a.call("digest", counting(StdArc::clone(&counter))).on_one_server();

        let mut b = Schedule::new();
        b.call("digest", counting(StdArc::clone(&counter))).on_one_server();

        let ran_a = a.run_due(&app, &first, at(9, 0)).await;
        let ran_b = b.run_due(&app, &second, at(9, 0)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one machine should run it");
        assert_eq!(ran_a.ran.len() + ran_b.ran.len(), 1);
        assert_eq!(ran_a.skipped.len() + ran_b.skipped.len(), 1, "and the other should say why");
    }

    #[tokio::test]
    async fn on_one_server_claims_each_occurrence_separately() {
        // The claim is keyed by the minute, so winning at 09:00 must not stop
        // the same machine — or another — running at 09:01.
        let app = Application::new(".");
        let locks = locks();
        let counter = StdArc::new(AtomicUsize::new(0));

        let mut schedule = Schedule::new();
        schedule.call("digest", counting(StdArc::clone(&counter))).on_one_server();

        schedule.run_due(&app, &locks, at(9, 0)).await;
        schedule.run_due(&app, &locks, at(9, 1)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_one_server_claim_outlives_the_run() {
        // Releasing it when the run finishes would let a machine whose clock is
        // a second behind claim the same minute and run it again.
        let app = Application::new(".");
        let locks = locks();
        let counter = StdArc::new(AtomicUsize::new(0));

        let mut schedule = Schedule::new();
        schedule.call("digest", counting(StdArc::clone(&counter))).on_one_server();

        schedule.run_due(&app, &locks, at(9, 0)).await;
        schedule.run_due(&app, &locks, at(9, 0)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1, "the same minute must not run twice");
    }

    #[test]
    fn duplicate_names_are_reported() {
        // Two tasks sharing a name share a lock, so `without_overlapping`
        // between them makes each block the other.
        let mut schedule = Schedule::new();
        schedule.call("prune", |_| Box::pin(futures_stub::Ready)).hourly();
        schedule.call("prune", |_| Box::pin(futures_stub::Ready)).daily();
        schedule.call("other", |_| Box::pin(futures_stub::Ready));

        assert_eq!(schedule.duplicate_names(), vec!["prune"]);
    }

    #[test]
    fn a_schedule_with_unique_names_reports_none() {
        let mut schedule = Schedule::new();
        schedule.call("a", |_| Box::pin(futures_stub::Ready));
        schedule.call("b", |_| Box::pin(futures_stub::Ready));

        assert!(schedule.duplicate_names().is_empty());
    }
}
