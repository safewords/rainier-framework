//! Atomic locks — [`LockManager`], [`Lock`] and [`LockGuard`].
//!
//! Named locks over the cache, and the machinery behind the scheduler's
//! `without_overlapping()` and `on_one_server()`.
//!
//! ```
//! # use rainier_cache::{LockManager, MemoryCache};
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let locks = LockManager::new(Arc::new(MemoryCache::new()));
//!
//! let outcome = locks
//!     .lock("reports:nightly", Duration::from_secs(600))
//!     .run(async {
//!         // …exactly one caller reaches here.
//!         Ok::<_, rainier_support::Error>("built")
//!     })
//!     .await?;
//!
//! assert_eq!(outcome, Some("built"));
//! # Ok(()) }
//! ```
//!
//! # What makes it a lock
//!
//! Three things, and leaving out any one of them produces something that looks
//! like a lock and is not.
//!
//! **An atomic acquire.** [`Cache::add`] is "store only if absent", decided by
//! the store rather than by the caller. A `has` followed by a `put` lets two
//! processes both observe the key absent and both write.
//!
//! **An owner token.** Every acquire mints a random one and stores it as the
//! value. Release is [`Cache::forget_if`], not `forget` — see below.
//!
//! **A TTL.** The holder can die. A lock with no expiry, taken by a process
//! that is then `kill -9`ed, is a task that never runs again until somebody
//! notices and deletes a key by hand.
//!
//! ## Why release compares the token
//!
//! The bug this avoids is worth spelling out, because it is invisible until it
//! is a production incident:
//!
//! ```text
//! t=0    A acquires `nightly`, ttl 60s          A holds it
//! t=0    A begins work
//! t=61   A is still stalled — GC, a slow query, a suspended VM
//! t=61   the key expires                        nobody holds it
//! t=62   B acquires `nightly`                   B holds it
//! t=63   A finishes and releases
//!          - with `forget`:     A deletes B's lock. C can now acquire while
//!                               B is still running. Two copies.
//!          - with `forget_if`:  A's token no longer matches. Nothing happens.
//! ```
//!
//! `forget_if` cannot make A's overrun safe — B is already running — but it
//! stops one overrun becoming an unbounded number.
//!
//! ## The guarantee, stated honestly
//!
//! This is a **lease**, not a mutex. If a holder overruns its TTL, another
//! process will take the lock and both will run. No lock built on a TTL can
//! prevent that, including Redlock; the standard advice applies:
//!
//! - Set the TTL comfortably longer than the work takes.
//! - [`extend`](LockGuard::extend) it from long-running work, so the TTL tracks
//!   progress rather than a guess made at the start.
//! - Where correctness genuinely cannot tolerate two runs, make the work
//!   idempotent or fence it with a token the *downstream* system checks. A lock
//!   is a coordination hint, not a correctness proof.
//!
//! And the obvious one: a lock in a [`MemoryCache`](crate::MemoryCache) is
//! per-process, so `on_one_server` across three servers holding three memory
//! caches is three servers all believing they are the one.
//! [`LockManager::is_shared`] answers that question.

use std::sync::Arc;
use std::time::Duration;

use rainier_support::{Error, Result};

use crate::cache::Cache;

/// How long to wait between attempts when blocking for a lock.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Mints [`Lock`]s against a cache.
///
/// Bind one in the container and resolve it; a lock taken through a different
/// cache is a different lock.
pub struct LockManager {
    cache: Arc<dyn Cache>,
    prefix: String,
    declared_shared: bool,
}

impl LockManager {
    /// Locks in `cache`, under the `lock:` prefix.
    pub fn new(cache: Arc<dyn Cache>) -> Self {
        Self { cache, prefix: "lock:".to_string(), declared_shared: false }
    }

    /// Locks under a different prefix.
    ///
    /// Worth changing when the cache is shared with something else that might
    /// pick the same names.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// A lock called `name`, held for at most `ttl`.
    pub fn lock(&self, name: impl Into<String>, ttl: Duration) -> Lock {
        Lock {
            cache: Arc::clone(&self.cache),
            key: format!("{}{}", self.prefix, name.into()),
            ttl,
            wait: None,
        }
    }

    /// Whether locks taken here are visible to other processes.
    ///
    /// `false` for an in-process cache, which means `on_one_server` is not
    /// doing what its name says. Worth asserting at boot rather than
    /// discovering from a report that ran twice.
    ///
    /// ```
    /// # use rainier_cache::{LockManager, MemoryCache};
    /// # use std::sync::Arc;
    /// let locks = LockManager::new(Arc::new(MemoryCache::new()));
    /// assert!(!locks.is_shared());
    /// ```
    pub fn is_shared(&self) -> bool {
        // Both, and the second is not redundant. A store can be perfectly
        // shared and still be unable to hold a lock: Cloudflare Workers KV is
        // visible to every replica on earth and has no compare-and-set, so two
        // callers both "win" the `add` and both believe they hold it.
        self.declared_shared || (self.cache.is_shared() && self.cache.supports_atomic_add())
    }

    /// Take this store's word for it: locks here *are* visible to other
    /// processes.
    ///
    /// For a [`Cache`] implemented outside this crate that has not overridden
    /// [`Cache::is_shared`] — a Consul, an etcd, somebody's own Redis client.
    /// Nothing verifies the claim, so declaring it about a per-process store
    /// disables the one check that would have caught it.
    ///
    /// ```
    /// # use rainier_cache::{LockManager, MemoryCache};
    /// # use std::sync::Arc;
    /// let locks = LockManager::new(Arc::new(MemoryCache::new())).declared_shared();
    /// assert!(locks.is_shared());
    /// ```
    #[must_use = "this returns a configured manager rather than configuring in place"]
    pub fn declared_shared(mut self) -> Self {
        self.declared_shared = true;
        self
    }

    /// The cache underneath.
    pub fn cache(&self) -> &Arc<dyn Cache> {
        &self.cache
    }
}

impl std::fmt::Debug for LockManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockManager")
            .field("cache", &self.cache.name())
            .field("prefix", &self.prefix)
            .finish()
    }
}

/// A named lock, not yet held.
///
/// Built by [`LockManager::lock`]. Nothing is taken until
/// [`acquire`](Self::acquire) or [`run`](Self::run).
pub struct Lock {
    cache: Arc<dyn Cache>,
    key: String,
    ttl: Duration,
    wait: Option<Duration>,
}

impl Lock {
    /// Wait up to `wait` for the lock instead of giving up immediately.
    ///
    /// Polls. That is deliberate: the alternative is a pub/sub channel per lock
    /// name, which is a lot of moving parts to shave a hundred milliseconds off
    /// a path that is by definition contended and by definition rare.
    ///
    /// Use it for "this must happen, just not twice at once". Leave it off for
    /// "if somebody else is already doing this, I have nothing to do" — which
    /// is the scheduler's case, and why `without_overlapping` does not wait.
    pub fn wait_for(mut self, wait: Duration) -> Self {
        self.wait = Some(wait);
        self
    }

    /// Take the lock, or return `None` if somebody else holds it.
    pub async fn acquire(&self) -> Result<Option<LockGuard>> {
        let deadline = self.wait.map(|wait| std::time::Instant::now() + wait);

        loop {
            let owner = mint_owner();

            if self.cache.add(&self.key, owner.as_bytes(), Some(self.ttl)).await? {
                return Ok(Some(LockGuard {
                    cache: Arc::clone(&self.cache),
                    key: self.key.clone(),
                    owner,
                    released: false,
                }));
            }

            match deadline {
                Some(deadline) if std::time::Instant::now() + POLL_INTERVAL < deadline => {
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                _ => return Ok(None),
            }
        }
    }

    /// Run `work` while holding the lock, or return `None` without running it.
    ///
    /// The lock is released whether `work` succeeds or fails — a task that
    /// errors must not hold its lock until the TTL expires, or one bad night
    /// skips the next hour of runs too.
    ///
    /// ```
    /// # use rainier_cache::{LockManager, MemoryCache};
    /// # use std::sync::Arc;
    /// # use std::time::Duration;
    /// # #[tokio::main] async fn main() -> rainier_support::Result<()> {
    /// # let locks = LockManager::new(Arc::new(MemoryCache::new()));
    /// let lock = locks.lock("import", Duration::from_secs(60));
    ///
    /// match lock.run(async { Ok::<_, rainier_support::Error>(42) }).await? {
    ///     Some(value) => assert_eq!(value, 42),
    ///     None => unreachable!("nobody else holds it"),
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn run<T, F>(&self, work: F) -> Result<Option<T>>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let Some(guard) = self.acquire().await? else {
            return Ok(None);
        };

        let outcome = work.await;

        // Released before the result is inspected, so an `Err` does not skip
        // the release on its way out.
        guard.release().await?;
        outcome.map(Some)
    }

    /// Take the lock over from whoever holds it.
    ///
    /// An escape hatch for an operator, not for code. The holder does not find
    /// out, and if it is still running you now have two.
    pub async fn force_release(&self) -> Result<bool> {
        self.cache.forget(&self.key).await
    }

    /// Whether anybody holds it.
    ///
    /// Racy by nature — true the moment you ask and false the moment after —
    /// so it is for a status page, not for deciding whether to acquire.
    pub async fn is_held(&self) -> Result<bool> {
        self.cache.has(&self.key).await
    }

    /// The cache key, for diagnostics.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl std::fmt::Debug for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lock").field("key", &self.key).field("ttl", &self.ttl).finish()
    }
}

/// A held lock.
///
/// # This does not release on drop
///
/// Releasing means a round trip to the cache, and `Drop` cannot await. The
/// options were a blocking call inside `Drop` (which deadlocks a single-threaded
/// runtime), spawning a task (which needs a runtime handle that may be shutting
/// down), or being explicit.
///
/// Explicit, then — with the TTL as the safety net. A guard that is dropped
/// without [`release`](Self::release) holds its lock until it expires, which is
/// late rather than forever. Prefer [`Lock::run`], which cannot forget.
#[must_use = "a dropped guard holds its lock until the TTL expires; call `release`"]
pub struct LockGuard {
    cache: Arc<dyn Cache>,
    key: String,
    owner: String,
    released: bool,
}

impl LockGuard {
    /// Release it. `true` if it was still ours to release.
    ///
    /// `false` means the TTL expired while we were working and somebody else
    /// has since taken it — so the work overran, and *two copies have been
    /// running*. Worth logging where it matters; it is the signal that the TTL
    /// is too short.
    pub async fn release(mut self) -> Result<bool> {
        self.released = true;
        self.cache.forget_if(&self.key, self.owner.as_bytes()).await
    }

    /// Push the expiry out to `ttl` from now. `true` if it was still ours.
    ///
    /// For work that is still going and does not know how long it has left. A
    /// `false` here is the same bad news as from `release`, and arrives early
    /// enough to abandon the work rather than finish it alongside a second
    /// copy.
    ///
    /// Implemented as a re-`put` rather than a bare `EXPIRE`, so it works on
    /// every driver — including the ones with no separate expiry command.
    pub async fn extend(&self, ttl: Duration) -> Result<bool> {
        // Check-then-write, and the gap is real: the lock could expire between
        // reading and writing, and this would resurrect it under our name while
        // another holder has it. Narrow, and the alternative is a per-driver
        // conditional-set primitive for a path that is already a best-effort
        // heartbeat. `release` is the one that has to be exact, and it is.
        match self.cache.get(&self.key).await? {
            Some(current) if current == self.owner.as_bytes() => {
                self.cache.put(&self.key, self.owner.as_bytes(), Some(ttl)).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Give up the guard **without** releasing the lock, so it stands until the
    /// TTL expires.
    ///
    /// Deliberate abandonment, not a leak — and distinct from dropping the
    /// guard, which logs a warning on the assumption you forgot.
    ///
    /// The case it exists for is a claim that has to outlive the work: the
    /// scheduler's `on_one_server` takes a lock for the current minute so the
    /// *other* machines find it taken, and releasing it when the run finishes
    /// would let a machine whose clock is a second behind claim the same
    /// minute and run it again.
    pub fn keep(mut self) {
        self.released = true;
    }

    /// Whether we still hold it.
    pub async fn is_still_held(&self) -> Result<bool> {
        Ok(self.cache.get(&self.key).await?.as_deref() == Some(self.owner.as_bytes()))
    }

    /// The random token identifying this holder.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The cache key.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if !self.released {
            // Not an error — the TTL covers it — but it is nearly always a
            // missing `release`, and the symptom otherwise is a task that
            // mysteriously does not run for the next few minutes.
            tracing::debug!(
                key = %self.key,
                "a lock guard was dropped without being released; it will expire on its TTL"
            );
        }
    }
}

impl std::fmt::Debug for LockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockGuard").field("key", &self.key).finish()
    }
}

/// A random token identifying one holder.
///
/// Two sources mixed, because either alone has a failure mode: a timestamp
/// collides when two processes acquire in the same nanosecond, and a process id
/// collides across machines. Together with the address of a fresh allocation
/// they are enough — this identifies a holder, it does not authenticate one.
fn mint_owner() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    // `RandomState` is seeded per process from the OS, and hashing a
    // monotonically-changing value gives a different token per call.
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    hasher.write_usize(&hasher as *const _ as usize);

    format!("{:016x}{:016x}", hasher.finish(), std::process::id())
}

/// The error a caller gets when a lock is required rather than optional.
pub fn contended(name: &str) -> Error {
    Error::internal(format!("`{name}` is already running elsewhere"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryCache;

    fn locks() -> LockManager {
        LockManager::new(Arc::new(MemoryCache::new()))
    }

    #[tokio::test]
    async fn exactly_one_caller_gets_the_lock() {
        let locks = locks();

        let first = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap();
        let second = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap();

        assert!(first.is_some());
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn releasing_lets_the_next_caller_in() {
        let locks = locks();

        let guard = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();
        assert!(guard.release().await.unwrap());

        assert!(locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_release_after_the_lock_moved_on_does_not_take_it_from_the_new_holder() {
        // The bug the owner token exists for. A holder that overran has already
        // lost the lock; its release must be a no-op, not a theft.
        let locks = locks();
        let cache = Arc::clone(locks.cache());

        let stale = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        // Simulate the TTL passing and somebody else taking it.
        cache.forget("lock:job").await.unwrap();
        let fresh = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        // The stale holder releases. It must not remove the fresh holder's lock.
        assert!(!stale.release().await.unwrap(), "the stale release should be a no-op");
        assert!(fresh.is_still_held().await.unwrap(), "the new holder should still hold it");

        // And a third caller must still be locked out.
        assert!(locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn two_holders_get_different_tokens() {
        let locks = locks();

        let first = locks.lock("a", Duration::from_secs(60)).acquire().await.unwrap().unwrap();
        let second = locks.lock("b", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        assert_ne!(first.owner(), second.owner());
    }

    #[tokio::test]
    async fn an_expired_lock_is_available_again() {
        let locks = locks();

        let _held = locks.lock("job", Duration::from_millis(30)).acquire().await.unwrap().unwrap();
        assert!(locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().is_none());

        tokio::time::sleep(Duration::from_millis(60)).await;

        assert!(
            locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().is_some(),
            "the TTL is what stops a dead holder locking a task out forever"
        );
    }

    #[tokio::test]
    async fn run_releases_on_success() {
        let locks = locks();
        let lock = locks.lock("job", Duration::from_secs(60));

        assert_eq!(lock.run(async { Ok(7) }).await.unwrap(), Some(7));
        assert!(!lock.is_held().await.unwrap());
    }

    #[tokio::test]
    async fn run_releases_on_failure_too() {
        // A task that errors must not hold its lock until the TTL: one bad run
        // would skip every scheduled run until then.
        let locks = locks();
        let lock = locks.lock("job", Duration::from_secs(3600));

        let outcome: Result<Option<()>> = lock.run(async { Err(Error::internal("boom")) }).await;

        assert!(outcome.is_err());
        assert!(!lock.is_held().await.unwrap(), "the lock should have been released");
    }

    #[tokio::test]
    async fn run_skips_the_work_when_somebody_else_holds_it() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let locks = locks();
        let _held = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        let ran = AtomicBool::new(false);
        let outcome = locks
            .lock("job", Duration::from_secs(60))
            .run(async {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();

        assert!(outcome.is_none());
        assert!(!ran.load(Ordering::SeqCst), "the work must not run");
    }

    #[tokio::test]
    async fn waiting_gets_the_lock_once_it_is_released() {
        let locks = locks();
        let guard = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        let cache = Arc::clone(locks.cache());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            guard.release().await.unwrap();
            drop(cache);
        });

        let waited = locks
            .lock("job", Duration::from_secs(60))
            .wait_for(Duration::from_secs(2))
            .acquire()
            .await
            .unwrap();

        assert!(waited.is_some(), "waiting should have outlasted the holder");
    }

    #[tokio::test]
    async fn waiting_gives_up_at_the_deadline() {
        let locks = locks();
        let _held = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        let started = std::time::Instant::now();
        let waited = locks
            .lock("job", Duration::from_secs(60))
            .wait_for(Duration::from_millis(250))
            .acquire()
            .await
            .unwrap();

        assert!(waited.is_none());
        assert!(started.elapsed() < Duration::from_secs(2), "it should not wait forever");
    }

    #[tokio::test]
    async fn extending_keeps_a_long_job_from_losing_its_lock() {
        let locks = locks();
        let guard = locks.lock("job", Duration::from_millis(50)).acquire().await.unwrap().unwrap();

        assert!(guard.extend(Duration::from_secs(60)).await.unwrap());

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().is_none(),
            "the original TTL would have expired by now"
        );
    }

    #[tokio::test]
    async fn extending_a_lock_that_moved_on_says_so() {
        let locks = locks();
        let guard = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        locks.lock("job", Duration::from_secs(60)).force_release().await.unwrap();
        let _other = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        assert!(!guard.extend(Duration::from_secs(60)).await.unwrap());
        assert!(!guard.is_still_held().await.unwrap());
    }

    #[tokio::test]
    async fn keep_leaves_the_lock_standing_until_the_ttl() {
        let locks = locks();

        locks.lock("minute", Duration::from_millis(60)).acquire().await.unwrap().unwrap().keep();

        assert!(
            locks.lock("minute", Duration::from_secs(60)).acquire().await.unwrap().is_none(),
            "the claim should still stand"
        );

        tokio::time::sleep(Duration::from_millis(90)).await;
        assert!(
            locks.lock("minute", Duration::from_secs(60)).acquire().await.unwrap().is_some(),
            "and expire on its own"
        );
    }

    #[tokio::test]
    async fn a_memory_cache_is_not_a_shared_lock_and_admits_it() {
        // The check worth making at boot: `on_one_server` over a per-process
        // cache is every server believing it is the one.
        assert!(!locks().is_shared());
    }

    #[tokio::test]
    async fn different_names_do_not_contend() {
        let locks = locks();

        let a = locks.lock("a", Duration::from_secs(60)).acquire().await.unwrap();
        let b = locks.lock("b", Duration::from_secs(60)).acquire().await.unwrap();

        assert!(a.is_some() && b.is_some());
    }

    #[tokio::test]
    async fn the_prefix_keeps_locks_out_of_the_caches_own_namespace() {
        let locks = locks();
        let guard = locks.lock("job", Duration::from_secs(60)).acquire().await.unwrap().unwrap();

        assert_eq!(guard.key(), "lock:job");
        assert!(Arc::clone(locks.cache()).has("lock:job").await.unwrap());
        assert!(!Arc::clone(locks.cache()).has("job").await.unwrap());
    }
}
