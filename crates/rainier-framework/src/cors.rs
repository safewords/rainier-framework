//! Where a CORS policy has to be registered, and what happens when it is not.
//!
//! ```ignore
//! // app/http/kernel.rs — global, not on a group.
//! registry.global(HandleCors::for_origins(["https://app.example"]).allow_credentials(true));
//! ```
//!
//! [`HandleCors`](rainier_middleware::HandleCors) answers a preflight itself,
//! which is why it must be somewhere a preflight reaches. A browser asks
//! permission before sending anything that is not a "simple" request —
//! everything carrying `Authorization`, and every `POST` of JSON — and it asks
//! with `OPTIONS` against the same path. No route declares `OPTIONS`, so the
//! router matches the path, rejects the method, and answers `405` **before**
//! entering the route's own pipeline. Middleware on a route or a group lives in
//! that pipeline and never runs.
//!
//! The result is not a policy that fails loudly. It is one that answers exactly
//! the requests that never needed it — a plain `GET` is decorated correctly —
//! and refuses every preflight, so the whole authenticated surface is
//! unreachable from a browser while every test that asserts a `GET` passes.
//!
//! Global middleware wraps the router instead of sitting inside it, so it sees
//! the preflight before routing does. It also puts the headers on `404`s and
//! `405`s, which is worth having: without them a browser reports a mistyped URL
//! as a CORS failure and the search starts in the wrong file.
//!
//! So the bootstrap **says so at boot** when it finds a policy on a route
//! pipeline and none in the global stack.

use std::sync::Arc;

use rainier_middleware::Middleware;
use rainier_routing::CompiledRouter;

/// The label [`HandleCors`](rainier_middleware::HandleCors) reports.
///
/// Matched by name for the same reason the rate-limit check matches
/// `"Throttle"`: a built stage is an `Arc<dyn Middleware>`, and the trait
/// carries no downcast. The framework owns both the type and the name, so the
/// two cannot drift apart without this file's tests noticing.
const CORS: &str = "HandleCors";

/// Say so when a CORS policy is mounted where a preflight cannot reach it.
///
/// A warning rather than a refusal. The application is serving, and the routes
/// a non-browser client uses are unaffected — CORS is a browser rule and
/// nothing else consults it. Refusing to start over it would turn a broken
/// front end into a broken deployment.
///
/// Silent when the global stack carries one, whether or not a group does too:
/// an application may legitimately want a second, narrower policy on a group,
/// and the global one is what answers the preflight either way.
pub fn warn_if_cors_cannot_answer_a_preflight(
    global: &[Arc<dyn Middleware>],
    router: &CompiledRouter,
) {
    if global.iter().any(|stage| stage.name().contains(CORS)) {
        // A preflight is answered before routing. Nothing to say.
        return;
    }

    let mounted: Vec<String> = router
        .describe()
        .into_iter()
        .filter(|route| route.middleware.iter().any(|stage| stage.contains(CORS)))
        .map(|route| format!("{} {}", route.methods.join("|"), route.uri))
        .collect();

    if mounted.is_empty() {
        // No policy anywhere. An application that serves no browser from
        // another origin does not need one, and guessing that it does would be
        // a warning on every API that is only ever called by a server.
        return;
    }

    tracing::warn!(
        routes = %mounted.join(", "),
        "{} route(s) carry a CORS policy on their own pipeline and the global stack carries \
         none, so no preflight is ever answered: a browser asks with OPTIONS, no route accepts \
         OPTIONS, and the router replies 405 before the policy runs. Only requests needing no \
         preflight work — every call carrying Authorization, and every POST of JSON, is blocked \
         by the browser. Register it globally instead: \
         registry.global(HandleCors::for_origins([..]).allow_credentials(true)).",
        mounted.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_container::Container;
    use rainier_middleware::{HandleCors, ThrottleRequests};
    use rainier_routing::Router;

    fn compiled(build: impl FnOnce(&mut Router)) -> CompiledRouter {
        let mut router = Router::new();
        build(&mut router);
        router.compile(&Container::new()).unwrap()
    }

    #[test]
    fn the_label_this_matches_on_is_the_one_the_middleware_reports() {
        // The check is stringly-typed by necessity, so this is the assertion
        // that keeps it honest: rename the middleware and this fails here
        // rather than turning the warning into a no-op nobody notices.
        assert_eq!(Middleware::name(&HandleCors::any_origin()), CORS);
    }

    #[test]
    fn a_policy_on_a_route_with_none_global_is_worth_saying() {
        let router = compiled(|router| {
            router.get("/api/posts", || async { "posts" }).middleware(HandleCors::any_origin());
        });

        assert!(router.describe()[0].middleware.iter().any(|s| s.contains(CORS)));
        // The condition the warning fires on, asserted directly — `tracing`
        // output is not capturable here, so the test pins the inputs that
        // reach it.
        warn_if_cors_cannot_answer_a_preflight(&[], &router);
    }

    #[test]
    fn a_global_policy_silences_it_even_when_a_group_has_one_too() {
        let router = compiled(|router| {
            router.get("/api/posts", || async { "posts" }).middleware(HandleCors::any_origin());
        });
        let global: Vec<Arc<dyn Middleware>> = vec![Arc::new(HandleCors::any_origin())];

        // A second, narrower policy on a group is a legitimate arrangement:
        // the global one still answers the preflight.
        assert!(global.iter().any(|stage| stage.name().contains(CORS)));
        warn_if_cors_cannot_answer_a_preflight(&global, &router);
    }

    #[test]
    fn an_application_with_no_policy_at_all_is_not_nagged() {
        // An API only ever called by another server needs none, and warning
        // about its absence would fire on every one of them.
        let router = compiled(|router| {
            router
                .get("/api/posts", || async { "posts" })
                .middleware(ThrottleRequests::per_minute(60));
        });

        assert!(!router.describe()[0].middleware.iter().any(|s| s.contains(CORS)));
        warn_if_cors_cannot_answer_a_preflight(&[], &router);
    }
}
