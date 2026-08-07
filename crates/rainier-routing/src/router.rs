//! The [`Router`] — where routes are declared — and [`CompiledRouter`], what
//! actually serves them.
//!
//! The split is deliberate. Declaring a route records the middleware it wants;
//! compiling flattens each route's group and own middleware into one pipeline,
//! **once**, at boot. A request then costs a match and a pipeline run, with no
//! per-request middleware assembly.
//!
//! Middleware is attached by value, not by name — see
//! [`MiddlewareStack`]. Compiling is still
//! where a [deferred](rainier_middleware::MiddlewareStack::deferred) stage is
//! built, so middleware that needs the container fails the boot rather than the
//! first request that reaches it.
//!
//! ```
//! use rainier_container::Container;
//! use rainier_http::{Method, Request, Response};
//! use rainier_routing::{GroupAttributes, Router};
//!
//! async fn index() -> &'static str { "all posts" }
//! async fn show() -> &'static str { "one post" }
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let mut router = Router::new();
//! router.get("/posts", index).name("posts.index");
//!
//! router.group(GroupAttributes::new().prefix("api").name("api."), |router| {
//!     router.get("/posts/{post}", show).name("posts.show").where_number("post");
//! });
//!
//! let compiled = router.compile(&Container::new())?;
//!
//! let response = compiled
//!     .dispatch(Request::builder().method(Method::GET).uri("/api/posts/7").build())
//!     .await;
//! assert_eq!(response.status(), 200);
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use rainier_container::Container;
use rainier_http::{IntoResponse, Method, Request, Response};
use rainier_middleware::{
    ConcreteMiddleware, Destination, IntoMiddlewareStack, Middleware, MiddlewareStack, Pipeline,
    ReadyPipeline,
};
use rainier_support::{BoxedFuture, Error, Result};

use crate::handler::{IntoRouteHandler, RouteHandler};
use crate::route::{normalise_uri, ParamConstraint, Route};

/// Details of the route that matched, placed in the request's extensions so
/// middleware and handlers can inspect it.
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The route's name, if it has one.
    pub name: Option<String>,
    /// The URI pattern (not the concrete path).
    pub uri: String,
    /// The methods the route answers.
    pub methods: Vec<Method>,
}

/// Attributes shared by every route declared inside a
/// [`group`](Router::group).
#[derive(Debug, Default, Clone)]
pub struct GroupAttributes {
    prefix: String,
    name_prefix: String,
    middleware: MiddlewareStack,
    constraints: HashMap<String, ParamConstraint>,
}

impl GroupAttributes {
    /// No attributes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Prepend a URI prefix to every route in the group.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Prepend a name prefix (conventionally ending in `.`) to every *named*
    /// route in the group.
    pub fn name(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = prefix.into();
        self
    }

    /// Apply middleware to every route in the group, outside their own.
    ///
    /// Takes the middleware itself, so a group is a function returning a
    /// [`MiddlewareStack`] rather than a name in a registry:
    ///
    /// ```ignore
    /// router.group(GroupAttributes::new().prefix("api").middleware(kernel::api()), |router| {
    ///     router.get("/posts", index);
    /// });
    /// ```
    pub fn middleware(mut self, middleware: impl IntoMiddlewareStack) -> Self {
        self.middleware = self.middleware.with_stack(middleware.into_middleware_stack());
        self
    }

    /// Apply a parameter constraint to every route in the group that does not
    /// set its own.
    pub fn where_param(mut self, param: impl Into<String>, constraint: ParamConstraint) -> Self {
        self.constraints.insert(param.into(), constraint);
        self
    }
}

/// The route table under declaration.
#[derive(Default)]
pub struct Router {
    routes: Vec<Route>,
    fallback: Option<Arc<dyn RouteHandler>>,
}

impl Router {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    // --- verbs -------------------------------------------------------------

    /// Register a route for `methods`, returning it for further configuration.
    pub fn add<H, Args>(
        &mut self,
        methods: Vec<Method>,
        uri: impl Into<String>,
        handler: H,
    ) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add_erased(methods, uri, handler.into_route_handler())
    }

    /// Register a route from an already-erased handler.
    ///
    /// The path resource routes and other generated routes take, where the
    /// handler was built dynamically rather than inferred from a function
    /// signature.
    pub fn add_erased(
        &mut self,
        methods: Vec<Method>,
        uri: impl Into<String>,
        handler: Arc<dyn RouteHandler>,
    ) -> &mut Route {
        self.routes.push(Route::new(methods, uri, handler));
        self.routes.last_mut().expect("just pushed")
    }

    /// `GET` (and `HEAD`).
    pub fn get<H, Args>(&mut self, uri: impl Into<String>, handler: H) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add(vec![Method::GET], uri, handler)
    }

    /// `POST`.
    pub fn post<H, Args>(&mut self, uri: impl Into<String>, handler: H) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add(vec![Method::POST], uri, handler)
    }

    /// `PUT`.
    pub fn put<H, Args>(&mut self, uri: impl Into<String>, handler: H) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add(vec![Method::PUT], uri, handler)
    }

    /// `PATCH`.
    pub fn patch<H, Args>(&mut self, uri: impl Into<String>, handler: H) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add(vec![Method::PATCH], uri, handler)
    }

    /// `DELETE`.
    pub fn delete<H, Args>(&mut self, uri: impl Into<String>, handler: H) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add(vec![Method::DELETE], uri, handler)
    }

    /// `OPTIONS`.
    pub fn options<H, Args>(&mut self, uri: impl Into<String>, handler: H) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add(vec![Method::OPTIONS], uri, handler)
    }

    /// Every common method.
    pub fn any<H, Args>(&mut self, uri: impl Into<String>, handler: H) -> &mut Route
    where
        H: IntoRouteHandler<Args>,
    {
        self.add(
            vec![
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ],
            uri,
            handler,
        )
    }

    /// A permanent redirect from `from` to `to`.
    pub fn redirect(&mut self, from: impl Into<String>, to: impl Into<String>) -> &mut Route {
        let to = to.into();
        self.get(from, move || {
            let to = to.clone();
            async move { rainier_http::Redirect::permanent(to) }
        })
    }

    /// Whether a fallback has been declared.
    ///
    /// For a caller wanting to install a default without overriding one an
    /// application chose — `bootstrap` installs `public/` this way.
    pub fn has_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    /// The handler for requests that match nothing.
    ///
    /// Without one, an unmatched request is a plain `404`.
    pub fn fallback<H, Args>(&mut self, handler: H)
    where
        H: IntoRouteHandler<Args>,
    {
        self.fallback = Some(handler.into_route_handler());
    }

    // --- groups ------------------------------------------------------------

    /// Declare routes that share a prefix, a name prefix, middleware, or
    /// parameter constraints.
    ///
    /// Groups nest: the attributes are applied to every route the closure
    /// added, *after* any inner group has applied its own — so the outer
    /// prefix ends up outermost and the outer middleware runs first.
    pub fn group(&mut self, attributes: GroupAttributes, declare: impl FnOnce(&mut Router)) {
        let start = self.routes.len();
        declare(self);

        for route in &mut self.routes[start..] {
            route.prefix_with(&attributes.prefix);
            route.prepend_middleware(&attributes.middleware);
            route.prepend_name(&attributes.name_prefix);
            route.add_constraints(&attributes.constraints);
        }
    }

    /// Merge another router's routes into this one, keeping their order.
    pub fn merge(&mut self, other: Router) {
        self.routes.extend(other.routes);
        if self.fallback.is_none() {
            self.fallback = other.fallback;
        }
    }

    // --- inspection --------------------------------------------------------

    /// Every declared route, in declaration order.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Mutable access to the declared routes.
    pub fn routes_mut(&mut self) -> &mut [Route] {
        &mut self.routes
    }

    /// How many routes are declared.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether no routes are declared.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// The URI pattern of a named route.
    pub fn uri_for(&self, name: &str) -> Option<&str> {
        self.routes.iter().find(|route| route.route_name() == Some(name)).map(|route| route.uri())
    }

    /// Every named route, as `(name, uri)`.
    pub fn named_routes(&self) -> Vec<(&str, &str)> {
        self.routes
            .iter()
            .filter_map(|route| route.route_name().map(|name| (name, route.uri())))
            .collect()
    }

    // --- compilation -------------------------------------------------------

    /// The route table, without compiling anything.
    ///
    /// [`CompiledRouter::describe`] needs a container, because compiling
    /// builds every middleware stage — and a `deferred` stage that resolves a
    /// service will fail if that service is not bound. That is the right
    /// behaviour before serving traffic and the wrong behaviour for a question
    /// as harmless as "what routes are there?".
    ///
    /// So `route:list`, a documentation generator and a test asserting on the
    /// table can all ask here. The middleware column is the **declared**
    /// labels, which is what those callers wanted to read anyway.
    pub fn describe(&self) -> Vec<RouteSummary> {
        self.routes
            .iter()
            .map(|route| RouteSummary {
                methods: route.methods().iter().map(|m| m.to_string()).collect(),
                uri: route.uri().to_string(),
                name: route.route_name().map(str::to_string),
                middleware: route.middleware_stack().labels(),
            })
            .collect()
    }

    /// Build every route's pipeline.
    ///
    /// Takes no registry: a route holds its middleware, so there is nothing to
    /// look up. It takes the **container**, because this is where every
    /// [deferred](rainier_middleware::MiddlewareStack::deferred) stage is
    /// built — the point in the lifecycle where the container is populated and
    /// no request has arrived yet.
    pub fn compile(self, container: &Container) -> Result<CompiledRouter> {
        let mut duplicate_names: HashMap<&str, usize> = HashMap::new();
        for route in &self.routes {
            if let Some(name) = route.route_name() {
                *duplicate_names.entry(name).or_insert(0) += 1;
            }
        }
        if let Some((name, _)) = duplicate_names.into_iter().find(|(_, count)| *count > 1) {
            return Err(Error::internal(format!(
                "two routes are both named `{name}` — route names must be unique for URL \
                 generation to be unambiguous"
            )));
        }

        let mut compiled = Vec::with_capacity(self.routes.len());
        for route in self.routes {
            let pipeline = build_pipeline(&route, container)?;
            compiled.push(CompiledRoute { route, pipeline });
        }

        let fallback = self.fallback.map(|handler| {
            let destination: Arc<dyn Destination> = Arc::new(HandlerDestination(handler));
            Pipeline::new().then_arc(destination)
        });

        Ok(CompiledRouter { routes: compiled, fallback })
    }
}

fn build_pipeline(route: &Route, container: &Container) -> Result<ReadyPipeline> {
    // Deferred stages are built here, which is why a failure names the route:
    // "the container has no AuthManager" is a great deal less useful without
    // knowing which route asked for one.
    let resolved = route.middleware_stack().resolve(container).map_err(|e| {
        Error::internal(format!(
            "route `{} {}`: {e}",
            route.methods().first().map(Method::as_str).unwrap_or("?"),
            route.uri()
        ))
    })?;

    // Exclusion happens after resolution because a deferred stage has no
    // identity until it is built — and by type, because a value has no other.
    let excluded = route.excluded_middleware();
    let resolved: Vec<Arc<dyn Middleware>> = resolved
        .into_iter()
        .filter(|stage| !excluded.contains(&ConcreteMiddleware::concrete_type_id(&**stage)))
        .collect();

    let destination: Arc<dyn Destination> =
        Arc::new(HandlerDestination(Arc::clone(route.handler())));
    Ok(Pipeline::new().through_all(resolved).then_arc(destination))
}

/// Adapts a [`RouteHandler`] to the pipeline's [`Destination`].
struct HandlerDestination(Arc<dyn RouteHandler>);

impl Destination for HandlerDestination {
    fn call(&self, request: Request) -> BoxedFuture<Response> {
        self.0.call(request)
    }
}

struct CompiledRoute {
    route: Route,
    pipeline: ReadyPipeline,
}

/// A router with every route's middleware resolved and pipeline built.
pub struct CompiledRouter {
    routes: Vec<CompiledRoute>,
    fallback: Option<ReadyPipeline>,
}

impl CompiledRouter {
    /// Match `request` to a route and run it.
    ///
    /// Routes are tried in **declaration order** and the first match
    /// wins. That means `/posts/create` must be declared before
    /// `/posts/{post}`, and it is why declaration order is preserved rather
    /// than the table being sorted by specificity.
    pub async fn dispatch(&self, mut request: Request) -> Response {
        let path = request.path().to_string();
        let mut allowed: Vec<Method> = Vec::new();

        for compiled in &self.routes {
            let Some(params) = compiled.route.match_path(&path) else {
                continue;
            };

            if !compiled.route.accepts(request.method()) {
                for method in compiled.route.methods() {
                    if !allowed.contains(method) {
                        allowed.push(method.clone());
                    }
                }
                continue;
            }

            // Route parameters and the matched route go in *before* the
            // pipeline runs, so middleware can read them — an authorisation
            // middleware needs `{post}` as much as the handler does.
            request.set_route_params(params);
            request.extensions_mut().insert(MatchedRoute {
                name: compiled.route.route_name().map(str::to_string),
                uri: compiled.route.uri().to_string(),
                methods: compiled.route.methods().to_vec(),
            });

            return compiled.pipeline.run(request).await;
        }

        if !allowed.is_empty() {
            let allow = allowed.iter().map(Method::as_str).collect::<Vec<_>>().join(", ");
            return Error::new(
                rainier_support::ErrorKind::MethodNotAllowed,
                format!("The {} method is not supported for this route.", request.method()),
            )
            .into_response()
            .with_header("allow", &allow);
        }

        match &self.fallback {
            Some(fallback) => fallback.run(request).await,
            None => Error::not_found(format!("No route matches {path}")).into_response(),
        }
    }

    /// A `route:list`-style summary: `(methods, uri, name, middleware)`.
    pub fn describe(&self) -> Vec<RouteSummary> {
        self.routes
            .iter()
            .map(|compiled| RouteSummary {
                methods: compiled.route.methods().iter().map(|m| m.to_string()).collect(),
                uri: compiled.route.uri().to_string(),
                name: compiled.route.route_name().map(str::to_string),
                middleware: compiled.pipeline.stage_names(),
            })
            .collect()
    }

    /// The URI pattern of a named route.
    pub fn uri_for(&self, name: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|compiled| compiled.route.route_name() == Some(name))
            .map(|compiled| compiled.route.uri())
    }

    /// Every named route, as `(name, uri)` — what the URL generator is built
    /// from.
    pub fn named_routes(&self) -> Vec<(String, String)> {
        self.routes
            .iter()
            .filter_map(|compiled| {
                compiled
                    .route
                    .route_name()
                    .map(|name| (name.to_string(), compiled.route.uri().to_string()))
            })
            .collect()
    }

    /// How many routes are served.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether nothing is served.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

impl std::fmt::Debug for CompiledRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledRouter")
            .field("routes", &self.len())
            .field("fallback", &self.fallback.is_some())
            .finish()
    }
}

/// One row of a `route:list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSummary {
    /// The methods the route answers.
    pub methods: Vec<String>,
    /// The URI pattern.
    pub uri: String,
    /// The route's name, if any.
    pub name: Option<String>,
    /// The middleware in its pipeline, outermost first.
    pub middleware: Vec<&'static str>,
}

/// A `404` handler, useful as an explicit fallback.
pub async fn not_found() -> Response {
    Error::not_found("Not Found").into_response()
}

/// Normalise a URI the way route declaration does. Re-exported for callers
/// building paths by hand.
pub fn normalise(uri: &str) -> String {
    normalise_uri(uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::Req;
    use rainier_http::StatusCode;
    use rainier_middleware::{Middleware, MiddlewareStack, Next};

    /// A service nothing binds.
    struct Unbound;

    /// A middleware built from [`Unbound`], so a `deferred` stage naming it
    /// cannot be built.
    struct Nothing;

    #[async_trait::async_trait]
    impl Middleware for Nothing {
        async fn handle(&self, request: Request, next: Next) -> Response {
            next.run(request).await
        }
    }

    async fn index() -> &'static str {
        "index"
    }
    async fn show(request: Req) -> String {
        format!("show:{}", request.route_param("post").unwrap_or("?"))
    }
    async fn store() -> &'static str {
        "store"
    }

    async fn body_of(response: Response) -> String {
        String::from_utf8(response.into_http().into_body().collect().await.unwrap().to_vec())
            .unwrap()
    }

    fn get(uri: &str) -> Request {
        Request::builder().method(Method::GET).uri(uri).build()
    }

    fn compile(router: Router) -> CompiledRouter {
        router.compile(&Container::new()).expect("compiles")
    }

    #[tokio::test]
    async fn dispatches_to_a_static_route() {
        let mut router = Router::new();
        router.get("/posts", index);

        let response = compile(router).dispatch(get("/posts")).await;
        assert_eq!(body_of(response).await, "index");
    }

    #[tokio::test]
    async fn captures_route_parameters() {
        let mut router = Router::new();
        router.get("/posts/{post}", show);

        let response = compile(router).dispatch(get("/posts/7")).await;
        assert_eq!(body_of(response).await, "show:7");
    }

    #[tokio::test]
    async fn an_unmatched_path_is_a_404() {
        let mut router = Router::new();
        router.get("/posts", index);

        let response = compile(router).dispatch(get("/nope")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_matched_path_with_the_wrong_method_is_a_405_listing_what_is_allowed() {
        let mut router = Router::new();
        router.get("/posts", index);
        router.post("/posts", store);

        let request = Request::builder().method(Method::DELETE).uri("/posts").build();
        let response = compile(router).dispatch(request).await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = response.header("allow").unwrap().to_string();
        assert!(allow.contains("GET"), "{allow}");
        assert!(allow.contains("POST"), "{allow}");
    }

    #[tokio::test]
    async fn the_first_declared_route_wins() {
        let mut router = Router::new();
        router.get("/posts/create", || async { "create form" });
        router.get("/posts/{post}", show);

        let compiled = compile(router);
        assert_eq!(body_of(compiled.dispatch(get("/posts/create")).await).await, "create form");
        assert_eq!(body_of(compiled.dispatch(get("/posts/9")).await).await, "show:9");
    }

    #[tokio::test]
    async fn a_constraint_lets_a_later_route_win() {
        let mut router = Router::new();
        router.get("/posts/{post}", show).where_number("post");
        router.get("/posts/create", || async { "create form" });

        let compiled = compile(router);
        // `create` fails the numeric constraint, so matching falls through.
        assert_eq!(body_of(compiled.dispatch(get("/posts/create")).await).await, "create form");
        assert_eq!(body_of(compiled.dispatch(get("/posts/9")).await).await, "show:9");
    }

    #[tokio::test]
    async fn the_fallback_catches_everything_unmatched() {
        let mut router = Router::new();
        router.get("/posts", index);
        router.fallback(|| async { "nothing here" });

        let response = compile(router).dispatch(get("/anywhere/else")).await;
        assert_eq!(body_of(response).await, "nothing here");
    }

    #[tokio::test]
    async fn groups_apply_a_prefix_and_a_name() {
        let mut router = Router::new();
        router.group(GroupAttributes::new().prefix("api/v1").name("api."), |router| {
            router.get("/posts", index).name("posts.index");
        });

        let compiled = compile(router);
        assert_eq!(compiled.uri_for("api.posts.index"), Some("/api/v1/posts"));
        assert_eq!(compiled.dispatch(get("/api/v1/posts")).await.status(), StatusCode::OK);
        assert_eq!(compiled.dispatch(get("/posts")).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn groups_nest_outermost_first() {
        let mut router = Router::new();
        router.group(GroupAttributes::new().prefix("api").name("api."), |router| {
            router.group(GroupAttributes::new().prefix("v1").name("v1."), |router| {
                router.get("/posts", index).name("posts.index");
            });
        });

        let compiled = compile(router);
        assert_eq!(compiled.uri_for("api.v1.posts.index"), Some("/api/v1/posts"));
    }

    #[tokio::test]
    async fn group_constraints_apply_to_every_route_that_lacks_its_own() {
        let mut router = Router::new();
        router.group(
            GroupAttributes::new().where_param("post", ParamConstraint::Number),
            |router| {
                router.get("/posts/{post}", show);
            },
        );

        let compiled = compile(router);
        assert_eq!(compiled.dispatch(get("/posts/7")).await.status(), StatusCode::OK);
        assert_eq!(compiled.dispatch(get("/posts/abc")).await.status(), StatusCode::NOT_FOUND);
    }

    struct Tag(&'static str);

    #[async_trait::async_trait]
    impl Middleware for Tag {
        async fn handle(&self, request: Request, next: Next) -> Response {
            next.run(request).await.with_added_header("x-tag", self.0)
        }
        fn name(&self) -> &'static str {
            "Tag"
        }
    }

    /// A second middleware type, so exclusion has something to discriminate on.
    struct Audit;

    #[async_trait::async_trait]
    impl Middleware for Audit {
        async fn handle(&self, request: Request, next: Next) -> Response {
            next.run(request).await.with_added_header("x-tag", "audit")
        }
    }

    #[tokio::test]
    async fn route_and_group_middleware_both_run() {
        let mut router = Router::new();
        router.group(GroupAttributes::new().middleware(Tag("group")), |router| {
            router.get("/posts", index).middleware(Tag("route"));
        });

        let compiled = router.compile(&Container::new()).unwrap();
        let response = compiled.dispatch(get("/posts")).await;
        let tags: Vec<_> =
            response.headers().get_all("x-tag").iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(tags.len(), 2);
    }

    #[tokio::test]
    async fn a_tuple_attaches_several_in_order() {
        let mut router = Router::new();
        router.get("/posts", index).middleware((Tag("first"), Audit));

        let compiled = router.compile(&Container::new()).unwrap();
        let response = compiled.dispatch(get("/posts")).await;
        let tags: Vec<_> =
            response.headers().get_all("x-tag").iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(tags, vec!["audit", "first"], "the outermost runs last on the way out");
    }

    #[tokio::test]
    async fn a_route_can_opt_out_of_group_middleware_by_type() {
        let mut router = Router::new();
        router.group(GroupAttributes::new().middleware((Tag("group"), Audit)), |router| {
            router.get("/open", index).without_middleware::<Audit>();
            router.get("/closed", index);
        });

        let compiled = router.compile(&Container::new()).unwrap();

        let open = compiled.dispatch(get("/open")).await;
        let open_tags: Vec<_> =
            open.headers().get_all("x-tag").iter().map(|v| v.to_str().unwrap()).collect();
        assert_eq!(open_tags, vec!["group"], "only the excluded type is dropped");

        let closed = compiled.dispatch(get("/closed")).await;
        assert_eq!(closed.headers().get_all("x-tag").iter().count(), 2);
    }

    #[test]
    fn a_deferred_stage_that_cannot_be_built_names_the_route() {
        // The replacement for "unknown middleware alias": the only way a route
        // can now fail to compile is middleware that needs something the
        // container does not have.
        let mut router = Router::new();
        router.get("/posts", index).middleware(
            MiddlewareStack::new()
                .deferred(|_| Err::<Audit, _>(Error::internal("AuthManager is not bound"))),
        );

        let err = router.compile(&Container::new()).err().expect("compilation should fail");
        assert!(err.message().contains("/posts"), "{}", err.message());
        assert!(err.message().contains("Audit"), "{}", err.message());
        assert!(err.message().contains("AuthManager"), "{}", err.message());
    }

    #[test]
    fn duplicate_route_names_are_rejected() {
        let mut router = Router::new();
        router.get("/a", index).name("same");
        router.get("/b", index).name("same");

        let err = router.compile(&Container::new()).err().expect("should fail");
        assert!(err.message().contains("`same`"), "{}", err.message());
    }

    #[tokio::test]
    async fn the_matched_route_is_readable_from_the_request() {
        async fn reveal(request: Req) -> String {
            request
                .extension::<MatchedRoute>()
                .map(|route| format!("{}|{:?}", route.uri, route.name))
                .unwrap_or_default()
        }

        let mut router = Router::new();
        router.get("/posts/{post}", reveal).name("posts.show");

        let response = compile(router).dispatch(get("/posts/1")).await;
        assert_eq!(body_of(response).await, "/posts/{post}|Some(\"posts.show\")");
    }

    #[tokio::test]
    async fn redirect_routes_send_a_location() {
        let mut router = Router::new();
        router.redirect("/old", "/new");

        let response = compile(router).dispatch(get("/old")).await;
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(response.header("location"), Some("/new"));
    }

    #[tokio::test]
    async fn merging_keeps_both_route_sets() {
        let mut web = Router::new();
        web.get("/", index).name("home");

        let mut api = Router::new();
        api.get("/api/posts", index).name("api.posts");

        web.merge(api);
        let compiled = compile(web);
        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled.dispatch(get("/api/posts")).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn describe_reports_the_table() {
        let mut router = Router::new();
        router.get("/posts", index).name("posts.index").middleware(Tag("route"));

        let compiled = router.compile(&Container::new()).unwrap();
        let rows = compiled.describe();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uri, "/posts");
        assert_eq!(rows[0].name.as_deref(), Some("posts.index"));
        assert!(rows[0].methods.contains(&"GET".to_string()));
        assert_eq!(rows[0].middleware, vec!["Tag"]);
    }

    #[tokio::test]
    async fn head_is_served_wherever_get_is() {
        let mut router = Router::new();
        router.get("/posts", index);

        let request = Request::builder().method(Method::HEAD).uri("/posts").build();
        assert_eq!(compile(router).dispatch(request).await.status(), StatusCode::OK);
    }

    #[test]
    fn an_uncompiled_router_can_still_describe_itself() {
        // The point: no container, so nothing resolves and nothing can fail.
        let mut router = Router::new();
        router.get("/", || async { "home" }).name("home");
        router.post("/posts", || async { "create" }).name("posts.store").middleware(
            // A stage that needs a service nothing has bound: compiling this
            // router would fail, and describing it must not.
            MiddlewareStack::new().resolved(|_: Arc<Unbound>| Nothing),
        );

        let table = router.describe();

        assert_eq!(table.len(), 2);
        assert_eq!(table[0].uri, "/");
        assert_eq!(table[0].name.as_deref(), Some("home"));
        assert_eq!(table[1].methods, vec!["POST"]);
        assert_eq!(table[1].middleware.len(), 1);

        // And the proof that this is worth having: compiling cannot answer.
        assert!(router.compile(&Container::new()).is_err());
    }

    #[tokio::test]
    async fn describing_uncompiled_matches_describing_compiled() {
        // Two implementations of the same table would drift. This is the test
        // that notices.
        let mut router = Router::new();
        router.get("/", || async { "home" }).name("home");
        router.put("/posts/{post}", || async { "update" }).name("posts.update");

        let before = router.describe();
        let after = router.compile(&Container::new()).unwrap().describe();

        assert_eq!(before.len(), after.len());
        for (before, after) in before.iter().zip(after.iter()) {
            assert_eq!(before.methods, after.methods);
            assert_eq!(before.uri, after.uri);
            assert_eq!(before.name, after.name);
        }
    }
}
