//! [`PrefixedCache`] — namespacing a shared cache.

use std::sync::Arc;
use std::time::Duration;

use rainier_support::{BoxFuture, Result};

use crate::cache::Cache;

/// Prefixes every key, so several applications can share one cache server.
///
/// Worth reaching for whenever the cache is not exclusively yours. Two
/// applications caching `user:1` on one Redis database will read each other's
/// values — and the symptom is a user seeing another application's data, not a
/// crash.
///
/// It also makes [`flush`](Cache::flush) safe to mean "mine": the inner cache's
/// flush would empty the whole database, including the other application's
/// keys and anybody's sessions.
///
/// ```
/// # use rainier_cache::{Cache, CacheExt, MemoryCache, PrefixedCache};
/// # use std::sync::Arc;
/// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
/// let shared: Arc<dyn Cache> = Arc::new(MemoryCache::new());
///
/// let billing = PrefixedCache::new(Arc::clone(&shared), "billing");
/// let catalogue = PrefixedCache::new(Arc::clone(&shared), "catalogue");
///
/// billing.put_string("total", "10", None).await?;
/// catalogue.put_string("total", "999", None).await?;
///
/// assert_eq!(billing.get_string("total").await?.as_deref(), Some("10"));
/// # Ok(()) }
/// ```
pub struct PrefixedCache {
    inner: Arc<dyn Cache>,
    prefix: String,
    /// The full prefix including the separator, precomputed.
    with_separator: String,
}

impl PrefixedCache {
    /// Wrap `inner`, prefixing every key with `prefix:`.
    pub fn new(inner: Arc<dyn Cache>, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        // Trailing separators are trimmed so `new(c, "app")` and
        // `new(c, "app:")` address the same keys rather than two namespaces
        // that differ by a character nobody notices.
        let trimmed = prefix.trim_end_matches(':').to_string();
        Self { with_separator: format!("{trimmed}:"), prefix: trimmed, inner }
    }

    /// The prefix, without its separator.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The cache underneath.
    pub fn inner(&self) -> &Arc<dyn Cache> {
        &self.inner
    }

    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.with_separator)
    }
}

impl Cache for PrefixedCache {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn is_shared(&self) -> bool {
        self.inner.is_shared()
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let key = self.key(key);
            self.inner.get(&key).await
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let key = self.key(key);
            self.inner.put(&key, value, ttl).await
        })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let key = self.key(key);
            self.inner.forget(&key).await
        })
    }

    /// **Not implemented as a flush.**
    ///
    /// There is no portable way to delete by prefix: Redis needs a `SCAN` loop
    /// and Memcached cannot do it at all. Delegating to the inner flush would
    /// empty the whole server, which is precisely what wrapping it was meant to
    /// prevent — so this refuses instead of doing something surprising.
    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            Err(rainier_support::Error::internal(format!(
                "a prefixed cache cannot flush just `{}:` — no cache backend can delete by \
                 prefix portably. Flush the underlying cache if you really mean everything, \
                 or forget the keys you know about.",
                self.prefix
            )))
        })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let key = self.key(key);
            self.inner.increment(&key, by).await
        })
    }

    // Both forward, so the atomicity is whatever the wrapped cache offers —
    // prefixing a key cannot make an operation less atomic, and a wrapper that
    // reimplemented either of these would be a wrapper that quietly downgraded
    // a lock.
    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let key = self.key(key);
            self.inner.add(&key, value, ttl).await
        })
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let key = self.key(key);
            self.inner.forget_if(&key, expected).await
        })
    }

    fn has<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let key = self.key(key);
            self.inner.has(&key).await
        })
    }
}

impl std::fmt::Debug for PrefixedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixedCache")
            .field("prefix", &self.prefix)
            .field("inner", &self.inner.name())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheExt;
    use crate::memory::MemoryCache;

    fn shared() -> Arc<MemoryCache> {
        Arc::new(MemoryCache::new())
    }

    #[tokio::test]
    async fn keys_are_namespaced() {
        let inner = shared();
        let cache = PrefixedCache::new(Arc::clone(&inner) as Arc<dyn Cache>, "billing");

        cache.put("total", b"10", None).await.unwrap();

        assert_eq!(cache.get("total").await.unwrap(), Some(b"10".to_vec()));
        assert_eq!(inner.get("billing:total").await.unwrap(), Some(b"10".to_vec()));
        assert_eq!(inner.get("total").await.unwrap(), None, "the bare key is untouched");
    }

    #[tokio::test]
    async fn two_namespaces_do_not_collide() {
        let inner = shared() as Arc<dyn Cache>;
        let billing = PrefixedCache::new(Arc::clone(&inner), "billing");
        let catalogue = PrefixedCache::new(inner, "catalogue");

        billing.put("total", b"10", None).await.unwrap();
        catalogue.put("total", b"999", None).await.unwrap();

        assert_eq!(billing.get("total").await.unwrap(), Some(b"10".to_vec()));
        assert_eq!(catalogue.get("total").await.unwrap(), Some(b"999".to_vec()));
    }

    #[test]
    fn a_trailing_separator_is_not_a_second_namespace() {
        let inner = shared() as Arc<dyn Cache>;

        assert_eq!(
            PrefixedCache::new(Arc::clone(&inner), "app").prefix(),
            PrefixedCache::new(inner, "app:").prefix()
        );
    }

    #[tokio::test]
    async fn every_operation_is_namespaced() {
        let inner = shared();
        let cache = PrefixedCache::new(Arc::clone(&inner) as Arc<dyn Cache>, "app");

        cache.increment("hits", 3).await.unwrap();
        assert_eq!(inner.get_string("app:hits").await.unwrap().as_deref(), Some("3"));

        assert!(cache.has("hits").await.unwrap());
        assert!(cache.forget("hits").await.unwrap());
        assert!(!inner.has("app:hits").await.unwrap());
    }

    #[tokio::test]
    async fn flushing_a_namespace_is_refused_rather_than_emptying_the_server() {
        let inner = shared();
        let cache = PrefixedCache::new(Arc::clone(&inner) as Arc<dyn Cache>, "app");

        inner.put("someone-elses-key", b"v", None).await.unwrap();

        let err = cache.flush().await.unwrap_err();
        assert!(err.message().contains("cannot flush"), "{}", err.message());
        assert!(
            inner.get("someone-elses-key").await.unwrap().is_some(),
            "the other application's keys must survive"
        );
    }

    #[tokio::test]
    async fn the_driver_name_passes_through() {
        let cache = PrefixedCache::new(shared() as Arc<dyn Cache>, "app");
        assert_eq!(cache.name(), "memory");
    }
}
