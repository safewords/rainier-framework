//! What recording costs a request, on the thread that serves it.
//!
//! Not a pass/fail test — it prints numbers, so it is `#[ignore]`d. Run it
//! with
//!
//! ```text
//! cargo test -p rainier-lookout --release --test hot_path_cost -- --ignored --nocapture
//! ```
//!
//! Each request is a unit of work publishing what the framework publishes
//! for one: twenty `db.query` activities of five shapes, three `cache.get`
//! and the `http.server.request` root, built by the same constructors the
//! database, cache and server use, on a bus with nothing but Lookout
//! subscribed to it. Lookout hears them on its subscriber's own thread; the
//! timed part is the request thread's share alone. The subscriber's catching
//! up, and the writer's flush to a memory store, are measured apart.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rainier_cache::events::CacheOperation;
use rainier_database::events::QueryExecuted;
use rainier_events::activity::publish_activity_with;
use rainier_events::{Dispatcher, Unit, UnitKind};
use rainier_lookout::entry::EntryType;
use rainier_lookout::{Lookout, LookoutConfig, MemoryStore};
use rainier_orm::Dialect;
use rainier_server::events::RequestHandled;

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

const SHAPES: &[&str] = &[
    "SELECT `posts`.* FROM `posts` WHERE `posts`.`id` = ? LIMIT ?",
    "SELECT `users`.* FROM `users` WHERE `users`.`id` IN (?, ?, ?, ?)",
    "SELECT COUNT(*) AS `count` FROM `comments` WHERE `comments`.`post_id` = ?",
    "SELECT `tags`.* FROM `tags` INNER JOIN `post_tag` ON `post_tag`.`tag_id` = `tags`.`id` WHERE `post_tag`.`post_id` = ?",
    "UPDATE `posts` SET `views` = `views` + ? WHERE `posts`.`id` = ?",
];

#[inline(never)]
fn request(events: &Arc<Dispatcher>, status: u16) {
    let unit = Unit::on(events, UnitKind::Request, None, Some("/posts/42".to_string()));
    unit.set_trace(Some(TRACE), None);
    {
        let _entered = unit.enter();
        let started = Instant::now();
        for i in 0..20 {
            let ran = Instant::now();
            publish_activity_with(|| {
                QueryExecuted::new(
                    Dialect::MySql,
                    SHAPES[i % SHAPES.len()],
                    ran.elapsed(),
                    Some(1),
                    false,
                )
            });
        }
        for _ in 0..3 {
            let ran = Instant::now();
            publish_activity_with(|| CacheOperation {
                kind: "cache.get",
                operation: "get",
                otel_name: "cache get",
                store: "redis".to_string(),
                key: Some("posts:42".to_string()),
                hit: Some(true),
                duration: ran.elapsed(),
                error: None,
            });
        }
        publish_activity_with(|| RequestHandled {
            method: "GET".to_string(),
            path: "/posts/42".to_string(),
            route: Some("/posts/{post}".to_string()),
            route_name: Some("posts.show".to_string()),
            status,
            user_agent: None,
            trace_id: Some(TRACE.to_string()),
            duration: started.elapsed(),
        });
    }
    // The unit's end: `UnitCompleted`, which closes the batch.
    drop(unit);
}

/// A bus with `lookout` (if any) subscribed.
fn bus(lookout: Option<&Lookout>) -> Arc<Dispatcher> {
    let events = Arc::new(Dispatcher::new());
    if let Some(lookout) = lookout {
        lookout.subscribe(&events);
    }
    events
}

fn print(label: &str, per: Duration) {
    println!("{label:<58} {:>8.2} µs/request", per.as_secs_f64() * 1e6);
}

/// The request thread's share per request; then, apart, the subscriber
/// catching up and the writer's flush.
async fn measure(label: &str, lookout: Option<&Lookout>, status: u16, n: u32) -> Duration {
    let events = bus(lookout);
    for _ in 0..200 {
        request(&events, status);
    }
    events.settled().await;
    if let Some(lookout) = lookout {
        lookout.flush().await;
    }
    let t = Instant::now();
    for _ in 0..n {
        request(&events, status);
    }
    let per = t.elapsed() / n;
    print(label, per);
    if let Some(lookout) = lookout.filter(|l| l.is_enabled()) {
        let t = Instant::now();
        events.settled().await;
        let heard = t.elapsed();
        let t = Instant::now();
        lookout.flush().await;
        let written = t.elapsed();
        print("  … the subscriber's thread, after", heard / n);
        print("  … the writer's flush, after", written / n);
    }
    per
}

fn config(local: bool) -> LookoutConfig {
    let mut config = LookoutConfig::default().enabled(true).local(local);
    // Room for every batch of the run: nothing is dropped for a full channel.
    config.ingest.channel_capacity = 200_000;
    config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "prints what recording costs; run with --ignored --nocapture"]
async fn what_recording_costs_a_request() {
    let n = 20_000;
    let store = Arc::new(MemoryStore::new(100));
    rainier_events::code_trace::reset();

    measure("nothing subscribed", None, 200, n).await;

    let off = Lookout::new(LookoutConfig::default(), store.clone());
    measure("Lookout off", Some(&off), 200, n).await;

    let production = Lookout::new(config(false), store.clone());
    measure("production, an ordinary request (not kept)", Some(&production), 200, n).await;

    for threads in [1, 4] {
        let mut sharded = config(false);
        sharded.ingest.threads = threads;
        let sharded = Lookout::new(sharded, store.clone());
        let label = format!("production, Lookout on {threads} thread(s)");
        measure(&label, Some(&sharded), 200, n).await;
    }

    let local = Lookout::new(config(true), store.clone());
    measure("local, every request kept", Some(&local), 200, n).await;

    // Every query over its threshold: kept, with a code trace on each shape.
    let mut slow = config(false);
    slow.watchers.entry(EntryType::Query).or_default().slow_ms = Some(0);
    let slow = Lookout::new(slow, store.clone());
    measure("production, 20 slow queries with code traces", Some(&slow), 200, 2_000).await;

    // The code-trace request is process-wide: withdraw the one above.
    rainier_events::code_trace::reset();
    let mut untraced = config(false);
    untraced.watchers.entry(EntryType::Query).or_default().slow_ms = Some(0);
    untraced.capture.code_traces = false;
    let untraced = Lookout::new(untraced, store.clone());
    measure("production, 20 slow queries, code traces off", Some(&untraced), 200, 2_000).await;
}

/// A slow query's `code.trace` names the code that ran it — captured by the
/// publisher, symbolised by the writer — and one batch walks at most
/// `MAX_CODE_TRACES_PER_BATCH` stacks however many slow queries it runs.
#[tokio::test]
async fn slow_queries_get_one_code_trace_per_shape() {
    use rainier_lookout::recorder::MAX_CODE_TRACES_PER_BATCH;
    use rainier_lookout::Store as _;

    let store = Arc::new(MemoryStore::new(100));
    let mut slow = config(false);
    slow.watchers.entry(EntryType::Query).or_default().slow_ms = Some(0);
    let lookout = Lookout::new(slow, store.clone());
    let events = bus(Some(&lookout));
    request(&events, 200);
    events.settled().await;
    lookout.flush().await;

    let listed = store.batches(Default::default()).await.unwrap();
    let batch = store.batch(&listed[0].id).await.unwrap().unwrap();
    let traces: Vec<&serde_json::Value> = batch
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Query)
        .filter_map(|e| e.content.get("code.trace"))
        .collect();
    // Twenty slow queries of five shapes: one stack per shape, not twenty —
    // a loop running the same query captures its stack once.
    assert_eq!(traces.len(), SHAPES.len().min(MAX_CODE_TRACES_PER_BATCH), "{traces:?}");
    let functions: Vec<&str> = traces[0]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.get("function").and_then(|f| f.as_str()))
        .collect();
    assert!(functions.iter().any(|f| f.ends_with("hot_path_cost::request")), "{functions:?}");
}
