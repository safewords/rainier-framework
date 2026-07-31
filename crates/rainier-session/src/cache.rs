//! [`CacheSessionStore`] — sessions in a [`Cache`], and so in Redis,
//! a Redis Cluster, or Memcached.

use std::sync::Arc;
use std::time::Duration;

use chrono::Duration as ChronoDuration;
use rainier_cache::{Cache, CacheExt};
use rainier_support::{BoxFuture, Result};

use crate::session::SessionData;
use crate::store::SessionStore;

/// The key prefix, so sessions do not collide with cached values.
const PREFIX: &str = "session:";

/// Sessions in a cache.
///
/// The usual production choice, because it gets the two things a session needs
/// from infrastructure you already have: **shared between instances**, so a load
/// balancer can send a user anywhere, and **expiring by itself**, so nothing has
/// to sweep old rows.
///
/// Works over any [`Cache`] — including a
/// sharded Redis Cluster (`rainier_cache::RedisCache`), where each session lands
/// on the node that owns its slot. Every operation here touches exactly one key,
/// which is what makes that safe.
///
/// ```
/// use rainier_cache::MemoryCache;
/// use rainier_session::CacheSessionStore;
/// use std::sync::Arc;
///
/// let store = CacheSessionStore::new(Arc::new(MemoryCache::new()));
/// # let _ = store;
/// ```
///
/// ## A cache can evict
///
/// This is the trade against a [database
/// store](crate::DatabaseSessionStore). A cache under memory pressure evicts
/// whatever it likes, and an evicted session logs somebody out mid-checkout.
///
/// For Redis, `maxmemory-policy volatile-lru` at least confines eviction to keys
/// with a TTL; `allkeys-lru` will take sessions ahead of things you would rather
/// lose. If being logged out is genuinely unacceptable, the database store is
/// the one that does not evict.
pub struct CacheSessionStore {
    cache: Arc<dyn Cache>,
    lifetime: Duration,
}

impl CacheSessionStore {
    /// A store over `cache`, with two-hour sessions.
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache, lifetime: Duration::from_secs(2 * 60 * 60) }
    }

    /// Sessions expiring after `lifetime`.
    pub fn with_lifetime(mut self, lifetime: ChronoDuration) -> Self {
        if let Ok(lifetime) = lifetime.to_std() {
            self.lifetime = lifetime;
        }
        self
    }

    /// The cache underneath, for sharing it.
    pub fn cache(&self) -> &Arc<dyn Cache> {
        &self.cache
    }

    /// How long a session lives without being touched.
    pub fn lifetime(&self) -> Duration {
        self.lifetime
    }

    fn key(id: &str) -> String {
        format!("{PREFIX}{id}")
    }
}

impl SessionStore for CacheSessionStore {
    fn name(&self) -> &str {
        // The driver underneath, not "cache" — an operator wants to know
        // *which* cache, and this is what `route:list`-style diagnostics print.
        self.cache.name()
    }

    fn read<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>>> {
        Box::pin(async move {
            // `get_json` treats an unparseable value as a miss, which is what
            // should happen to a session written by an older shape of the
            // application: the user gets a fresh session rather than a 500 they
            // cannot clear without deleting cookies.
            self.cache.get_json(&Self::key(id)).await
        })
    }

    fn write<'a>(&'a self, id: &'a str, data: &'a SessionData) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // The TTL is set on every write, so an active session keeps sliding
            // forward and an abandoned one expires on its own. That is the whole
            // reason a cache suits sessions: no sweeper.
            self.cache.put_json(&Self::key(id), data, Some(self.lifetime)).await
        })
    }

    fn destroy<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.cache.forget(&Self::key(id)).await?;
            Ok(())
        })
    }

    /// Nothing to do: the cache expires its own keys.
    ///
    /// Returns zero rather than pretending, and deliberately does **not**
    /// delegate to [`Cache::flush`] — that would empty the whole cache, which is
    /// not what "collect expired sessions" means.
    fn gc(&self) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }
}

impl std::fmt::Debug for CacheSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheSessionStore")
            .field("cache", &self.cache.name())
            .field("lifetime", &self.lifetime)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;

    fn store() -> (CacheSessionStore, Arc<MemoryCache>) {
        let cache = Arc::new(MemoryCache::new());
        (CacheSessionStore::new(Arc::clone(&cache) as Arc<dyn Cache>), cache)
    }

    fn data(user: u64) -> SessionData {
        let mut values = serde_json::Map::new();
        values.insert("user_id".into(), user.into());
        SessionData { values, flash: Vec::new() }
    }

    #[tokio::test]
    async fn a_written_session_reads_back() {
        let (store, _) = store();
        store.write("abc", &data(42)).await.unwrap();

        assert_eq!(store.read("abc").await.unwrap().unwrap().values["user_id"], 42);
    }

    #[tokio::test]
    async fn an_unknown_session_reads_as_none() {
        let (store, _) = store();
        assert!(store.read("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn destroying_removes_it() {
        let (store, _) = store();
        store.write("abc", &data(42)).await.unwrap();

        store.destroy("abc").await.unwrap();
        assert!(store.read("abc").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn keys_are_prefixed_so_they_do_not_collide_with_cached_values() {
        let (store, cache) = store();
        store.write("abc", &data(42)).await.unwrap();

        assert!(cache.has("session:abc").await.unwrap());
        assert!(!cache.has("abc").await.unwrap(), "the bare key must be free for other uses");
    }

    #[tokio::test]
    async fn a_ttl_is_set_on_every_write() {
        // An active session slides forward; an abandoned one expires without
        // anything sweeping it. An already-elapsed lifetime proves the TTL is
        // applied rather than the value being stored forever.
        let brief = CacheSessionStore::new(Arc::new(MemoryCache::new()))
            .with_lifetime(ChronoDuration::nanoseconds(1));

        brief.write("brief", &data(1)).await.unwrap();
        assert!(brief.read("brief").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unreadable_session_reads_as_a_fresh_one() {
        let (store, cache) = store();
        cache.put("session:abc", b"not json", None).await.unwrap();

        assert!(store.read("abc").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn gc_does_not_flush_the_cache() {
        // "Collect expired sessions" must not mean "empty the cache".
        let (store, cache) = store();
        cache.put("something-else", b"v", None).await.unwrap();

        assert_eq!(store.gc().await.unwrap(), 0);
        assert!(cache.has("something-else").await.unwrap());
    }

    #[tokio::test]
    async fn the_name_reports_the_driver_underneath() {
        let (store, _) = store();
        assert_eq!(store.name(), "memory");
    }

    #[test]
    fn the_lifetime_is_configurable_and_ignores_a_negative_one() {
        let (store, _) = store();
        let store = store.with_lifetime(ChronoDuration::days(14));
        assert_eq!(store.lifetime(), Duration::from_secs(14 * 24 * 60 * 60));

        // A negative duration has no `std` equivalent; keeping the previous
        // value beats panicking or silently meaning "expire immediately".
        let store = store.with_lifetime(ChronoDuration::seconds(-1));
        assert_eq!(store.lifetime(), Duration::from_secs(14 * 24 * 60 * 60));
    }

    #[tokio::test]
    async fn it_is_not_client_side() {
        let (store, _) = store();
        assert!(!store.is_client_side());
        assert!(store.decode("chosen-by-the-client").is_err());
    }
}
