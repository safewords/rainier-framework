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
use rainier_database::Database;
use rainier_events::Dispatcher;
use rainier_filesystem::Storage;
use rainier_mail::Mailer;
use rainier_middleware::MiddlewareRegistry;
use rainier_queue::QueueManager;
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
    pub fn with_database(mut self, database: Database) -> Self {
        self.database = Some(database);
        self
    }

    /// Use this queue.
    pub fn with_queue(mut self, queue: QueueManager) -> Self {
        self.queue = Some(queue);
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
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage = Some(storage);
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
            Arc::new(TemplateEngine::new(self.base_path.join("resources").join("views")))
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
        app.instance(
            self.storage
                .unwrap_or_else(|| Storage::local(self.base_path.join("storage").join("app"))),
        );

        if let Some(database) = self.database {
            app.instance(database);
        }
        if let Some(queue) = self.queue {
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
}
