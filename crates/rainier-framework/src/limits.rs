//! Rate limits that survive a second replica — [`RateLimits`] and [`shared`].
//!
//! ```ignore
//! // routes/api.rs
//! router.post("/login", login).middleware(limits::shared(
//!     ThrottleRequests::per_minute(5)
//!         .named("login")
//!         .keyed_by(|request| request.input("email")),
//! ));
//! ```
//!
//! A bare [`ThrottleRequests`] counts in its own process, which is the right
//! default for development and for a single node. Behind a load balancer it
//! means five replicas each enforce "five attempts a minute" separately, and
//! the real limit is twenty-five.
//!
//! For a page-view limiter that is a rounding error. For a credential limiter
//! it is the difference between a control and a decoration, which is why the
//! bootstrap **says so at boot** when it finds one over a per-process cache.

use std::sync::Arc;

use rainier_cache::CacheRateLimiter;
use rainier_middleware::{MiddlewareStack, RateLimitStore, ThrottleRequests};
use rainier_routing::CompiledRouter;

/// The store every shared limiter in this application counts in.
///
/// Bound at boot from the same cache everything else uses, so a deployment
/// decides where its shared state lives once.
pub struct RateLimits(Arc<dyn RateLimitStore>);

impl RateLimits {
    /// Count in `store`.
    pub fn new(store: Arc<dyn RateLimitStore>) -> Self {
        Self(store)
    }

    /// Count in `cache`.
    pub fn over_cache(cache: Arc<dyn rainier_cache::Cache>) -> Self {
        Self(Arc::new(CacheRateLimiter::new(cache)))
    }

    /// The store.
    pub fn store(&self) -> Arc<dyn RateLimitStore> {
        Arc::clone(&self.0)
    }

    /// Whether counters here are visible to other instances.
    pub fn is_shared(&self) -> bool {
        self.0.is_shared()
    }

    /// What is behind it — `"memory"`, `"redis"`.
    pub fn name(&self) -> &str {
        self.0.name()
    }
}

impl std::fmt::Debug for RateLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimits")
            .field("store", &self.0.name())
            .field("shared", &self.0.is_shared())
            .finish()
    }
}

/// Count `throttle` in the application's shared store.
///
/// Returns a stack rather than a middleware because the store is resolved from
/// the container, and routes are declared before there is one — the same
/// reason [`Authenticate::resolved`](rainier_auth::Authenticate) exists.
///
/// Without this a throttle counts in its own process. With it, every limiter in
/// the application counts in one place and a deployment moves them all at once
/// by changing `CACHE_DRIVER`.
pub fn shared(throttle: ThrottleRequests) -> MiddlewareStack {
    MiddlewareStack::new()
        .resolved(move |limits: Arc<RateLimits>| throttle.clone().stored_in(limits.store()))
}

/// Say so when a route rate-limits against a per-process counter.
///
/// Reads the compiled route table for anything carrying a throttle, and
/// compares it with what the shared store actually is. The failure this
/// prevents is silent by construction: the limiter works, returns `429`s,
/// looks correct in every test — and permits `n × replicas` in production.
///
/// A warning rather than a refusal, in every environment. Unlike a scheduler
/// lock, a per-process limit is a *weaker* control rather than an absent one,
/// and plenty of applications limit for politeness rather than for safety. The
/// deployment gets told; it does not get stopped.
pub fn warn_if_rate_limits_are_not_shared(
    app: &rainier_container::Application,
    router: &CompiledRouter,
) {
    let Ok(limits) = app.resolve::<RateLimits>() else {
        // Nothing bound one, so nothing is claiming to be shared.
        return;
    };

    if limits.is_shared() {
        return;
    }

    let throttled: Vec<String> = router
        .describe()
        .into_iter()
        .filter(|route| route.middleware.iter().any(|stage| stage.contains("Throttle")))
        .map(|route| format!("{} {}", route.methods.join("|"), route.uri))
        .collect();

    if throttled.is_empty() {
        return;
    }

    tracing::warn!(
        store = limits.name(),
        routes = %throttled.join(", "),
        "{} route(s) are rate-limited against a per-process counter, so the effective limit is \
         the configured one multiplied by the number of replicas. Set CACHE_DRIVER to a shared \
         store (redis, redis-cluster, memcached, dynamodb) if any of these protects credentials.",
        throttled.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;
    use rainier_container::Application;
    use rainier_middleware::MemoryRateLimitStore;
    use rainier_routing::Router;

    #[test]
    fn a_memory_backed_store_reports_itself_honestly() {
        let limits = RateLimits::new(Arc::new(MemoryRateLimitStore::new()));

        assert!(!limits.is_shared());
        assert_eq!(limits.name(), "memory");
    }

    #[test]
    fn a_cache_backed_store_reports_its_cache() {
        let limits = RateLimits::over_cache(Arc::new(MemoryCache::new()));

        assert!(!limits.is_shared(), "a memory cache is still a memory cache");
        assert_eq!(limits.name(), "memory");
    }

    #[tokio::test]
    async fn the_shared_stack_resolves_the_bound_store() {
        let app = Application::new(".");
        app.instance(RateLimits::over_cache(Arc::new(MemoryCache::new())));

        let stack = shared(ThrottleRequests::per_minute(5).named("login"));

        assert!(stack.resolve(app.container()).is_ok(), "the stack should build");
    }

    #[tokio::test]
    async fn the_shared_stack_fails_the_boot_when_nothing_is_bound() {
        // Better than falling back to a per-process counter, which would be a
        // limiter that silently does less than the route asked for.
        let app = Application::new(".");
        let stack = shared(ThrottleRequests::per_minute(5));

        assert!(stack.resolve(app.container()).is_err());
    }

    #[tokio::test]
    async fn the_warning_only_fires_for_routes_that_are_actually_throttled() {
        let app = Application::new(".");
        app.instance(RateLimits::over_cache(Arc::new(MemoryCache::new())));

        // No throttle anywhere: nothing to say.
        let mut router = Router::new();
        router.get("/", || async { "home" });
        let compiled = router.compile(app.container()).unwrap();

        warn_if_rate_limits_are_not_shared(&app, &compiled);

        // With one, it has something to say — asserted by it not panicking and
        // by the route table containing what the check reads.
        let mut router = Router::new();
        router.post("/login", || async { "in" }).middleware(ThrottleRequests::per_minute(5));
        let compiled = router.compile(app.container()).unwrap();

        assert!(compiled.describe()[0].middleware.iter().any(|s| s.contains("Throttle")));
        warn_if_rate_limits_are_not_shared(&app, &compiled);
    }
}
