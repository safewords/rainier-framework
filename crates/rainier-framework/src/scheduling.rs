//! Scheduling — the tasks and commands that need more than the scheduler crate
//! depends on.
//!
//! `rainier-scheduler` knows about the container and the cache and nothing
//! else, which is what keeps it usable without a queue or a console. The two
//! things people actually schedule — a queued job and a console command — are
//! adapters, and they live here because this is the crate that has both.
//!
//! ```ignore
//! // src/routes/console.rs
//! pub fn schedule(schedule: &mut Schedule) {
//!     schedule.job(PruneSessions).daily_at("03:00").without_overlapping(HALF_HOUR);
//!     schedule.command("app:seed").weekly_on(1, "04:00").on_one_server();
//!     schedule.call("heartbeat", |_| Box::pin(async { Ok(()) })).every_five_minutes();
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rainier_cache::LockManager;
use rainier_console::{exit, io, Arguments, Command, Console};
use rainier_container::Application;
use rainier_queue::{Job, QueueManager};
use rainier_scheduler::{Schedule, ScheduledTask, Task};
use rainier_support::{BoxFuture, Error, Result};

/// A [`Task`] that dispatches a queued job.
///
/// The scheduler's own work is then only "put it on the queue", which is what
/// you want: a scheduled task that runs for ten minutes is ten minutes the
/// scheduler is not looking at anything else, and a worker is the thing built
/// to run long work.
pub struct JobTask<J> {
    job: Arc<J>,
}

impl<J: Job + Clone> JobTask<J> {
    /// Dispatch `job` when the schedule fires.
    pub fn new(job: J) -> Self {
        Self { job: Arc::new(job) }
    }
}

impl<J: Job + Clone> Task for JobTask<J> {
    fn name(&self) -> String {
        // The job's registered name, which is stable across restarts and
        // machines — exactly what a lock key has to be.
        J::NAME.to_string()
    }

    fn run<'a>(&'a self, app: &'a Application) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let queue = app.resolve::<QueueManager>()?;
            queue.dispatch((*self.job).clone()).await?;
            Ok(())
        })
    }
}

/// A [`Task`] that runs a console command.
///
/// The command is resolved from the
/// console at run time, so a typo is a failure of that occurrence rather than
/// of the boot — which is the trade for being able to name a command the
/// scheduler crate cannot see.
pub struct CommandTask {
    name: String,
    argv: Vec<String>,
    console: Arc<Console>,
}

impl CommandTask {
    /// Run `name` from `console`, with `argv` after it.
    pub fn new(console: Arc<Console>, name: impl Into<String>, argv: Vec<String>) -> Self {
        Self { name: name.into(), argv, console }
    }
}

impl Task for CommandTask {
    fn name(&self) -> String {
        if self.argv.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.name, self.argv.join(" "))
        }
    }

    fn run<'a>(&'a self, app: &'a Application) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut argv = vec![self.name.clone()];
            argv.extend(self.argv.iter().cloned());

            let code = self.console.run_argv(app, argv).await;
            if code == exit::SUCCESS {
                Ok(())
            } else {
                Err(Error::internal(format!("`{}` exited with {code}", self.name)))
            }
        })
    }
}

/// `job` and `command`, on top of [`Schedule`].
///
/// An extension trait rather than methods on `Schedule`, because the scheduler
/// crate would otherwise have to depend on the queue and the console — and then
/// an application with neither would compile both.
pub trait ScheduleExt {
    /// Dispatch a queued job on a schedule.
    fn job<J: Job + Clone>(&mut self, job: J) -> &mut ScheduledTask;

    /// Run a console command on a schedule.
    fn command(&mut self, console: Arc<Console>, name: impl Into<String>) -> &mut ScheduledTask;

    /// Run a console command with arguments.
    fn command_with(
        &mut self,
        console: Arc<Console>,
        name: impl Into<String>,
        argv: impl IntoIterator<Item = impl Into<String>>,
    ) -> &mut ScheduledTask;
}

impl ScheduleExt for Schedule {
    fn job<J: Job + Clone>(&mut self, job: J) -> &mut ScheduledTask {
        self.add(JobTask::new(job))
    }

    fn command(&mut self, console: Arc<Console>, name: impl Into<String>) -> &mut ScheduledTask {
        self.add(CommandTask::new(console, name, Vec::new()))
    }

    fn command_with(
        &mut self,
        console: Arc<Console>,
        name: impl Into<String>,
        argv: impl IntoIterator<Item = impl Into<String>>,
    ) -> &mut ScheduledTask {
        self.add(CommandTask::new(console, name, argv.into_iter().map(Into::into).collect()))
    }
}

/// `schedule:run` — run whatever is due this minute, then exit.
///
/// What a system cron entry calls:
///
/// ```cron
/// * * * * * cd /srv/app && ./app schedule:run >> /dev/null 2>&1
/// ```
#[derive(Debug, Default)]
pub struct ScheduleRunCommand;

#[async_trait::async_trait]
impl Command for ScheduleRunCommand {
    fn name(&self) -> &str {
        "schedule:run"
    }

    fn description(&self) -> &str {
        "Run every scheduled task that is due"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  schedule:run\n\n\
             Runs the tasks due this minute and exits. Call it once a minute\n\
             from cron, a systemd timer, or a Kubernetes CronJob.\n\n\
             Exits non-zero if any task failed, so a supervisor notices.",
        )
    }

    async fn handle(&self, _args: &Arguments, app: &Application) -> Result<i32> {
        let schedule = app.resolve::<Schedule>()?;
        let locks = app.resolve::<LockManager>()?;

        refuse_a_broken_schedule(&schedule)?;
        assert_locks_are_shared(app)?;

        let summary = schedule.run_due(app, &locks, Utc::now()).await;

        if summary.considered() == 0 {
            println!("Nothing is due.");
            return Ok(exit::SUCCESS);
        }

        for name in &summary.ran {
            println!("Ran: {name}");
        }
        for (name, why) in &summary.skipped {
            println!("Skipped: {name} — {why}");
        }
        for (name, error) in &summary.failed {
            eprintln!("Failed: {name} — {error}");
        }

        Ok(if summary.has_failures() { exit::FAILURE } else { exit::SUCCESS })
    }
}

/// `schedule:work` — the same thing in a loop, for a container.
///
/// For a deployment with no cron: one process that wakes at the top of each
/// minute. Equivalent to `schedule:run` on a timer, and easier to supervise.
#[derive(Debug, Default)]
pub struct ScheduleWorkCommand;

#[async_trait::async_trait]
impl Command for ScheduleWorkCommand {
    fn name(&self) -> &str {
        "schedule:work"
    }

    fn description(&self) -> &str {
        "Run the scheduler in the foreground, once a minute"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  schedule:work [--once]\n\n\
             Options:\n  --once  Run the due tasks once and exit\n\n\
             For a deployment with no cron — a container, a systemd unit. One\n\
             process, supervised like any other.",
        )
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let schedule = app.resolve::<Schedule>()?;
        let locks = app.resolve::<LockManager>()?;

        refuse_a_broken_schedule(&schedule)?;
        assert_locks_are_shared(app)?;

        if args.flag("once") {
            let summary = schedule.run_due(app, &locks, Utc::now()).await;
            return Ok(if summary.has_failures() { exit::FAILURE } else { exit::SUCCESS });
        }

        println!("Scheduler running — {} task(s). Press Ctrl-C to stop.", schedule.len());

        loop {
            // Sleep to the top of the next minute rather than for sixty
            // seconds. Sleeping for a fixed interval drifts: each pass takes a
            // little longer than it slept, and after enough hours a task
            // scheduled for `:00` is running at `:01` and missing its minute
            // entirely.
            tokio::time::sleep(until_next_minute()).await;

            let summary = schedule.run_due(app, &locks, Utc::now()).await;
            if summary.considered() > 0 {
                tracing::info!(
                    ran = summary.ran.len(),
                    skipped = summary.skipped.len(),
                    failed = summary.failed.len(),
                    "scheduler pass"
                );
            }
        }
    }
}

/// `schedule:list` — what is scheduled, and when it next runs.
#[derive(Debug, Default)]
pub struct ScheduleListCommand;

#[async_trait::async_trait]
impl Command for ScheduleListCommand {
    fn name(&self) -> &str {
        "schedule:list"
    }

    fn description(&self) -> &str {
        "List every scheduled task and when it next runs"
    }

    async fn handle(&self, _args: &Arguments, app: &Application) -> Result<i32> {
        let schedule = app.resolve::<Schedule>()?;

        if schedule.is_empty() {
            println!("Nothing is scheduled.");
            return Ok(exit::SUCCESS);
        }

        let now = Utc::now();
        let rows: Vec<Vec<String>> = schedule
            .tasks()
            .iter()
            .map(|task| {
                let next = task
                    .next_run(now)
                    .map(|at| at.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "never".to_string());

                let mut guards = Vec::new();
                if task.overlap_ttl().is_some() {
                    guards.push("without-overlapping");
                }
                if task.is_one_server() {
                    guards.push("one-server");
                }

                vec![
                    task.description(),
                    task.expression().source().to_string(),
                    next,
                    guards.join(", "),
                ]
            })
            .collect();

        io::table(&["TASK", "SCHEDULE", "NEXT RUN (UTC)", "GUARDS"], &rows);

        // Both of these make a lock silently useless, so they are worth
        // printing even when nobody asked.
        for (name, error) in schedule.errors() {
            eprintln!("\nInvalid schedule for `{name}`: {error}");
        }
        for name in schedule.duplicate_names() {
            eprintln!(
                "\nTwo tasks are both called `{name}`. They share a lock, so \
                 `without_overlapping` between them makes each block the other."
            );
        }

        println!("\n{} task(s)", schedule.len());
        Ok(exit::SUCCESS)
    }
}

/// Fail rather than run a schedule with a task on the wrong expression.
///
/// The builders are lenient so a chain does not need a `?` in the middle of it;
/// this is where that leniency is paid back. A task quietly on `* * * * *`
/// because `.daily_at("3pm")` did not parse is worse than a scheduler that
/// refuses to start.
fn refuse_a_broken_schedule(schedule: &Schedule) -> Result<()> {
    let errors = schedule.errors();
    if errors.is_empty() {
        return Ok(());
    }

    let listed: Vec<String> =
        errors.iter().map(|(name, error)| format!("`{name}`: {error}")).collect();

    Err(Error::internal(format!(
        "the schedule has {} task(s) with an invalid expression — {}",
        errors.len(),
        listed.join("; ")
    )))
}

/// Refuse to run a schedule whose locking is decoration.
///
/// `without_overlapping` and `on_one_server` are only as shared as the cache
/// behind them. Over the in-process default, three machines each take their
/// own lock, each get it, and each run: the report goes out three times, the
/// backup runs three times, and nothing anywhere says why. The flags read as
/// if they are doing something, which is worse than not having them.
///
/// So `schedule:run` and `schedule:work` refuse in production, and warn
/// everywhere else. A developer running one process is not wrong to use the
/// memory cache; a production scheduler believing a guarantee it does not have
/// is a bug already shipped.
///
/// This is checked **here**, in the process that actually runs the schedule,
/// rather than at boot — a web container refusing to serve HTTP over a
/// scheduling concern would be a much larger outage than the one being
/// prevented. Boot still says so out loud: see
/// [`warn_if_locks_are_not_shared`].
pub fn assert_locks_are_shared(app: &Application) -> Result<()> {
    let Some(complaint) = lock_complaint(app)? else {
        return Ok(());
    };

    if app.is_production() {
        return Err(Error::internal(format!(
            "{complaint} In production this is refused rather than warned about: {ADVICE}."
        )));
    }

    tracing::warn!("{complaint} In production this refuses to run. To fix: {ADVICE}.");
    Ok(())
}

/// Say so at boot, in any environment, and carry on.
///
/// The counterpart to [`assert_locks_are_shared`]: every process gets told,
/// and only the one whose guarantees are at stake refuses. Silence is not an
/// option in either — a guard that fails open with no message is not a guard.
pub fn warn_if_locks_are_not_shared(app: &Application) {
    match lock_complaint(app) {
        Ok(Some(complaint)) => {
            // At `error` in production, because there it is a real one: the
            // scheduler in this deployment will refuse to run at all.
            if app.is_production() {
                tracing::error!(
                    "{complaint} `schedule:run` will refuse to start. To fix: {ADVICE}."
                );
            } else {
                tracing::warn!("{complaint} To fix: {ADVICE}.");
            }
        }
        Ok(None) => {}
        // Nothing is bound, so there is no schedule to be wrong about.
        Err(_) => {}
    }
}

/// What to do about it. One sentence, in one place, so the two messages above
/// cannot drift.
const ADVICE: &str = "set CACHE_DRIVER to a shared store (redis, redis-cluster, memcached, \
                      dynamodb) and hand the built store to `Rainier::with_cache`";

/// The complaint, or `None` when there is nothing to complain about.
fn lock_complaint(app: &Application) -> Result<Option<String>> {
    let schedule = app.resolve::<Schedule>()?;
    let locks = app.resolve::<LockManager>()?;

    let needs = schedule.tasks_needing_shared_locks();
    if needs.is_empty() || locks.is_shared() {
        return Ok(None);
    }

    Ok(Some(format!(
        "{} scheduled task(s) declare `without_overlapping` or `on_one_server`, but the lock is \
         in-process — every machine will take its own, get it, and run. Tasks: {}.",
        needs.len(),
        needs.join(", ")
    )))
}

/// How long until the top of the next minute.
fn until_next_minute() -> Duration {
    let now = Utc::now();
    let seconds = 60 - (now.timestamp() % 60);

    // At least a second, so a pass that finishes instantly cannot spin through
    // the same minute twice.
    Duration::from_secs(seconds.max(1) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;
    use rainier_console::Console;

    fn app_with(schedule: Schedule) -> Application {
        let app = Application::new(".");
        app.instance(schedule);
        app.instance(LockManager::new(Arc::new(MemoryCache::new())));
        app
    }

    #[tokio::test]
    async fn schedule_run_reports_what_it_did() {
        let mut schedule = Schedule::new();
        schedule.call("tick", |_| Box::pin(async { Ok(()) }));

        let app = app_with(schedule);
        let console = Console::new("rainier").register(ScheduleRunCommand);

        assert_eq!(console.run_argv(&app, ["schedule:run"]).await, exit::SUCCESS);
    }

    #[tokio::test]
    async fn schedule_run_exits_non_zero_when_a_task_fails() {
        // So a supervisor, or the `&&` in a deploy script, notices.
        let mut schedule = Schedule::new();
        schedule.call("broken", |_| Box::pin(async { Err(Error::internal("boom")) }));

        let app = app_with(schedule);
        let console = Console::new("rainier").register(ScheduleRunCommand);

        assert_eq!(console.run_argv(&app, ["schedule:run"]).await, exit::FAILURE);
    }

    #[tokio::test]
    async fn a_schedule_with_an_unparsable_expression_refuses_to_run() {
        // The builders are lenient mid-chain; this is where that is paid back.
        let mut schedule = Schedule::new();
        schedule.call("wrong", |_| Box::pin(async { Ok(()) })).cron("not a cron expression");

        let app = app_with(schedule);
        let console = Console::new("rainier").register(ScheduleRunCommand);

        assert_eq!(console.run_argv(&app, ["schedule:run"]).await, exit::FAILURE);
    }

    #[tokio::test]
    async fn schedule_list_names_a_duplicate() {
        let mut schedule = Schedule::new();
        schedule.call("prune", |_| Box::pin(async { Ok(()) })).hourly();
        schedule.call("prune", |_| Box::pin(async { Ok(()) })).daily();

        let app = app_with(schedule);
        let console = Console::new("rainier").register(ScheduleListCommand);

        // It prints rather than fails — a duplicate is a warning, not a reason
        // to stop the whole schedule.
        assert_eq!(console.run_argv(&app, ["schedule:list"]).await, exit::SUCCESS);
        assert_eq!(app.resolve::<Schedule>().unwrap().duplicate_names(), vec!["prune"]);
    }

    #[tokio::test]
    async fn schedule_work_once_runs_a_single_pass() {
        let mut schedule = Schedule::new();
        schedule.call("tick", |_| Box::pin(async { Ok(()) }));

        let app = app_with(schedule);
        let console = Console::new("rainier").register(ScheduleWorkCommand);

        assert_eq!(console.run_argv(&app, ["schedule:work", "--once"]).await, exit::SUCCESS);
    }

    #[test]
    fn the_sleep_lands_on_the_next_minute_rather_than_drifting() {
        // Sleeping a fixed sixty seconds drifts by however long each pass took,
        // and after enough hours a task scheduled for `:00` runs at `:01`.
        let wait = until_next_minute();

        assert!(wait.as_secs() >= 1 && wait.as_secs() <= 60, "{wait:?}");
    }

    #[test]
    fn a_broken_schedule_is_described_rather_than_just_refused() {
        let mut schedule = Schedule::new();
        schedule.call("wrong", |_| Box::pin(async { Ok(()) })).cron("* * *");

        let err = refuse_a_broken_schedule(&schedule).unwrap_err();
        assert!(err.message().contains("`wrong`"), "{}", err.message());
        assert!(err.message().contains("5"), "{}", err.message());
    }

    /// A schedule with one task that needs the lock to mean something.
    fn schedule_needing_shared_locks() -> Schedule {
        let mut schedule = Schedule::new();
        schedule.call("digest", |_| Box::pin(async { Ok(()) })).daily().on_one_server();
        schedule
    }

    #[test]
    fn an_unshared_lock_is_refused_in_production() {
        let app = app_with(schedule_needing_shared_locks());
        app.set_environment("production");

        let err = assert_locks_are_shared(&app).unwrap_err();

        // Which task, and what to do about it — a message naming neither is a
        // message nobody can act on.
        assert!(err.message().contains("digest"), "{}", err.message());
        assert!(err.message().contains("CACHE_DRIVER"), "{}", err.message());
    }

    #[tokio::test]
    async fn schedule_run_refuses_in_production_rather_than_running_everywhere_at_once() {
        // The command, not the boot: this is the process whose guarantees are
        // at stake, and the only one that should refuse over them.
        let mut schedule = Schedule::new();
        schedule.call("digest", |_| Box::pin(async { Ok(()) })).every_minute().on_one_server();

        let app = app_with(schedule);
        app.set_environment("production");
        let console = Console::new("rainier").register(ScheduleRunCommand);

        assert_eq!(console.run_argv(&app, ["schedule:run"]).await, exit::FAILURE);
    }

    #[test]
    fn the_boot_warning_never_stops_anything() {
        // A web container must not refuse to serve HTTP over a scheduling
        // concern — that would be a larger outage than the one prevented.
        let app = app_with(schedule_needing_shared_locks());

        for environment in ["production", "staging", "local"] {
            app.set_environment(environment);
            warn_if_locks_are_not_shared(&app);
        }
    }

    #[test]
    fn the_boot_warning_survives_an_application_with_no_schedule_bound() {
        // `warn_if_locks_are_not_shared` runs during every boot, including
        // ones that never bound a scheduler.
        warn_if_locks_are_not_shared(&Application::new("."));
    }

    #[test]
    fn an_unshared_lock_is_allowed_outside_production() {
        // Warned about — a developer running one process is not wrong — but
        // not refused, or no local schedule would ever boot.
        let app = app_with(schedule_needing_shared_locks());
        app.set_environment("local");

        assert!(assert_locks_are_shared(&app).is_ok());
    }

    #[test]
    fn a_shared_lock_passes_in_production() {
        let app = Application::new(".");
        app.instance(schedule_needing_shared_locks());
        app.instance(LockManager::new(Arc::new(MemoryCache::new())).declared_shared());
        app.set_environment("production");

        assert!(assert_locks_are_shared(&app).is_ok());
    }

    #[test]
    fn a_schedule_that_takes_no_locks_does_not_care() {
        // No `without_overlapping`, no `on_one_server`, nothing to guarantee.
        let mut schedule = Schedule::new();
        schedule.call("heartbeat", |_| Box::pin(async { Ok(()) })).every_minute();

        let app = app_with(schedule);
        app.set_environment("production");

        assert!(assert_locks_are_shared(&app).is_ok());
    }
}
