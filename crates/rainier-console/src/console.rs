//! The console kernel — [`Command`] and [`Console`].

use std::sync::Arc;

use rainier_container::Application;
use rainier_support::Result;

use crate::arguments::Arguments;

/// Exit codes, following the shell convention.
pub mod exit {
    /// Everything worked.
    pub const SUCCESS: i32 = 0;
    /// The command ran and reported a failure.
    pub const FAILURE: i32 = 1;
    /// The command line was wrong — an unknown command, a missing argument.
    pub const USAGE: i32 = 2;
}

/// A console command.
///
/// ```
/// use rainier_console::{Arguments, Command};
/// use rainier_container::Application;
/// use rainier_support::Result;
///
/// struct Greet;
///
/// #[async_trait::async_trait]
/// impl Command for Greet {
///     fn name(&self) -> &str { "greet" }
///     fn description(&self) -> &str { "Say hello" }
///
///     async fn handle(&self, args: &Arguments, _app: &Application) -> Result<i32> {
///         println!("Hello, {}", args.argument(0).unwrap_or("world"));
///         Ok(0)
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait Command: Send + Sync + 'static {
    /// The name it is invoked by — `route:list`.
    fn name(&self) -> &str;

    /// A one-line summary, shown in the command list.
    fn description(&self) -> &str {
        ""
    }

    /// Longer help, shown for `<command> --help`.
    fn help(&self) -> Option<&str> {
        None
    }

    /// Run it, returning an exit code.
    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32>;
}

/// The console kernel: a registry of commands and the dispatcher for them.
pub struct Console {
    name: String,
    commands: Vec<Arc<dyn Command>>,
}

impl Console {
    /// A console named `name`, as shown in its usage line.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), commands: Vec::new() }
    }

    /// Register a command.
    pub fn register(mut self, command: impl Command) -> Self {
        self.commands.push(Arc::new(command));
        self
    }

    /// Register an already-shared command.
    pub fn register_arc(mut self, command: Arc<dyn Command>) -> Self {
        self.commands.push(command);
        self
    }

    /// Every registered command, sorted by name.
    pub fn commands(&self) -> Vec<Arc<dyn Command>> {
        let mut commands = self.commands.clone();
        commands.sort_by(|a, b| a.name().cmp(b.name()));
        commands
    }

    /// Look a command up by name.
    pub fn find(&self, name: &str) -> Option<Arc<dyn Command>> {
        self.commands.iter().find(|command| command.name() == name).cloned()
    }

    /// Run the command named in `args`, returning its exit code.
    ///
    /// Never panics and never propagates: a console's job is to report and
    /// exit with a code, so an error becomes a message on stderr and
    /// [`exit::FAILURE`].
    pub async fn run(&self, app: &Application, args: Arguments) -> i32 {
        if args.is_empty() {
            println!("{}", self.usage());
            return exit::SUCCESS;
        }

        if args.command() == "list" {
            println!("{}", self.usage());
            return exit::SUCCESS;
        }

        if args.command() == "help" {
            return match args.argument(0) {
                Some(name) => self.print_help(name),
                None => {
                    println!("{}", self.usage());
                    exit::SUCCESS
                }
            };
        }

        let Some(command) = self.find(args.command()) else {
            eprintln!("Command `{}` is not defined.", args.command());
            if let Some(suggestion) = self.closest(args.command()) {
                eprintln!("Did you mean `{suggestion}`?");
            }
            eprintln!("\nRun `{} list` to see what is available.", self.name);
            return exit::USAGE;
        };

        if args.flag("help") || args.flag("h") {
            return self.print_help(command.name());
        }

        match command.handle(&args, app).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{}: {e}", command.name());
                exit::FAILURE
            }
        }
    }

    /// Parse `argv` (without the program name) and run it.
    pub async fn run_argv<I, S>(&self, app: &Application, argv: I) -> i32
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.run(app, Arguments::parse(argv)).await
    }

    /// Parse the process's own arguments and run.
    pub async fn run_from_env(&self, app: &Application) -> i32 {
        self.run_argv(app, std::env::args().skip(1)).await
    }

    /// The usage text: a header and the command list, aligned.
    pub fn usage(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(out, "{}\n", self.name);
        let _ = writeln!(out, "Usage:\n  {} <command> [options]\n", self.name);

        if self.commands.is_empty() {
            let _ = writeln!(out, "No commands are registered.");
            return out;
        }

        let width = self.commands.iter().map(|c| c.name().len()).max().unwrap_or(0);
        let _ = writeln!(out, "Available commands:");
        for command in self.commands() {
            let _ = writeln!(
                out,
                "  {:width$}  {}",
                command.name(),
                command.description(),
                width = width
            );
        }
        out
    }

    fn print_help(&self, name: &str) -> i32 {
        let Some(command) = self.find(name) else {
            eprintln!("Command `{name}` is not defined.");
            return exit::USAGE;
        };

        println!("{}\n", command.name());
        if !command.description().is_empty() {
            println!("{}\n", command.description());
        }
        match command.help() {
            Some(help) => println!("{help}"),
            None => println!("Usage:\n  {} {name} [options]", self.name),
        }
        exit::SUCCESS
    }

    /// The registered command closest to `name`, for a "did you mean?".
    ///
    /// Prefix and substring matching rather than an edit distance: a console
    /// with namespaced commands (`queue:work`, `queue:retry`) makes a typo in
    /// the namespace far more likely than a transposition, and `queue:` should
    /// suggest something.
    fn closest(&self, name: &str) -> Option<String> {
        let lower = name.to_lowercase();

        self.commands()
            .iter()
            .map(|command| command.name().to_string())
            .filter(|candidate| {
                let candidate = candidate.to_lowercase();
                candidate.starts_with(&lower)
                    || lower.starts_with(&candidate)
                    || candidate.contains(&lower)
                    || lower.contains(&candidate)
            })
            .min_by_key(|candidate| candidate.len())
    }
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console")
            .field("name", &self.name)
            .field(
                "commands",
                &self.commands().iter().map(|c| c.name().to_string()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static RAN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static CALLS: AtomicUsize = AtomicUsize::new(0);

    struct Greet;

    #[async_trait::async_trait]
    impl Command for Greet {
        fn name(&self) -> &str {
            "greet"
        }
        fn description(&self) -> &str {
            "Say hello"
        }
        async fn handle(&self, args: &Arguments, _: &Application) -> Result<i32> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            RAN.lock().unwrap().push(args.argument(0).unwrap_or("world").to_string());
            Ok(exit::SUCCESS)
        }
    }

    struct Failing;

    #[async_trait::async_trait]
    impl Command for Failing {
        fn name(&self) -> &str {
            "queue:work"
        }
        fn description(&self) -> &str {
            "Process queued jobs"
        }
        async fn handle(&self, _: &Arguments, _: &Application) -> Result<i32> {
            Err(Error::internal("the queue is unreachable"))
        }
    }

    struct NonZero;

    #[async_trait::async_trait]
    impl Command for NonZero {
        fn name(&self) -> &str {
            "check"
        }
        async fn handle(&self, _: &Arguments, _: &Application) -> Result<i32> {
            Ok(3)
        }
    }

    fn console() -> Console {
        Console::new("rainier").register(Greet).register(Failing).register(NonZero)
    }

    fn app() -> Application {
        Application::new(".")
    }

    async fn run(line: &str) -> i32 {
        console().run_argv(&app(), line.split_whitespace()).await
    }

    #[tokio::test]
    async fn runs_a_registered_command() {
        RAN.lock().unwrap().clear();
        assert_eq!(run("greet Ada").await, exit::SUCCESS);
        assert_eq!(*RAN.lock().unwrap(), vec!["Ada"]);
    }

    #[tokio::test]
    async fn a_commands_exit_code_is_returned() {
        assert_eq!(run("check").await, 3);
    }

    #[tokio::test]
    async fn an_erroring_command_reports_a_failure_rather_than_propagating() {
        // A console's contract is a message and an exit code, not a `Result`.
        assert_eq!(run("queue:work").await, exit::FAILURE);
    }

    #[tokio::test]
    async fn an_unknown_command_is_a_usage_error() {
        assert_eq!(run("nonsense").await, exit::USAGE);
    }

    #[tokio::test]
    async fn no_command_prints_the_list() {
        assert_eq!(console().run_argv(&app(), Vec::<String>::new()).await, exit::SUCCESS);
        assert_eq!(run("list").await, exit::SUCCESS);
    }

    #[tokio::test]
    async fn help_is_available_for_a_command() {
        assert_eq!(run("help greet").await, exit::SUCCESS);
        assert_eq!(run("help nonsense").await, exit::USAGE);
        assert_eq!(run("help").await, exit::SUCCESS);
    }

    #[tokio::test]
    async fn a_help_flag_shows_help_instead_of_running() {
        // Its own counter and its own command, because `CALLS` is process-wide
        // and every other test that runs `greet` increments it. Reading it
        // before and after only proves this command did not run when nothing
        // else runs concurrently — and the test harness runs these in parallel,
        // so the assertion failed whenever another test's increment landed
        // between the two reads. Passing under `--test-threads=1` and failing
        // otherwise is the tell.
        static SOLO_CALLS: AtomicUsize = AtomicUsize::new(0);

        struct Solo;

        #[async_trait::async_trait]
        impl Command for Solo {
            fn name(&self) -> &str {
                "solo"
            }
            fn description(&self) -> &str {
                "Only this test runs it"
            }
            async fn handle(&self, _: &Arguments, _: &Application) -> Result<i32> {
                SOLO_CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(exit::SUCCESS)
            }
        }

        let console = Console::new("rainier").register(Solo);

        assert_eq!(console.run_argv(&app(), ["solo", "--help"]).await, exit::SUCCESS);
        assert_eq!(SOLO_CALLS.load(Ordering::SeqCst), 0, "the command must not have run");

        // The other half of the claim: without the flag it does run, so the
        // assertion above is about `--help` and not about a command that never
        // runs at all.
        assert_eq!(console.run_argv(&app(), ["solo"]).await, exit::SUCCESS);
        assert_eq!(SOLO_CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_usage_text_lists_every_command_aligned() {
        let usage = console().usage();

        assert!(usage.contains("Available commands:"), "{usage}");
        assert!(usage.contains("greet"), "{usage}");
        assert!(usage.contains("Say hello"), "{usage}");
        assert!(usage.contains("queue:work"), "{usage}");
        // Sorted, so the list is stable between runs.
        assert!(
            usage.find("check").unwrap() < usage.find("greet").unwrap(),
            "commands should be sorted: {usage}"
        );
    }

    #[test]
    fn an_empty_console_says_so() {
        assert!(Console::new("rainier").usage().contains("No commands"));
    }

    #[test]
    fn a_near_miss_is_suggested() {
        let console = console();
        assert_eq!(console.closest("greet2").as_deref(), Some("greet"));
        assert_eq!(console.closest("gree").as_deref(), Some("greet"));
        // The namespace case: a typo in `queue:` should still find something.
        assert_eq!(console.closest("queue").as_deref(), Some("queue:work"));
        assert_eq!(console.closest("zzz"), None);
    }

    #[test]
    fn commands_can_be_looked_up() {
        let console = console();
        assert!(console.find("greet").is_some());
        assert!(console.find("nope").is_none());
        assert_eq!(console.commands().len(), 3);
    }
}
