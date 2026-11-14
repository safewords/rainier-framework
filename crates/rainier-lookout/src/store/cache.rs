//! [`CacheStore`]: a ring buffer of the last N batches in the application's
//! cache — any `rainier-cache` store; Redis in production.
//!
//! ## Layout
//!
//! Every key starts with the store's prefix (`lookout:` by default):
//!
//! | Key | Holds | Expires |
//! |---|---|---|
//! | `index` | the [`BatchSummary`] of every kept batch, newest first, at most `capacity` | batch retention, refreshed on every write |
//! | `batch:{id}` | one [`Batch`], as JSON | batch retention |
//! | `buckets` | which aggregate buckets exist: `(period, type, start)` | aggregate retention, refreshed |
//! | `agg:{period}:{type}:{start}` | the [`Aggregate`]s of one bucket | aggregate retention |
//! | `lock:write` | the writers' lock | 30 s |
//!
//! The index carries whole summaries, so a listing and every filter is one
//! `get`; [`Store::entries`] reads the batches that hold the type. Each index
//! and bucket record also carries its own expiry, so a listing never shows a
//! batch whose key the cache has already expired.
//!
//! ## Concurrency, and where it is only best effort
//!
//! Writers — `store`, `prune`, `clear` — serialise on a [`Lock`](rainier_cache::Lock)
//! built from the cache's atomic `add` / `forget_if`, so two pods flushing at
//! once do not lose each other's index updates or aggregate increments. A
//! writer that cannot get the lock within the wait (5 s by default) fails
//! rather than writing unlocked; the recorder counts that like any other
//! failed write.
//!
//! What remains racy:
//!
//! - **Readers take no lock.** A listing can miss a batch being written, and
//!   [`Store::batch`] can return `None` for an id that was just trimmed.
//! - **A writer that outlives the lock's TTL** (a stalled process) can
//!   overwrite the index written by the next holder.
//! - **Stores without an atomic `add`** (Cloudflare Workers KV) cannot hold a
//!   lock at all; concurrent writers there can drop each other's batches from
//!   the index and under-count aggregates. Use one writer, or another store.
//! - **Every write rewrites the index** — O(capacity) bytes per flush. Keep
//!   the capacity in the low thousands.
//!
//! Every cache call runs inside a span carrying `rainier.lookout.internal =
//! true`, so Lookout does not record its own cache traffic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rainier_cache::{Cache, LockManager};
use rainier_support::{BoxFuture, Error, Result};
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use super::conformance::{effective_limit, matches, newest_first, select_aggregates};
use super::{AggregateQuery, BatchQuery, Store};
use crate::aggregate::{self, bucket_start};
use crate::entry::{Aggregate, Batch, BatchSummary, Entry, EntryType};

/// How long the writers' lock is held at most, if its holder dies.
const LOCK_TTL: Duration = Duration::from_secs(30);

/// One kept batch, in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Indexed {
    summary: BatchSummary,
    /// Unix milliseconds after which the batch's own key is gone.
    expires_ms: i64,
}

/// One aggregate bucket, in the bucket list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Bucket {
    period_s: u32,
    entry_type: EntryType,
    /// Unix seconds.
    start: i64,
    expires_ms: i64,
}

impl Bucket {
    fn same(&self, other: &Bucket) -> bool {
        (self.period_s, self.entry_type, self.start)
            == (other.period_s, other.entry_type, other.start)
    }
}

/// The last `capacity` batches, in the application's cache.
///
/// ```ignore
/// let store = CacheStore::new(cache, 1_000)
///     .with_retention(Duration::from_secs(24 * 3600), Duration::from_secs(30 * 86_400));
/// ```
pub struct CacheStore {
    cache: Arc<dyn Cache>,
    locks: LockManager,
    prefix: String,
    capacity: usize,
    batch_ttl: Duration,
    aggregate_ttl: Duration,
    lock_wait: Duration,
}

impl CacheStore {
    /// Keep the last `capacity` batches in `cache`, for 24 hours, and
    /// aggregates for 30 days — [`Retention`](crate::config::Retention)'s
    /// defaults.
    pub fn new(cache: Arc<dyn Cache>, capacity: usize) -> Self {
        let prefix = "lookout:".to_string();
        Self {
            locks: LockManager::new(Arc::clone(&cache)).with_prefix(format!("{prefix}lock:")),
            cache,
            prefix,
            capacity: capacity.max(1),
            batch_ttl: Duration::from_secs(24 * 3600),
            aggregate_ttl: Duration::from_secs(30 * 86_400),
            lock_wait: Duration::from_secs(5),
        }
    }

    /// Put every key under `prefix` instead of `lookout:` — to keep two
    /// applications sharing a cache apart.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self.locks =
            LockManager::new(Arc::clone(&self.cache)).with_prefix(format!("{}lock:", self.prefix));
        self
    }

    /// How long a batch, and an aggregate bucket, live in the cache.
    pub fn with_retention(mut self, batches: Duration, aggregates: Duration) -> Self {
        self.batch_ttl = batches;
        self.aggregate_ttl = aggregates;
        self
    }

    /// How long a writer waits for another to finish before failing.
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// How many batches are kept.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn key(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }

    fn batch_key(&self, id: &str) -> String {
        self.key(&format!("batch:{id}"))
    }

    fn bucket_key(&self, bucket: &Bucket) -> String {
        self.key(&format!("agg:{}:{}:{}", bucket.period_s, bucket.entry_type, bucket.start))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Result<Option<T>> {
        match self.cache.get(key).await? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn put_json<T: Serialize>(&self, key: &str, value: &T, ttl: Duration) -> Result<()> {
        self.cache.put(key, &serde_json::to_vec(value)?, Some(ttl)).await
    }

    /// The live index entries, newest first.
    async fn index(&self) -> Result<Vec<Indexed>> {
        let now = now_ms();
        let mut index: Vec<Indexed> = self.get_json(&self.key("index")).await?.unwrap_or_default();
        index.retain(|i| i.expires_ms > now);
        Ok(index)
    }

    async fn buckets(&self) -> Result<Vec<Bucket>> {
        let now = now_ms();
        let mut buckets: Vec<Bucket> =
            self.get_json(&self.key("buckets")).await?.unwrap_or_default();
        buckets.retain(|b| b.expires_ms > now);
        Ok(buckets)
    }

    /// Run `work` holding the writers' lock, releasing it whatever happens.
    async fn locked<T>(&self, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let lock = self.locks.lock("write", LOCK_TTL).wait_for(self.lock_wait);
        let Some(guard) = lock.acquire().await? else {
            return Err(Error::internal(format!(
                "lookout: another writer held `{}` for longer than {:?}",
                lock.key(),
                self.lock_wait
            )));
        };
        let outcome = work.await;
        let released = guard.release().await;
        let value = outcome?;
        released?;
        Ok(value)
    }

    async fn write(&self, batches: Vec<Batch>) -> Result<()> {
        let mut unique: BTreeMap<String, Batch> = BTreeMap::new();
        for batch in batches {
            unique.insert(batch.id.clone(), batch);
        }
        if unique.is_empty() {
            return Ok(());
        }
        let batches: Vec<Batch> = unique.into_values().collect();

        // The batches before the index, so a reader that finds an id in the
        // index finds its batch.
        for batch in &batches {
            self.put_json(&self.batch_key(&batch.id), batch, self.batch_ttl).await?;
        }

        self.locked(async {
            let expires_ms = now_ms().saturating_add(millis(self.batch_ttl));
            let ids: BTreeSet<&str> = batches.iter().map(|b| b.id.as_str()).collect();
            let mut index = self.index().await?;
            index.retain(|i| !ids.contains(i.summary.id.as_str()));
            index.extend(batches.iter().map(|b| Indexed { summary: b.summarize(), expires_ms }));
            index.sort_by(|a, b| newest_first(&a.summary, &b.summary));
            let trimmed: Vec<Indexed> = index.split_off(self.capacity.min(index.len()));
            self.put_json(&self.key("index"), &index, self.batch_ttl).await?;
            for gone in trimmed {
                self.cache.forget(&self.batch_key(&gone.summary.id)).await?;
            }

            self.roll_up(&batches).await
        })
        .await
    }

    /// Merge `batches`' aggregates into their buckets. Under the lock.
    async fn roll_up(&self, batches: &[Batch]) -> Result<()> {
        let mut incoming: BTreeMap<(u32, EntryType, i64), Vec<Aggregate>> = BTreeMap::new();
        for row in aggregate::rollup(batches) {
            incoming
                .entry((row.period_s, row.entry_type, row.bucket_start.timestamp()))
                .or_default()
                .push(row);
        }
        if incoming.is_empty() {
            return Ok(());
        }
        let expires_ms = now_ms().saturating_add(millis(self.aggregate_ttl));
        let mut buckets = self.buckets().await?;
        for ((period_s, entry_type, start), rows) in incoming {
            let bucket = Bucket { period_s, entry_type, start, expires_ms };
            let key = self.bucket_key(&bucket);
            let mut stored: BTreeMap<String, Aggregate> = self
                .get_json::<Vec<Aggregate>>(&key)
                .await?
                .unwrap_or_default()
                .into_iter()
                .map(|a| (a.key.clone(), a))
                .collect();
            for row in rows {
                match stored.get_mut(&row.key) {
                    Some(existing) => aggregate::merge(existing, &row),
                    None => {
                        stored.insert(row.key.clone(), row);
                    }
                }
            }
            let stored: Vec<Aggregate> = stored.into_values().collect();
            self.put_json(&key, &stored, self.aggregate_ttl).await?;
            buckets.retain(|b| !b.same(&bucket));
            buckets.push(bucket);
        }
        buckets.sort();
        self.put_json(&self.key("buckets"), &buckets, self.aggregate_ttl).await
    }

    async fn list(&self, query: BatchQuery) -> Result<Vec<BatchSummary>> {
        let limit = effective_limit(query.limit);
        Ok(self
            .index()
            .await?
            .into_iter()
            .map(|i| i.summary)
            .filter(|s| matches(&query, s))
            .take(limit)
            .collect())
    }

    async fn find(&self, id: &str) -> Result<Option<Batch>> {
        self.get_json(&self.batch_key(id)).await
    }

    async fn of_type(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Entry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // An entry starts no earlier than its batch, so a batch that started
        // at or after `before` holds nothing earlier than it.
        let candidates: Vec<String> = self
            .index()
            .await?
            .into_iter()
            .filter(|i| i.summary.counts.get(&entry_type).copied().unwrap_or(0) > 0)
            .filter(|i| before.is_none_or(|before| i.summary.started_at < before))
            .map(|i| i.summary.id)
            .collect();
        let mut found: Vec<(usize, Entry)> = Vec::new();
        for id in candidates {
            let Some(batch) = self.find(&id).await? else { continue };
            for (seq, entry) in batch.entries.into_iter().enumerate() {
                if entry.entry_type == entry_type
                    && before.is_none_or(|before| entry.occurred_at < before)
                {
                    found.push((seq, entry));
                }
            }
        }
        // As the database store orders them: newest first, then batch id and
        // position descending.
        found.sort_by(|(sa, a), (sb, b)| {
            b.occurred_at
                .cmp(&a.occurred_at)
                .then_with(|| b.batch_id.cmp(&a.batch_id))
                .then_with(|| sb.cmp(sa))
        });
        Ok(found.into_iter().take(limit).map(|(_, e)| e).collect())
    }

    async fn rolled_up(&self, query: AggregateQuery) -> Result<Vec<Aggregate>> {
        let from = bucket_start(query.since, query.period_s).timestamp();
        let mut rows = Vec::new();
        for bucket in self.buckets().await? {
            if bucket.period_s == query.period_s
                && bucket.entry_type == query.entry_type
                && bucket.start >= from
            {
                rows.extend(
                    self.get_json::<Vec<Aggregate>>(&self.bucket_key(&bucket))
                        .await?
                        .unwrap_or_default(),
                );
            }
        }
        Ok(select_aggregates(rows, &query))
    }

    async fn prune_keys(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> Result<u64> {
        self.locked(async {
            let (gone, kept): (Vec<Indexed>, Vec<Indexed>) = self
                .index()
                .await?
                .into_iter()
                .partition(|i| i.summary.started_at < batches_before);
            if !gone.is_empty() {
                self.put_json(&self.key("index"), &kept, self.batch_ttl).await?;
                for batch in &gone {
                    self.cache.forget(&self.batch_key(&batch.summary.id)).await?;
                }
            }

            let cutoff = aggregates_before.timestamp()
                + i64::from(aggregates_before.timestamp_subsec_nanos() > 0);
            let (old, kept): (Vec<Bucket>, Vec<Bucket>) =
                self.buckets().await?.into_iter().partition(|b| b.start < cutoff);
            if !old.is_empty() {
                self.put_json(&self.key("buckets"), &kept, self.aggregate_ttl).await?;
                for bucket in &old {
                    self.cache.forget(&self.bucket_key(bucket)).await?;
                }
            }
            Ok(gone.len() as u64)
        })
        .await
    }

    async fn clear_keys(&self) -> Result<()> {
        self.locked(async {
            for batch in
                self.get_json::<Vec<Indexed>>(&self.key("index")).await?.unwrap_or_default()
            {
                self.cache.forget(&self.batch_key(&batch.summary.id)).await?;
            }
            for bucket in
                self.get_json::<Vec<Bucket>>(&self.key("buckets")).await?.unwrap_or_default()
            {
                self.cache.forget(&self.bucket_key(&bucket)).await?;
            }
            self.cache.forget(&self.key("index")).await?;
            self.cache.forget(&self.key("buckets")).await?;
            Ok(())
        })
        .await
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// The span every store call runs in.
fn internal(operation: &'static str) -> tracing::Span {
    tracing::info_span!(
        "lookout.store",
        rainier.lookout.internal = true,
        lookout.store = "cache",
        lookout.operation = operation
    )
}

impl Store for CacheStore {
    fn name(&self) -> &'static str {
        "cache"
    }

    fn store(&self, batches: Vec<Batch>) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.write(batches).instrument(internal("store")))
    }

    fn batches(&self, query: BatchQuery) -> BoxFuture<'_, Result<Vec<BatchSummary>>> {
        Box::pin(self.list(query).instrument(internal("batches")))
    }

    fn batch(&self, id: &str) -> BoxFuture<'_, Result<Option<Batch>>> {
        let id = id.to_string();
        Box::pin(async move { self.find(&id).await }.instrument(internal("batch")))
    }

    fn entries(
        &self,
        entry_type: EntryType,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>> {
        Box::pin(self.of_type(entry_type, before, limit).instrument(internal("entries")))
    }

    fn aggregates(&self, query: AggregateQuery) -> BoxFuture<'_, Result<Vec<Aggregate>>> {
        Box::pin(self.rolled_up(query).instrument(internal("aggregates")))
    }

    fn prune(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(self.prune_keys(batches_before, aggregates_before).instrument(internal("prune")))
    }

    fn clear(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.clear_keys().instrument(internal("clear")))
    }
}

impl std::fmt::Debug for CacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheStore")
            .field("cache", &self.cache.name())
            .field("prefix", &self.prefix)
            .field("capacity", &self.capacity)
            .field("batch_ttl", &self.batch_ttl)
            .field("aggregate_ttl", &self.aggregate_ttl)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::BatchKind;
    use crate::store::conformance::{at, batch};
    use rainier_cache::{CacheExt as _, MemoryCache};

    fn store(capacity: usize) -> CacheStore {
        CacheStore::new(Arc::new(MemoryCache::new()), capacity)
    }

    #[tokio::test]
    async fn conforms_on_a_memory_cache() {
        crate::store::conformance::run(&store(1_000)).await;
    }

    #[tokio::test]
    async fn the_ring_keeps_only_the_newest_and_forgets_the_rest() {
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let store = CacheStore::new(Arc::clone(&cache), 3);
        for i in 0..5 {
            store.store(vec![batch(&format!("b{i}"), BatchKind::Other, at(i, 0))]).await.unwrap();
        }
        let ids: Vec<String> =
            store.batches(BatchQuery::default()).await.unwrap().into_iter().map(|b| b.id).collect();
        assert_eq!(ids, vec!["b4", "b3", "b2"]);
        assert!(store.batch("b0").await.unwrap().is_none(), "trimmed batches are forgotten");
        assert!(cache.get("lookout:batch:b1").await.unwrap().is_none());
        assert!(store.batch("b2").await.unwrap().is_some());

        // A batch older than everything kept falls straight out.
        store.store(vec![batch("old", BatchKind::Other, at(-100, 0))]).await.unwrap();
        assert!(store.batch("old").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn keys_live_under_the_prefix_and_clear_leaves_other_keys_alone() {
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        cache.put_string("unrelated", "kept", None).await.unwrap();
        let store = CacheStore::new(Arc::clone(&cache), 10).with_prefix("app1:lookout:");
        store.store(crate::store::conformance::sample()).await.unwrap();
        assert!(cache.get("app1:lookout:index").await.unwrap().is_some());
        assert!(cache.get("app1:lookout:batch:b1").await.unwrap().is_some());

        store.clear().await.unwrap();
        assert!(cache.get("app1:lookout:index").await.unwrap().is_none());
        assert!(cache.get("app1:lookout:batch:b1").await.unwrap().is_none());
        assert_eq!(cache.get_string("unrelated").await.unwrap().as_deref(), Some("kept"));
    }

    #[tokio::test]
    async fn a_writer_waits_for_the_lock_and_then_gives_up() {
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let store =
            CacheStore::new(Arc::clone(&cache), 10).with_lock_wait(Duration::from_millis(250));
        // Somebody else holds the writers' lock.
        assert!(cache
            .add("lookout:lock:write", b"other", Some(Duration::from_secs(30)))
            .await
            .unwrap());
        let err = store.store(vec![batch("b", BatchKind::Other, at(0, 0))]).await.unwrap_err();
        assert!(err.to_string().contains("lookout:lock:write"), "{err}");

        cache.forget("lookout:lock:write").await.unwrap();
        store.store(vec![batch("b", BatchKind::Other, at(0, 0))]).await.unwrap();
        assert!(!cache.has("lookout:lock:write").await.unwrap(), "released after the write");
    }

    #[tokio::test]
    async fn concurrent_writers_lose_nothing() {
        let store = Arc::new(store(1_000));
        let mut tasks = Vec::new();
        for w in 0..8 {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                for i in 0..5 {
                    let id = format!("w{w}-{i}");
                    let mut b = batch(&id, BatchKind::Request, at(i64::from(w * 10 + i), 0));
                    b.entries = vec![crate::store::conformance::entry(
                        &id,
                        0,
                        EntryType::Request,
                        "posts.index",
                        at(5, 0),
                        Some(10),
                    )];
                    store.store(vec![b]).await.unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let listed =
            store.batches(BatchQuery { limit: Some(200), ..Default::default() }).await.unwrap();
        assert_eq!(listed.len(), 40);
        let agg = store
            .aggregates(AggregateQuery {
                entry_type: EntryType::Request,
                period_s: 60,
                since: at(0, 0),
                order: crate::store::AggregateOrder::Count,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(agg[0].count, 40);
        assert_eq!(agg[0].sum_us, 400);
    }
}
