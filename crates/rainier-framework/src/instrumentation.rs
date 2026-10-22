//! Passive instrumentation — what the `telemetry` feature installs at boot.
//!
//! With the feature on, every component already opens its spans (see
//! `docs/observability.md#passive-instrumentation`). What is left for an
//! application to do is *send them somewhere*, and this module does that too,
//! so turning tracing on is a build flag and two environment variables:
//!
//! ```env
//! TELEMETRY_ENABLED=true
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4317
//! ```
//!
//! | At boot | What happens |
//! |---|---|
//! | the subscriber | logging as before, plus the OTLP layer when an endpoint is set — one subscriber, installed once |
//! | the kernel | [`Trace`](rainier_telemetry::Trace) outermost in the global stack, unless the application already put one there |
//! | `queue:work` | a [`QueueMetrics`] observer on every worker, when a `Metrics` registry is bound |
//! | `schedule:run` / `schedule:work` | each pass's task timings into the registry, when one is bound |
//!
//! # Configuration first, then the environment
//!
//! An application that reads `telemetry.*` in its own `config/` keeps doing so,
//! and what it set wins. Where it set nothing, the conventional variables are
//! read — `TELEMETRY_ENABLED`, and the OpenTelemetry SDK's own
//! `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME` and
//! `OTEL_TRACES_SAMPLER_ARG` — so a deployment configured for any other
//! OpenTelemetry SDK is configured for this one.
//!
//! # Nothing is installed twice
//!
//! A subscriber the application installed itself is left alone, exporter and
//! all: replacing somebody's logging is worse than not installing ours. A
//! `Trace` middleware the application registered is not joined by a second.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rainier_config::{Config, Env};
use rainier_metrics::{labels, Metrics};
use rainier_middleware::Middleware;
use rainier_queue::{JobTiming, QueueObserver};
use rainier_scheduler::RunSummary;
use rainier_telemetry::{LogFormat, Otlp};

use crate::keys;
use crate::observability::TelemetrySettings;

impl TelemetrySettings {
    /// [`from_config`](Self::from_config), falling back to the conventional
    /// environment variables for anything the configuration does not set.
    pub fn from_config_and_env(config: &Config, env: &Env) -> Self {
        let mut settings = Self::from_config(config);

        if config.get(keys::TELEMETRY_ENABLED).is_none() {
            settings.enabled = env.bool("TELEMETRY_ENABLED", false);
        }
        if settings.endpoint.is_none() {
            settings.endpoint =
                env.get("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|value| !value.trim().is_empty());
        }
        if config.get(keys::TELEMETRY_SERVICE_NAME).is_none() {
            if let Some(name) = env.get("OTEL_SERVICE_NAME").filter(|name| !name.is_empty()) {
                settings.service_name = name;
            }
        }
        if config.get(keys::TELEMETRY_SAMPLE_RATIO).is_none() {
            if let Some(ratio) =
                env.get("OTEL_TRACES_SAMPLER_ARG").and_then(|ratio| ratio.trim().parse().ok())
            {
                settings.sample_ratio = ratio;
            }
        }

        settings
    }

    /// Install logging and, when spans are exported, the OTLP layer beside
    /// it — one subscriber.
    ///
    /// `false` when a subscriber was already installed, in which case nothing
    /// is built: an exporter nobody's subscriber feeds is a background task
    /// and a socket for nothing. When the exporter cannot be built, logging is
    /// still installed and the failure is printed — tracing is not worth a
    /// boot failure, and a service that will not start is worse than one that
    /// is not traced.
    pub fn install(&self, environment: &str, filter: &str) -> bool {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::{fmt, EnvFilter, Layer};

        if !self.exports() {
            return self.install_logging(environment, filter);
        }
        if tracing::dispatcher::has_been_set() {
            return false;
        }

        let endpoint = self.endpoint.clone().unwrap_or_default();
        let otlp = Otlp::new(endpoint, self.service_name.clone())
            .environment(environment)
            .sample_ratio(self.sample_ratio);

        let exporter = match otlp.layer() {
            Ok(layer) => layer,
            Err(error) => {
                eprintln!("telemetry: spans will not be exported: {}", error.message());
                return self.install_logging(environment, filter);
            }
        };

        let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
        let lines = match self.log_format.resolve(environment) {
            LogFormat::Json => {
                fmt::layer().json().flatten_event(true).with_current_span(true).boxed()
            }
            LogFormat::Compact => fmt::layer().compact().boxed(),
            LogFormat::Pretty | LogFormat::Auto => fmt::layer().boxed(),
        };

        tracing_subscriber::registry().with(filter).with(lines).with(exporter).try_init().is_ok()
    }
}

/// Put [`Trace`](rainier_telemetry::Trace) outermost in `global`, unless it is off or already there.
///
/// Outermost so that every log line — including the ones from middleware that
/// rejected the request — carries the trace id, and so that the outbound
/// calls and jobs a request causes inherit its trace.
pub fn install_trace_middleware(
    global: &mut Vec<Arc<dyn Middleware>>,
    settings: &TelemetrySettings,
) {
    let Some(trace) = settings.middleware() else { return };

    // By its label: an application that registered its own `Trace` — with its
    // own sampling or header choices — keeps exactly that one.
    if global.iter().any(|stage| stage.name() == "Trace") {
        return;
    }

    global.insert(0, Arc::new(trace));
}

/// Flush spans the batch exporter is still holding. Call before exiting.
pub fn shutdown() {
    rainier_telemetry::shutdown();
}

/// A [`QueueObserver`] over the application's `Metrics` registry.
///
/// | Metric | Type | Labels |
/// |---|---|---|
/// | `queue_job_duration_seconds` | histogram | queue, job, outcome |
/// | `queue_job_wait_seconds` | histogram | queue, job |
/// | `queue_reserve_duration_seconds` | histogram | queue |
/// | `queue_jobs_total` | counter | queue, job, outcome |
/// | `queue_jobs_in_flight` | gauge | queue |
/// | `queue_depth` | gauge | queue |
/// | `queue_pod_start_seconds` | histogram | queue, job |
/// | `queue_pod_run_seconds` | histogram | queue, job |
///
/// `job` is the job's **type name**, never its id, and `outcome` is one of
/// five values — so the series count is jobs × queues × five, fixed by the
/// code rather than by traffic.
pub struct QueueMetrics {
    metrics: Arc<Metrics>,
    in_flight: Mutex<HashMap<String, i64>>,
}

/// Wait buckets reach to two hours: a backlog is measured in minutes, and a
/// histogram whose top bucket is ten seconds says only "more than ten".
const WAIT_BUCKETS: [f64; 14] =
    [0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0];

/// Job and pod durations: from milliseconds to an hour.
const DURATION_BUCKETS: [f64; 14] =
    [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 60.0, 300.0, 3600.0];

impl QueueMetrics {
    /// Declare the queue metrics on `metrics`.
    pub fn new(metrics: Arc<Metrics>) -> Self {
        metrics.histogram_with(
            "queue_job_duration_seconds",
            "How long jobs ran, by outcome",
            DURATION_BUCKETS,
        );
        metrics.histogram_with(
            "queue_job_wait_seconds",
            "How long jobs waited between becoming runnable and being reserved",
            WAIT_BUCKETS,
        );
        metrics.histogram(
            "queue_reserve_duration_seconds",
            "How long the driver took to hand a job over",
        );
        metrics.counter("queue_jobs_total", "Jobs finished, by outcome");
        metrics.gauge("queue_jobs_in_flight", "Jobs this process is running now");
        metrics.gauge("queue_depth", "Jobs waiting, as last sampled");
        metrics.histogram_with(
            "queue_pod_start_seconds",
            "Kubernetes jobs: creating the Job to its container starting",
            WAIT_BUCKETS,
        );
        metrics.histogram_with(
            "queue_pod_run_seconds",
            "Kubernetes jobs: how long the container ran",
            DURATION_BUCKETS,
        );
        Self { metrics, in_flight: Mutex::new(HashMap::new()) }
    }

    fn in_flight(&self, queue: &str, delta: i64) {
        let value = {
            let mut counts = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            let count = counts.entry(queue.to_string()).or_insert(0);
            *count = (*count + delta).max(0);
            *count
        };
        self.metrics.set("queue_jobs_in_flight", labels([("queue", queue)]), value as f64);
    }
}

impl QueueObserver for QueueMetrics {
    fn started(&self, queue: &str, _job: &str) {
        self.in_flight(queue, 1);
    }

    fn finished(&self, timing: &JobTiming) {
        self.in_flight(&timing.queue, -1);

        let outcome = timing.outcome.as_str();
        let by_job = || labels([("queue", timing.queue.as_str()), ("job", timing.job.as_str())]);
        let by_outcome = || {
            labels([
                ("queue", timing.queue.as_str()),
                ("job", timing.job.as_str()),
                ("outcome", outcome),
            ])
        };

        self.metrics.increment("queue_jobs_total", by_outcome());
        self.metrics.observe(
            "queue_job_duration_seconds",
            by_outcome(),
            timing.handle.as_secs_f64(),
        );
        self.metrics.observe(
            "queue_reserve_duration_seconds",
            labels([("queue", timing.queue.as_str())]),
            timing.reserve.as_secs_f64(),
        );
        if let Some(wait) = timing.wait {
            self.metrics.observe("queue_job_wait_seconds", by_job(), wait.as_secs_f64());
        }
        if let Some(start) = timing.pod_start {
            self.metrics.observe("queue_pod_start_seconds", by_job(), start.as_secs_f64());
        }
        if let Some(run) = timing.pod_run {
            self.metrics.observe("queue_pod_run_seconds", by_job(), run.as_secs_f64());
        }
    }

    fn depth(&self, queue: &str, depth: u64) {
        self.metrics.set("queue_depth", labels([("queue", queue)]), depth as f64);
    }
}

impl std::fmt::Debug for QueueMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueMetrics").finish_non_exhaustive()
    }
}

/// Record one scheduler pass into `metrics`.
///
/// | Metric | Type | Labels |
/// |---|---|---|
/// | `scheduler_task_runs_total` | counter | task, outcome |
/// | `scheduler_task_duration_seconds` | histogram | task, outcome |
///
/// `outcome` is `ran`, `failed`, or `skipped_` and the reason —
/// `skipped_still_running` is the overlap guard turning a run away, which is
/// the number to alert on when a task has started taking longer than its
/// interval. Task names are declared in code, so the label is bounded.
pub fn record_schedule(metrics: &Metrics, summary: &RunSummary) {
    metrics.counter("scheduler_task_runs_total", "Scheduled tasks considered, by outcome");
    metrics.histogram_with(
        "scheduler_task_duration_seconds",
        "How long scheduled tasks took, by outcome",
        DURATION_BUCKETS,
    );

    let outcome_of = |name: &str| -> String {
        if summary.failed.iter().any(|(task, _)| task == name) {
            "failed".to_string()
        } else if let Some((_, why)) = summary.skipped.iter().find(|(task, _)| task == name) {
            format!("skipped_{}", why.label())
        } else {
            "ran".to_string()
        }
    };

    for (task, took) in &summary.durations {
        let outcome = outcome_of(task);
        let series = labels([("task", task.as_str()), ("outcome", outcome.as_str())]);
        metrics.increment("scheduler_task_runs_total", series.clone());
        metrics.observe("scheduler_task_duration_seconds", series, took.as_secs_f64());
    }
}

/// Serve `metrics` at `GET /metrics` on `address` — for a process with no
/// HTTP server of its own, which is what a queue worker is.
///
/// Deliberately minimal: one route, the text format, `Connection: close`. It
/// is a scrape target, not an application server, and anything it does not
/// recognise is a 404. Runs until the returned task is aborted or the
/// process exits.
pub async fn serve_metrics(
    address: std::net::SocketAddr,
    metrics: Arc<Metrics>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(address).await?;
    Ok(tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { continue };
            let metrics = Arc::clone(&metrics);
            tokio::spawn(async move {
                let mut head = [0u8; 1024];
                let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut head))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(0);
                let request = String::from_utf8_lossy(&head[..read]);

                let response = if request.starts_with("GET /metrics ") {
                    let body = metrics.render();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; version=0.0.4; \
                         charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_queue::JobOutcome;
    use rainier_telemetry::Trace;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_map(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string()))).isolated()
    }

    #[test]
    fn the_environment_turns_it_on_when_the_configuration_says_nothing() {
        let settings = TelemetrySettings::from_config_and_env(
            &Config::new(),
            &env(&[
                ("TELEMETRY_ENABLED", "true"),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317"),
                ("OTEL_SERVICE_NAME", "billing-worker"),
                ("OTEL_TRACES_SAMPLER_ARG", "0.25"),
            ]),
        );

        assert!(settings.enabled);
        assert!(settings.exports());
        assert_eq!(settings.service_name, "billing-worker");
        assert_eq!(settings.sample_ratio, 0.25);
    }

    #[test]
    fn what_the_configuration_set_wins_over_the_environment() {
        let config = Config::new();
        config.set(keys::TELEMETRY_ENABLED, false).unwrap();
        config.set(keys::TELEMETRY_SERVICE_NAME, "api".to_string()).unwrap();

        let settings = TelemetrySettings::from_config_and_env(
            &config,
            &env(&[("TELEMETRY_ENABLED", "true"), ("OTEL_SERVICE_NAME", "other")]),
        );

        assert!(!settings.enabled);
        assert_eq!(settings.service_name, "api");
    }

    #[test]
    fn the_trace_middleware_goes_outermost_once() {
        let mut settings = TelemetrySettings::from_config(&Config::new());
        let mut global: Vec<Arc<dyn Middleware>> = vec![Arc::new(Trace::new())];
        global.clear();

        install_trace_middleware(&mut global, &settings);
        assert!(global.is_empty(), "off means off");

        settings.enabled = true;
        install_trace_middleware(&mut global, &settings);
        install_trace_middleware(&mut global, &settings);
        assert_eq!(global.len(), 1, "never twice");
        assert_eq!(global[0].name(), "Trace");
    }

    fn timing(outcome: JobOutcome) -> JobTiming {
        JobTiming {
            queue: "default".into(),
            driver: "redis".into(),
            job: "mail.welcome".into(),
            attempt: 1,
            outcome,
            wait: Some(Duration::from_secs(12)),
            reserve: Duration::from_millis(3),
            handle: Duration::from_millis(250),
            backoff: None,
            pod_start: None,
            pod_run: None,
        }
    }

    #[test]
    fn a_job_lands_in_every_queue_metric_with_bounded_labels() {
        let metrics = Arc::new(Metrics::new());
        let observer = QueueMetrics::new(Arc::clone(&metrics));

        observer.started("default", "mail.welcome");
        assert_eq!(
            metrics.value("queue_jobs_in_flight", &labels([("queue", "default")])),
            Some(1.0)
        );

        observer.finished(&timing(JobOutcome::Processed));
        observer.finished(&timing(JobOutcome::TimedOut));
        observer.depth("default", 42);

        let processed =
            labels([("queue", "default"), ("job", "mail.welcome"), ("outcome", "processed")]);
        let timed_out =
            labels([("queue", "default"), ("job", "mail.welcome"), ("outcome", "timed_out")]);
        assert_eq!(metrics.value("queue_jobs_total", &processed), Some(1.0));
        assert_eq!(metrics.value("queue_jobs_total", &timed_out), Some(1.0));
        assert_eq!(metrics.value("queue_job_duration_seconds", &processed), Some(1.0));
        assert_eq!(
            metrics.value(
                "queue_job_wait_seconds",
                &labels([("queue", "default"), ("job", "mail.welcome")])
            ),
            Some(2.0),
            "two observations"
        );
        assert_eq!(metrics.value("queue_depth", &labels([("queue", "default")])), Some(42.0));
        assert_eq!(
            metrics.value("queue_jobs_in_flight", &labels([("queue", "default")])),
            Some(0.0),
            "never below zero"
        );
    }

    #[test]
    fn a_scheduler_pass_is_counted_and_timed_by_outcome() {
        let metrics = Metrics::new();
        let mut summary = RunSummary::default();
        summary.ran.push("backup".into());
        summary.skipped.push(("report".into(), rainier_scheduler::Skipped::StillRunning));
        summary.durations.push(("backup".into(), Duration::from_secs(2)));
        summary.durations.push(("report".into(), Duration::from_millis(1)));

        record_schedule(&metrics, &summary);

        assert_eq!(
            metrics.value(
                "scheduler_task_runs_total",
                &labels([("task", "backup"), ("outcome", "ran")])
            ),
            Some(1.0)
        );
        assert_eq!(
            metrics.value(
                "scheduler_task_runs_total",
                &labels([("task", "report"), ("outcome", "skipped_still_running")])
            ),
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn the_worker_scrape_endpoint_serves_the_registry() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let metrics = Arc::new(Metrics::new());
        metrics.counter("probe_total", "a probe");
        metrics.increment("probe_total", labels([("a", "b")]));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let server = serve_metrics(address, metrics).await.unwrap();

        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(b"GET /metrics HTTP/1.1\r\nhost: x\r\n\r\n").await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        server.abort();

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("probe_total{a=\"b\"} 1"), "{response}");
    }
}
