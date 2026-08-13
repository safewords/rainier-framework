//! The commands Rainier ships — `route:list`, `serve`, `migrate`,
//! `queue:work`, `key:generate`.
//!
//! Each resolves what it needs from the container, so registering one is a
//! single line and it works against whatever the application bound.

use std::sync::Arc;
use std::time::Duration;

use rainier_console::{exit, io, Arguments, Command};
use rainier_container::Application;
use rainier_database::{Database, Migrator};
use rainier_events::Dispatcher;
use rainier_queue::{QueueManager, Worker, WorkerOptions};
use rainier_routing::CompiledRouter;
use rainier_server::{Kernel, Server, ServerOptions};
use rainier_support::Result;

/// `route:list` — print the route table.
#[derive(Debug, Default)]
pub struct RouteListCommand;

#[async_trait::async_trait]
impl Command for RouteListCommand {
    fn name(&self) -> &str {
        "route:list"
    }

    fn description(&self) -> &str {
        "List every registered route"
    }

    fn help(&self) -> Option<&str> {
        Some("Usage:\n  route:list [--json]\n\nOptions:\n  --json  Emit the table as JSON")
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let router = app.resolve::<CompiledRouter>()?;
        let rows = router.describe();

        if args.flag("json") {
            let payload: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "methods": row.methods,
                        "uri": row.uri,
                        "name": row.name,
                        "middleware": row.middleware,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(exit::SUCCESS);
        }

        if rows.is_empty() {
            println!("No routes are registered.");
            return Ok(exit::SUCCESS);
        }

        io::table(
            &["METHOD", "URI", "NAME", "MIDDLEWARE"],
            &rows
                .iter()
                .map(|row| {
                    vec![
                        row.methods.join("|"),
                        row.uri.clone(),
                        row.name.clone().unwrap_or_default(),
                        row.middleware.join(", "),
                    ]
                })
                .collect::<Vec<_>>(),
        );
        println!("{} route(s)", rows.len());
        Ok(exit::SUCCESS)
    }
}

/// `serve` — run the HTTP server.
#[derive(Debug, Default)]
pub struct ServeCommand;

#[async_trait::async_trait]
impl Command for ServeCommand {
    fn name(&self) -> &str {
        "serve"
    }

    fn description(&self) -> &str {
        "Start the HTTP server"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  serve [--host=127.0.0.1] [--port=8000]\n\n\
             Defaults come from `server.host` and `server.port` in the config.",
        )
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let config = app.resolve::<rainier_config::Config>()?;
        let kernel = app.resolve::<Kernel>()?;

        let host = args
            .option("host")
            .map(str::to_string)
            .unwrap_or_else(|| config.get_or("server.host", "127.0.0.1".to_string()));
        let port: u16 = args.parsed_or("port", config.get_or("server.port", 8000u16));
        let max_body: usize = config.get_or("server.max_body_bytes", 2 * 1024 * 1024usize);

        let options = ServerOptions::default()
            .bind_to(&host, port)?
            .max_body_bytes(max_body)
            .trust_forwarded_for(config.get_or("server.trust_proxy", false));

        let mut server = Server::from_arc(kernel).with_options(options);

        // Each connection is served in a spawned task, and a spawned task
        // inherits no facade scope — so without this a handler resolves
        // through whatever is installed process-wide. In a normal boot that is
        // this same application and nothing changes; it matters for a second
        // server in the same process, and for a test that boots its own.
        if let Some(application) = rainier_container::try_facade_application() {
            server = server.for_application(application);
        }

        // Sockets share the listener, so this is the whole of "run them
        // concurrently" — an upgrade is a request the same accept loop takes.
        if let Ok(sockets) = app.resolve::<rainier_websocket::WebSocketRoutes>() {
            if !sockets.is_empty() {
                println!("  websockets: {}", sockets.patterns().join(", "));
                server = server.with_websockets(sockets);
            }
        }

        println!("Rainier is serving http://{host}:{port} — press Ctrl-C to stop");
        server.run().await?;

        // Terminating hooks run once the server has stopped accepting, which
        // is where an application flushes whatever it deferred.
        app.terminate();
        Ok(exit::SUCCESS)
    }
}

/// `migrate` — run pending migrations.
#[derive(Debug, Default)]
pub struct MigrateCommand;

#[async_trait::async_trait]
impl Command for MigrateCommand {
    fn name(&self) -> &str {
        "migrate"
    }

    fn description(&self) -> &str {
        "Run any migrations that have not been applied"
    }

    fn help(&self) -> Option<&str> {
        Some("Usage:\n  migrate [--pretend]\n\nOptions:\n  --pretend  List what would run")
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let database = app.resolve::<Database>()?;
        let migrator = app.resolve::<Migrator>()?;

        if args.flag("pretend") {
            let pending = migrator.pending(&database).await?;

            if pending.is_empty() {
                println!("Nothing to migrate.");
            } else {
                println!("Would run:");
                for name in pending {
                    println!("  {name}");
                }
            }
            return Ok(exit::SUCCESS);
        }

        let ran = migrator.run(&database).await?;
        if ran.is_empty() {
            println!("Nothing to migrate.");
        } else {
            for name in &ran {
                println!("Migrated: {name}");
            }
            println!("\n{} migration(s) applied.", ran.len());

            // Worth knowing at the moment you deploy, not at the moment you
            // need to go back.
            let irreversible = migrator.irreversible(database.dialect());
            let stuck: Vec<&&str> =
                irreversible.iter().filter(|name| ran.iter().any(|r| r == *name)).collect();
            if !stuck.is_empty() {
                println!("\nNot reversible, so `migrate:rollback` will refuse this batch:");
                for name in stuck {
                    println!("  {name}");
                }
            }
        }
        Ok(exit::SUCCESS)
    }
}

/// `migrate:rollback` — undo the most recent batch of migrations.
#[derive(Debug, Default)]
pub struct MigrateRollbackCommand;

#[async_trait::async_trait]
impl Command for MigrateRollbackCommand {
    fn name(&self) -> &str {
        "migrate:rollback"
    }

    fn description(&self) -> &str {
        "Undo the last batch of migrations"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  migrate:rollback [--batches=1] [--pretend]\n\n\
             Options:\n  \
             --batches=N  How many batches to undo (default 1)\n  \
             --pretend    List what would be undone\n\n\
             A batch is everything one `migrate` applied. Steps are undone in\n\
             reverse, and the whole rollback is refused up front if any step in\n\
             range declared itself irreversible.",
        )
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let database = app.resolve::<Database>()?;
        let migrator = app.resolve::<Migrator>()?;
        let batches: u32 = args.parsed_or("batches", 1);

        if args.flag("pretend") {
            let targets = migrator.rollback_targets(&database, batches).await?;
            if targets.is_empty() {
                println!("Nothing to roll back.");
                return Ok(exit::SUCCESS);
            }

            // A step in range that cannot be undone refuses the *whole*
            // rollback, so `--pretend` has to say which one — otherwise it
            // reports a plan the real run will decline to carry out.
            let irreversible = migrator.irreversible(database.dialect());

            println!("Would undo, in this order:");
            for name in &targets {
                let blocked = irreversible.contains(&name.as_str());
                println!("  {name}{}", if blocked { "   (NOT REVERSIBLE)" } else { "" });
            }

            let blocking: Vec<&String> =
                targets.iter().filter(|name| irreversible.contains(&name.as_str())).collect();
            if !blocking.is_empty() {
                println!(
                    "
{} step(s) in this range cannot be undone, so the rollback will refuse                      the whole batch rather than half-apply it.",
                    blocking.len()
                );
            }

            return Ok(exit::SUCCESS);
        }

        let rolled_back = migrator.rollback(&database, batches).await?;
        if rolled_back.is_empty() {
            println!("Nothing to roll back.");
        } else {
            for name in &rolled_back {
                println!("Rolled back: {name}");
            }
            println!("\n{} migration(s) rolled back.", rolled_back.len());
        }
        Ok(exit::SUCCESS)
    }
}

/// `queue:work` — process queued jobs.
#[derive(Debug, Default)]
pub struct QueueWorkCommand;

#[async_trait::async_trait]
impl Command for QueueWorkCommand {
    fn name(&self) -> &str {
        "queue:work"
    }

    fn description(&self) -> &str {
        "Process jobs from the queue"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  queue:work [--queue=default,high] [--once] [--max-jobs=N] [--sleep=1]\n\n\
             Options:\n  \
             --queue     Comma-separated queues, in priority order.\n              Defaults to the queues the application declared.\n  \
             --once      Process what is waiting, then stop\n  \
             --max-jobs  Stop after N jobs (a worker that recycles)\n  \
             --sleep     Seconds to wait when the queue is empty",
        )
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let manager = app.resolve::<QueueManager>()?;

        // The flag wins; without it, the queues this binary actually has jobs
        // for — see `QueueManager::default_queues`. Deriving it means the
        // worker cannot be pointed at a queue it has nothing to run, and
        // cannot miss one it does, which is what happens when the list is
        // repeated on a command line in a Dockerfile, a chart and a systemd
        // unit. That failure is silent: the worker starts, drains a queue
        // nothing is dispatched to, and reports itself healthy while
        // processing nothing.
        let queues: Vec<String> = match args.option("queue") {
            Some(flag) => flag
                .split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect(),
            None => manager.default_queues(),
        };

        // Only reachable from `--queue=` or `--queue=,,`: the declared default
        // cannot be empty. A flag that is present and names nothing is a typo,
        // and draining no queues at all is never what it meant.
        if queues.is_empty() {
            eprintln!("--queue named no queues; there would be nothing to drain");
            return Ok(1);
        }

        let mut options = WorkerOptions::default()
            .queues(queues.clone())
            .sleep(Duration::from_secs(args.parsed_or("sleep", 1u64)));

        if args.flag("once") {
            options = options.stop_when_empty();
        }
        if let Some(max) = args.option("max-jobs").and_then(|value| value.parse().ok()) {
            options = options.max_jobs(max);
        }

        // A job that runs forever holds its worker forever, so a timeout is on
        // by default; `--timeout=0` is how a caller says "no limit" rather
        // than having to mean it by omission.
        //
        // This sets the *fallback*, not an override. A job that declares
        // `Job::TIMEOUT` is answering for itself and wins over this — how long
        // the work legitimately takes belongs to the work, and a flag that
        // could overrule it would make the declaration a decoy that a
        // deployment silently disagrees with.
        if let Some(seconds) = args.option("timeout").and_then(|value| value.parse::<u64>().ok()) {
            options = options.timeout(match seconds {
                0 => None,
                seconds => Some(Duration::from_secs(seconds)),
            });
        }

        // Recycling on a clock bounds whatever a long-lived process leaks.
        if let Some(seconds) = args.option("max-time").and_then(|value| value.parse::<u64>().ok()) {
            options = options.max_time(Duration::from_secs(seconds));
        }

        // A floor, not a ceiling: a job that asked for more attempts keeps them.
        if let Some(tries) = args.option("tries").and_then(|value| value.parse::<u32>().ok()) {
            options = options.tries(tries);
        }

        let mut worker = Worker::new(
            Arc::clone(manager.queue()),
            Arc::clone(manager.registry()),
            Arc::clone(app.container()),
        )
        .with_options(options);

        // Worker events are how an application observes its queue; wire them
        // up when a dispatcher is available.
        if let Ok(events) = app.resolve::<Dispatcher>() {
            worker = worker.with_events(events);
        }

        // Stop taking work when asked to stop.
        //
        // Nothing called `Worker::stop`, so a `SIGTERM` did nothing at all: the
        // worker kept reserving jobs and the process never exited. Under an
        // orchestrator that means the full termination grace period every time
        // — fifteen minutes of a pod sitting in `Terminating`, still claiming
        // work it will be killed in the middle of — and a rollout that has to
        // be forced through by hand.
        //
        // The flag is checked between jobs, never during one, so the job in
        // flight is finished and acknowledged before the loop breaks. That is
        // the behaviour a grace period is for: a worker that abandoned work
        // halfway to exit promptly would trade a slow rollout for redelivered
        // jobs.
        let worker = Arc::new(worker);
        let signalled = Arc::clone(&worker);

        tokio::spawn(async move {
            if let Some(signal) = stop_signal().await {
                tracing::info!(
                    signal,
                    "shutting down: finishing the job in flight and taking no more"
                );
                signalled.stop();
            }
        });

        println!("Processing jobs from: {}", queues.join(", "));
        let stats = worker.run().await?;

        println!(
            "\nProcessed {}, retried {}, failed {}.",
            stats.processed, stats.released, stats.failed
        );
        Ok(if stats.failed > 0 { exit::FAILURE } else { exit::SUCCESS })
    }
}

/// Wait for a shutdown signal, and say which one arrived.
///
/// `SIGTERM` is the one that matters — it is what an orchestrator sends and
/// what a `docker stop` sends — but `SIGINT` is what a person pressing Ctrl-C
/// sends, and a worker that drains on one and not the other is confusing in
/// exactly the situation where confusion is expensive.
///
/// `None` if no handler could be installed. That is not fatal: the worker
/// carries on and shuts down the way it did before, which is the behaviour
/// this replaces rather than a new failure.
#[cfg(unix)]
async fn stop_signal() -> Option<&'static str> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate())
        .inspect_err(|e| tracing::warn!(error = %e, "no SIGTERM handler; shutdown will not drain"))
        .ok()?;
    let mut interrupt = signal(SignalKind::interrupt())
        .inspect_err(|e| tracing::warn!(error = %e, "no SIGINT handler"))
        .ok()?;

    tokio::select! {
        _ = term.recv() => Some("SIGTERM"),
        _ = interrupt.recv() => Some("SIGINT"),
    }
}

/// Ctrl-C, which is all a non-Unix host offers here.
#[cfg(not(unix))]
async fn stop_signal() -> Option<&'static str> {
    tokio::signal::ctrl_c()
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "no Ctrl-C handler; shutdown will not drain"))
        .ok()
        .map(|()| "Ctrl-C")
}

/// `key:generate` — mint an application key.
#[derive(Debug, Default)]
pub struct KeyGenerateCommand;

#[async_trait::async_trait]
impl Command for KeyGenerateCommand {
    fn name(&self) -> &str {
        "key:generate"
    }

    fn description(&self) -> &str {
        "Generate an application key for APP_KEY"
    }

    fn help(&self) -> Option<&str> {
        Some(
            "Usage:\n  key:generate\n\n\
             Prints a new key. Put it in `.env` as APP_KEY.\n\n\
             Rotating: move the current APP_KEY into APP_PREVIOUS_KEYS (a\n\
             comma-separated list), put the new one in APP_KEY, and deploy.\n\
             Retired keys are still needed to read what they wrote, so do not\n\
             remove one until nothing encrypted with it remains.",
        )
    }

    async fn handle(&self, _args: &Arguments, _app: &Application) -> Result<i32> {
        // Printed rather than written into `.env`: editing a file the operator
        // owns, possibly clobbering a key that is still in use, is not
        // something a command should do without being asked very explicitly.
        println!("{}", rainier_crypt::Key::generate().to_base64());
        Ok(exit::SUCCESS)
    }
}

/// A console with every built-in command registered.
pub fn console(name: impl Into<String>) -> rainier_console::Console {
    rainier_console::Console::new(name)
        .register(RouteListCommand)
        .register(ServeCommand)
        .register(MigrateCommand)
        .register(MigrateRollbackCommand)
        .register(QueueWorkCommand)
        .register(crate::scheduling::ScheduleRunCommand)
        .register(crate::scheduling::ScheduleWorkCommand)
        .register(crate::scheduling::ScheduleListCommand)
        .register(KeyGenerateCommand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rainier;
    use rainier_console::Console;
    use rainier_database::testing::{fake_database, MemoryConnection};
    use rainier_orm::Dialect;
    use rainier_queue::{JobRegistry, MemoryQueue};

    async fn app() -> Arc<Application> {
        Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.get("/", || async { "home" }).name("home");
                router.get("/posts/{post}", || async { "post" }).name("posts.show");
            })
            .boot()
            .await
            .expect("boots")
    }

    #[tokio::test]
    async fn route_list_reports_the_table() {
        let app = app().await;
        let console = Console::new("rainier").register(RouteListCommand);

        assert_eq!(console.run_argv(&app, ["route:list"]).await, exit::SUCCESS);
        assert_eq!(console.run_argv(&app, ["route:list", "--json"]).await, exit::SUCCESS);
    }

    #[tokio::test]
    async fn route_list_sees_the_same_table_the_kernel_serves() {
        // Regression guard: the router used to be compiled twice, so
        // `route:list` described an empty table while the kernel served a
        // full one.
        let app = app().await;
        let router = app.resolve::<CompiledRouter>().unwrap();

        let names: Vec<String> = router.describe().into_iter().filter_map(|row| row.name).collect();
        assert!(names.contains(&"home".to_string()), "{names:?}");
        assert!(names.contains(&"posts.show".to_string()), "{names:?}");
    }

    #[tokio::test]
    async fn route_list_fails_cleanly_with_nothing_bound() {
        let bare = Application::new(".");
        let console = Console::new("rainier").register(RouteListCommand);
        assert_eq!(console.run_argv(&bare, ["route:list"]).await, exit::FAILURE);
    }

    #[tokio::test]
    async fn migrate_reports_what_it_ran() {
        let app = app().await;
        let (database, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));

        app.instance(database);
        app.instance(Migrator::new().raw(
            "0001_a",
            vec!["CREATE TABLE a (id INT)".into()],
            vec!["DROP TABLE a".into()],
        ));

        let console = Console::new("rainier").register(MigrateCommand);
        assert_eq!(console.run_argv(&app, ["migrate"]).await, exit::SUCCESS);
        assert_eq!(console.run_argv(&app, ["migrate", "--pretend"]).await, exit::SUCCESS);
    }

    #[tokio::test]
    async fn migrate_rollback_undoes_the_last_batch() {
        use rainier_database::row::OwnedRow;

        let app = app().await;
        let (database, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite)
                .returning([OwnedRow::new().with("name", "0001_a").with("batch", 1_i64)]),
        );

        app.instance(database);
        app.instance(Migrator::new().raw(
            "0001_a",
            vec!["CREATE TABLE a (id INT)".into()],
            vec!["DROP TABLE a".into()],
        ));

        let console = Console::new("rainier").register(MigrateRollbackCommand);
        assert_eq!(console.run_argv(&app, ["migrate:rollback"]).await, exit::SUCCESS);
        assert!(connection.statements().iter().any(|s| s == "DROP TABLE a"));
    }

    #[tokio::test]
    async fn migrate_rollback_reports_an_irreversible_step_as_a_failure() {
        // Exit non-zero, so a deploy script that chains a rollback stops rather
        // than carrying on against a schema it did not actually change.
        use rainier_database::row::OwnedRow;

        let app = app().await;
        let (database, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite)
                .returning([OwnedRow::new().with("name", "0001_a").with("batch", 1_i64)]),
        );

        app.instance(database);
        app.instance(Migrator::new().raw_irreversible(
            "0001_a",
            vec!["UPDATE a SET b = NULL".into()],
            "the old values are gone",
        ));

        let console = Console::new("rainier").register(MigrateRollbackCommand);
        assert_eq!(console.run_argv(&app, ["migrate:rollback"]).await, exit::FAILURE);
        assert!(
            !connection.statements().iter().any(|s| s.starts_with("DELETE FROM")),
            "the ledger row must survive a refused rollback"
        );
    }

    #[tokio::test]
    async fn queue_work_drains_the_queue_and_stops() {
        use rainier_queue::{Job, JobContext};
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        struct Ping;

        #[async_trait::async_trait]
        impl Job for Ping {
            const NAME: &'static str = "test.ping";
            async fn handle(&self, _: &JobContext) -> Result<()> {
                Ok(())
            }
        }

        let app = app().await;
        let queue = Arc::new(MemoryQueue::new());
        let registry = Arc::new(JobRegistry::new().with::<Ping>());
        let manager = QueueManager::new(Arc::clone(&queue) as Arc<_>, registry);

        manager.dispatch(Ping).await.unwrap();
        app.instance(manager);

        let console = Console::new("rainier").register(QueueWorkCommand);
        assert_eq!(console.run_argv(&app, ["queue:work", "--once"]).await, exit::SUCCESS);

        use rainier_queue::Queue as _;
        assert_eq!(queue.size("default").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn queue_work_reports_a_failure_in_its_exit_code() {
        use rainier_queue::{Job, JobContext};
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        struct Boom;

        #[async_trait::async_trait]
        impl Job for Boom {
            const NAME: &'static str = "test.boom";
            const TRIES: u32 = 1;
            async fn handle(&self, _: &JobContext) -> Result<()> {
                Err(rainier_support::Error::internal("nope"))
            }
        }

        let app = app().await;
        let queue = Arc::new(MemoryQueue::new());
        let manager = QueueManager::new(
            Arc::clone(&queue) as Arc<_>,
            Arc::new(JobRegistry::new().with::<Boom>()),
        );
        manager.dispatch(Boom).await.unwrap();
        app.instance(manager);

        let console = Console::new("rainier").register(QueueWorkCommand);
        assert_eq!(console.run_argv(&app, ["queue:work", "--once"]).await, exit::FAILURE);
    }

    #[test]
    fn the_built_in_console_registers_everything() {
        let console = console("rainier");
        for name in [
            "route:list",
            "serve",
            "migrate",
            "migrate:rollback",
            "queue:work",
            "schedule:run",
            "schedule:work",
            "schedule:list",
            "key:generate",
        ] {
            assert!(console.find(name).is_some(), "`{name}` should be registered");
        }
    }
}
