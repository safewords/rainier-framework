//! What the queue publishes on the bus: `JobQueued` per dispatch, and per
//! attempt a unit of work whose root activity is `JobRan` — joined into a
//! request, as an operation, when the job runs inside one.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rainier_container::Container;
use rainier_events::{Context, Dispatcher, Unit, UnitCompleted, UnitKind};
use rainier_queue::{
    Job, JobContext, JobProcessed, JobQueued, JobRan, JobRegistry, MemoryQueue, Outcome, Queue,
    QueueManager, QueuedJob, SyncQueue, Worker,
};
use serde::{Deserialize, Serialize};

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

/// An event the job publishes, to see where its work lands.
#[derive(Debug, Clone)]
struct Touched;

#[derive(Serialize, Deserialize)]
struct Touch;

#[rainier_queue::async_trait]
impl Job for Touch {
    const NAME: &'static str = "test.touch";

    async fn handle(&self, _: &JobContext) -> rainier_support::Result<()> {
        rainier_events::publish(Touched);
        Ok(())
    }
}

/// What a subscriber saw: the activity or event, and the context it came in.
#[derive(Debug, Clone)]
struct Seen {
    what: String,
    unit: Option<(u64, UnitKind, Option<String>, Option<String>)>,
    parent: Option<u64>,
    operation: Option<u64>,
    fields: Vec<(String, String)>,
}

impl Seen {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn unit_of(cx: &Context) -> Option<(u64, UnitKind, Option<String>, Option<String>)> {
    cx.unit()
        .map(|u| (u.id(), u.kind(), u.name().map(str::to_string), u.trace_id().map(str::to_string)))
}

fn watch(events: &Arc<Dispatcher>) -> Arc<Mutex<Vec<Seen>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (a, b, c, d) = (Arc::clone(&seen), Arc::clone(&seen), Arc::clone(&seen), Arc::clone(&seen));
    events
        .subscriber("test")
        .on_activity(move |activity, cx| {
            a.lock().unwrap().push(Seen {
                what: activity.kind().to_string(),
                unit: unit_of(cx),
                parent: cx.parent(),
                operation: activity.operation(),
                fields: activity
                    .collect_fields()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.render()))
                    .collect(),
            });
            Ok(())
        })
        .on(move |_: &Touched, cx| {
            b.lock().unwrap().push(Seen {
                what: "touched".into(),
                unit: unit_of(cx),
                parent: cx.parent(),
                operation: None,
                fields: Vec::new(),
            });
            Ok(())
        })
        .on(move |_: &JobProcessed, cx| {
            c.lock().unwrap().push(Seen {
                what: "processed".into(),
                unit: unit_of(cx),
                parent: cx.parent(),
                operation: None,
                fields: Vec::new(),
            });
            Ok(())
        })
        .on(move |done: &UnitCompleted, _| {
            d.lock().unwrap().push(Seen {
                what: "completed".into(),
                unit: Some((done.id, done.kind, done.name.clone(), done.trace_id.clone())),
                parent: None,
                operation: None,
                fields: Vec::new(),
            });
            Ok(())
        })
        .start();
    seen
}

async fn until(events: &Arc<Dispatcher>, seen: &Arc<Mutex<Vec<Seen>>>, what: &str, count: usize) {
    for _ in 0..2000 {
        events.settled().await;
        if seen.lock().unwrap().iter().filter(|s| s.what == what).count() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("no {what}: {:?}", seen.lock().unwrap());
}

fn find<'a>(seen: &'a [Seen], what: &str) -> &'a Seen {
    seen.iter().find(|s| s.what == what).unwrap_or_else(|| panic!("no {what}: {seen:?}"))
}

fn worker(queue: Arc<MemoryQueue>, events: &Arc<Dispatcher>) -> Worker {
    Worker::new(
        queue as Arc<dyn Queue>,
        Arc::new(JobRegistry::new().with::<Touch>()),
        Arc::new(Container::new()),
    )
    .with_events(Arc::clone(events))
}

#[tokio::test]
async fn an_attempt_is_a_job_unit_in_the_trace_it_carries_with_job_ran_as_its_root() {
    let events = Arc::new(Dispatcher::new());
    let seen = watch(&events);

    let queue = Arc::new(MemoryQueue::new());
    let mut job = QueuedJob::from_job(&Touch).unwrap();
    job.traceparent = Some(TRACEPARENT.into());
    queue.push(job).await.unwrap();

    assert_eq!(worker(Arc::clone(&queue), &events).run_next().await.unwrap(), Outcome::Processed);
    until(&events, &seen, "completed", 1).await;

    let seen = seen.lock().unwrap().clone();
    let ran = find(&seen, "queue.process");
    let (unit, kind, name, trace) = ran.unit.clone().expect("inside the attempt's unit");
    assert_eq!(kind, UnitKind::Job);
    assert_eq!(name.as_deref(), Some("test.touch"));
    assert_eq!(trace.as_deref(), Some(TRACE_ID), "the trace the job carried");
    assert_eq!(ran.operation, None, "its own unit, not an operation in another");

    assert_eq!(ran.field("otel.name"), Some("process test.touch"));
    assert_eq!(ran.field("otel.kind"), Some("consumer"));
    assert_eq!(ran.field("messaging.system"), Some("memory"));
    assert_eq!(ran.field("messaging.operation.type"), Some("process"));
    assert_eq!(ran.field("messaging.destination.name"), Some("default"));
    assert_eq!(ran.field("rainier.job.name"), Some("test.touch"));
    assert_eq!(ran.field("rainier.job.attempt"), Some("1"));
    assert_eq!(ran.field("rainier.job.outcome"), Some("processed"));
    assert_eq!(ran.field("rainier.job.runner"), Some("in_process"));
    assert_eq!(ran.field("trace_id"), Some(TRACE_ID));
    assert!(ran.field("rainier.queue.reserve_ms").is_some());
    assert_eq!(ran.field("otel.status_code"), None);

    // What the job and the worker published landed in the same unit.
    assert_eq!(find(&seen, "touched").unit.as_ref().map(|u| u.0), Some(unit));
    assert_eq!(find(&seen, "processed").unit.as_ref().map(|u| u.0), Some(unit));
    let done = find(&seen, "completed");
    assert_eq!(done.unit.as_ref().map(|u| u.0), Some(unit));
    let position = |what: &str| seen.iter().position(|s| s.what == what).unwrap();
    assert!(position("queue.process") < position("completed"), "{seen:?}");
}

#[tokio::test]
async fn a_job_run_inside_a_request_joins_it_as_an_operation() {
    let events = Arc::new(Dispatcher::new());
    let seen = watch(&events);

    let queue = Arc::new(MemoryQueue::new());
    queue.push(QueuedJob::from_job(&Touch).unwrap()).await.unwrap();

    let request = Unit::on(&events, UnitKind::Request, None, Some("/".into()));
    let worker = worker(Arc::clone(&queue), &events);
    request.scope(worker.run_next()).await.unwrap();
    let request_id = request.id();
    drop(request);
    until(&events, &seen, "completed", 1).await;

    let seen = seen.lock().unwrap().clone();
    let ran = find(&seen, "queue.process");
    assert_eq!(ran.unit.as_ref().map(|u| (u.0, u.1)), Some((request_id, UnitKind::Request)));
    let operation = ran.operation.expect("the job ran as an operation in the request");
    let touched = find(&seen, "touched");
    assert_eq!(touched.parent, Some(operation), "what the job did nests under it");
    assert_eq!(touched.unit.as_ref().map(|u| u.0), Some(request_id));
    assert_eq!(seen.iter().filter(|s| s.what == "completed").count(), 1, "no unit of its own");
}

#[tokio::test]
async fn the_sync_driver_in_a_request_is_an_operation_with_its_job_ran() {
    let events = Arc::new(Dispatcher::new());
    let seen = watch(&events);
    let registry = Arc::new(JobRegistry::new().with::<Touch>());
    let manager = QueueManager::new(
        Arc::new(SyncQueue::new(Arc::clone(&registry), Arc::new(Container::new()))),
        registry,
    );

    let request = Unit::on(&events, UnitKind::Request, None, Some("/".into()));
    request.set_trace(None, Some(TRACEPARENT));
    request.scope(manager.dispatch(Touch)).await.unwrap();
    drop(request);
    until(&events, &seen, "completed", 1).await;

    let seen = seen.lock().unwrap().clone();
    let ran = find(&seen, "queue.process");
    assert_eq!(ran.field("messaging.system"), Some("sync"));
    assert_eq!(ran.field("rainier.job.outcome"), Some("processed"));
    assert_eq!(ran.field("trace_id"), Some(TRACE_ID));
    let operation = ran.operation.expect("an operation in the request");
    assert_eq!(find(&seen, "touched").parent, Some(operation));

    let queued = find(&seen, "queue.publish");
    assert_eq!(queued.field("messaging.system"), Some("sync"));
    assert_eq!(queued.field("rainier.job.name"), Some("test.touch"));
}

#[tokio::test]
async fn a_dispatch_publishes_job_queued_and_the_job_carries_the_units_trace() {
    let events = Arc::new(Dispatcher::new());
    let seen = watch(&events);
    let queue = Arc::new(MemoryQueue::new());
    let manager = QueueManager::new(
        Arc::clone(&queue) as Arc<dyn Queue>,
        Arc::new(JobRegistry::new().with::<Touch>()),
    );

    // A request in a trace queues a job...
    let request = Unit::on(&events, UnitKind::Request, None, Some("/".into()));
    request.set_trace(None, Some(TRACEPARENT));
    let id = request.scope(manager.dispatch(Touch)).await.unwrap().expect("an id");
    let request_id = request.id();
    drop(request);

    let pending = queue.pending("default");
    let carried = pending[0].traceparent.clone().expect("the job carries a traceparent");
    assert!(carried.contains(TRACE_ID), "{carried}");

    // ...and the worker's attempt is in that trace.
    worker(Arc::clone(&queue), &events).run_next().await.unwrap();
    until(&events, &seen, "completed", 2).await;

    let seen = seen.lock().unwrap().clone();
    let queued = find(&seen, "queue.publish");
    assert_eq!(queued.unit.as_ref().map(|u| u.0), Some(request_id), "in the request's unit");
    assert_eq!(queued.field("otel.name"), Some("publish default"));
    assert_eq!(queued.field("otel.kind"), Some("producer"));
    assert_eq!(queued.field("messaging.system"), Some("memory"));
    assert_eq!(queued.field("messaging.operation.type"), Some("send"));
    assert_eq!(queued.field("messaging.destination.name"), Some("default"));
    assert_eq!(queued.field("messaging.message.id"), Some(id.as_str()));
    assert_eq!(queued.field("rainier.job.name"), Some("test.touch"));

    let ran = find(&seen, "queue.process");
    assert_eq!(ran.unit.as_ref().and_then(|u| u.3.clone()).as_deref(), Some(TRACE_ID));
}

#[tokio::test]
async fn with_nobody_listening_nothing_is_built() {
    let events = Arc::new(Dispatcher::new());
    assert!(!events.has_activity_subscribers::<JobRan>());
    assert!(!events.has_activity_subscribers::<JobQueued>());

    let queue = Arc::new(MemoryQueue::new());
    let manager = QueueManager::new(
        Arc::clone(&queue) as Arc<dyn Queue>,
        Arc::new(JobRegistry::new().with::<Touch>()),
    );
    manager.dispatch(Touch).await.unwrap();
    assert_eq!(worker(queue, &events).run_next().await.unwrap(), Outcome::Processed);

    // And a fake records each, once.
    let fake = Arc::new(Dispatcher::fake());
    let queue = Arc::new(MemoryQueue::new());
    queue.push(QueuedJob::from_job(&Touch).unwrap()).await.unwrap();
    worker(queue, &fake).run_next().await.unwrap();
    fake.assert_dispatched_times::<JobProcessed>(1);
    fake.assert_dispatched_times::<JobRan>(1);
}
