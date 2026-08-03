//! Bootstrapping — [`Rainier`], the builder that assembles an application.
//!
//! Wiring a framework by hand means binding a dozen services in the right
//! order before anything works. This builder does the parts every application
//! wants the same way — environment, config, events, views, logging, the
//! middleware registry, the facades — and leaves the parts that differ (the
//! database, the queue driver, the routes) to explicit calls.

use std::path::PathBuf;
use std::sync::Arc;

use rainier_cache::CacheManager;
use rainier_config::{Config, Env};
use rainier_container::{Application, ServiceProvider};
use rainier_crypt::{Encryption, Key, KeyRing};
use rainier_database::{Database, DatabaseManager, Databases};
use rainier_events::Dispatcher;
use rainier_filesystem::Storage;
use rainier_mail::Mailer;
use rainier_middleware::MiddlewareRegistry;
use rainier_queue::{
    ConnectionConfig, Connections as QueueConnections, JobRegistry, KafkaConnection, QueueDriver,
    QueueManager, QueueResources, RedisConnection, SqsConnection,
};
use rainier_routing::{Router, UrlGenerator};
use rainier_server::Kernel;
use rainier_session::{MemorySessionStore, SessionManager};
use rainier_support::{Error, Result};
use rainier_view::{TemplateEngine, ViewEngine};

use crate::facades::Views;
use crate::keys;

/// Assembles an [`Application`].
///
/// ```no_run
/// use rainier_framework::Rainier;
///
/// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
/// let app = Rainier::new(".")
///     .with_routes(|router| {
///         router.get("/", || async { "Hello from Rainier" });
///     })
///     .boot()
///     .await?;
/// # let _ = app;
/// # Ok(()) }
/// ```
pub struct Rainier {
    base_path: PathBuf,
    env: Env,
    config: Config,
    /// A configuration error from [`Rainier::new`], surfaced by
    /// [`boot`](Rainier::boot).
    ///
    /// `new` cannot return a `Result` without putting a `?` in the middle of
    /// every builder chain, and an unreadable `CACHE_DRIVER` must not be
    /// swallowed. So it waits here.
    deferred: Option<Error>,
    events: Dispatcher,
    middleware: MiddlewareRegistry,
    router: Router,
    views: Option<Arc<dyn ViewEngine>>,
    database: Option<Database>,
    queue: Option<QueueManager>,
    jobs: Option<Arc<JobRegistry>>,
    mailer: Option<Mailer>,
    sessions: Option<SessionManager>,
    crypt: Option<Encryption>,
    cache: Option<CacheManager>,
    notifier: Option<rainier_notify::Notifier>,
    broadcasting: Option<rainier_broadcast::Broadcasting>,
    websockets: Option<rainier_websocket::WebSocketRoutes>,
    /// Values bound as-is, in declaration order.
    #[allow(clippy::type_complexity, reason = "one boxed closure per bound value")]
    instances: Vec<Box<dyn Fn(&Application)>>,
    schedule: Option<rainier_scheduler::Schedule>,
    storage: Option<Storage>,
    providers: Vec<Arc<dyn ServiceProvider>>,
    install_facades: bool,
    install_tracing: bool,
}

impl Rainier {
    /// Start from `base_path`, reading `.env` from it if there is one.
    ///
    /// Infallible so the builder chain reads as one expression. A driver name
    /// the environment got wrong is held in `deferred` and returned from
    /// [`boot`](Self::boot) — the error still stops the application, it just
    /// does not force a `?` into the middle of the chain.
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        let base_path = base_path.into();
        let env = Env::load_or_default(base_path.join(".env"));
        let (config, deferred) = match default_config(&env, &base_path) {
            Ok(config) => (config, None),
            // Carry on with an empty tree so `with_*` calls still apply; `boot`
            // returns the error before any of it is used.
            Err(e) => (Config::new(), Some(e)),
        };

        Self {
            base_path,
            env,
            config,
            deferred,
            events: Dispatcher::new(),
            middleware: default_middleware(),
            router: Router::new(),
            views: None,
            database: None,
            queue: None,
            jobs: None,
            mailer: None,
            sessions: None,
            crypt: None,
            cache: None,
            notifier: None,
            broadcasting: None,
            websockets: None,
            instances: Vec::new(),
            schedule: None,
            storage: None,
            providers: Vec::new(),
            install_facades: true,
            install_tracing: true,
        }
    }

    /// The environment, for reading deployment values while building.
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// The configuration, for adding to it while building.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Adjust the configuration.
    pub fn configure(self, adjust: impl FnOnce(&Config)) -> Self {
        adjust(&self.config);
        self
    }

    /// Register event listeners.
    pub fn with_events(self, register: impl FnOnce(&Dispatcher)) -> Self {
        register(&self.events);
        self
    }

    /// Register middleware aliases and groups.
    pub fn with_middleware(self, register: impl FnOnce(&MiddlewareRegistry)) -> Self {
        register(&self.middleware);
        self
    }

    /// Declare routes.
    pub fn with_routes(mut self, declare: impl FnOnce(&mut Router)) -> Self {
        declare(&mut self.router);
        self
    }

    /// Use a specific view engine. Defaults to a [`TemplateEngine`] over
    /// `<base>/resources/views`.
    pub fn with_views(mut self, engine: Arc<dyn ViewEngine>) -> Self {
        self.views = Some(engine);
        self
    }

    /// Use this database.
    ///
    /// Takes a built [`Database`], so it is the escape hatch — a test's fake
    /// connection, or a backend no configuration file can describe, such as a
    /// D1 or libSQL executor over a caller-supplied transport. Declaring
    /// connections is [`with_databases`](Self::with_databases) or `DATABASE_URL`,
    /// both of which open each connection from its own settings.
    ///
    /// It wins over both, exactly as [`with_storage`](Self::with_storage) wins
    /// over the declared disks: it is in the builder chain a reviewer is already
    /// reading, next to whatever it overrides, rather than in an environment a
    /// platform injected.
    pub fn with_database(mut self, database: Database) -> Self {
        self.database = Some(database);
        self
    }

    /// Declare the database connections, and let the framework open them.
    ///
    /// ```no_run
    /// # use rainier_framework::Rainier;
    /// use rainier_framework::database::{Databases, ServerDatabase, SqliteDatabase};
    ///
    /// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
    /// let app = Rainier::new(".")
    ///     .with_databases(
    ///         Databases::new("primary")
    ///             .with("primary", ServerDatabase::mysql("app").host("db.internal"))
    ///             .with("reporting", SqliteDatabase::new("storage/reporting.sqlite")),
    ///     )
    ///     .boot()
    ///     .await?;
    /// # let _ = app; Ok(()) }
    /// ```
    ///
    /// Written into the configuration tree rather than held aside, so
    /// `config.get(keys::DATABASES)` answers with what the application actually
    /// declared and a later [`configure`](Self::configure) can still add to it.
    ///
    /// Every declared connection is opened at [`boot`](Self::boot), which is why
    /// a replica that is down stops the process starting: a handle that has not
    /// connected is a handle that might not be a database, and the alternative
    /// moves every DSN mistake from a boot failure a deploy can catch to a
    /// runtime failure at whatever hour the query first runs.
    ///
    /// Declaring this **and** setting `DATABASE_URL` fails the boot — see
    /// [`keys::DATABASES`].
    pub fn with_databases(mut self, databases: Databases) -> Self {
        if let Err(e) = self.config.set(keys::DATABASES, databases) {
            self.deferred = self.deferred.or(Some(e));
        }
        self
    }

    /// Use this queue.
    ///
    /// Takes a built [`QueueManager`], so it is the escape hatch — a
    /// [`fake`](QueueManager::fake) for a test, or a backend built in code.
    /// Declaring connections is [`with_queues`](Self::with_queues) or
    /// `QUEUE_DRIVER`, and it wins over both for the reason
    /// [`with_database`](Self::with_database) does.
    pub fn with_queue(mut self, queue: QueueManager) -> Self {
        self.queue = Some(queue);
        self
    }

    /// Declare the queue connections, and let the framework build them.
    ///
    /// ```no_run
    /// # use rainier_framework::Rainier;
    /// use rainier_framework::queue::{ConnectionConfig, Connections, SqsConnection};
    ///
    /// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
    /// let app = Rainier::new(".")
    ///     .with_queues(
    ///         Connections::new("primary")
    ///             .with("primary", ConnectionConfig::database())
    ///             .with("bulk", SqsConnection::new("https://sqs.example.com/0/bulk")),
    ///     )
    ///     .boot()
    ///     .await?;
    /// # let _ = app; Ok(()) }
    /// ```
    ///
    /// Written into the configuration tree, like
    /// [`with_databases`](Self::with_databases). The three things a connection
    /// needs that a file cannot hold arrive from the booting application: the
    /// job registry [`with_jobs`](Self::with_jobs) declares, the container a
    /// `sync` job resolves its dependencies from, and the database a `database`
    /// connection stores its jobs in — so the database is opened first, and a
    /// `database` connection declared without one fails naming what is missing.
    ///
    /// Declaring this **and** setting `QUEUE_DRIVER` fails the boot — see
    /// [`keys::QUEUES`].
    pub fn with_queues(mut self, queues: QueueConnections) -> Self {
        if let Err(e) = self.config.set(keys::QUEUES, queues) {
            self.deferred = self.deferred.or(Some(e));
        }
        self
    }

    /// Declare the jobs a worker can turn back into code.
    ///
    /// A job travels as a name and a payload, so something has to map the name
    /// back to a type before it can be run. That is this — and it is needed
    /// wherever the *framework* builds the queue, because a
    /// [`QueueManager`] cannot be constructed without a registry and an empty
    /// one runs nothing.
    ///
    /// ```no_run
    /// # use rainier_framework::{queue::{Job, JobContext, JobRegistry}, Rainier};
    /// # use serde::{Deserialize, Serialize};
    /// # #[derive(Serialize, Deserialize)] struct SendInvoice;
    /// # #[rainier_framework::queue::async_trait]
    /// # impl Job for SendInvoice {
    /// #     const NAME: &'static str = "billing.send-invoice";
    /// #     async fn handle(&self, _: &JobContext) -> rainier_support::Result<()> { Ok(()) }
    /// # }
    /// Rainier::new(".").with_jobs(JobRegistry::new().with::<SendInvoice>())
    /// # ;
    /// ```
    ///
    /// An application that hands over its own [`with_queue`](Self::with_queue)
    /// already gave that manager a registry and does not need this. Registering
    /// jobs in a provider does not reach the framework's own queue either: a
    /// provider runs *after* the queue is built, because a provider may
    /// legitimately resolve one.
    pub fn with_jobs(mut self, jobs: JobRegistry) -> Self {
        self.jobs = Some(Arc::new(jobs));
        self
    }

    /// Use this mailer.
    pub fn with_mailer(mut self, mailer: Mailer) -> Self {
        self.mailer = Some(mailer);
        self
    }

    /// Use this session store, and register the `session` middleware alias.
    ///
    /// Without this, sessions default to an in-process store — right for
    /// development and wrong the moment there are two instances. See
    /// [`SessionManager`].
    pub fn with_sessions(mut self, sessions: SessionManager) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// Use this cache.
    ///
    /// Defaults to an in-process one, which is right for development and wrong
    /// the moment a second instance exists — see [`CacheManager`].
    pub fn with_cache(mut self, cache: CacheManager) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Use these notification channels.
    ///
    /// Without it, notifications go to the log — which delivers nothing, and is
    /// the right thing to have by accident: a misconfigured deployment logs
    /// them rather than emailing real people from a copy of production data.
    ///
    /// ```ignore
    /// Rainier::new(".").with_notifier(
    ///     Notifier::new()
    ///         .with(MailChannel::new(mailer))
    ///         .with(DatabaseChannel::new(database)),
    /// )
    /// ```
    pub fn with_notifier(mut self, notifier: rainier_notify::Notifier) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Use this broadcast driver.
    ///
    /// Without it, broadcasts go to the log — they reach no browser, which is
    /// the right accident to have: an application that has not configured a
    /// relay logs what it would have published instead of failing requests.
    ///
    /// The **channel table** is not set here, because it is generic over your
    /// user model. Bind a `ChannelRegistry<User>` in a provider; until you do,
    /// every private channel is denied.
    ///
    /// ```ignore
    /// Rainier::new(".").with_broadcasting(Broadcasting::new(Arc::new(
    ///     RedisBroadcaster::connect(&connector).await?.with_pusher_auth(auth),
    /// )))
    /// ```
    pub fn with_broadcasting(mut self, broadcasting: rainier_broadcast::Broadcasting) -> Self {
        self.broadcasting = Some(broadcasting);
        self
    }

    /// Serve these WebSocket routes.
    ///
    /// On the **same port** as HTTP: a socket connection begins as a `GET`
    /// asking to upgrade, so there is no second listener to start and nothing
    /// to run alongside anything.
    ///
    /// ```ignore
    /// Rainier::new(".").with_websockets(
    ///     WebSocketRoutes::new().add("/ws/rooms/{room}", Chat::new(rooms)),
    /// )
    /// ```
    pub fn with_websockets(mut self, routes: rainier_websocket::WebSocketRoutes) -> Self {
        self.websockets = Some(routes);
        self
    }

    /// Bind a value you already have.
    ///
    /// The gap the typed `with_*` methods leave: an application often holds
    /// something the framework has never heard of — a
    /// [`Rooms`](rainier_websocket::Rooms) registry, a feature-flag client, an
    /// HTTP client with a pool — and it needs to be resolvable without a
    /// provider whose whole body is one `instance` call.
    ///
    /// A provider is still the right answer when the value has to be *built*
    /// from other bound services; this is for one that already exists.
    pub fn with_instance<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        let value = Arc::new(value);
        self.instances.push(Box::new(move |app| app.instance_arc(Arc::clone(&value))));
        self
    }

    /// Bind something already shared, keeping your handle to it.
    pub fn with_instance_arc<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        self.instances.push(Box::new(move |app| app.instance_arc(Arc::clone(&value))));
        self
    }

    /// Declare the scheduled tasks.
    ///
    /// ```ignore
    /// Rainier::new(".").with_schedule(routes::console::schedule)
    /// ```
    ///
    /// Bound in the container, so `schedule:run` and `schedule:list` find it.
    pub fn with_schedule(mut self, declare: impl FnOnce(&mut rainier_scheduler::Schedule)) -> Self {
        let mut schedule = self.schedule.take().unwrap_or_default();
        declare(&mut schedule);
        self.schedule = Some(schedule);
        self
    }

    /// Use this file storage.
    ///
    /// Defaults to `<base>/storage/app`, so `Storage::instance()` works on a
    /// fresh clone without configuration.
    ///
    /// Takes a built [`Storage`], so it is the escape hatch — a test's memory
    /// disk, or a disk on a driver the framework does not ship. Declaring disks
    /// is [`with_disks`](Self::with_disks) or the `filesystems` section, both of
    /// which build each disk from its own settings.
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Declare the disks, and let the framework build them.
    ///
    /// ```no_run
    /// # use rainier_framework::Rainier;
    /// use rainier_framework::filesystem::{DiskConfig, Disks, S3Disk};
    ///
    /// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
    /// let app = Rainier::new(".")
    ///     .with_disks(
    ///         Disks::new("uploads")
    ///             .with("uploads", DiskConfig::local("storage/app"))
    ///             .with("archive", S3Disk::new("archive-bucket").region("us-east-1")),
    ///     )
    ///     .boot()
    ///     .await?;
    /// # let _ = app; Ok(()) }
    /// ```
    ///
    /// Written into the configuration tree rather than held aside, so
    /// `config.get(keys::FILESYSTEMS)` answers with what the application
    /// actually declared and a later
    /// [`configure`](Self::configure) can still add to it. A declaration that
    /// cannot be built — a default naming a disk nobody declared, half a key
    /// pair — fails at [`boot`](Self::boot).
    pub fn with_disks(mut self, disks: rainier_filesystem::Disks) -> Self {
        if let Err(e) = self.config.set(keys::FILESYSTEMS, disks) {
            self.deferred = self.deferred.or(Some(e));
        }
        self
    }

    /// Use these keys for encryption and signing.
    ///
    /// Defaults to `APP_KEY` from the environment. With neither, a random key
    /// is generated per boot — which works, and silently invalidates every
    /// encrypted value the last boot produced, so it warns.
    pub fn with_encryption(mut self, crypt: Encryption) -> Self {
        self.crypt = Some(crypt);
        self
    }

    /// Register a service provider.
    pub fn with_provider(mut self, provider: impl ServiceProvider) -> Self {
        self.providers.push(Arc::new(provider));
        self
    }

    /// Do not install the facades' global application.
    ///
    /// Two applications in one process would otherwise fight over it — which
    /// is exactly what a test suite building an app per test does.
    pub fn without_facades(mut self) -> Self {
        self.install_facades = false;
        self
    }

    /// Do not install a tracing subscriber.
    pub fn without_tracing(mut self) -> Self {
        self.install_tracing = false;
        self
    }

    /// Build and boot the application.
    ///
    /// Compiling the router here is what makes an unknown middleware alias or
    /// a duplicate route name a **boot** failure rather than a surprise on the
    /// first request that hits it.
    pub async fn boot(self) -> Result<Arc<Application>> {
        // Before anything is installed, and before tracing: a `.env` the
        // process cannot read is not a state to start serving in.
        if let Some(error) = self.deferred {
            return Err(error);
        }

        if self.install_tracing {
            install_tracing(&self.env, &self.config);
        }

        let environment = self.config.get(keys::APP_ENV).unwrap_or_default().to_string();
        let app = Arc::new(Application::new(self.base_path.clone()).with_environment(&environment));

        // Installed *before* anything else runs, not at the end. A service
        // provider's `boot`, and a middleware alias factory resolved while the
        // router compiles, both legitimately reach for a facade — and both
        // happen during this call. Installing last would make them panic.
        if self.install_facades {
            rainier_container::set_facade_application(Arc::clone(&app));
        }

        let views = self.views.unwrap_or_else(|| {
            // The `@vite` resolver comes attached over `<base>/public` — the
            // PHP-framework convention (`hot` from the dev server,
            // `build/manifest.json` from a build). Costs nothing until a
            // template actually says `@vite`, which is the opt-in.
            Arc::new(
                TemplateEngine::new(self.base_path.join("resources").join("views"))
                    .with_vite(rainier_view::Vite::new(self.base_path.join("public"))),
            )
        });

        let crypt = self.crypt.unwrap_or_else(|| {
            let keys = app_keys(&self.env);

            match self.config.setting(keys::APP_CIPHER).unwrap_or_default() {
                rainier_crypt::CryptScheme::Native => Encryption::from_keys(keys),
                // A database PHP already filled. Reads and writes the PHP
                // envelope byte for byte; see `rainier_crypt::php` for why
                // this is a migration position rather than a destination.
                rainier_crypt::CryptScheme::Php => Encryption::new(
                    Arc::new(rainier_crypt::PhpEncrypter::new(keys.clone())),
                    Arc::new(rainier_crypt::HmacSigner::new(keys)),
                ),
            }
        });

        // Signed URLs, over the application's own ring where it has one. An
        // application that supplied a hand-built `Encryption` did not hand
        // over a ring, so this falls back to `APP_KEY` — the same ring in
        // every normal setup, and the honest answer when it is not.
        //
        // Bound **here** rather than beside the URL generator: a route can
        // carry `ValidateSignature`, and a route's middleware is built when
        // the router compiles, which is long before there are named routes to
        // generate links to.
        let url_signer = rainier_crypt::UrlSigner::new(
            crypt.keys().cloned().unwrap_or_else(|| app_keys(&self.env)),
        );
        let sessions = self
            .sessions
            .unwrap_or_else(|| SessionManager::new(Arc::new(MemorySessionStore::default())));

        app.instance(self.config);
        app.instance(self.env);
        app.instance(self.events);
        app.instance(Views::new(views));
        app.instance(crypt);
        app.instance(url_signer);
        app.instance(sessions);
        // Nobody handed one over, so this is the in-process default. If the
        // configuration asked for Redis, that is a gap between what the
        // deployment believes and what it has — and the symptom otherwise is a
        // rate limiter that counts to `N ×` its limit with nothing logged.
        let cache = match self.cache {
            Some(cache) => cache,
            None => {
                if let Ok(driver) = app.resolve::<Config>()?.setting(keys::CACHE_DRIVER) {
                    if driver.is_shared() {
                        tracing::warn!(
                            configured = %driver,
                            "CACHE_DRIVER asks for a shared cache but none was built, so \
                             this process is using the in-process one — locks, rate limits \
                             and anything else cached for correctness are per-instance. \
                             Build the store and pass it to `Rainier::with_cache`."
                        );
                    }
                }
                CacheManager::memory()
            }
        };

        // The locks behind `without_overlapping` and `on_one_server` — and
        // anything else that needs one. Bound from the same cache the
        // application uses, so "shared" means the same thing for both.
        app.instance(rainier_cache::LockManager::new(Arc::clone(cache.store())));

        // Rate-limit counters, in the same place as the locks and for the same
        // reason: a deployment decides where its shared state lives once.
        app.instance(crate::limits::RateLimits::over_cache(Arc::clone(cache.store())));

        // And the codes people type — a verification code, a second factor, an
        // email change. Cache-backed, so they expire on their own and there is
        // no purge job to write.
        app.instance(rainier_auth::Challenges::new(Arc::clone(cache.store())));
        app.instance(cache);

        // An empty schedule, so `schedule:list` and `schedule:run` work on a
        // fresh clone. An application replaces it in a provider.
        app.instance(self.schedule.unwrap_or_default());

        // Whatever the application handed over, before the providers run so
        // one of them can resolve it.
        for bind in self.instances {
            bind(&app);
        }

        // No socket routes unless the application declares some, and an
        // upgrade then falls through to the router like any other GET.
        app.instance(self.websockets.unwrap_or_default());

        // Broadcasts default to the log, for the same reason notifications do
        // — except the risk here is the reverse one: publishing a private
        // channel to a relay the application never meant to reach.
        app.instance(self.broadcasting.unwrap_or_else(rainier_broadcast::Broadcasting::log));

        // Notifications default to the log channel. Not to mail: a default that
        // can reach a real person is a default that reaches one from staging.
        app.instance(
            self.notifier.unwrap_or_else(|| {
                rainier_notify::Notifier::new().with(rainier_notify::LogChannel)
            }),
        );
        // An explicit `with_storage` wins; otherwise the declared disks are
        // built, each from its own settings.
        let storage = match self.storage {
            Some(storage) => storage,
            None => build_storage(app.resolve::<Config>()?.as_ref(), &self.base_path).await?,
        };
        app.instance(storage);

        // An explicit `with_database` wins; otherwise the declared connections
        // are opened, each from its own settings. Both the manager and its
        // default connection are bound, because they answer different
        // questions: `Database` is "the application's database", which is all
        // most code ever needs, and `DatabaseManager` is "which of them", which
        // only a query that names one asks.
        let databases = match self.database {
            Some(database) => Some(DatabaseManager::from(database)),
            None => {
                build_databases(app.resolve::<Env>()?.as_ref(), app.resolve::<Config>()?.as_ref())
                    .await?
            }
        };
        if let Some(databases) = databases {
            app.instance(databases.default_connection().clone());
            app.instance(databases);
        }

        // Bound only when the application declared some, so nothing gains an
        // empty registry it did not ask for.
        if let Some(jobs) = &self.jobs {
            app.instance_arc(Arc::clone(jobs));
        }

        let queue = match self.queue {
            Some(queue) => Some(queue),
            None => {
                let resources = queue_resources(&app, self.jobs.unwrap_or_default())?;
                build_queues(
                    app.resolve::<Env>()?.as_ref(),
                    app.resolve::<Config>()?.as_ref(),
                    &resources,
                )
                .await?
            }
        };
        if let Some(queue) = queue {
            // Given the same locks as everything else, so `unique_id` is
            // enforced against the cache the application actually shares.
            // Without this a job declaring one is dispatched anyway, with a
            // warning — degraded rather than silently broken.
            app.instance(queue.with_locks(rainier_cache::LockManager::new(Arc::clone(
                app.resolve::<CacheManager>()?.store(),
            ))));
        }
        if let Some(mailer) = self.mailer {
            app.instance(mailer);
        }

        // Providers register before the router compiles, so a middleware that
        // resolves a service gets one a provider bound.
        for provider in self.providers {
            app.register_arc(provider)?;
        }

        // Compiled once and shared: the kernel serves it, and the container
        // holds the same table so `route:list` can describe what is actually
        // being served rather than a second compilation of it.
        //
        // This is also where every `resolved`/`deferred` middleware stage is
        // built, which is why it happens after the providers and before the
        // server starts.
        let compiled = Arc::new(self.router.compile(&app)?);
        let urls = Arc::new(
            UrlGenerator::from_routes(compiled.named_routes())
                .with_base(app.resolve::<Config>()?.get_or(keys::APP_URL, String::new())),
        );

        // Signed links, over the same key ring everything else uses — so
        // rotating a key retires the links it signed, which is correct and
        // worth knowing before rotating one.
        app.instance(crate::signed::SignedUrls::new(
            Arc::clone(&urls),
            app.resolve::<rainier_crypt::UrlSigner>()?,
        ));

        let mut global = self.middleware.global_stack(&app)?;
        let settings = app.resolve::<Config>()?;

        // Outside everything the application registered, so it compresses
        // whatever any of it produced.
        if settings.get_or(keys::SERVER_COMPRESSION, false) {
            global.insert(0, Arc::new(rainier_middleware::Compress::new()));
        }

        // Outside even that: a timeout that sits inside the throttle cannot
        // cancel time spent in the throttle.
        let timeout_secs = settings.get_or(keys::SERVER_REQUEST_TIMEOUT_SECS, 0u64);
        if timeout_secs > 0 {
            global.insert(0, Arc::new(rainier_middleware::Timeout::seconds(timeout_secs)));
        }

        let kernel = Kernel::from_shared(Arc::clone(&compiled), global)
            .with_debug(app.resolve::<Config>()?.get_or(keys::APP_DEBUG, false));

        app.instance_arc(Arc::clone(&compiled));
        app.instance_arc(urls);
        app.instance(self.middleware);
        app.instance(kernel);

        app.boot().await?;

        // Last, because a provider's `boot` can still add scheduled tasks. A
        // task declaring `on_one_server` over an in-process lock is a
        // guarantee that is not one, and every process gets told — but only
        // `schedule:run` refuses, because that is the one whose guarantees are
        // at stake. A web container refusing to serve HTTP over a scheduling
        // concern would be a larger outage than the one being prevented.
        crate::scheduling::warn_if_locks_are_not_shared(&app);

        // The same question one layer over: a route that rate-limits against a
        // per-process counter permits its limit once per replica, and looks
        // entirely correct while doing it.
        crate::limits::warn_if_rate_limits_are_not_shared(&app, &compiled);

        Ok(app)
    }
}

/// The configuration every application starts with, read from the environment.
///
/// Fails only on a driver name outside its [closed
/// set](rainier_support::Setting) — `QUEUE_DRIVER=databse` is a deployment
/// mistake, and running on the default instead of the driver somebody asked for
/// is not a recovery. Everything else here has a total parse: a `SERVER_PORT`
/// that is not a number falls back, because the alternative is refusing to boot
/// over a trailing space.
fn default_config(env: &Env, base_path: &std::path::Path) -> Result<Config> {
    let config = Config::new();

    config.set(keys::APP_NAME, env.string("APP_NAME", "Rainier"))?;
    config.set(keys::APP_ENV, env.setting("APP_ENV")?)?;
    config.set(keys::APP_DEBUG, env.bool("APP_DEBUG", false))?;
    config.set(keys::APP_URL, env.string("APP_URL", "http://localhost:8000"))?;
    // A closed set, like the drivers: `LOG_FORMAT=jsn` on a production box
    // would otherwise log prose into something that wanted objects, and say
    // nothing.
    config.set(keys::LOG_FORMAT, env.setting("LOG_FORMAT")?)?;

    // A closed set for the same reason the drivers are: writing the wrong
    // envelope is not a preference that degrades, it is a column nothing can
    // read.
    config.set(keys::APP_CIPHER, env.setting("APP_CIPHER")?)?;
    config.set(keys::APP_BASE_PATH, base_path.to_string_lossy().into_owned())?;

    config.set(keys::SERVER_HOST, env.string("SERVER_HOST", "127.0.0.1"))?;
    config.set(keys::SERVER_PORT, env.int("SERVER_PORT", 8000) as u16)?;
    config.set(keys::SERVER_MAX_BODY_BYTES, env.int("SERVER_MAX_BODY", 2 * 1024 * 1024) as u64)?;
    config.set(
        keys::SERVER_REQUEST_TIMEOUT_SECS,
        env.int("SERVER_REQUEST_TIMEOUT", 0).max(0) as u64,
    )?;
    config.set(keys::SERVER_COMPRESSION, env.bool("SERVER_COMPRESSION", false))?;

    config.set(keys::DATABASE_URL, env.string("DATABASE_URL", "sqlite::memory:"))?;

    // One local disk, under the base path, so a fresh clone has working storage
    // without a `filesystems` section. An application declares the rest —
    // `Rainier::with_disks`, or `config.merge("filesystems", …)` — and each of
    // those disks carries its own driver, bucket, endpoint and credentials.
    //
    // The default *name* comes from the environment because which disk a
    // deployment writes to is a deployment's decision; naming one it never
    // declared fails at boot rather than falling back to this one.
    config.set(
        keys::FILESYSTEMS,
        rainier_filesystem::Disks::new(env.string("FILESYSTEM_DISK", "local")).with(
            "local",
            rainier_filesystem::DiskConfig::local(base_path.join("storage").join("app")),
        ),
    )?;

    config.set(keys::CACHE_DRIVER, env.setting("CACHE_DRIVER")?)?;
    config.set(keys::CACHE_REDIS_URL, env.string("REDIS_URL", "redis://127.0.0.1:6379/"))?;
    config.set(keys::CACHE_MEMCACHED_URL, env.string("MEMCACHED_URL", "127.0.0.1:11211"))?;

    config.set(keys::SESSION_DRIVER, env.setting("SESSION_DRIVER")?)?;
    config.set(keys::SESSION_LIFETIME, env.int("SESSION_LIFETIME", 7200))?;
    config.set(keys::SESSION_COOKIE, env.string("SESSION_COOKIE", "rainier_session"))?;
    config.set(keys::SESSION_SECURE, env.bool("SESSION_SECURE", false))?;

    config.set(keys::QUEUE_DRIVER, env.setting("QUEUE_DRIVER")?)?;
    config.set(keys::QUEUE_DEFAULT, env.string("QUEUE_DEFAULT", "default"))?;

    config.set(keys::KAFKA_BROKERS, env.string("KAFKA_BROKERS", ""))?;
    config.set(keys::KAFKA_GROUP, env.string("KAFKA_GROUP", "rainier"))?;
    config.set(keys::KAFKA_TOPIC_PREFIX, env.string("KAFKA_TOPIC_PREFIX", ""))?;
    config.set(keys::KAFKA_BROADCAST_TOPIC, env.string("KAFKA_BROADCAST_TOPIC", "broadcasts"))?;
    config.set(keys::KAFKA_TLS, env.bool("KAFKA_TLS", false))?;
    config.set(keys::KAFKA_USERNAME, env.string("KAFKA_USERNAME", ""))?;
    config.set(keys::KAFKA_PASSWORD, env.string("KAFKA_PASSWORD", ""))?;
    config.set(keys::KAFKA_SASL_MECHANISM, env.string("KAFKA_SASL_MECHANISM", "plain"))?;

    config.set(keys::MAIL_DRIVER, env.setting("MAIL_DRIVER")?)?;
    config.set(keys::MAIL_FROM_ADDRESS, env.string("MAIL_FROM", "hello@example.com"))?;
    config.set(keys::MAIL_FROM_NAME, env.string("MAIL_FROM_NAME", "Rainier"))?;
    config.set(keys::MAIL_ALWAYS_TO, env.string("MAIL_ALWAYS_TO", ""))?;
    config.set(keys::MAIL_FILE_PATH, env.string("MAIL_FILE_PATH", "storage/mail"))?;
    config.set(keys::MAIL_HOST, env.string("MAIL_HOST", ""))?;
    config.set(keys::MAIL_PORT, env.int("MAIL_PORT", 0))?;
    config.set(keys::MAIL_USERNAME, env.string("MAIL_USERNAME", ""))?;
    config.set(keys::MAIL_PASSWORD, env.string("MAIL_PASSWORD", ""))?;
    config.set(keys::MAIL_ENCRYPTION, env.setting("MAIL_ENCRYPTION")?)?;
    config.set(keys::MAIL_TIMEOUT, env.int("MAIL_TIMEOUT", 30))?;
    config.set(keys::MAIL_POSTMARK_TOKEN, env.string("MAIL_POSTMARK_TOKEN", ""))?;
    config.set(keys::MAIL_MAILGUN_DOMAIN, env.string("MAIL_MAILGUN_DOMAIN", ""))?;
    config.set(keys::MAIL_MAILGUN_SECRET, env.string("MAIL_MAILGUN_SECRET", ""))?;
    config.set(keys::MAIL_MAILGUN_ENDPOINT, env.string("MAIL_MAILGUN_ENDPOINT", ""))?;
    config.set(keys::MAIL_SENDGRID_KEY, env.string("MAIL_SENDGRID_KEY", ""))?;
    config.set(keys::MAIL_RESEND_KEY, env.string("MAIL_RESEND_KEY", ""))?;

    Ok(config)
}

/// Build the [`Storage`] the application's `filesystems` section declares.
///
/// Every disk is built from **its own** declaration, which is the whole reason
/// this reads a section rather than a driver name and one set of connection
/// settings: two disks on two services share no endpoint and no credentials,
/// and building the second from the first's connector gives it the right bucket
/// name pointed at the wrong host. That does not raise — it reads an empty
/// prefix, which is indistinguishable from an empty bucket.
///
/// A tree with no section at all still gets a working local disk, so a `Config`
/// assembled by hand — a test, an embedded use — does not have to know this
/// section exists. A section that *is* there and does not make sense is an
/// error: `default` naming a disk nobody declared has no safe interpretation.
async fn build_storage(config: &Config, base_path: &std::path::Path) -> Result<Storage> {
    if !config.has(keys::FILESYSTEMS) {
        return Ok(Storage::local(base_path.join("storage").join("app")));
    }

    // `require` rather than `get`: `get` answers `None` for both "nothing
    // declared" and "declared, and wrong", and the second must not read as the
    // first and quietly leave every upload in a container's directory.
    config.require(keys::FILESYSTEMS)?.build().await
}

/// Open the database connections the application declared, if it declared any.
///
/// Two ways to declare them, and **never both at once**. `DATABASE_URL` is one
/// connection written as one string, which is the whole configuration for
/// nearly every application; a `databases` section is the one that can express
/// a replica or a warehouse. Each names the *default* connection, so having
/// both is two answers to one question.
///
/// This refuses to pick between them rather than applying a precedence rule,
/// and it is the loudest of the available options on purpose. Whichever
/// declaration lost would still be sitting in the configuration, read by
/// everyone who opens the file and used by nothing — so repointing the database
/// by editing the visible one would review cleanly, deploy cleanly and change
/// nothing. Then the query runs against the *other* database and **answers**:
/// rows come back, the types match, the page renders. There is no failure to
/// notice, which is what makes silently preferring one the worst outcome here
/// and a boot failure the best.
///
/// Declaring neither opens nothing. That is not a degraded mode — an
/// application without a database should not have one invented for it, and a
/// seeded `sqlite::memory:` would accept every statement, migrate cleanly and
/// answer every question about the application's own data with no rows.
async fn build_databases(env: &Env, config: &Config) -> Result<Option<DatabaseManager>> {
    let declared = config.has(keys::DATABASES);

    // The environment, not the configuration tree: `database.url` is seeded
    // with a fallback whether or not a deployment set one, and a fallback
    // nobody chose must not read as a second declaration.
    let url = env.get("DATABASE_URL").filter(|url| !url.trim().is_empty());

    match (declared, url) {
        (true, Some(_)) => Err(Error::internal(
            "`DATABASE_URL` is set and a `databases` section is declared, and both name the \
             default database connection. Rainier will not choose between them: the one that \
             lost would stay in the configuration being read by whoever changes it next, and a \
             query against the one that won comes back with rows rather than an error, so \
             nothing would ever report the mistake. Keep one — drop the section and let \
             `DATABASE_URL` declare the single connection, or unset `DATABASE_URL` and declare \
             every connection in the section. When the platform injects the variable and you \
             need more than one connection, read it while building: \
             `Databases::from_url(&builder.env().require(\"DATABASE_URL\")?)?.with(\"replica\", …)`",
        )),

        // `require` rather than `get`: `get` answers `None` for both "nothing
        // declared" and "declared, and wrong", and the second must not read as
        // the first and quietly leave the application with no database at all.
        (true, None) => Ok(Some(config.require(keys::DATABASES)?.build().await?)),

        (false, Some(url)) => {
            let databases = Databases::from_url(&url)?;

            // Written into the tree, so `config.get(keys::DATABASES)` describes
            // what was actually opened rather than answering `None` for a
            // database the application demonstrably has.
            config.set(keys::DATABASES, databases.clone())?;
            Ok(Some(databases.build().await?))
        }

        (false, None) => Ok(None),
    }
}

/// Build the queue connections the application declared, if it declared any.
///
/// The same rule as [`build_databases`], and the reason to hold it here is
/// stronger rather than weaker. A query against the wrong database at least
/// returns something a person could look at; a job pushed to the wrong backend
/// returns an id and then waits in a store no worker drains. Nothing fails, so
/// nothing is logged, and there is no failed-job row — the job never failed. It
/// was never run.
async fn build_queues(
    env: &Env,
    config: &Config,
    resources: &QueueResources,
) -> Result<Option<QueueManager>> {
    let declared = config.has(keys::QUEUES);
    let driver = env.get("QUEUE_DRIVER").filter(|driver| !driver.trim().is_empty());

    match (declared, driver) {
        (true, Some(_)) => Err(Error::internal(
            "`QUEUE_DRIVER` is set and a `queues` section is declared, and both name the default \
             queue connection. Rainier will not choose between them: a dispatch to the one that \
             lost is still accepted, still returns an id, and then waits in a store nothing \
             drains — so unlike a misconfigured database, this one reports nothing at all. Keep \
             one — drop the section and let `QUEUE_DRIVER` declare the single connection, or \
             unset `QUEUE_DRIVER` and declare every connection in the section",
        )),

        (true, None) => Ok(Some(config.require(keys::QUEUES)?.build(resources).await?)),

        (false, Some(_)) => {
            let queues = queues_from_env(env)?;
            config.set(keys::QUEUES, queues.clone())?;
            Ok(Some(queues.build(resources).await?))
        }

        (false, None) => Ok(None),
    }
}

/// The one connection `QUEUE_DRIVER` declares, named after its driver.
///
/// Named after the driver rather than something like `default` because that is
/// already this section's convention for a connection nobody named — see
/// [`Connections`](rainier_queue::Connections) — and because it is the one name
/// that cannot be a lie about what the connection is.
///
/// Each driver reads the settings it needs from the environment beside it, and
/// nothing else: there is no shared client for a second connection to inherit,
/// because there is no second connection. A driver whose settings are missing
/// fails here, naming the variable, rather than connecting to whatever a
/// default would have pointed at.
fn queues_from_env(env: &Env) -> Result<QueueConnections> {
    let driver: QueueDriver = env.setting("QUEUE_DRIVER")?;

    let connection: ConnectionConfig = match driver {
        QueueDriver::Sync => ConnectionConfig::sync(),
        QueueDriver::Memory => ConnectionConfig::memory(),
        QueueDriver::Database => ConnectionConfig::database(),

        // The same variable and the same fallback the cache reads, because
        // "we have a Redis at this URL" is one fact about a deployment. A
        // fallback is safe to have here in a way it is not for SQS: this
        // connection is opened at boot, so a localhost Redis that is not there
        // stops the process rather than accepting jobs into nothing.
        QueueDriver::Redis => {
            RedisConnection::new(env.string("REDIS_URL", "redis://127.0.0.1:6379/")).into()
        }

        // No fallback: an SQS queue *is* a URL, so there is nothing to guess
        // that would not be a queue in somebody else's account.
        QueueDriver::Sqs => SqsConnection::new(env.require("SQS_QUEUE_URL")?).into(),

        QueueDriver::Kafka => {
            // Empties dropped, so `KAFKA_BROKERS=` and a stray trailing comma
            // both arrive as "no brokers" and are refused as such, rather than
            // as a broker whose host is the empty string.
            let brokers: Vec<String> = env
                .string("KAFKA_BROKERS", "")
                .split(',')
                .map(str::trim)
                .filter(|broker| !broker.is_empty())
                .map(str::to_string)
                .collect();

            let mut kafka = KafkaConnection::new(brokers);
            if let Some(group) = env.get("KAFKA_GROUP").filter(|g| !g.trim().is_empty()) {
                kafka = kafka.group(group);
            }
            if let Some(prefix) = env.get("KAFKA_TOPIC_PREFIX").filter(|p| !p.trim().is_empty()) {
                kafka = kafka.topic_prefix(prefix);
            }
            kafka.into()
        }
    };

    let name = driver.as_str();
    Ok(QueueConnections::new(name).with(name, connection))
}

/// What a queue connection needs that the configuration tree cannot hold.
///
/// The registry and the container are always there. The database is there only
/// when the application has one, and a `database` connection declared without
/// one fails at boot naming the missing piece rather than becoming something
/// else. The lock store is the cache the rest of the application shares, so
/// `Job::unique_id` and Kafka's partition leases exclude whatever the
/// deployment's locks exclude — including, when that is an in-process cache,
/// nothing, which the Kafka driver refuses outright.
fn queue_resources(app: &Application, jobs: Arc<JobRegistry>) -> Result<QueueResources> {
    let mut resources = QueueResources::new(jobs, Arc::clone(app.container()))
        .with_lock_store(Arc::clone(app.resolve::<CacheManager>()?.store()));

    if let Some(database) = app.try_resolve::<Database>() {
        resources = resources.with_database(database.as_ref().clone());
    }
    Ok(resources)
}

/// The keys encryption and signing use.
///
/// `APP_KEY` is the current key; `APP_PREVIOUS_KEYS` is a comma-separated list
/// of retired ones, still needed to read what they wrote. See
/// [`rainier_crypt`].
fn app_keys(env: &Env) -> KeyRing {
    let previous: Vec<String> =
        env.get("APP_PREVIOUS_KEYS").unwrap_or_default().split(',').map(str::to_string).collect();

    match env.get("APP_KEY").filter(|key| !key.trim().is_empty()) {
        Some(key) => match KeyRing::from_base64(&key, &previous) {
            Ok(ring) => ring,
            Err(e) => {
                // Falling back to a random key would "work" and quietly make
                // every existing encrypted value unreadable, so say plainly
                // what is wrong instead of hiding it in a decrypt failure
                // three screens later.
                tracing::error!(
                    error = %e.message(),
                    "APP_KEY is not usable; generating a temporary key. Encrypted values \
                     written before this boot cannot be read, and values written now cannot \
                     be read after a restart."
                );
                KeyRing::new(Key::generate())
            }
        },
        None => {
            tracing::warn!(
                "APP_KEY is not set; generating a temporary key. Set one with \
                 `APP_KEY={}` or nothing encrypted will survive a restart.",
                Key::generate().to_base64()
            );
            KeyRing::new(Key::generate())
        }
    }
}

/// The middleware every application starts with.
///
/// Only the input normalisers are **global**, because they are the ones whose
/// absence produces confusing bugs rather than missing features. Everything
/// else is registered as an **alias** and opted into per route, since applying
/// CORS, throttling, sessions or authentication everywhere by default is
/// nearly always wrong.
fn default_middleware() -> MiddlewareRegistry {
    let registry = MiddlewareRegistry::new();

    // Input normalisation only. Everything else — CORS, throttling, sessions,
    // authentication — is opted into per route or per group, because applying
    // any of it everywhere by default is how an API ends up allocating session
    // rows nobody reads.
    //
    // The groups Rainier ships are functions in [`crate::groups`], not entries
    // here: there is nothing to look them up by.
    registry.global(crate::groups::normalise_input());

    registry
}

fn install_tracing(env: &Env, config: &Config) {
    let environment = config.get(keys::APP_ENV).unwrap_or_default().to_string();

    // A second subscriber would panic, and an application that installs its
    // own should keep it — so a failure here is expected and ignored.
    crate::observability::TelemetrySettings::from_config(config)
        .install_logging(&environment, &env.string("RUST_LOG", "info"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups;
    use rainier_http::{Method, Request, StatusCode};
    use rainier_session::SessionRequestExt as _;
    use rainier_session::StartSession;

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
    async fn booting_binds_the_core_services() {
        let app = app().await;

        assert!(app.resolve::<Config>().is_ok());
        assert!(app.resolve::<Dispatcher>().is_ok());
        assert!(app.resolve::<Views>().is_ok());
        assert!(app.resolve::<Kernel>().is_ok());
        assert!(app.resolve::<UrlGenerator>().is_ok());
        assert!(app.resolve::<MiddlewareRegistry>().is_ok());
        assert!(app.is_booted());
    }

    #[tokio::test]
    async fn the_kernel_serves_the_declared_routes() {
        let app = app().await;
        let kernel = app.resolve::<Kernel>().unwrap();

        let response =
            kernel.handle_request(Request::builder().method(Method::GET).uri("/").build()).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn named_routes_reach_the_url_generator() {
        let app = app().await;
        let urls = app.resolve::<UrlGenerator>().unwrap();

        assert_eq!(urls.route("posts.show", &[("post", "7")]).unwrap(), "/posts/7");
    }

    #[tokio::test]
    async fn the_default_config_comes_from_the_environment() {
        let app = app().await;
        let config = app.resolve::<Config>().unwrap();

        assert_eq!(config.get(keys::APP_NAME).as_deref(), Some("Rainier"));
        assert_eq!(config.get(keys::SERVER_PORT), Some(8000));
        assert_eq!(config.get(keys::APP_DEBUG), Some(false));
    }

    #[test]
    fn the_default_drivers_are_the_ones_that_need_no_infrastructure() {
        // What a fresh clone gets, and the reason `cargo run -- serve` works
        // with an empty `.env`.
        let config = default_config(&Env::new(), std::path::Path::new(".")).unwrap();

        assert_eq!(config.setting(keys::APP_ENV).unwrap(), rainier_config::AppEnv::Production);
        assert_eq!(config.setting(keys::CACHE_DRIVER).unwrap(), rainier_cache::CacheDriver::Memory);
        assert_eq!(
            config.setting(keys::SESSION_DRIVER).unwrap(),
            rainier_session::SessionDriver::Memory
        );
        assert_eq!(config.setting(keys::QUEUE_DRIVER).unwrap(), rainier_queue::QueueDriver::Sync);
        assert_eq!(config.setting(keys::MAIL_DRIVER).unwrap(), rainier_mail::MailDriver::Log);
    }

    #[test]
    fn a_driver_written_as_its_wire_spelling_reads_back_as_the_enum() {
        let env = Env::parse("CACHE_DRIVER=redis-cluster\nQUEUE_DRIVER=database\nAPP_ENV=local");
        let config = default_config(&env, std::path::Path::new(".")).unwrap();

        assert_eq!(
            config.setting(keys::CACHE_DRIVER).unwrap(),
            rainier_cache::CacheDriver::RedisCluster
        );
        assert_eq!(
            config.setting(keys::QUEUE_DRIVER).unwrap(),
            rainier_queue::QueueDriver::Database
        );
        assert!(config.setting(keys::APP_ENV).unwrap().is_developing());
    }

    #[test]
    fn a_misspelled_driver_is_a_configuration_error_not_a_fallback() {
        // The bug this whole layer exists to prevent: `redys` used to boot on
        // an in-process cache and stay that way until someone noticed the rate
        // limiter letting through N× its limit.
        let env = Env::parse("CACHE_DRIVER=redys");
        let err = default_config(&env, std::path::Path::new(".")).unwrap_err();

        assert!(err.message().contains("CACHE_DRIVER"), "{}", err.message());
        assert!(err.message().contains("`redys`"), "{}", err.message());
        assert!(err.message().contains("`redis-cluster`"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_misspelled_driver_fails_the_boot_rather_than_being_deferred_forever() {
        // `Rainier::new` cannot return a `Result`, so the error waits on the
        // builder. It must not be possible to reach a running application
        // without seeing it.
        let mut builder = Rainier::new(".").without_facades().without_tracing();
        builder.deferred = Some(Error::internal("`QUEUE_DRIVER`: nope"));

        let err = builder.boot().await.unwrap_err();
        assert!(err.message().contains("QUEUE_DRIVER"), "{}", err.message());
    }

    #[tokio::test]
    async fn configuration_can_be_adjusted_while_building() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .configure(|config| {
                config.set("app.name", "Custom").unwrap();
            })
            .boot()
            .await
            .unwrap();

        assert_eq!(app.resolve::<Config>().unwrap().string("app.name").as_deref(), Some("Custom"));
    }

    #[tokio::test]
    async fn the_default_middleware_normalises_input() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.post("/echo", |request: rainier_routing::Req| async move {
                    request.input_or("name", "(absent)")
                });
            })
            .boot()
            .await
            .unwrap();

        let response = app
            .resolve::<Kernel>()
            .unwrap()
            .handle_request(
                Request::builder()
                    .method(Method::POST)
                    .uri("/echo")
                    .json(&serde_json::json!({ "name": "  Ada  " }))
                    .build(),
            )
            .await;

        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "Ada", "TrimStrings should be global");
    }

    #[tokio::test]
    async fn the_shipped_groups_all_build() {
        // Each of these is a stack an application may attach without wiring
        // anything. The one that can fail is `web`, whose session stage is
        // resolved from the container — so this is really asserting that the
        // builder binds a `SessionManager` before it compiles the router.
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.get("/a", || async { "a" }).middleware(groups::api());
                router.get("/b", || async { "b" }).middleware(groups::web());
                router.get("/c", || async { "c" }).middleware(groups::api_throttled(5));
                router.get("/d", || async { "d" }).middleware(groups::session());
                router.get("/e", || async { "e" }).middleware(groups::trust_local_proxies());
                router.get("/f", || async { "f" }).middleware(groups::normalise_input());
            })
            .boot()
            .await;

        assert!(app.is_ok(), "{:?}", app.err().map(|e| e.message().to_string()));
    }

    #[tokio::test]
    async fn booting_binds_encryption_and_sessions() {
        let app = app().await;

        assert!(app.resolve::<Encryption>().is_ok());
        assert!(app.resolve::<SessionManager>().is_ok());
        assert_eq!(app.resolve::<SessionManager>().unwrap().driver(), "memory");
    }

    #[tokio::test]
    async fn encryption_round_trips_through_the_container() {
        let app = app().await;
        let crypt = app.resolve::<Encryption>().unwrap();

        let sealed = crypt.encrypt("a secret").unwrap();
        assert_eq!(crypt.decrypt(&sealed).unwrap(), "a secret");
    }

    #[tokio::test]
    async fn an_app_key_from_the_environment_is_used() {
        let key = Key::generate();
        let sealed = Encryption::from_keys(KeyRing::new(key.clone())).encrypt("before").unwrap();

        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .configure(|_| {})
            .boot()
            .await
            .unwrap();

        // The default boot has no APP_KEY here, so it must *not* read that.
        assert!(app.resolve::<Encryption>().unwrap().decrypt(&sealed).is_err());

        // With the key on the ring, it must.
        let with_key = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_encryption(Encryption::from_keys(KeyRing::new(key)))
            .boot()
            .await
            .unwrap();

        assert_eq!(with_key.resolve::<Encryption>().unwrap().decrypt(&sealed).unwrap(), "before");
    }

    #[tokio::test]
    async fn the_session_middleware_gives_a_route_a_session() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router
                    .get("/count", |request: rainier_routing::Req| async move {
                        let session = request.session().expect("StartSession ran");
                        let seen: u64 = session.get("seen").unwrap_or(0);
                        session.put("seen", seen + 1).unwrap();
                        format!("{seen}")
                    })
                    .middleware(groups::session());
            })
            .boot()
            .await
            .unwrap();

        let kernel = app.resolve::<Kernel>().unwrap();
        let response = kernel
            .handle_request(Request::builder().method(Method::GET).uri("/count").build())
            .await;

        assert!(response.header("set-cookie").is_some(), "the session should be persisted");
        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "0");
    }

    #[tokio::test]
    async fn a_route_without_the_session_middleware_has_no_session() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.get("/plain", |request: rainier_routing::Req| async move {
                    format!("{}", request.session().is_some())
                });
            })
            .boot()
            .await
            .unwrap();

        let response = app
            .resolve::<Kernel>()
            .unwrap()
            .handle_request(Request::builder().method(Method::GET).uri("/plain").build())
            .await;

        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "false");
    }

    #[tokio::test]
    async fn middleware_that_needs_an_unbound_service_fails_at_boot() {
        // The only way a route's middleware can now fail to attach. A
        // misspelled name is not on the list, because there is no name.
        struct NeverBound;

        let result = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router.get("/a", || async { "a" }).middleware(
                    rainier_middleware::MiddlewareStack::new().resolved(|_: Arc<NeverBound>| {
                        StartSession::new(Arc::new(MemorySessionStore::default()))
                    }),
                );
            })
            .boot()
            .await;

        let err = result.err().expect("boot should fail");
        assert!(err.message().contains("/a"), "{}", err.message());
        assert!(err.message().contains("NeverBound"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_route_attaches_middleware_by_value() {
        // The headline of the whole design: the middleware itself, no name.
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_routes(|router| {
                router
                    .get("/guarded", || async { "ok" })
                    .middleware(rainier_middleware::AddHeaders::security_defaults());
            })
            .boot()
            .await
            .unwrap();

        let response = app
            .resolve::<Kernel>()
            .unwrap()
            .handle_request(Request::builder().method(Method::GET).uri("/guarded").build())
            .await;

        assert!(response.header("x-content-type-options").is_some());
    }

    #[tokio::test]
    async fn a_provider_registers_before_the_router_compiles() {
        struct AliasProvider;
        impl ServiceProvider for AliasProvider {
            fn register(&self, app: &Application) -> Result<()> {
                app.instance(42u32);
                Ok(())
            }
        }

        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_provider(AliasProvider)
            .boot()
            .await
            .unwrap();

        assert_eq!(*app.resolve::<u32>().unwrap(), 42);
    }

    #[tokio::test]
    async fn listeners_registered_while_building_survive_into_the_dispatcher() {
        struct Ping;

        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_events(|events| {
                events.listen(|_: Arc<Ping>| async { Ok(()) });
            })
            .boot()
            .await
            .unwrap();

        assert!(app.resolve::<Dispatcher>().unwrap().has_listeners::<Ping>());
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use rainier_cache::CacheExt as _;
    use rainier_filesystem::{FilesystemExt as _, MemoryFilesystem};

    async fn app() -> Arc<Application> {
        Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_storage(Storage::new(Arc::new(MemoryFilesystem::new())))
            .boot()
            .await
            .expect("boots")
    }

    #[tokio::test]
    async fn booting_binds_a_cache_and_a_storage() {
        let app = app().await;

        assert_eq!(app.resolve::<CacheManager>().unwrap().driver(), "memory");
        assert_eq!(app.resolve::<Storage>().unwrap().driver(), "memory");
    }

    #[tokio::test]
    async fn the_default_storage_is_local_under_the_base_path() {
        // A fresh clone should have working storage without configuring one.
        let app = Rainier::new(".").without_facades().without_tracing().boot().await.unwrap();

        assert_eq!(app.resolve::<Storage>().unwrap().driver(), "local");
    }

    #[tokio::test]
    async fn the_cache_round_trips_through_the_container() {
        let app = app().await;
        let cache = app.resolve::<CacheManager>().unwrap();

        cache.put_string("k", "v", None).await.unwrap();
        assert_eq!(cache.get_string("k").await.unwrap().as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn storage_round_trips_through_the_container() {
        let app = app().await;
        let storage = app.resolve::<Storage>().unwrap();

        storage.put_string("uploads/a.txt", "hello").await.unwrap();
        assert_eq!(storage.get_string("uploads/a.txt").await.unwrap().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn a_traversal_through_the_container_is_still_refused() {
        let storage = app().await.resolve::<Storage>().unwrap();
        assert!(storage.get("../../etc/passwd").await.is_err());
    }

    // --- disks from configuration -------------------------------------------

    #[tokio::test]
    async fn the_seeded_section_declares_the_disk_the_default_used_to_be() {
        // The default boot goes through the declarative path now, and has to
        // come out the other side with exactly what it came out with before.
        let app = Rainier::new(".").without_facades().without_tracing().boot().await.unwrap();
        let config = app.resolve::<Config>().unwrap();

        assert_eq!(config.string(keys::FILESYSTEM_DEFAULT).as_deref(), Some("local"));
        assert_eq!(app.resolve::<Storage>().unwrap().driver(), "local");
    }

    #[tokio::test]
    async fn declared_disks_are_reachable_by_name_after_boot() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_disks(
                rainier_filesystem::Disks::new("uploads")
                    .with("uploads", rainier_filesystem::DiskConfig::memory())
                    .with("archive", rainier_filesystem::DiskConfig::memory()),
            )
            .boot()
            .await
            .unwrap();

        let storage = app.resolve::<Storage>().unwrap();

        assert_eq!(storage.driver(), "memory");
        assert!(storage.disk("uploads").is_some());
        assert!(storage.disk("archive").is_some());
        // Still no falling back for one nobody declared.
        assert!(storage.disk("scratch").is_none());
    }

    #[tokio::test]
    async fn a_default_naming_an_undeclared_disk_stops_the_boot() {
        // The alternative is an application whose uploads all went to a
        // directory that goes away with the container, discovered later.
        let error = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_disks(
                rainier_filesystem::Disks::new("uploads")
                    .with("archive", rainier_filesystem::DiskConfig::memory()),
            )
            .boot()
            .await
            .err()
            .expect("the default is not declared");

        assert!(error.message().contains("`uploads`"), "{}", error.message());
    }

    #[tokio::test]
    async fn an_unreadable_section_stops_the_boot_rather_than_reading_as_no_disks() {
        let error = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .configure(|config| {
                config.set("filesystems.disks.uploads.driver", "s4").unwrap();
            })
            .boot()
            .await
            .err()
            .expect("`s4` is not a driver");

        assert!(error.message().contains("filesystems"), "{}", error.message());
    }

    #[tokio::test]
    async fn an_explicit_storage_still_wins_over_the_declared_disks() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_disks(
                rainier_filesystem::Disks::new("uploads")
                    .with("uploads", rainier_filesystem::DiskConfig::local("storage/app")),
            )
            .with_storage(Storage::new(Arc::new(MemoryFilesystem::new())))
            .boot()
            .await
            .unwrap();

        assert_eq!(app.resolve::<Storage>().unwrap().driver(), "memory");
    }
}

#[cfg(test)]
mod database_and_queue_tests {
    use super::*;
    use rainier_container::Container;
    use rainier_database::testing::{fake_database, MemoryConnection};
    use rainier_database::{Dialect, SqliteDatabase};

    /// The two things a queue connection needs that no configuration holds.
    ///
    /// Neither a database nor a lock store, so the drivers that need one say so
    /// by name — which is what the tests below assert.
    fn resources() -> QueueResources {
        QueueResources::new(Arc::new(JobRegistry::new()), Arc::new(Container::new()))
    }

    /// An environment that ignores the process's, so a `DATABASE_URL` exported
    /// in the shell running the suite cannot state a test's premise for it.
    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_map(pairs.iter().copied())
    }

    // --- declaring nothing --------------------------------------------------

    #[tokio::test]
    async fn declaring_nothing_opens_no_database_and_builds_no_queue() {
        // Not a degraded mode. An application with no database should not have
        // one invented for it: a seeded `sqlite::memory:` accepts every
        // statement, migrates cleanly, and answers every question about the
        // application's own data with no rows.
        let config = Config::new();

        assert!(build_databases(&env(&[]), &config).await.unwrap().is_none());
        assert!(build_queues(&env(&[]), &config, &resources()).await.unwrap().is_none());
    }

    // --- one scalar, which is what nearly every application has --------------

    #[tokio::test]
    async fn a_database_url_alone_still_opens_the_one_database() {
        let built =
            build_databases(&env(&[("DATABASE_URL", "sqlite::memory:")]), &Config::new()).await;

        if cfg!(feature = "sea-orm-executor") {
            let manager = built.expect("opens").expect("declared");

            // Reachable both ways, and the same handle either way — a second
            // pool for `sqlite::memory:` would be a second, empty database.
            assert!(manager.connection(rainier_database::Databases::DEFAULT_NAME).is_some());
            assert_eq!(manager.resolve(None).unwrap().dialect(), Dialect::Sqlite);
        } else {
            // Loud, and naming the fix. Never a substitution.
            let err = built.err().expect("no executor was compiled in");
            assert!(err.message().contains("sea-orm-executor"), "{}", err.message());
        }
    }

    #[tokio::test]
    async fn a_queue_driver_alone_still_builds_the_one_queue() {
        let manager =
            build_queues(&env(&[("QUEUE_DRIVER", "memory")]), &Config::new(), &resources())
                .await
                .unwrap()
                .expect("declared");

        // Named after its driver, and the default, and one backend under both.
        assert_eq!(manager.queue().name(), "memory");
        assert!(manager.connection("memory").is_some());
    }

    #[tokio::test]
    async fn what_a_scalar_declared_is_written_into_the_configuration() {
        // Otherwise `config.get(keys::QUEUES)` answers `None` for a queue the
        // application demonstrably has, and a config dump describes a
        // deployment that is not the one running.
        let config = Config::new();
        build_queues(&env(&[("QUEUE_DRIVER", "sync")]), &config, &resources()).await.unwrap();

        assert_eq!(config.string(keys::QUEUE_DEFAULT_CONNECTION).as_deref(), Some("sync"));
        assert_eq!(config.require(keys::QUEUES).unwrap().names().collect::<Vec<_>>(), vec!["sync"]);
    }

    #[test]
    fn each_driver_declares_a_connection_named_after_itself() {
        for (driver, settings) in [
            ("sync", &[][..]),
            ("memory", &[]),
            ("database", &[]),
            ("redis", &[]),
            ("sqs", &[("SQS_QUEUE_URL", "https://sqs.example.com/0/jobs")]),
            ("kafka", &[("KAFKA_BROKERS", "one:9092,two:9092")]),
        ] {
            let mut pairs = vec![("QUEUE_DRIVER", driver)];
            pairs.extend_from_slice(settings);

            let queues = queues_from_env(&env(&pairs)).expect(driver);

            assert_eq!(queues.default_name(), driver);
            assert_eq!(queues.get(driver).expect(driver).driver().as_str(), driver);
        }
    }

    #[test]
    fn an_sqs_connection_with_no_queue_url_names_the_variable() {
        // No fallback: an SQS queue *is* a URL, so anything guessed here is a
        // queue in somebody else's account that accepts every job and runs none.
        let err = queues_from_env(&env(&[("QUEUE_DRIVER", "sqs")])).unwrap_err();

        assert!(err.message().contains("SQS_QUEUE_URL"), "{}", err.message());
    }

    #[test]
    fn an_empty_broker_list_is_no_brokers_rather_than_one_empty_broker() {
        let queues = queues_from_env(&env(&[("QUEUE_DRIVER", "kafka"), ("KAFKA_BROKERS", " , ")]))
            .expect("declaring it is not what fails");

        let rendered = format!("{:?}", queues.get("kafka").unwrap());
        assert!(rendered.contains("brokers: []"), "{rendered}");
    }

    // --- a section, which is what the rest have ------------------------------

    #[tokio::test]
    async fn a_declared_queue_section_is_built_and_reachable_by_name() {
        let config = Config::new();
        config
            .set(
                keys::QUEUES,
                QueueConnections::new("primary")
                    .with("primary", ConnectionConfig::memory())
                    .with("bulk", ConnectionConfig::memory()),
            )
            .unwrap();

        let manager =
            build_queues(&env(&[]), &config, &resources()).await.unwrap().expect("declared");

        assert!(manager.connection("primary").is_some());
        assert!(manager.connection("bulk").is_some());
        // Still no falling back for one nobody declared.
        assert!(manager.connection("scratch").is_none());
    }

    #[cfg(feature = "sea-orm-executor")]
    #[tokio::test]
    async fn a_declared_database_section_is_opened_and_reachable_by_name() {
        let config = Config::new();
        config
            .set(
                keys::DATABASES,
                Databases::new("primary")
                    .with("primary", SqliteDatabase::in_memory())
                    .with("reporting", SqliteDatabase::in_memory()),
            )
            .unwrap();

        let manager = build_databases(&env(&[]), &config).await.unwrap().expect("declared");

        assert!(manager.connection("primary").is_some());
        assert!(manager.connection("reporting").is_some());
        assert!(manager.connection("reportng").is_none());
    }

    #[tokio::test]
    async fn a_database_default_naming_an_undeclared_connection_stops_the_boot() {
        // Checked before anything is opened, so it holds without an executor:
        // falling back would answer, from a database nobody named, in a way no
        // caller can tell from a correct answer.
        let config = Config::new();
        config
            .set(
                keys::DATABASES,
                Databases::new("primary").with("reporting", SqliteDatabase::in_memory()),
            )
            .unwrap();

        let err = build_databases(&env(&[]), &config).await.err().expect("`primary` is undeclared");

        assert!(err.message().contains("`primary`"), "{}", err.message());
        assert!(err.message().contains("`reporting`"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_queue_default_naming_an_undeclared_connection_stops_the_boot() {
        let config = Config::new();
        config
            .set(
                keys::QUEUES,
                QueueConnections::new("primary").with("bulk", ConnectionConfig::memory()),
            )
            .unwrap();

        let err = build_queues(&env(&[]), &config, &resources())
            .await
            .err()
            .expect("`primary` is undeclared");

        assert!(err.message().contains("`primary`"), "{}", err.message());
        assert!(err.message().contains("`bulk`"), "{}", err.message());
    }

    // --- both, which is the one that has no safe reading ---------------------

    #[tokio::test]
    async fn a_database_url_beside_a_declared_section_is_refused_rather_than_resolved() {
        // The loudest available answer, and the reason is that every quieter
        // one is invisible: the losing declaration stays in the file being
        // read, and the query that runs against the winner answers with rows.
        let config = Config::new();
        config
            .set(
                keys::DATABASES,
                Databases::new("primary").with("primary", SqliteDatabase::in_memory()),
            )
            .unwrap();

        let err = build_databases(&env(&[("DATABASE_URL", "sqlite::memory:")]), &config)
            .await
            .err()
            .expect("two declarations of the default connection");

        assert!(err.message().contains("DATABASE_URL"), "{}", err.message());
        assert!(err.message().contains("databases"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_queue_driver_beside_a_declared_section_is_refused_rather_than_resolved() {
        let config = Config::new();
        config
            .set(
                keys::QUEUES,
                QueueConnections::new("primary").with("primary", ConnectionConfig::memory()),
            )
            .unwrap();

        let err = build_queues(&env(&[("QUEUE_DRIVER", "memory")]), &config, &resources())
            .await
            .err()
            .expect("two declarations of the default connection");

        assert!(err.message().contains("QUEUE_DRIVER"), "{}", err.message());
        assert!(err.message().contains("queues"), "{}", err.message());
    }

    #[tokio::test]
    async fn an_empty_scalar_is_not_a_declaration() {
        // A platform that sets the variable to nothing has not named a
        // database, and refusing the boot over it would be a conflict with
        // something that declares no connection at all.
        let config = Config::new();
        config
            .set(
                keys::DATABASES,
                Databases::new("primary").with("primary", SqliteDatabase::in_memory()),
            )
            .unwrap();

        let built = build_databases(&env(&[("DATABASE_URL", "  ")]), &config).await;

        if cfg!(feature = "sea-orm-executor") {
            assert!(built.unwrap().is_some());
        } else {
            assert!(built.unwrap_err().message().contains("sea-orm-executor"));
        }
    }

    // --- through the builder -------------------------------------------------

    #[tokio::test]
    async fn a_handed_over_database_is_bound_as_both_the_handle_and_the_manager() {
        // The single-database application, unchanged: `with_database` binds a
        // `Database`, which is what every repository and the `DB` facade take.
        let (database, _) = fake_database(MemoryConnection::new(Dialect::MySql));

        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_database(database)
            .boot()
            .await
            .unwrap();

        assert_eq!(app.resolve::<Database>().unwrap().dialect(), Dialect::MySql);
        // …and the manager over it, with no names, because there is nothing to
        // distinguish.
        assert_eq!(app.resolve::<DatabaseManager>().unwrap().connection_names().count(), 0);
    }

    #[tokio::test]
    async fn a_handed_over_queue_is_still_the_one_that_is_bound() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_queue(QueueManager::fake())
            .boot()
            .await
            .unwrap();

        assert!(app.resolve::<QueueManager>().unwrap().is_faking());
    }

    #[tokio::test]
    async fn declared_queues_are_reachable_by_name_after_boot() {
        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_queues(
                QueueConnections::new("primary")
                    .with("primary", ConnectionConfig::memory())
                    .with("bulk", ConnectionConfig::memory()),
            )
            .boot()
            .await
            .unwrap();

        let manager = app.resolve::<QueueManager>().unwrap();

        assert!(manager.connection("primary").is_some());
        assert!(manager.connection("bulk").is_some());
        assert!(manager.connection("scratch").is_none());
    }

    #[tokio::test]
    async fn a_declared_database_default_nobody_declared_fails_the_boot() {
        let err = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_databases(
                Databases::new("primary").with("reporting", SqliteDatabase::in_memory()),
            )
            .boot()
            .await
            .err()
            .expect("the default is not declared");

        assert!(err.message().contains("`primary`"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_declared_job_registry_reaches_the_queue_the_framework_builds() {
        // Without it the framework's own queue could dispatch nothing: a job
        // travels as a name, and an empty registry maps no name back to code.
        use rainier_queue::{Job, JobContext};
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        struct Ping;

        #[async_trait::async_trait]
        impl Job for Ping {
            const NAME: &'static str = "test.bootstrap-ping";
            async fn handle(&self, _: &JobContext) -> Result<()> {
                Ok(())
            }
        }

        let app = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_jobs(JobRegistry::new().with::<Ping>())
            .with_queues(
                QueueConnections::new("primary").with("primary", ConnectionConfig::memory()),
            )
            .boot()
            .await
            .unwrap();

        let manager = app.resolve::<QueueManager>().unwrap();
        manager.dispatch(Ping).await.unwrap();

        assert_eq!(manager.connection("primary").unwrap().size("default").await.unwrap(), 1);
        assert!(app.resolve::<JobRegistry>().unwrap().names().contains(&"test.bootstrap-ping"));
    }

    #[tokio::test]
    async fn a_database_queue_connection_without_a_database_names_what_is_missing() {
        // The one resource a `queues` section cannot declare for itself, and a
        // connection that quietly became something else would accept every job.
        let err = Rainier::new(".")
            .without_facades()
            .without_tracing()
            .with_queues(
                QueueConnections::new("primary").with("primary", ConnectionConfig::database()),
            )
            .boot()
            .await
            .err()
            .expect("no database was declared");

        assert!(err.message().contains("`primary`"), "{}", err.message());
        assert!(err.message().contains("database"), "{}", err.message());
    }
}
