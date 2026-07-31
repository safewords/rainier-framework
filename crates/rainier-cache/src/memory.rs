//! [`MemoryCache`] — a cache in this process's memory.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rainier_support::{BoxFuture, Result};

use crate::cache::{decode_counter, encode_counter, Cache};

/// A cache held in this process.
///
/// Right for development, tests, and anything genuinely per-process. Two
/// instances behind a load balancer do **not** share it, which for a cache
/// is a performance question rather than a correctness one — unlike a
/// [session](https://docs.rs/rainier-session), where it is both.
///
/// The exception is anything you cache *for* correctness: a rate-limit counter
/// or a lock in a memory cache is per-instance, so the limit is really `N ×
/// limit` and the lock is not one.
pub struct MemoryCache {
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone)]
struct Entry {
    value: Vec<u8>,
    expires_at: Option<DateTime<Utc>>,
}

impl Entry {
    fn is_live(&self) -> bool {
        match self.expires_at {
            Some(at) => at > Utc::now(),
            None => true,
        }
    }
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self { entries: Mutex::new(HashMap::new()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // A panic elsewhere must not make every later cache read fail; the map's
        // invariants do not depend on the panicking section having finished.
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many entries are held, expired ones included.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every expired entry. Returns how many.
    ///
    /// Reads clean up the key they touch, but a key never read again would
    /// otherwise sit there — so a long-lived process wants this on a timer.
    pub fn purge(&self) -> u64 {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|_, entry| entry.is_live());
        (before - entries.len()) as u64
    }

    fn expiry(ttl: Option<Duration>) -> Option<DateTime<Utc>> {
        ttl.and_then(|ttl| chrono::Duration::from_std(ttl).ok()).map(|ttl| Utc::now() + ttl)
    }
}

impl Cache for MemoryCache {
    fn name(&self) -> &str {
        "memory"
    }

    /// One process. Two instances of the application share nothing here.
    fn is_shared(&self) -> bool {
        false
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            let mut entries = self.lock();
            match entries.get(key) {
                Some(entry) if entry.is_live() => Ok(Some(entry.value.clone())),
                Some(_) => {
                    entries.remove(key);
                    Ok(None)
                }
                None => Ok(None),
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
            self.lock().insert(
                key.to_string(),
                Entry { value: value.to_vec(), expires_at: Self::expiry(ttl) },
            );
            Ok(())
        })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.lock().remove(key).is_some()) })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.lock().clear();
            Ok(())
        })
    }

    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // The check and the write under one lock. That is the whole
            // difference between this and a `has` then a `put`, and it is why
            // an in-process lock works at all.
            let mut entries = self.lock();

            if entries.get(key).is_some_and(Entry::is_live) {
                return Ok(false);
            }

            entries.insert(
                key.to_string(),
                Entry { value: value.to_vec(), expires_at: Self::expiry(ttl) },
            );
            Ok(true)
        })
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let mut entries = self.lock();

            match entries.get(key) {
                Some(entry) if entry.is_live() && entry.value == expected => {
                    entries.remove(key);
                    Ok(true)
                }
                // Absent, expired, or somebody else's — all three mean "not
                // yours to remove", and the caller wants to know which only in
                // the sense of "did I still hold it".
                _ => Ok(false),
            }
        })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            // Under one lock, so two concurrent increments cannot both read the
            // same value and lose one of the two.
            let mut entries = self.lock();

            let current = match entries.get(key) {
                Some(entry) if entry.is_live() => decode_counter(&entry.value)?,
                _ => 0,
            };
            let next = current.saturating_add(by);

            // The expiry is preserved, not reset: a counter with a window on it
            // must not have its window extended by being counted.
            let expires_at = entries.get(key).filter(|e| e.is_live()).and_then(|e| e.expires_at);
            entries.insert(key.to_string(), Entry { value: encode_counter(next), expires_at });

            Ok(next)
        })
    }
}

impl std::fmt::Debug for MemoryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryCache").field("entries", &self.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheExt;

    #[tokio::test]
    async fn a_value_round_trips() {
        let cache = MemoryCache::new();
        cache.put("k", b"v", None).await.unwrap();

        assert_eq!(cache.get("k").await.unwrap(), Some(b"v".to_vec()));
        assert!(cache.has("k").await.unwrap());
    }

    #[tokio::test]
    async fn a_miss_is_none_not_an_error() {
        assert_eq!(MemoryCache::new().get("absent").await.unwrap(), None);
        assert!(!MemoryCache::new().has("absent").await.unwrap());
    }

    #[tokio::test]
    async fn forgetting_reports_whether_it_was_there() {
        let cache = MemoryCache::new();
        cache.put("k", b"v", None).await.unwrap();

        assert!(cache.forget("k").await.unwrap());
        assert!(!cache.forget("k").await.unwrap());
    }

    #[tokio::test]
    async fn an_expired_value_reads_as_absent_and_is_dropped() {
        let cache = MemoryCache::new();
        cache.put("k", b"v", Some(Duration::from_nanos(1))).await.unwrap();

        // Nanosecond TTL: already gone.
        assert_eq!(cache.get("k").await.unwrap(), None);
        assert!(cache.is_empty(), "reading an expired key should clean it up");
    }

    #[tokio::test]
    async fn no_ttl_means_no_expiry() {
        let cache = MemoryCache::new();
        cache.put("k", b"v", None).await.unwrap();

        assert_eq!(cache.purge(), 0);
        assert!(cache.get("k").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn purge_removes_only_the_expired() {
        let cache = MemoryCache::new();
        cache.put("gone", b"v", Some(Duration::from_nanos(1))).await.unwrap();
        cache.put("kept", b"v", None).await.unwrap();

        assert_eq!(cache.purge(), 1);
        assert_eq!(cache.len(), 1);
        assert!(cache.get("kept").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn flush_empties_everything() {
        let cache = MemoryCache::new();
        cache.put("a", b"1", None).await.unwrap();
        cache.put("b", b"2", None).await.unwrap();

        cache.flush().await.unwrap();
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn increment_starts_from_zero() {
        let cache = MemoryCache::new();

        assert_eq!(cache.increment("hits", 1).await.unwrap(), 1);
        assert_eq!(cache.increment("hits", 1).await.unwrap(), 2);
        assert_eq!(cache.increment("hits", 5).await.unwrap(), 7);
        assert_eq!(cache.decrement("hits", 3).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn increment_does_not_extend_the_window() {
        // A rate-limit counter whose window resets on every request is not a
        // rate limit.
        let cache = MemoryCache::new();
        cache.put("hits", b"0", Some(Duration::from_secs(60))).await.unwrap();
        let before = cache.lock().get("hits").unwrap().expires_at;

        cache.increment("hits", 1).await.unwrap();

        assert_eq!(cache.lock().get("hits").unwrap().expires_at, before);
    }

    #[tokio::test]
    async fn increment_saturates_rather_than_wrapping() {
        let cache = MemoryCache::new();
        cache.put("k", i64::MAX.to_string().as_bytes(), None).await.unwrap();

        assert_eq!(cache.increment("k", 1).await.unwrap(), i64::MAX);
    }

    #[tokio::test]
    async fn incrementing_a_non_counter_errors_rather_than_overwriting() {
        let cache = MemoryCache::new();
        cache.put("name", b"Ada", None).await.unwrap();

        assert!(cache.increment("name", 1).await.is_err());
        assert_eq!(cache.get("name").await.unwrap(), Some(b"Ada".to_vec()));
    }

    #[tokio::test]
    async fn concurrent_increments_do_not_lose_any() {
        let cache = std::sync::Arc::new(MemoryCache::new());

        let mut handles = Vec::new();
        for _ in 0..50 {
            let cache = std::sync::Arc::clone(&cache);
            handles.push(tokio::spawn(async move { cache.increment("hits", 1).await }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        assert_eq!(cache.get_string("hits").await.unwrap().as_deref(), Some("50"));
    }

    #[tokio::test]
    async fn json_round_trips() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Post {
            id: u64,
            title: String,
        }

        let cache = MemoryCache::new();
        let post = Post { id: 1, title: "Hello".into() };

        cache.put_json("post", &post, None).await.unwrap();
        assert_eq!(cache.get_json::<Post>("post").await.unwrap(), Some(post));
    }

    #[tokio::test]
    async fn an_unreadable_cached_value_reads_as_a_miss() {
        // A deploy that changes a cached shape must not poison every request
        // until the key expires.
        let cache = MemoryCache::new();
        cache.put("post", b"not json at all", None).await.unwrap();

        assert_eq!(cache.get_json::<Vec<u8>>("post").await.unwrap(), None);
    }

    #[tokio::test]
    async fn strings_round_trip() {
        let cache = MemoryCache::new();
        cache.put_string("name", "Ada", None).await.unwrap();

        assert_eq!(cache.get_string("name").await.unwrap().as_deref(), Some("Ada"));
    }

    #[tokio::test]
    async fn add_only_stores_when_absent() {
        let cache = MemoryCache::new();

        assert!(cache.add("k", b"first", None).await.unwrap());
        assert!(!cache.add("k", b"second", None).await.unwrap());
        assert_eq!(cache.get("k").await.unwrap(), Some(b"first".to_vec()));
    }

    #[tokio::test]
    async fn the_port_is_object_safe() {
        let cache: std::sync::Arc<dyn Cache> = std::sync::Arc::new(MemoryCache::new());

        cache.put("k", b"v", None).await.unwrap();
        assert_eq!(cache.get_string("k").await.unwrap().as_deref(), Some("v"));
    }
}
