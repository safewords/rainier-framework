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
//! | `GET` | `/api/entries/{type}` | `lookout.api.entries` | [`Entry`](crate::entry::Entry) list |
//! | `GET` | `/api/aggregates/{type}` | `lookout.api.aggregates` | [`Aggregate`](crate::entry::Aggregate) list |
//! | `GET` | `/api/status` | `lookout.api.status` | Store, watchers, sample rates, retention |
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

mod api;
mod params;
mod shell;

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
        Self { check: Arc::new(check) }
    }

    /// A gate from an already-shared check.
    pub fn from_check(check: AccessCheck) -> Self {
        Self { check }
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
    });

    let group = GroupAttributes::new().prefix(base).name("lookout.");
    router.group(group, |router| {
        router.get("/", guarded(&state, shell::page)).name("dashboard");
        router.get("/assets/lookout.js", guarded(&state, shell::script)).name("assets.js");
        router.get("/assets/lookout.css", guarded(&state, shell::stylesheet)).name("assets.css");

        router.get("/api/batches", guarded(&state, api::batches)).name("api.batches");
        router.delete("/api/batches", guarded(&state, api::clear)).name("api.clear");
        router.get("/api/batches/{id}", guarded(&state, api::batch)).name("api.batch");
        router.get("/api/entries/{type}", guarded(&state, api::entries)).name("api.entries");
        router
            .get("/api/aggregates/{type}", guarded(&state, api::aggregates))
            .name("api.aggregates");
        router.get("/api/status", guarded(&state, api::status)).name("api.status");

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
                let allow = if rest.trim_end_matches('/') == "api/batches" {
                    "GET, HEAD, DELETE"
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
