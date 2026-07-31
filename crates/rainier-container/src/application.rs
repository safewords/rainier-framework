//! The [`Application`] — the container plus a lifecycle.
//!
//! An application *is* a container with extra
//! responsibilities: `Application` derefs to
//! [`Container`], so `app.singleton(..)` and `app.resolve::<T>()` work directly,
//! while the application adds the parts a bare registry has no opinion about —
//! the two-phase provider lifecycle, the environment, the base paths, and the
//! lifecycle **hooks**.
//!
//! ```
//! # use rainier_container::{Application, Container};
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let app = Application::new(".");
//! app.instance(42u32);
//! app.booted(|_| println!("ready"));
//! app.boot().await?;
//! assert_eq!(*app.resolve::<u32>()?, 42);
//! # Ok(()) }
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rainier_support::Result;

use crate::container::Container;
use crate::provider::ServiceProvider;

/// A callback invoked at a point in the application lifecycle.
pub type LifecycleHook = Box<dyn Fn(&Application) + Send + Sync>;

/// The application: a [`Container`] with a boot lifecycle, an environment, and
/// a filesystem layout.
pub struct Application {
    container: Arc<Container>,
    providers: Mutex<Vec<Arc<dyn ServiceProvider>>>,
    /// How many entries of `providers` have already been booted. Providers
    /// registered after boot are booted by the next `boot()` call.
    booted_upto: Mutex<usize>,
    booting_hooks: Mutex<Vec<LifecycleHook>>,
    booted_hooks: Mutex<Vec<LifecycleHook>>,
    terminating_hooks: Mutex<Vec<LifecycleHook>>,
    environment: RwLock<String>,
    base_path: PathBuf,
    booted: AtomicBool,
}

impl Application {
    /// A new application rooted at `base_path`, in the `production`
    /// environment until told otherwise.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            container: Arc::new(Container::new()),
            providers: Mutex::new(Vec::new()),
            booted_upto: Mutex::new(0),
            booting_hooks: Mutex::new(Vec::new()),
            booted_hooks: Mutex::new(Vec::new()),
            terminating_hooks: Mutex::new(Vec::new()),
            environment: RwLock::new("production".to_string()),
            base_path: base_path.into(),
            booted: AtomicBool::new(false),
        }
    }

    /// Builder form of [`set_environment`](Self::set_environment).
    pub fn with_environment(self, environment: impl Into<String>) -> Self {
        self.set_environment(environment);
        self
    }

    /// The shared container. Hand this to anything that needs to resolve
    /// services without holding the whole application.
    pub fn container(&self) -> &Arc<Container> {
        &self.container
    }

    // --- providers ---------------------------------------------------------

    /// Register a provider, running its
    /// [`register`](ServiceProvider::register) pass immediately.
    ///
    /// Booting is deliberately *not* done here: a provider's
    /// [`boot`](ServiceProvider::boot) may resolve services that a
    /// later-registered provider binds, so booting has to wait until every
    /// provider has registered. Register everything, then call
    /// [`boot`](Self::boot).
    pub fn register(&self, provider: impl ServiceProvider) -> Result<()> {
        self.register_arc(Arc::new(provider))
    }

    /// [`register`](Self::register) for a provider that is already shared.
    pub fn register_arc(&self, provider: Arc<dyn ServiceProvider>) -> Result<()> {
        provider.register(self)?;
        self.providers.lock().expect("providers lock poisoned").push(provider);
        Ok(())
    }

    /// The names of every registered provider, in registration order.
    pub fn provider_names(&self) -> Vec<&'static str> {
        self.providers.lock().expect("providers lock poisoned").iter().map(|p| p.name()).collect()
    }

    // --- lifecycle ---------------------------------------------------------

    /// Boot every provider that has not booted yet, running the `booting`
    /// hooks first and the `booted` hooks after.
    ///
    /// Idempotent, and safe to call again after registering more providers —
    /// only the new ones boot.
    pub async fn boot(&self) -> Result<()> {
        let first_boot = !self.booted.load(Ordering::SeqCst);
        if first_boot {
            self.run_hooks(&self.booting_hooks);
        }

        loop {
            // Take one provider at a time rather than snapshotting the list:
            // a provider's `boot` is allowed to register further providers,
            // and those must boot in this same pass.
            let next = {
                let providers = self.providers.lock().expect("providers lock poisoned");
                let mut upto = self.booted_upto.lock().expect("providers lock poisoned");
                if *upto >= providers.len() {
                    break;
                }
                let provider = Arc::clone(&providers[*upto]);
                *upto += 1;
                provider
            };

            next.boot(self).await.map_err(|e| {
                rainier_support::Error::internal(format!(
                    "service provider `{}` failed to boot: {e}",
                    next.name()
                ))
            })?;
        }

        self.booted.store(true, Ordering::SeqCst);
        if first_boot {
            self.run_hooks(&self.booted_hooks);
        }
        Ok(())
    }

    /// Whether [`boot`](Self::boot) has completed at least once.
    pub fn is_booted(&self) -> bool {
        self.booted.load(Ordering::SeqCst)
    }

    /// Run `hook` immediately before providers boot.
    ///
    /// Ignored if the application has already booted — "before boot" has
    /// passed, and silently running it late would be worse than not running it.
    pub fn booting(&self, hook: impl Fn(&Application) + Send + Sync + 'static) {
        if self.is_booted() {
            tracing::warn!("`booting` hook registered after the application booted; ignoring");
            return;
        }
        self.booting_hooks.lock().expect("hooks lock poisoned").push(Box::new(hook));
    }

    /// Run `hook` once the application has booted — or immediately, if it
    /// already has. The immediate call is what makes this usable from code
    /// that cannot know whether boot has happened yet.
    pub fn booted(&self, hook: impl Fn(&Application) + Send + Sync + 'static) {
        if self.is_booted() {
            hook(self);
            return;
        }
        self.booted_hooks.lock().expect("hooks lock poisoned").push(Box::new(hook));
    }

    /// Run `hook` when [`terminate`](Self::terminate) is called — after a
    /// response has been sent, for work that should not delay it.
    pub fn terminating(&self, hook: impl Fn(&Application) + Send + Sync + 'static) {
        self.terminating_hooks.lock().expect("hooks lock poisoned").push(Box::new(hook));
    }

    /// Run the `terminating` hooks. The HTTP kernel calls this after flushing
    /// a response; a console command calls it before exiting.
    pub fn terminate(&self) {
        self.run_hooks(&self.terminating_hooks);
    }

    fn run_hooks(&self, hooks: &Mutex<Vec<LifecycleHook>>) {
        // Drain into a local first: a hook is free to register another hook,
        // and holding the lock across the callback would deadlock.
        let drained: Vec<LifecycleHook> =
            hooks.lock().expect("hooks lock poisoned").drain(..).collect();
        for hook in drained {
            hook(self);
        }
    }

    // --- environment -------------------------------------------------------

    /// The current environment name (`local`, `testing`, `production`, …).
    pub fn environment(&self) -> String {
        self.environment.read().expect("environment lock poisoned").clone()
    }

    /// Set the environment name.
    pub fn set_environment(&self, environment: impl Into<String>) {
        *self.environment.write().expect("environment lock poisoned") = environment.into();
    }

    /// Whether the environment is any of `names`.
    pub fn environment_is(&self, names: &[&str]) -> bool {
        let current = self.environment();
        names.iter().any(|n| *n == current)
    }

    /// Whether the environment is `local`.
    pub fn is_local(&self) -> bool {
        self.environment_is(&["local"])
    }

    /// Whether the environment is `testing`.
    pub fn is_testing(&self) -> bool {
        self.environment_is(&["testing", "test"])
    }

    /// Whether the environment is `production`.
    pub fn is_production(&self) -> bool {
        self.environment_is(&["production", "prod"])
    }

    // --- paths -------------------------------------------------------------

    /// The application root.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// A path relative to the application root.
    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.base_path.join(relative)
    }

    /// `<base>/config`.
    pub fn config_path(&self) -> PathBuf {
        self.path("config")
    }

    /// `<base>/storage`.
    pub fn storage_path(&self) -> PathBuf {
        self.path("storage")
    }

    /// `<base>/resources` — where views live.
    pub fn resource_path(&self) -> PathBuf {
        self.path("resources")
    }

    /// `<base>/database`.
    pub fn database_path(&self) -> PathBuf {
        self.path("database")
    }
}

impl std::ops::Deref for Application {
    type Target = Container;

    fn deref(&self) -> &Self::Target {
        &self.container
    }
}

impl std::fmt::Debug for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Application")
            .field("environment", &self.environment())
            .field("base_path", &self.base_path)
            .field("booted", &self.is_booted())
            .field("providers", &self.provider_names().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::BoxFuture;
    use std::sync::atomic::AtomicUsize;

    /// The providers below record their lifecycle steps into one process-wide
    /// log, so the tests that assert on ordering must not interleave. Each one
    /// takes this lock for its duration and starts from an empty log.
    static SERIAL: Mutex<()> = Mutex::new(());
    static ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

    struct Recording(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Recording {
        fn start() -> Self {
            let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            ORDER.lock().unwrap().clear();
            Self(guard)
        }

        fn steps(&self) -> Vec<&'static str> {
            ORDER.lock().unwrap().clone()
        }
    }

    fn record(step: &'static str) {
        ORDER.lock().unwrap().push(step);
    }

    struct First;
    struct Second;
    struct Marker(u32);

    impl ServiceProvider for First {
        fn register(&self, app: &Application) -> Result<()> {
            record("register:first");
            app.instance(Marker(1));
            Ok(())
        }
        fn boot<'a>(&'a self, _app: &'a Application) -> BoxFuture<'a, Result<()>> {
            Box::pin(async {
                record("boot:first");
                Ok(())
            })
        }
    }

    impl ServiceProvider for Second {
        fn register(&self, _app: &Application) -> Result<()> {
            record("register:second");
            Ok(())
        }
        fn boot<'a>(&'a self, app: &'a Application) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                record("boot:second");
                // Legal here and nowhere earlier: every provider has registered.
                assert_eq!(app.resolve::<Marker>()?.0, 1);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn every_provider_registers_before_any_boots() {
        let log = Recording::start();
        let app = Application::new(".");
        app.register(First).unwrap();
        app.register(Second).unwrap();
        app.boot().await.unwrap();

        assert_eq!(
            log.steps(),
            vec!["register:first", "register:second", "boot:first", "boot:second"]
        );
    }

    #[tokio::test]
    async fn boot_is_idempotent() {
        let log = Recording::start();
        let app = Application::new(".");
        app.register(First).unwrap();
        app.boot().await.unwrap();
        app.boot().await.unwrap();

        assert_eq!(log.steps().iter().filter(|s| **s == "boot:first").count(), 1);
    }

    #[tokio::test]
    async fn providers_registered_after_boot_are_booted_by_the_next_boot() {
        let log = Recording::start();
        let app = Application::new(".");
        app.boot().await.unwrap();
        app.register(First).unwrap();
        app.boot().await.unwrap();

        assert!(log.steps().contains(&"boot:first"));
    }

    #[tokio::test]
    async fn hooks_run_around_booting() {
        let app = Application::new(".");
        let seen = Arc::new(Mutex::new(Vec::new()));

        let s = Arc::clone(&seen);
        app.booting(move |_| s.lock().unwrap().push("booting"));
        let s = Arc::clone(&seen);
        app.booted(move |_| s.lock().unwrap().push("booted"));
        let s = Arc::clone(&seen);
        app.terminating(move |_| s.lock().unwrap().push("terminating"));

        app.boot().await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec!["booting", "booted"]);

        app.terminate();
        assert_eq!(*seen.lock().unwrap(), vec!["booting", "booted", "terminating"]);
    }

    #[tokio::test]
    async fn a_booted_hook_added_late_fires_immediately() {
        let app = Application::new(".");
        app.boot().await.unwrap();

        let fired = Arc::new(AtomicUsize::new(0));
        let f = Arc::clone(&fired);
        app.booted(move |_| {
            f.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failing_provider_names_itself() {
        struct Broken;
        impl ServiceProvider for Broken {
            fn name(&self) -> &'static str {
                "BrokenProvider"
            }
            fn boot<'a>(&'a self, _: &'a Application) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Err(rainier_support::Error::internal("no socket")) })
            }
        }

        let app = Application::new(".");
        app.register(Broken).unwrap();
        let err = app.boot().await.unwrap_err();
        assert!(err.message().contains("BrokenProvider"), "{}", err.message());
        assert!(err.message().contains("no socket"), "{}", err.message());
    }

    #[test]
    fn the_application_derefs_to_its_container() {
        let app = Application::new(".");
        app.instance(Marker(9));
        assert_eq!(app.resolve::<Marker>().unwrap().0, 9);
    }

    #[test]
    fn environment_predicates() {
        let app = Application::new(".").with_environment("local");
        assert!(app.is_local());
        assert!(!app.is_production());
        app.set_environment("testing");
        assert!(app.is_testing());
    }

    #[test]
    fn paths_hang_off_the_base_path() {
        let app = Application::new("/srv/app");
        assert_eq!(app.config_path(), PathBuf::from("/srv/app/config"));
        assert_eq!(app.path("routes/web.rs"), PathBuf::from("/srv/app/routes/web.rs"));
    }
}
