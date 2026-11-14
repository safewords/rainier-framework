//! Where the queue's failures go.
//!
//! The [`Worker`](crate::Worker) holds a [`Reporter`] when it was given one.
//! The drivers and the Kubernetes dispatcher hold nothing of the application,
//! so they report through the **process-wide** reporter
//! ([`rainier_events::report::install`]), which does nothing when none is
//! installed. Either way, reporting never changes what happens to a job.

use rainier_events::report::{self, Failure};
use rainier_events::{Exception, Reporter};
use rainier_support::Error;

/// Snapshot an error the caller still has to return.
///
/// [`Exception::from_error`] takes the error, and [`Error`] is not `Clone`;
/// this keeps everything but the original value, which a report that travels
/// as JSON loses anyway.
pub(crate) fn snapshot(error: &Error) -> Exception {
    Exception {
        kind: Some(error.kind()),
        status: error.status(),
        message: error.message().to_string(),
        details: error.details().cloned(),
        sources: error
            .source_error()
            .map(|source| source.chain().map(ToString::to_string).collect())
            .unwrap_or_default(),
        backtrace: error.backtrace().map(ToString::to_string),
        panic: false,
        error: None,
    }
}

/// Report `failure` through `reporter`, or through the process-wide reporter
/// when there is none. Returns whether it was reported.
pub(crate) async fn report_to(reporter: Option<&Reporter>, failure: impl Into<Failure>) -> bool {
    match reporter {
        Some(reporter) => reporter.report(failure).await,
        None => report::report(failure).await,
    }
}

/// Report `failure` through the process-wide reporter.
pub(crate) async fn report(failure: impl Into<Failure>) -> bool {
    report::report(failure).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_events::report::{capture, Failure, QueueOperationFailed};
    use rainier_events::Dispatcher;
    use std::sync::Arc;

    #[test]
    fn a_snapshot_keeps_the_kind_the_message_and_the_cause_chain() {
        let error = Error::service_unavailable("broker down")
            .with_source(std::io::Error::other("connection refused"));
        let exception = snapshot(&error);

        assert_eq!(exception.kind, Some(rainier_support::ErrorKind::ServiceUnavailable));
        assert_eq!(exception.status, 503);
        assert_eq!(exception.message, "broker down");
        assert_eq!(exception.sources, vec!["connection refused"]);
        assert!(!exception.panic);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn without_a_reporter_the_process_wide_one_is_used() {
        let capture = capture();
        let failure = QueueOperationFailed::new(
            Error::internal("x"),
            Some("memory".to_string()),
            None,
            "reporting-test-fallback",
            None,
        );
        assert!(report_to(None, failure).await);

        let seen: Vec<_> = capture
            .reports()
            .into_iter()
            .filter(|r| {
                matches!(&r.failure, Failure::QueueOperation(f) if f.operation == "reporting-test-fallback")
            })
            .collect();
        assert_eq!(seen.len(), 1);
    }

    #[tokio::test]
    async fn a_reporter_given_is_used_instead() {
        let events = Arc::new(Dispatcher::fake());
        let reporter = Reporter::new(Arc::clone(&events));
        let failure =
            QueueOperationFailed::new(Error::internal("x"), None, None, "reporting-test-own", None);

        assert!(report_to(Some(&reporter), failure).await);
        events.assert_dispatched_times::<QueueOperationFailed>(1);
    }
}
