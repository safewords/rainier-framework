//! [`MemcachedCache`] — a cache on Memcached.

use std::time::Duration;

use rainier_drivers::memcached::{check_key, expiry_seconds, Stored};
use rainier_drivers::MemcachedConnector;
use rainier_support::{BoxFuture, Result};

use crate::cache::Cache;

/// A cache backed by Memcached.
///
/// Simpler than Redis and correspondingly more limited: keys are capped at 250
/// bytes, values at 1 MiB by default, counters are unsigned, and there is no way
/// to enumerate or delete by prefix. Reach for it when it is already running;
/// reach for Redis when you want more than get, set and a counter.
///
/// ```no_run
/// use rainier_cache::MemcachedCache;
/// use rainier_drivers::MemcachedConnector;
///
/// let cache = MemcachedCache::new(MemcachedConnector::open("127.0.0.1:11211"));
/// # let _ = cache;
/// ```
pub struct MemcachedCache {
    connector: MemcachedConnector,
}

impl MemcachedCache {
    /// A cache over `connector`.
    pub fn new(connector: MemcachedConnector) -> Self {
        Self { connector }
    }

    /// The connector, for sharing it.
    pub fn connector(&self) -> &MemcachedConnector {
        &self.connector
    }

    /// Store only if the key is absent — Memcached's `add`.
    ///
    /// **Atomic**, unlike [`Cache::add`]'s default,
    /// which is a check then a write. This is the one to build a lock on.
    pub async fn add_atomic(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<bool> {
        check_key(key)?;
        let mut guard = self.connector.acquire().await?;

        match guard.connection().add(key, value, expiry_seconds(ttl)).await {
            Ok(stored) => Ok(stored == Stored::Stored),
            Err(e) => {
                guard.discard();
                Err(e)
            }
        }
    }
}

impl Cache for MemcachedCache {
    fn name(&self) -> &str {
        "memcached"
    }

    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        // Memcached's `add` is atomic by definition — it is the command for
        // "store only if absent", decided by the server.
        Box::pin(async move { self.add_atomic(key, value, ttl).await })
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            check_key(key)?;
            let mut guard = self.connector.acquire().await?;

            match guard.connection().delete_if(key, expected).await {
                Ok(removed) => Ok(removed),
                Err(e) => {
                    guard.discard();
                    Err(e)
                }
            }
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            check_key(key)?;
            let mut guard = self.connector.acquire().await?;

            match guard.connection().get(key).await {
                Ok(value) => Ok(value),
                Err(e) => {
                    // A connection that failed mid-command may have unread
                    // bytes on it; the next borrower would read them as its own
                    // reply and return one key's value for another.
                    guard.discard();
                    Err(e)
                }
            }
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            check_key(key)?;
            let mut guard = self.connector.acquire().await?;

            match guard.connection().set(key, value, expiry_seconds(ttl)).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    guard.discard();
                    Err(e)
                }
            }
        })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            check_key(key)?;
            let mut guard = self.connector.acquire().await?;

            match guard.connection().delete(key).await {
                Ok(removed) => Ok(removed),
                Err(e) => {
                    guard.discard();
                    Err(e)
                }
            }
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut guard = self.connector.acquire().await?;

            match guard.connection().flush_all().await {
                Ok(()) => Ok(()),
                Err(e) => {
                    guard.discard();
                    Err(e)
                }
            }
        })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            check_key(key)?;

            // Memcached's `incr`/`decr` refuse to create a key, and its
            // counters are unsigned and saturate at zero. So: try the delta
            // first, and only seed the key if it was absent. Doing it that way
            // round means an existing counter is incremented atomically, and
            // only the create path has a race — where the loser's `add` fails
            // and it simply retries the delta.
            let mut guard = self.connector.acquire().await?;
            let existing = match guard.connection().increment(key, by.unsigned_abs(), by >= 0).await
            {
                Ok(value) => value,
                Err(e) => {
                    guard.discard();
                    return Err(e);
                }
            };

            if let Some(value) = existing {
                return Ok(value as i64);
            }

            // Absent: create it at the delta itself, clamped at zero because a
            // Memcached counter cannot be negative.
            let seed = by.max(0);
            match guard.connection().add(key, seed.to_string().as_bytes(), 0).await {
                Ok(Stored::Stored) => Ok(seed),
                Ok(Stored::NotStored) => {
                    // Somebody else created it between our two commands. Their
                    // value is authoritative; apply our delta to it.
                    match guard.connection().increment(key, by.unsigned_abs(), by >= 0).await {
                        Ok(Some(value)) => Ok(value as i64),
                        Ok(None) => Ok(seed),
                        Err(e) => {
                            guard.discard();
                            Err(e)
                        }
                    }
                }
                Err(e) => {
                    guard.discard();
                    Err(e)
                }
            }
        })
    }
}

impl std::fmt::Debug for MemcachedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemcachedCache").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheExt;

    fn cache() -> MemcachedCache {
        MemcachedCache::new(MemcachedConnector::open("127.0.0.1:11211"))
    }

    #[test]
    fn the_driver_is_named() {
        assert_eq!(cache().name(), "memcached");
    }

    #[tokio::test]
    async fn an_invalid_key_is_refused_before_connecting() {
        // The connector points at a live port in CI or not; either way this
        // must fail on the key rather than on the network.
        let cache = MemcachedCache::new(MemcachedConnector::open("127.0.0.1:1"));
        let err = cache.get("has space").await.unwrap_err();

        assert!(err.message().contains("spaces"), "{}", err.message());
    }

    #[tokio::test]
    async fn an_over_long_key_is_refused_before_connecting() {
        let cache = MemcachedCache::new(MemcachedConnector::open("127.0.0.1:1"));
        assert!(cache.put(&"k".repeat(251), b"v", None).await.is_err());
    }

    #[tokio::test]
    #[ignore = "needs a live Memcached"]
    async fn a_value_round_trips() {
        let cache = cache();
        cache.put("k", b"v", None).await.unwrap();

        assert_eq!(cache.get("k").await.unwrap(), Some(b"v".to_vec()));
        assert!(cache.forget("k").await.unwrap());
        assert_eq!(cache.get("k").await.unwrap(), None);
    }

    #[tokio::test]
    #[ignore = "needs a live Memcached"]
    async fn forgetting_an_absent_key_is_false_not_an_error() {
        assert!(!cache().forget("definitely-absent").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "needs a live Memcached"]
    async fn increment_creates_then_accumulates() {
        let cache = cache();
        cache.forget("hits").await.unwrap();

        assert_eq!(cache.increment("hits", 1).await.unwrap(), 1);
        assert_eq!(cache.increment("hits", 4).await.unwrap(), 5);
        assert_eq!(cache.decrement("hits", 2).await.unwrap(), 3);
    }

    #[tokio::test]
    #[ignore = "needs a live Memcached"]
    async fn a_counter_does_not_go_negative() {
        // Memcached's own behaviour, surfaced rather than papered over.
        let cache = cache();
        cache.forget("balance").await.unwrap();
        cache.increment("balance", 1).await.unwrap();

        assert_eq!(cache.decrement("balance", 10).await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore = "needs a live Memcached"]
    async fn add_atomic_lets_exactly_one_caller_win() {
        let cache = cache();
        cache.forget("lock").await.unwrap();

        assert!(cache.add_atomic("lock", b"mine", Some(Duration::from_secs(5))).await.unwrap());
        assert!(!cache.add_atomic("lock", b"theirs", Some(Duration::from_secs(5))).await.unwrap());
    }
}
