//! A single [`Route`]: its URI pattern, methods, middleware, name and
//! parameter constraints.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use percent_encoding::percent_decode_str;
use rainier_http::Method;
use rainier_middleware::{IntoMiddlewareStack, Middleware, MiddlewareStack};

use crate::handler::RouteHandler;

/// One compiled piece of a URI pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// A literal, matched exactly.
    Static(String),
    /// `{name}` — one segment, captured.
    Param(String),
    /// `{name?}` — one optional segment, captured. Only meaningful trailing.
    OptionalParam(String),
    /// `{name*}` — the rest of the path, captured with its slashes.
    Wildcard(String),
}

/// A restriction on what a route parameter may contain — a closed
/// set of named kinds rather than a regular expression.
///
/// Named kinds instead of regexes keeps the crate free of a regex dependency
/// and, more usefully, makes the common constraints declarative and
/// impossible to get subtly wrong. [`ParamConstraint::Custom`] covers the rest.
#[derive(Clone)]
pub enum ParamConstraint {
    /// ASCII digits only.
    Number,
    /// ASCII letters only.
    Alpha,
    /// ASCII letters and digits.
    AlphaNumeric,
    /// Letters, digits, `-` and `_` — a URL slug.
    Slug,
    /// A canonical 8-4-4-4-12 hexadecimal UUID.
    Uuid,
    /// One of a fixed set of values.
    In(Vec<String>),
    /// Any predicate.
    Custom(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl ParamConstraint {
    /// Whether `value` satisfies this constraint.
    pub fn allows(&self, value: &str) -> bool {
        match self {
            ParamConstraint::Number => {
                !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
            }
            ParamConstraint::Alpha => {
                !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphabetic())
            }
            ParamConstraint::AlphaNumeric => {
                !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric())
            }
            ParamConstraint::Slug => {
                !value.is_empty()
                    && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            }
            ParamConstraint::Uuid => is_uuid(value),
            ParamConstraint::In(allowed) => allowed.iter().any(|a| a == value),
            ParamConstraint::Custom(predicate) => predicate(value),
        }
    }
}

impl std::fmt::Debug for ParamConstraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamConstraint::Number => f.write_str("Number"),
            ParamConstraint::Alpha => f.write_str("Alpha"),
            ParamConstraint::AlphaNumeric => f.write_str("AlphaNumeric"),
            ParamConstraint::Slug => f.write_str("Slug"),
            ParamConstraint::Uuid => f.write_str("Uuid"),
            ParamConstraint::In(values) => write!(f, "In({values:?})"),
            ParamConstraint::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

fn is_uuid(value: &str) -> bool {
    let groups = [8, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for expected in groups {
        let Some(part) = parts.next() else { return false };
        if part.len() != expected || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

/// A declared route.
#[derive(Clone)]
pub struct Route {
    methods: Vec<Method>,
    uri: String,
    segments: Vec<Segment>,
    handler: Arc<dyn RouteHandler>,
    middleware: MiddlewareStack,
    excluded_middleware: Vec<TypeId>,
    name: Option<String>,
    constraints: HashMap<String, ParamConstraint>,
}

impl Route {
    /// A route for `methods` at `uri`, served by `handler`.
    ///
    /// `HEAD` is added alongside `GET` automatically, as every HTTP server is
    /// expected to answer a `HEAD` wherever it answers a `GET`.
    pub fn new(
        methods: Vec<Method>,
        uri: impl Into<String>,
        handler: Arc<dyn RouteHandler>,
    ) -> Self {
        let uri = normalise_uri(&uri.into());
        let segments = compile(&uri);

        let mut methods = methods;
        if methods.contains(&Method::GET) && !methods.contains(&Method::HEAD) {
            methods.push(Method::HEAD);
        }

        Self {
            methods,
            uri,
            segments,
            handler,
            middleware: MiddlewareStack::new(),
            excluded_middleware: Vec::new(),
            name: None,
            constraints: HashMap::new(),
        }
    }

    // --- declaration -------------------------------------------------------

    // Builders take `&mut self` and return `&mut Self` rather than consuming
    // `self`, because a route is declared *into* the router: `router.get(..)`
    // hands back a borrow of the route it just stored, and the chained calls
    // configure it in place.

    /// Name the route, so it can be resolved into a URL.
    pub fn name(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    /// Attach middleware.
    ///
    /// Takes the middleware **itself** — one instance, a tuple of them, or a
    /// whole [`MiddlewareStack`] from a group function. There is no name to
    /// misspell and no registry to look anything up in.
    ///
    /// ```ignore
    /// router.post("/login", login)
    ///     .middleware(ThrottleRequests::per_minute(10));
    ///
    /// router.get("/admin", dashboard)
    ///     .middleware((Authenticate::new(auth), RequireRole::Admin));
    ///
    /// router.get("/", home).middleware(kernel::web());
    /// ```
    pub fn middleware(&mut self, middleware: impl IntoMiddlewareStack) -> &mut Self {
        self.middleware =
            std::mem::take(&mut self.middleware).with_stack(middleware.into_middleware_stack());
        self
    }

    /// Skip a middleware this route would otherwise inherit from its group.
    ///
    /// By type, because that is the only identity a value has:
    ///
    /// ```ignore
    /// router.post("/webhooks/stripe", stripe)
    ///     .without_middleware::<VerifyCsrfToken>();
    /// ```
    ///
    /// Matches the **concrete type**, so excluding `ThrottleRequests` removes
    /// every rate limiter the group applied, not one particular configuration
    /// of it. Where that is too blunt, do not put it in the group.
    pub fn without_middleware<M: Middleware>(&mut self) -> &mut Self {
        self.excluded_middleware.push(TypeId::of::<M>());
        self
    }

    /// Constrain a route parameter.
    pub fn where_param(
        &mut self,
        param: impl Into<String>,
        constraint: ParamConstraint,
    ) -> &mut Self {
        self.constraints.insert(param.into(), constraint);
        self
    }

    /// Constrain a parameter to digits.
    pub fn where_number(&mut self, param: impl Into<String>) -> &mut Self {
        self.where_param(param, ParamConstraint::Number)
    }

    /// Constrain a parameter to a slug.
    pub fn where_slug(&mut self, param: impl Into<String>) -> &mut Self {
        self.where_param(param, ParamConstraint::Slug)
    }

    /// Constrain a parameter to a UUID.
    pub fn where_uuid(&mut self, param: impl Into<String>) -> &mut Self {
        self.where_param(param, ParamConstraint::Uuid)
    }

    /// Constrain a parameter to a fixed set of values.
    pub fn where_in(
        &mut self,
        param: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<String>>,
    ) -> &mut Self {
        self.where_param(param, ParamConstraint::In(values.into_iter().map(Into::into).collect()))
    }

    // --- accessors ---------------------------------------------------------

    /// The methods this route answers.
    pub fn methods(&self) -> &[Method] {
        &self.methods
    }

    /// The URI pattern, normalised to start with `/`.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// The compiled segments.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The route's name, if it has one.
    pub fn route_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The middleware attached to this route, outermost first.
    pub fn middleware_stack(&self) -> &MiddlewareStack {
        &self.middleware
    }

    /// The types this route opts out of.
    pub fn excluded_middleware(&self) -> &[TypeId] {
        &self.excluded_middleware
    }

    /// The handler.
    pub fn handler(&self) -> &Arc<dyn RouteHandler> {
        &self.handler
    }

    /// The parameter names this route captures, in order.
    pub fn param_names(&self) -> Vec<&str> {
        self.segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Static(_) => None,
                Segment::Param(name) | Segment::OptionalParam(name) | Segment::Wildcard(name) => {
                    Some(name.as_str())
                }
            })
            .collect()
    }

    // --- internal, used by the router --------------------------------------

    pub(crate) fn prefix_with(&mut self, prefix: &str) {
        if prefix.is_empty() {
            return;
        }
        let joined = format!("/{}/{}", prefix.trim_matches('/'), self.uri.trim_start_matches('/'));
        self.uri = normalise_uri(&joined);
        self.segments = compile(&self.uri);
    }

    pub(crate) fn prepend_middleware(&mut self, outer: &MiddlewareStack) {
        // Group middleware runs outside the route's own, so it goes first.
        self.middleware.prepend(outer);
    }

    pub(crate) fn prepend_name(&mut self, prefix: &str) {
        if prefix.is_empty() {
            return;
        }
        // A group name prefix alone does not name an unnamed route — `admin.`
        // is a prefix, not a route name — so `None` stays `None`.
        self.name = self.name.take().map(|name| format!("{prefix}{name}"));
    }

    pub(crate) fn add_constraints(&mut self, constraints: &HashMap<String, ParamConstraint>) {
        for (param, constraint) in constraints {
            // A route's own `where` wins over the group's.
            self.constraints.entry(param.clone()).or_insert_with(|| constraint.clone());
        }
    }

    /// Match `path` against this route's pattern, returning the captured
    /// parameters. `None` if the shape differs or a constraint rejects a value.
    pub fn match_path(&self, path: &str) -> Option<HashMap<String, String>> {
        let actual: Vec<&str> = split_path(path);
        let mut params = HashMap::new();
        let mut index = 0;

        for (position, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Static(literal) => {
                    if actual.get(index) != Some(&literal.as_str()) {
                        return None;
                    }
                    index += 1;
                }
                Segment::Param(name) => {
                    let value = decode(actual.get(index)?);
                    if !self.allows(name, &value) {
                        return None;
                    }
                    params.insert(name.clone(), value);
                    index += 1;
                }
                Segment::OptionalParam(name) => {
                    let Some(raw) = actual.get(index) else {
                        // Absent: legal only if nothing after it is required.
                        return self.rest_is_optional(position + 1).then_some(params);
                    };
                    let value = decode(raw);
                    if !self.allows(name, &value) {
                        return None;
                    }
                    params.insert(name.clone(), value);
                    index += 1;
                }
                Segment::Wildcard(name) => {
                    // Swallows the remainder, slashes included.
                    let rest = actual[index.min(actual.len())..].join("/");
                    let value = decode(&rest);
                    if !self.allows(name, &value) {
                        return None;
                    }
                    params.insert(name.clone(), value);
                    index = actual.len();
                }
            }
        }

        (index == actual.len()).then_some(params)
    }

    fn rest_is_optional(&self, from: usize) -> bool {
        self.segments[from..]
            .iter()
            .all(|s| matches!(s, Segment::OptionalParam(_) | Segment::Wildcard(_)))
    }

    fn allows(&self, param: &str, value: &str) -> bool {
        match self.constraints.get(param) {
            Some(constraint) => constraint.allows(value),
            None => true,
        }
    }

    /// Whether `path` matches, ignoring the method — used to answer `405`
    /// instead of `404` when a route exists but not for this method.
    pub fn matches_path_only(&self, path: &str) -> bool {
        self.match_path(path).is_some()
    }

    /// Whether this route answers `method`.
    pub fn accepts(&self, method: &Method) -> bool {
        self.methods.iter().any(|m| m == method)
    }
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("methods", &self.methods.iter().map(|m| m.as_str()).collect::<Vec<_>>())
            .field("uri", &self.uri)
            .field("name", &self.name)
            .field("middleware", &self.middleware)
            .finish()
    }
}

/// `posts/{post}` → `/posts/{post}`; `//a//b/` → `/a/b`. The root is `/`.
pub fn normalise_uri(uri: &str) -> String {
    let trimmed: Vec<&str> = split_path(uri);
    if trimmed.is_empty() {
        return "/".to_string();
    }
    format!("/{}", trimmed.join("/"))
}

/// Split a path into non-empty segments, so `/a//b/` and `a/b` agree.
fn split_path(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn decode(raw: &str) -> String {
    percent_decode_str(raw).decode_utf8_lossy().into_owned()
}

/// Compile a URI pattern into segments.
pub fn compile(uri: &str) -> Vec<Segment> {
    split_path(uri)
        .into_iter()
        .map(|piece| {
            let Some(inner) = piece.strip_prefix('{').and_then(|p| p.strip_suffix('}')) else {
                return Segment::Static(piece.to_string());
            };
            if let Some(name) = inner.strip_suffix('?') {
                Segment::OptionalParam(name.to_string())
            } else if let Some(name) = inner.strip_suffix('*') {
                Segment::Wildcard(name.to_string())
            } else {
                Segment::Param(inner.to_string())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_http::{Request, Response};
    use rainier_support::BoxedFuture;

    struct Noop;
    impl RouteHandler for Noop {
        fn call(&self, _: Request) -> BoxedFuture<Response> {
            Box::pin(async { Response::no_content() })
        }
    }

    fn route(uri: &str) -> Route {
        Route::new(vec![Method::GET], uri, Arc::new(Noop))
    }

    fn matched(route: &Route, path: &str) -> Option<Vec<(String, String)>> {
        route.match_path(path).map(|params| {
            let mut pairs: Vec<_> = params.into_iter().collect();
            pairs.sort();
            pairs
        })
    }

    #[test]
    fn normalises_uris() {
        assert_eq!(normalise_uri("posts"), "/posts");
        assert_eq!(normalise_uri("/posts/"), "/posts");
        assert_eq!(normalise_uri("//a//b/"), "/a/b");
        assert_eq!(normalise_uri(""), "/");
        assert_eq!(normalise_uri("/"), "/");
    }

    #[test]
    fn compiles_the_segment_kinds() {
        assert_eq!(
            compile("/posts/{post}/comments/{comment?}/{rest*}"),
            vec![
                Segment::Static("posts".into()),
                Segment::Param("post".into()),
                Segment::Static("comments".into()),
                Segment::OptionalParam("comment".into()),
                Segment::Wildcard("rest".into()),
            ]
        );
    }

    #[test]
    fn matches_static_paths() {
        let route = route("/posts");
        assert_eq!(matched(&route, "/posts"), Some(vec![]));
        assert_eq!(matched(&route, "/posts/"), Some(vec![]));
        assert_eq!(matched(&route, "/other"), None);
        assert_eq!(matched(&route, "/posts/1"), None);
    }

    #[test]
    fn matches_the_root() {
        let route = route("/");
        assert_eq!(matched(&route, "/"), Some(vec![]));
        assert_eq!(matched(&route, "/a"), None);
    }

    #[test]
    fn captures_parameters() {
        let route = route("/posts/{post}/comments/{comment}");
        assert_eq!(
            matched(&route, "/posts/7/comments/3"),
            Some(vec![("comment".into(), "3".into()), ("post".into(), "7".into())])
        );
        assert_eq!(matched(&route, "/posts/7/comments"), None);
    }

    #[test]
    fn percent_decodes_captured_values() {
        let route = route("/tags/{tag}");
        assert_eq!(
            matched(&route, "/tags/hello%20world"),
            Some(vec![("tag".into(), "hello world".into())])
        );
    }

    #[test]
    fn optional_parameters_may_be_absent_when_trailing() {
        let route = route("/posts/{post?}");
        assert_eq!(matched(&route, "/posts"), Some(vec![]));
        assert_eq!(matched(&route, "/posts/7"), Some(vec![("post".into(), "7".into())]));
    }

    #[test]
    fn an_optional_parameter_before_a_required_one_cannot_be_skipped() {
        let route = route("/a/{maybe?}/b");
        assert_eq!(matched(&route, "/a/b"), None, "`b` is required, so `maybe` cannot vanish");
        assert_eq!(matched(&route, "/a/x/b"), Some(vec![("maybe".into(), "x".into())]));
    }

    #[test]
    fn a_wildcard_swallows_the_rest_including_slashes() {
        let route = route("/files/{path*}");
        assert_eq!(
            matched(&route, "/files/a/b/c.txt"),
            Some(vec![("path".into(), "a/b/c.txt".into())])
        );
        assert_eq!(matched(&route, "/files"), Some(vec![("path".into(), "".into())]));
    }

    #[test]
    fn constraints_reject_non_matching_values() {
        let mut route = route("/posts/{post}");
        route.where_number("post");
        assert!(route.match_path("/posts/7").is_some());
        assert!(route.match_path("/posts/create").is_none());
    }

    #[test]
    fn every_constraint_kind() {
        assert!(ParamConstraint::Number.allows("123"));
        assert!(!ParamConstraint::Number.allows("12a"));
        assert!(!ParamConstraint::Number.allows(""));

        assert!(ParamConstraint::Alpha.allows("abc"));
        assert!(!ParamConstraint::Alpha.allows("a1"));

        assert!(ParamConstraint::AlphaNumeric.allows("a1"));
        assert!(!ParamConstraint::AlphaNumeric.allows("a-1"));

        assert!(ParamConstraint::Slug.allows("hello-world_1"));
        assert!(!ParamConstraint::Slug.allows("hello world"));

        assert!(ParamConstraint::Uuid.allows("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!ParamConstraint::Uuid.allows("123e4567-e89b-12d3-a456"));
        assert!(!ParamConstraint::Uuid.allows("123e4567xe89b-12d3-a456-426614174000"));

        let one_of = ParamConstraint::In(vec!["draft".into(), "live".into()]);
        assert!(one_of.allows("draft"));
        assert!(!one_of.allows("other"));

        let custom = ParamConstraint::Custom(Arc::new(|v: &str| v.starts_with("x")));
        assert!(custom.allows("xyz"));
        assert!(!custom.allows("abc"));
    }

    #[test]
    fn get_routes_also_answer_head() {
        let route = route("/posts");
        assert!(route.accepts(&Method::GET));
        assert!(route.accepts(&Method::HEAD));
        assert!(!route.accepts(&Method::POST));
    }

    #[test]
    fn head_is_not_added_to_non_get_routes() {
        let route = Route::new(vec![Method::POST], "/posts", Arc::new(Noop));
        assert!(!route.accepts(&Method::HEAD));
    }

    #[test]
    fn prefixing_rewrites_the_uri_and_recompiles() {
        let mut route = route("/users/{user}");
        route.prefix_with("admin");
        assert_eq!(route.uri(), "/admin/users/{user}");
        assert!(route.match_path("/admin/users/1").is_some());
        assert!(route.match_path("/users/1").is_none());
    }

    struct Own;
    struct Group;

    macro_rules! passthrough {
        ($ty:ty) => {
            #[async_trait::async_trait]
            impl rainier_middleware::Middleware for $ty {
                async fn handle(
                    &self,
                    request: rainier_http::Request,
                    next: rainier_middleware::Next,
                ) -> rainier_http::Response {
                    next.run(request).await
                }
                fn name(&self) -> &'static str {
                    stringify!($ty)
                }
            }
        };
    }

    passthrough!(Own);
    passthrough!(Group);

    #[test]
    fn group_middleware_runs_outside_route_middleware() {
        let mut route = route("/x");
        route.middleware(Own);
        route.prepend_middleware(&MiddlewareStack::new().with(Group));

        assert_eq!(route.middleware_stack().labels(), vec!["Group", "Own"]);
    }

    #[test]
    fn a_route_opts_out_by_type_not_by_name() {
        let mut route = route("/x");
        route.middleware((Group, Own)).without_middleware::<Group>();

        assert_eq!(route.excluded_middleware(), &[TypeId::of::<Group>()]);
        assert_eq!(
            route.middleware_stack().labels(),
            vec!["Group", "Own"],
            "exclusion is applied when the pipeline is built, not when it is declared"
        );
    }

    #[test]
    fn a_name_prefix_only_applies_to_named_routes() {
        let mut named = route("/x");
        named.name("index");
        named.prepend_name("admin.");
        assert_eq!(named.route_name(), Some("admin.index"));

        let mut unnamed = route("/y");
        unnamed.prepend_name("admin.");
        assert_eq!(unnamed.route_name(), None);
    }

    #[test]
    fn a_route_constraint_beats_the_groups() {
        let mut route = route("/{id}");
        route.where_param("id", ParamConstraint::Alpha);
        let mut group = HashMap::new();
        group.insert("id".to_string(), ParamConstraint::Number);
        route.add_constraints(&group);

        assert!(route.match_path("/abc").is_some(), "the route's own Alpha should win");
        assert!(route.match_path("/123").is_none());
    }

    #[test]
    fn lists_its_parameter_names() {
        let route = route("/posts/{post}/tags/{tag?}/{rest*}");
        assert_eq!(route.param_names(), vec!["post", "tag", "rest"]);
    }
}
