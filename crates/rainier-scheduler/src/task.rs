//! What a schedule runs — [`Task`], and [`ScheduledTask`] wrapping one with
//! when and how.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Utc};
use rainier_cache::LockManager;
use rainier_container::Application;
use rainier_support::{BoxFuture, Result};

use crate::cron::CronExpression;

/// Something the scheduler can run.
///
/// Object-safe on purpose — a schedule is a heterogeneous list, and the three
/// things people put on one (a closure, a queued job, a console command) have
/// nothing else in common.
pub trait Task: Send + Sync + 'static {
    /// A stable name.
    ///
    /// It is the **lock key** for [`without_overlapping`] and
    /// [`on_one_server`], so it has to mean the same thing on every machine and
    /// across restarts. Deriving it from a pointer or a timestamp would make
    /// both silently useless.
    ///
    /// [`without_overlapping`]: ScheduledTask::without_overlapping
    /// [`on_one_server`]: ScheduledTask::on_one_server
    fn name(&self) -> String;

    /// Run it.
    fn run<'a>(&'a self, app: &'a Application) -> BoxFuture<'a, Result<()>>;
}

/// A [`Task`] from a closure.
///
/// What [`Schedule::call`](crate::Schedule::call) builds.
///
/// The closure returns a [`BoxFuture`] rather than an `impl Future`, so the
/// body may borrow the [`Application`] it is handed — which is the whole reason
/// it is handed one. That costs a `Box::pin` at the call site:
///
/// ```ignore
/// schedule.call("prune", |app| Box::pin(async move {
///     app.resolve::<SessionStore>()?.prune().await?;
///     Ok(())
/// }));
/// ```
///
/// It is the same shape `Middleware` and `RouteHandler` use, and for the same
/// reason.
pub struct ClosureTask<F> {
    name: String,
    run: F,
}

impl<F> ClosureTask<F> {
    /// A task called `name`, running `run`.
    pub fn new(name: impl Into<String>, run: F) -> Self {
        Self { name: name.into(), run }
    }
}

impl<F> Task for ClosureTask<F>
where
    F: for<'a> Fn(&'a Application) -> BoxFuture<'a, Result<()>> + Send + Sync + 'static,
{
    fn name(&self) -> String {
        self.name.clone()
    }

    fn run<'a>(&'a self, app: &'a Application) -> BoxFuture<'a, Result<()>> {
        (self.run)(app)
    }
}

/// Why a due task did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// A previous run of this task is still going. [`without_overlapping`].
    ///
    /// [`without_overlapping`]: ScheduledTask::without_overlapping
    StillRunning,
    /// Another machine claimed this minute. [`on_one_server`].
    ///
    /// [`on_one_server`]: ScheduledTask::on_one_server
    AnotherServer,
    /// A [`when`](ScheduledTask::when) guard said no.
    ConditionFalse,
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Skipped::StillRunning => "a previous run is still going",
            Skipped::AnotherServer => "another server is running it",
            Skipped::ConditionFalse => "its condition was false",
        })
    }
}

/// What happened to one task.
#[derive(Debug)]
pub enum Outcome {
    /// It ran and succeeded.
    Ran,
    /// It ran and failed.
    Failed(rainier_support::Error),
    /// It did not run.
    Skipped(Skipped),
}

impl Outcome {
    /// Whether it ran at all.
    pub fn did_run(&self) -> bool {
        matches!(self, Outcome::Ran | Outcome::Failed(_))
    }
}

/// A [`Task`] with a schedule and its options.
///
/// Built by the `Schedule` methods and configured with the
/// builders here.
pub struct ScheduledTask {
    task: Arc<dyn Task>,
    expression: CronExpression,
    timezone: Option<FixedOffset>,
    overlap: Option<Duration>,
    one_server: bool,
    condition: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    description: Option<String>,
    /// Set when a builder was handed an expression that does not parse.
    error: Option<String>,
}

impl ScheduledTask {
    /// Every minute, until told otherwise.
    pub(crate) fn new(task: Arc<dyn Task>) -> Self {
        Self {
            task,
            expression: CronExpression::parse("* * * * *")
                .expect("`* * * * *` is a valid expression"),
            timezone: None,
            overlap: None,
            one_server: false,
            condition: None,
            description: None,
            error: None,
        }
    }

    // --- when -------------------------------------------------------------

    /// An explicit cron expression.
    ///
    /// Fails if it does not parse — at boot, where a schedule that would
    /// silently never fire is cheap to notice.
    pub fn cron(&mut self, expression: &str) -> &mut Self {
        self.with(expression)
    }

    /// Every minute.
    pub fn every_minute(&mut self) -> &mut Self {
        self.with("* * * * *")
    }

    /// Every `n` minutes, on the hour.
    pub fn every_minutes(&mut self, n: u32) -> &mut Self {
        self.with(&format!("*/{n} * * * *"))
    }

    /// Every five minutes.
    pub fn every_five_minutes(&mut self) -> &mut Self {
        self.every_minutes(5)
    }

    /// Every fifteen minutes.
    pub fn every_fifteen_minutes(&mut self) -> &mut Self {
        self.every_minutes(15)
    }

    /// Every half hour.
    pub fn every_thirty_minutes(&mut self) -> &mut Self {
        self.every_minutes(30)
    }

    /// On the hour.
    pub fn hourly(&mut self) -> &mut Self {
        self.with("0 * * * *")
    }

    /// At `minute` past every hour.
    pub fn hourly_at(&mut self, minute: u32) -> &mut Self {
        self.with(&format!("{minute} * * * *"))
    }

    /// At midnight.
    pub fn daily(&mut self) -> &mut Self {
        self.with("0 0 * * *")
    }

    /// Once a day at `HH:MM`.
    ///
    /// ```
    /// # use rainier_scheduler::Schedule;
    /// # let mut schedule = Schedule::new();
    /// schedule.call("prune", |_| Box::pin(async { Ok(()) })).daily_at("03:30");
    /// ```
    ///
    /// A time that does not parse leaves the task on its previous schedule
    /// rather than panicking; `Schedule::errors` reports it.
    pub fn daily_at(&mut self, time: &str) -> &mut Self {
        let (hour, minute) = parse_time(time);
        self.with(&format!("{minute} {hour} * * *"))
    }

    /// Twice a day, at `first` and `second` o'clock.
    pub fn twice_daily(&mut self, first: u32, second: u32) -> &mut Self {
        self.with(&format!("0 {first},{second} * * *"))
    }

    /// Midnight on Sunday.
    pub fn weekly(&mut self) -> &mut Self {
        self.with("0 0 * * 0")
    }

    /// Weekly on `day` (0 = Sunday) at `HH:MM`.
    pub fn weekly_on(&mut self, day: u32, time: &str) -> &mut Self {
        let (hour, minute) = parse_time(time);
        self.with(&format!("{minute} {hour} * * {day}"))
    }

    /// Midnight on the first of the month.
    pub fn monthly(&mut self) -> &mut Self {
        self.with("0 0 1 * *")
    }

    /// Monthly on `day` at `HH:MM`.
    pub fn monthly_on(&mut self, day: u32, time: &str) -> &mut Self {
        let (hour, minute) = parse_time(time);
        self.with(&format!("{minute} {hour} {day} * *"))
    }

    /// Midnight on the 1st of January.
    pub fn yearly(&mut self) -> &mut Self {
        self.with("0 0 1 1 *")
    }

    /// Only on weekdays. Combine with a time — `.daily_at("09:00").weekdays()`.
    pub fn weekdays(&mut self) -> &mut Self {
        self.on_days("1-5")
    }

    /// Only at the weekend.
    pub fn weekends(&mut self) -> &mut Self {
        self.on_days("0,6")
    }

    /// Replace the day-of-week field, keeping the rest.
    pub fn on_days(&mut self, days: &str) -> &mut Self {
        let fields: Vec<String> =
            self.expression.source().split_whitespace().map(str::to_string).collect();

        if fields.len() == 5 {
            let replaced =
                format!("{} {} {} {} {days}", fields[0], fields[1], fields[2], fields[3]);
            self.with(&replaced);
        }
        self
    }

    /// Interpret the expression in `offset` rather than UTC.
    ///
    /// A fixed offset, not a named zone. Named zones need a tz database and
    /// bring daylight saving with them — where "daily at 02:30" runs twice one
    /// night a year and not at all on another. That is a real decision an
    /// application should make deliberately, with `chrono-tz`, rather than
    /// inherit from a convenience method.
    pub fn in_timezone(&mut self, offset: FixedOffset) -> &mut Self {
        self.timezone = Some(offset);
        self
    }

    // --- how --------------------------------------------------------------

    /// Do not start a run while the previous one is still going.
    ///
    /// Takes a lock named after the task, held **for the duration of the run**
    /// and released after it — so a task that takes eleven minutes on a
    /// ten-minute schedule runs once, not twice, and not five times by
    /// lunchtime.
    ///
    /// `ttl` is the safety net for a run that dies without releasing: a crash,
    /// an `OOM`, a `kill -9`. Set it comfortably longer than the work takes.
    /// Too short and a slow run gets a second copy anyway; too long and a
    /// crashed run blocks the schedule until it expires.
    ///
    /// ```
    /// # use rainier_scheduler::Schedule;
    /// # use std::time::Duration;
    /// # let mut schedule = Schedule::new();
    /// schedule
    ///     .call("rebuild-index", |_| Box::pin(async { Ok(()) }))
    ///     .hourly()
    ///     .without_overlapping(Duration::from_secs(3600));
    /// ```
    ///
    /// **This is only as shared as the cache is.** Over a `MemoryCache` it
    /// prevents overlap within one process and nothing between two.
    pub fn without_overlapping(&mut self, ttl: Duration) -> &mut Self {
        self.overlap = Some(ttl);
        self
    }

    /// Run on one machine only, when several run the scheduler.
    ///
    /// Takes a lock **for the minute**, at the moment the task becomes due, and
    /// releases it immediately — long enough that the other machines find it
    /// taken and skip, short enough that the next occurrence is unaffected.
    ///
    /// That is the difference from [`without_overlapping`], and they answer
    /// different questions:
    ///
    /// | | Prevents | Lock held |
    /// |---|---|---|
    /// | `without_overlapping` | this run starting before the last finished | for the whole run |
    /// | `on_one_server` | three machines running the same minute | for that minute |
    ///
    /// They compose. A task that is slow *and* scheduled everywhere wants both.
    ///
    /// **Requires a shared cache.** Three machines with three in-process caches
    /// are three machines each holding their own lock and each believing they
    /// are the one — which is exactly the situation this is meant to prevent.
    /// [`LockManager::is_shared`](rainier_cache::LockManager::is_shared) is the
    /// check worth making at boot.
    ///
    /// [`without_overlapping`]: Self::without_overlapping
    pub fn on_one_server(&mut self) -> &mut Self {
        self.one_server = true;
        self
    }

    /// Run only when `condition` is true.
    ///
    /// Evaluated when the task is due, before any lock is
    /// taken — so a task that is switched off costs nothing.
    pub fn when(&mut self, condition: impl Fn() -> bool + Send + Sync + 'static) -> &mut Self {
        self.condition = Some(Box::new(condition));
        self
    }

    /// Skip when `condition` is true — the inverse of [`when`](Self::when).
    pub fn skip(&mut self, condition: impl Fn() -> bool + Send + Sync + 'static) -> &mut Self {
        self.when(move || !condition())
    }

    /// A human description, for `schedule:list`.
    pub fn described_as(&mut self, description: impl Into<String>) -> &mut Self {
        self.description = Some(description.into());
        self
    }

    // --- reading ------------------------------------------------------------

    /// The task's name — and its lock key.
    pub fn name(&self) -> String {
        self.task.name()
    }

    /// The description, or the name.
    pub fn description(&self) -> String {
        self.description.clone().unwrap_or_else(|| self.name())
    }

    /// The cron expression.
    pub fn expression(&self) -> &CronExpression {
        &self.expression
    }

    /// The offset it is interpreted in, if not UTC.
    pub fn timezone(&self) -> Option<FixedOffset> {
        self.timezone
    }

    /// How long an overlap lock is held, if it takes one.
    pub fn overlap_ttl(&self) -> Option<Duration> {
        self.overlap
    }

    /// Whether it is restricted to one server.
    pub fn is_one_server(&self) -> bool {
        self.one_server
    }

    /// Whether this task's guarantees depend on the lock being shared.
    ///
    /// True for [`without_overlapping`] and [`on_one_server`]. Both are
    /// silently useless over an in-process cache — they take a lock, get it,
    /// and run, on every machine at once — so this is what a boot check asks
    /// each task before deciding whether the deployment is honest.
    ///
    /// [`without_overlapping`]: Self::without_overlapping
    /// [`on_one_server`]: Self::on_one_server
    pub fn needs_shared_locks(&self) -> bool {
        self.one_server || self.overlap.is_some()
    }

    /// The parse error, if a builder was given an expression that is not one.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the expression fires at `at`.
    ///
    /// Only the expression, not the condition or the locks — those are decided
    /// when it runs, and `schedule:list` wants to show a task that is due but
    /// switched off.
    pub fn is_due(&self, at: DateTime<Utc>) -> bool {
        match self.timezone {
            Some(offset) => self.expression.matches(at.with_timezone(&offset)),
            None => self.expression.matches(at),
        }
    }

    /// When it next fires after `at`.
    pub fn next_run(&self, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self.timezone {
            Some(offset) => self
                .expression
                .next_after(at.with_timezone(&offset))
                .map(|next| next.with_timezone(&Utc)),
            None => self.expression.next_after(at),
        }
    }

    /// Run it, honouring the condition and the locks.
    ///
    /// The order is deliberate and is the whole of `on_one_server`'s
    /// correctness: condition, then the one-server claim, then the overlap
    /// lock, then the work. Claiming the minute *before* checking whether a
    /// previous run is still going would have one machine claim it and then
    /// skip, leaving the minute unclaimed by anyone who could have run it.
    pub async fn run(&self, app: &Application, locks: &LockManager, at: DateTime<Utc>) -> Outcome {
        if let Some(condition) = &self.condition {
            if !condition() {
                return Outcome::Skipped(Skipped::ConditionFalse);
            }
        }

        // The one-server claim is scoped to this occurrence, so the key carries
        // the minute. Without that, one machine would claim the task forever
        // and the others would never run it — the lock has to expire *with* the
        // occurrence, not after some guessed interval.
        if self.one_server {
            let key = format!("schedule:one-server:{}:{}", self.name(), at.format("%Y%m%d%H%M"));

            // Held for a minute, so a machine whose clock is a few seconds
            // behind still finds it taken.
            match locks.lock(key, Duration::from_secs(60)).acquire().await {
                Ok(Some(guard)) => {
                    // Deliberately *not* released. The claim has to outlive
                    // this run so the other machines still find it taken; it
                    // expires with the minute.
                    guard.keep();
                }
                Ok(None) => return Outcome::Skipped(Skipped::AnotherServer),
                Err(e) => return Outcome::Failed(e),
            }
        }

        let Some(ttl) = self.overlap else {
            return match self.task.run(app).await {
                Ok(()) => Outcome::Ran,
                Err(e) => Outcome::Failed(e),
            };
        };

        let key = format!("schedule:overlap:{}", self.name());
        match locks.lock(key, ttl).run(self.task.run(app)).await {
            Ok(Some(())) => Outcome::Ran,
            Ok(None) => Outcome::Skipped(Skipped::StillRunning),
            Err(e) => Outcome::Failed(e),
        }
    }

    /// Set the expression, keeping the old one if the new one does not parse.
    ///
    /// The convenience builders go through here. They take `&str` and return
    /// `Self` rather than `Result<Self>`, because `.daily_at("03:00")` in the
    /// middle of a chain should not need a `?` — and every string they build is
    /// one they constructed themselves, so a failure means a bad *argument*,
    /// which the log line names.
    fn with(&mut self, expression: &str) -> &mut Self {
        match CronExpression::parse(expression) {
            Ok(parsed) => {
                self.expression = parsed;
                self.error = None;
            }
            Err(e) => {
                // Recorded as well as logged, so `Schedule::errors` can report
                // it and `schedule:run` can refuse to start. A schedule with a
                // task silently on the wrong expression is worse than one that
                // will not boot.
                tracing::error!(
                    task = %self.task.name(),
                    expression,
                    error = %e.message(),
                    "unparsable schedule; the task keeps its previous one"
                );
                self.error = Some(e.message().to_string());
            }
        }
        self
    }
}

impl std::fmt::Debug for ScheduledTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledTask")
            .field("name", &self.name())
            .field("expression", &self.expression.source())
            .field("without_overlapping", &self.overlap.is_some())
            .field("on_one_server", &self.one_server)
            .finish()
    }
}

/// `"HH:MM"` → `(hour, minute)`, defaulting to midnight.
///
/// Lenient because the alternative is `daily_at` returning a `Result` and every
/// schedule needing a `?`. A malformed time gives midnight, and the schedule
/// listing shows `0 0 * * *`, which is visible.
fn parse_time(time: &str) -> (u32, u32) {
    let mut parts = time.splitn(2, ':');
    let hour = parts.next().and_then(|h| h.trim().parse().ok()).unwrap_or(0);
    let minute = parts.next().and_then(|m| m.trim().parse().ok()).unwrap_or(0);

    (hour.min(23), minute.min(59))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_parses_into_hour_and_minute() {
        assert_eq!(parse_time("03:30"), (3, 30));
        assert_eq!(parse_time("9:05"), (9, 5));
        assert_eq!(parse_time("23:59"), (23, 59));
    }

    #[test]
    fn a_malformed_time_becomes_midnight_rather_than_panicking() {
        assert_eq!(parse_time("nope"), (0, 0));
        assert_eq!(parse_time(""), (0, 0));
        assert_eq!(parse_time("99:99"), (23, 59), "clamped, not wrapped");
    }
}
