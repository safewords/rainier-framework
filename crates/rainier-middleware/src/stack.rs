//! Middleware as values — [`MiddlewareStack`] and [`IntoMiddlewareStack`].
//!
//! In a name-keyed kernel a route names its middleware — `middleware("auth")`
//! — resolved through an alias map at runtime. That map is the only thing
//! standing between a typo and a route that runs unguarded, and a dynamic
//! language has no better option.
//!
//! Rust does. A route takes the **middleware itself**:
//!
//! ```
//! # use rainier_middleware::{AddHeaders, MiddlewareStack, ThrottleRequests};
//! let stack = MiddlewareStack::from(AddHeaders::security_defaults())
//!     .with(ThrottleRequests::per_minute(60));
//!
//! assert_eq!(stack.len(), 2);
//! ```
//!
//! There is nothing to misspell, nothing to register, and nothing to look up.
//! Deleting a middleware breaks every route that used it, in the compiler,
//! naming each one.
//!
//! ## Groups are functions, not names
//!
//! Elsewhere, `web` and `api` groups are keys in a map. Here a group is a
//! function that returns a stack — so it is discoverable by "go to definition",
//! it can take arguments, and calling one that does not exist is a compile
//! error:
//!
//! ```
//! # use rainier_middleware::{AddHeaders, HandleCors, MiddlewareStack, ThrottleRequests};
//! pub fn api(per_minute: u32) -> MiddlewareStack {
//!     MiddlewareStack::new()
//!         .with(HandleCors::any_origin())
//!         .with(ThrottleRequests::per_minute(per_minute))
//! }
//!
//! pub fn web() -> MiddlewareStack {
//!     MiddlewareStack::new().with(AddHeaders::security_defaults())
//! }
//!
//! assert_eq!(api(60).len(), 2);
//! ```
//!
//! Nesting one group inside another is [`with_stack`](MiddlewareStack::with_stack),
//! and a cycle is not expressible — a function cannot call itself into a value
//! without recursing forever, which is a stack overflow the type system already
//! prevents you from writing by accident, rather than a runtime check the old
//! registry had to perform.
//!
//! ## When the middleware needs the container
//!
//! Some middleware cannot be built while routes are being declared: an
//! authentication guard needs the `AuthManager` the application binds, and the
//! container is not populated yet. That is the one case the old
//! name-and-factory indirection genuinely solved.
//!
//! [`resolved`](MiddlewareStack::resolved) solves it without a name. The
//! closure runs when the router **compiles**, which is after the container is
//! populated and still before the first request:
//!
//! ```ignore
//! MiddlewareStack::new()
//!     .resolved(|auth: Arc<AuthManager<User>>| Authenticate::new(auth))
//! ```
//!
//! The container arrives as an argument rather than through a global. That is
//! the difference between a builder you can call in a test and one that panics
//! unless a process-wide slot happens to be filled — and it is the same reason
//! the names went: a value handed to you is checkable, a value looked up is
//! not.
//!
//! A failure fails the boot, naming the middleware's type and the route that
//! wanted it.

use std::sync::Arc;

use rainier_container::Container;
use rainier_support::{Error, Result};

use crate::pipeline::Middleware;

/// Builds one middleware from the container, when the router compiles.
type Builder = Arc<dyn Fn(&Container) -> Result<Arc<dyn Middleware>> + Send + Sync>;

/// One entry in a stack: an instance, or a closure that will build one.
enum Stage {
    Ready(Arc<dyn Middleware>),
    /// The `&'static str` is the middleware's type name, captured at
    /// declaration so a failure can say what failed to build.
    Deferred(&'static str, Builder),
}

impl Clone for Stage {
    fn clone(&self) -> Self {
        match self {
            Stage::Ready(middleware) => Stage::Ready(Arc::clone(middleware)),
            Stage::Deferred(name, build) => Stage::Deferred(name, Arc::clone(build)),
        }
    }
}

/// An ordered list of middleware, outermost first.
///
/// What a route, a group, or a controller action attaches. The value that
/// replaces a name-keyed group entry.
#[derive(Default, Clone)]
pub struct MiddlewareStack {
    stages: Vec<Stage>,
}

impl MiddlewareStack {
    /// An empty stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a middleware instance.
    pub fn with(mut self, middleware: impl Middleware) -> Self {
        self.stages.push(Stage::Ready(Arc::new(middleware)));
        self
    }

    /// Append an already-shared instance.
    ///
    /// For middleware built once and attached to several routes — a rate
    /// limiter, whose counters are the point of sharing it.
    pub fn with_arc(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.stages.push(Stage::Ready(middleware));
        self
    }

    /// Append every stage of `other`, keeping its order.
    ///
    /// How a group nests inside a group.
    pub fn with_stack(mut self, other: MiddlewareStack) -> Self {
        self.stages.extend(other.stages);
        self
    }

    /// Append middleware built when the router compiles, from the container.
    ///
    /// For anything that cannot exist while routes are being declared: the
    /// container is populated by then, and the first request has not arrived.
    /// See [the module docs](self).
    pub fn deferred<M, F>(mut self, build: F) -> Self
    where
        M: Middleware,
        F: Fn(&Container) -> Result<M> + Send + Sync + 'static,
    {
        let name = crate::pipeline::short_type_name(std::any::type_name::<M>());
        self.stages.push(Stage::Deferred(
            name,
            Arc::new(move |container| build(container).map(|m| Arc::new(m) as Arc<dyn Middleware>)),
        ));
        self
    }

    /// Append middleware built from one service in the container.
    ///
    /// The common shape of [`deferred`](Self::deferred), with the resolve
    /// spelled once:
    ///
    /// ```ignore
    /// MiddlewareStack::new()
    ///     .resolved(|auth: Arc<AuthManager<User>>| Authenticate::new(auth))
    /// ```
    ///
    /// A missing binding fails the boot with the service's type name in the
    /// message, which is the container's own error.
    pub fn resolved<S, M, F>(self, build: F) -> Self
    where
        S: Send + Sync + 'static,
        M: Middleware,
        F: Fn(Arc<S>) -> M + Send + Sync + 'static,
    {
        self.deferred(move |container| Ok(build(container.resolve::<S>()?)))
    }

    /// Prepend every stage of `outer`, so it runs first.
    ///
    /// How a group's middleware ends up outside a route's own.
    pub fn prepend(&mut self, outer: &MiddlewareStack) {
        let mut combined = outer.stages.clone();
        combined.append(&mut self.stages);
        self.stages = combined;
    }

    /// How many stages are declared.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// Whether nothing is declared.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Build every stage, running the deferred closures against `container`.
    ///
    /// Called once, when the router compiles.
    pub fn resolve(&self, container: &Container) -> Result<Vec<Arc<dyn Middleware>>> {
        self.stages
            .iter()
            .map(|stage| match stage {
                Stage::Ready(middleware) => Ok(Arc::clone(middleware)),
                Stage::Deferred(name, build) => build(container).map_err(|e| {
                    Error::internal(format!("could not build the `{name}` middleware: {e}"))
                }),
            })
            .collect()
    }

    /// The label of each stage, for diagnostics before resolution.
    ///
    /// A deferred stage reports its declared type, which is the best that can
    /// be said before it is built.
    pub fn labels(&self) -> Vec<&'static str> {
        self.stages
            .iter()
            .map(|stage| match stage {
                Stage::Ready(middleware) => middleware.name(),
                Stage::Deferred(name, _) => name,
            })
            .collect()
    }
}

impl std::fmt::Debug for MiddlewareStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.labels()).finish()
    }
}

/// Anything that can be attached where middleware is expected.
///
/// Implemented for a single middleware, an `Arc<dyn Middleware>`, a
/// [`MiddlewareStack`], and tuples of up to eight — so all of these read the
/// same way:
///
/// ```
/// # use rainier_middleware::{AddHeaders, HandleCors, IntoMiddlewareStack, ThrottleRequests};
/// let one = AddHeaders::security_defaults().into_middleware_stack();
/// let several = (HandleCors::any_origin(), ThrottleRequests::per_minute(60))
///     .into_middleware_stack();
///
/// assert_eq!(one.len(), 1);
/// assert_eq!(several.len(), 2);
/// ```
pub trait IntoMiddlewareStack {
    /// Convert into a stack.
    fn into_middleware_stack(self) -> MiddlewareStack;
}

impl<M: Middleware> IntoMiddlewareStack for M {
    fn into_middleware_stack(self) -> MiddlewareStack {
        MiddlewareStack::new().with(self)
    }
}

impl IntoMiddlewareStack for MiddlewareStack {
    fn into_middleware_stack(self) -> MiddlewareStack {
        self
    }
}

impl IntoMiddlewareStack for Arc<dyn Middleware> {
    fn into_middleware_stack(self) -> MiddlewareStack {
        MiddlewareStack::new().with_arc(self)
    }
}

impl IntoMiddlewareStack for Vec<Arc<dyn Middleware>> {
    fn into_middleware_stack(self) -> MiddlewareStack {
        self.into_iter().fold(MiddlewareStack::new(), MiddlewareStack::with_arc)
    }
}

impl<M: Middleware> From<M> for MiddlewareStack {
    fn from(middleware: M) -> Self {
        MiddlewareStack::new().with(middleware)
    }
}

/// `impl IntoMiddlewareStack` for tuples, so several stages attach in one call
/// and keep their order.
macro_rules! tuple_stacks {
    ($( ($($name:ident),+) ),+ $(,)?) => {
        $(
            #[allow(non_snake_case, reason = "the idiomatic spelling for a tuple impl")]
            impl<$($name: Middleware),+> IntoMiddlewareStack for ($($name,)+) {
                fn into_middleware_stack(self) -> MiddlewareStack {
                    let ($($name,)+) = self;
                    MiddlewareStack::new()$(.with($name))+
                }
            }
        )+
    };
}

tuple_stacks! {
    (A, B),
    (A, B, C),
    (A, B, C, D),
    (A, B, C, D, E),
    (A, B, C, D, E, F),
    (A, B, C, D, E, F, G),
    (A, B, C, D, E, F, G, H),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Next;
    use rainier_http::{Request, Response};

    macro_rules! tag {
        ($name:ident) => {
            struct $name;

            #[async_trait::async_trait]
            impl Middleware for $name {
                async fn handle(&self, request: Request, next: Next) -> Response {
                    next.run(request).await
                }
                fn name(&self) -> &'static str {
                    stringify!($name)
                }
            }
        };
    }

    tag!(First);
    tag!(Second);
    tag!(Third);

    fn resolved_names(stack: &MiddlewareStack) -> Vec<&'static str> {
        stack.resolve(&Container::new()).unwrap().iter().map(|m| m.name()).collect()
    }

    #[test]
    fn a_stack_keeps_the_order_it_was_built_in() {
        let stack = MiddlewareStack::new().with(First).with(Second).with(Third);
        assert_eq!(resolved_names(&stack), vec!["First", "Second", "Third"]);
    }

    #[test]
    fn one_middleware_converts_to_a_stack_of_one() {
        assert_eq!(resolved_names(&First.into_middleware_stack()), vec!["First"]);
    }

    #[test]
    fn a_tuple_converts_in_order() {
        let stack = (First, Second, Third).into_middleware_stack();
        assert_eq!(resolved_names(&stack), vec!["First", "Second", "Third"]);
    }

    #[test]
    fn a_stack_converts_to_itself() {
        let stack = MiddlewareStack::new().with(First);
        assert_eq!(resolved_names(&stack.into_middleware_stack()), vec!["First"]);
    }

    #[test]
    fn nesting_a_stack_flattens_it_in_place() {
        // What a group inside a group has to do, and the reason the old
        // registry needed a cycle check that this cannot want.
        let inner = MiddlewareStack::new().with(Second).with(Third);
        let outer = MiddlewareStack::new().with(First).with_stack(inner);

        assert_eq!(resolved_names(&outer), vec!["First", "Second", "Third"]);
    }

    #[test]
    fn prepending_puts_the_group_outside_the_routes_own() {
        let mut route = MiddlewareStack::new().with(Third);
        route.prepend(&MiddlewareStack::new().with(First).with(Second));

        assert_eq!(resolved_names(&route), vec!["First", "Second", "Third"]);
    }

    #[test]
    fn a_deferred_stage_is_built_at_resolution_not_declaration() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static BUILT: AtomicUsize = AtomicUsize::new(0);

        let stack = MiddlewareStack::new().deferred(|_| {
            BUILT.fetch_add(1, Ordering::SeqCst);
            Ok(First)
        });

        assert_eq!(BUILT.load(Ordering::SeqCst), 0, "declaring must not build");

        assert_eq!(resolved_names(&stack), vec!["First"]);
        assert_eq!(BUILT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_deferred_stage_that_fails_names_its_type() {
        // The container was missing a binding. The message has to say which
        // middleware wanted it, or the boot failure is a mystery.
        let stack = MiddlewareStack::new()
            .deferred(|_| Err::<First, _>(Error::internal("AuthManager is not bound")));

        let err = stack.resolve(&Container::new()).err().expect("resolution should fail");
        assert!(err.message().contains("`First`"), "{}", err.message());
        assert!(err.message().contains("AuthManager is not bound"), "{}", err.message());
    }

    #[test]
    fn resolved_builds_from_a_binding() {
        struct Guard(&'static str);

        let container = Container::new();
        container.instance(Guard("bound"));

        let stack = MiddlewareStack::new().resolved(|guard: Arc<Guard>| {
            assert_eq!(guard.0, "bound");
            First
        });

        // Against *this* container — the binding is the point of the test.
        let built = stack.resolve(&container).unwrap();
        assert_eq!(built.iter().map(|m| m.name()).collect::<Vec<_>>(), vec!["First"]);
    }

    #[test]
    fn resolved_reports_the_missing_binding_by_type() {
        // The whole message a developer gets when the kernel and the providers
        // disagree, so it has to name both the service and the middleware.
        struct NeverBound;

        let stack = MiddlewareStack::new().resolved(|_: Arc<NeverBound>| First);

        let err = stack.resolve(&Container::new()).err().expect("should fail");
        assert!(err.message().contains("`First`"), "{}", err.message());
        assert!(err.message().contains("NeverBound"), "{}", err.message());
    }

    #[test]
    fn a_deferred_stage_labels_itself_before_it_is_built() {
        // `route:list` and a debug print both want this without running the
        // closure, which needs a container the caller may not have.
        let stack = MiddlewareStack::new().with(First).deferred(|_| Ok(Second));

        assert_eq!(stack.labels(), vec!["First", "Second"]);
        assert_eq!(format!("{stack:?}"), r#"["First", "Second"]"#);
    }

    #[test]
    fn a_shared_instance_is_not_cloned_per_route() {
        // The property a rate limiter depends on: two routes attaching the same
        // `Arc` share its counters.
        let shared: Arc<dyn Middleware> = Arc::new(First);

        let a = MiddlewareStack::new().with_arc(Arc::clone(&shared));
        let b = MiddlewareStack::new().with_arc(Arc::clone(&shared));

        let container = Container::new();
        assert!(Arc::ptr_eq(
            &a.resolve(&container).unwrap()[0],
            &b.resolve(&container).unwrap()[0]
        ));
    }
}
