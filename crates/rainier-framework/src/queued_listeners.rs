//! Listeners that go on the queue — [`DispatcherExt`] and [`FromEvent`].
//!
//! ```ignore
//! // src/providers/event.rs
//! events.listen_queued::<UserRegistered, SendWelcomeEmail>();
//!
//! impl FromEvent<UserRegistered> for SendWelcomeEmail {
//!     fn from_event(event: &UserRegistered) -> Self {
//!         Self { user_id: event.user_id }
//!     }
//! }
//! ```
//!
//! An ordinary listener runs **inside
//! the dispatch**, which means inside the request that dispatched it: a
//! welcome email sent from a listener is 400ms of SMTP the person who just
//! signed up waits for, and an SMTP server having a bad minute becomes a
//! registration endpoint having a bad minute.
//!
//! A queued listener does one write and returns. The work happens in a worker,
//! with retries, backoff and a failed-jobs table — all of which the inline
//! version has none of.
//!
//! # Which one to use
//!
//! | | Inline | Queued |
//! |---|---|---|
//! | Runs | in the request | in a worker |
//! | On failure | fails the dispatch | retried, then recorded |
//! | Sees | the event itself | what [`FromEvent`] copied out |
//! | Right for | updating a counter, invalidating a cache | mail, webhooks, anything over a network |
//!
//! # The job is built at dispatch, not at handle
//!
//! [`FromEvent::from_event`] runs while the event is still in hand, and what
//! it copies out is serialised into the payload. So a job takes an id, not a
//! model — by the time a worker picks it up, minutes may have passed and the
//! row may have changed. Re-reading it is the point.

use std::sync::Arc;

use rainier_events::{Dispatcher, Event};
use rainier_queue::{Job, QueueManager};
use rainier_support::Error;

/// A job that can be built from an event.
///
/// The seam between "something happened" and "here is the work". Keep it to
/// ids and values that are already true — see the module docs.
pub trait FromEvent<E: Event>: Job {
    /// Build the job this event should queue.
    fn from_event(event: &E) -> Self;
}

/// [`listen_queued`](DispatcherExt::listen_queued), on top of [`Dispatcher`].
///
/// An extension trait rather than methods on `Dispatcher`, because the events
/// crate would otherwise depend on the queue — and an application with no
/// queue at all would still compile one.
pub trait DispatcherExt {
    /// When `E` happens, queue a `J` built from it.
    ///
    /// The [`QueueManager`] is resolved from the container when the event
    /// fires, not now — listeners are registered while the application is
    /// still being built, and the queue is bound during that same build.
    ///
    /// Dispatching therefore fails with a clear error if there is no queue
    /// bound, rather than silently doing nothing. A listener that quietly
    /// stopped queueing is a feature that quietly stopped working.
    fn listen_queued<E, J>(&self)
    where
        E: Event,
        J: FromEvent<E>;

    /// [`listen_queued`](Self::listen_queued), against a queue you hand over.
    ///
    /// For a test, or for an application that does not install the facades.
    fn listen_queued_on<E, J>(&self, queue: Arc<QueueManager>)
    where
        E: Event,
        J: FromEvent<E>;
}

impl DispatcherExt for Dispatcher {
    fn listen_queued<E, J>(&self)
    where
        E: Event,
        J: FromEvent<E>,
    {
        self.listen(move |event: Arc<E>| async move {
            let app = rainier_container::try_facade_application().ok_or_else(|| {
                Error::internal(format!(
                    "`{}` should queue a `{}`, but no application is installed for the facades \
                     — use `listen_queued_on` with the queue instead",
                    std::any::type_name::<E>(),
                    J::NAME
                ))
            })?;

            let queue = app.resolve::<QueueManager>()?;
            queue.dispatch(J::from_event(&event)).await?;
            Ok(())
        });
    }

    fn listen_queued_on<E, J>(&self, queue: Arc<QueueManager>)
    where
        E: Event,
        J: FromEvent<E>,
    {
        self.listen(move |event: Arc<E>| {
            let queue = Arc::clone(&queue);
            async move {
                queue.dispatch(J::from_event(&event)).await?;
                Ok(())
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_queue::{JobContext, QueueManager};
    use serde::{Deserialize, Serialize};

    struct UserRegistered {
        user_id: u64,
        /// Deliberately not copied into the job — see `from_event`.
        #[allow(dead_code, reason = "the point is that the job does not take it")]
        email: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct SendWelcomeEmail {
        user_id: u64,
    }

    #[async_trait::async_trait]
    impl Job for SendWelcomeEmail {
        const NAME: &'static str = "send-welcome-email";

        async fn handle(&self, _context: &JobContext) -> rainier_support::Result<()> {
            Ok(())
        }
    }

    impl FromEvent<UserRegistered> for SendWelcomeEmail {
        fn from_event(event: &UserRegistered) -> Self {
            // The id, not the email — by the time a worker runs this, the
            // address may have been changed or confirmed.
            Self { user_id: event.user_id }
        }
    }

    #[tokio::test]
    async fn dispatching_the_event_puts_a_job_on_the_queue() {
        let queue = Arc::new(QueueManager::fake());
        let events = Dispatcher::new();
        events.listen_queued_on::<UserRegistered, SendWelcomeEmail>(Arc::clone(&queue));

        events
            .dispatch(UserRegistered { user_id: 7, email: "ada@example.com".into() })
            .await
            .unwrap();

        let pushed = queue.pushed::<SendWelcomeEmail>();
        assert_eq!(pushed.len(), 1);
        // The id, because that is what `from_event` copied out.
        assert_eq!(pushed[0].payload["user_id"], 7);
    }

    #[tokio::test]
    async fn an_event_with_no_listener_queues_nothing() {
        let queue = Arc::new(QueueManager::fake());
        let events = Dispatcher::new();

        events
            .dispatch(UserRegistered { user_id: 7, email: "ada@example.com".into() })
            .await
            .unwrap();

        assert!(queue.all_pushed().is_empty());
    }

    #[tokio::test]
    async fn without_a_container_the_failure_says_so() {
        // Rather than the listener silently doing nothing, which is a feature
        // that quietly stopped working.
        let events = Dispatcher::new();
        events.listen_queued::<UserRegistered, SendWelcomeEmail>();

        let app = Arc::new(rainier_container::Application::new("."));
        let _scope = rainier_container::scope_facade_application(app);

        let failed =
            events.dispatch(UserRegistered { user_id: 7, email: "ada@example.com".into() }).await;

        // There is an application, but nothing bound a queue into it.
        let message = failed.unwrap_err().message().to_string();
        assert!(message.contains("QueueManager"), "{message}");
    }
}
