//! Passive instrumentation, end to end, offline — no collector needed.
//!
//! What it shows:
//!
//! 1. one request through the real kernel, whose handler reads the cache, runs
//!    a query, calls another service and queues a job — and the spans each of
//!    those opens without a line of tracing code in the handler;
//! 2. the `traceparent` the outbound call carried, written by the HTTP client;
//! 3. the job, processed by a worker, joining the **same trace** as the
//!    request that queued it, with its wait and outcome on its span;
//! 4. the `queue_job_*` metrics the worker recorded into the registry.
//!
//! ```sh
//! cargo run -p rainier-framework --example telemetry --features telemetry
//! ```
//!
//! # In production there is no code at all
//!
//! Everything below that sets up a subscriber, a fake transport and a fake
//! database is here so the example runs on a laptop with nothing installed.
//! A deployment builds with the `telemetry` feature and sets two variables:
//!
//! ```env
//! TELEMETRY_ENABLED=true
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
//! ```
//!
//! and at boot the framework installs the OTLP exporter beside its log
//! subscriber and puts `Trace` outermost in the global middleware — the same
//! spans you see printed here go to the collector instead. A queue worker that
//! should be scraped also sets `WORKER_METRICS_ADDRESS=0.0.0.0:9100`.
//!
//! The database is the in-memory fake connection, because the default build
//! has no SQL driver compiled in; with `sea-orm-executor` and a real
//! connection the `db.query` span is the same.

use std::sync::Arc;
use std::time::Duration;

use rainier_framework::cache::CacheManager;
use rainier_framework::database::testing::{fake_database, MemoryConnection};
use rainier_framework::database::{ColumnRequest, Database, OwnedRow};
use rainier_framework::http::Response;
use rainier_framework::http_client::{FakeTransport, Http};
use rainier_framework::instrumentation::QueueMetrics;
use rainier_framework::metrics::Metrics;
use rainier_framework::queue::{
    Job, JobContext, JobRegistry, MemoryQueue, Queue, QueueManager, QueuedJob, Worker,
};
use rainier_framework::rainier_orm::{ColumnType, Dialect};
use rainier_framework::routing::Req;
use rainier_framework::support::Result;
use rainier_framework::telemetry::TraceContext;
use rainier_framework::testing::TestApp;
use rainier_framework::{keys, Rainier};
use serde::{Deserialize, Serialize};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// Stands in for the exporter: prints each span as it closes — its name and
/// its own fields, including the ones recorded after it opened — indented by
/// how deep it sits in the trace.
struct PrintSpans;

#[derive(Default)]
struct Fields(std::collections::BTreeMap<String, String>);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_string(), format!("{value:?}").trim_matches('"').to_string());
    }
}

impl<S> tracing_subscriber::Layer<S> for PrintSpans
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        attributes.record(&mut fields);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        if let Some(span) = context.span(id) {
            if let Some(fields) = span.extensions_mut().get_mut::<Fields>() {
                values.record(fields);
            }
        }
    }

    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        let Some(span) = context.span(&id) else { return };
        let depth = span.scope().count() - 1;
        let extensions = span.extensions();
        let fields = extensions
            .get::<Fields>()
            .map(|fields| {
                fields.0.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        println!("  {}{}  {}", "  ".repeat(depth), span.name(), fields.join(" "));
    }
}

/// The job the request queues.
#[derive(Serialize, Deserialize)]
struct ShipOrder {
    order: u64,
}

#[rainier_framework::queue::async_trait]
impl Job for ShipOrder {
    const NAME: &'static str = "orders.ship";

    async fn handle(&self, _: &JobContext) -> Result<()> {
        // Anything a job calls joins the trace it was queued in.
        println!(
            "  [job] shipping order {} — trace in force here: {}",
            self.order,
            rainier_framework::telemetry::propagation::current().unwrap_or_default()
        );
        Ok(())
    }
}

/// `GET /orders/{order}` — an ordinary handler. Not one line of it is about
/// tracing.
async fn show(request: Req) -> Result<Response> {
    let order: u64 = request.route_param("order").unwrap_or("0").parse().unwrap_or(0);
    let app = rainier_framework::container::facade_application();

    // A cache read and a write: `cache.get` (with `cache.hit`) and `cache.put`.
    let cache = app.resolve::<CacheManager>()?;
    if cache.get("orders:recent").await?.is_none() {
        cache.put("orders:recent", b"[]", Some(Duration::from_secs(60))).await?;
    }

    // A query: `db.query`, named `SELECT orders`, its text with placeholders
    // only — the bound order id never reaches the span.
    let database = app.resolve::<Database>()?;
    database
        .query("SELECT id FROM orders WHERE id = ?")
        .bind(order as i64)
        .fetch(vec![ColumnRequest::new("id", ColumnType::BigInt)])
        .await?;

    // A call to another service: `http.client.request`, and a `traceparent`
    // header on the wire naming this request's trace.
    let carrier = app.resolve::<FakeTransport>()?;
    Http::get(format!("https://carrier.example.com/v1/rates?order={order}"))
        .through(carrier)
        .send()
        .await?;

    // A dispatch: `queue.publish`, and the trace written onto the job.
    app.resolve::<QueueManager>()?.dispatch(ShipOrder { order }).await?;

    Ok(Response::text(format!("order {order}")))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // In production boot installs the log subscriber with the OTLP exporter
    // in it; here a layer that prints spans stands in for the exporter, at
    // the same `info` level a deployment filters at.
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::INFO)
        .with(PrintSpans)
        .init();

    let queue = Arc::new(MemoryQueue::new());
    let registry = Arc::new(JobRegistry::new().with::<ShipOrder>());
    let carrier = Arc::new(FakeTransport::new());
    carrier.responding(200, r#"{"rate": "4.20"}"#);
    let (database, _) = fake_database(
        MemoryConnection::new(Dialect::Sqlite).returning([OwnedRow::new().with("id", 42_i64)]),
    );

    let app = Rainier::new(".")
        // The subscriber above is the one this example wants; in production
        // boot installs its own, with the exporter in it.
        .without_tracing()
        // In production this is `TELEMETRY_ENABLED=true` in the environment.
        .configure(|config| {
            let _ = config.set(keys::TELEMETRY_ENABLED, true);
        })
        .with_database(database)
        .with_queue(QueueManager::new(Arc::clone(&queue) as Arc<dyn Queue>, Arc::clone(&registry)))
        .with_instance_arc(Arc::clone(&carrier))
        .with_routes(|router| {
            router.get("/orders/{order}", show).name("orders.show");
        })
        .boot()
        .await?;

    println!("\n== 1. one request, through the kernel ==");
    let client = TestApp::new(Arc::clone(&app))?;
    let response = client.get("/orders/42").await;
    let request_trace = response
        .header("traceparent")
        .and_then(|header| TraceContext::parse(header).ok())
        .map(|context| context.trace_id().to_string())
        .unwrap_or_default();
    println!("  [http] {} — trace {request_trace}", response.status());

    println!("\n== 2. what the outbound call carried ==");
    let sent = &carrier.recorded()[0];
    println!("  [carrier] GET {}", sent.url());
    println!("  [carrier] traceparent: {}", sent.header("traceparent").unwrap_or("(none)"));

    println!("\n== 3. the job, processed by a worker ==");
    let queued: Vec<QueuedJob> = queue.pending("default");
    let job_trace = queued[0]
        .traceparent
        .as_deref()
        .and_then(|header| TraceContext::parse(header).ok())
        .map(|context| context.trace_id().to_string())
        .unwrap_or_default();
    println!("  [queue] job carries trace {job_trace}");
    println!("  [queue] same trace as the request: {}", job_trace == request_trace);

    // Let it sit in the queue for a moment, so its span has a wait to report.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // In `queue:work` this observer is installed for you when a `Metrics`
    // registry is bound.
    let metrics = Arc::new(Metrics::new());
    let worker = Worker::new(queue, registry, Arc::clone(app.container()))
        .with_observer(Arc::new(QueueMetrics::new(Arc::clone(&metrics))));
    let outcome = worker.run_next().await?;
    println!("  [worker] {outcome:?}");

    println!("\n== 4. the queue metrics the worker recorded ==");
    for line in metrics.render().lines() {
        let interesting = line.starts_with("queue_jobs_total")
            || line.starts_with("queue_job_wait_seconds_count")
            || line.starts_with("queue_job_duration_seconds_sum")
            || line.starts_with("queue_jobs_in_flight");
        if interesting {
            println!("  {line}");
        }
    }

    Ok(())
}
