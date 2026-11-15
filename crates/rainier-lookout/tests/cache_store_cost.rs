//! What the cache store costs a production cache, per write and per dashboard
//! read: round trips, bytes, and wall time with a fixed latency per command.
//!
//! Not a pass/fail test — it prints numbers, so it is `#[ignore]`d. Run it
//! with
//!
//! ```text
//! cargo test -p rainier-lookout --release --test cache_store_cost -- --ignored --nocapture
//! ```
//!
//! The cache under the store is a [`MemoryCache`] behind a wrapper that counts
//! every command and the bytes it moves, and sleeps [`RTT`] per command, the
//! way a network cache would. Concurrent commands overlap, as they do on a
//! multiplexed Redis connection, so the wall time shows how many round trips
//! a call makes one after another, not just how many it makes.
//!
//! The store is first filled to a steady state like production's — a full
//! ring of `CAPACITY` batches, a day of per-minute aggregate buckets for six
//! entry types, and `CASES` open cases — and then measured.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rainier_cache::{Cache, MemoryCache};
use rainier_lookout::cases::CaseQuery;
use rainier_lookout::entry::{Batch, BatchKind, EntryType};
use rainier_lookout::store::conformance::{at, batch, entry};
use rainier_lookout::{BatchQuery, CacheStore, Store};
use rainier_support::{BoxFuture, Result};

const RTT: Duration = Duration::from_millis(1);
const CAPACITY: usize = 1_000;
const CASES: usize = 1_000;
/// Minutes of per-minute buckets to fill: a day.
const MINUTES: i64 = 1_440;

#[derive(Default)]
struct Counts {
    commands: AtomicU64,
    read: AtomicU64,
    written: AtomicU64,
}

/// A cache that counts and waits.
struct Metered {
    inner: MemoryCache,
    counts: Counts,
    latency: std::sync::atomic::AtomicBool,
}

impl Metered {
    async fn wait(&self) {
        self.counts.commands.fetch_add(1, Ordering::Relaxed);
        if self.latency.load(Ordering::Relaxed) {
            tokio::time::sleep(RTT).await;
        }
    }

    fn reset(&self) {
        self.counts.commands.store(0, Ordering::Relaxed);
        self.counts.read.store(0, Ordering::Relaxed);
        self.counts.written.store(0, Ordering::Relaxed);
    }

    fn report(&self, what: &str, took: Duration) {
        println!(
            "{what:<44} {:>6} commands {:>11} B read {:>11} B written {:>9.1} ms",
            self.counts.commands.load(Ordering::Relaxed),
            self.counts.read.load(Ordering::Relaxed),
            self.counts.written.load(Ordering::Relaxed),
            took.as_secs_f64() * 1000.0
        );
    }
}

impl Cache for Metered {
    fn name(&self) -> &str {
        "metered"
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            self.wait().await;
            let value = self.inner.get(key).await?;
            if let Some(v) = &value {
                self.counts.read.fetch_add(v.len() as u64, Ordering::Relaxed);
            }
            Ok(value)
        })
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.wait().await;
            self.counts.written.fetch_add(value.len() as u64, Ordering::Relaxed);
            self.inner.put(key, value, ttl).await
        })
    }
    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.wait().await;
            self.inner.forget(key).await
        })
    }
    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        self.inner.flush()
    }
    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            self.wait().await;
            self.inner.increment(key, by).await
        })
    }
    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.wait().await;
            self.inner.add(key, value, ttl).await
        })
    }
    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.wait().await;
            self.inner.forget_if(key, expected).await
        })
    }
}

const SHAPES: &[&str] = &[
    "SELECT `posts`.`id`, `posts`.`user_id`, `posts`.`title`, `posts`.`body`, `posts`.`created_at` FROM `posts` WHERE `posts`.`id` = ? LIMIT ?",
    "SELECT `users`.`id`, `users`.`name`, `users`.`email`, `users`.`avatar` FROM `users` WHERE `users`.`id` IN (?, ?, ?, ?)",
    "SELECT COUNT(*) AS `count` FROM `comments` WHERE `comments`.`post_id` = ? AND `comments`.`deleted_at` IS NULL",
    "SELECT `tags`.* FROM `tags` INNER JOIN `post_tag` ON `post_tag`.`tag_id` = `tags`.`id` WHERE `post_tag`.`post_id` = ?",
    "UPDATE `posts` SET `views` = `views` + ? WHERE `posts`.`id` = ?",
];

/// A request like a production one: a root, fifteen queries of five shapes,
/// three cache calls, and — when `failure` is given — an exception with a
/// stack, whose fingerprint is `failure`.
fn request(id: &str, started: DateTime<Utc>, failure: Option<usize>) -> Batch {
    let mut b = batch(id, BatchKind::Request, started);
    b.route_name = Some(format!("posts.show.{}", id.len() % 7));
    b.route_pattern = Some("/posts/{post}".into());
    b.method = Some("GET".into());
    b.status = Some(if failure.is_some() { 500 } else { 200 });
    b.user_id = Some(format!("{}", id.len() * 7919 % 5000));
    b.tags = vec![format!("route:{}", b.route_name.as_deref().unwrap_or_default()), "slow".into()];
    b.has_slow = true;
    let mut n = 0;
    let mut push = |b: &mut Batch, t: EntryType, key: &str, offset_ms: i64, us: Option<u64>| {
        let mut e = entry(id, n, t, key, started + chrono::Duration::milliseconds(offset_ms), us);
        e.content = serde_json::json!({
            "db.system.name": "mysql",
            "db.query.text": key,
            "db.namespace": "app",
            "server.address": "db-0.db.app.svc.cluster.local",
            "rainier.db.rows": 1,
            "code.trace": [
                {"function": "shop::app::http::controllers::posts::show"},
                {"function": "shop::app::repositories::posts::find"},
            ],
        });
        b.entries.push(e);
        n += 1;
    };
    let route = b.route_name.clone().unwrap_or_default();
    push(&mut b, EntryType::Request, &route, 0, Some(180_000));
    for i in 0..15 {
        push(&mut b, EntryType::Query, SHAPES[i % SHAPES.len()], 2 + i as i64, Some(900));
    }
    for i in 0..3 {
        push(&mut b, EntryType::Cache, &format!("get (redis) {i}"), 20 + i, Some(300));
    }
    if let Some(f) = failure {
        let mut e = entry(
            id,
            n,
            EntryType::Exception,
            &format!("HttpRequest|Internal|thing {f} broke|shop::app::x{f}"),
            started + chrono::Duration::milliseconds(150),
            None,
        );
        e.content = serde_json::json!({
            "type": "HttpRequest",
            "exception": {"status": 500, "message": format!("thing {f} broke")},
            "frames": (0..10).map(|i| serde_json::json!({"function": format!("shop::app::module{i}::function{f}"), "file": "./src/app/x.rs", "line": i})).collect::<Vec<_>>(),
        });
        b.entries.push(e);
        b.has_exception = true;
    }
    b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "prints what the cache store costs; run with --ignored --nocapture"]
async fn what_the_cache_store_costs() {
    let cache = Arc::new(Metered {
        inner: MemoryCache::new(),
        counts: Counts::default(),
        latency: false.into(),
    });
    let store = CacheStore::new(cache.clone() as Arc<dyn Cache>, CAPACITY);

    // Steady state, written without latency.
    let fill = Instant::now();
    let mut seq = 0usize;
    for minute in 0..MINUTES {
        // One batch a minute keeps a day of per-minute buckets.
        let mut b = request(&format!("fill-{minute}"), at(minute * 60, 0), None);
        // Six types a minute, as a busy application has.
        for (i, t) in
            [EntryType::HttpClient, EntryType::Job, EntryType::Mail].into_iter().enumerate()
        {
            b.entries.push(entry(&b.id.clone(), 100 + i, t, "x", at(minute * 60, 5), Some(10)));
        }
        store.store(vec![b]).await.unwrap();
        seq += 1;
    }
    let mut chunk = Vec::new();
    for c in 0..CASES {
        chunk.push(request(&format!("case-{c}"), at(MINUTES * 60 + c as i64, 0), Some(c)));
        if chunk.len() == 50 {
            store.store(std::mem::take(&mut chunk)).await.unwrap();
        }
    }
    println!("filled {seq} + {CASES} batches in {:.1} s", fill.elapsed().as_secs_f64());

    cache.latency.store(true, Ordering::Relaxed);
    let now = MINUTES * 60 + CASES as i64 + 10;

    // A flush: five requests, one of which failed.
    cache.reset();
    let flush: Vec<Batch> = (0..5)
        .map(|i| request(&format!("flush-{i}"), at(now + i, 0), (i == 0).then_some(7)))
        .collect();
    let t = Instant::now();
    store.store(flush).await.unwrap();
    cache.report("store: 5 batches, 1 exception", t.elapsed());

    cache.reset();
    let flush: Vec<Batch> =
        (0..5).map(|i| request(&format!("quiet-{i}"), at(now + 10 + i, 0), None)).collect();
    let t = Instant::now();
    store.store(flush).await.unwrap();
    cache.report("store: 5 batches, no exception", t.elapsed());

    cache.reset();
    let t = Instant::now();
    let listed = store.batches(BatchQuery::default()).await.unwrap();
    cache.report(&format!("batches(): {} listed", listed.len()), t.elapsed());

    cache.reset();
    let t = Instant::now();
    let found = store.entries(EntryType::Query, None, 50).await.unwrap();
    cache.report(&format!("entries(query, 50): {} found", found.len()), t.elapsed());

    cache.reset();
    let t = Instant::now();
    let found = store.family_entries(EntryType::Query, SHAPES[2], None, 50).await.unwrap();
    cache.report(&format!("family_entries(query, 50): {} found", found.len()), t.elapsed());

    cache.reset();
    let t = Instant::now();
    let mut rows = 0;
    for &t in rainier_lookout::aggregate::AGGREGATED {
        rows += store.aggregate_rows(t, 600, at(now - 2 * 3600, 0)).await.unwrap().len();
    }
    cache.report(&format!("aggregate_rows x10 (insights): {rows} rows"), t.elapsed());

    cache.reset();
    let t = Instant::now();
    let rows = store.aggregate_rows(EntryType::Query, 60, at(now - 3600, 0)).await.unwrap();
    cache.report(&format!("aggregate_rows(query, 60, 1h): {} rows", rows.len()), t.elapsed());

    cache.reset();
    let t = Instant::now();
    let cases = store.cases(CaseQuery::default()).await.unwrap();
    cache.report(&format!("cases(): {} listed", cases.len()), t.elapsed());

    cache.reset();
    let t = Instant::now();
    let case = store.case(&cases[3].id).await.unwrap().unwrap();
    store.save_case(case).await.unwrap();
    cache.report("case() + save_case()", t.elapsed());
}
