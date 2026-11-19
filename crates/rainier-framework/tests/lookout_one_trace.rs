//! End to end on the bus: a request queues a job, that job queues another,
//! a worker runs both — and Lookout shows the three as one trace, each its
//! own batch, each holding what it did.
//!
//! Nothing here reads spans: the `telemetry` feature need not be on. Its own
//! binary: booting installs the process-wide bus.

#![cfg(feature = "lookout")]

use std::sync::{Arc, OnceLock};

use rainier_container::Container;
use rainier_events::Dispatcher;
use rainier_framework::lookout::{BatchKind, BatchQuery, EntryType, Lookout, Store};
use rainier_framework::testing::TestApp;
use rainier_framework::Rainier;
use rainier_queue::{
    Job, JobContext, JobRegistry, MemoryQueue, Outcome, Queue, QueueManager, Worker,
};
use serde::{Deserialize, Serialize};

const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";

/// The queue the route and the first job dispatch onto.
static QUEUE: OnceLock<Arc<QueueManager>> = OnceLock::new();

fn queue() -> &'static Arc<QueueManager> {
    QUEUE.get().expect("the test sets the queue first")
}

/// Queued by the request; queues [`SendReceipt`].
#[derive(Serialize, Deserialize)]
struct ChargeOrder {
    order_id: u64,
}

#[rainier_queue::async_trait]
impl Job for ChargeOrder {
    const NAME: &'static str = "orders.charge";

    async fn handle(&self, _: &JobContext) -> rainier_support::Result<()> {
        queue().dispatch(SendReceipt { order_id: self.order_id }).await?;
        Ok(())
    }
}

/// Queued by [`ChargeOrder`].
#[derive(Serialize, Deserialize)]
struct SendReceipt {
    order_id: u64,
}

#[rainier_queue::async_trait]
impl Job for SendReceipt {
    const NAME: &'static str = "orders.receipt";

    async fn handle(&self, _: &JobContext) -> rainier_support::Result<()> {
        let _ = self.order_id;
        Ok(())
    }
}

fn registry() -> Arc<JobRegistry> {
    Arc::new(JobRegistry::new().with::<ChargeOrder>().with::<SendReceipt>())
}

#[tokio::test]
async fn a_request_its_job_and_that_jobs_job_are_one_trace_of_three_batches() {
    let memory = Arc::new(MemoryQueue::new());
    assert!(
        QUEUE
            .set(Arc::new(QueueManager::new(Arc::clone(&memory) as Arc<dyn Queue>, registry())))
            .is_ok(),
        "set once"
    );

    let app = Rainier::new(".")
        .without_facades()
        .without_tracing()
        .with_lookout(|mut config| {
            config.ingest.flush_interval_ms = 3_600_000;
            config.enabled(true).local(true)
        })
        .with_routes(|router| {
            router
                .post("/orders/{order}/checkout", || async {
                    queue().dispatch(ChargeOrder { order_id: 42 }).await.map(|_| "queued")
                })
                .name("orders.checkout");
        })
        .boot()
        .await
        .expect("boots");
    // The request arrives in a trace someone upstream started.
    let app = TestApp::new(app).unwrap().with_header("traceparent", TRACEPARENT);
    let events = app.resolve::<Dispatcher>().unwrap();

    let empty = std::collections::HashMap::<String, String>::new();
    app.post("/orders/42/checkout", &empty).await.assert_ok();

    // A worker — the application's bus, a container of its own — runs the
    // job the request queued, then the job that job queued.
    let worker =
        Worker::new(Arc::clone(&memory) as Arc<dyn Queue>, registry(), Arc::new(Container::new()))
            .with_events(Arc::clone(&events));
    assert_eq!(worker.run_next().await.unwrap(), Outcome::Processed);
    assert_eq!(worker.run_next().await.unwrap(), Outcome::Processed);

    // Every unit's events delivered — and the completions they set off.
    events.settled().await;
    events.settled().await;
    app.resolve::<Lookout>().unwrap().flush().await;

    let store = app.resolve::<Arc<dyn Store>>().unwrap();
    let query = BatchQuery { trace_id: Some(TRACE_ID.into()), ..Default::default() };
    let mut traced = store.batches(query).await.unwrap();
    traced.sort_by_key(|b| b.started_at);
    assert_eq!(traced.len(), 3, "the request and both jobs: {traced:#?}");
    assert!(traced.iter().all(|b| b.trace_id == TRACE_ID));

    let request = &traced[0];
    assert_eq!(request.kind, BatchKind::Request);
    assert_eq!(request.route_name.as_deref(), Some("orders.checkout"));

    let (charge, receipt) = (&traced[1], &traced[2]);
    assert_eq!((charge.kind, receipt.kind), (BatchKind::Job, BatchKind::Job));
    assert!(charge.summary.contains("orders.charge"), "{charge:#?}");
    assert!(receipt.summary.contains("orders.receipt"), "{receipt:#?}");

    // Each batch holds what its unit did: the request and the charge each
    // queued the next job.
    for (batch, queued) in [(request, "orders.charge"), (charge, "orders.receipt")] {
        let full = store.batch(&batch.id).await.unwrap().expect("stored");
        let dispatched = full
            .entries
            .iter()
            .filter(|e| e.entry_type == EntryType::Job)
            .any(|e| e.summary.contains(&format!("dispatched {queued}")));
        assert!(dispatched, "{} should show it queued {queued}: {:#?}", batch.id, full.entries);
    }
}
