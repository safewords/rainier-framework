//! `lookout:prune`, `lookout:clear` and `lookout:status`.
//!
//! [`commands`] builds all three over the application's [`Store`] and
//! [`LookoutConfig`]; register them on the console:
//!
//! ```ignore
//! let mut console = Console::new("rainier");
//! for command in rainier_lookout::commands::commands(store.clone(), config.clone()) {
//!     console = console.register_arc(command);
//! }
//! ```
//!
//! # Pruning on a schedule
//!
//! Raw batches and aggregates are only removed when something prunes them.
//! Schedule `lookout:prune` like any console command — hourly keeps the store
//! close to the configured retention:
//!
//! ```ignore
//! schedule.command(console.clone(), "lookout:prune").hourly();
//! ```
//!
//! It is idempotent and deletes in bounded chunks (the store's contract), so a
//! run that overlaps the next is harmless.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{Duration, Utc};
use rainier_console::{exit, io, Arguments, Command};
use rainier_container::Application;
use rainier_support::Result;

use crate::config::LookoutConfig;
use crate::store::{BatchQuery, Store};

/// Every Lookout command, ready for
/// [`Console::register_arc`](rainier_console::Console::register_arc).
pub fn commands(store: Arc<dyn Store>, config: Arc<LookoutConfig>) -> Vec<Arc<dyn Command>> {
    vec![
        Arc::new(PruneCommand::new(Arc::clone(&store), Arc::clone(&config))),
        Arc::new(ClearCommand::new(Arc::clone(&store))),
        Arc::new(StatusCommand::new(store, config)),
    ]
}

/// Read a non-negative whole-number option, or say what was wrong.
fn number(args: &Arguments, name: &str, out: &mut String) -> std::result::Result<Option<u64>, ()> {
    match args.option(name) {
        None => Ok(None),
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) => Ok(Some(n)),
            Err(_) => {
                let _ = writeln!(out, "--{name} must be a whole number, like --{name}=48.");
                Err(())
            }
        },
    }
}

// --- lookout:prune -------------------------------------------------------------

/// `lookout:prune [--hours=N] [--days=N]` — delete what is past retention.
pub struct PruneCommand {
    store: Arc<dyn Store>,
    config: Arc<LookoutConfig>,
}

impl PruneCommand {
    /// Over `store`, with `config`'s retention as the default.
    pub fn new(store: Arc<dyn Store>, config: Arc<LookoutConfig>) -> Self {
        Self { store, config }
    }

    /// Run it, writing what it says to `out`.
    pub async fn execute(&self, args: &Arguments, out: &mut String) -> Result<i32> {
        let Ok(hours) = number(args, "hours", out) else { return Ok(exit::USAGE) };
        let Ok(days) = number(args, "days", out) else { return Ok(exit::USAGE) };
        let hours = hours.unwrap_or(self.config.retention.batches_hours);
        let days = days.unwrap_or(self.config.retention.aggregates_days);

        let now = Utc::now();
        // Clamped so an absurd override is "everything" rather than an
        // overflow.
        let batches_before = now - Duration::hours(hours.min(24 * 365 * 100) as i64);
        let aggregates_before = now - Duration::days(days.min(365 * 100) as i64);

        let pruned = self.store.prune(batches_before, aggregates_before).await?;
        let _ = writeln!(
            out,
            "Pruned {pruned} {} from the {} store: batches older than {hours}h (before {}), \
             aggregates older than {days}d (before {}).",
            if pruned == 1 { "batch" } else { "batches" },
            self.store.name(),
            batches_before.format("%Y-%m-%d %H:%M:%S UTC"),
            aggregates_before.format("%Y-%m-%d %H:%M:%S UTC"),
        );
        Ok(exit::SUCCESS)
    }
}

#[rainier_console::async_trait]
impl Command for PruneCommand {
    fn name(&self) -> &str {
        "lookout:prune"
    }

    fn description(&self) -> &str {
        "Delete Lookout batches and aggregates past their retention"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  lookout:prune [--hours=N] [--days=N]\n\n\
             Options:\n  --hours=N  Keep raw batches for N hours (default: retention.batches_hours)\n  \
             --days=N   Keep aggregates for N days (default: retention.aggregates_days)\n\n\
             Schedule it hourly so the store stays near its retention:\n  \
             schedule.command(console.clone(), \"lookout:prune\").hourly();",
        )
    }

    async fn handle(&self, args: &Arguments, _app: &Application) -> Result<i32> {
        let mut out = String::new();
        let code = self.execute(args, &mut out).await;
        print!("{out}");
        code
    }
}

// --- lookout:clear -------------------------------------------------------------

/// How [`ClearCommand::execute`] asks the operator a yes/no question.
pub type Ask = dyn Fn(&str) -> Result<bool> + Sync;

/// `lookout:clear [--force]` — delete everything Lookout has recorded.
///
/// Outside the local environment it asks first, and refuses when there is
/// nobody to ask, unless given `--force`.
pub struct ClearCommand {
    store: Arc<dyn Store>,
}

impl ClearCommand {
    /// Over `store`.
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    /// Run it in `environment`. `confirm` asks the operator; it is only
    /// called outside the local environment without `--force`, and `None`
    /// means there is nobody to ask.
    pub async fn execute(
        &self,
        args: &Arguments,
        environment: &str,
        confirm: Option<&Ask>,
        out: &mut String,
    ) -> Result<i32> {
        let local = crate::http::is_local(environment);
        if !local && !args.flag("force") {
            let question = format!("Delete everything Lookout has recorded in `{environment}`?");
            match confirm {
                Some(ask) => {
                    if !ask(&question)? {
                        let _ = writeln!(out, "Nothing was deleted.");
                        return Ok(exit::FAILURE);
                    }
                }
                None => {
                    let _ = writeln!(
                        out,
                        "Refusing to clear Lookout in `{environment}` without --force."
                    );
                    return Ok(exit::FAILURE);
                }
            }
        }
        self.store.clear().await?;
        let _ = writeln!(out, "Cleared the {} store.", self.store.name());
        Ok(exit::SUCCESS)
    }
}

#[rainier_console::async_trait]
impl Command for ClearCommand {
    fn name(&self) -> &str {
        "lookout:clear"
    }

    fn description(&self) -> &str {
        "Delete everything Lookout has recorded"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  lookout:clear [--force]\n\n\
             Options:\n  --force  Do not ask, outside the local environment",
        )
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let ask = |question: &str| io::confirm(question, false);
        let confirm: Option<&Ask> = if io::is_interactive() { Some(&ask) } else { None };
        let mut out = String::new();
        let code = self.execute(args, &app.environment(), confirm, &mut out).await;
        print!("{out}");
        code
    }
}

// --- lookout:status ------------------------------------------------------------

/// `lookout:status` — what Lookout is doing and where it keeps it.
pub struct StatusCommand {
    store: Arc<dyn Store>,
    config: Arc<LookoutConfig>,
}

/// How many recent batches `lookout:status` counts before saying "N+".
const COUNT_CAP: usize = 200;

impl StatusCommand {
    /// Over `store`, describing `config`.
    pub fn new(store: Arc<dyn Store>, config: Arc<LookoutConfig>) -> Self {
        Self { store, config }
    }

    /// Run it in `environment`, writing the report to `out`.
    pub async fn execute(&self, environment: &str, out: &mut String) -> Result<i32> {
        let status = crate::http::status_value(self.store.name(), &self.config, Some(environment));
        let config = &self.config;

        // One page of summaries — cheap on every store — rather than a count
        // the `Store` contract does not offer.
        let count = match self
            .store
            .batches(BatchQuery { limit: Some(COUNT_CAP), ..BatchQuery::default() })
            .await
        {
            Ok(found) if found.len() >= COUNT_CAP => format!("{COUNT_CAP}+"),
            Ok(found) => found.len().to_string(),
            Err(e) => format!("unavailable ({e})"),
        };

        let rows = vec![
            vec!["Enabled".to_string(), if config.enabled { "yes" } else { "no" }.to_string()],
            vec!["Environment".to_string(), environment.to_string()],
            vec![
                "Store".to_string(),
                format!("{} ({})", self.store.name(), status["store_kind"].as_str().unwrap_or("?")),
            ],
            vec!["Batches kept".to_string(), count],
            vec![
                "Sample rates".to_string(),
                format!(
                    "requests {} · jobs {} · commands {}",
                    config.request_sample_rate, config.job_sample_rate, config.command_sample_rate
                ),
            ],
            vec![
                "Retention".to_string(),
                format!(
                    "batches {}h · aggregates {}d",
                    config.retention.batches_hours, config.retention.aggregates_days
                ),
            ],
            vec![
                "Dashboard".to_string(),
                if config.dashboard.enabled {
                    config.dashboard.path.clone()
                } else {
                    "off".to_string()
                },
            ],
        ];
        out.push_str(&io::table_to_string(&["Setting", "Value"], &rows));

        let watchers: Vec<Vec<String>> = crate::entry::EntryType::ALL
            .iter()
            .map(|&t| {
                let w = config.watcher(t);
                vec![
                    t.as_str().to_string(),
                    if w.enabled { "on" } else { "off" }.to_string(),
                    w.sample_rate.to_string(),
                    w.slow_ms.map(|ms| format!("{ms}ms")).unwrap_or_else(|| "—".to_string()),
                ]
            })
            .collect();
        out.push('\n');
        out.push_str(&io::table_to_string(&["Watcher", "State", "Sample", "Slow"], &watchers));
        Ok(exit::SUCCESS)
    }
}

#[rainier_console::async_trait]
impl Command for StatusCommand {
    fn name(&self) -> &str {
        "lookout:status"
    }

    fn description(&self) -> &str {
        "Show Lookout's store, watchers, sampling and retention"
    }

    async fn handle(&self, _args: &Arguments, app: &Application) -> Result<i32> {
        let mut out = String::new();
        let code = self.execute(&app.environment(), &mut out).await;
        print!("{out}");
        code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::{request, FakeStore};

    fn args(argv: &[&str]) -> Arguments {
        Arguments::parse(argv.iter().copied())
    }

    fn store_with_ages() -> Arc<FakeStore> {
        Arc::new(FakeStore::with(vec![
            request("old", "posts.index", 200, 3 * 3600, vec![]),
            request("new", "posts.index", 200, 60, vec![]),
        ]))
    }

    #[tokio::test]
    async fn prune_uses_the_configured_retention() {
        let store = store_with_ages();
        let mut config = LookoutConfig::default();
        config.retention.batches_hours = 2;
        let command = PruneCommand::new(store.clone(), Arc::new(config));
        let mut out = String::new();
        let code = command.execute(&args(&["lookout:prune"]), &mut out).await.unwrap();
        assert_eq!(code, exit::SUCCESS);
        assert!(out.starts_with("Pruned 1 batch from the fake store"), "{out}");
        assert!(out.contains("older than 2h") && out.contains("older than 30d"), "{out}");
        assert_eq!(store.batches.lock().unwrap().len(), 1);
        let (b, a) = store.pruned_with.lock().unwrap().unwrap();
        let hours = (Utc::now() - b).num_minutes();
        assert!((119..=121).contains(&hours), "{hours}");
        assert!((Utc::now() - a).num_days() == 30);
    }

    #[tokio::test]
    async fn prune_takes_overrides_and_rejects_nonsense() {
        let store = store_with_ages();
        let command = PruneCommand::new(store.clone(), Arc::new(LookoutConfig::default()));
        let mut out = String::new();
        let code = command
            .execute(&args(&["lookout:prune", "--hours=0", "--days=7"]), &mut out)
            .await
            .unwrap();
        assert_eq!(code, exit::SUCCESS);
        assert!(out.starts_with("Pruned 2 batches"), "{out}");
        assert!(out.contains("older than 0h") && out.contains("older than 7d"), "{out}");

        let mut out = String::new();
        let code =
            command.execute(&args(&["lookout:prune", "--hours=soon"]), &mut out).await.unwrap();
        assert_eq!(code, exit::USAGE);
        assert!(out.contains("--hours must be a whole number"), "{out}");
    }

    #[tokio::test]
    async fn clear_runs_locally_without_asking() {
        let store = store_with_ages();
        let command = ClearCommand::new(store.clone());
        let mut out = String::new();
        let code =
            command.execute(&args(&["lookout:clear"]), "local", None, &mut out).await.unwrap();
        assert_eq!(code, exit::SUCCESS);
        assert_eq!(out.trim(), "Cleared the fake store.");
        assert!(store.batches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn clear_elsewhere_needs_force_or_a_yes() {
        let store = store_with_ages();
        let command = ClearCommand::new(store.clone());

        let mut out = String::new();
        let code =
            command.execute(&args(&["lookout:clear"]), "production", None, &mut out).await.unwrap();
        assert_eq!(code, exit::FAILURE);
        assert!(out.contains("without --force"), "{out}");
        assert_eq!(store.batches.lock().unwrap().len(), 2);

        let no = |_: &str| Ok(false);
        let mut out = String::new();
        let code = command
            .execute(&args(&["lookout:clear"]), "production", Some(&no), &mut out)
            .await
            .unwrap();
        assert_eq!(code, exit::FAILURE);
        assert_eq!(store.batches.lock().unwrap().len(), 2);

        let yes = |q: &str| {
            assert!(q.contains("`production`"));
            Ok(true)
        };
        let mut out = String::new();
        let code = command
            .execute(&args(&["lookout:clear"]), "production", Some(&yes), &mut out)
            .await
            .unwrap();
        assert_eq!(code, exit::SUCCESS);
        assert!(store.batches.lock().unwrap().is_empty());

        let store = store_with_ages();
        let command = ClearCommand::new(store.clone());
        let mut out = String::new();
        let code = command
            .execute(&args(&["lookout:clear", "--force"]), "production", None, &mut out)
            .await
            .unwrap();
        assert_eq!(code, exit::SUCCESS);
        assert!(store.batches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_reports_store_sampling_retention_and_watchers() {
        let store = store_with_ages();
        let config = LookoutConfig::default().enabled(true).disable(crate::entry::EntryType::Log);
        let command = StatusCommand::new(store, Arc::new(config));
        let mut out = String::new();
        let code = command.execute("staging", &mut out).await.unwrap();
        assert_eq!(code, exit::SUCCESS);
        for needle in [
            "Enabled",
            "yes",
            "staging",
            "fake (memory)",
            "Batches kept",
            "requests 1 · jobs 1 · commands 1",
            "batches 24h · aggregates 30d",
            "/_lookout",
            "query",
            "100ms",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in\n{out}");
        }
        let log_row = out.lines().find(|l| l.contains("| log ")).unwrap();
        assert!(log_row.contains("off"), "{log_row}");
        let kept = out.lines().find(|l| l.contains("Batches kept")).unwrap();
        assert!(kept.contains(" 2 "), "{kept}");
    }

    #[test]
    fn exposes_three_commands() {
        let store: Arc<dyn Store> = Arc::new(FakeStore::default());
        let names: Vec<String> = commands(store, Arc::new(LookoutConfig::default()))
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        assert_eq!(names, ["lookout:prune", "lookout:clear", "lookout:status"]);
    }
}
