//! Reporting — putting every failure on the bus, for the application to act on.
//!
//! The framework does not decide which failures matter. Everything it would
//! otherwise swallow — a request that ended in an error, a job attempt that
//! failed, a command that panicked, a cache write that was lost, a captcha
//! provider that did not answer — is published as a typed event from the
//! catalogue in [`rainier_events::report`], and again as the catch-all
//! [`ExceptionReported`]. The application listens for what it cares about:
//!
//! ```no_run
//! use rainier_framework::prelude::*;
//! use rainier_framework::report::{HttpRequestFailed, JobAttemptFailed};
//! use rainier_framework::ExceptionReported;
//!
//! # async fn boot() -> Result<()> {
//! let app = Rainier::new(".")
//!     .with_events(|events| {
//!         // One kind of failure, with its own fields.
//!         events.listen(|failed: Arc<JobAttemptFailed>| async move {
//!             if failed.final_attempt {
//!                 tracing::error!(job = %failed.job, "dead: {}", failed.exception);
//!             }
//!             Ok(())
//!         });
//!         // Everything, for an error tracker.
//!         events.listen(|reported: Arc<ExceptionReported>| async move {
//!             tracing::warn!(kind = %reported.kind(), "{}", reported.exception());
//!             Ok(())
//!         });
//!     })
//!     .boot()
//!     .await?;
//! # let _ = app; Ok(()) }
//! ```
//!
//! # One reporter per application, and one per process
//!
//! [`Rainier::boot`](crate::Rainier::boot) builds one [`Reporter`] over the
//! application's dispatcher, binds it in the container, and hands it to the
//! HTTP kernel; the queue worker, the console and the scheduler resolve it.
//! Its filters and context are set with
//! [`Rainier::with_reporting`](crate::Rainier::with_reporting).
//!
//! Boot also [installs](rainier_events::report::install) it as the
//! **process-wide** reporter, which is how a failure deep in the stack — a
//! queue driver, a cache, a failing event listener — reaches the
//! application's bus without a handle to it. The last application booted
//! wins. An application booted with
//! [`without_facades`](crate::Rainier::without_facades), which is what a test
//! does, leaves the process-wide reporter alone, so parallel tests do not
//! receive each other's failures.
//!
//! Application code that handled an error and still wants it seen reports it
//! with [`report`] (as an [`ApplicationFailed`]), [`report_failure`] (any
//! catalogue type), or the [`Report`](crate::facades::Report) facade.

use rainier_container::Application;
use rainier_events::report::{self as process, ApplicationFailed, Failure};
use rainier_events::{Exception, Reporter};

#[cfg(doc)]
use rainier_events::ExceptionReported;

/// Report an error the application handled, as an [`ApplicationFailed`].
///
/// Goes through the application's bound [`Reporter`] or, for an
/// [`Application`] assembled by hand rather than booted by
/// [`Rainier`](crate::Rainier), the process-wide one. Returns whether it was
/// reported: `false` when the reporter's filters dropped it, and `false` when
/// there is no reporter at all.
///
/// ```no_run
/// # use rainier_framework::prelude::*;
/// # async fn charge(app: &Application) {
/// if let Err(error) = refund().await {
///     // Handled — the customer sees a friendly page — but somebody should know.
///     rainier_framework::report(app, error).await;
/// }
/// # }
/// # async fn refund() -> Result<()> { Ok(()) }
/// ```
pub async fn report(app: &Application, exception: impl Into<Exception>) -> bool {
    report_failure(app, ApplicationFailed::new(exception)).await
}

/// Report any failure from the catalogue — an [`IntegrationFailed`] for the
/// payment provider that timed out, a [`BackgroundTaskFailed`] for a sweeper
/// the application spawned — through the application's reporter.
///
/// Attach fields of your own with
/// [`with_extra`](rainier_events::FailureEvent::with_extra) before reporting.
///
/// ```no_run
/// # use rainier_framework::prelude::*;
/// use rainier_framework::report::{FailureEvent as _, IntegrationFailed};
/// # async fn charge(app: &Application, error: Error) {
/// rainier_framework::report_failure(
///     app,
///     IntegrationFailed::new(error, "payments", "refund").with_extra("order", 42),
/// )
/// .await;
/// # }
/// ```
///
/// [`IntegrationFailed`]: rainier_events::report::IntegrationFailed
/// [`BackgroundTaskFailed`]: rainier_events::report::BackgroundTaskFailed
pub async fn report_failure(app: &Application, failure: impl Into<Failure>) -> bool {
    match reporter_for(app) {
        Some(reporter) => reporter.report(failure).await,
        None => false,
    }
}

/// The application's bound reporter, or the process-wide one.
pub(crate) fn reporter_for(app: &Application) -> Option<Reporter> {
    match app.try_resolve::<Reporter>() {
        Some(reporter) => Some((*reporter).clone()),
        None => process::installed(),
    }
}

/// The message a `panic!` carried, when it carried a string.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use rainier_console::{exit, Arguments, Command, Console};
    use rainier_events::report::{
        CommandFailed, ConfigurationFailed, FailureKind, HttpRequestFailed, ScheduledTaskFailed,
    };
    use rainier_events::FailureEvent as _;
    use rainier_http::StatusCode;
    use rainier_server::Kernel;
    use rainier_support::{Error, ErrorKind, Result};

    use crate::testing::TestApp;
    use crate::Rainier;

    fn builder() -> Rainier {
        Rainier::new(".").without_facades().without_tracing().with_routes(|router| {
            router.get("/boom", || async { Err::<&str, _>(Error::internal("disk full")) });
            router.get("/missing", || async { Err::<&str, _>(Error::not_found("no post")) });
            router.get("/fine", || async { "fine" });
        })
    }

    async fn boot(builder: Rainier) -> TestApp {
        TestApp::new(builder.boot().await.expect("boots")).expect("has a kernel")
    }

    /// The reports of everything but configuration warnings, which a test
    /// application on the default (in-process) stores may well produce at
    /// boot.
    fn reported_without_configuration(app: &TestApp) -> Vec<rainier_events::ExceptionReported> {
        app.reported().into_iter().filter(|r| r.kind() != FailureKind::Configuration).collect()
    }

    struct Failing;

    #[async_trait::async_trait]
    impl Command for Failing {
        fn name(&self) -> &str {
            "app:fail"
        }
        async fn handle(&self, _: &Arguments, _: &Application) -> Result<i32> {
            Err(Error::internal("the export bucket is gone"))
        }
    }

    struct Panicking;

    #[async_trait::async_trait]
    impl Command for Panicking {
        fn name(&self) -> &str {
            "app:panic"
        }
        async fn handle(&self, _: &Arguments, _: &Application) -> Result<i32> {
            panic!("the invariant did not hold")
        }
    }

    #[tokio::test]
    async fn booting_binds_one_reporter_and_hands_it_to_the_kernel() {
        let app = builder().boot().await.unwrap();

        let reporter = app.resolve::<Reporter>().expect("a reporter is bound");
        let kernel = app.resolve::<Kernel>().unwrap();
        assert!(kernel.reporter().is_some(), "the kernel reports");

        // Over the dispatcher the container hands out, not a private one.
        let events = app.resolve::<rainier_events::Dispatcher>().unwrap();
        assert!(Arc::ptr_eq(reporter.events(), &events));
    }

    #[tokio::test]
    async fn a_500_is_reported_as_a_failed_request_with_where_it_happened() {
        let app = boot(builder()).await;

        app.get("/boom").await.assert_status(StatusCode::INTERNAL_SERVER_ERROR);

        let failed = app.reported_of::<HttpRequestFailed>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].method, "GET");
        assert_eq!(failed[0].path, "/boom");
        assert_eq!(failed[0].exception.message, "disk full");
        assert_eq!(failed[0].exception.kind, Some(ErrorKind::Internal));
        // The default context every report carries.
        assert_eq!(failed[0].meta.extra["app"], "Rainier");
        assert!(failed[0].meta.extra.contains_key("environment"));
        assert!(!failed[0].meta.id.is_empty());

        // And once more as the catch-all, with the same id.
        let reported = reported_without_configuration(&app);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].kind(), FailureKind::HttpRequest);
        assert_eq!(reported[0].meta().id, failed[0].meta.id);
    }

    #[tokio::test]
    async fn a_listener_registered_with_events_sees_the_typed_report() {
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let app = boot(builder().with_events(move |events| {
            events.listen(move |_: Arc<HttpRequestFailed>| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            });
        }))
        .await;

        app.get("/boom").await;
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn client_errors_are_reported_too_and_successes_are_not() {
        let app = boot(builder()).await;

        app.get("/fine").await.assert_ok();
        assert!(app.reported_of::<HttpRequestFailed>().is_empty());

        app.get("/missing").await.assert_not_found();
        let failed = app.reported_of::<HttpRequestFailed>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].exception.status, 404);
    }

    #[tokio::test]
    async fn with_reporting_filters() {
        let quiet = boot(builder().with_reporting(|r| r.dont_report(ErrorKind::NotFound))).await;
        quiet.get("/missing").await.assert_not_found();
        assert!(quiet.reported_of::<HttpRequestFailed>().is_empty());

        let deaf = boot(builder().with_reporting(|r| r.ignore(FailureKind::HttpRequest))).await;
        deaf.get("/boom").await.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
        assert!(deaf.reported_of::<HttpRequestFailed>().is_empty());
    }

    #[tokio::test]
    async fn with_reporting_adds_context_after_the_defaults_and_applies_in_order() {
        let app = boot(
            builder()
                .with_reporting(|r| {
                    r.with_context(|_, meta| {
                        meta.extra.insert("release".into(), "1.2.3".into());
                    })
                })
                .with_reporting(|r| {
                    r.with_context(|kind, meta| {
                        meta.extra.insert("app".into(), "storefront".into());
                        meta.extra.insert("kind".into(), kind.as_str().into());
                    })
                }),
        )
        .await;

        app.get("/boom").await;

        let failed = app.reported_of::<HttpRequestFailed>();
        assert_eq!(failed[0].meta.extra["release"], "1.2.3");
        assert_eq!(failed[0].meta.extra["app"], "storefront", "a later provider wins");
        assert_eq!(failed[0].meta.extra["kind"], "HttpRequest");
    }

    #[tokio::test]
    async fn report_reports_an_application_failure_and_honours_the_filters() {
        let app = boot(builder().with_reporting(|r| r.dont_report(ErrorKind::NotFound))).await;

        assert!(report(app.app(), Error::internal("refund failed")).await);
        assert!(!report(app.app(), Error::not_found("no such order")).await);

        let failed = app.reported_of::<ApplicationFailed>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].exception.message, "refund failed");
    }

    #[tokio::test]
    async fn report_failure_keeps_the_type_and_the_callers_fields() {
        let app = boot(builder()).await;

        let failure = rainier_events::report::IntegrationFailed::new(
            Error::internal("timed out"),
            "payments",
            "refund",
        )
        .with_extra("order", 42);
        assert!(report_failure(app.app(), failure).await);

        let failed = app.reported_of::<rainier_events::report::IntegrationFailed>();
        assert_eq!(failed[0].service, "payments");
        assert_eq!(failed[0].meta.extra["order"], 42);
        assert_eq!(failed[0].meta.extra["app"], "Rainier", "the providers still ran");
    }

    #[tokio::test]
    async fn the_report_facade_is_the_bound_reporter() {
        use rainier_container::Facade as _;

        let app = boot(builder()).await;
        crate::facades::Report::instance().report_error(Error::internal("via the facade")).await;
        assert_eq!(app.reported_of::<ApplicationFailed>()[0].exception.message, "via the facade");
    }

    #[tokio::test]
    async fn without_a_bound_reporter_report_goes_to_the_process_wide_one() {
        let capture = rainier_events::report::capture();
        let app = Application::new(".");
        assert!(report(&app, Error::internal("nobody bound one")).await);
        assert!(capture.reports().iter().any(|r| r.exception().message == "nobody bound one"));
    }

    #[tokio::test]
    async fn a_failing_command_is_reported_with_its_name() {
        let app = boot(builder()).await;
        let console = Console::new("rainier").register(Failing);

        assert_eq!(console.run_argv(app.app(), ["app:fail"]).await, exit::FAILURE);

        let failed = app.reported_of::<CommandFailed>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].command, "app:fail");
        assert_eq!(failed[0].exception.message, "the export bucket is gone");
    }

    #[tokio::test]
    async fn a_panicking_command_is_reported_and_still_panics() {
        let app = builder().boot().await.unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        app.resolve::<Reporter>().unwrap().events().listen(move |failed: Arc<CommandFailed>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push((*failed).clone());
                Ok(())
            }
        });

        let console = Arc::new(Console::new("rainier").register(Panicking));
        let task = {
            let app = Arc::clone(&app);
            let console = Arc::clone(&console);
            tokio::spawn(async move { console.run_argv(&app, ["app:panic"]).await })
        };
        assert!(task.await.expect_err("the panic is resumed").is_panic());

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].exception.panic);
        assert_eq!(seen[0].exception.message, "the invariant did not hold");
        assert_eq!(seen[0].command, "app:panic");
    }

    #[tokio::test]
    async fn a_failing_scheduled_task_is_reported_with_its_name() {
        let app = boot(builder().with_schedule(|schedule| {
            schedule
                .call("prune-exports", |_| {
                    Box::pin(async {
                        Err(Error::internal("prune failed")
                            .with_source(std::io::Error::other("permission denied")))
                    })
                })
                .every_minute();
            schedule.call("heartbeat", |_| Box::pin(async { Ok(()) })).every_minute();
        }))
        .await;

        let console = crate::console("rainier");
        assert_eq!(console.run_argv(app.app(), ["schedule:run"]).await, exit::FAILURE);

        // Once, as the task — `schedule:run` itself returned `Ok(1)`, which is
        // the command's own answer and not a second failure.
        let failed = app.reported_of::<ScheduledTaskFailed>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].task, "prune-exports");
        assert_eq!(failed[0].exception.message, "prune failed");
        assert_eq!(failed[0].exception.sources, vec!["permission denied"]);
        assert!(!failed[0].meta.extra.contains_key("command"), "a closure is not a command");
        assert!(app.reported_of::<CommandFailed>().is_empty());
    }

    #[tokio::test]
    async fn a_failing_scheduled_command_is_reported_twice_and_the_two_are_linked() {
        let console = Arc::new(Console::new("rainier").register(Failing));
        let app = boot(builder().with_schedule({
            let console = Arc::clone(&console);
            move |schedule| {
                use crate::ScheduleExt as _;
                schedule.command_with(console, "app:fail", ["--dry-run"]).every_minute();
            }
        }))
        .await;

        let runner = crate::console("rainier");
        assert_eq!(runner.run_argv(app.app(), ["schedule:run"]).await, exit::FAILURE);

        // The command's own failure, with its own error…
        let command = app.reported_of::<CommandFailed>();
        assert_eq!(command.len(), 1);
        assert_eq!(command[0].command, "app:fail");
        assert_eq!(command[0].exception.message, "the export bucket is gone");

        // …and the schedule's, naming the command it ran.
        let task = app.reported_of::<ScheduledTaskFailed>();
        assert_eq!(task.len(), 1);
        assert_eq!(task[0].task, "app:fail --dry-run");
        assert_eq!(task[0].meta.extra["command"], "app:fail");
    }

    #[tokio::test]
    async fn boot_reports_unsafe_configuration_it_carried_on_with() {
        // Boot reports before a harness exists, so listen from the builder.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        builder()
            .with_events(move |events| {
                events.listen(move |failed: Arc<ConfigurationFailed>| {
                    let sink = Arc::clone(&sink);
                    async move {
                        sink.lock().unwrap().push((*failed).clone());
                        Ok(())
                    }
                });
            })
            .boot()
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        // No APP_KEY in the test environment, so one was generated.
        if std::env::var("APP_KEY").map(|k| k.trim().is_empty()).unwrap_or(true) {
            assert!(seen.iter().any(|c| c.setting == "APP_KEY"), "{seen:?}");
        }
        // Never the generated key itself.
        assert!(seen.iter().all(|c| !c.exception.message.contains("APP_KEY=")));
    }

    #[test]
    fn a_panic_message_is_read_from_either_string_type() {
        assert_eq!(panic_message(&"static"), "static");
        assert_eq!(panic_message(&String::from("owned")), "owned");
        assert_eq!(panic_message(&7u8), "panicked with a non-string payload");
    }
}
