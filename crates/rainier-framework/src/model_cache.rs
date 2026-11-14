//! A cache of hot rows, dropped the moment the row changes.
//!
//! [`ModelCache`] keeps rows of one model in the application's cache — Redis
//! in production, so every replica shares them — but **only the rows that are
//! being read often**. A row read once an hour never touches the cache: it is
//! loaded from the database exactly as it would be without one. A row read
//! several times in a few seconds (a popular post, the profile on every request
//! its owner makes) is served from the cache until it changes.
//!
//! # Invalidated by the write, whatever wrote it
//!
//! A cache built on a TTL alone serves a stale row for up to that TTL after
//! every edit. This one subscribes to its table through
//! [`Database::on_change`], which hears **every** write a statement builder or
//! `rainier_orm::repo` makes — a repository update, a bulk `update_matching`, a
//! counter `SET n = n + 1`, a delete — and is awaited before the write
//! returns. So:
//!
//! - a write that names its rows (by key, or with the key pinned in its
//!   criteria) **tombstones** each of them: the cached copy is replaced by a
//!   marker for a few seconds, and a reader that loaded the old row just before
//!   the write cannot put it back, because it repopulates with an atomic
//!   set-if-absent and the marker is there;
//! - a write that does not name its rows (a bulk update by another column)
//!   advances the table's **generation**, which retires every cached row of
//!   it at once. Other replicas see a new generation within
//!   [`ModelCacheConfig::generation_refresh`].
//!
//! The TTL ([`ModelCacheConfig::ttl`]) is the backstop for the one write this
//! cannot hear: SQL passed as a string.
//!
//! # What "hot" means
//!
//! Each process counts reads per key over a rolling window
//! ([`ModelCacheConfig::hot_window`]); a key read at least
//! [`ModelCacheConfig::hot_threshold`] times in this window or the last is
//! hot. The count is per process and bounded
//! ([`ModelCacheConfig::max_tracked`]), so it costs a hash map, not a cache
//! round trip. A hot read costs one cache `GET`; a cold one costs nothing
//! beyond the database read it was going to make anyway.
//!
//! # Lookups by something other than the key
//!
//! [`get_by`](ModelCache::get_by) caches an index — `username → id` — and then
//! the row by its key, and checks the row still matches before trusting the
//! index. A write reports the key it changed, not the old username; a renamed
//! row simply fails the check and is looked up again.
//!
//! # Failure
//!
//! A cache that is down, slow to answer, or holding something that does not
//! decode is a miss: the row comes from the database. Nothing here makes a read
//! fail that would have succeeded without it.
//!
//! Each failure worked around — a read, a write, an invalidation, a generation
//! that could not be read or advanced, a cached row that did not decode — is
//! reported as a [`CacheFailed`] with store `model_cache`, the key when there
//! is one, and `meta.extra["table"]`, through the process-wide reporter. An
//! application that does not want them ignores
//! [`FailureKind::Cache`](rainier_events::FailureKind::Cache).

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rainier_cache::Cache;
use rainier_database::{ChangeKind, ChangedKeys, Database, RowChange};
use rainier_events::report::{CacheFailed, FailureEvent as _};
use rainier_events::Exception;
use rainier_orm::Entity;
use rainier_support::Result;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// What a tombstoned key holds: not a JSON document, so it cannot be mistaken
/// for one.
const TOMBSTONE: &[u8] = b"\0tombstone";

/// How a [`ModelCache`] decides what is hot and how long it keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCacheConfig {
    /// The longest a cached row lives, invalidated or not. The backstop for a
    /// write the cache cannot hear about.
    pub ttl: Duration,
    /// How long a tombstone holds a changed key against a reader that loaded
    /// the old row just before the write. Longer than any read takes.
    pub tombstone_ttl: Duration,
    /// Reads of one key, within a window, that make it hot.
    pub hot_threshold: u32,
    /// The window reads are counted over. A key stays hot for the window after
    /// the one it qualified in.
    pub hot_window: Duration,
    /// The most keys one process counts at once; past it, new keys are cold
    /// until the window rolls.
    pub max_tracked: usize,
    /// How long a process trusts its copy of the table generation before
    /// asking the cache again — the delay before another replica's bulk write
    /// retires this one's view.
    pub generation_refresh: Duration,
}

impl Default for ModelCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            tombstone_ttl: Duration::from_secs(10),
            hot_threshold: 3,
            hot_window: Duration::from_secs(10),
            max_tracked: 50_000,
            generation_refresh: Duration::from_secs(1),
        }
    }
}

/// Counts since start, for a dashboard or a test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCacheStats {
    /// Reads of keys that were not hot: straight to the database.
    pub cold: u64,
    /// Hot reads served from the cache.
    pub hits: u64,
    /// Hot reads that went to the database and populated the cache.
    pub misses: u64,
    /// Hot reads of a key changed in the last few seconds: the database, and
    /// no populate.
    pub tombstoned: u64,
    /// Rows invalidated by a write.
    pub invalidated: u64,
    /// Generations advanced by a write that did not name its rows.
    pub generations: u64,
}

/// A cache of the hot rows of `M`. See the module docs. Cheap to clone.
pub struct ModelCache<M> {
    inner: Arc<Inner>,
    _model: PhantomData<fn() -> M>,
}

impl<M> Clone for ModelCache<M> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner), _model: PhantomData }
    }
}

struct Inner {
    cache: Arc<dyn Cache>,
    table: &'static str,
    config: ModelCacheConfig,
    hot: Mutex<HotWindow>,
    generation: Mutex<Generation>,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    cold: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    tombstoned: AtomicU64,
    invalidated: AtomicU64,
    generations: AtomicU64,
}

struct HotWindow {
    started: Instant,
    current: HashMap<String, u32>,
    previous: HashMap<String, u32>,
}

struct Generation {
    value: i64,
    fetched: Option<Instant>,
}

impl<M> ModelCache<M>
where
    M: Entity + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// A cache of `M` in `cache`, invalidated by `db`'s writes, with the
    /// default configuration.
    pub fn new(cache: Arc<dyn Cache>, db: &Database) -> Self {
        Self::with_config(cache, db, ModelCacheConfig::default())
    }

    /// As [`new`](Self::new), with `config`.
    pub fn with_config(cache: Arc<dyn Cache>, db: &Database, config: ModelCacheConfig) -> Self {
        if !cache.supports_atomic_add() {
            tracing::warn!(
                table = M::table(),
                store = cache.name(),
                "this cache store cannot add atomically, so a read racing a write can put a \
                 stale row back for up to the TTL"
            );
        }
        let inner = Arc::new(Inner {
            cache,
            table: M::table(),
            config,
            hot: Mutex::new(HotWindow {
                started: Instant::now(),
                current: HashMap::new(),
                previous: HashMap::new(),
            }),
            generation: Mutex::new(Generation { value: 0, fetched: None }),
            stats: Stats::default(),
        });
        let subscriber = Arc::clone(&inner);
        db.on_change(move |change: RowChange| {
            let inner = Arc::clone(&subscriber);
            async move { inner.apply(change).await }
        });
        Self { inner, _model: PhantomData }
    }

    /// The row keyed `key`: from the cache when it is hot and cached, otherwise
    /// from `load` — and into the cache, when it is hot.
    ///
    /// `key` is the primary key as a string, as a [`RowChange`] names it.
    ///
    /// # Errors
    ///
    /// Only `load`'s. The cache never fails a read.
    pub async fn get<F, Fut>(&self, key: &str, load: F) -> Result<Option<M>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<M>>>,
    {
        let inner = &self.inner;
        if !inner.record_read(key) {
            inner.stats.cold.fetch_add(1, Ordering::Relaxed);
            return load().await;
        }

        let entry = inner.entry_key(inner.generation(false).await, key);
        match inner.cache.get(&entry).await {
            Ok(Some(bytes)) if bytes == TOMBSTONE => {
                inner.stats.tombstoned.fetch_add(1, Ordering::Relaxed);
                return load().await;
            }
            Ok(Some(bytes)) => match serde_json::from_slice::<M>(&bytes) {
                Ok(row) => {
                    inner.stats.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(row));
                }
                // A row cached by another version of the model, usually:
                // a miss, and replaced once its TTL runs out.
                Err(e) => report(inner.table, Some(key), "decode", Exception::from_std(&e)).await,
            },
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e.message(), table = inner.table, "model cache unreachable; reading the database");
                report(inner.table, Some(key), "get", e).await;
                return load().await;
            }
        }

        inner.stats.misses.fetch_add(1, Ordering::Relaxed);
        let loaded = load().await?;
        if let Some(row) = &loaded {
            if let Ok(bytes) = serde_json::to_vec(row) {
                // Set-if-absent: a tombstone written since this read began
                // wins, and the row this read saw is not put back.
                if let Err(e) = inner.cache.add(&entry, &bytes, Some(inner.config.ttl)).await {
                    report(inner.table, Some(key), "fill", e).await;
                }
            }
        }
        Ok(loaded)
    }

    /// The row whose `index` is `value` (a username, a slug): through a cached
    /// `index → key` entry and [`get`](Self::get), when hot.
    ///
    /// `key_of` gives a row's primary key, and `matches` whether a row still
    /// has `value` — the check that makes a renamed row a miss rather than a
    /// wrong answer. `load` looks the row up by `value`.
    pub async fn get_by<K, P, F, Fut>(
        &self,
        index: &str,
        value: &str,
        key_of: K,
        matches: P,
        load: F,
    ) -> Result<Option<M>>
    where
        K: Fn(&M) -> String,
        P: Fn(&M) -> bool,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<M>>>,
    {
        let inner = &self.inner;
        let index_key = format!("{index}={value}");
        if !inner.record_read(&index_key) {
            inner.stats.cold.fetch_add(1, Ordering::Relaxed);
            return load().await;
        }

        let pointer = inner.entry_key(inner.generation(false).await, &index_key);
        if let Ok(Some(bytes)) = inner.cache.get(&pointer).await {
            if let Ok(key) = String::from_utf8(bytes) {
                // The row by its key is hot whenever its index is.
                inner.mark_hot(&key);
                let by_key = self.get(&key, || async { Ok(None) }).await?;
                if let Some(row) = by_key.filter(|row| matches(row)) {
                    return Ok(Some(row));
                }
            }
        }

        inner.stats.misses.fetch_add(1, Ordering::Relaxed);
        let loaded = load().await?;
        if let Some(row) = &loaded {
            let key = key_of(row);
            let ttl = Some(inner.config.ttl);
            // A plain put: the pointer is checked on every use, so a stale one
            // costs a miss, never a wrong row.
            if let Err(e) = inner.cache.put(&pointer, key.as_bytes(), ttl).await {
                report(inner.table, Some(index_key.as_str()), "fill_index", e).await;
            }
            inner.mark_hot(&key);
            let entry = inner.entry_key(inner.generation(false).await, &key);
            if let Ok(bytes) = serde_json::to_vec(row) {
                if let Err(e) = inner.cache.add(&entry, &bytes, ttl).await {
                    report(inner.table, Some(key.as_str()), "fill", e).await;
                }
            }
        }
        Ok(loaded)
    }

    /// Drop the cached copy of `key` — what a write does by itself; for a
    /// change made where the database cannot see it.
    pub async fn invalidate(&self, key: &str) {
        self.inner.invalidate(key).await;
    }

    /// Retire every cached row of the table.
    pub async fn invalidate_all(&self) {
        self.inner.advance_generation().await;
    }

    /// Counts since start.
    pub fn stats(&self) -> ModelCacheStats {
        let s = &self.inner.stats;
        ModelCacheStats {
            cold: s.cold.load(Ordering::Relaxed),
            hits: s.hits.load(Ordering::Relaxed),
            misses: s.misses.load(Ordering::Relaxed),
            tombstoned: s.tombstoned.load(Ordering::Relaxed),
            invalidated: s.invalidated.load(Ordering::Relaxed),
            generations: s.generations.load(Ordering::Relaxed),
        }
    }
}

impl Inner {
    fn generation_key(&self) -> String {
        format!("model-cache:{}:generation", self.table)
    }

    fn entry_key(&self, generation: i64, key: &str) -> String {
        format!("model-cache:{}:{generation}:{key}", self.table)
    }

    /// Count a read of `key`; whether it is hot.
    fn record_read(&self, key: &str) -> bool {
        let Ok(mut hot) = self.hot.lock() else { return false };
        self.roll(&mut hot);
        let previous = hot.previous.get(key).copied().unwrap_or(0);
        if !hot.current.contains_key(key) && hot.current.len() >= self.config.max_tracked {
            return previous >= self.config.hot_threshold;
        }
        let count = hot.current.entry(key.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        *count >= self.config.hot_threshold || previous >= self.config.hot_threshold
    }

    /// Make `key` hot without a read — a row reached through a hot index.
    fn mark_hot(&self, key: &str) {
        let Ok(mut hot) = self.hot.lock() else { return };
        self.roll(&mut hot);
        if hot.current.len() < self.config.max_tracked || hot.current.contains_key(key) {
            let threshold = self.config.hot_threshold;
            let count = hot.current.entry(key.to_string()).or_insert(0);
            *count = (*count).max(threshold);
        }
    }

    fn roll(&self, hot: &mut HotWindow) {
        let elapsed = hot.started.elapsed();
        if elapsed < self.config.hot_window {
            return;
        }
        // More than one window idle: nothing from before is recent.
        hot.previous = if elapsed < self.config.hot_window * 2 {
            std::mem::take(&mut hot.current)
        } else {
            hot.current.clear();
            HashMap::new()
        };
        hot.started = Instant::now();
    }

    /// The table generation: this process's copy while it is fresh, else the
    /// cache's. `force` asks the cache regardless.
    async fn generation(&self, force: bool) -> i64 {
        if !force {
            if let Ok(local) = self.generation.lock() {
                if local.fetched.is_some_and(|at| at.elapsed() < self.config.generation_refresh) {
                    return local.value;
                }
            }
        }
        let shared = match self.cache.get(&self.generation_key()).await {
            Ok(Some(bytes)) => {
                String::from_utf8(bytes).ok().and_then(|s| s.trim().parse::<i64>().ok())
            }
            Ok(None) => Some(0),
            Err(e) => {
                report(self.table, None, "read_generation", e).await;
                None
            }
        };
        let Ok(mut local) = self.generation.lock() else { return shared.unwrap_or(0) };
        if let Some(value) = shared {
            // Never backwards: a stale read must not resurrect retired rows.
            local.value = local.value.max(value);
            local.fetched = Some(Instant::now());
        }
        local.value
    }

    async fn invalidate(&self, key: &str) {
        // The generation is asked for afresh: a tombstone under a generation
        // another replica has already retired would guard nothing.
        let entry = self.entry_key(self.generation(true).await, key);
        if let Err(e) = self.cache.put(&entry, TOMBSTONE, Some(self.config.tombstone_ttl)).await {
            tracing::warn!(error = %e.message(), table = self.table, key, "model cache: a changed row was not invalidated; it expires with its TTL");
            report(self.table, Some(key), "invalidate", e).await;
        }
        self.stats.invalidated.fetch_add(1, Ordering::Relaxed);
    }

    async fn advance_generation(&self) {
        match self.cache.increment(&self.generation_key(), 1).await {
            Ok(value) => {
                if let Ok(mut local) = self.generation.lock() {
                    local.value = local.value.max(value);
                    local.fetched = Some(Instant::now());
                }
            }
            Err(e) => {
                tracing::warn!(error = %e.message(), table = self.table, "model cache: a bulk write could not retire cached rows; they expire with their TTL");
                report(self.table, None, "advance_generation", e).await;
            }
        }
        self.stats.generations.fetch_add(1, Ordering::Relaxed);
    }

    async fn apply(&self, change: RowChange) {
        if change.table != self.table || change.kind == ChangeKind::Insert {
            // New rows: there is no cached copy of them to be stale.
            return;
        }
        match &change.keys {
            ChangedKeys::Keys(keys) => {
                for key in keys {
                    self.invalidate(key).await;
                }
            }
            ChangedKeys::Unknown => self.advance_generation().await,
        }
    }
}

/// Report a cache failure the model cache worked around.
async fn report(table: &str, key: Option<&str>, operation: &str, exception: impl Into<Exception>) {
    rainier_events::report::report(
        CacheFailed::new(exception, "model_cache", key.map(str::to_string), operation)
            .with_extra("table", table),
    )
    .await;
}

#[cfg(all(test, feature = "sea-orm-executor"))]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;
    use rainier_database::criteria::Assignment;
    use rainier_database::{statement, Criteria, EntityRepository, Repository};
    use rainier_drivers::sql::SeaOrmExecutor;
    use rainier_orm::{Dialect, Executor, PoolConfig};

    #[derive(Debug, Clone, PartialEq, rainier_orm::Entity, Serialize, serde::Deserialize)]
    #[orm(table = "members")]
    struct Member {
        #[orm(pk)]
        id: i64,
        username: String,
        followers: i64,
    }

    impl rainier_database::Model for Member {}

    async fn world() -> (Database, ModelCache<Member>) {
        let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
            .await
            .expect("connect");
        for sql in rainier_orm::schema::schema_ddl::<Member>(Dialect::Sqlite) {
            exec.execute(&sql, Vec::new()).await.expect("create table");
        }
        let db = Database::new(exec);
        let members = EntityRepository::<Member>::new(db.clone());
        for (id, name) in [(1, "ana"), (2, "bo")] {
            members.create(Member { id, username: name.into(), followers: 0 }).await.expect("seed");
        }
        let cache = ModelCache::with_config(
            Arc::new(MemoryCache::new()),
            &db,
            ModelCacheConfig { hot_threshold: 2, ..ModelCacheConfig::default() },
        );
        (db, cache)
    }

    async fn read(db: &Database, cache: &ModelCache<Member>, id: i64) -> Option<Member> {
        let repo = EntityRepository::<Member>::new(db.clone());
        cache
            .get(&id.to_string(), || async move { repo.find(id.into()).await })
            .await
            .expect("read")
    }

    async fn read_by_name(db: &Database, cache: &ModelCache<Member>, name: &str) -> Option<Member> {
        let repo = EntityRepository::<Member>::new(db.clone());
        let wanted = name.to_string();
        cache
            .get_by(
                "username",
                name,
                |m: &Member| m.id.to_string(),
                |m: &Member| m.username == wanted,
                || async move {
                    repo.first_matching(Criteria::new().where_eq("username", name.to_string()))
                        .await
                },
            )
            .await
            .expect("read")
    }

    #[tokio::test]
    async fn a_cold_row_never_touches_the_cache_and_a_hot_one_is_served_from_it() {
        let (db, cache) = world().await;
        read(&db, &cache, 1).await;
        assert_eq!(cache.stats(), ModelCacheStats { cold: 1, ..Default::default() });

        read(&db, &cache, 1).await; // hot now: a miss, populated
        read(&db, &cache, 1).await; // a hit
        let stats = cache.stats();
        assert_eq!((stats.misses, stats.hits), (1, 1));
    }

    #[tokio::test]
    async fn every_kind_of_write_drops_the_cached_row_before_it_returns() {
        let (db, cache) = world().await;
        for _ in 0..3 {
            read(&db, &cache, 1).await;
        }
        let repo = EntityRepository::<Member>::new(db.clone());

        // A repository update.
        repo.update(&Member { id: 1, username: "ana".into(), followers: 5 }).await.expect("update");
        assert_eq!(read(&db, &cache, 1).await.expect("row").followers, 5);

        // A counter increment built as a statement.
        db.execute(statement::update_matching_with::<Member>(
            db.dialect(),
            &Criteria::new().where_eq("id", 1_i64),
            vec![("followers".to_string(), Assignment::Increment(1))],
        ))
        .await
        .expect("increment");
        assert_eq!(read(&db, &cache, 1).await.expect("row").followers, 6);

        // A bulk update that names no key retires the table.
        repo.update_matching(
            Criteria::new().where_eq("username", "ana"),
            vec![("followers".into(), 9_i64.into())],
        )
        .await
        .expect("bulk");
        assert_eq!(read(&db, &cache, 1).await.expect("row").followers, 9);
        assert_eq!(cache.stats().generations, 1);

        // A delete.
        repo.delete(1_i64.into()).await.expect("delete");
        assert_eq!(read(&db, &cache, 1).await, None);
    }

    #[tokio::test]
    async fn a_read_that_raced_a_write_cannot_put_the_old_row_back() {
        let (db, cache) = world().await;
        for _ in 0..2 {
            read(&db, &cache, 2).await;
        }
        let stale = Member { id: 2, username: "bo".into(), followers: 0 };
        // The write lands (and tombstones) while a reader holding the old row
        // has not yet populated.
        EntityRepository::<Member>::new(db.clone())
            .update(&Member { id: 2, username: "bo".into(), followers: 3 })
            .await
            .expect("update");
        let raced = cache.get("2", || async { Ok(Some(stale.clone())) }).await.expect("raced");
        assert_eq!(raced.map(|m| m.followers), Some(0), "the racing reader keeps its own copy");
        // But nobody else is given it.
        assert_eq!(read(&db, &cache, 2).await.expect("row").followers, 3);
    }

    #[tokio::test]
    async fn a_lookup_by_name_follows_a_rename_rather_than_serving_the_old_row() {
        let (db, cache) = world().await;
        for _ in 0..3 {
            assert_eq!(read_by_name(&db, &cache, "ana").await.expect("ana").id, 1);
        }
        assert!(cache.stats().hits >= 1, "{:?}", cache.stats());

        EntityRepository::<Member>::new(db.clone())
            .update(&Member { id: 1, username: "anna".into(), followers: 0 })
            .await
            .expect("rename");
        assert_eq!(read_by_name(&db, &cache, "ana").await, None);
        assert_eq!(read_by_name(&db, &cache, "anna").await.expect("anna").id, 1);
    }

    #[tokio::test]
    async fn a_row_that_does_not_decode_is_a_miss_and_is_reported() {
        use rainier_events::report::Failure;

        let capture = rainier_events::report::capture();
        let (db, cache) = world().await;
        read(&db, &cache, 1).await; // cold
        let entry = cache.inner.entry_key(cache.inner.generation(false).await, "1");
        cache.inner.cache.put(&entry, b"not json", None).await.unwrap();

        assert_eq!(read(&db, &cache, 1).await.map(|m| m.id), Some(1));

        let failed: Vec<_> = capture
            .reports()
            .into_iter()
            .filter_map(|r| match r.failure {
                Failure::Cache(f) if f.store == "model_cache" && f.operation == "decode" => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].key.as_deref(), Some("1"));
        assert_eq!(failed[0].meta.extra["table"], "members");
    }

    #[tokio::test]
    async fn another_tables_writes_leave_the_cache_alone() {
        let (db, cache) = world().await;
        for _ in 0..3 {
            read(&db, &cache, 1).await;
        }
        db.notify(RowChange::unknown("posts", ChangeKind::Update)).await;
        let before = cache.stats().hits;
        read(&db, &cache, 1).await;
        assert_eq!(cache.stats().hits, before + 1);
        assert_eq!(cache.stats().generations, 0);
    }
}
