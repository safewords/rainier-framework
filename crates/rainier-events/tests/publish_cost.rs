//! What `publish` costs the code that calls it.
//!
//! Not a pass/fail test — it prints numbers, so it is `#[ignore]`d. Run it
//! with
//!
//! ```text
//! cargo test -p rainier-events --release --test publish_cost -- --ignored --nocapture
//! ```
//!
//! Each figure is the publishing thread's time per call, averaged over a
//! million calls on one thread of a multi-threaded runtime, inside a unit of
//! work (as a query in a request would be). Delivery happens on other worker
//! threads and is not timed — that is the point.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rainier_events::{Dispatcher, Unit, UnitKind};

/// The shape of a query event: a few strings and numbers.
#[allow(dead_code)]
struct QueryRan {
    sql: &'static str,
    connection: &'static str,
    duration_us: u64,
    rows: u64,
}

const N: u64 = 1_000_000;

fn query() -> QueryRan {
    QueryRan {
        sql: "SELECT `posts`.* FROM `posts` WHERE `posts`.`id` = ? LIMIT ?",
        connection: "mysql",
        duration_us: 120,
        rows: 1,
    }
}

async fn measure(label: &str, events: &Arc<Dispatcher>, via_bus: bool) {
    let unit = Unit::on(events, UnitKind::Request, Some("GET /posts/{id}".into()), None);
    let publisher = Arc::clone(events);
    let elapsed = unit
        .scope(async move {
            let started = Instant::now();
            for _ in 0..N {
                if via_bus {
                    rainier_events::publish_with(query);
                } else {
                    publisher.publish_with(query);
                }
            }
            started.elapsed()
        })
        .await;
    drop(unit);
    let ns = elapsed.as_nanos() as f64 / N as f64;
    println!("{label:<52} {ns:>8.1} ns/publish");
    events.settled().await;
}

/// Request-shaped: 23 publishes (20 queries and 3 cache calls' worth), then
/// the request awaits something and the runtime gets its threads back.
/// Only the publishes are timed.
async fn measure_requests(label: &str, events: &Arc<Dispatcher>) {
    const REQUESTS: u64 = 20_000;
    let mut timed = std::time::Duration::ZERO;
    for _ in 0..REQUESTS {
        let unit = Unit::on(events, UnitKind::Request, Some("GET /posts/{id}".into()), None);
        let publisher = Arc::clone(events);
        timed += unit
            .scope(async move {
                let started = Instant::now();
                for _ in 0..23 {
                    publisher.publish_with(query);
                }
                started.elapsed()
            })
            .await;
        drop(unit);
        tokio::task::yield_now().await;
    }
    events.settled().await;
    let ns = timed.as_nanos() as f64 / (REQUESTS * 23) as f64;
    println!("{label:<52} {ns:>8.1} ns/publish  ({:.1} µs/request)", ns * 23.0 / 1000.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "prints timings"]
async fn publish_cost() {
    let bare = Arc::new(Dispatcher::new());
    measure("no subscribers (dispatcher in hand)", &bare, false).await;
    measure("no subscribers (process bus, resolved per call)", &bare, true).await;

    let one = Arc::new(Dispatcher::new());
    let count = Arc::new(AtomicU64::new(0));
    let c = Arc::clone(&count);
    one.on::<QueryRan>().run_sync(move |_, _| {
        c.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    measure("one sync subscriber", &one, false).await;

    let filtered = Arc::new(Dispatcher::new());
    filtered.on::<QueryRan>().when(|q| q.duration_us > 1_000_000).run_sync(|_, _| Ok(()));
    measure("one subscriber whose filter says no", &filtered, false).await;

    let many = Arc::new(Dispatcher::new());
    for _ in 0..3 {
        many.on::<QueryRan>().run(|_, _| async { Ok(()) });
    }
    many.on_any().run_sync(|_, _, _| Ok(()));
    measure("three async subscribers + a wildcard", &many, false).await;

    assert_eq!(count.load(Ordering::Relaxed), N);

    println!();
    measure_requests("request-shaped: no subscribers", &bare).await;
    measure_requests("request-shaped: one sync subscriber", &one).await;
    measure_requests("request-shaped: three async + a wildcard", &many).await;
}
