//! The behaviour every [`Store`] must have, as runnable checks — and the
//! reference semantics they check.
//!
//! **A test helper.** Nothing here runs in production; it is public so that a
//! store outside this crate can prove itself against the same suite:
//!
//! ```ignore
//! #[tokio::test]
//! async fn my_store_conforms() {
//!     let store = MyStore::new(/* … */);
//!     rainier_lookout::store::conformance::run(&store).await;
//! }
//! ```
//!
//! Every check starts with [`Store::clear`], seeds fixed data (timestamps are
//! fixed, from [`T0`], with microsecond parts so a store that truncates to
//! seconds is caught) and panics with the store's name on the first
//! difference. They are independent: run [`run`] for all of them, or one by
//! name while working on a store.
//!
//! ## Reference semantics
//!
//! The parts of the [`Store`] contract that are a choice rather than obvious,
//! fixed here so that every store makes the same one. The pure helpers below
//! ([`effective_limit`], [`matches()`], [`select_aggregates`]) are those choices
//! as code, and the in-process stores use them directly.
//!
//! - **Listing** ([`Store::batches`]) is newest `started_at` first, ties broken
//!   by id descending. `before` is strict. `limit` defaults to
//!   [`DEFAULT_LIMIT`] and is capped at [`MAX_LIMIT`].
//! - **`entry_type`** matches a batch holding at least one entry of the type;
//!   **`tag`** matches a batch-level tag exactly.
//! - **Storing an id that is already stored replaces the batch** (raw rows
//!   only: its entries are rolled into aggregates again, so a replayed write
//!   counts twice in aggregates).
//! - **[`Store::batch`]** returns the entries in the order they were given.
//! - **[`Store::entries`]** is newest `occurred_at` first, `before` strict.
//! - **[`Store::aggregates`]** returns one row per key: every bucket of
//!   `period_s` and `entry_type` whose start is at or after the start of the
//!   bucket holding `since`, merged with [`aggregate::merge`]. The row's
//!   `bucket_start` is the earliest bucket merged. Ordered by the query's
//!   [`AggregateOrder`] descending, ties by key ascending, then cut to `limit`.
//! - **[`Store::prune`]** removes batches with `started_at < batches_before`
//!   and aggregates with `bucket_start < aggregates_before`, independently,
//!   and returns the number of batches removed.
//! - Timestamps keep **microsecond** precision.

use std::cmp::{Ordering, Reverse};
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;

use super::{AggregateOrder, AggregateQuery, BatchQuery, Store};
use crate::aggregate::{self, bucket_start};
use crate::entry::{Aggregate, Batch, BatchKind, BatchSummary, Entry, EntryType};

/// How many batches a listing returns when the query does not say.
pub const DEFAULT_LIMIT: usize = 50;

/// The most batches a listing returns, whatever the query asks for.
pub const MAX_LIMIT: usize = 200;

/// The listing limit a [`BatchQuery`] resolves to.
pub fn effective_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
}

/// Whether `batch` passes every filter of `query` except `limit`.
pub fn matches(query: &BatchQuery, batch: &BatchSummary) -> bool {
    if query.kind.is_some_and(|kind| kind != batch.kind) {
        return false;
    }
    if let Some(entry_type) = query.entry_type {
        if batch.counts.get(&entry_type).copied().unwrap_or(0) == 0 {
            return false;
        }
    }
    if query.trace_id.as_ref().is_some_and(|t| &batch.trace_id != t) {
        return false;
    }
    if query.route_name.as_ref().is_some_and(|r| batch.route_name.as_ref() != Some(r)) {
        return false;
    }
    if query.user_id.as_ref().is_some_and(|u| batch.user_id.as_ref() != Some(u)) {
        return false;
    }
    if query.tag.as_ref().is_some_and(|t| !batch.tags.contains(t)) {
        return false;
    }
    if query.has_exception.is_some_and(|v| v != batch.has_exception) {
        return false;
    }
    if query.has_slow.is_some_and(|v| v != batch.has_slow) {
        return false;
    }
    if query.before.is_some_and(|before| batch.started_at >= before) {
        return false;
    }
    true
}

/// The listing order: newest first, ties by id descending.
pub fn newest_first(a: &BatchSummary, b: &BatchSummary) -> Ordering {
    b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id))
}

/// Answer `query` from stored aggregate rows (any mix of periods, types and
/// buckets): filter, merge per key, order and cut. See the module docs.
pub fn select_aggregates(
    rows: impl IntoIterator<Item = Aggregate>,
    query: &AggregateQuery,
) -> Vec<Aggregate> {
    let from = bucket_start(query.since, query.period_s);
    let mut per_key: BTreeMap<String, Aggregate> = BTreeMap::new();
    for row in rows {
        if row.period_s != query.period_s
            || row.entry_type != query.entry_type
            || row.bucket_start < from
        {
            continue;
        }
        match per_key.get_mut(&row.key) {
            Some(existing) => {
                aggregate::merge(existing, &row);
                existing.bucket_start = existing.bucket_start.min(row.bucket_start);
            }
            None => {
                per_key.insert(row.key.clone(), row);
            }
        }
    }
    let mut keys: Vec<Aggregate> = per_key.into_values().collect();
    // `per_key` iterated in key order and the sort is stable, so equal values
    // stay in key order.
    match query.order {
        AggregateOrder::Count => keys.sort_by_key(|a| Reverse(a.count)),
        AggregateOrder::Max => keys.sort_by_key(|a| Reverse(a.max_us)),
        AggregateOrder::Average => keys.sort_by(|a, b| {
            // sum_a / count_a vs sum_b / count_b, without floats or division.
            let left = u128::from(b.sum_us) * u128::from(a.count.max(1));
            let right = u128::from(a.sum_us) * u128::from(b.count.max(1));
            left.cmp(&right)
        }),
    }
    keys.truncate(query.limit);
    keys
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The instant every fixture is relative to: 2026-01-01T00:00:00Z.
pub const T0: i64 = 1_767_225_600;

/// `T0 + seconds`, plus `micros` — fixtures carry a sub-second part so a store
/// that drops it is caught.
pub fn at(seconds: i64, micros: u32) -> DateTime<Utc> {
    Utc.timestamp_opt(T0 + seconds, micros * 1_000).single().expect("a valid fixture instant")
}

/// An entry of `entry_type` in `batch_id`, keyed `key`, at `occurred_at`.
pub fn entry(
    batch_id: &str,
    n: usize,
    entry_type: EntryType,
    key: &str,
    occurred_at: DateTime<Utc>,
    duration_us: Option<u64>,
) -> Entry {
    Entry {
        id: format!("{batch_id}-e{n}"),
        batch_id: batch_id.to_string(),
        entry_type,
        span_id: Some(format!("{batch_id}-s{n}")),
        parent_span_id: (n > 0).then(|| format!("{batch_id}-s0")),
        family_hash: Some(key.to_string()),
        summary: format!("{key} #{n}"),
        occurred_at,
        duration_us,
        tags: if n.is_multiple_of(2) { vec![] } else { vec![format!("n:{n}")] },
        content: json!({ "key": key, "n": n, "nested": { "list": [1, 2, 3], "null": null } }),
    }
}

/// A request batch with no entries yet.
pub fn batch(id: &str, kind: BatchKind, started_at: DateTime<Utc>) -> Batch {
    Batch {
        id: id.to_string(),
        trace_id: format!("trace-{id}"),
        kind,
        summary: format!("{id} summary"),
        route_name: None,
        route_pattern: None,
        method: None,
        status: None,
        guard: None,
        user_id: None,
        duration_us: 1_500,
        has_exception: false,
        has_slow: false,
        host: Some("web-1".into()),
        started_at,
        tags: vec![],
        entries: vec![],
    }
}

/// The five batches most checks share, oldest first:
///
/// | id | kind | at | route | user | flags | tags | entries |
/// |---|---|---|---|---|---|---|---|
/// | b1 | request | +10s | posts.index | 1 | | route:posts.index | request, query |
/// | b2 | request | +20s | posts.show | 2 | exception | failed | request, query×2, exception |
/// | b3 | job (queued by b2: same trace) | +30s | | | slow | slow | job |
/// | b4 | command | +40s | | | | user:1 | command |
/// | b5 | request | +50s | posts.index | 1 | | vip, route:posts.index | request, cache |
pub fn sample() -> Vec<Batch> {
    let mut b1 = batch("b1", BatchKind::Request, at(10, 1));
    b1.route_name = Some("posts.index".into());
    b1.route_pattern = Some("/posts".into());
    b1.method = Some("GET".into());
    b1.status = Some(200);
    b1.guard = Some("session".into());
    b1.user_id = Some("1".into());
    b1.tags = vec!["route:posts.index".into()];
    b1.entries = vec![
        entry("b1", 0, EntryType::Request, "posts.index", at(10, 1), Some(1_000)),
        entry("b1", 1, EntryType::Query, "select * from posts", at(10, 200), Some(100)),
    ];

    let mut b2 = batch("b2", BatchKind::Request, at(20, 2));
    b2.route_name = Some("posts.show".into());
    b2.route_pattern = Some("/posts/{post}".into());
    b2.method = Some("GET".into());
    b2.status = Some(500);
    b2.user_id = Some("2".into());
    b2.has_exception = true;
    b2.tags = vec!["failed".into()];
    b2.entries = vec![
        entry("b2", 0, EntryType::Request, "posts.show", at(20, 2), Some(9_000)),
        entry(
            "b2",
            1,
            EntryType::Query,
            "select * from posts where id = ?",
            at(20, 300),
            Some(400),
        ),
        entry("b2", 2, EntryType::Query, "select * from comments", at(20, 900), Some(50)),
        entry("b2", 3, EntryType::Exception, "boom", at(21, 0), None),
    ];

    let mut b3 = batch("b3", BatchKind::Job, at(30, 3));
    // Queued by b2: the same trace, a batch of its own.
    b3.trace_id = b2.trace_id.clone();
    b3.summary = "job SendInvoice".into();
    b3.has_slow = true;
    b3.duration_us = 5_000_000;
    b3.tags = vec!["slow".into()];
    b3.entries = vec![entry("b3", 0, EntryType::Job, "SendInvoice", at(30, 3), Some(5_000_000))];

    let mut b4 = batch("b4", BatchKind::Command, at(40, 4));
    b4.host = None;
    b4.tags = vec!["user:1".into()];
    b4.entries = vec![entry("b4", 0, EntryType::Command, "migrate", at(40, 4), Some(70))];

    let mut b5 = batch("b5", BatchKind::Request, at(50, 5));
    b5.route_name = Some("posts.index".into());
    b5.method = Some("GET".into());
    b5.status = Some(200);
    b5.user_id = Some("1".into());
    b5.tags = vec!["vip".into(), "route:posts.index".into()];
    b5.entries = vec![
        entry("b5", 0, EntryType::Request, "posts.index", at(50, 5), Some(3_000)),
        entry("b5", 1, EntryType::Cache, "posts:1", at(50, 10), Some(5)),
    ];

    vec![b1, b2, b3, b4, b5]
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Every check, in order. Panics on the first failure.
pub async fn run(store: &dyn Store) {
    lists_newest_first_with_full_summaries(store).await;
    filters_narrow_the_listing(store).await;
    before_is_a_strict_cursor(store).await;
    limit_defaults_to_50_and_caps_at_200(store).await;
    a_batch_round_trips_with_its_entries_in_order(store).await;
    storing_an_id_again_replaces_it(store).await;
    entries_by_type_are_newest_first_with_a_cursor(store).await;
    aggregates_roll_up_and_order(store).await;
    aggregates_merge_across_writes_into_one_bucket(store).await;
    prune_cuts_batches_and_aggregates_independently(store).await;
    clear_removes_everything(store).await;
}

async fn seed(store: &dyn Store, batches: Vec<Batch>) {
    store.clear().await.unwrap_or_else(|e| panic!("[{}] clear: {e}", store.name()));
    store.store(batches).await.unwrap_or_else(|e| panic!("[{}] store: {e}", store.name()));
}

async fn list(store: &dyn Store, query: BatchQuery) -> Vec<BatchSummary> {
    store
        .batches(query.clone())
        .await
        .unwrap_or_else(|e| panic!("[{}] batches({query:?}): {e}", store.name()))
}

async fn ids(store: &dyn Store, query: BatchQuery) -> Vec<String> {
    list(store, query).await.into_iter().map(|b| b.id).collect()
}

/// Stored batches list newest first, and each summary is exactly
/// [`Batch::summarize`] of what went in — every field, counts and tags
/// included.
pub async fn lists_newest_first_with_full_summaries(store: &dyn Store) {
    let batches = sample();
    // Stored out of order, in two writes: the order is the store's job.
    seed(store, vec![batches[2].clone(), batches[0].clone(), batches[4].clone()]).await;
    store.store(vec![batches[3].clone(), batches[1].clone()]).await.expect("second write");

    let listed = list(store, BatchQuery::default()).await;
    let expected: Vec<BatchSummary> = batches.iter().rev().map(Batch::summarize).collect();
    assert_eq!(listed, expected, "[{}] listing", store.name());
}

/// Each [`BatchQuery`] filter narrows on its own, and they combine.
pub async fn filters_narrow_the_listing(store: &dyn Store) {
    seed(store, sample()).await;
    let name = store.name();
    let q = BatchQuery::default;

    let cases: Vec<(&str, BatchQuery, Vec<&str>)> = vec![
        ("kind", BatchQuery { kind: Some(BatchKind::Job), ..q() }, vec!["b3"]),
        (
            "kind request",
            BatchQuery { kind: Some(BatchKind::Request), ..q() },
            vec!["b5", "b2", "b1"],
        ),
        ("entry_type", BatchQuery { entry_type: Some(EntryType::Query), ..q() }, vec!["b2", "b1"]),
        ("entry_type none", BatchQuery { entry_type: Some(EntryType::Mail), ..q() }, vec![]),
        (
            "route_name",
            BatchQuery { route_name: Some("posts.index".into()), ..q() },
            vec!["b5", "b1"],
        ),
        ("trace_id", BatchQuery { trace_id: Some("trace-b2".into()), ..q() }, vec!["b3", "b2"]),
        ("user_id", BatchQuery { user_id: Some("1".into()), ..q() }, vec!["b5", "b1"]),
        ("tag", BatchQuery { tag: Some("route:posts.index".into()), ..q() }, vec!["b5", "b1"]),
        ("tag exact", BatchQuery { tag: Some("route".into()), ..q() }, vec![]),
        ("tag user", BatchQuery { tag: Some("user:1".into()), ..q() }, vec!["b4"]),
        ("has_exception", BatchQuery { has_exception: Some(true), ..q() }, vec!["b2"]),
        (
            "has_exception false",
            BatchQuery { has_exception: Some(false), ..q() },
            vec!["b5", "b4", "b3", "b1"],
        ),
        ("has_slow", BatchQuery { has_slow: Some(true), ..q() }, vec!["b3"]),
        (
            "combined",
            BatchQuery {
                kind: Some(BatchKind::Request),
                user_id: Some("1".into()),
                tag: Some("vip".into()),
                entry_type: Some(EntryType::Cache),
                ..q()
            },
            vec!["b5"],
        ),
        (
            "combined, contradictory",
            BatchQuery { route_name: Some("posts.index".into()), has_exception: Some(true), ..q() },
            vec![],
        ),
    ];
    for (label, query, expected) in cases {
        assert_eq!(ids(store, query).await, expected, "[{name}] filter: {label}");
    }
}

/// `before` pages: strictly earlier batches only, and it composes with
/// filters and `limit`.
pub async fn before_is_a_strict_cursor(store: &dyn Store) {
    seed(store, sample()).await;
    let name = store.name();
    let b3 = at(30, 3);

    let page = ids(store, BatchQuery { before: Some(b3), ..Default::default() }).await;
    assert_eq!(page, vec!["b2", "b1"], "[{name}] before is strict");

    let page = ids(
        store,
        BatchQuery {
            before: Some(b3 + Duration::microseconds(1)),
            limit: Some(2),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(page, vec!["b3", "b2"], "[{name}] before + limit");

    let page = ids(
        store,
        BatchQuery {
            before: Some(at(50, 5)),
            route_name: Some("posts.index".into()),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(page, vec!["b1"], "[{name}] before + filter");
}

/// No `limit` lists 50; a larger one is capped at 200; a smaller one is kept.
pub async fn limit_defaults_to_50_and_caps_at_200(store: &dyn Store) {
    let many: Vec<Batch> =
        (0..210).map(|i| batch(&format!("m{i:03}"), BatchKind::Other, at(i, 7))).collect();
    seed(store, many).await;
    let name = store.name();

    let all = ids(store, BatchQuery::default()).await;
    assert_eq!(all.len(), DEFAULT_LIMIT, "[{name}] default limit");
    assert_eq!(all.first().map(String::as_str), Some("m209"), "[{name}] newest first");
    assert_eq!(all.last().map(String::as_str), Some("m160"), "[{name}] newest 50");

    let capped = ids(store, BatchQuery { limit: Some(1_000), ..Default::default() }).await;
    assert_eq!(capped.len(), MAX_LIMIT, "[{name}] capped limit");

    let three = ids(store, BatchQuery { limit: Some(3), ..Default::default() }).await;
    assert_eq!(three, vec!["m209", "m208", "m207"], "[{name}] small limit");
}

/// [`Store::batch`] returns exactly what was stored — entries in their
/// original order, content, tags and span ids intact — and `None` for an
/// unknown id.
pub async fn a_batch_round_trips_with_its_entries_in_order(store: &dyn Store) {
    let mut batches = sample();
    // Entries deliberately not in id order, and two at the same instant: the
    // given order is what comes back.
    let b2 = &mut batches[1];
    b2.entries.swap(1, 2);
    b2.entries[1].occurred_at = b2.entries[2].occurred_at;
    let expected = batches.clone();
    seed(store, batches).await;
    let name = store.name();

    for batch in &expected {
        let found = store.batch(&batch.id).await.unwrap_or_else(|e| panic!("[{name}] batch: {e}"));
        assert_eq!(found.as_ref(), Some(batch), "[{name}] batch {}", batch.id);
    }
    assert_eq!(store.batch("nope").await.expect("batch"), None, "[{name}] unknown id");
}

/// Storing a batch whose id is already stored replaces it rather than
/// duplicating it or failing.
pub async fn storing_an_id_again_replaces_it(store: &dyn Store) {
    let batches = sample();
    seed(store, batches.clone()).await;
    let name = store.name();

    let mut again = batches[0].clone();
    again.summary = "replaced".into();
    again.tags = vec!["second".into()];
    again.entries.truncate(1);
    store.store(vec![again.clone()]).await.unwrap_or_else(|e| panic!("[{name}] restore: {e}"));

    let listed = list(store, BatchQuery::default()).await;
    assert_eq!(listed.len(), 5, "[{name}] no duplicate");
    assert_eq!(store.batch("b1").await.expect("batch"), Some(again.clone()), "[{name}] replaced");
    assert_eq!(
        ids(store, BatchQuery { tag: Some("route:posts.index".into()), ..Default::default() })
            .await,
        vec!["b5"],
        "[{name}] old tags gone"
    );
    assert_eq!(
        ids(store, BatchQuery { entry_type: Some(EntryType::Query), ..Default::default() }).await,
        vec!["b2"],
        "[{name}] old entries gone"
    );
}

/// [`Store::entries`] lists one type across batches, newest first, with a
/// strict cursor and a limit.
pub async fn entries_by_type_are_newest_first_with_a_cursor(store: &dyn Store) {
    let batches = sample();
    seed(store, batches.clone()).await;
    let name = store.name();
    let all_queries: Vec<Entry> = {
        let mut q: Vec<Entry> = batches
            .iter()
            .flat_map(|b| b.entries.iter())
            .filter(|e| e.entry_type == EntryType::Query)
            .cloned()
            .collect();
        q.sort_by_key(|e| Reverse(e.occurred_at));
        q
    };
    assert_eq!(all_queries.len(), 3);

    let got = store.entries(EntryType::Query, None, 10).await.expect("entries");
    assert_eq!(got, all_queries, "[{name}] all queries");

    let got = store.entries(EntryType::Query, None, 2).await.expect("entries");
    assert_eq!(got, all_queries[..2].to_vec(), "[{name}] limit");

    let cursor = all_queries[1].occurred_at;
    let got = store.entries(EntryType::Query, Some(cursor), 10).await.expect("entries");
    assert_eq!(got, all_queries[2..].to_vec(), "[{name}] strict cursor");

    let got = store.entries(EntryType::Exception, None, 10).await.expect("entries");
    assert_eq!(
        got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        vec!["b2-e3"],
        "[{name}] exceptions"
    );

    assert!(store.entries(EntryType::Mail, None, 10).await.expect("entries").is_empty());
}

fn agg_query(
    entry_type: EntryType,
    period_s: u32,
    since: DateTime<Utc>,
    order: AggregateOrder,
    limit: usize,
) -> AggregateQuery {
    AggregateQuery { entry_type, period_s, since, order, limit }
}

/// Aggregates match [`aggregate::rollup`] of what was stored, per period, and
/// order by count, average and max.
pub async fn aggregates_roll_up_and_order(store: &dyn Store) {
    // Three query families in the first minute, one in the third, designed
    // so each order puts a different key first:
    //   a: 3 × 10µs          → most frequent
    //   b: 1 × 500µs         → slowest on average and worst case… until c
    //   c: 2 × (100, 900)µs  → worst single (900), average 500 = b's; tie by key: b
    //   d: 1 × 1µs at +130s  → only in the third minute
    let mut x = batch("agg", BatchKind::Request, at(0, 0));
    let e = |n, key: &str, s, us| entry("agg", n, EntryType::Query, key, at(s, 0), Some(us));
    x.entries = vec![
        e(0, "a", 1, 10),
        e(1, "a", 2, 10),
        e(2, "a", 3, 10),
        e(3, "b", 4, 500),
        e(4, "c", 5, 100),
        e(5, "c", 6, 900),
        e(6, "d", 130, 1),
        // Not aggregated: a log line, and a cache hit without a duration.
        entry("agg", 7, EntryType::Log, "log", at(7, 0), Some(1)),
        entry("agg", 8, EntryType::Cache, "nodur", at(8, 0), None),
    ];
    seed(store, vec![x.clone()]).await;
    let name = store.name();
    let fetch = |q: AggregateQuery| async move {
        store
            .aggregates(q.clone())
            .await
            .unwrap_or_else(|e| panic!("[{name}] aggregates({q:?}): {e}"))
    };
    let keys = |rows: &[Aggregate]| rows.iter().map(|a| a.key.clone()).collect::<Vec<_>>();

    // The oracle: the shared rollup, selected the shared way.
    for &period in aggregate::PERIODS {
        for order in [AggregateOrder::Count, AggregateOrder::Average, AggregateOrder::Max] {
            let q = agg_query(EntryType::Query, period, at(0, 0), order, 10);
            let expected = select_aggregates(aggregate::rollup(std::slice::from_ref(&x)), &q);
            assert_eq!(fetch(q.clone()).await, expected, "[{name}] {q:?}");
        }
    }

    let since = at(0, 0);
    let by_count = fetch(agg_query(EntryType::Query, 60, since, AggregateOrder::Count, 10)).await;
    assert_eq!(keys(&by_count), vec!["a", "c", "b", "d"], "[{name}] by count");
    let a = &by_count[0];
    assert_eq!((a.count, a.sum_us, a.max_us, a.period_s), (3, 30, 10, 60), "[{name}] a totals");
    assert_eq!(a.bucket_start, at(0, 0), "[{name}] a bucket");
    assert_eq!(a.entry_type, EntryType::Query);

    let by_avg = fetch(agg_query(EntryType::Query, 60, since, AggregateOrder::Average, 10)).await;
    assert_eq!(keys(&by_avg), vec!["b", "c", "a", "d"], "[{name}] by average");

    let by_max = fetch(agg_query(EntryType::Query, 60, since, AggregateOrder::Max, 2)).await;
    assert_eq!(keys(&by_max), vec!["c", "b"], "[{name}] by max, limited");

    // `since` inside the second minute starts at that minute's bucket.
    let late = fetch(agg_query(EntryType::Query, 60, at(90, 0), AggregateOrder::Count, 10)).await;
    assert_eq!(keys(&late), vec!["d"], "[{name}] since");
    assert_eq!(late[0].bucket_start, at(120, 0));

    // `since` inside the first minute includes it.
    let early = fetch(agg_query(EntryType::Query, 60, at(30, 0), AggregateOrder::Count, 10)).await;
    assert_eq!(early.len(), 4, "[{name}] since rounds down to its bucket");

    // On the hour, everything is one bucket.
    let hour = fetch(agg_query(EntryType::Query, 3600, since, AggregateOrder::Count, 10)).await;
    assert!(hour.iter().all(|a| a.bucket_start == at(0, 0) && a.period_s == 3600));

    assert!(fetch(agg_query(EntryType::Log, 60, since, AggregateOrder::Count, 10))
        .await
        .is_empty());
    assert!(fetch(agg_query(EntryType::Cache, 60, since, AggregateOrder::Count, 10))
        .await
        .is_empty());
    assert!(fetch(agg_query(EntryType::Job, 60, since, AggregateOrder::Count, 10))
        .await
        .is_empty());
}

/// Two writes landing in the same bucket merge: counts and sums add, the max
/// is the larger. Different periods stay apart.
pub async fn aggregates_merge_across_writes_into_one_bucket(store: &dyn Store) {
    let one = |id: &str, s, us| {
        let mut b = batch(id, BatchKind::Request, at(s, 0));
        b.entries = vec![entry(id, 0, EntryType::Request, "posts.index", at(s, 0), Some(us))];
        b
    };
    seed(store, vec![one("w1", 5, 300)]).await;
    store.store(vec![one("w2", 15, 100)]).await.expect("second write");
    store.store(vec![one("w3", 25, 700), one("w4", 65, 50)]).await.expect("third write");
    let name = store.name();

    let minute = store
        .aggregates(agg_query(EntryType::Request, 60, at(0, 0), AggregateOrder::Count, 10))
        .await
        .expect("aggregates");
    assert_eq!(minute.len(), 1, "[{name}] one key: {minute:?}");
    let m = &minute[0];
    assert_eq!((m.count, m.sum_us, m.max_us), (4, 1_150, 700), "[{name}] merged across buckets");

    let first_minute = store
        .aggregates(agg_query(EntryType::Request, 60, at(0, 0), AggregateOrder::Count, 10))
        .await
        .expect("aggregates");
    assert_eq!(first_minute[0].bucket_start, at(0, 0));

    let second_minute = store
        .aggregates(agg_query(EntryType::Request, 60, at(60, 0), AggregateOrder::Count, 10))
        .await
        .expect("aggregates");
    assert_eq!(
        second_minute.iter().map(|a| (a.count, a.sum_us, a.max_us)).collect::<Vec<_>>(),
        vec![(1, 50, 50)],
        "[{name}] the second minute is its own bucket"
    );

    let ten = store
        .aggregates(agg_query(EntryType::Request, 600, at(0, 0), AggregateOrder::Count, 10))
        .await
        .expect("aggregates");
    assert_eq!(
        ten.iter().map(|a| (a.count, a.sum_us, a.max_us)).collect::<Vec<_>>(),
        vec![(4, 1_150, 700)],
        "[{name}] the ten-minute bucket holds all four"
    );
}

/// Prune removes old batches (their entries and tags too) and old aggregate
/// buckets, each by its own cutoff, and counts only batches.
pub async fn prune_cuts_batches_and_aggregates_independently(store: &dyn Store) {
    let mut batches = sample();
    // One more request, in the third minute, so a 60s bucket survives a prune
    // of the first.
    let mut late = batch("late", BatchKind::Request, at(130, 0));
    late.entries = vec![entry("late", 0, EntryType::Request, "posts.index", at(130, 0), Some(42))];
    batches.push(late);
    seed(store, batches).await;
    let name = store.name();

    // Batches before b3; aggregates untouched (cutoff before everything).
    let pruned = store.prune(at(30, 3), at(-3_600, 0)).await.expect("prune");
    assert_eq!(pruned, 2, "[{name}] two batches pruned");
    assert_eq!(ids(store, BatchQuery::default()).await, vec!["late", "b5", "b4", "b3"]);
    assert_eq!(store.batch("b1").await.expect("batch"), None, "[{name}] b1 gone");
    assert!(
        ids(store, BatchQuery { tag: Some("failed".into()), ..Default::default() })
            .await
            .is_empty(),
        "[{name}] pruned tags gone"
    );
    assert!(
        store.entries(EntryType::Query, None, 10).await.expect("entries").is_empty(),
        "[{name}] pruned entries gone"
    );
    let q = agg_query(EntryType::Query, 60, at(-3_600, 0), AggregateOrder::Count, 10);
    assert_eq!(
        store.aggregates(q.clone()).await.expect("agg").len(),
        3,
        "[{name}] aggregates kept"
    );

    // Aggregates before the second minute; no batches (cutoff before all).
    let pruned = store.prune(at(-3_600, 0), at(60, 0)).await.expect("prune");
    assert_eq!(pruned, 0, "[{name}] no batches pruned");
    assert_eq!(ids(store, BatchQuery::default()).await.len(), 4, "[{name}] batches kept");
    assert!(store.aggregates(q).await.expect("agg").is_empty(), "[{name}] old buckets gone");
    let requests = store
        .aggregates(agg_query(EntryType::Request, 60, at(-3_600, 0), AggregateOrder::Count, 10))
        .await
        .expect("agg");
    assert_eq!(
        requests.iter().map(|a| (a.key.as_str(), a.count, a.bucket_start)).collect::<Vec<_>>(),
        vec![("posts.index", 1, at(120, 0))],
        "[{name}] the third minute survives"
    );
    // Hour and ten-minute buckets start at T0, before the cutoff: gone.
    assert!(store
        .aggregates(agg_query(EntryType::Request, 3600, at(-3_600, 0), AggregateOrder::Count, 10))
        .await
        .expect("agg")
        .is_empty());

    // Pruning everything.
    let pruned = store.prune(at(10_000, 0), at(10_000, 0)).await.expect("prune");
    assert_eq!(pruned, 4, "[{name}] the rest");
    assert!(ids(store, BatchQuery::default()).await.is_empty());
}

/// [`Store::clear`] leaves nothing: no batches, entries or aggregates.
pub async fn clear_removes_everything(store: &dyn Store) {
    seed(store, sample()).await;
    let name = store.name();
    store.clear().await.expect("clear");

    assert!(ids(store, BatchQuery::default()).await.is_empty(), "[{name}] batches");
    assert_eq!(store.batch("b1").await.expect("batch"), None, "[{name}] batch");
    assert!(store.entries(EntryType::Request, None, 10).await.expect("entries").is_empty());
    for &period in aggregate::PERIODS {
        let q = agg_query(EntryType::Request, period, at(-3_600, 0), AggregateOrder::Count, 10);
        assert!(store.aggregates(q).await.expect("agg").is_empty(), "[{name}] aggregates");
    }

    // And it is usable afterwards.
    store.store(sample()).await.expect("store after clear");
    assert_eq!(ids(store, BatchQuery::default()).await.len(), 5);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_default_and_cap() {
        assert_eq!(effective_limit(None), 50);
        assert_eq!(effective_limit(Some(10)), 10);
        assert_eq!(effective_limit(Some(10_000)), 200);
    }

    #[test]
    fn the_sample_summaries_say_what_the_table_says() {
        let s = sample();
        assert!(s[1].has_exception && s[2].has_slow);
        assert_eq!(s[1].counts().get(&EntryType::Query), Some(&2));
        assert!(matches(
            &BatchQuery { tag: Some("vip".into()), ..Default::default() },
            &s[4].summarize()
        ));
    }

    #[test]
    fn average_ties_break_by_key() {
        let row = |key: &str, count, sum_us| Aggregate {
            bucket_start: at(0, 0),
            period_s: 60,
            entry_type: EntryType::Query,
            key: key.into(),
            count,
            sum_us,
            max_us: sum_us,
        };
        let q = agg_query(EntryType::Query, 60, at(0, 0), AggregateOrder::Average, 10);
        let got =
            select_aggregates(vec![row("z", 2, 1000), row("a", 1, 500), row("m", 1, 501)], &q);
        assert_eq!(got.iter().map(|a| a.key.as_str()).collect::<Vec<_>>(), vec!["m", "a", "z"]);
    }
}
