//! Lookout, wired into the framework — the `lookout` feature.
//!
//! Everything [`rainier_lookout`] exports is re-exported here, so
//! `rainier_framework::lookout::{Lookout, LookoutConfig, Store, …}` is the one
//! path an application needs. What this module adds is the framework's side:
//! what [`Rainier::boot`](crate::Rainier::boot) does with it, the
//! [`migrations`], the console commands, and [`shutdown`].
//!
//! # What boot does
//!
//! With the feature compiled in, boot reads [`LookoutConfig::from_env`]
//! (through the framework's `Env`, with `APP_ENV` from the configuration),
//! applies every [`Rainier::with_lookout`](crate::Rainier::with_lookout)
//! adjustment on top, and — when the result is enabled:
//!
//! 1. builds the [`Lookout`] **before** the tracing subscriber, and installs
//!    its layer unfiltered beside the log lines (see
//!    [`TelemetrySettings::subscriber`](crate::observability::TelemetrySettings::subscriber));
//! 2. builds the store — [`MemoryStore`], a [`DatabaseStore`] on the named
//!    connection, or a [`CacheStore`] on the named cache store — and binds
//!    [`Lookout`] and `Arc<dyn Store>` in the container;
//! 3. reads who a request was for from the
//!    [`Authenticated`](rainier_auth::Authenticated) marker the `Authenticate`
//!    middleware leaves on the response;
//! 4. listens on the application's event bus;
//! 5. puts Lookout's middleware in the global stack, just inside `Trace`;
//! 6. mounts the dashboard at [`Dashboard::path`](config::Dashboard::path),
//!    outside every middleware group, behind the [`LookoutGate`] described
//!    under [access](#who-may-see-the-dashboard);
//! 7. starts the background writer once the application has booted.
//!
//! An invalid `LOOKOUT_*` variable **fails the boot**, like a misspelled
//! `QUEUE_DRIVER`: a typo in a deployment should stop it, not silently record
//! nothing. A disabled Lookout binds nothing and mounts nothing.
//!
//! # Who may see the dashboard
//!
//! In the local environment with
//! [`open_locally`](config::Dashboard::open_locally), everyone. Anywhere else
//! the viewer is authenticated with [`Dashboard::guard`](config::Dashboard::guard)
//! (the default guard when `None`) and must pass
//! [`Dashboard::ability`](config::Dashboard::ability) — `viewLookout` unless
//! changed — on the application's `Gate`. Both are generic over the
//! application's user type, which the framework cannot name, so the
//! application says it once:
//!
//! ```ignore
//! Rainier::new(".")
//!     .with_lookout_viewers::<User>()
//!     .with_instance(Gate::<User>::new().define_simple("viewLookout", |u: &User| u.is_admin))
//! ```
//!
//! Without that call, or without a `Gate<User>` and `AuthManager<User>` bound,
//! or without the ability defined on it, the answer is no — and "no" is the
//! router's own 404.
//!
//! An application whose idea of "may see it" lives on the request rather than
//! the user — groups carried in the session, say — names a predicate instead
//! of a gate ability:
//!
//! ```ignore
//! Rainier::new(".")
//!     .with_lookout_viewers_where(|_user: &User, request: &Request| is_admin(request))
//! ```
//!
//! The predicate runs only for a viewer the guard authenticated, and sees the
//! request with its session loaded.
//!
//! # A subscriber of your own
//!
//! An application that installs its own subscriber (or boots
//! [`without_tracing`](crate::Rainier::without_tracing)) adds Lookout's layer
//! to it itself; boot logs a warning saying it could not. Resolve the booted
//! recorder and install the subscriber after boot — no request, job or
//! command runs before boot returns, so nothing is missed:
//!
//! ```ignore
//! let app = Rainier::new(".").without_tracing().boot().await?;
//! let lookout = app.resolve::<rainier_framework::lookout::Lookout>()?;
//! tracing_subscriber::registry().with(my_layers).with(lookout.layer()).init();
//! ```
//!
//! Keep `RUST_LOG` off Lookout's layer: a global filter decides for every
//! layer, and Lookout needs the `info` spans a `warn` filter would drop. Put
//! the filter on your own layers with `.with_filter(...)`.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use chrono::{DateTime, Utc};
use rainier_auth::{AuthManager, Authenticatable, Gate};
use rainier_config::{Config, Env};
use rainier_console::{Arguments, Command};
use rainier_container::Application;
use rainier_database::Migrator;
use rainier_http::Request;
use rainier_support::{BoxFuture, Error, Result};

pub use rainier_lookout::*;

use crate::keys;

/// The migrations Lookout's database store needs, to merge into the
/// application's own — the same opt-in as `DatabaseQueue::migrations()` and
/// `DatabaseSessionStore::migrations()`:
///
/// ```ignore
/// app.instance(
///     Migrator::new()
///         .merge(app_migrations())
///         .merge(rainier_framework::lookout::migrations()),
/// );
/// ```
///
/// Only needed with `LOOKOUT_STORE=database`. The tables are named
/// `lookout_*`; the migrations `rainier_lookout_000N_…`.
pub fn migrations() -> Migrator {
    DatabaseStore::migrations()
}

/// One adjustment to the configuration, from
/// [`Rainier::with_lookout`](crate::Rainier::with_lookout).
pub(crate) type Adjustment = Box<dyn FnOnce(LookoutConfig) -> LookoutConfig>;

/// Builds the dashboard's access check for one user type — see
/// [`Rainier::with_lookout_viewers`](crate::Rainier::with_lookout_viewers).
pub(crate) type Viewers =
    Arc<dyn Fn(Weak<Application>, config::Dashboard) -> http::AccessCheck + Send + Sync>;

/// The configuration boot uses: the environment, then the adjustments.
pub(crate) fn configure(
    env: &Env,
    config: &Config,
    adjustments: Vec<Adjustment>,
) -> Result<LookoutConfig> {
    let app_env = config.get(keys::APP_ENV).unwrap_or_default().to_string();
    let mut lookout = LookoutConfig::from_lookup(|name| {
        if name == "APP_ENV" {
            Some(app_env.clone())
        } else {
            env.get(name)
        }
    })
    .map_err(|e| Error::internal(format!("lookout: {e}")))?;
    for adjust in adjustments {
        lookout = adjust(lookout);
    }
    lookout.validate().map_err(|e| Error::internal(format!("lookout: {e}")))?;
    Ok(lookout)
}

/// The environment name the dashboard's gate is given. [`LookoutConfig::local`]
/// is the authority — an application can turn it off in a local environment,
/// or on in a test — so the gate sees `local` exactly when it is set.
pub(crate) fn gate_environment(local: bool, environment: &str) -> String {
    match (local, http::is_local(environment)) {
        (true, _) => "local".to_string(),
        (false, true) => "local (not treated as local)".to_string(),
        (false, false) => environment.to_string(),
    }
}

/// How an authenticated viewer is authorised: the bound `Gate<U>`'s ability,
/// or the application's own predicate over the user and the request.
pub(crate) type ViewerPredicate<U> = Arc<ViewerFn<U>>;

/// The predicate itself, unshared.
pub(crate) type ViewerFn<U> = dyn Fn(&U, &Request) -> bool + Send + Sync;

/// The access check for viewers of type `U`: authenticate with the
/// dashboard's guard, then ask the bound `Gate<U>` for its ability — or,
/// given `predicate`, ask that instead.
pub(crate) fn viewers<U: Authenticatable>(predicate: Option<ViewerPredicate<U>>) -> Viewers {
    Arc::new(move |app: Weak<Application>, dashboard: config::Dashboard| -> http::AccessCheck {
        let predicate = predicate.clone();
        Arc::new(move |request: &Request| -> BoxFuture<'static, bool> {
            // A header-only copy: the check outlives the borrow, and a guard
            // reads nothing but the headers and the session.
            let copy = Request::new(
                request.method().clone(),
                request.uri().clone(),
                request.headers().clone(),
                Default::default(),
            );
            let app = app.clone();
            let guard = dashboard.guard.clone();
            let ability = dashboard.ability.clone();
            let predicate = predicate.clone();
            Box::pin(async move {
                let Some(app) = app.upgrade() else { return false };
                allows::<U>(&app, copy, guard.as_deref(), &ability, predicate.as_deref()).await
            })
        })
    })
}

async fn allows<U: Authenticatable>(
    app: &Application,
    mut request: Request,
    guard: Option<&str>,
    ability: &str,
    predicate: Option<&ViewerFn<U>>,
) -> bool {
    let decision = decide(app, &mut request, guard, ability, predicate).await;
    if let Err(reason) = decision {
        // Why a viewer was turned away, for the operator: the response is the
        // router's own 404 and says nothing. The reason only — never the
        // cookie, the session id or the user.
        tracing::info!(target: "rainier_lookout::gate", reason, "lookout: dashboard refused");
    }
    decision.is_ok()
}

async fn decide<U: Authenticatable>(
    app: &Application,
    request: &mut Request,
    guard: Option<&str>,
    ability: &str,
    predicate: Option<&ViewerFn<U>>,
) -> std::result::Result<(), &'static str> {
    let auth = app.resolve::<AuthManager<U>>().map_err(|_| "no AuthManager is bound")?;

    // The dashboard is mounted outside the `web` group, so nothing has loaded
    // the session. Load it here, read-only: running `StartSession` on these
    // routes would set a cookie on a refused request's 404, and the 404 must
    // be indistinguishable from a path that does not exist.
    //
    // No session is not fatal: a guard that reads a header may still
    // authenticate. Kept so a refusal can say what was missing.
    let no_session = match read_session(app, request).await {
        Ok(session) => {
            request.extensions_mut().insert(session);
            None
        }
        Err(reason) => Some(reason),
    };

    let user = match guard {
        Some(name) => auth.user_via(name, request).await,
        None => auth.user(request).await,
    };
    let user = match user {
        Ok(Some(user)) => user,
        Ok(None) => return Err(no_session.unwrap_or("the session has no authenticated user")),
        Err(_) => return Err("the guard failed"),
    };
    let allowed = match predicate {
        Some(predicate) => predicate(&user, request),
        None => match app.resolve::<Gate<U>>() {
            Ok(gate) => gate.allows::<()>(ability, &user, None),
            Err(_) => return Err("no Gate is bound"),
        },
    };
    if allowed {
        Ok(())
    } else {
        Err("the viewer is authenticated but not allowed")
    }
}

async fn read_session(
    app: &Application,
    request: &Request,
) -> std::result::Result<rainier_session::Session, &'static str> {
    let manager = app
        .resolve::<rainier_session::SessionManager>()
        .map_err(|_| "no SessionManager is bound")?;
    let value = request.cookie(&manager.config().cookie).ok_or("no session cookie")?;
    let (id, carried) =
        manager.store().decode(value).map_err(|_| "the session cookie did not decode")?;
    let data = match carried {
        Some(data) => data,
        None => manager
            .read(&id)
            .await
            .map_err(|_| "the session store failed")?
            .ok_or("the session cookie names no stored session")?,
    };
    Ok(rainier_session::Session::restore(id, data))
}

/// Build the store `kind` names, from what boot has already bound.
pub(crate) fn build_store(app: &Application, kind: &config::StoreKind) -> Result<Arc<dyn Store>> {
    Ok(match kind {
        config::StoreKind::Memory => Arc::new(MemoryStore::new(config::DEFAULT_CACHE_CAPACITY)),
        config::StoreKind::Database { connection } => {
            let databases = app.resolve::<rainier_database::DatabaseManager>().map_err(|_| {
                Error::internal(
                    "lookout: LOOKOUT_STORE is \"database\", but no database is configured",
                )
            })?;
            let db = databases
                .resolve(connection.as_deref())
                .map_err(|e| Error::internal(format!("lookout: {}", e.message())))?;
            Arc::new(DatabaseStore::new(db.clone()))
        }
        config::StoreKind::Cache { store, capacity } => {
            let caches = app.resolve::<rainier_cache::CacheManager>()?;
            let cache = match store {
                None => Arc::clone(caches.store()),
                Some(name) => Arc::clone(caches.store_named(name).ok_or_else(|| {
                    Error::internal(format!(
                        "lookout: LOOKOUT_CACHE_STORE names `{name}`, which is not a declared \
                         cache store"
                    ))
                })?),
            };
            Arc::new(CacheStore::new(cache, *capacity))
        }
    })
}

/// The store the recorder writes to, filled in once boot has built the real
/// one.
///
/// The recorder has to exist before the tracing subscriber — its layer is
/// part of it — and the subscriber has to exist before boot opens databases
/// and caches, so their failures are logged. The store needs those databases
/// and caches. So the recorder starts with this, and boot fills it in; the
/// writer is not started until it has been.
#[derive(Default)]
pub(crate) struct DeferredStore {
    inner: OnceLock<Arc<dyn Store>>,
}

impl DeferredStore {
    pub(crate) fn set(&self, store: Arc<dyn Store>) {
        let _ = self.inner.set(store);
    }

    fn get(&self) -> Result<&Arc<dyn Store>> {
        self.inner.get().ok_or_else(|| Error::internal("lookout: the store is not built yet"))
    }
}

impl Store for DeferredStore {
    fn name(&self) -> &'static str {
        self.inner.get().map_or("pending", |s| s.name())
    }

    fn store(&self, batches: Vec<Batch>) -> BoxFuture<'_, Result<()>> {
        match self.get() {
            Ok(store) => store.store(batches),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn batches(&self, query: BatchQuery) -> BoxFuture<'_, Result<Vec<BatchSummary>>> {
        match self.get() {
            Ok(store) => store.batches(query),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn batch(&self, id: &str) -> BoxFuture<'_, Result<Option<Batch>>> {
        match self.get() {
            Ok(store) => store.batch(id),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn entries(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>> {
        match self.get() {
            Ok(store) => store.entries(entry_type, before, limit),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn aggregates(&self, query: AggregateQuery) -> BoxFuture<'_, Result<Vec<Aggregate>>> {
        match self.get() {
            Ok(store) => store.aggregates(query),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn prune(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<u64>> {
        match self.get() {
            Ok(store) => store.prune(batches_before, aggregates_before),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        match self.get() {
            Ok(store) => store.clear(),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
}

/// The running writer, kept in the container for the life of the process.
pub(crate) struct Writer(Mutex<Option<WriterHandle>>);

impl Writer {
    pub(crate) fn new(handle: WriterHandle) -> Self {
        Self(Mutex::new(Some(handle)))
    }
}

/// Write everything Lookout has closed so far. Does nothing when Lookout is
/// not enabled. `serve`, `queue:work` and `schedule:run` call it as they
/// finish.
pub async fn flush(app: &Application) {
    if let Ok(lookout) = app.resolve::<Lookout>() {
        lookout.flush().await;
    }
}

/// Flush, then stop the background writer. Call once, as the process exits —
/// after the console has run, so the command's own batch is written too:
///
/// ```ignore
/// let code = rainier_framework::console("app").run_from_env(&app).await;
/// rainier_framework::lookout::shutdown(&app).await;
/// std::process::exit(code);
/// ```
///
/// Nothing is recorded after it — later batches wait for a writer that is
/// gone — so it is the last thing a process does.
pub async fn shutdown(app: &Application) {
    let Ok(lookout) = app.resolve::<Lookout>() else { return };
    lookout.flush().await;
    let handle = app
        .resolve::<Writer>()
        .ok()
        .and_then(|writer| writer.0.lock().unwrap_or_else(|e| e.into_inner()).take());
    if let Some(handle) = handle {
        handle.shutdown().await;
    }
}

/// `lookout:prune`, `lookout:clear` and `lookout:status`, for a console that
/// is built before the application — [`console`](crate::commands::console)
/// registers them.
///
/// Each resolves the booted [`Lookout`] when it runs and hands over to the
/// command of the same name in [`rainier_lookout::commands`]. With Lookout
/// not enabled, each says so and fails.
pub fn console_commands() -> Vec<Arc<dyn Command>> {
    let config = Arc::new(LookoutConfig::default());
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new(1));
    commands::commands(store, config)
        .into_iter()
        .map(|prototype| Arc::new(Delegate { prototype }) as Arc<dyn Command>)
        .collect()
}

/// One of Lookout's commands, built over the booted Lookout when it runs.
struct Delegate {
    /// For its name, description and help.
    prototype: Arc<dyn Command>,
}

#[async_trait::async_trait]
impl Command for Delegate {
    fn name(&self) -> &str {
        self.prototype.name()
    }

    fn description(&self) -> &str {
        self.prototype.description()
    }

    fn help(&self) -> Option<&str> {
        self.prototype.help()
    }

    async fn handle(&self, args: &Arguments, app: &Application) -> Result<i32> {
        let Ok(lookout) = app.resolve::<Lookout>() else {
            eprintln!("Lookout is not enabled. Set LOOKOUT_ENABLED=true to turn it on.");
            return Ok(rainier_console::exit::FAILURE);
        };
        let store: Arc<dyn Store> = match app.resolve::<Arc<dyn Store>>() {
            Ok(store) => Arc::clone(&*store),
            Err(_) => Arc::clone(lookout.store()),
        };
        let real = commands::commands(store, Arc::new(lookout.config().clone()))
            .into_iter()
            .find(|command| command.name() == self.prototype.name())
            .ok_or_else(|| Error::internal(format!("no Lookout command `{}`", self.name())))?;
        real.handle(args, app).await
    }
}

#[cfg(test)]
mod tests;
