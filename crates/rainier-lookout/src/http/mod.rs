//! The dashboard and its JSON API.
//!
//! [`routes`] declares everything under the configured path (`/_lookout` by
//! default) on a [`Router`] the application merges into its own:
//!
//! | Method | URI (under the path) | Name | What |
//! |---|---|---|---|
//! | `GET` | `/` | `lookout.dashboard` | The dashboard: an HTML shell that mounts the Vue app |
//! | `GET` | `/assets/lookout.js` | `lookout.assets.js` | The app, built from `frontend/` |
//! | `GET` | `/assets/lookout.css` | `lookout.assets.css` | Its stylesheet |
//! | `GET` | `/api/batches` | `lookout.api.batches` | [`BatchSummary`](crate::entry::BatchSummary) list |
//! | `DELETE` | `/api/batches` | `lookout.api.clear` | Delete everything — needs `X-Lookout: 1` |
//! | `GET` | `/api/batches/{id}` | `lookout.api.batch` | One [`Batch`](crate::entry::Batch), with entries |
//! | `GET` | `/api/entries/{type}` | `lookout.api.entries` | [`Entry`](crate::entry::Entry) list; `family` is a 16-hex id |
//! | `GET` | `/api/aggregates/{type}` | `lookout.api.aggregates` | [`Aggregate`](crate::entry::Aggregate) list; `until` ends the window |
//! | `GET` | `/api/series/{type}` | `lookout.api.series` | Zero-filled points per bucket from `since` to `until` (or now), a forecast, and per-key series with `keys=N`; `family` is a 16-hex id; `histograms=1` adds each bucket's histogram |
//! | `GET` | `/api/models` | `lookout.api.models` | Every model written since `since`, with its writes per action ([`ModelSummary`](crate::watchers::models::ModelSummary)) |
//! | `GET` | `/api/models/{model}` | `lookout.api.model` | One model's recorded changes, newest first; `before` pages |
//! | `GET` | `/api/insights` | `lookout.api.insights` | What changed since the previous window ([`trends`](crate::trends)) |
//! | `GET` | `/api/cases` | `lookout.api.cases` | [`Case`](crate::cases::Case) list: `status`, `severity` (minimum), `q`, `limit` |
//! | `GET` | `/api/cases/{id}` | `lookout.api.case` | One case |
//! | `POST` | `/api/cases/{id}` | `lookout.api.case.act` | `{"action": "resolve"\|"reopen"\|"ignore"\|"severity", "severity"?, "note"?}` — needs `X-Lookout: 1` |
//! | `GET` | `/api/status` | `lookout.api.status` | Store, watchers, sample rates, retention |
//! | `GET` | `/api/health` | `lookout.api.health` | One service's health over a window: requests, error rate, percentiles, Apdex, jobs, exceptions, cases, a status and the routes behind it |
//! | `GET` | `/api/requests` | `lookout.api.requests` | Every request, grouped by any of route, method and status class, and filtered |
//! | `GET` | `/api/queues` | `lookout.api.queues` | Every queue: depth, concurrency against slots, throughput by outcome, waits, run times and a status |
//! | `GET` | `/api/rum/vitals` | `lookout.api.rum.vitals` | Real users' page views and their p75 vitals, grouped by one dimension |
//! | `GET` | `/api/rum/errors` | `lookout.api.rum.errors` | Real users' script errors, by message, file and line |
//! | `GET` | `/{rest*}` | `lookout.app` | The same shell, for the app's own client-side routes |
//!
//! The fallback never answers under `/api/` or `/assets/`: an unknown API
//! path is a JSON 404, not a page.
//!
//! The app is a Vue 3 + Tailwind CSS build in `crates/rainier-lookout/frontend`,
//! committed to `assets/dist/` with stable file names and compiled into the
//! binary with `include_bytes!` — building the crate never needs Node. See
//! `frontend/README.md` to rebuild it.
//!
//! # Who may look
//!
//! Every route runs the [`LookoutGate`] first. In the local environment, with
//! [`Dashboard::open_locally`] set, everyone may; anywhere else the gate's
//! check must pass. A request it refuses gets **the router's own 404**, byte
//! for byte — and so does any other method on a Lookout path — so a
//! production probe cannot tell the dashboard is there.
//!
//! The check is the application's: typically "authenticate with the
//! dashboard's guard, then ask the `Gate` for [`Dashboard::ability`]". This
//! module takes it as a closure so it depends on neither.
//!
//! # Readers
//!
//! Another service may be let in as a **reader**
//! ([`LookoutGate::with_readers`]): a check over the request alone — a
//! service token, say — that admits it to the read-only API another service
//! needs and nothing else: `GET` on [`READER_ROUTES`]. Everything else, the
//! dashboard's shell included, stays the viewers' alone, and a refused
//! reader gets the same 404 as anyone refused.
//!
//! # Longmire's link
//!
//! A request signed with one of [`LookoutConfig::longmire_keys`] is
//! Longmire's, and its answer is sealed for it (`rainier-lookout-link`). It
//! reaches the readers' routes; with
//! [`LongmireReach::Viewer`](crate::config::LongmireReach)
//! (`LOOKOUT_LONGMIRE_REACH=viewer`) also [`LINK_VIEWER_ROUTES`] — what the
//! dashboard's viewers read, and the case actions, whose body is signed too.
//! Never clearing, the live stream or the shell.
//!
//! # What every response carries
//!
//! `Cache-Control: no-store` (the two assets instead: `private`, a year,
//! `immutable` — the shell links them with a content-hash query string),
//! `X-Robots-Tag: noindex, nofollow`, `X-Content-Type-Options: nosniff` and
//! `Referrer-Policy: no-referrer`. The shell adds the
//! [`content_security_policy`] — scripts, styles and `fetch` from the same
//! origin only, nothing inline — and `X-Frame-Options: DENY`. Entries hold
//! user-controlled data — a URL, a query's text, an exception message — and
//! the app only ever renders them as text; the one value the server writes
//! into the shell, its config, is JSON escaped for a `<script>` element.
//!
//! # Clearing
//!
//! `DELETE /api/batches` is the one route that changes anything. Besides the
//! gate it needs an `X-Lookout: 1` header: a cross-site form cannot set one,
//! and a cross-site `fetch` that does is preflighted, so the header is the
//! CSRF defence for a JSON API that has no session token of its own.

use std::future::Future;
use std::sync::Arc;

use rainier_http::{Method, Request, Response, StatusCode};
use rainier_routing::{GroupAttributes, Req, Router};
use rainier_support::{BoxFuture, Error};

use crate::config::{Dashboard, LookoutConfig};
use crate::store::Store;

pub(crate) mod api;
pub(crate) mod health;
mod params;
mod rum;
mod shell;
pub(crate) mod stream;
pub(crate) mod traces;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

pub(crate) use api::status_value;
pub use shell::content_security_policy;

/// The header [`routes`]' clearing endpoint requires, and the value it must
/// have.
pub const CLEAR_HEADER: (&str, &str) = ("x-lookout", "1");

/// The application's answer to "may this request see Lookout?".
///
/// `'static` so it can outlive the borrow of the request: a check that needs
/// something from the request — a cookie, a bearer token — clones it before
/// the `async` block.
pub type AccessCheck = Arc<dyn Fn(&Request) -> BoxFuture<'static, bool> + Send + Sync>;

/// Who may see the dashboard and call its API.
///
/// The policy is [`Dashboard`]'s: in the local environment with
/// [`open_locally`](Dashboard::open_locally) set, everyone; otherwise whoever
/// the check lets through. See [`LookoutGate::allows`].
#[derive(Clone)]
pub struct LookoutGate {
    check: AccessCheck,
    readers: Option<AccessCheck>,
}

impl std::fmt::Debug for LookoutGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookoutGate").finish_non_exhaustive()
    }
}

impl LookoutGate {
    /// A gate that runs `check` outside the local environment.
    ///
    /// ```
    /// use rainier_lookout::LookoutGate;
    ///
    /// // Let in whoever presents the operator token. A real application asks
    /// // its guard and its `Gate` for the dashboard's ability instead.
    /// let gate = LookoutGate::new(|request| {
    ///     let ok = request.header("x-operator") == Some("let-me-in");
    ///     Box::pin(async move { ok })
    /// });
    /// # let _ = gate;
    /// ```
    pub fn new(
        check: impl Fn(&Request) -> BoxFuture<'static, bool> + Send + Sync + 'static,
    ) -> Self {
        Self { check: Arc::new(check), readers: None }
    }

    /// A gate from an already-shared check.
    pub fn from_check(check: AccessCheck) -> Self {
        Self { check, readers: None }
    }

    /// Also let in, to the read-only API another service needs — `GET` on
    /// [`READER_ROUTES`] and nothing else — whoever `check` admits: a check
    /// over the request alone, such as a service's bearer token. Off unless
    /// given. A reader `check` refuses still has the viewer's check to pass.
    ///
    /// ```
    /// use rainier_lookout::LookoutGate;
    ///
    /// let gate = LookoutGate::deny_all().with_readers(|request| {
    ///     let ok = request.header("authorization") == Some("Bearer service-token");
    ///     Box::pin(async move { ok })
    /// });
    /// # let _ = gate;
    /// ```
    pub fn with_readers(
        self,
        check: impl Fn(&Request) -> BoxFuture<'static, bool> + Send + Sync + 'static,
    ) -> Self {
        self.with_reader_check(Arc::new(check))
    }

    /// [`with_readers`](Self::with_readers), from an already-shared check.
    pub fn with_reader_check(mut self, check: AccessCheck) -> Self {
        self.readers = Some(check);
        self
    }

    /// Whether `request` may call a reader's route: as a reader, or else as a
    /// viewer ([`allows`](Self::allows)).
    pub async fn allows_reader(
        &self,
        request: &Request,
        dashboard: &Dashboard,
        environment: &str,
    ) -> bool {
        if let Some(readers) = &self.readers {
            if (readers)(request).await {
                return true;
            }
        }
        self.allows(request, dashboard, environment).await
    }

    /// A gate that lets nobody in — except, through
    /// [`open_locally`](Dashboard::open_locally), in the local environment.
    /// The safe default when the application has said nothing.
    pub fn deny_all() -> Self {
        Self::new(|_| Box::pin(async { false }))
    }

    /// A gate that lets everybody in, everywhere. For tests, and for a
    /// dashboard served only on an internal port.
    pub fn allow_all() -> Self {
        Self::new(|_| Box::pin(async { true }))
    }

    /// Whether `request` may see Lookout, under `dashboard`'s policy, in
    /// `environment`.
    ///
    /// The environment is passed in rather than read here, so the decision is
    /// the application's own idea of where it is running.
    pub async fn allows(
        &self,
        request: &Request,
        dashboard: &Dashboard,
        environment: &str,
    ) -> bool {
        if dashboard.open_locally && is_local(environment) {
            return true;
        }
        (self.check)(request).await
    }
}

/// The routes a reader ([`LookoutGate::with_readers`]) may call, with `GET`
/// (and `HEAD`) only, as patterns under the dashboard's path.
pub const READER_ROUTES: &[&str] = &[
    "/api/health",
    "/api/requests",
    "/api/queues",
    "/api/rum/vitals",
    "/api/rum/errors",
    "/api/aggregates/{type}",
    "/api/series/{type}",
    "/api/cases",
    "/api/status",
];

/// The routes a reader may also call in request tracing
/// ([`RequestTracing`](crate::config::RequestTracing)): a trace's shape — its
/// spans and timings, never their content. Outside request tracing they are
/// the viewers' alone.
pub const TRACING_READER_ROUTES: &[&str] = &["/api/traces", "/api/traces/{trace}"];

/// The routes Longmire's link may also call where this Lookout lets it reach
/// as far as a viewer ([`LongmireReach::Viewer`](crate::config::LongmireReach)),
/// with the method each is called with. Everything a viewer reads, and the
/// case actions; never `DELETE /api/batches`, the live stream or the shell.
pub const LINK_VIEWER_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/batches"),
    ("GET", "/api/batches/{id}"),
    ("GET", "/api/entries/{type}"),
    ("GET", "/api/families/{type}/{id}"),
    ("GET", "/api/models"),
    ("GET", "/api/models/{model}"),
    ("GET", "/api/insights"),
    ("GET", "/api/cases/{id}"),
    ("POST", "/api/cases/{id}"),
    ("GET", "/api/traces"),
    ("GET", "/api/traces/{trace}"),
];

/// Whether `environment` is the local one — the only place
/// [`Dashboard::open_locally`] applies. `testing` is deliberately not: a test
/// asserting on the gate should see the production behaviour.
pub fn is_local(environment: &str) -> bool {
    environment.eq_ignore_ascii_case("local")
}

/// What every handler shares.
pub(crate) struct Shared {
    pub(crate) store: Arc<dyn Store>,
    pub(crate) config: Arc<LookoutConfig>,
    pub(crate) gate: LookoutGate,
    pub(crate) environment: String,
    /// The dashboard path, normalised: `/_lookout`, or `""` when mounted at
    /// the root.
    pub(crate) base: String,
    /// The one tail of the store "Live" viewers share.
    pub(crate) tail: Arc<stream::Tail>,
    /// How much each address has reported this minute.
    pub(crate) rum_limiter: crate::rum::Limiter,
    /// The distinct pages, view keys and errors the RUM rollups have counted
    /// this hour, for their caps.
    pub(crate) rum_caps: crate::rum_rollup::Caps,
    /// The nonces of Longmire requests already answered.
    pub(crate) replay: rainier_lookout_link::ReplayGuard,
}

/// Declare the dashboard and its API on a new [`Router`], under
/// [`Dashboard::path`], with route names prefixed `lookout.`.
///
/// Merge it into the application's router with
/// [`Router::merge`]. When [`Dashboard::enabled`] is off the router is empty,
/// so a disabled dashboard is the same 404 as a path that never existed.
///
/// `environment` is the application's (`local`, `production`, …), for
/// [`Dashboard::open_locally`].
///
/// ```
/// use std::sync::Arc;
/// use rainier_lookout::{routes, LookoutConfig, LookoutGate, Store};
///
/// # fn mount(store: Arc<dyn Store>, app_router: &mut rainier_routing::Router) {
/// let router = routes(store, Arc::new(LookoutConfig::default()), LookoutGate::deny_all(), "production");
/// app_router.merge(router);
/// # }
/// ```
pub fn routes(
    store: Arc<dyn Store>,
    config: Arc<LookoutConfig>,
    gate: LookoutGate,
    environment: &str,
) -> Router {
    let mut router = Router::new();
    if !config.dashboard.enabled {
        return router;
    }

    let base = rainier_routing::route::normalise_uri(&config.dashboard.path);
    let base = if base == "/" { String::new() } else { base };
    let state = Arc::new(Shared {
        store,
        config,
        gate,
        environment: environment.to_string(),
        base: base.clone(),
        tail: stream::Tail::new(),
        rum_limiter: crate::rum::Limiter::default(),
        rum_caps: crate::rum_rollup::Caps::default(),
        replay: rainier_lookout_link::ReplayGuard::default(),
    });

    let group = GroupAttributes::new().prefix(base).name("lookout.");
    router.group(group, |router| {
        router.get("/", guarded(&state, shell::page)).name("dashboard");
        router.get("/assets/lookout.js", guarded(&state, shell::script)).name("assets.js");
        router.get("/assets/lookout.css", guarded(&state, shell::stylesheet)).name("assets.css");

        router.get("/api/batches", viewable(&state, api::batches)).name("api.batches");
        router.delete("/api/batches", guarded(&state, api::clear)).name("api.clear");
        router.get("/api/batches/{id}", viewable(&state, api::batch)).name("api.batch");
        router.get("/api/entries/{type}", viewable(&state, api::entries)).name("api.entries");
        router
            .get("/api/aggregates/{type}", readable(&state, api::aggregates))
            .name("api.aggregates");
        router.get("/api/series/{type}", readable(&state, api::series)).name("api.series");
        router.get("/api/families/{type}/{id}", viewable(&state, api::family)).name("api.family");
        router.get("/api/models", viewable(&state, api::models)).name("api.models");
        router.get("/api/models/{model}", viewable(&state, api::model)).name("api.model");
        router.get("/api/insights", viewable(&state, api::insights)).name("api.insights");
        router.get("/api/cases", readable(&state, api::cases)).name("api.cases");
        router.get("/api/cases/{id}", viewable(&state, api::case)).name("api.case");
        router.post("/api/cases/{id}", viewable(&state, api::act_on_case)).name("api.case.act");
        router.get("/api/status", readable(&state, api::status)).name("api.status");
        router.get("/api/health", readable(&state, health::health)).name("api.health");
        router.get("/api/requests", readable(&state, health::requests)).name("api.requests");
        router.get("/api/queues", readable(&state, health::queues)).name("api.queues");
        router.get("/api/rum/vitals", readable(&state, health::rum_vitals)).name("api.rum.vitals");
        router.get("/api/rum/errors", readable(&state, health::rum_errors)).name("api.rum.errors");
        router.get("/api/stream", guarded(&state, stream::stream)).name("api.stream");
        // A trace's shape is a reader's to read only in request tracing,
        // which is what it is for; otherwise the viewers' like a batch.
        if state.config.request_tracing.enabled {
            router.get("/api/traces", readable(&state, traces::list)).name("api.traces");
            router.get("/api/traces/{trace}", readable(&state, traces::show)).name("api.trace");
        } else {
            router.get("/api/traces", viewable(&state, traces::list)).name("api.traces");
            router.get("/api/traces/{trace}", viewable(&state, traces::show)).name("api.trace");
        }

        // Real user monitoring: public, because every visitor's browser
        // loads the script and reports. Only while it is on.
        if state.config.rum.enabled {
            router.get("/rum.js", open(&state, rum::script)).name("rum.script");
            router.post("/rum", open(&state, rum::ingest)).name("rum.ingest");
            if state.config.rum.replay.enabled {
                router.get("/rum-replay.js", open(&state, rum::recorder)).name("rum.recorder");
                router.post("/rum/replay", open(&state, rum::ingest_replay)).name("rum.replay");
            }
        }

        // Declared last, so every route above wins: the app's own client
        // routes (`/batches/abc`) get the shell on a reload or a shared link.
        router.get("/{rest*}", guarded(&state, shell::fallback)).name("app");

        // Every other method, anywhere under the path. Without this the
        // router would answer `405 Allow: GET` to anyone — which says
        // "something is here". Behind the gate, a refused request gets the
        // plain 404 and an allowed one the honest 405.
        router.add(
            vec![Method::POST, Method::PUT, Method::PATCH, Method::DELETE, Method::OPTIONS],
            "/{rest*}",
            guarded(&state, |_: Arc<Shared>, request: Req| async move {
                let rest = request.route_param("rest").unwrap_or_default();
                let rest = rest.trim_end_matches('/');
                let allow = if rest == "api/batches" {
                    "GET, HEAD, DELETE"
                } else if rest.starts_with("api/cases/") {
                    "GET, HEAD, POST"
                } else {
                    "GET, HEAD"
                };
                Response::new(StatusCode::METHOD_NOT_ALLOWED).with_header("allow", allow)
            }),
        );
    });

    router
}

/// Wrap a handler in the gate and the response headers.
fn guarded<F, Fut, R>(
    state: &Arc<Shared>,
    handler: F,
) -> impl Fn(Req) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static
where
    F: Fn(Arc<Shared>, Req) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: rainier_http::IntoResponse,
{
    let state = Arc::clone(state);
    move |request: Req| {
        let state = Arc::clone(&state);
        let handler = handler.clone();
        Box::pin(async move {
            if !state.gate.allows(&request, &state.config.dashboard, &state.environment).await {
                return denied(&request);
            }
            secure(handler(state, request).await.into_response())
        })
    }
}

/// Wrap a handler a reader may call: the reader's check, else the viewer's,
/// and the response headers. Registered for `GET` only, so every other
/// method on its path falls to the viewers' catch-all.
fn readable<F, Fut, R>(
    state: &Arc<Shared>,
    handler: F,
) -> impl Fn(Req) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static
where
    F: Fn(Arc<Shared>, Req) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: rainier_http::IntoResponse,
{
    let state = Arc::clone(state);
    move |request: Req| {
        let state = Arc::clone(&state);
        let handler = handler.clone();
        Box::pin(async move {
            // Longmire's link: a request signed with one of the link keys,
            // answered sealed. A request that says it is one and is not gets
            // the same 404 as anything else refused.
            if let Some(header) = request.header(rainier_lookout_link::HEADER) {
                let Some(seen) = recognise_link(&state, &request, header) else {
                    return denied(&request);
                };
                let response = handler(Arc::clone(&state), request).await.into_response();
                return secure(seal_for_link(&state, &seen, response).await);
            }
            let allowed = state
                .gate
                .allows_reader(&request, &state.config.dashboard, &state.environment)
                .await;
            if !allowed {
                return denied(&request);
            }
            secure(handler(state, request).await.into_response())
        })
    }
}

/// Wrap a handler the dashboard's viewers call, which Longmire's link may
/// also call when this Lookout lets it reach that far
/// ([`LongmireReach::Viewer`](crate::config::LongmireReach)): the link's
/// request answered sealed, anyone else through the viewers' gate. A link
/// request where the reach is the readers' gets the same 404 as anything
/// else refused.
fn viewable<F, Fut, R>(
    state: &Arc<Shared>,
    handler: F,
) -> impl Fn(Req) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static
where
    F: Fn(Arc<Shared>, Req) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: rainier_http::IntoResponse,
{
    let state = Arc::clone(state);
    move |request: Req| {
        let state = Arc::clone(&state);
        let handler = handler.clone();
        Box::pin(async move {
            if let Some(header) = request.header(rainier_lookout_link::HEADER) {
                if state.config.longmire_reach != crate::config::LongmireReach::Viewer {
                    return denied(&request);
                }
                let Some(seen) = recognise_link(&state, &request, header) else {
                    return denied(&request);
                };
                let response = handler(Arc::clone(&state), request).await.into_response();
                return secure(seal_for_link(&state, &seen, response).await);
            }
            if !state.gate.allows(&request, &state.config.dashboard, &state.environment).await {
                return denied(&request);
            }
            secure(handler(state, request).await.into_response())
        })
    }
}

/// A Longmire request, recognised by one of the configured link keys —
/// signature (its body's too), freshness, first use. `None` for anything
/// else.
fn recognise_link(
    state: &Shared,
    request: &Request,
    header: &str,
) -> Option<rainier_lookout_link::Verified> {
    let keys = &state.config.longmire_keys;
    if keys.is_empty() {
        return None;
    }
    let path_and_query = request.uri().path_and_query().map_or(request.path(), |p| p.as_str());
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    rainier_lookout_link::verify_request(
        header,
        keys,
        request.method().as_str(),
        path_and_query,
        request.body(),
        now,
        &state.replay,
    )
}

/// The answer to a Longmire request, sealed for it: its status kept, its body
/// readable only by the holder of the key, and only as the answer to that
/// request.
async fn seal_for_link(
    state: &Shared,
    seen: &rainier_lookout_link::Verified,
    mut response: Response,
) -> Response {
    use rainier_http::IntoResponse as _;
    let Some(key) = state.config.longmire_keys.iter().find(|k| k.id() == seen.key_id) else {
        return Error::internal("link key vanished").into_response();
    };
    let status = response.status();
    let body = match response.take_body().collect().await {
        Ok(body) => body,
        Err(_) => return Error::internal("could not read the answer").into_response(),
    };
    match rainier_lookout_link::seal(key, &seen.nonce, &body) {
        Ok(sealed) => {
            Response::ok(sealed).with_status(status).with_content_type(rainier_lookout_link::SEALED)
        }
        Err(error) => error.into_response(),
    }
}

/// A handler every visitor may reach: no gate, and its own headers.
fn open<F, Fut>(
    state: &Arc<Shared>,
    handler: F,
) -> impl Fn(Req) -> BoxFuture<'static, Response> + Clone + Send + Sync + 'static
where
    F: Fn(Arc<Shared>, Req) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    let state = Arc::clone(state);
    move |request: Req| {
        let state = Arc::clone(&state);
        let handler = handler.clone();
        Box::pin(async move { handler(state, request).await })
    }
}

/// Exactly what the router answers for a path it has no route for.
fn denied(request: &Request) -> Response {
    use rainier_http::IntoResponse as _;
    Error::not_found(format!("No route matches {}", request.path())).into_response()
}

/// The headers every Lookout response carries.
///
/// A response that already chose its caching (the assets) keeps it.
fn secure(response: Response) -> Response {
    let html = response.header("content-type").is_some_and(|value| value.starts_with("text/html"));
    let response = if response.header("cache-control").is_some() {
        response
    } else {
        response.with_header("cache-control", "no-store")
    };
    let response = response
        .with_header("x-robots-tag", "noindex, nofollow")
        .with_header("x-content-type-options", "nosniff")
        .with_header("referrer-policy", "no-referrer");
    if html {
        response
            .with_header("content-security-policy", content_security_policy())
            .with_header("x-frame-options", "DENY")
    } else {
        response
            .with_header("content-security-policy", "default-src 'none'; frame-ancestors 'none'")
    }
}
