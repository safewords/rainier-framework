//! Where a throttle keeps its counters — [`RateLimitStore`].
//!
//! A rate limit is a number and a clock, and the only interesting question is
//! **who else can see it**. Over an in-process map, five replicas behind a load
//! balancer enforce "five attempts a minute" five times over, and the effective
//! limit is twenty-five. For a page-view limiter that is a rounding error; for
//! a credential limiter it is the difference between a working control and a
//! decorative one.
//!
//! So the counter is a port. [`MemoryRateLimitStore`] is the default and is
//! honest about being per-process; `rainier-cache` implements this over any
//! [`Cache`](https://docs.rs/rainier-cache), which puts the counters wherever
//! `CACHE_DRIVER` already points.
//!
//! # Why not depend on the cache directly
//!
//! Because a throttle needs *a shared counter*, not *the cache*. Keeping it a
//! port means a deployment can put its limits somewhere else — a dedicated
//! rate-limit service, a database table with a unique index — without the
//! middleware crate learning about any of them. It also keeps
//! `rainier-middleware` depending on `rainier-http` and nothing else, which is
//! what lets it compile for a Worker.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rainier_support::{BoxFuture, Result};

/// One key's state after a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// How many hits this key has taken in the current window, including this
    /// one.
    pub count: u32,
    /// How long until the window resets.
    pub resets_in: Duration,
}

/// A counter a throttle can share.
///
/// Fixed windows rather than a sliding log: one integer and one expiry per key
/// instead of a timestamp per request, which is the difference between a
/// limiter that costs nothing and one that is its own scaling problem. The
/// trade is a boundary effect — a caller can spend a full window's allowance
/// either side of a reset — and for the things people actually rate-limit that
/// is an acceptable answer.
pub trait RateLimitStore: Send + Sync + 'static {
    /// Record one hit against `key`, in a window of `window`.
    ///
    /// Returns the state **after** the hit. The window starts at the first hit
    /// and expires on its own; nothing needs sweeping.
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit>>;

    /// The state of `key` without recording anything.
    ///
    /// `None` when the key has no live window, which is the same answer as
    /// "zero hits" — an expired window and an absent one are indistinguishable
    /// to a caller, and both mean the allowance is whole.
    fn peek<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Hit>>>;

    /// Forget `key`, restoring its full allowance.
    ///
    /// What a successful login calls, so a failed-attempt limiter does not
    /// keep punishing somebody who has just proved who they are.
    fn clear<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Whether other instances of this application see the same counters.
    ///
    /// `false` here means a limit of `n` is really `n × replicas`, and the
    /// application should be told at boot rather than finding out from a
    /// credential-stuffing run that succeeded.
    fn is_shared(&self) -> bool;

    /// A label for diagnostics — `"memory"`, `"cache:redis"`.
    fn name(&self) -> &str;
}

/// The default: one map, this process, no dependencies.
///
/// Right for development, for a single node, and for anything limited for
/// politeness rather than for safety. Wrong for a credential limiter on more
/// than one replica — see [`is_shared`](RateLimitStore::is_shared).
#[derive(Debug, Default)]
pub struct MemoryRateLimitStore {
    windows: Mutex<HashMap<String, Window>>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    count: u32,
    started: Instant,
    length: Duration,
}

impl Window {
    fn is_live(&self, now: Instant) -> bool {
        now.duration_since(self.started) < self.length
    }

    fn resets_in(&self, now: Instant) -> Duration {
        self.length.saturating_sub(now.duration_since(self.started))
    }
}

impl MemoryRateLimitStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many keys are being tracked.
    ///
    /// For a test, and for the one operational question this store raises:
    /// whether it is growing without bound.
    pub fn tracked(&self) -> usize {
        self.windows.lock().expect("rate limit lock poisoned").len()
    }
}

impl RateLimitStore for MemoryRateLimitStore {
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit>> {
        Box::pin(async move {
            let now = Instant::now();
            let mut windows = self.windows.lock().expect("rate limit lock poisoned");

            // Evict on read rather than resetting in place. A key nobody has
            // touched since its window ended is dead weight, and a limiter
            // keyed by IP address accumulates a lot of it — this is what stops
            // the map growing for the lifetime of the process.
            windows.retain(|_, existing| existing.is_live(now));

            let existing = windows.entry(key.to_string()).or_insert(Window {
                count: 0,
                started: now,
                length: window,
            });

            existing.count += 1;
            Ok(Hit { count: existing.count, resets_in: existing.resets_in(now) })
        })
    }

    fn peek<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Hit>>> {
        Box::pin(async move {
            let now = Instant::now();
            let windows = self.windows.lock().expect("rate limit lock poisoned");

            Ok(windows
                .get(key)
                .filter(|window| window.is_live(now))
                .map(|window| Hit { count: window.count, resets_in: window.resets_in(now) }))
        })
    }

    fn clear<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.windows.lock().expect("rate limit lock poisoned").remove(key);
            Ok(())
        })
    }

    fn is_shared(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MemoryRateLimitStore {
        MemoryRateLimitStore::new()
    }

    #[tokio::test]
    async fn hits_count_up_within_a_window() {
        let store = store();
        let window = Duration::from_secs(60);

        for expected in 1..=3 {
            let hit = store.hit("ada", window).await.unwrap();
            assert_eq!(hit.count, expected);
        }
    }

    #[tokio::test]
    async fn keys_are_counted_separately() {
        let store = store();
        let window = Duration::from_secs(60);

        store.hit("ada", window).await.unwrap();
        store.hit("ada", window).await.unwrap();
        let grace = store.hit("grace", window).await.unwrap();

        assert_eq!(grace.count, 1);
        assert_eq!(store.peek("ada").await.unwrap().unwrap().count, 2);
    }

    #[tokio::test]
    async fn a_window_expires_and_the_allowance_comes_back() {
        let store = store();
        let window = Duration::from_millis(30);

        store.hit("ada", window).await.unwrap();
        store.hit("ada", window).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(store.hit("ada", window).await.unwrap().count, 1);
    }

    #[tokio::test]
    async fn an_expired_window_reads_as_nothing_rather_than_as_a_stale_count() {
        let store = store();
        let window = Duration::from_millis(30);

        store.hit("ada", window).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(store.peek("ada").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dead_keys_are_evicted_rather_than_accumulating() {
        // The failure this prevents is not wrong answers; it is a map that
        // grows for the lifetime of the process, one entry per address that
        // ever made a request.
        let store = store();
        let window = Duration::from_millis(30);

        for i in 0..100 {
            store.hit(&format!("ip:{i}"), window).await.unwrap();
        }
        assert_eq!(store.tracked(), 100);

        tokio::time::sleep(Duration::from_millis(50)).await;
        store.hit("ip:fresh", window).await.unwrap();

        assert_eq!(store.tracked(), 1, "the expired keys were not evicted");
    }

    #[tokio::test]
    async fn clearing_restores_the_whole_allowance() {
        let store = store();
        let window = Duration::from_secs(60);

        store.hit("ada", window).await.unwrap();
        store.hit("ada", window).await.unwrap();
        store.clear("ada").await.unwrap();

        assert!(store.peek("ada").await.unwrap().is_none());
        assert_eq!(store.hit("ada", window).await.unwrap().count, 1);
    }

    #[tokio::test]
    async fn the_reset_time_counts_down() {
        let store = store();
        let window = Duration::from_secs(60);

        let first = store.hit("ada", window).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = store.hit("ada", window).await.unwrap();

        assert!(
            second.resets_in < first.resets_in,
            "{:?} !< {:?}",
            second.resets_in,
            first.resets_in
        );
        // And the second hit did not restart the window.
        assert_eq!(second.count, 2);
    }

    #[test]
    fn it_says_it_is_not_shared() {
        assert!(!store().is_shared());
        assert_eq!(store().name(), "memory");
    }
}
