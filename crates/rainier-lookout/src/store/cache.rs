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
//! | `agg:{period}:{type}:{start}` | the [`Aggregate`]s of one bucket | aggregate retention |
//! | `buckets:{period}:{type}:{day}` | the starts of one UTC day's buckets of one period and type | aggregate retention and a day, refreshed |
//! | `buckets:days` | which of those day lists exist | aggregate retention and a day, refreshed |
//! | `case:{id}` | one [`Case`] | aggregate retention, refreshed |
//! | `cases:index` | every kept case's id, status, severity and last sighting, at most [`MAX_CASES`] | aggregate retention, refreshed |
//! | `lock:write` | the writers' lock | 30 s |
//!
//! The index carries whole summaries, so a listing and every filter is one
//! `get`. Each index and bucket record also carries its own expiry, so a
//! listing never shows a batch whose key the cache has already expired.
//!
//! ## What a write costs
//!
//! Commands run concurrently wherever they do not depend on each other — the
//! connection multiplexes, so a write is a handful of round trips however
//! many keys it touches. Under the lock a write reads and rewrites the index
//! (O(capacity) bytes: keep the capacity in the low thousands), each bucket
//! its batches roll into, the cases its exceptions touch and the small case
//! index. A day's bucket list is rewritten only when a bucket first appears —
//! once a minute per type at most — and the day list index only when a day
//! does.
//!
//! [`Store::entries`] reads the batches holding the type newest first, a few
//! at a time, and stops as soon as no batch left could hold a newer entry
//! than the ones found.
//!
//! **Earlier layouts.** Before 0.x, every bucket was listed in one `buckets`
//! key and every case in one `cases` key, each read and rewritten whole on
//! every write — tens of megabytes once a month of per-minute buckets had
//! built up. A writer folds either key, when it finds one, into the layout
//! above and deletes it; readers merge it in until then.
//!
//! ## Concurrency, and where it is only best effort
//!
//! Writers — `store`, `prune`, `clear`, `save_case` — serialise on a
//! [`Lock`](rainier_cache::Lock) built from the cache's atomic `add` /
//! `forget_if`, so two pods flushing at once do not lose each other's index
//! updates or aggregate increments. A writer that cannot get the lock within
//! the wait (5 s by default) fails rather than writing unlocked; the recorder
//! counts that like any other failed write.
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
//!
//! Every cache call runs inside a span carrying `rainier.lookout.internal =
//! true`, so Lookout does not record its own cache traffic.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::future::{try_join, try_join3};
use futures_util::stream::{self, StreamExt as _, TryStreamExt as _};
use rainier_cache::{Cache, LockManager};
use rainier_support::{BoxFuture, Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use super::conformance::{effective_limit, matches, newest_first};
use super::{BatchQuery, Store};
use crate::aggregate::{self, bucket_start};
use crate::cases::{observe, occurrences, select_cases, Case, CaseQuery, CaseSeverity, CaseStatus};
use crate::entry::{Aggregate, Batch, BatchSummary, Entry, EntryType};

/// The most cases the cache store keeps.
pub const MAX_CASES: usize = 1000;

/// How long the writers' lock is held at most, if its holder dies.
const LOCK_TTL: Duration = Duration::from_secs(30);

/// The most cache commands one call keeps in flight.
const IN_FLIGHT: usize = 32;

/// How many batches [`Store::entries`] reads first; each further round reads
/// twice as many, up to [`MAX_WAVE`].
const FIRST_WAVE: usize = 16;
const MAX_WAVE: usize = 128;

/// A UTC day, in seconds.
const DAY_S: i64 = 86_400;

/// One kept batch, in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Indexed {
    summary: BatchSummary,
    /// Unix milliseconds after which the batch's own key is gone.
    expires_ms: i64,
    /// Unix microseconds of the batch's latest entry — the newest entry
    /// [`Store::entries`] could find in it. Absent from records written
    /// before it was kept, which are read as "could be anything".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_us: Option<i64>,
}

/// A bucket as the earlier single `buckets` key listed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyBucket {
    period_s: u32,
    entry_type: EntryType,
    /// Unix seconds.
    start: i64,
    expires_ms: i64,
}

/// (period, type, bucket start in Unix seconds).
type BucketId = (u32, EntryType, i64);

/// One day's bucket list, in `buckets:days`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Day {
    period_s: u32,
    entry_type: EntryType,
    /// Unix seconds / [`DAY_S`].
    day: i64,
    expires_ms: i64,
}

/// One day's buckets: `(start, expires_ms)`, oldest first.
type DayList = Vec<(i64, i64)>;

/// A case, as the case index keeps it: what a listing filters and orders by.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CaseRef {
    id: String,
    status: CaseStatus,
    severity: CaseSeverity,
    last_seen: DateTime<Utc>,
    expires_ms: i64,
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

    // --- keys -------------------------------------------------------------

    fn key(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }

    fn batch_key(&self, id: &str) -> String {
        self.key(&format!("batch:{id}"))
    }

    fn bucket_key(&self, (period_s, entry_type, start): BucketId) -> String {
        self.key(&format!("agg:{period_s}:{entry_type}:{start}"))
    }

    fn day_key(&self, period_s: u32, entry_type: EntryType, day: i64) -> String {
        self.key(&format!("buckets:{period_s}:{entry_type}:{day}"))
    }

    fn case_key(&self, id: &str) -> String {
        self.key(&format!("case:{id}"))
    }

    /// How long a day list, and the list of them, live: a day longer than the
    /// buckets, since one is written to for a day after it is created.
    fn day_ttl(&self) -> Duration {
        self.aggregate_ttl.saturating_add(Duration::from_secs(DAY_S as u64))
    }

    // --- commands ---------------------------------------------------------

    async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.cache.get(key).await? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn put_json<T: Serialize>(&self, key: &str, value: &T, ttl: Duration) -> Result<()> {
        self.cache.put(key, &serde_json::to_vec(value)?, Some(ttl)).await
    }

    /// `get` every key, [`IN_FLIGHT`] at a time, in order.
    async fn get_many<T: DeserializeOwned>(&self, keys: Vec<String>) -> Result<Vec<Option<T>>> {
        stream::iter(keys)
            .map(|key| async move { self.get_json::<T>(&key).await })
            .buffered(IN_FLIGHT)
            .try_collect()
            .await
    }

    /// `put` every `(key, bytes, ttl)`, [`IN_FLIGHT`] at a time.
    async fn put_many(&self, puts: Vec<(String, Vec<u8>, Duration)>) -> Result<()> {
        stream::iter(puts)
            .map(|(key, bytes, ttl)| async move { self.cache.put(&key, &bytes, Some(ttl)).await })
            .buffer_unordered(IN_FLIGHT)
            .try_collect::<Vec<()>>()
            .await
            .map(drop)
    }

    /// `forget` every key, [`IN_FLIGHT`] at a time.
    async fn forget_many(&self, keys: Vec<String>) -> Result<()> {
        stream::iter(keys)
            .map(|key| async move { self.cache.forget(&key).await })
            .buffer_unordered(IN_FLIGHT)
            .try_collect::<Vec<bool>>()
            .await
            .map(drop)
    }

    // --- reading the lists --------------------------------------------------

    /// The live index entries, newest first.
    async fn index(&self) -> Result<Vec<Indexed>> {
        let now = now_ms();
        let mut index: Vec<Indexed> = self.get_json(&self.key("index")).await?.unwrap_or_default();
        index.retain(|i| i.expires_ms > now);
        Ok(index)
    }

    /// Every day list, expired ones included.
    async fn days(&self) -> Result<Vec<Day>> {
        Ok(self.get_json(&self.key("buckets:days")).await?.unwrap_or_default())
    }

    /// The earlier layout's single bucket list, if a writer has not yet
    /// folded it in.
    async fn legacy_buckets(&self) -> Result<Option<Vec<LegacyBucket>>> {
        self.get_json(&self.key("buckets")).await
    }

    /// The earlier layout's single case list, if a writer has not yet folded
    /// it in.
    async fn legacy_cases(&self) -> Result<Option<Vec<Case>>> {
        self.get_json(&self.key("cases")).await
    }

    /// The case index, live entries only.
    async fn case_refs(&self) -> Result<Vec<CaseRef>> {
        let now = now_ms();
        let mut refs: Vec<CaseRef> =
            self.get_json(&self.key("cases:index")).await?.unwrap_or_default();
        refs.retain(|r| r.expires_ms > now);
        Ok(refs)
    }

    // --- writing ------------------------------------------------------------

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

    async fn write(&self, batches: Vec<Batch>, counted: Vec<Aggregate>) -> Result<()> {
        let mut unique: BTreeMap<String, Batch> = BTreeMap::new();
        for batch in batches {
            unique.insert(batch.id.clone(), batch);
        }
        if unique.is_empty() {
            if counted.is_empty() {
                return Ok(());
            }
            return self.locked(self.roll_up(counted)).await;
        }
        let batches: Vec<Batch> = unique.into_values().collect();

        // The batches before the index, so a reader that finds an id in the
        // index finds its batch.
        let puts = batches
            .iter()
            .map(|b| Ok((self.batch_key(&b.id), serde_json::to_vec(b)?, self.batch_ttl)))
            .collect::<Result<Vec<_>>>()?;
        self.put_many(puts).await?;

        // Three sets of keys that do not overlap, so they go at once.
        self.locked(async {
            try_join3(
                self.add_to_index(&batches),
                self.roll_up({
                    let mut rolled = aggregate::rollup(&batches);
                    rolled.extend(counted);
                    rolled
                }),
                self.observe_cases(&batches),
            )
            .await
            .map(drop)
        })
        .await
    }

    /// Add `batches` to the index and forget what falls off its end. Under
    /// the lock.
    async fn add_to_index(&self, batches: &[Batch]) -> Result<()> {
        let expires_ms = now_ms().saturating_add(millis(self.batch_ttl));
        let ids: BTreeSet<&str> = batches.iter().map(|b| b.id.as_str()).collect();
        let mut index = self.index().await?;
        index.retain(|i| !ids.contains(i.summary.id.as_str()));
        index.extend(batches.iter().map(|b| {
            Indexed {
                summary: b.summarize(),
                expires_ms,
                last_us: Some(
                    b.entries
                        .iter()
                        .map(|e| e.occurred_at.timestamp_micros())
                        .max()
                        .unwrap_or(i64::MIN),
                ),
            }
        }));
        index.sort_by(|a, b| newest_first(&a.summary, &b.summary));
        let trimmed: Vec<Indexed> = index.split_off(self.capacity.min(index.len()));
        self.put_json(&self.key("index"), &index, self.batch_ttl).await?;
        self.forget_many(trimmed.iter().map(|gone| self.batch_key(&gone.summary.id)).collect())
            .await
    }

    /// Merge `batches`' aggregates into their buckets, and list the buckets
    /// that are new. Under the lock.
    async fn roll_up(&self, rolled: Vec<Aggregate>) -> Result<()> {
        let mut incoming: BTreeMap<BucketId, Vec<Aggregate>> = BTreeMap::new();
        for row in rolled {
            incoming
                .entry((row.period_s, row.entry_type, row.bucket_start.timestamp()))
                .or_default()
                .push(row);
        }
        if incoming.is_empty() {
            return Ok(());
        }
        let ids: Vec<BucketId> = incoming.keys().copied().collect();
        let (stored, legacy) = try_join(
            self.get_many::<Vec<Aggregate>>(ids.iter().map(|&id| self.bucket_key(id)).collect()),
            self.legacy_buckets(),
        )
        .await?;

        let mut fresh = Vec::new();
        let mut puts = Vec::with_capacity(ids.len());
        for ((id, rows), stored) in incoming.into_iter().zip(stored) {
            if stored.is_none() {
                fresh.push(id);
            }
            let mut merged: BTreeMap<String, Aggregate> =
                stored.unwrap_or_default().into_iter().map(|a| (a.key.clone(), a)).collect();
            for row in rows {
                match merged.get_mut(&row.key) {
                    Some(existing) => aggregate::merge(existing, &row),
                    None => {
                        merged.insert(row.key.clone(), row);
                    }
                }
            }
            let merged: Vec<Aggregate> = merged.into_values().collect();
            puts.push((self.bucket_key(id), serde_json::to_vec(&merged)?, self.aggregate_ttl));
        }
        let had_legacy = legacy.is_some();
        fresh.extend(live_legacy(legacy.unwrap_or_default()));
        try_join(self.put_many(puts), self.list_buckets(fresh)).await?;
        if had_legacy {
            self.cache.forget(&self.key("buckets")).await?;
        }
        Ok(())
    }

    /// Add `buckets` to their day lists, and new day lists to the list of
    /// them. Under the lock.
    async fn list_buckets(&self, buckets: Vec<BucketId>) -> Result<()> {
        if buckets.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        let expires_ms = now.saturating_add(millis(self.day_ttl()));
        let mut by_day: BTreeMap<(u32, EntryType, i64), BTreeSet<i64>> = BTreeMap::new();
        for (period_s, entry_type, start) in buckets {
            by_day
                .entry((period_s, entry_type, start.div_euclid(DAY_S)))
                .or_default()
                .insert(start);
        }
        let keys: Vec<String> =
            by_day.keys().map(|&(period_s, t, day)| self.day_key(period_s, t, day)).collect();
        let (lists, mut days) =
            try_join(self.get_many::<DayList>(keys.clone()), self.days()).await?;

        let mut puts = Vec::with_capacity(by_day.len());
        let mut days_changed = false;
        for ((((period_s, entry_type, day), starts), list), key) in
            by_day.into_iter().zip(lists).zip(keys)
        {
            let mut list = list.unwrap_or_default();
            list.retain(|(start, expires)| *expires > now && !starts.contains(start));
            list.extend(starts.into_iter().map(|start| (start, expires_ms)));
            list.sort_unstable();
            puts.push((key, serde_json::to_vec(&list)?, self.day_ttl()));

            // Refreshed at most hourly: the list is rewritten when a bucket
            // first appears, the list of lists only when a day does.
            let same = |d: &Day| (d.period_s, d.entry_type, d.day) == (period_s, entry_type, day);
            match days.iter_mut().find(|d| same(d)) {
                Some(d) if d.expires_ms > expires_ms - 3_600_000 => {}
                Some(d) => {
                    d.expires_ms = expires_ms;
                    days_changed = true;
                }
                None => {
                    days.push(Day { period_s, entry_type, day, expires_ms });
                    days_changed = true;
                }
            }
        }
        if days_changed {
            days.retain(|d| d.expires_ms > now);
            days.sort_by_key(|d| (d.period_s, d.entry_type, d.day));
            puts.push((self.key("buckets:days"), serde_json::to_vec(&days)?, self.day_ttl()));
        }
        self.put_many(puts).await
    }

    /// Fold `batches`' exceptions into the cases they touch. Under the lock.
    async fn observe_cases(&self, batches: &[Batch]) -> Result<()> {
        let seen = occurrences(batches);
        if seen.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> =
            seen.iter().map(|o| o.id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        let (refs, legacy, current) = try_join3(
            self.case_refs(),
            self.legacy_cases(),
            self.get_many::<Case>(ids.iter().map(|id| self.case_key(id)).collect()),
        )
        .await?;
        let mut touched: BTreeMap<String, Case> =
            current.into_iter().flatten().map(|c| (c.id.clone(), c)).collect();
        let mut refs: BTreeMap<String, CaseRef> =
            refs.into_iter().map(|r| (r.id.clone(), r)).collect();

        // The earlier layout's cases, moved over once.
        let had_legacy = legacy.is_some();
        let mut adopted = Vec::new();
        for case in legacy.unwrap_or_default() {
            if refs.contains_key(&case.id) || touched.contains_key(&case.id) {
                continue;
            }
            if ids.contains(&case.id) {
                touched.insert(case.id.clone(), case);
            } else {
                adopted.push(case);
            }
        }

        for occurrence in &seen {
            let existing = touched.remove(&occurrence.id);
            touched.insert(occurrence.id.clone(), observe(existing, occurrence));
        }
        let changed: Vec<Case> = touched.into_values().chain(adopted).collect();
        self.put_cases(refs_with(&mut refs, &changed, self.case_expiry()), changed).await?;
        if had_legacy {
            self.cache.forget(&self.key("cases")).await?;
        }
        Ok(())
    }

    fn case_expiry(&self) -> i64 {
        now_ms().saturating_add(millis(self.aggregate_ttl))
    }

    /// Write `changed` and the index `refs`, keeping at most [`MAX_CASES`]:
    /// the ones seen longest ago go first, closed before open.
    async fn put_cases(&self, mut refs: Vec<CaseRef>, changed: Vec<Case>) -> Result<()> {
        let mut dropped = BTreeSet::new();
        if refs.len() > MAX_CASES {
            refs.sort_by(|a, b| {
                (b.status == CaseStatus::Open)
                    .cmp(&(a.status == CaseStatus::Open))
                    .then_with(|| b.last_seen.cmp(&a.last_seen))
            });
            dropped = refs.split_off(MAX_CASES).into_iter().map(|r| r.id).collect();
        }
        let mut puts = Vec::with_capacity(changed.len() + 1);
        for case in changed.iter().filter(|c| !dropped.contains(&c.id)) {
            puts.push((self.case_key(&case.id), serde_json::to_vec(case)?, self.aggregate_ttl));
        }
        puts.push((self.key("cases:index"), serde_json::to_vec(&refs)?, self.aggregate_ttl));
        try_join(
            self.put_many(puts),
            self.forget_many(dropped.iter().map(|id| self.case_key(id)).collect()),
        )
        .await
        .map(drop)
    }

    async fn save(&self, case: Case) -> Result<()> {
        self.locked(async {
            let (refs, legacy) = try_join(self.case_refs(), self.legacy_cases()).await?;
            let mut refs: BTreeMap<String, CaseRef> =
                refs.into_iter().map(|r| (r.id.clone(), r)).collect();
            let had_legacy = legacy.is_some();
            let mut changed: Vec<Case> = legacy
                .unwrap_or_default()
                .into_iter()
                .filter(|c| c.id != case.id && !refs.contains_key(&c.id))
                .collect();
            changed.push(case);
            self.put_cases(refs_with(&mut refs, &changed, self.case_expiry()), changed).await?;
            if had_legacy {
                self.cache.forget(&self.key("cases")).await?;
            }
            Ok(())
        })
        .await
    }

    // --- reading ------------------------------------------------------------

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
        key: Option<&str>,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<Entry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // An entry starts no earlier than its batch, so a batch that started
        // at or after `before` holds nothing earlier than it.
        let candidates: Vec<(String, i64)> = self
            .index()
            .await?
            .into_iter()
            .filter(|i| i.summary.counts.get(&entry_type).copied().unwrap_or(0) > 0)
            .filter(|i| before.is_none_or(|before| i.summary.started_at < before))
            .map(|i| (i.summary.id, i.last_us.unwrap_or(i64::MAX)))
            .collect();
        // `latest[i]`: the newest entry any candidate from `i` on could hold.
        let mut latest = vec![i64::MIN; candidates.len() + 1];
        for i in (0..candidates.len()).rev() {
            latest[i] = latest[i + 1].max(candidates[i].1);
        }

        // As the database store orders them: newest first, then batch id and
        // position descending.
        let order = |(sa, a): &(usize, Entry), (sb, b): &(usize, Entry)| {
            b.occurred_at
                .cmp(&a.occurred_at)
                .then_with(|| b.batch_id.cmp(&a.batch_id))
                .then_with(|| sb.cmp(sa))
        };
        let mut found: Vec<(usize, Entry)> = Vec::new();
        let mut next = 0;
        let mut wave = FIRST_WAVE;
        while next < candidates.len() {
            let end = (next + wave).min(candidates.len());
            let keys = candidates[next..end].iter().map(|(id, _)| self.batch_key(id)).collect();
            for batch in self.get_many::<Batch>(keys).await?.into_iter().flatten() {
                for (seq, entry) in batch.entries.into_iter().enumerate() {
                    if entry.entry_type == entry_type
                        && key.is_none_or(|key| entry.family_key() == key)
                        && before.is_none_or(|before| entry.occurred_at < before)
                    {
                        found.push((seq, entry));
                    }
                }
            }
            next = end;
            wave = (wave * 2).min(MAX_WAVE);
            if found.len() >= limit {
                found.sort_by(order);
                found.truncate(limit);
                // Enough, unless a batch not read yet could still hold an
                // entry newer than the last one kept.
                if latest[next] < found[limit - 1].1.occurred_at.timestamp_micros() {
                    break;
                }
            }
        }
        found.sort_by(order);
        Ok(found.into_iter().take(limit).map(|(_, e)| e).collect())
    }

    async fn rows(
        &self,
        entry_type: EntryType,
        period_s: u32,
        since: DateTime<Utc>,
    ) -> Result<Vec<Aggregate>> {
        let from = bucket_start(since, period_s).timestamp();
        let now = now_ms();
        let (days, legacy) = try_join(self.days(), self.legacy_buckets()).await?;
        let wanted: Vec<String> = days
            .iter()
            .filter(|d| d.period_s == period_s && d.entry_type == entry_type)
            .filter(|d| d.expires_ms > now && (d.day + 1) * DAY_S > from)
            .map(|d| self.day_key(d.period_s, d.entry_type, d.day))
            .collect();
        let mut starts: BTreeSet<i64> = self
            .get_many::<DayList>(wanted)
            .await?
            .into_iter()
            .flatten()
            .flatten()
            .filter(|&(start, expires)| start >= from && expires > now)
            .map(|(start, _)| start)
            .collect();
        starts.extend(
            live_legacy(legacy.unwrap_or_default())
                .filter(|&(p, t, start)| p == period_s && t == entry_type && start >= from)
                .map(|(_, _, start)| start),
        );
        Ok(self
            .get_many::<Vec<Aggregate>>(
                starts
                    .into_iter()
                    .map(|start| self.bucket_key((period_s, entry_type, start)))
                    .collect(),
            )
            .await?
            .into_iter()
            .flatten()
            .flatten()
            .collect())
    }

    async fn list_cases(&self, query: CaseQuery) -> Result<Vec<Case>> {
        let (refs, legacy) = try_join(self.case_refs(), self.legacy_cases()).await?;
        let mut refs: Vec<CaseRef> = refs
            .into_iter()
            .filter(|r| query.status.is_none_or(|s| r.status == s))
            .filter(|r| query.min_severity.is_none_or(|s| r.severity >= s))
            .collect();
        // Without a search the index alone says which cases make the page,
        // in `select_cases`'s order; a search reads them all.
        if query.search.is_none() {
            let rank = |s: CaseStatus| match s {
                CaseStatus::Open => 0,
                CaseStatus::Resolved => 1,
                CaseStatus::Ignored => 2,
            };
            refs.sort_by(|a, b| {
                rank(a.status)
                    .cmp(&rank(b.status))
                    .then_with(|| b.severity.cmp(&a.severity))
                    .then_with(|| b.last_seen.cmp(&a.last_seen))
                    .then_with(|| a.id.cmp(&b.id))
            });
            refs.truncate(query.limit.unwrap_or(50).min(200));
        }
        let mut all: BTreeMap<String, Case> = self
            .get_many::<Case>(refs.iter().map(|r| self.case_key(&r.id)).collect())
            .await?
            .into_iter()
            .flatten()
            .map(|c| (c.id.clone(), c))
            .collect();
        for case in legacy.unwrap_or_default() {
            all.entry(case.id.clone()).or_insert(case);
        }
        Ok(select_cases(all.into_values(), &query))
    }

    async fn find_case(&self, id: &str) -> Result<Option<Case>> {
        if let Some(case) = self.get_json::<Case>(&self.case_key(id)).await? {
            return Ok(Some(case));
        }
        Ok(self.legacy_cases().await?.unwrap_or_default().into_iter().find(|c| c.id == id))
    }

    // --- pruning --------------------------------------------------------------

    async fn prune_keys(
        &self,
        batches_before: DateTime<Utc>,
        aggregates_before: DateTime<Utc>,
    ) -> Result<u64> {
        self.locked(async {
            let (gone, (), ()) = try_join3(
                self.prune_batches(batches_before),
                self.prune_buckets(aggregates_before),
                self.prune_cases(aggregates_before),
            )
            .await?;
            Ok(gone)
        })
        .await
    }

    async fn prune_batches(&self, before: DateTime<Utc>) -> Result<u64> {
        let (gone, kept): (Vec<Indexed>, Vec<Indexed>) =
            self.index().await?.into_iter().partition(|i| i.summary.started_at < before);
        if !gone.is_empty() {
            self.put_json(&self.key("index"), &kept, self.batch_ttl).await?;
            self.forget_many(gone.iter().map(|b| self.batch_key(&b.summary.id)).collect()).await?;
        }
        Ok(gone.len() as u64)
    }

    /// Fold the earlier layout's bucket list into the day lists. Under the
    /// lock.
    async fn adopt_legacy_buckets(&self) -> Result<()> {
        if let Some(legacy) = self.legacy_buckets().await? {
            self.list_buckets(live_legacy(legacy).collect()).await?;
            self.cache.forget(&self.key("buckets")).await?;
        }
        Ok(())
    }

    async fn prune_buckets(&self, before: DateTime<Utc>) -> Result<()> {
        self.adopt_legacy_buckets().await?;
        // `bucket_start < before`, in whole seconds.
        let cutoff = before.timestamp() + i64::from(before.timestamp_subsec_nanos() > 0);
        let now = now_ms();
        let days = self.days().await?;
        let (old, mut kept): (Vec<Day>, Vec<Day>) =
            days.into_iter().partition(|d| d.day * DAY_S < cutoff || d.expires_ms <= now);
        if old.is_empty() {
            return Ok(());
        }
        let keys: Vec<String> =
            old.iter().map(|d| self.day_key(d.period_s, d.entry_type, d.day)).collect();
        let lists = self.get_many::<DayList>(keys.clone()).await?;
        let mut forget = Vec::new();
        let mut puts = Vec::new();
        for ((day, list), key) in old.into_iter().zip(lists).zip(keys) {
            let (gone, left): (DayList, DayList) =
                list.unwrap_or_default().into_iter().partition(|&(start, _)| start < cutoff);
            forget.extend(
                gone.iter()
                    .map(|&(start, _)| self.bucket_key((day.period_s, day.entry_type, start))),
            );
            if left.is_empty() || day.expires_ms <= now {
                forget.push(key);
            } else {
                puts.push((key, serde_json::to_vec(&left)?, self.day_ttl()));
                kept.push(day);
            }
        }
        kept.sort_by_key(|d| (d.period_s, d.entry_type, d.day));
        puts.push((self.key("buckets:days"), serde_json::to_vec(&kept)?, self.day_ttl()));
        try_join(self.put_many(puts), self.forget_many(forget)).await.map(drop)
    }

    async fn prune_cases(&self, before: DateTime<Utc>) -> Result<()> {
        let (refs, legacy) = try_join(self.case_refs(), self.legacy_cases()).await?;
        let mut refs: BTreeMap<String, CaseRef> =
            refs.into_iter().map(|r| (r.id.clone(), r)).collect();
        let had_legacy = legacy.is_some();
        let adopted: Vec<Case> = legacy
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.last_seen >= before && !refs.contains_key(&c.id))
            .collect();
        let gone: Vec<String> =
            refs.values().filter(|r| r.last_seen < before).map(|r| r.id.clone()).collect();
        if gone.is_empty() && !had_legacy {
            return Ok(());
        }
        refs.retain(|_, r| r.last_seen >= before);
        let refs = refs_with(&mut refs, &adopted, self.case_expiry());
        try_join(
            self.put_cases(refs, adopted),
            self.forget_many(gone.iter().map(|id| self.case_key(id)).collect()),
        )
        .await?;
        if had_legacy {
            self.cache.forget(&self.key("cases")).await?;
        }
        Ok(())
    }

    async fn clear_keys(&self) -> Result<()> {
        self.locked(async {
            self.adopt_legacy_buckets().await?;
            let (index, days, refs) = try_join3(
                self.get_json::<Vec<Indexed>>(&self.key("index")),
                self.days(),
                self.get_json::<Vec<CaseRef>>(&self.key("cases:index")),
            )
            .await?;
            let day_keys: Vec<String> =
                days.iter().map(|d| self.day_key(d.period_s, d.entry_type, d.day)).collect();
            let lists = self.get_many::<DayList>(day_keys.clone()).await?;
            let mut forget: Vec<String> = index
                .unwrap_or_default()
                .iter()
                .map(|i| self.batch_key(&i.summary.id))
                .chain(refs.unwrap_or_default().iter().map(|r| self.case_key(&r.id)))
                .collect();
            for (day, list) in days.iter().zip(lists) {
                forget.extend(
                    list.unwrap_or_default()
                        .iter()
                        .map(|&(start, _)| self.bucket_key((day.period_s, day.entry_type, start))),
                );
            }
            forget.extend(day_keys);
            forget.extend(
                ["index", "buckets", "buckets:days", "cases", "cases:index"].map(|k| self.key(k)),
            );
            self.forget_many(forget).await
        })
        .await
    }
}

/// `refs` with `changed` put in, as a list.
fn refs_with(
    refs: &mut BTreeMap<String, CaseRef>,
    changed: &[Case],
    expires_ms: i64,
) -> Vec<CaseRef> {
    for case in changed {
        refs.insert(
            case.id.clone(),
            CaseRef {
                id: case.id.clone(),
                status: case.status,
                severity: case.severity,
                last_seen: case.last_seen,
                expires_ms,
            },
        );
    }
    refs.values().cloned().collect()
}

/// The buckets of the earlier layout's list that have not expired.
fn live_legacy(legacy: Vec<LegacyBucket>) -> impl Iterator<Item = BucketId> {
    let now = now_ms();
    legacy
        .into_iter()
        .filter(move |b| b.expires_ms > now)
        .map(|b| (b.period_s, b.entry_type, b.start))
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
        Box::pin(self.write(batches, Vec::new()).instrument(internal("store")))
    }

    fn store_counting(
        &self,
        batches: Vec<Batch>,
        counted: Vec<Aggregate>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.write(batches, counted).instrument(internal("store")))
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
        Box::pin(self.of_type(entry_type, None, before, limit).instrument(internal("entries")))
    }

    fn family_entries(
        &self,
        entry_type: EntryType,
        key: &str,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<Entry>>> {
        let key = key.to_string();
        Box::pin(async move {
            self.of_type(entry_type, Some(&key), before, limit)
                .instrument(internal("family_entries"))
                .await
        })
    }

    fn aggregate_rows(
        &self,
        entry_type: EntryType,
        period_s: u32,
        since: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<Vec<Aggregate>>> {
        Box::pin(self.rows(entry_type, period_s, since).instrument(internal("aggregates")))
    }

    fn cases(&self, query: CaseQuery) -> BoxFuture<'_, Result<Vec<Case>>> {
        Box::pin(self.list_cases(query).instrument(internal("cases")))
    }

    fn case(&self, id: &str) -> BoxFuture<'_, Result<Option<Case>>> {
        let id = id.to_string();
        Box::pin(async move { self.find_case(&id).await }.instrument(internal("case")))
    }

    fn save_case(&self, case: Case) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.save(case).instrument(internal("save_case")))
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
    use crate::store::AggregateQuery;
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

    /// `entries` stops reading batches early; whatever the batches' spans,
    /// it must answer exactly what reading every batch would.
    #[tokio::test]
    async fn entries_stop_early_and_still_answer_exactly() {
        use crate::store::conformance::entry;
        let cache_store = store(1_000);
        let mut batches = Vec::new();
        for b in 0..300i64 {
            let id = format!("b{b:03}");
            let mut batch = batch(&id, BatchKind::Request, at(b * 2, 0));
            // Some batches run long, so their entries land among later ones,
            // and some entries tie to the microsecond.
            let span = if b % 7 == 0 { 400 } else { 3 };
            for n in 0..5usize {
                let offset = (b * 31 + n as i64 * 17) % span;
                let key = if n % 2 == 0 { "even" } else { "odd" };
                batch.entries.push(entry(
                    &id,
                    n,
                    EntryType::Query,
                    key,
                    at(b * 2 + offset, n as u32),
                    Some(5),
                ));
            }
            batches.push(batch);
        }
        for chunk in batches.chunks(40) {
            cache_store.store(chunk.to_vec()).await.unwrap();
        }
        // Every batch read, sorted as the store promises.
        let reference = |key: Option<&str>, before: Option<DateTime<Utc>>, limit: usize| {
            let mut all: Vec<(usize, Entry)> = batches
                .iter()
                .flat_map(|b| b.entries.iter().cloned().enumerate())
                .filter(|(_, e)| key.is_none_or(|k| e.family_key() == k))
                .filter(|(_, e)| before.is_none_or(|b| e.occurred_at < b))
                .collect();
            all.sort_by(|(sa, a), (sb, b)| {
                b.occurred_at
                    .cmp(&a.occurred_at)
                    .then_with(|| b.batch_id.cmp(&a.batch_id))
                    .then_with(|| sb.cmp(sa))
            });
            all.into_iter().take(limit).map(|(_, e)| e.id).collect::<Vec<_>>()
        };
        let ids = |es: Vec<Entry>| es.into_iter().map(|e| e.id).collect::<Vec<_>>();
        for limit in [1, 7, 50, 200] {
            for before in [None, Some(at(300, 0)), Some(at(599, 0)), Some(at(10, 0))] {
                let got = cache_store.entries(EntryType::Query, before, limit).await.unwrap();
                assert_eq!(ids(got), reference(None, before, limit), "{limit}, {before:?}");
                let got = cache_store
                    .family_entries(EntryType::Query, "odd", before, limit)
                    .await
                    .unwrap();
                assert_eq!(ids(got), reference(Some("odd"), before, limit), "{limit}, {before:?}");
            }
        }
    }

    /// The earlier layout kept every bucket in one `buckets` key and every
    /// case in one `cases` key. Readers see them; the next write folds them
    /// into the current layout and deletes them.
    #[tokio::test]
    async fn the_earlier_single_key_layout_is_read_and_then_folded_in() {
        use crate::cases::{case_id, Occurrence};
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let store = CacheStore::new(Arc::clone(&cache), 10);
        let far = now_ms() + 86_400_000;

        // What the earlier version wrote: a bucket, listed in `buckets`.
        let row = Aggregate {
            bucket_start: at(0, 0),
            period_s: 60,
            entry_type: EntryType::Query,
            key: "select 1".into(),
            count: 3,
            sum_us: 30,
            max_us: 20,
            histogram: Default::default(),
        };
        let start = at(0, 0).timestamp();
        cache
            .put(
                &format!("lookout:agg:60:query:{start}"),
                &serde_json::to_vec(&vec![row]).unwrap(),
                None,
            )
            .await
            .unwrap();
        let legacy = vec![LegacyBucket {
            period_s: 60,
            entry_type: EntryType::Query,
            start,
            expires_ms: far,
        }];
        cache.put("lookout:buckets", &serde_json::to_vec(&legacy).unwrap(), None).await.unwrap();
        // And a case, in `cases`.
        let occurrence = |key: &str, s: i64| Occurrence {
            id: case_id(key),
            key: key.into(),
            title: key.into(),
            failure_kind: "HttpRequest".into(),
            severity: CaseSeverity::High,
            at: at(s, 0),
            batch_id: "old".into(),
            source: None,
            user: None,
            frames: vec![],
        };
        let old_case = observe(None, &occurrence("old failure", 0));
        cache
            .put("lookout:cases", &serde_json::to_vec(&vec![old_case.clone()]).unwrap(), None)
            .await
            .unwrap();

        // Read before any write.
        let rows = store.aggregate_rows(EntryType::Query, 60, at(0, 0)).await.unwrap();
        assert_eq!(rows.iter().map(|r| r.count).sum::<u64>(), 3);
        assert_eq!(store.cases(CaseQuery::default()).await.unwrap().len(), 1);
        assert!(store.case(&old_case.id).await.unwrap().is_some());

        // A write into the same bucket, with an exception.
        let mut b = batch("b", BatchKind::Request, at(1, 0));
        b.entries = vec![
            crate::store::conformance::entry(
                "b",
                0,
                EntryType::Query,
                "select 1",
                at(1, 0),
                Some(50),
            ),
            crate::store::conformance::entry(
                "b",
                1,
                EntryType::Exception,
                "new failure",
                at(1, 1),
                None,
            ),
        ];
        store.store(vec![b]).await.unwrap();
        assert!(!cache.has("lookout:buckets").await.unwrap(), "the bucket list is folded in");
        assert!(!cache.has("lookout:cases").await.unwrap(), "the case list is folded in");

        let rows = store.aggregate_rows(EntryType::Query, 60, at(0, 0)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].count, rows[0].max_us), (4, 50));
        let cases = store.cases(CaseQuery::default()).await.unwrap();
        assert_eq!(cases.len(), 2, "{cases:?}");
        assert!(store.case(&old_case.id).await.unwrap().is_some());

        // Pruning the old aggregates away removes the bucket.
        store.prune(at(-10, 0), at(120, 0)).await.unwrap();
        assert!(store.aggregate_rows(EntryType::Query, 60, at(0, 0)).await.unwrap().is_empty());
        assert!(!cache.has(&format!("lookout:agg:60:query:{start}")).await.unwrap());
    }

    /// A busy write touches a few small keys, not every bucket and case ever
    /// kept: the day's bucket list is rewritten only when a bucket first
    /// appears.
    #[tokio::test]
    async fn a_write_into_existing_buckets_leaves_the_lists_alone() {
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let store = CacheStore::new(Arc::clone(&cache), 10);
        let one = |id: &str, s: i64| {
            let mut b = batch(id, BatchKind::Request, at(s, 0));
            b.entries = vec![crate::store::conformance::entry(
                id,
                0,
                EntryType::Query,
                "q",
                at(s, 0),
                Some(5),
            )];
            b
        };
        store.store(vec![one("a", 0)]).await.unwrap();
        let day = at(0, 0).timestamp().div_euclid(DAY_S);
        let list_key = format!("lookout:buckets:60:query:{day}");
        let before = cache.get(&list_key).await.unwrap().unwrap();
        // Same minute: no new bucket, so the list is not rewritten. Swap its
        // contents to see whether it is.
        cache
            .put(
                &list_key,
                &serde_json::to_vec(&vec![(at(0, 0).timestamp(), now_ms() + 60_000)]).unwrap(),
                None,
            )
            .await
            .unwrap();
        store.store(vec![one("b", 5)]).await.unwrap();
        let after = cache.get(&list_key).await.unwrap().unwrap();
        assert_ne!(before, after, "untouched by a write into an existing bucket");
        // The next minute is a new bucket: listed.
        store.store(vec![one("c", 65)]).await.unwrap();
        let list: DayList =
            serde_json::from_slice(&cache.get(&list_key).await.unwrap().unwrap()).unwrap();
        assert_eq!(list.len(), 2);
    }
}
