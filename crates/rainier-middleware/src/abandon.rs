//! Running work that may outlive whoever is waiting for it —
//! [`run_detachable`].
//!
//! A future in Rust is stopped by dropping it, and whoever holds it decides
//! when: hyper drops a request's future when its client hangs up, and
//! [`Timeout`](crate::Timeout) drops its inner one at the deadline. To let a
//! route's work *survive* that, the work has to be owned by something else —
//! its own task — with the caller only waiting on it.
//!
//! [`run_detachable`] is that, plus the decision of what to do when the waiting
//! stops early: it reads why the request was [abandoned](Abandonment) and
//! asks the route's [`AbandonPolicy`] whether to let the task finish or to stop
//! it.

use std::future::Future;

use rainier_http::{AbandonPolicy, Abandonment, Response};

/// Run `work` in its own task and wait for it.
///
/// If this future is dropped before `work` is done, `abandonment` says why —
/// the client hung up, or a timeout gave up — and `policy` decides: on
/// [`OnAbandon::Finish`](rainier_http::OnAbandon::Finish) the task runs on
/// and its response is discarded; on
/// [`OnAbandon::Cancel`](rainier_http::OnAbandon::Cancel) it is aborted at its
/// next `await`. Dropped for no recorded reason, it is aborted: work is only
/// ever kept running because a policy asked for it.
///
/// The task inherits what a request's work expects to find around it — the
/// facade application, the event bus's unit of work, and the tracing span —
/// so a handler cannot tell it was moved. A panic in it is re-raised here, for
/// the kernel to catch as it would any handler's.
///
/// On wasm, where there is no runtime to spawn on, `work` simply runs in
/// place and is cancelled with its caller.
pub async fn run_detachable<F>(work: F, abandonment: Abandonment, policy: AbandonPolicy) -> Response
where
    F: Future<Output = Response> + Send + 'static,
{
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (abandonment, policy);
        work.await
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use tracing::Instrument;

        let span = tracing::Span::current();
        let context = rainier_events::Context::current();
        let work = context.scope(work).instrument(span);
        let handle = match rainier_container::try_facade_application() {
            Some(app) => tokio::spawn(rainier_container::with_facade_application(app, work)),
            None => tokio::spawn(work),
        };

        let mut guard = Detached { handle: Some(handle), abandonment, policy };
        let joined = match guard.handle.as_mut() {
            Some(handle) => handle.await,
            None => unreachable!("the handle is only taken on drop"),
        };
        guard.handle = None;

        match joined {
            Ok(response) => response,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            // Only the runtime shutting down aborts the task while we are
            // still waiting on it.
            Err(_) => rainier_http::IntoResponse::into_response(rainier_support::Error::new(
                rainier_support::ErrorKind::ServiceUnavailable,
                "The server is shutting down.",
            )),
        }
    }
}

/// Owns the task while it runs, and decides its fate if the waiting stops.
#[cfg(not(target_arch = "wasm32"))]
struct Detached {
    handle: Option<tokio::task::JoinHandle<Response>>,
    abandonment: Abandonment,
    policy: AbandonPolicy,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Detached {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else { return };
        if handle.is_finished() {
            return;
        }
        match self.abandonment.reason() {
            Some(reason) if self.policy.for_reason(reason) == rainier_http::OnAbandon::Finish => {
                tracing::debug!(
                    reason = reason.name(),
                    "request abandoned; its work runs on to the end"
                );
                // Dropping a JoinHandle detaches the task; it keeps running.
                drop(handle);
            }
            reason => {
                tracing::debug!(
                    reason = reason.map(|r| r.name()).unwrap_or("none"),
                    "request abandoned; its work is cancelled"
                );
                handle.abort();
            }
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use rainier_http::{AbandonReason, OnAbandon};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    async fn slow(flag: Arc<AtomicBool>) -> Response {
        tokio::time::sleep(Duration::from_millis(100)).await;
        flag.store(true, Ordering::SeqCst);
        Response::text("done")
    }

    #[tokio::test]
    async fn finished_work_is_returned() {
        let flag = Arc::new(AtomicBool::new(false));
        let response =
            run_detachable(slow(Arc::clone(&flag)), Abandonment::new(), AbandonPolicy::new()).await;
        assert_eq!(response.status(), rainier_http::StatusCode::OK);
        assert!(flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropped_with_no_reason_is_cancelled() {
        let flag = Arc::new(AtomicBool::new(false));
        let waiting =
            run_detachable(slow(Arc::clone(&flag)), Abandonment::new(), AbandonPolicy::finish());
        let _ = tokio::time::timeout(Duration::from_millis(20), waiting).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!flag.load(Ordering::SeqCst), "work was kept running with no policy asking");
    }

    #[tokio::test]
    async fn abandoned_and_told_to_finish_runs_on() {
        let flag = Arc::new(AtomicBool::new(false));
        let abandonment = Abandonment::new();
        let policy = AbandonPolicy::new().on_timeout(OnAbandon::Finish);
        let mut waiting =
            Box::pin(run_detachable(slow(Arc::clone(&flag)), abandonment.clone(), policy));
        // Start it, then abandon it before dropping, as `Timeout` does.
        let _ = tokio::time::timeout(Duration::from_millis(20), waiting.as_mut()).await;
        abandonment.abandon(AbandonReason::TimedOut);
        drop(waiting);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(flag.load(Ordering::SeqCst), "the work was cancelled despite the policy");
    }

    #[tokio::test]
    async fn abandoned_and_told_to_cancel_stops() {
        let flag = Arc::new(AtomicBool::new(false));
        let abandonment = Abandonment::new();
        let mut waiting = Box::pin(run_detachable(
            slow(Arc::clone(&flag)),
            abandonment.clone(),
            AbandonPolicy::new(),
        ));
        let _ = tokio::time::timeout(Duration::from_millis(20), waiting.as_mut()).await;
        // The default cancels at a timeout.
        abandonment.abandon(AbandonReason::TimedOut);
        drop(waiting);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!flag.load(Ordering::SeqCst), "the work ran on despite the policy");
    }

    #[tokio::test]
    async fn a_panic_is_raised_in_the_caller() {
        let outcome = tokio::spawn(run_detachable(
            async { panic!("boom") },
            Abandonment::new(),
            AbandonPolicy::new(),
        ))
        .await;
        assert!(outcome.unwrap_err().is_panic());
    }
}
