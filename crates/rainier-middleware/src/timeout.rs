//! A ceiling on how long a request may take — [`Timeout`].
//!
//! ```ignore
//! // Globally, from `server.request_timeout_secs`.
//! registry.global(Timeout::seconds(30));
//!
//! // Or on the one route that talks to something slow.
//! router.post("/import", import).middleware(Timeout::seconds(300));
//! ```
//!
//! Without one, a handler that never returns holds its connection, its task
//! and whatever it borrowed for as long as the process lives. Enough of those
//! and the pool is gone; the symptom is a service that stops answering
//! everything, with nothing in the log about the one endpoint that hung.
//!
//! # What cancelling actually does
//!
//! At the deadline the handler's future is **dropped**. Everything it holds is
//! released and the response becomes `408`. But dropping a future only stops
//! the part that had not run yet — work already handed to someone else keeps
//! going:
//!
//! - A query in flight was already sent. The database will run it to
//!   completion and the row will be written; only the *answer* is discarded.
//! - An HTTP request already issued reaches the other service.
//! - A job already pushed to the queue will be worked.
//!
//! So this bounds *this service's* latency. It is not a way to undo anything,
//! and a handler that must be all-or-nothing needs a transaction, not a
//! timeout.
//!
//! # It can only interrupt at an await
//!
//! A future is cancelled between its `await` points. A handler that blocks the
//! thread — a long CPU loop, a synchronous file read, a `std::thread::sleep` —
//! never yields, so the timer never gets to fire and the whole runtime worker
//! stays blocked. Move that work to
//! [`spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html);
//! this middleware cannot save you from it.

use std::time::Duration;

use rainier_http::{IntoResponse, Request, Response};
use rainier_support::Error;

use crate::pipeline::{Middleware, Next};

/// Answers `408` when the handler takes longer than `limit`.
#[derive(Debug, Clone, Copy)]
pub struct Timeout {
    limit: Duration,
}

impl Timeout {
    /// A limit of `limit`.
    pub fn new(limit: Duration) -> Self {
        Self { limit }
    }

    /// A limit in whole seconds — the unit a configuration file uses.
    pub fn seconds(seconds: u64) -> Self {
        Self::new(Duration::from_secs(seconds))
    }

    /// A limit in milliseconds, for a fast internal endpoint.
    pub fn millis(millis: u64) -> Self {
        Self::new(Duration::from_millis(millis))
    }

    /// The limit.
    pub fn limit(&self) -> Duration {
        self.limit
    }
}

#[async_trait::async_trait]
impl Middleware for Timeout {
    async fn handle(&self, request: Request, next: Next) -> Response {
        // Kept for the log line: `request` is moved into the future, and the
        // whole point is that the future may not come back.
        let method = request.method().to_string();
        let uri = request.uri().path().to_string();

        match tokio::time::timeout(self.limit, next.run(request)).await {
            Ok(response) => response,
            Err(_) => {
                // At `warn`, not `error`: one slow request is a fact about the
                // request. It is worth seeing, and it is not a fault of the
                // process.
                tracing::warn!(
                    %method,
                    %uri,
                    timeout_ms = self.limit.as_millis() as u64,
                    "request cancelled at its timeout"
                );

                Error::request_timeout(format!(
                    "The request did not finish within {} seconds.",
                    self.limit.as_secs_f64()
                ))
                .into_response()
            }
        }
    }

    fn name(&self) -> &'static str {
        "Timeout"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;
    use rainier_http::{Method, StatusCode};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn request() -> Request {
        Request::builder().method(Method::GET).uri("/slow").build()
    }

    #[tokio::test]
    async fn a_handler_that_finishes_in_time_is_untouched() {
        let response = Pipeline::new()
            .through(Timeout::seconds(30))
            .then(|_| async { Response::ok("in time") })
            .run(request())
            .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_handler_that_overruns_gets_408() {
        let response = Pipeline::new()
            .through(Timeout::millis(10))
            .then(|_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Response::ok("far too late")
            })
            .run(request())
            .await;

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn the_overrunning_handler_is_actually_dropped() {
        // Not just abandoned to run in the background — the whole point is
        // that it stops holding its connection and its buffers.
        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);

        let response = Pipeline::new()
            .through(Timeout::millis(10))
            .then(move |_| {
                let flag = Arc::clone(&flag);
                async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    flag.store(true, Ordering::SeqCst);
                    Response::ok("never")
                }
            })
            .run(request())
            .await;

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);

        // Long enough that it would have finished had it survived.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!finished.load(Ordering::SeqCst), "the handler kept running past its timeout");
    }

    #[tokio::test]
    async fn the_message_says_how_long_it_waited() {
        let response = Pipeline::new()
            .through(Timeout::millis(10))
            .then(|_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Response::ok("no")
            })
            .run(request())
            .await;

        let body = response.into_string().await.unwrap();
        assert!(body.contains("0.01"), "{body}");
    }
}
