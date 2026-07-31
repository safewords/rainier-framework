//! The [`Cache`] port and the conveniences every implementation gets.

use std::time::Duration;

use rainier_support::{BoxFuture, Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A key/value store with expiry.
///
/// Bytes rather than strings, because the interesting things to cache — a
/// serialised session, a rendered page, a protobuf — are not all text, and
/// forcing them through UTF-8 would mean base64 on top of an already-encoded
/// value.
///
/// Every method is fallible and **a miss is not a failure**: `get` returns
/// `Ok(None)` for an absent key and `Err` only when the cache itself could not
/// be reached. Conflating the two is how a cache outage becomes an outage.
pub trait Cache: Send + Sync + 'static {
    /// A label for diagnostics — `"memory"`, `"redis"`, `"memcached"`.
    fn name(&self) -> &str;

    /// The value at `key`, or `None` if it is absent or expired.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>>;

    /// Store `value`, expiring after `ttl`. `None` means no expiry.
    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>>;

    /// Remove a key. `true` if it was there.
    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// Remove everything.
    ///
    /// On a shared server this removes **everything in the database**, not just
    /// this application's keys — which is why [`prefix`](crate::PrefixedCache)
    /// exists and why this is not something to call on a schedule.
    fn flush(&self) -> BoxFuture<'_, Result<()>>;

    /// Whether this store is visible to other processes.
    ///
    /// The question anything cached **for correctness** has to ask: a lock, a
    /// rate limit, a "has this webhook been handled" marker. Over a per-process
    /// store each instance answers from its own copy, which for a lock means
    /// every instance holds it at once.
    ///
    /// Defaults to `true`, because almost every implementation is a server
    /// somewhere. An in-process one **must** override it — silence here is what
    /// makes the failure quiet.
    fn is_shared(&self) -> bool {
        true
    }

    /// Whether [`add`](Self::add) is genuinely atomic here.
    ///
    /// `true` for everything that can compare-and-set — every driver in this
    /// crate except Cloudflare Workers KV, which has no such operation at all.
    ///
    /// It matters because `add` is the primitive a **lock** is built from. A
    /// store where two callers can both "win" an `add` cannot hold a lock, and
    /// the failure is silent: both callers proceed, both believe they are the
    /// only one, and the report goes out twice. So
    /// [`LockManager::is_shared`](crate::LockManager::is_shared) requires this
    /// as well as [`is_shared`](Self::is_shared), which is what makes the
    /// scheduler refuse rather than trusting an operator to have read the
    /// driver's documentation.
    fn supports_atomic_add(&self) -> bool {
        true
    }

    /// Whether a key is present and unexpired.
    fn has<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.get(key).await?.is_some()) })
    }

    /// Add `by` to a counter, creating it at zero first. Returns the new value.
    ///
    /// Atomic where the driver can be — which is the point of having it on the
    /// port rather than as a get-then-put in application code, because that
    /// version loses increments under concurrency.
    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>>;

    /// Store `value` **only if** `key` is absent. `true` if it was stored.
    ///
    /// # This must be atomic
    ///
    /// Not "atomic where convenient" — atomic. It is the primitive
    /// [`Lock`](crate::Lock) is built from, and therefore the primitive behind
    /// `without_overlapping` and `on_one_server`. A `has` followed by a `put`
    /// satisfies the signature and satisfies no caller: two processes both see
    /// the key absent, both write, both believe they hold the lock, and the
    /// scheduled task runs twice.
    ///
    /// It is a **required** method with no default for exactly that reason. A
    /// default would be inherited silently by the next driver somebody adds,
    /// and the failure it causes is rare, non-local and expensive.
    ///
    /// | Driver | How |
    /// |---|---|
    /// | memory | inside the same mutex as the map |
    /// | Redis | `SET key value NX PX ttl` |
    /// | Memcached | the `add` command, which is atomic by definition |
    /// | DynamoDB | `PutItem` with a condition on the key not existing |
    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>>;

    /// Remove `key` **only if** it currently holds `expected`. `true` if it was
    /// removed.
    ///
    /// The other half of a lock, and the half people leave out.
    ///
    /// Releasing with a plain [`forget`](Self::forget) is wrong in a way that
    /// only shows up under load. A holder that stalls past its TTL — a long GC
    /// pause, a slow query, a suspended VM — has already lost the lock; another
    /// process has taken it. When the first one finally finishes and deletes
    /// the key, it deletes *someone else's* lock, and now a third process can
    /// take it while the second still thinks it holds it.
    ///
    /// Comparing the value first closes that. The value is the holder's random
    /// token, so "is this still mine" has an answer.
    ///
    /// Also required, and for the same reason as [`add`](Self::add).
    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>>;
}

/// The typed conveniences every [`Cache`] gets.
///
/// Kept out of the object-safe trait so `Arc<dyn Cache>` still exists.
#[async_trait::async_trait]
pub trait CacheExt: Cache {
    /// A value, deserialised from JSON.
    async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key).await? {
            Some(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => Ok(Some(value)),
                Err(e) => {
                    // A cached value we cannot parse is a value from an older
                    // shape of the application. Treating it as a miss lets the
                    // caller recompute; failing would make a deploy poison
                    // every request until the cache expired.
                    tracing::warn!(key, error = %e, "discarding an unreadable cached value");
                    Ok(None)
                }
            },
            None => Ok(None),
        }
    }

    /// Store a value as JSON.
    async fn put_json<T: Serialize + Sync>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        self.put(key, &bytes, ttl).await
    }

    /// A string value.
    async fn get_string(&self, key: &str) -> Result<Option<String>> {
        match self.get(key).await? {
            Some(bytes) => Ok(String::from_utf8(bytes).ok()),
            None => Ok(None),
        }
    }

    /// Store a string.
    async fn put_string(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<()> {
        self.put(key, value.as_bytes(), ttl).await
    }

    /// Store a string only if the key is absent.
    ///
    /// [`Cache::add`] with the encoding done for you. Atomic, like the method
    /// it calls.
    async fn add_string(&self, key: &str, value: &str, ttl: Option<Duration>) -> Result<bool> {
        self.add(key, value.as_bytes(), ttl).await
    }

    /// Subtract from a counter.
    async fn decrement(&self, key: &str, by: i64) -> Result<i64> {
        self.increment(key, -by).await
    }

    /// The cached value at `key`, or `compute` it and cache that.
    ///
    /// The pattern nearly every cache use is, written once:
    ///
    /// ```ignore
    /// let settings: Settings = cache
    ///     .remember("settings", Some(Duration::from_secs(300)), || async {
    ///         load_settings(&database).await
    ///     })
    ///     .await?;
    /// ```
    ///
    /// # A failure is never cached
    ///
    /// If `compute` returns an error, the error is returned and **nothing is
    /// stored**. Caching a failure for five minutes turns one bad second into
    /// five bad minutes, and the request that would have succeeded never gets
    /// to try. Cache a *value*; let the next caller retry.
    ///
    /// A cached value that will not deserialise is treated as a miss and
    /// recomputed — see [`get_json`](Self::get_json).
    ///
    /// # It is not a lock
    ///
    /// Ten simultaneous misses run `compute` ten times and the last write
    /// wins. That is the right trade for a cache: serialising them would mean
    /// nine requests waiting on a lock held by a tenth that might fail. When
    /// the computation is expensive enough that a stampede matters, take a
    /// [`Lock`](crate::Lock) around it explicitly.
    async fn remember<T, F, Fut>(&self, key: &str, ttl: Option<Duration>, compute: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned + Sync + Send,
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
    {
        if let Some(cached) = self.get_json::<T>(key).await? {
            return Ok(cached);
        }

        let value = compute().await?;

        // A cache that cannot be written to is a slow application, not a
        // broken one — the value in hand is still the right answer.
        if let Err(e) = self.put_json(key, &value, ttl).await {
            tracing::warn!(key, error = %e.message(), "could not cache a computed value");
        }

        Ok(value)
    }

    /// [`remember`](Self::remember), with no expiry.
    ///
    /// For something that changes only when the
    /// application says so — and which therefore needs
    /// [`forget`](Cache::forget) somewhere, or it is a leak with a nice name.
    async fn remember_forever<T, F, Fut>(&self, key: &str, compute: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned + Sync + Send,
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<T>> + Send,
    {
        self.remember(key, None, compute).await
    }
}

impl<C: Cache + ?Sized> CacheExt for C {}

/// Encode a counter for storage.
///
/// Decimal text rather than little-endian bytes, so a counter set by Rainier
/// reads the same as one set by `redis-cli INCR` — and so `INCR` works on it at
/// all, since Redis parses the stored string.
pub(crate) fn encode_counter(value: i64) -> Vec<u8> {
    value.to_string().into_bytes()
}

/// Decode a counter, treating anything unparseable as zero.
pub(crate) fn decode_counter(bytes: &[u8]) -> Result<i64> {
    if bytes.is_empty() {
        return Ok(0);
    }
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.trim().parse::<i64>().ok())
        .ok_or_else(|| Error::internal("the cached value is not a counter"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_round_trip_as_decimal_text() {
        for value in [0i64, 1, -1, i64::MAX, i64::MIN] {
            assert_eq!(decode_counter(&encode_counter(value)).unwrap(), value);
        }
        assert_eq!(encode_counter(42), b"42".to_vec(), "readable by redis-cli");
    }

    #[test]
    fn an_empty_value_counts_as_zero() {
        assert_eq!(decode_counter(b"").unwrap(), 0);
    }

    #[test]
    fn a_non_counter_is_an_error_rather_than_zero() {
        // Silently treating "hello" as 0 would let an increment overwrite an
        // unrelated value.
        assert!(decode_counter(b"hello").is_err());
    }

    #[tokio::test]
    async fn remember_computes_once_and_then_reads_the_cache() {
        use crate::MemoryCache;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let cache = MemoryCache::new();
        let calls = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let calls = Arc::clone(&calls);
            let value: String = cache
                .remember("settings", None, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok("computed".to_string())
                })
                .await
                .unwrap();

            assert_eq!(value, "computed");
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1, "it recomputed a value it already had");
    }

    #[tokio::test]
    async fn remember_does_not_cache_a_failure() {
        // The whole point. Caching an error for five minutes turns one bad
        // second into five bad minutes, and the request that would have
        // succeeded never gets to try.
        use crate::MemoryCache;

        let cache = MemoryCache::new();

        let failed: Result<String> = cache
            .remember("thing", None, || async { Err(Error::internal("upstream is down")) })
            .await;
        assert!(failed.is_err());

        assert!(cache.get("thing").await.unwrap().is_none(), "the failure was cached");

        // And the next caller gets to succeed.
        let recovered: String = cache
            .remember("thing", None, || async { Ok("it works now".to_string()) })
            .await
            .unwrap();
        assert_eq!(recovered, "it works now");
    }

    #[tokio::test]
    async fn remember_recomputes_a_value_it_can_no_longer_read() {
        // A deploy changed the shape of the cached type. Treating that as a
        // miss is what stops one deploy poisoning every request until the TTL.
        use crate::MemoryCache;

        let cache = MemoryCache::new();
        cache.put("count", b"\"not a number\"", None).await.unwrap();

        let count: u32 = cache.remember("count", None, || async { Ok(7) }).await.unwrap();
        assert_eq!(count, 7);
    }
}
