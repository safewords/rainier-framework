//! A cache of computed results, dropped when a table they were read from
//! changes.
//!
//! [`ModelCache`](crate::model_cache::ModelCache) caches **rows**, one by one,
//! in the shared cache. [`QueryCache`] caches what a request computes **from**
//! rows — the reference list a form needs, an aggregate a dashboard shows, the
//! set of ids a filter excludes — and keeps it **in this process**, as the
//! value itself. A hit is a hash lookup and an `Arc` clone: no network, no
//! decoding.
//!
//! # Invalidated by the write, on every replica
//!
//! Each entry is remembered with the tables it was read from. Every table it
//! names has a **generation**, and the entry is stamped with each table's
//! generation as it was *before* the load began. A read checks the stamp
//! against the current generations and treats any difference as a miss.
//!
//! A write to a table advances its generation through
//! [`Database::on_change`](rainier_database::Database::on_change), which hears
//! every builder and repository write and runs before the write returns:
//!
//! - **On the replica that wrote**, the local generation moves before the
//!   write's future completes, so the writer's own next read misses.
//! - **On every other replica**, the shared generation — a counter in the
//!   application's cache, advanced by the writer — is read back at most
//!   [`QueryCacheConfig::generation_refresh`] later. A background task keeps
//!   it current; a read that finds it older than twice that refreshes it
//!   before trusting anything stamped with it. A replica therefore serves an
//!   entry for at most about the refresh interval after another replica's
//!   write, and never longer.
//!
//! A load that races a write cannot outlive it: its stamp is taken before it
//! reads, so whatever the write changed, the stamp is already behind.
//!
//! # Declare the tables at boot
//!
//! A process announces a write to a table only once it knows something reads
//! that table. One that has not cached from it yet — a replica that just
//! started, a queue worker that never serves the read at all — would write
//! without telling anyone. So name every table a cached result depends on
//! when the cache is built, with [`QueryCache::watching`], in **every**
//! process that writes: web, worker and scheduler alike.
//!
//! ```ignore
//! let cache = QueryCache::new(&db, Some(shared)).watching(&["genders", "pronouns"]);
//! ```
//!
//! The TTL each call passes is the backstop for the writes the cache cannot
//! hear — SQL passed as a string, a write from another application — and the
//! only freshness rule for a result that depends on the clock rather than on a
//! write. Call [`QueryCache::invalidate`] after a raw-SQL write to a table a
//! cached result reads.
//!
//! # What belongs here
//!
//! Results that are **the same for every viewer** and **bounded in size**
//! however large the tables grow: a lookup table, the few rows currently in
//! some state, an aggregate. Never a whole growing table, and never something
//! filtered by who is asking — keep those per request, or key them by the
//! viewer and accept the memory. The cache holds at most
//! [`QueryCacheConfig::capacity`] entries and drops the oldest past it.
//!
//! A table whose writes are constant (a view counter) makes a poor
//! dependency: every write retires everything read from it, and each one
//! costs a round trip to advance the shared generation. Depend on the tables
//! the result actually needs.
//!
//! # Failure
//!
//! The shared store being unreachable never fails a read. A generation that
//! cannot be advanced is reported as a [`CacheFailed`] with store
//! `query_cache`; the writer's own replica is still correct, and the others
//! fall back to the TTL until the store answers again.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use rainier_cache::Cache;
use rainier_database::{Database, RowChange};
use rainier_events::report::{CacheFailed, FailureEvent as _};
use rainier_support::Result;

/// How a [`QueryCache`] is sized and how quickly replicas converge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryCacheConfig {
    /// The most entries one process keeps. Past it, the oldest go first.
    pub capacity: usize,
    /// How often each process reads the shared generations back — the
    /// longest another replica's write can go unseen here.
    pub generation_refresh: Duration,
}

impl Default for QueryCacheConfig {
    fn default() -> Self {
        Self { capacity: 10_000, generation_refresh: Duration::from_secs(1) }
    }
}

/// Counts since start, for a dashboard or a test.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryCacheStats {
    /// Reads answered from the cache.
    pub hits: u64,
    /// Reads that loaded: absent, expired, or stamped with an old generation.
    pub misses: u64,
    /// Generations advanced by a write.
    pub invalidations: u64,
    /// Entries dropped to stay within capacity.
    pub evictions: u64,
}

/// A process-local cache of computed results, invalidated by table writes on
/// any replica. See the module docs. Cheap to clone.
#[derive(Clone)]
pub struct QueryCache {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Option<Arc<dyn Cache>>,
    config: QueryCacheConfig,
    tables: RwLock<HashMap<String, Arc<TableGeneration>>>,
    entries: Mutex<Entries>,
    loading: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    refresher_running: AtomicBool,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    hits: AtomicU64,
    misses: AtomicU64,
    invalidations: AtomicU64,
    evictions: AtomicU64,
}

/// One table's generation: this process's own writes, and the shared counter
/// every replica's writes advance.
struct TableGeneration {
    table: String,
    local: AtomicU64,
    shared: AtomicU64,
    refreshed: Mutex<Option<Instant>>,
}

impl TableGeneration {
    fn current(&self) -> (u64, u64) {
        (self.local.load(Ordering::Acquire), self.shared.load(Ordering::Acquire))
    }
}

type Stamp = Vec<(Arc<TableGeneration>, (u64, u64))>;

struct Entry {
    value: Arc<dyn Any + Send + Sync>,
    stamp: Stamp,
    expires: Instant,
    sequence: u64,
}

#[derive(Default)]
struct Entries {
    map: HashMap<String, Entry>,
    /// Keys in the order they were stored, with the sequence they were stored
    /// under — so a key stored again is not evicted for its older self.
    order: VecDeque<(String, u64)>,
    next: u64,
}

const GENERATION_PREFIX: &str = "query-cache:generation:";

impl QueryCache {
    /// A cache invalidated by `db`'s writes, sharing generations through
    /// `shared` so other replicas' writes are seen too.
    ///
    /// Without a shared store only this process's writes are seen, which is
    /// right for a single process and a test.
    pub fn new(db: &Database, shared: Option<Arc<dyn Cache>>) -> Self {
        Self::with_config(db, shared, QueryCacheConfig::default())
    }

    /// As [`new`](Self::new), with `config`.
    pub fn with_config(
        db: &Database,
        shared: Option<Arc<dyn Cache>>,
        config: QueryCacheConfig,
    ) -> Self {
        // A per-process store cannot carry a generation to another replica:
        // treat it as no shared store, rather than as one that works.
        let shared = shared.filter(|store| store.is_shared());
        let inner = Arc::new(Inner {
            shared,
            config,
            tables: RwLock::new(HashMap::new()),
            entries: Mutex::new(Entries::default()),
            loading: Mutex::new(HashMap::new()),
            refresher_running: AtomicBool::new(false),
            stats: Stats::default(),
        });
        let subscriber = Arc::downgrade(&inner);
        db.on_change(move |change: RowChange| {
            let inner = subscriber.upgrade();
            async move {
                if let Some(inner) = inner {
                    inner.written(&change.table).await;
                }
            }
        });
        Inner::start_refresher(&inner);
        Self { inner }
    }

    /// Announce writes to `tables` from now on, whether or not this process
    /// has cached anything read from them yet. See the module docs: every
    /// table a cached result depends on belongs here, in every process.
    pub fn watching(self, tables: &[&str]) -> Self {
        for table in tables {
            self.inner.table(table);
        }
        self
    }

    /// The tables this cache announces writes to.
    pub fn watched(&self) -> Vec<String> {
        let tables = self.inner.tables.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut names: Vec<String> = tables.keys().cloned().collect();
        names.sort();
        names
    }

    /// The result cached under `key`, or `load`'s — cached for up to `ttl`, and
    /// until a write to any of `tables`.
    ///
    /// Concurrent misses of one key load once: the rest wait for that load and
    /// read what it stored.
    ///
    /// # Errors
    ///
    /// Only `load`'s. A failed load caches nothing.
    pub async fn remember<T, F, Fut>(
        &self,
        key: &str,
        tables: &[&str],
        ttl: Duration,
        load: F,
    ) -> Result<Arc<T>>
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let inner = &self.inner;
        let generations: Vec<Arc<TableGeneration>> =
            tables.iter().map(|table| inner.table(table)).collect();
        inner.refresh_if_behind(&generations).await;

        if let Some(value) = inner.lookup::<T>(key) {
            inner.stats.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(value);
        }

        // One load per key at a time. Whoever waited re-checks first: the
        // load it waited on has usually just stored the answer.
        let gate = inner.gate(key);
        let _held = gate.lock().await;
        if let Some(value) = inner.lookup::<T>(key) {
            inner.stats.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(value);
        }

        inner.stats.misses.fetch_add(1, Ordering::Relaxed);
        // Stamped before the load reads anything: a write landing during the
        // load leaves this stamp behind, and the entry is a miss from then on.
        let stamp: Stamp = generations.iter().map(|g| (Arc::clone(g), g.current())).collect();
        let value = Arc::new(load().await?);
        inner.store(key, Arc::clone(&value) as Arc<dyn Any + Send + Sync>, stamp, ttl);
        Ok(value)
    }

    /// Drop the entry under `key` on this replica.
    pub fn forget(&self, key: &str) {
        self.inner.lock_entries().map.remove(key);
    }

    /// Advance `table`'s generation, retiring everything read from it on every
    /// replica — for a write this cache could not hear, such as raw SQL.
    pub async fn invalidate(&self, table: &str) {
        self.inner.written(table).await;
    }

    /// Counts since start.
    pub fn stats(&self) -> QueryCacheStats {
        let stats = &self.inner.stats;
        QueryCacheStats {
            hits: stats.hits.load(Ordering::Relaxed),
            misses: stats.misses.load(Ordering::Relaxed),
            invalidations: stats.invalidations.load(Ordering::Relaxed),
            evictions: stats.evictions.load(Ordering::Relaxed),
        }
    }

    /// How many entries this process holds.
    pub fn len(&self) -> usize {
        self.inner.lock_entries().map.len()
    }

    /// Whether this process holds none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read every shared generation back now, rather than on the next tick.
    pub async fn refresh(&self) {
        self.inner.refresh_all().await;
    }
}

impl Inner {
    fn lock_entries(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn table(&self, table: &str) -> Arc<TableGeneration> {
        if let Some(found) =
            self.tables.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(table)
        {
            return Arc::clone(found);
        }
        let mut tables = self.tables.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(tables.entry(table.to_string()).or_insert_with(|| {
            Arc::new(TableGeneration {
                table: table.to_string(),
                local: AtomicU64::new(0),
                shared: AtomicU64::new(0),
                refreshed: Mutex::new(None),
            })
        }))
    }

    fn tracked(&self, table: &str) -> Option<Arc<TableGeneration>> {
        self.tables.read().unwrap_or_else(|poisoned| poisoned.into_inner()).get(table).cloned()
    }

    fn lookup<T: Send + Sync + 'static>(&self, key: &str) -> Option<Arc<T>> {
        let mut entries = self.lock_entries();
        let entry = entries.map.get(key)?;
        let current = entry.expires > Instant::now()
            && entry.stamp.iter().all(|(table, seen)| table.current() == *seen);
        if !current {
            entries.map.remove(key);
            return None;
        }
        // A key reused for a different type is a miss, not a panic.
        Arc::clone(&entry.value).downcast::<T>().ok()
    }

    fn store(&self, key: &str, value: Arc<dyn Any + Send + Sync>, stamp: Stamp, ttl: Duration) {
        let mut entries = self.lock_entries();
        let sequence = entries.next;
        entries.next += 1;
        entries.map.insert(
            key.to_string(),
            Entry { value, stamp, expires: Instant::now() + ttl, sequence },
        );
        entries.order.push_back((key.to_string(), sequence));

        while entries.map.len() > self.config.capacity {
            let Some((oldest, stored_as)) = entries.order.pop_front() else { break };
            if entries.map.get(&oldest).is_some_and(|entry| entry.sequence == stored_as) {
                entries.map.remove(&oldest);
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Keys stored more than once leave stale positions behind; keep the
        // queue from outgrowing the map by more than the map itself.
        if entries.order.len() > self.config.capacity.saturating_mul(2).max(64) {
            let Entries { map, order, .. } = &mut *entries;
            order.retain(|(key, sequence)| map.get(key).is_some_and(|e| e.sequence == *sequence));
        }
    }

    fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut loading = self.loading.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(gate) = loading.get(key).and_then(Weak::upgrade) {
            return gate;
        }
        // Gates nobody holds any more go as new ones are made.
        loading.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        loading.insert(key.to_string(), Arc::downgrade(&gate));
        gate
    }

    /// A write to `table` succeeded: move its generation here, then in the
    /// shared store for everyone else.
    async fn written(&self, table: &str) {
        let Some(generation) = self.tracked(table) else {
            // Nothing cached reads this table, so nothing can be stale.
            return;
        };
        generation.local.fetch_add(1, Ordering::AcqRel);
        self.stats.invalidations.fetch_add(1, Ordering::Relaxed);

        let Some(shared) = &self.shared else { return };
        match shared.increment(&format!("{GENERATION_PREFIX}{table}"), 1).await {
            Ok(value) => {
                generation.shared.fetch_max(value.max(0) as u64, Ordering::AcqRel);
            }
            Err(e) => report_failure(table, "advance", e).await,
        }
    }

    /// Read the shared generations of `generations` back if the background
    /// refresh has fallen behind — or never ran, outside a runtime that could
    /// keep it.
    async fn refresh_if_behind(&self, generations: &[Arc<TableGeneration>]) {
        if self.shared.is_none() {
            return;
        }
        let limit = self.config.generation_refresh * 2;
        for generation in generations {
            let behind = generation
                .refreshed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none_or(|at| at.elapsed() > limit);
            if behind {
                self.refresh_one(generation).await;
            }
        }
    }

    async fn refresh_all(&self) {
        let generations: Vec<Arc<TableGeneration>> = self
            .tables
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        for generation in &generations {
            self.refresh_one(generation).await;
        }
    }

    async fn refresh_one(&self, generation: &TableGeneration) {
        let Some(shared) = &self.shared else { return };
        let key = format!("{GENERATION_PREFIX}{}", generation.table);
        match shared.get(&key).await {
            Ok(value) => {
                let seen = value
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .and_then(|text| text.trim().parse::<u64>().ok())
                    .unwrap_or(0);
                generation.shared.fetch_max(seen, Ordering::AcqRel);
                *generation.refreshed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(Instant::now());
            }
            // Left as it was, and not marked refreshed: the next read tries
            // again. Entries stay valid for this replica's own writes and
            // their TTL.
            Err(e) => report_failure(&generation.table, "refresh", e).await,
        }
    }

    /// Keep the shared generations current in the background, so a read
    /// never waits on one — when there is a runtime to run on and a shared
    /// store to read. Stops when the cache is dropped.
    fn start_refresher(inner: &Arc<Self>) {
        if inner.shared.is_none() || inner.refresher_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            inner.refresher_running.store(false, Ordering::Release);
            return;
        };
        let weak = Arc::downgrade(inner);
        let every = inner.config.generation_refresh.max(Duration::from_millis(10));
        runtime.spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                inner.refresh_all().await;
            }
        });
    }
}

async fn report_failure(table: &str, operation: &str, error: rainier_support::Error) {
    tracing::debug!(error = %error.message(), table, operation, "query cache generation unavailable");
    rainier_events::report::report(
        CacheFailed::new(error, "query_cache", None, operation).with_extra("table", table),
    )
    .await;
}

#[cfg(all(test, feature = "sea-orm-executor"))]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;
    use rainier_database::{EntityRepository, Repository};
    use rainier_drivers::sql::SeaOrmExecutor;
    use rainier_orm::{Dialect, Executor, PoolConfig};

    #[derive(
        Debug, Clone, PartialEq, rainier_orm::Entity, serde::Serialize, serde::Deserialize,
    )]
    #[orm(table = "genders")]
    struct Gender {
        #[orm(pk)]
        id: i64,
        name: String,
    }

    impl rainier_database::Model for Gender {}

    #[derive(
        Debug, Clone, PartialEq, rainier_orm::Entity, serde::Serialize, serde::Deserialize,
    )]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk)]
        id: i64,
        title: String,
    }

    impl rainier_database::Model for Post {}

    async fn db() -> Database {
        let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::in_memory())
            .await
            .expect("connect");
        for sql in rainier_orm::schema::schema_ddl::<Gender>(Dialect::Sqlite)
            .into_iter()
            .chain(rainier_orm::schema::schema_ddl::<Post>(Dialect::Sqlite))
        {
            exec.execute(&sql, Vec::new()).await.expect("create table");
        }
        Database::new(exec)
    }

    async fn insert(db: &Database, id: i64, name: &str) {
        EntityRepository::<Gender>::new(db.clone())
            .create(Gender { id, name: name.into() })
            .await
            .expect("insert");
    }

    async fn names(db: &Database) -> Result<Vec<String>> {
        let mut rows = EntityRepository::<Gender>::new(db.clone()).all().await?;
        rows.sort_by_key(|g| g.id);
        Ok(rows.into_iter().map(|g| g.name).collect())
    }

    /// A memory store that says it is shared, so two caches in one test can
    /// stand for two replicas.
    struct SharedMemory(MemoryCache);

    impl Cache for SharedMemory {
        fn name(&self) -> &str {
            "shared-memory"
        }
        fn get<'a>(
            &'a self,
            key: &'a str,
        ) -> rainier_support::BoxFuture<'a, Result<Option<Vec<u8>>>> {
            self.0.get(key)
        }
        fn put<'a>(
            &'a self,
            key: &'a str,
            value: &'a [u8],
            ttl: Option<Duration>,
        ) -> rainier_support::BoxFuture<'a, Result<()>> {
            self.0.put(key, value, ttl)
        }
        fn forget<'a>(&'a self, key: &'a str) -> rainier_support::BoxFuture<'a, Result<bool>> {
            self.0.forget(key)
        }
        fn flush(&self) -> rainier_support::BoxFuture<'_, Result<()>> {
            self.0.flush()
        }
        fn increment<'a>(
            &'a self,
            key: &'a str,
            by: i64,
        ) -> rainier_support::BoxFuture<'a, Result<i64>> {
            self.0.increment(key, by)
        }
        fn add<'a>(
            &'a self,
            key: &'a str,
            value: &'a [u8],
            ttl: Option<Duration>,
        ) -> rainier_support::BoxFuture<'a, Result<bool>> {
            self.0.add(key, value, ttl)
        }
        fn forget_if<'a>(
            &'a self,
            key: &'a str,
            expected: &'a [u8],
        ) -> rainier_support::BoxFuture<'a, Result<bool>> {
            self.0.forget_if(key, expected)
        }
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn a_second_read_is_served_without_loading() {
        let db = db().await;
        insert(&db, 1, "a").await;
        let cache = QueryCache::new(&db, None);

        let first = cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();
        let second = cache
            .remember::<Vec<String>, _, _>("genders", &["genders"], HOUR, || async {
                panic!("served from the cache")
            })
            .await
            .unwrap();

        assert_eq!(*first, vec!["a"]);
        assert!(Arc::ptr_eq(&first, &second), "the same value, not a copy");
        assert_eq!(cache.stats().hits, 1);
    }

    #[tokio::test]
    async fn a_write_to_a_table_it_read_retires_it_before_the_write_returns() {
        let db = db().await;
        insert(&db, 1, "a").await;
        let cache = QueryCache::new(&db, None);
        cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();

        insert(&db, 2, "b").await;

        let after = cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();
        assert_eq!(*after, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_write_to_another_table_leaves_it_alone() {
        let db = db().await;
        insert(&db, 1, "a").await;
        let cache = QueryCache::new(&db, None);
        cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();

        EntityRepository::<Post>::new(db.clone())
            .create(Post { id: 1, title: "x".into() })
            .await
            .unwrap();

        cache
            .remember::<Vec<String>, _, _>("genders", &["genders"], HOUR, || async {
                panic!("still cached")
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_ttl_expires_an_entry_no_write_touched() {
        let db = db().await;
        let cache = QueryCache::new(&db, None);
        cache.remember("k", &[], Duration::from_millis(20), || async { Ok(1u32) }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;

        let reloaded = cache.remember("k", &[], HOUR, || async { Ok(2u32) }).await.unwrap();
        assert_eq!(*reloaded, 2);
    }

    #[tokio::test]
    async fn a_load_that_races_a_write_is_not_kept() {
        let db = db().await;
        insert(&db, 1, "a").await;
        let cache = QueryCache::new(&db, None);

        // The load reads the old rows, and the write lands before it stores
        // them: the stamp it took is already behind.
        let stale = cache
            .remember("genders", &["genders"], HOUR, || async {
                let read = names(&db).await;
                insert(&db, 2, "b").await;
                read
            })
            .await
            .unwrap();
        assert_eq!(*stale, vec!["a"], "the racing read itself returns what it read");

        let next = cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();
        assert_eq!(*next, vec!["a", "b"], "but it is never served again");
    }

    #[tokio::test]
    async fn another_replicas_write_is_seen_after_a_refresh() {
        let shared: Arc<dyn Cache> = Arc::new(SharedMemory(MemoryCache::new()));
        // Two replicas of one database: each its own handle and its own
        // process-local cache, sharing only the generation store. Neither
        // handle hears the other's writes.
        let here_db = db().await;
        let elsewhere_db = db().await;
        let here = QueryCache::new(&here_db, Some(Arc::clone(&shared)));
        let elsewhere = QueryCache::new(&elsewhere_db, Some(Arc::clone(&shared)));
        // The other replica caches from `genders` too, so its writes count.
        elsewhere.remember("genders", &["genders"], HOUR, || async { Ok(0u8) }).await.unwrap();

        here.remember("genders", &["genders"], HOUR, || async { Ok(1u8) }).await.unwrap();
        insert(&elsewhere_db, 1, "b").await;

        here.refresh().await;
        let reloaded =
            here.remember("genders", &["genders"], HOUR, || async { Ok(2u8) }).await.unwrap();
        assert_eq!(*reloaded, 2, "a write on another replica retired the entry here");
    }

    #[tokio::test]
    async fn a_process_that_never_read_a_watched_table_still_announces_writes_to_it() {
        // A worker that writes and never serves the cached read.
        let shared: Arc<dyn Cache> = Arc::new(SharedMemory(MemoryCache::new()));
        let web_db = db().await;
        let worker_db = db().await;
        let web = QueryCache::new(&web_db, Some(Arc::clone(&shared))).watching(&["genders"]);
        let _worker = QueryCache::new(&worker_db, Some(Arc::clone(&shared))).watching(&["genders"]);

        web.remember("genders", &["genders"], HOUR, || async { Ok(1u8) }).await.unwrap();
        insert(&worker_db, 1, "b").await;
        web.refresh().await;

        let reloaded =
            web.remember("genders", &["genders"], HOUR, || async { Ok(2u8) }).await.unwrap();
        assert_eq!(*reloaded, 2);
        assert_eq!(web.watched(), vec!["genders"]);
    }

    #[tokio::test]
    async fn a_replica_refreshes_on_its_own_within_the_interval() {
        let shared: Arc<dyn Cache> = Arc::new(SharedMemory(MemoryCache::new()));
        let config = QueryCacheConfig {
            generation_refresh: Duration::from_millis(20),
            ..Default::default()
        };
        let here_db = db().await;
        let elsewhere_db = db().await;
        let here = QueryCache::with_config(&here_db, Some(Arc::clone(&shared)), config);
        let elsewhere = QueryCache::with_config(&elsewhere_db, Some(Arc::clone(&shared)), config);
        elsewhere.remember("genders", &["genders"], HOUR, || async { Ok(0u8) }).await.unwrap();
        here.remember("genders", &["genders"], HOUR, || async { Ok(1u8) }).await.unwrap();

        insert(&elsewhere_db, 1, "b").await;
        // No explicit refresh: the background task or the read's own check.
        tokio::time::sleep(Duration::from_millis(80)).await;

        let reloaded =
            here.remember("genders", &["genders"], HOUR, || async { Ok(2u8) }).await.unwrap();
        assert_eq!(*reloaded, 2);
    }

    #[tokio::test]
    async fn concurrent_misses_load_once() {
        let db = db().await;
        let cache = QueryCache::new(&db, None);
        let loads = Arc::new(AtomicU64::new(0));

        let reads: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let loads = Arc::clone(&loads);
                tokio::spawn(async move {
                    cache
                        .remember("slow", &[], HOUR, || async move {
                            loads.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            Ok(7u32)
                        })
                        .await
                        .unwrap()
                })
            })
            .collect();
        for read in reads {
            assert_eq!(*read.await.unwrap(), 7);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn it_keeps_at_most_its_capacity() {
        let db = db().await;
        let cache = QueryCache::with_config(
            &db,
            None,
            QueryCacheConfig { capacity: 3, ..QueryCacheConfig::default() },
        );
        for i in 0..10u32 {
            cache.remember(&format!("k{i}"), &[], HOUR, || async move { Ok(i) }).await.unwrap();
        }
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.stats().evictions, 7);
        cache
            .remember::<u32, _, _>("k9", &[], HOUR, || async { panic!("the newest stayed") })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_failed_load_caches_nothing() {
        let db = db().await;
        let cache = QueryCache::new(&db, None);
        let failed = cache
            .remember::<u32, _, _>("k", &[], HOUR, || async {
                Err(rainier_support::Error::internal("database down"))
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(*cache.remember("k", &[], HOUR, || async { Ok(1u32) }).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_raw_sql_write_is_retired_by_invalidate() {
        let db = db().await;
        insert(&db, 1, "a").await;
        let cache = QueryCache::new(&db, None);
        cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();

        db.statement("INSERT INTO genders (id, name) VALUES (2, 'b')").await.unwrap();
        cache.invalidate("genders").await;

        let after = cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();
        assert_eq!(*after, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_rolled_back_transaction_leaves_it_and_a_committed_one_retires_it() {
        let db = db().await;
        insert(&db, 1, "a").await;
        let cache = QueryCache::new(&db, None);
        cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();

        let _: Result<()> = db
            .transaction(|tx| async move {
                insert(&tx, 2, "b").await;
                Err(rainier_support::Error::internal("undo"))
            })
            .await;
        cache
            .remember::<Vec<String>, _, _>("genders", &["genders"], HOUR, || async {
                panic!("a write that was rolled back retired nothing")
            })
            .await
            .unwrap();

        db.transaction(|tx| async move {
            insert(&tx, 2, "b").await;
            Ok(())
        })
        .await
        .unwrap();
        let after = cache.remember("genders", &["genders"], HOUR, || names(&db)).await.unwrap();
        assert_eq!(*after, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_key_read_as_another_type_is_a_miss_not_a_panic() {
        let db = db().await;
        let cache = QueryCache::new(&db, None);
        cache.remember("k", &[], HOUR, || async { Ok(1u32) }).await.unwrap();
        let other = cache.remember("k", &[], HOUR, || async { Ok("text") }).await.unwrap();
        assert_eq!(*other, "text");
    }
}
