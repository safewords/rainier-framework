//! Resource routes — [`ResourceController`] and the seven RESTful routes
//! `router.resource` generates.
//!
//! | Verb | URI | Action | Name |
//! |---|---|---|---|
//! | GET | `/posts` | index | `posts.index` |
//! | GET | `/posts/create` | create | `posts.create` |
//! | POST | `/posts` | store | `posts.store` |
//! | GET | `/posts/{post}` | show | `posts.show` |
//! | GET | `/posts/{post}/edit` | edit | `posts.edit` |
//! | PUT, PATCH | `/posts/{post}` | update | `posts.update` |
//! | DELETE | `/posts/{post}` | destroy | `posts.destroy` |
//!
//! The parameter name is the singular of the resource name (`posts` → `post`),
//! and `create`/`edit` are registered **before** `{post}` so the literal
//! segments are not swallowed by the parameter.
//!
//! Every action defaults to `405 Method Not Allowed`, so a controller
//! implements only what it supports and the rest answer honestly rather than
//! panicking or 404-ing.

use std::sync::Arc;

use rainier_http::{IntoResponse, Method, Request, Response};
use rainier_middleware::{IntoMiddlewareStack, MiddlewareStack};
use rainier_support::{str, BoxedFuture, Error, ErrorKind};

use crate::handler::RouteHandler;
use crate::router::Router;

/// One of the seven RESTful actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceAction {
    /// List the collection.
    Index,
    /// Show the creation form.
    Create,
    /// Persist a new member.
    Store,
    /// Show one member.
    Show,
    /// Show the edit form for one member.
    Edit,
    /// Update one member.
    Update,
    /// Delete one member.
    Destroy,
}

impl ResourceAction {
    /// All seven, in registration order.
    pub const ALL: [ResourceAction; 7] = [
        ResourceAction::Index,
        ResourceAction::Create,
        ResourceAction::Store,
        ResourceAction::Show,
        ResourceAction::Edit,
        ResourceAction::Update,
        ResourceAction::Destroy,
    ];

    /// The five an API needs — no HTML form pages.
    pub const API: [ResourceAction; 5] = [
        ResourceAction::Index,
        ResourceAction::Store,
        ResourceAction::Show,
        ResourceAction::Update,
        ResourceAction::Destroy,
    ];

    /// The action's name, as used in the route name suffix.
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceAction::Index => "index",
            ResourceAction::Create => "create",
            ResourceAction::Store => "store",
            ResourceAction::Show => "show",
            ResourceAction::Edit => "edit",
            ResourceAction::Update => "update",
            ResourceAction::Destroy => "destroy",
        }
    }

    fn methods(self) -> Vec<Method> {
        match self {
            ResourceAction::Index
            | ResourceAction::Create
            | ResourceAction::Show
            | ResourceAction::Edit => vec![Method::GET],
            ResourceAction::Store => vec![Method::POST],
            ResourceAction::Update => vec![Method::PUT, Method::PATCH],
            ResourceAction::Destroy => vec![Method::DELETE],
        }
    }

    /// The URI suffix appended to the resource's base path.
    fn uri_suffix(self, param: &str) -> String {
        match self {
            ResourceAction::Index | ResourceAction::Store => String::new(),
            ResourceAction::Create => "/create".to_string(),
            ResourceAction::Show | ResourceAction::Update | ResourceAction::Destroy => {
                format!("/{{{param}}}")
            }
            ResourceAction::Edit => format!("/{{{param}}}/edit"),
        }
    }
}

/// The name of an action a [`ControllerMiddleware`] rule can be scoped to.
///
/// A name rather than a [`ResourceAction`], because not every controller is a
/// RESTful resource. A controller with actions like `search` or `export` has
/// nothing to say in a seven-variant enum, and scoping rules on that enum
/// leaves those actions unable to carry a rule at all.
///
/// [`ResourceAction`] converts into one, so a resource controller keeps
/// naming its actions with the enum and gets the compiler's spell-checking.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActionName(String);

impl ActionName {
    /// The name as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<ResourceAction> for ActionName {
    fn from(action: ResourceAction) -> Self {
        ActionName(action.as_str().to_string())
    }
}

impl From<&str> for ActionName {
    fn from(name: &str) -> Self {
        ActionName(name.to_string())
    }
}

impl From<String> for ActionName {
    fn from(name: String) -> Self {
        ActionName(name)
    }
}

/// Which actions one rule covers.
///
/// Held as the rule that was written rather than as the set of actions it
/// resolved to, which is what lets [`ControllerMiddleware::except`] work on a
/// controller whose full action list the framework never sees: "everything but
/// these" is answerable for an action nobody enumerated, where "all seven minus
/// these" is not.
#[derive(Clone, Debug)]
enum Scope {
    Always,
    Only(Vec<ActionName>),
    Except(Vec<ActionName>),
}

impl Scope {
    fn covers(&self, action: &ActionName) -> bool {
        match self {
            Scope::Always => true,
            Scope::Only(actions) => actions.contains(action),
            Scope::Except(actions) => !actions.contains(action),
        }
    }
}

/// Middleware a controller applies to its own actions.
///
/// Declaring it on the controller rather than at the route puts the rule next
/// to the code it protects: someone adding a `destroy` action reads the guard
/// in the same file, instead of having to remember that a route file three
/// directories away is what stops it being public.
///
/// Actions are named by [`ResourceAction`] on a resource controller, or by
/// plain name on one that is not a resource — the two mix freely, because both
/// convert into [`ActionName`].
///
/// ```
/// use rainier_routing::{ControllerMiddleware, ResourceAction};
/// # use rainier_middleware::{AddHeaders, ThrottleRequests};
///
/// let middleware = ControllerMiddleware::new()
///     // Every action.
///     .always(AddHeaders::security_defaults())
///     // Only the ones that change something.
///     .only(
///         [ResourceAction::Store, ResourceAction::Update, ResourceAction::Destroy],
///         ThrottleRequests::per_minute(20),
///     );
///
/// assert_eq!(middleware.for_action(ResourceAction::Index).len(), 1);
/// assert_eq!(middleware.for_action(ResourceAction::Store).len(), 2);
/// ```
///
/// A controller whose actions are not the RESTful seven scopes by name, which
/// is the same rule written the same way:
///
/// ```
/// use rainier_routing::ControllerMiddleware;
/// # use rainier_middleware::ThrottleRequests;
///
/// let middleware = ControllerMiddleware::new()
///     .except(["show", "search"], ThrottleRequests::per_minute(20));
///
/// assert!(middleware.for_action("search").is_empty());
/// assert_eq!(middleware.for_action("export").len(), 1);
/// ```
#[derive(Default, Clone, Debug)]
pub struct ControllerMiddleware {
    entries: Vec<(Scope, MiddlewareStack)>,
}

impl ControllerMiddleware {
    /// No middleware.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply to every action.
    pub fn always(mut self, middleware: impl IntoMiddlewareStack) -> Self {
        self.entries.push((Scope::Always, middleware.into_middleware_stack()));
        self
    }

    /// Apply to the listed actions only.
    pub fn only(
        mut self,
        actions: impl IntoIterator<Item = impl Into<ActionName>>,
        middleware: impl IntoMiddlewareStack,
    ) -> Self {
        let actions = actions.into_iter().map(Into::into).collect();
        self.entries.push((Scope::Only(actions), middleware.into_middleware_stack()));
        self
    }

    /// Apply to every action but the listed ones.
    ///
    /// The safer default of the two: a new action added later is guarded unless
    /// it is named here, rather than unguarded unless it is named in an
    /// [`only`](Self::only).
    ///
    /// That guarantee is the reason this stores the exclusion rather than the
    /// inclusion it would expand to. An action added next year is covered
    /// because it is not in the list, without anyone having to re-register
    /// anything — which is precisely the case the safer default exists for.
    pub fn except(
        mut self,
        actions: impl IntoIterator<Item = impl Into<ActionName>>,
        middleware: impl IntoMiddlewareStack,
    ) -> Self {
        let actions = actions.into_iter().map(Into::into).collect();
        self.entries.push((Scope::Except(actions), middleware.into_middleware_stack()));
        self
    }

    /// The stack for one action, in declaration order.
    pub fn for_action(&self, action: impl Into<ActionName>) -> MiddlewareStack {
        let action = action.into();

        self.entries
            .iter()
            .filter(|(scope, _)| scope.covers(&action))
            .fold(MiddlewareStack::new(), |stack, (_, middleware)| {
                stack.with_stack(middleware.clone())
            })
    }

    /// Whether anything is declared.
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|(_, stack)| stack.is_empty())
    }
}

/// A controller serving a RESTful resource.
///
/// Implement the actions the resource supports; the rest answer `405`.
///
/// ```
/// use rainier_http::{Request, Response};
/// use rainier_routing::{ResourceController, Router};
/// use std::sync::Arc;
///
/// struct PostController;
///
/// #[async_trait::async_trait]
/// impl ResourceController for PostController {
///     async fn index(&self, _request: Request) -> Response {
///         Response::text("every post")
///     }
///     async fn show(&self, request: Request) -> Response {
///         Response::text(format!("post {}", request.route_param("post").unwrap_or("?")))
///     }
/// }
///
/// let mut router = Router::new();
/// router.resource("posts", Arc::new(PostController));
/// assert_eq!(router.uri_for("posts.show"), Some("/posts/{post}"));
/// ```
#[async_trait::async_trait]
pub trait ResourceController: Send + Sync + 'static {
    /// `GET /resource`
    async fn index(&self, request: Request) -> Response {
        unsupported(request, "index")
    }

    /// `GET /resource/create`
    async fn create(&self, request: Request) -> Response {
        unsupported(request, "create")
    }

    /// `POST /resource`
    async fn store(&self, request: Request) -> Response {
        unsupported(request, "store")
    }

    /// `GET /resource/{id}`
    async fn show(&self, request: Request) -> Response {
        unsupported(request, "show")
    }

    /// `GET /resource/{id}/edit`
    async fn edit(&self, request: Request) -> Response {
        unsupported(request, "edit")
    }

    /// `PUT|PATCH /resource/{id}`
    async fn update(&self, request: Request) -> Response {
        unsupported(request, "update")
    }

    /// `DELETE /resource/{id}`
    async fn destroy(&self, request: Request) -> Response {
        unsupported(request, "destroy")
    }

    /// Middleware this controller applies to its own actions.
    ///
    /// Runs **inside** anything the route's group applied and outside the
    /// action itself, so a group's session middleware still wraps a
    /// controller's authorisation check.
    ///
    /// ```ignore
    /// fn middleware(&self) -> ControllerMiddleware {
    ///     ControllerMiddleware::new()
    ///         .except([ResourceAction::Index, ResourceAction::Show], Authenticate::new(auth))
    /// }
    /// ```
    fn middleware(&self) -> ControllerMiddleware {
        ControllerMiddleware::new()
    }
}

fn unsupported(request: Request, action: &str) -> Response {
    Error::new(
        ErrorKind::MethodNotAllowed,
        format!("This resource does not support the `{action}` action."),
    )
    .with_details(serde_json_path(request.path()))
    .into_response()
}

fn serde_json_path(path: &str) -> serde_json::Value {
    serde_json::json!({ "path": path })
}

/// Routes one action to its controller method.
struct ResourceHandler {
    controller: Arc<dyn ResourceController>,
    action: ResourceAction,
}

impl RouteHandler for ResourceHandler {
    fn call(&self, request: Request) -> BoxedFuture<Response> {
        // The `Arc` is cloned into the future so the borrow the async trait
        // method takes of `&*controller` outlives this call.
        let controller = Arc::clone(&self.controller);
        let action = self.action;

        Box::pin(async move {
            match action {
                ResourceAction::Index => controller.index(request).await,
                ResourceAction::Create => controller.create(request).await,
                ResourceAction::Store => controller.store(request).await,
                ResourceAction::Show => controller.show(request).await,
                ResourceAction::Edit => controller.edit(request).await,
                ResourceAction::Update => controller.update(request).await,
                ResourceAction::Destroy => controller.destroy(request).await,
            }
        })
    }
}

impl Router {
    /// Register all seven RESTful routes for `name`.
    pub fn resource(&mut self, name: &str, controller: Arc<dyn ResourceController>) {
        self.resource_actions(name, controller, &ResourceAction::ALL);
    }

    /// Register the five API routes for `name` — everything except the
    /// `create` and `edit` form pages.
    pub fn api_resource(&mut self, name: &str, controller: Arc<dyn ResourceController>) {
        self.resource_actions(name, controller, &ResourceAction::API);
    }

    /// Register exactly the listed actions.
    ///
    /// They are always registered in [`ResourceAction::ALL`] order regardless
    /// of the order given, because `/posts/create` must be declared before
    /// `/posts/{post}` or the parameter swallows it.
    pub fn resource_actions(
        &mut self,
        name: &str,
        controller: Arc<dyn ResourceController>,
        actions: &[ResourceAction],
    ) {
        let base = format!("/{}", name.trim_matches('/'));
        let param = str::singular(name.rsplit('/').next().unwrap_or(name));

        // Asked once rather than per action: a controller building its stack in
        // `middleware()` should not have that run seven times.
        let middleware = controller.middleware();

        for action in ResourceAction::ALL {
            if !actions.contains(&action) {
                continue;
            }

            let handler: Arc<dyn RouteHandler> =
                Arc::new(ResourceHandler { controller: Arc::clone(&controller), action });

            let uri = format!("{base}{}", action.uri_suffix(&param));
            let route_name = format!("{}.{}", name.replace('/', "."), action.as_str());

            let route = self.add_erased(action.methods(), uri, handler);
            route.name(route_name);

            let for_action = middleware.for_action(action);
            if !for_action.is_empty() {
                route.middleware(for_action);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::CompiledRouter;
    use rainier_http::StatusCode;

    struct Posts;

    #[async_trait::async_trait]
    impl ResourceController for Posts {
        async fn index(&self, _: Request) -> Response {
            Response::text("index")
        }
        async fn show(&self, request: Request) -> Response {
            Response::text(format!("show:{}", request.route_param("post").unwrap_or("?")))
        }
        async fn store(&self, _: Request) -> Response {
            Response::text("store")
        }
        async fn destroy(&self, _: Request) -> Response {
            Response::text("destroy")
        }
    }

    fn compiled(router: Router) -> CompiledRouter {
        router.compile(&rainier_container::Container::new()).expect("compiles")
    }

    async fn body_of(response: Response) -> String {
        String::from_utf8(response.into_http().into_body().collect().await.unwrap().to_vec())
            .unwrap()
    }

    fn request(method: Method, uri: &str) -> Request {
        Request::builder().method(method).uri(uri).build()
    }

    fn posts_router() -> Router {
        let mut router = Router::new();
        router.resource("posts", Arc::new(Posts));
        router
    }

    // --- controller middleware ---------------------------------------------

    struct Tag(&'static str);

    #[async_trait::async_trait]
    impl rainier_middleware::Middleware for Tag {
        async fn handle(&self, request: Request, next: rainier_middleware::Next) -> Response {
            next.run(request).await.with_added_header("x-tag", self.0)
        }
    }

    /// A controller that guards its writes and nothing else.
    struct Guarded;

    #[async_trait::async_trait]
    impl ResourceController for Guarded {
        async fn index(&self, _: Request) -> Response {
            Response::text("index")
        }
        async fn store(&self, _: Request) -> Response {
            Response::text("store")
        }
        async fn destroy(&self, _: Request) -> Response {
            Response::text("destroy")
        }

        fn middleware(&self) -> ControllerMiddleware {
            ControllerMiddleware::new()
                .always(Tag("every"))
                .only([ResourceAction::Store, ResourceAction::Destroy], Tag("writes"))
        }
    }

    fn tags(response: &Response) -> Vec<String> {
        response
            .headers()
            .get_all("x-tag")
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn controller_middleware_applies_only_to_the_actions_it_names() {
        let mut router = Router::new();
        router.resource("posts", Arc::new(Guarded));
        let compiled = compiled(router);

        let read = compiled.dispatch(request(Method::GET, "/posts")).await;
        assert_eq!(tags(&read), vec!["every"]);

        let write = compiled.dispatch(request(Method::POST, "/posts")).await;
        assert_eq!(tags(&write), vec!["writes", "every"]);
    }

    #[test]
    fn a_rule_can_be_scoped_to_an_action_that_is_not_one_of_the_seven() {
        // A controller that is not a RESTful resource still has actions, and
        // they still need guarding. Naming them as strings is the whole reason
        // scoping is not expressed as the enum.
        let middleware = ControllerMiddleware::new()
            .only(["search", "export"], Tag("throttle"))
            .always(Tag("headers"));

        assert_eq!(middleware.for_action("search").len(), 2);
        assert_eq!(middleware.for_action("export").len(), 2);
        assert_eq!(middleware.for_action("show").len(), 1);
    }

    #[test]
    fn except_on_a_named_action_covers_one_the_framework_never_saw() {
        // The case that decided how `Scope` is stored. `except` used to expand
        // to "all seven minus these", which answers nothing for an action that
        // is not one of the seven — a controller action named here would have
        // come back unguarded, which is the exact failure `except` exists to
        // prevent.
        let middleware = ControllerMiddleware::new().except(["public_read"], Tag("guard"));

        assert!(middleware.for_action("public_read").is_empty());
        assert_eq!(middleware.for_action("destroy_everything").len(), 1);
    }

    #[test]
    fn the_two_ways_of_naming_an_action_agree() {
        // `ResourceAction::Index` and `"index"` are the same action, so a rule
        // written with one must bind a lookup written with the other.
        let middleware = ControllerMiddleware::new().only([ResourceAction::Index], Tag("guard"));

        assert_eq!(middleware.for_action("index").len(), 1);

        let by_name = ControllerMiddleware::new().only(["index"], Tag("guard"));
        assert_eq!(by_name.for_action(ResourceAction::Index).len(), 1);
    }

    #[test]
    fn except_covers_an_action_added_later() {
        // The reason `except` is the safer of the two: the guard applies to
        // everything that is not explicitly opted out, so a new action arrives
        // protected.
        let middleware = ControllerMiddleware::new().except([ResourceAction::Index], Tag("guard"));

        for action in ResourceAction::ALL {
            let expected = usize::from(action != ResourceAction::Index);
            assert_eq!(middleware.for_action(action).len(), expected, "{action:?}");
        }
    }

    #[test]
    fn a_controller_declares_no_middleware_by_default() {
        assert!(ControllerMiddleware::new().is_empty());
        assert!(Posts.middleware().is_empty());
    }

    #[tokio::test]
    async fn a_group_wraps_a_controllers_own_middleware() {
        // Order matters: the group is outside, so it sees the response the
        // controller's middleware produced.
        use crate::router::GroupAttributes;

        let mut router = Router::new();
        router.group(GroupAttributes::new().middleware(Tag("group")), |router| {
            router.resource("posts", Arc::new(Guarded));
        });

        let response = compiled(router).dispatch(request(Method::GET, "/posts")).await;
        assert_eq!(tags(&response), vec!["every", "group"]);
    }

    #[test]
    fn registers_all_seven_routes_with_conventional_names() {
        let router = posts_router();
        assert_eq!(router.len(), 7);

        assert_eq!(router.uri_for("posts.index"), Some("/posts"));
        assert_eq!(router.uri_for("posts.create"), Some("/posts/create"));
        assert_eq!(router.uri_for("posts.store"), Some("/posts"));
        assert_eq!(router.uri_for("posts.show"), Some("/posts/{post}"));
        assert_eq!(router.uri_for("posts.edit"), Some("/posts/{post}/edit"));
        assert_eq!(router.uri_for("posts.update"), Some("/posts/{post}"));
        assert_eq!(router.uri_for("posts.destroy"), Some("/posts/{post}"));
    }

    #[test]
    fn the_parameter_is_the_singular_of_the_resource() {
        let mut router = Router::new();
        router.resource("categories", Arc::new(Posts));
        assert_eq!(router.uri_for("categories.show"), Some("/categories/{category}"));
    }

    #[tokio::test]
    async fn create_is_declared_before_the_parameter_route() {
        // Otherwise `/posts/create` would match `show` with post = "create".
        let compiled = compiled(posts_router());
        let response = compiled.dispatch(request(Method::GET, "/posts/create")).await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "should reach the unimplemented `create`, not `show`"
        );
    }

    #[tokio::test]
    async fn dispatches_each_action() {
        let compiled = compiled(posts_router());

        assert_eq!(body_of(compiled.dispatch(request(Method::GET, "/posts")).await).await, "index");
        assert_eq!(
            body_of(compiled.dispatch(request(Method::POST, "/posts")).await).await,
            "store"
        );
        assert_eq!(
            body_of(compiled.dispatch(request(Method::GET, "/posts/7")).await).await,
            "show:7"
        );
        assert_eq!(
            body_of(compiled.dispatch(request(Method::DELETE, "/posts/7")).await).await,
            "destroy"
        );
    }

    #[tokio::test]
    async fn update_answers_both_put_and_patch() {
        let compiled = compiled(posts_router());
        for method in [Method::PUT, Method::PATCH] {
            let response = compiled.dispatch(request(method.clone(), "/posts/7")).await;
            assert_eq!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} should reach the unimplemented `update` action"
            );
        }
    }

    #[tokio::test]
    async fn an_unimplemented_action_is_a_405_naming_itself() {
        let compiled = compiled(posts_router());
        let response = compiled.dispatch(request(Method::GET, "/posts/7/edit")).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(body_of(response).await.contains("edit"));
    }

    #[test]
    fn an_api_resource_omits_the_form_pages() {
        let mut router = Router::new();
        router.api_resource("posts", Arc::new(Posts));

        assert_eq!(router.len(), 5);
        assert_eq!(router.uri_for("posts.create"), None);
        assert_eq!(router.uri_for("posts.edit"), None);
        assert_eq!(router.uri_for("posts.index"), Some("/posts"));
    }

    #[test]
    fn only_the_listed_actions_are_registered() {
        let mut router = Router::new();
        router.resource_actions(
            "posts",
            Arc::new(Posts),
            &[ResourceAction::Show, ResourceAction::Index],
        );

        assert_eq!(router.len(), 2);
        assert_eq!(router.uri_for("posts.store"), None);
        // Registration order is canonical, not the order given.
        assert_eq!(router.routes()[0].route_name(), Some("posts.index"));
    }

    #[test]
    fn nested_resource_names_use_dots() {
        let mut router = Router::new();
        router.resource("admin/posts", Arc::new(Posts));
        assert_eq!(router.uri_for("admin.posts.index"), Some("/admin/posts"));
        assert_eq!(router.uri_for("admin.posts.show"), Some("/admin/posts/{post}"));
    }
}
