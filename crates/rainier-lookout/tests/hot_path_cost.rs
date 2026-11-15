//! What recording costs a request, on the thread that serves it.
//!
//! Not a pass/fail test — it prints numbers, so it is `#[ignore]`d. Run it
//! with
//!
//! ```text
//! cargo test -p rainier-lookout --release --test hot_path_cost -- --ignored --nocapture
//! ```
//!
//! Each request is a `http.server.request` span holding twenty `db.query`
//! spans of five shapes and three `cache.get` spans — the spans the framework
//! emits with its `telemetry` features — run through a `tracing` registry
//! with nothing but Lookout's layer on it. The batches go to a memory store
//! that is flushed outside the timed part, so what is measured is the request
//! thread's share alone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rainier_lookout::entry::EntryType;
use rainier_lookout::{Lookout, LookoutConfig, MemoryStore};
use tracing::field::Empty;
use tracing_subscriber::layer::SubscriberExt;

const SHAPES: &[&str] = &[
    "SELECT `posts`.* FROM `posts` WHERE `posts`.`id` = ? LIMIT ?",
    "SELECT `users`.* FROM `users` WHERE `users`.`id` IN (?, ?, ?, ?)",
    "SELECT COUNT(*) AS `count` FROM `comments` WHERE `comments`.`post_id` = ?",
    "SELECT `tags`.* FROM `tags` INNER JOIN `post_tag` ON `post_tag`.`tag_id` = `tags`.`id` WHERE `post_tag`.`post_id` = ?",
    "UPDATE `posts` SET `views` = `views` + ? WHERE `posts`.`id` = ?",
];

#[inline(never)]
fn request(status: u16) {
    let span = tracing::info_span!(
        "http.server.request",
        otel.name = "GET",
        http.request.method = "GET",
        url.path = "/posts/42",
        http.route = Empty,
        rainier.route.name = Empty,
        http.response.status_code = Empty,
        trace_id = Empty,
    );
    {
        let _entered = span.enter();
        span.record("trace_id", "4bf92f3577b34da6a3ce929d0e0e4736");
        span.record("http.route", "/posts/{post}");
        span.record("rainier.route.name", "posts.show");
        for i in 0..20 {
            let q = tracing::info_span!(
                "db.query",
                db.system.name = "mysql",
                db.query.text = SHAPES[i % SHAPES.len()],
                db.namespace = "app",
                otel.status_code = Empty,
            );
            let _entered = q.enter();
        }
        for _ in 0..3 {
            let c = tracing::info_span!(
                "cache.get",
                cache.operation = "get",
                cache.store = "redis",
                cache.key = "posts:42",
                cache.hit = true,
            );
            let _entered = c.enter();
        }
    }
    span.record("http.response.status_code", status);
}

fn measure(label: &str, lookout: Option<&Lookout>, status: u16, n: u32) -> Duration {
    let subscriber = tracing_subscriber::registry().with(lookout.map(Lookout::layer));
    tracing::subscriber::with_default(subscriber, || {
        for _ in 0..200 {
            request(status);
        }
        let t = Instant::now();
        for _ in 0..n {
            request(status);
        }
        let per = t.elapsed() / n;
        println!("{label:<58} {:>8.2} µs/request", per.as_secs_f64() * 1e6);
        per
    })
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

    measure("no Lookout layer", None, 200, n);

    let off = Lookout::new(LookoutConfig::default(), store.clone());
    measure("Lookout off", Some(&off), 200, n);

    let production = Lookout::new(config(false), store.clone());
    measure("production, an ordinary request (not kept)", Some(&production), 200, n);
    production.flush().await;

    let local = Lookout::new(config(true), store.clone());
    measure("local, every request kept", Some(&local), 200, n);
    local.flush().await;

    // Every query over its threshold: kept, with a code trace on each.
    let mut slow = config(false);
    slow.watchers.entry(EntryType::Query).or_default().slow_ms = Some(0);
    let slow = Lookout::new(slow, store.clone());
    measure("production, 20 slow queries with code traces", Some(&slow), 200, 2_000);
    let t = Instant::now();
    slow.flush().await;
    println!(
        "{:<58} {:>8.2} µs/request",
        "  … and the writer's share of those",
        t.elapsed().as_secs_f64() * 1e6 / 2_200.0
    );

    let mut untraced = config(false);
    untraced.watchers.entry(EntryType::Query).or_default().slow_ms = Some(0);
    untraced.capture.code_traces = false;
    let untraced = Lookout::new(untraced, store.clone());
    measure("production, 20 slow queries, code traces off", Some(&untraced), 200, 2_000);
    untraced.flush().await;
}

/// A slow query's `code.trace` names the code that ran it — symbolised by the
/// writer, not the request — and one batch walks at most
/// `MAX_CODE_TRACES_PER_BATCH` stacks however many slow queries it runs.
#[tokio::test]
async fn slow_queries_get_code_traces_up_to_the_cap() {
    use rainier_lookout::recorder::MAX_CODE_TRACES_PER_BATCH;
    use rainier_lookout::Store as _;

    let store = Arc::new(MemoryStore::new(100));
    let mut slow = config(false);
    slow.watchers.entry(EntryType::Query).or_default().slow_ms = Some(0);
    let lookout = Lookout::new(slow, store.clone());
    let subscriber = tracing_subscriber::registry().with(lookout.layer());
    tracing::subscriber::with_default(subscriber, || request(200));
    lookout.flush().await;

    let listed = store.batches(Default::default()).await.unwrap();
    let batch = store.batch(&listed[0].id).await.unwrap().unwrap();
    let traces: Vec<&serde_json::Value> = batch
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Query)
        .filter_map(|e| e.content.get("code.trace"))
        .collect();
    assert_eq!(traces.len(), MAX_CODE_TRACES_PER_BATCH.min(20), "{traces:?}");
    let functions: Vec<&str> = traces[0]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f.get("function").and_then(|f| f.as_str()))
        .collect();
    assert!(functions.iter().any(|f| f.ends_with("hot_path_cost::request")), "{functions:?}");
}
