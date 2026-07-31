//! Rate-limit counters in the cache — [`CacheRateLimiter`].
//!
//! ```ignore
//! let limits: Arc<dyn RateLimitStore> = Arc::new(CacheRateLimiter::new(cache.store()));
//!
//! router.post("/login", login).middleware(
//!     ThrottleRequests::per_minute(5).named("login").stored_in(Arc::clone(&limits)),
//! );
//! ```
//!
//! The counters go wherever `CACHE_DRIVER` already points, the same way
//! [`LockManager`](crate::LockManager) does — so a deployment that has already
//! decided where its shared state lives does not decide again for this.
//!
//! # Two keys, and why
//!
//! A fixed window needs the counter to expire on its own, and the cache port
//! has no "increment, and set a TTL if you just created it". So the first hit
//! does `add(key, 0, ttl)` — atomic, and `false` if somebody beat it — and then
//! `increment`. That is the `SET key 0 EX 60 NX` / `INCR` pair everybody writes
//! against Redis by hand, and it has the same property: the window is anchored
//! to its **first** hit and cannot be extended by later ones. A caller cannot
//! hold a window open by continuing to knock.
//!
//! The second key holds **when the window ends**, as a wall-clock instant. The
//! cache port cannot report a key's remaining TTL, and without it `retry-after`
//! could only say "the whole window" — telling somebody who is one second from
//! the reset to come back in a minute. On a credential limiter that is a real
//! cost paid by the person who mistyped their password, so it is worth the
//! extra read.

use std::sync::Arc;
use std::time::Duration;

use rainier_middleware::{Hit, RateLimitStore};
use rainier_support::{BoxFuture, Result};

use crate::cache::Cache;

/// Rate-limit counters kept in a [`Cache`].
pub struct CacheRateLimiter {
    cache: Arc<dyn Cache>,
    prefix: String,
}

impl CacheRateLimiter {
    /// Count in `cache`, under the `rate:` prefix.
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache, prefix: "rate:".to_string() }
    }

    /// Count under a different prefix.
    ///
    /// Worth changing when the cache is shared with something else that might
    /// pick the same names.
    #[must_use = "this returns a configured limiter rather than configuring in place"]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// The cache underneath.
    pub fn cache(&self) -> &Arc<dyn Cache> {
        &self.cache
    }

    fn counter(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    /// The companion key holding when this window ends.
    ///
    /// Stored rather than derived, because the cache port cannot report a
    /// key's remaining TTL — and `retry-after` has to say something true.
    fn deadline(&self, key: &str) -> String {
        format!("{}{}:until", self.prefix, key)
    }
}

impl RateLimitStore for CacheRateLimiter {
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit>> {
        Box::pin(async move {
            let counter = self.counter(key);
            let deadline = self.deadline(key);

            // Atomic, and `false` when somebody else opened the window first —
            // which is the whole reason this is `add` and not `put`.
            let opened = self.cache.add(&counter, b"0", Some(window)).await?;

            let ends_at = if opened {
                // Only the caller that opened the window writes the deadline,
                // so a later hit cannot push it back. An absolute instant
                // rather than a duration: the point is when it ends, and every
                // later reader is asking at a different time.
                let ends_at = now_millis() + window.as_millis() as u64;
                let _ =
                    self.cache.add(&deadline, ends_at.to_string().as_bytes(), Some(window)).await;
                Some(ends_at)
            } else {
                read_millis(self.cache.get(&deadline).await?)
            };

            let count = self.cache.increment(&counter, 1).await?;

            Ok(Hit { count: count.max(0) as u32, resets_in: remaining(ends_at, window) })
        })
    }

    fn peek<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Hit>>> {
        Box::pin(async move {
            let Some(raw) = self.cache.get(&self.counter(key)).await? else {
                return Ok(None);
            };

            let count = String::from_utf8_lossy(&raw).trim().parse::<u32>().unwrap_or(0);
            let ends_at = read_millis(self.cache.get(&self.deadline(key)).await?);

            Ok(Some(Hit { count, resets_in: remaining(ends_at, Duration::ZERO) }))
        })
    }

    fn clear<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.cache.forget(&self.counter(key)).await?;
            self.cache.forget(&self.deadline(key)).await?;
            Ok(())
        })
    }

    fn is_shared(&self) -> bool {
        self.cache.is_shared()
    }

    fn name(&self) -> &str {
        self.cache.name()
    }
}

/// Now, in milliseconds since the epoch.
///
/// A wall clock rather than an `Instant`, because the deadline is written by
/// one process and read by another — and an `Instant` means nothing outside
/// the process that made it.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// A stored deadline, if it is there and reads as a number.
fn read_millis(stored: Option<Vec<u8>>) -> Option<u64> {
    String::from_utf8(stored?).ok()?.trim().parse().ok()
}

/// How long until `ends_at`, or `fallback` when there is no deadline to read.
///
/// The fallback matters: the deadline key can be missing where the counter is
/// not — a cache that evicted one under memory pressure, or a window opened by
/// an older version of this code. Reporting the whole window then is an
/// over-estimate, which is the safe direction for a `retry-after`.
fn remaining(ends_at: Option<u64>, fallback: Duration) -> Duration {
    match ends_at {
        Some(ends_at) => Duration::from_millis(ends_at.saturating_sub(now_millis())),
        None => fallback,
    }
}

impl std::fmt::Debug for CacheRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheRateLimiter")
            .field("cache", &self.cache.name())
            .field("shared", &self.cache.is_shared())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryCache;

    fn limiter() -> CacheRateLimiter {
        CacheRateLimiter::new(Arc::new(MemoryCache::new()))
    }

    #[tokio::test]
    async fn hits_count_up() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        for expected in 1..=3 {
            assert_eq!(limiter.hit("ada", window).await.unwrap().count, expected);
        }
    }

    #[tokio::test]
    async fn keys_are_counted_separately() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        limiter.hit("ada", window).await.unwrap();
        limiter.hit("ada", window).await.unwrap();

        assert_eq!(limiter.hit("grace", window).await.unwrap().count, 1);
        assert_eq!(limiter.peek("ada").await.unwrap().unwrap().count, 2);
    }

    #[tokio::test]
    async fn the_window_expires_on_its_own() {
        let limiter = limiter();
        let window = Duration::from_millis(40);

        limiter.hit("ada", window).await.unwrap();
        limiter.hit("ada", window).await.unwrap();

        tokio::time::sleep(Duration::from_millis(60)).await;

        assert!(limiter.peek("ada").await.unwrap().is_none());
        assert_eq!(limiter.hit("ada", window).await.unwrap().count, 1);
    }

    #[tokio::test]
    async fn a_later_hit_cannot_extend_the_window() {
        // The property that makes this a fixed window rather than a sliding
        // one: knocking repeatedly must not hold the door open.
        let limiter = limiter();
        let window = Duration::from_millis(120);

        // Opens the window at t=0, so it ends at t=120.
        limiter.hit("ada", window).await.unwrap();

        // Three more inside it, at t=20, t=40, t=60. Deliberately not up to
        // the boundary: a hit at exactly t=120 finds the counter already gone
        // and legitimately opens the *next* window, which is the behaviour and
        // not the bug.
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let hit = limiter.hit("ada", window).await.unwrap();
            assert!(hit.count > 1, "a hit inside the window opened a new one");
        }

        // Now past t=120, and it is gone despite four hits' worth of traffic.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(limiter.peek("ada").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clearing_restores_the_allowance() {
        let limiter = limiter();
        let window = Duration::from_secs(60);

        limiter.hit("ada", window).await.unwrap();
        limiter.hit("ada", window).await.unwrap();
        limiter.clear("ada").await.unwrap();

        assert!(limiter.peek("ada").await.unwrap().is_none());
        assert_eq!(limiter.hit("ada", window).await.unwrap().count, 1);
    }

    #[tokio::test]
    async fn it_reports_the_sharedness_of_its_cache() {
        // The whole point of putting counters here, and the thing a boot check
        // asks about.
        assert!(!limiter().is_shared());
        assert_eq!(limiter().name(), "memory");
    }

    #[tokio::test]
    async fn peeking_at_nothing_is_none() {
        assert!(limiter().peek("nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_reset_time_counts_down_rather_than_reporting_the_whole_window() {
        // The reason the second key exists. Telling somebody one second from
        // the reset to come back in a minute is a real cost, paid by the
        // person who mistyped their password.
        let limiter = limiter();
        let window = Duration::from_millis(400);

        let first = limiter.hit("ada", window).await.unwrap();
        assert!(first.resets_in <= window);

        tokio::time::sleep(Duration::from_millis(150)).await;
        let second = limiter.hit("ada", window).await.unwrap();

        assert!(
            second.resets_in < first.resets_in,
            "{:?} should be less than {:?}",
            second.resets_in,
            first.resets_in
        );
        assert!(second.resets_in < Duration::from_millis(300), "{:?}", second.resets_in);
    }

    #[tokio::test]
    async fn a_missing_deadline_falls_back_rather_than_reporting_zero() {
        // The deadline key can be gone where the counter is not — an eviction
        // under memory pressure, or a window opened by an older build. A
        // `retry-after` of zero would invite an immediate retry that is
        // refused again.
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let limiter = CacheRateLimiter::new(Arc::clone(&cache));
        let window = Duration::from_secs(60);

        limiter.hit("ada", window).await.unwrap();
        cache.forget("rate:ada:until").await.unwrap();

        let hit = limiter.hit("ada", window).await.unwrap();

        assert_eq!(hit.count, 2, "the counter itself is untouched");
        assert_eq!(hit.resets_in, window, "it falls back to the whole window");
    }
}
