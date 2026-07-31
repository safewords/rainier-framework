//! Health checks — [`Health`] and the endpoint that renders them.
//!
//! ```ignore
//! // In a provider.
//! let health = Health::new()
//!     .register("database", |app| async move {
//!         app.resolve::<Database>()?.query("SELECT 1").execute().await?;
//!         Ok(())
//!     })
//!     .register("cache", |app| async move {
//!         app.resolve::<CacheManager>()?.store().has("health").await?;
//!         Ok(())
//!     });
//!
//! // And a route.
//! router.get("/health/ready", health::endpoint).name("health.ready");
//! ```
//!
//! ```json
//! {
//!   "status": "ok",
//!   "checks": { "database": { "status": "ok", "duration_ms": 3 } },
//!   "build": { "name": "identity", "version": "2.4.1", "profile": "release" }
//! }
//! ```
//!
//! Roughly the same three hundred lines in every service anybody deploys, and
//! they diverge in the ways that cost an outage: one returns `200` while its
//! database is unreachable, another has no timeout and hangs the probe, a
//! third reports a version that is a hardcoded string somebody forgot.
//!
//! # Liveness and readiness are different questions
//!
//! | | Asks | Answering wrong |
//! |---|---|---|
//! | **liveness** | is this process running? | a restart loop |
//! | **readiness** | can it serve a request? | traffic to a replica that cannot |
//!
//! A liveness probe that checks the database is a liveness probe that restarts
//! every replica when the database blips — turning a degradation into an
//! outage, and taking away the processes that would have recovered. That is
//! why [`endpoint`] is readiness and why a liveness route should be two lines
//! that do no I/O.
//!
//! # Every check has a deadline
//!
//! A probe that hangs is worse than one that fails: the orchestrator waits,
//! decides nothing, and the replica sits in an unknown state. So each check
//! runs under [`timeout`](Health::timeout) and a check that overruns is a
//! failed check with a reason.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rainier_container::{facade_application, Application};
use rainier_http::{Response, StatusCode};
use rainier_support::{BoxedFuture, Error, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// One check: a name and something that either works or does not.
type Check = Arc<dyn Fn(Arc<Application>) -> BoxedFuture<Result<()>> + Send + Sync>;

/// What one check reported.
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// `"ok"` or `"failing"`.
    pub status: &'static str,
    /// How long it took.
    pub duration_ms: u64,
    /// Why it failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Outcome {
    /// Whether this check passed.
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

/// The whole report.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// `"ok"` when every check passed.
    pub status: &'static str,
    /// Each check, by name.
    pub checks: BTreeMap<String, Outcome>,
}

impl Report {
    /// Whether every check passed.
    pub fn is_healthy(&self) -> bool {
        self.status == "ok"
    }

    /// The status a probe should read.
    ///
    /// `503`, not `500`: a dependency being down means "try another replica,
    /// try again shortly", and that is what a load balancer and a retrying
    /// client both need to hear. A `500` says this service is broken, which
    /// invites a different and slower response from whoever is paged.
    pub fn status_code(&self) -> StatusCode {
        if self.is_healthy() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }

    /// The names that failed.
    pub fn failing(&self) -> Vec<&str> {
        self.checks
            .iter()
            .filter(|(_, outcome)| !outcome.is_ok())
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// A registry of health checks.
#[derive(Clone, Default)]
pub struct Health {
    checks: Vec<(String, Check)>,
    timeout: Option<Duration>,
    build: Option<Value>,
}

impl Health {
    /// An empty registry, with a five-second deadline per check.
    pub fn new() -> Self {
        Self { checks: Vec::new(), timeout: Some(Duration::from_secs(5)), build: None }
    }

    /// Add a check.
    ///
    /// It receives the application, so it can resolve whatever it needs to
    /// prove — which is the point: a check that does not touch the dependency
    /// is a check that reports the dependency is fine when it is not.
    #[must_use = "this returns a registry with the check added"]
    pub fn register<F, Fut>(mut self, name: impl Into<String>, check: F) -> Self
    where
        F: Fn(Arc<Application>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        self.checks.push((name.into(), Arc::new(move |app| Box::pin(check(app)))));
        self
    }

    /// How long a single check may take before it counts as failed.
    ///
    /// `None` removes the deadline, which is almost always wrong: a probe that
    /// hangs leaves the replica in an unknown state, and an orchestrator that
    /// cannot decide does nothing at all.
    #[must_use = "this returns a configured registry rather than configuring in place"]
    pub fn timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Include this build description in the report.
    ///
    /// Pass [`build_info!()`](macro@rainier_support::build_info) — the version and
    /// commit belong in the same document as the health, because the first two
    /// questions of an incident are "is it up" and "which one is it".
    #[must_use = "this returns a configured registry rather than configuring in place"]
    pub fn describing_build(mut self, build: impl Serialize) -> Self {
        self.build = serde_json::to_value(build).ok();
        self
    }

    /// The names registered, in order.
    pub fn names(&self) -> Vec<&str> {
        self.checks.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Run every check.
    ///
    /// Concurrently: a report of six checks that take a second each should
    /// take a second, not six, because a probe has a deadline of its own and
    /// a slow report is a failed one.
    pub async fn run(&self, app: Arc<Application>) -> Report {
        let running = self.checks.iter().map(|(name, check)| {
            let app = Arc::clone(&app);
            let name = name.clone();
            let check = Arc::clone(check);
            let timeout = self.timeout;

            async move { (name, run_one(check, app, timeout).await) }
        });

        let checks: BTreeMap<String, Outcome> =
            futures_util::future::join_all(running).await.into_iter().collect();

        let status = if checks.values().all(Outcome::is_ok) { "ok" } else { "failing" };

        Report { status, checks }
    }

    /// Run every check and render the document a probe reads.
    pub async fn render(&self, app: Arc<Application>) -> Response {
        let report = self.run(app).await;
        let status = report.status_code();

        let mut document = json!({
            "status": report.status,
            "checks": report.checks,
        });

        if let Some(build) = &self.build {
            document["build"] = build.clone();
        }

        Response::json(&document).with_status(status)
    }
}

/// Run one check, under its deadline and in its own task.
///
/// Spawned for two reasons. A panicking check is contained rather than taking
/// the whole probe with it — a check is somebody else's closure and one of
/// them will panic eventually, and a probe that returns nothing leaves the
/// replica in exactly the unknown state the deadline exists to avoid.
///
/// And [`spawn_with_facades`] carries the application scope in, so a check
/// that reaches for a facade finds the same application the report is about.
async fn run_one(check: Check, app: Arc<Application>, timeout: Option<Duration>) -> Outcome {
    let started = Instant::now();

    let running = rainier_container::spawn_with_facades(async move {
        match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, check(app)).await {
                Ok(result) => result,
                Err(_) => Err(Error::service_unavailable(format!(
                    "did not answer within {}ms",
                    timeout.as_millis()
                ))),
            },
            None => check(app).await,
        }
    });

    let result = match running.await {
        Ok(result) => result,
        Err(e) if e.is_panic() => Err(Error::internal("the check panicked".to_string())),
        Err(_) => Err(Error::internal("the check was cancelled")),
    };

    let duration_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(()) => Outcome { status: "ok", duration_ms, error: None },
        // The message, not the whole error chain: this document is served to
        // whoever can reach the endpoint, and a connection string in a probe
        // response is a connection string in somebody's monitoring system.
        Err(e) => Outcome { status: "failing", duration_ms, error: Some(e.message().to_string()) },
    }
}

impl std::fmt::Debug for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Health")
            .field("checks", &self.names())
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// The readiness endpoint, resolving [`Health`] from the container.
///
/// ```ignore
/// router.get("/health/ready", health::endpoint).name("health.ready");
/// ```
///
/// Answers `503` when anything is failing — see
/// [`Report::status_code`].
pub async fn endpoint() -> Response {
    let app = facade_application();

    match app.resolve::<Health>() {
        Ok(health) => health.render(app).await,
        // Nothing registered. Saying so beats reporting healthy, which is what
        // an empty registry would otherwise mean — a probe that passes because
        // it checks nothing.
        Err(_) => Response::json(&json!({
            "status": "unknown",
            "checks": {},
            "error": "no health checks are registered — bind a `Health` in a provider",
        }))
        .with_status(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> Arc<Application> {
        Arc::new(Application::new("."))
    }

    fn healthy() -> Health {
        Health::new()
            .register("database", |_| async { Ok(()) })
            .register("cache", |_| async { Ok(()) })
    }

    #[tokio::test]
    async fn every_check_passing_is_ok() {
        let report = healthy().run(app()).await;

        assert!(report.is_healthy());
        assert_eq!(report.status_code(), StatusCode::OK);
        assert_eq!(report.checks.len(), 2);
        assert!(report.failing().is_empty());
    }

    #[tokio::test]
    async fn one_failing_check_fails_the_report() {
        let health =
            healthy().register("search", |_| async { Err(Error::internal("connection refused")) });

        let report = health.run(app()).await;

        assert!(!report.is_healthy());
        assert_eq!(report.failing(), vec!["search"]);
        // The others still report, so a reader can see what is and is not
        // working rather than only that something is wrong.
        assert!(report.checks["database"].is_ok());
    }

    #[tokio::test]
    async fn a_failing_check_says_why() {
        let health = Health::new()
            .register("search", |_| async { Err(Error::internal("connection refused")) });

        let report = health.run(app()).await;

        assert_eq!(report.checks["search"].error.as_deref(), Some("connection refused"));
    }

    #[tokio::test]
    async fn the_status_is_503_rather_than_500() {
        // "Try another replica, try again shortly" — which is what a load
        // balancer and a retrying client both need to hear.
        let health = Health::new().register("db", |_| async { Err(Error::internal("down")) });

        assert_eq!(health.run(app()).await.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_hanging_check_fails_at_its_deadline() {
        // A probe that hangs is worse than one that fails: the orchestrator
        // waits and decides nothing.
        let health =
            Health::new().timeout(Some(Duration::from_millis(30))).register("slow", |_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            });

        let report = health.run(app()).await;

        assert!(!report.is_healthy());
        assert!(report.checks["slow"].error.as_deref().unwrap().contains("30ms"));
    }

    #[tokio::test]
    async fn checks_run_concurrently() {
        // Six slow checks should take about as long as one, because the probe
        // reading this has a deadline of its own.
        let health = (0..6).fold(Health::new(), |health, i| {
            health.register(format!("slow-{i}"), |_| async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok(())
            })
        });

        let started = Instant::now();
        let report = health.run(app()).await;

        assert!(report.is_healthy());
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "took {:?}, so they ran one after another",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn each_check_is_timed() {
        let health = Health::new().register("slow", |_| async {
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok(())
        });

        assert!(health.run(app()).await.checks["slow"].duration_ms >= 30);
    }

    #[tokio::test]
    async fn the_build_travels_with_the_report() {
        let health = healthy().describing_build(json!({ "version": "2.4.1" }));

        let body = health.render(app()).await.into_string().await.unwrap();
        let document: Value = serde_json::from_str(&body).unwrap();

        assert_eq!(document["build"]["version"], "2.4.1");
        assert_eq!(document["status"], "ok");
    }

    #[tokio::test]
    async fn an_empty_registry_is_healthy_and_says_nothing() {
        // Worth pinning: an empty registry passing is only acceptable because
        // the *endpoint* refuses when nothing is bound at all.
        let report = Health::new().run(app()).await;

        assert!(report.is_healthy());
        assert!(report.checks.is_empty());
    }

    #[tokio::test]
    async fn a_check_can_resolve_from_the_application() {
        struct Thing(&'static str);

        let app = app();
        app.instance(Thing("bound"));

        let health = Health::new().register("thing", |app| async move {
            match app.resolve::<Thing>() {
                Ok(thing) if thing.0 == "bound" => Ok(()),
                _ => Err(Error::internal("not bound")),
            }
        });

        assert!(health.run(app).await.is_healthy());
    }

    #[tokio::test]
    async fn a_panicking_check_does_not_take_the_probe_with_it() {
        // A check is somebody else's closure and one of them will panic
        // eventually. A probe that returns nothing leaves the replica in
        // exactly the unknown state the deadline exists to avoid, so the
        // panic has to become a failed check like any other.
        let health = Health::new()
            .register("fine", |_| async { Ok(()) })
            .register("panics", |_| async { panic!("this check is broken") });

        let report = health.run(app()).await;

        assert!(!report.is_healthy());
        assert_eq!(report.failing(), vec!["panics"]);
        assert!(report.checks["fine"].is_ok(), "the others still reported");
        assert_eq!(report.checks["panics"].error.as_deref(), Some("the check panicked"));
    }
}
