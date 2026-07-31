//! The [`Event`] and [`Listener`] contracts.

use std::any::{type_name, Any};
use std::future::Future;
use std::sync::Arc;

use rainier_support::{BoxedFuture, Result};

/// Something that happened, which listeners may react to.
///
/// Any plain object can be an event — a blanket impl covers
/// every eligible type, so there is no `impl Event for OrderShipped` to write:
///
/// ```
/// # use rainier_events::Event;
/// struct OrderShipped { pub order_id: u64 }
///
/// assert!(OrderShipped::event_name().ends_with("OrderShipped"));
/// ```
///
/// The bounds are what dispatch requires: `Send + Sync` because listeners may
/// run on any task, and `'static` because the event is type-erased into an
/// `Arc<dyn Any>` to reach listeners registered for its type.
///
/// The cost of the blanket impl is that [`event_name`](Event::event_name)
/// cannot be overridden per type — an impl for your type would conflict with
/// it. When the derived Rust path is not the name you want a wildcard listener
/// to see, dispatch it explicitly with
/// [`Dispatcher::dispatch_as`](crate::Dispatcher::dispatch_as).
pub trait Event: Send + Sync + 'static {
    /// A human-readable name, used by wildcard listeners and log output.
    /// Always the Rust type path; see the trait docs.
    fn event_name() -> &'static str
    where
        Self: Sized,
    {
        type_name::<Self>()
    }
}

impl<T: Send + Sync + 'static> Event for T {}

/// A reaction to an event of type `E`.
///
/// Listeners take `Arc<E>` rather than `&E` so their returned futures own
/// what they need and can be `'static`. A borrowing signature would force
/// every listener's future to be tied to the `dispatch` call's stack frame,
/// which rules out boxing them into one heterogeneous list — and sharing one
/// `Arc` across listeners avoids cloning the event per listener besides.
pub trait Listener<E: Event>: Send + Sync + 'static {
    /// Handle the event.
    ///
    /// Returning `Err` stops the remaining listeners for this event; see
    /// [`Dispatcher::dispatch`](crate::Dispatcher::dispatch).
    fn handle(&self, event: Arc<E>) -> BoxedFuture<Result<()>>;
}

/// Any `async` closure taking `Arc<E>` is a listener.
///
/// ```
/// # use rainier_events::Dispatcher;
/// # use std::sync::Arc;
/// # struct UserRegistered { email: String }
/// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
/// let events = Dispatcher::new();
/// events.listen(|event: Arc<UserRegistered>| async move {
///     println!("welcome {}", event.email);
///     Ok(())
/// });
/// # events.dispatch(UserRegistered { email: "a@b.c".into() }).await?;
/// # Ok(()) }
/// ```
impl<E, F, Fut> Listener<E> for F
where
    E: Event,
    F: Fn(Arc<E>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    fn handle(&self, event: Arc<E>) -> BoxedFuture<Result<()>> {
        Box::pin(self(event))
    }
}

/// A listener registered for *every* event, receiving the event's name and its
/// type-erased payload.
///
/// The payload can be recovered with
/// [`downcast`](std::sync::Arc::downcast) when the listener knows the type;
/// most wildcard listeners (logging, metrics, an audit trail) only need the
/// name.
pub trait WildcardListener: Send + Sync + 'static {
    /// Handle any dispatched event.
    fn handle(
        &self,
        name: &'static str,
        event: Arc<dyn Any + Send + Sync>,
    ) -> BoxedFuture<Result<()>>;
}

impl<F, Fut> WildcardListener for F
where
    F: Fn(&'static str, Arc<dyn Any + Send + Sync>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    fn handle(
        &self,
        name: &'static str,
        event: Arc<dyn Any + Send + Sync>,
    ) -> BoxedFuture<Result<()>> {
        Box::pin(self(name, event))
    }
}

/// A type that registers several related listeners at once — an event
/// subscriber.
///
/// Keeps a feature's event wiring in one place instead of scattering
/// `listen` calls across a service provider.
///
/// ```
/// # use rainier_events::{Dispatcher, EventSubscriber};
/// # use std::sync::Arc;
/// # struct OrderPlaced; struct OrderShipped;
/// struct OrderNotifications;
///
/// impl EventSubscriber for OrderNotifications {
///     fn subscribe(&self, events: &Dispatcher) {
///         events.listen(|_: Arc<OrderPlaced>| async { Ok(()) });
///         events.listen(|_: Arc<OrderShipped>| async { Ok(()) });
///     }
/// }
/// ```
pub trait EventSubscriber {
    /// Register this subscriber's listeners.
    fn subscribe(&self, dispatcher: &crate::Dispatcher);
}
