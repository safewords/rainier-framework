//! The global middleware stack — [`MiddlewareRegistry`].
//!
//! A name-keyed HTTP kernel carries three lists: a global stack, a
//! name-to-class alias map, and named groups of those names. Rainier keeps
//! the first and has no use for the other two.
//!
//! A route attaches [the middleware itself](crate::stack), and a group is a
//! function returning a [`MiddlewareStack`]. Both are
//! values, so neither needs a registry to look them up in — and neither can be
//! misspelled.
//!
//! What is left genuinely is a registry: middleware that runs on **every**
//! request, which by definition is not attached at any one route.
//!
//! ```
//! use rainier_middleware::{MiddlewareRegistry, TrimStrings};
//!
//! let registry = MiddlewareRegistry::new();
//! registry.global(TrimStrings::new());
//!
//! assert_eq!(registry.global_labels(), vec!["TrimStrings"]);
//! ```

use std::sync::{Arc, RwLock};

use rainier_container::Container;

use crate::pipeline::Middleware;
use crate::stack::{IntoMiddlewareStack, MiddlewareStack};

/// The middleware that runs on every request.
///
/// Held by the application and given to the kernel, which runs it outside every
/// route's own pipeline.
#[derive(Default)]
pub struct MiddlewareRegistry {
    global: RwLock<MiddlewareStack>,
}

impl MiddlewareRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add middleware that runs on **every** request, in registration order,
    /// before any route middleware.
    ///
    /// Takes anything a route does — one middleware, a tuple, or a whole
    /// [`MiddlewareStack`]:
    ///
    /// ```
    /// # use rainier_middleware::{ConvertEmptyStringsToNull, MiddlewareRegistry, TrimStrings};
    /// let registry = MiddlewareRegistry::new();
    /// registry.global((TrimStrings::new(), ConvertEmptyStringsToNull));
    ///
    /// assert_eq!(registry.global_labels().len(), 2);
    /// ```
    pub fn global(&self, middleware: impl IntoMiddlewareStack) {
        let mut global = self.global.write().expect("registry lock poisoned");
        let existing = std::mem::take(&mut *global);
        *global = existing.with_stack(middleware.into_middleware_stack());
    }

    /// [`global`](Self::global) with an already-shared instance.
    pub fn global_arc(&self, middleware: Arc<dyn Middleware>) {
        let mut global = self.global.write().expect("registry lock poisoned");
        let existing = std::mem::take(&mut *global);
        *global = existing.with_arc(middleware);
    }

    /// The global stack, unresolved.
    pub fn global_middleware(&self) -> MiddlewareStack {
        self.global.read().expect("registry lock poisoned").clone()
    }

    /// The global stack, built against `container`.
    ///
    /// Fails if a [deferred](MiddlewareStack::deferred) stage cannot be built —
    /// at boot, where the container is the thing to look at.
    pub fn global_stack(
        &self,
        container: &Container,
    ) -> rainier_support::Result<Vec<Arc<dyn Middleware>>> {
        self.global.read().expect("registry lock poisoned").resolve(container)
    }

    /// The labels of the global stack, without building it.
    pub fn global_labels(&self) -> Vec<&'static str> {
        self.global.read().expect("registry lock poisoned").labels()
    }
}

impl std::fmt::Debug for MiddlewareRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiddlewareRegistry")
            .field("global", &self.global.read().map(|g| g.labels()).unwrap_or_default())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Next;
    use rainier_http::{Request, Response};

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl Middleware for Named {
        async fn handle(&self, request: Request, next: Next) -> Response {
            next.run(request).await
        }
        fn name(&self) -> &'static str {
            self.0
        }
    }

    fn names(registry: &MiddlewareRegistry) -> Vec<&'static str> {
        registry.global_stack(&Container::new()).unwrap().iter().map(|m| m.name()).collect()
    }

    #[test]
    fn the_global_stack_keeps_its_order() {
        let registry = MiddlewareRegistry::new();
        registry.global(Named("first"));
        registry.global(Named("second"));

        assert_eq!(names(&registry), vec!["first", "second"]);
    }

    #[test]
    fn a_tuple_registers_several_at_once_in_order() {
        let registry = MiddlewareRegistry::new();
        registry.global((Named("first"), Named("second")));
        registry.global(Named("third"));

        assert_eq!(names(&registry), vec!["first", "second", "third"]);
    }

    #[test]
    fn a_whole_stack_can_be_registered_globally() {
        let registry = MiddlewareRegistry::new();
        registry.global(MiddlewareStack::new().with(Named("a")).with(Named("b")));

        assert_eq!(names(&registry), vec!["a", "b"]);
    }

    #[test]
    fn a_failing_deferred_global_stops_the_boot() {
        // Global middleware that cannot be built is not something to carry on
        // without: it is on every request by definition.
        let registry = MiddlewareRegistry::new();
        registry.global(
            MiddlewareStack::new()
                .deferred(|_| Err::<Named, _>(rainier_support::Error::internal("nothing bound"))),
        );

        let err = registry.global_stack(&Container::new()).err().expect("should fail");
        assert!(err.message().contains("nothing bound"), "{}", err.message());
    }

    #[test]
    fn the_labels_are_readable_without_a_container() {
        let registry = MiddlewareRegistry::new();
        registry.global(Named("first"));

        assert_eq!(registry.global_labels(), vec!["first"]);
    }
}
