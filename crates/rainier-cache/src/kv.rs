//! Cloudflare Workers KV as a cache (feature `cloudflare-kv`).
//!
//! ```ignore
//! // Inside a Worker, over the binding.
//! let cache = KvCache::new(Arc::new(BindingTransport::new(env.kv("CACHE")?)));
//!
//! // Outside one, over the REST API.
//! let cache = KvCache::new(Arc::new(ApiTransport::new(account, namespace, token)));
//! ```
//!
//! # Read this before using it
//!
//! KV is a **read-heavy, eventually consistent** edge store. Those two words
//! are not a caveat, they are the design: a write is visible at the edge that
//! made it almost immediately and everywhere else within roughly a minute.
//!
//! What that rules out, definitively:
//!
//! | Not this | Because |
//! |---|---|
//! | [`LockManager`](crate::LockManager) | no compare-and-set, so two callers both win |
//! | `on_one_server` / `without_overlapping` | same reason, one layer up |
//! | a rate limiter protecting credentials | a counter that propagates in a minute counts nothing useful in a minute |
//! | a sliding-expiry session | every read would rewrite, and the rewrite is what KV is worst at |
//!
//! What it is genuinely good at: a configuration blob, a feature-flag set, a
//! rendered fragment, a public key set — things read constantly, written
//! rarely, and harmless to serve one version late.
//!
//! # The framework refuses rather than trusting you to know
//!
//! [`Cache::supports_atomic_add`] is `false` here, so a
//! [`LockManager`](crate::LockManager) over this reports itself
//! unshared, and the scheduler's boot check refuses `schedule:run` in
//! production. That is deliberate: "the operator will read the docs" is not a
//! control, and the failure it prevents is silent — two replicas each holding
//! a lock neither of them has.
//!
//! # Minimum expiry
//!
//! KV's shortest `expirationTtl` is **60 seconds**. A shorter one is raised to
//! it rather than rejected, and [`MIN_TTL`] says so, because the alternative is
//! an error from an API call for something the caller could not have known.

use std::sync::Arc;
use std::time::Duration;

use crate::cache::{encode_counter, Cache};
use rainier_support::{BoxFuture, Error, Result};

/// The shortest expiry Cloudflare accepts.
pub const MIN_TTL: Duration = Duration::from_secs(60);

/// The bytes-on-the-wire half, which differs inside a Worker and outside one.
///
/// The same split the D1 executor in `rainier-drivers` uses, and for the same
/// reason:
/// it is the only part that is not wasm-safe everywhere, so keeping it a trait
/// is what lets this module compile into a Worker unchanged.
pub trait KvTransport: Send + Sync + 'static {
    /// The value at `key`, or `None`.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>>;

    /// Write `value`, expiring after `ttl`.
    ///
    /// `ttl` has already been clamped to [`MIN_TTL`] by the caller.
    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>>;

    /// Delete `key`.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Delete every key under `prefix`, if the transport can.
    ///
    /// KV has no flush. The REST API can list and delete in batches; the
    /// Worker binding can too, slowly. A transport that will not is entitled
    /// to say so, and [`Cache::flush`] reports that rather than pretending.
    fn delete_all<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<()>> {
        let _ = prefix;
        Box::pin(async {
            Err(Error::internal(
                "this KV transport cannot flush — KV has no flush operation, so it has to be a \
                 list-and-delete and not every transport implements one",
            ))
        })
    }
}

/// A [`Cache`] over Workers KV.
pub struct KvCache {
    transport: Arc<dyn KvTransport>,
    prefix: String,
}

impl KvCache {
    /// Cache in `transport`.
    pub fn new(transport: Arc<dyn KvTransport>) -> Self {
        Self { transport, prefix: String::new() }
    }

    /// Namespace every key.
    #[must_use = "this returns a configured cache rather than configuring in place"]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    fn key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }

    /// KV's floor, applied rather than reported as an error.
    fn clamp(ttl: Option<Duration>) -> Option<Duration> {
        ttl.map(|ttl| ttl.max(MIN_TTL))
    }
}

impl Cache for KvCache {
    fn name(&self) -> &str {
        "cloudflare-kv"
    }

    fn is_shared(&self) -> bool {
        // Globally, in fact — which is exactly why the *other* property below
        // has to exist. Shared and safe-for-locks are different questions.
        true
    }

    fn supports_atomic_add(&self) -> bool {
        // KV has no compare-and-set. `add` below is a read-then-write, which
        // two callers can both win.
        false
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move { self.transport.get(&self.key(key)).await })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.transport.put(&self.key(key), value, Self::clamp(ttl)).await })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // KV's delete does not report whether anything was there, and a
            // read first would double the cost of every eviction to answer a
            // question almost nobody asks. `true` means "it is gone now".
            self.transport.delete(&self.key(key)).await?;
            Ok(true)
        })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.transport.delete_all(&self.prefix).await })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            // Read, add, write — and **not atomic**, which is the whole story
            // of this driver. Two concurrent increments can both read the same
            // value and one of them is lost.
            //
            // Left in rather than refused because a hit counter that is
            // approximately right is a legitimate use of KV, and refusing
            // would push callers into writing this same loop themselves with
            // no comment explaining it. Anything that must be exact — a rate
            // limit, a quota, a stock level — belongs somewhere else.
            let key = self.key(key);
            let current = match self.transport.get(&key).await? {
                Some(raw) => String::from_utf8_lossy(&raw).trim().parse::<i64>().unwrap_or(0),
                None => 0,
            };

            let next = current.saturating_add(by);
            self.transport.put(&key, &encode_counter(next), None).await?;

            Ok(next)
        })
    }

    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // Read-then-write, and therefore a lie about atomicity — which is
            // what `supports_atomic_add` is for. Two callers racing here both
            // see nothing and both write, and both are told they won.
            //
            // The honest alternative is returning an error, and it is worse:
            // `add` is also how a cache says "only if absent" for entirely
            // benign things, and failing those would make the driver unusable
            // for the workloads it *is* right for.
            let full = self.key(key);

            if self.transport.get(&full).await?.is_some() {
                return Ok(false);
            }

            self.transport.put(&full, value, Self::clamp(ttl)).await?;
            Ok(true)
        })
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // Same story: this is the release half of a lock, and it cannot be
            // atomic here either.
            let full = self.key(key);

            match self.transport.get(&full).await? {
                Some(current) if current == expected => {
                    self.transport.delete(&full).await?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
    }
}

impl std::fmt::Debug for KvCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvCache").field("prefix", &self.prefix).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CacheExt, LockManager};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A transport that behaves like KV without the network: it stores what it
    /// is given and reports what it stored.
    /// What one key holds: its value and the expiry it was written with.
    type Entry = (Vec<u8>, Option<Duration>);

    #[derive(Default)]
    struct FakeKv {
        entries: Mutex<HashMap<String, Entry>>,
    }

    impl FakeKv {
        fn ttl_of(&self, key: &str) -> Option<Duration> {
            self.entries.lock().unwrap().get(key).and_then(|(_, ttl)| *ttl)
        }
    }

    impl KvTransport for FakeKv {
        fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
            Box::pin(async move {
                Ok(self.entries.lock().unwrap().get(key).map(|(value, _)| value.clone()))
            })
        }

        fn put<'a>(
            &'a self,
            key: &'a str,
            value: &'a [u8],
            ttl: Option<Duration>,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.entries.lock().unwrap().insert(key.to_string(), (value.to_vec(), ttl));
                Ok(())
            })
        }

        fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.entries.lock().unwrap().remove(key);
                Ok(())
            })
        }
    }

    fn cache() -> (KvCache, Arc<FakeKv>) {
        let transport = Arc::new(FakeKv::default());
        (KvCache::new(Arc::clone(&transport) as Arc<dyn KvTransport>), transport)
    }

    #[tokio::test]
    async fn values_round_trip() {
        let (cache, _) = cache();

        cache.put("greeting", b"hello", None).await.unwrap();

        assert_eq!(cache.get("greeting").await.unwrap().as_deref(), Some(&b"hello"[..]));
        assert!(cache.has("greeting").await.unwrap());
    }

    #[tokio::test]
    async fn a_short_ttl_is_raised_to_the_floor_rather_than_refused() {
        // A caller asking for ten seconds could not have known KV's minimum,
        // and an error from an API call is a worse way to find out.
        let (cache, transport) = cache();

        cache.put("k", b"v", Some(Duration::from_secs(10))).await.unwrap();

        assert_eq!(transport.ttl_of("k"), Some(MIN_TTL));
    }

    #[tokio::test]
    async fn a_long_ttl_is_left_alone() {
        let (cache, transport) = cache();

        cache.put("k", b"v", Some(Duration::from_secs(3600))).await.unwrap();

        assert_eq!(transport.ttl_of("k"), Some(Duration::from_secs(3600)));
    }

    #[tokio::test]
    async fn the_prefix_namespaces_every_key() {
        let transport = Arc::new(FakeKv::default());
        let cache =
            KvCache::new(Arc::clone(&transport) as Arc<dyn KvTransport>).with_prefix("app:");

        cache.put("k", b"v", None).await.unwrap();

        assert!(transport.entries.lock().unwrap().contains_key("app:k"));
    }

    #[tokio::test]
    async fn it_is_shared_but_cannot_hold_a_lock() {
        // The distinction this whole driver turns on, and the reason
        // `supports_atomic_add` exists at all.
        let (cache, _) = cache();

        assert!(cache.is_shared(), "KV is visible to every replica");
        assert!(!cache.supports_atomic_add(), "and cannot compare-and-set");
    }

    #[tokio::test]
    async fn a_lock_manager_over_it_reports_itself_unshared() {
        // Which is what makes the scheduler's boot check refuse, rather than
        // trusting an operator to have read the driver's documentation.
        let (cache, _) = cache();
        let locks = LockManager::new(Arc::new(cache));

        assert!(!locks.is_shared());
    }

    #[tokio::test]
    async fn add_only_writes_when_the_key_is_absent() {
        // It does the right thing when uncontended, which is why it is here at
        // all — see the comment on the implementation for what it cannot do.
        let (cache, _) = cache();

        assert!(cache.add("k", b"first", None).await.unwrap());
        assert!(!cache.add("k", b"second", None).await.unwrap());
        assert_eq!(cache.get("k").await.unwrap().as_deref(), Some(&b"first"[..]));
    }

    #[tokio::test]
    async fn counters_count() {
        let (cache, _) = cache();

        assert_eq!(cache.increment("hits", 1).await.unwrap(), 1);
        assert_eq!(cache.increment("hits", 5).await.unwrap(), 6);
        assert_eq!(cache.decrement("hits", 2).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn forgetting_removes_it() {
        let (cache, _) = cache();
        cache.put("k", b"v", None).await.unwrap();

        assert!(cache.forget("k").await.unwrap());
        assert!(cache.get("k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_if_compares_first() {
        let (cache, _) = cache();
        cache.put("lock", b"mine", None).await.unwrap();

        assert!(!cache.forget_if("lock", b"somebody-elses").await.unwrap());
        assert!(cache.forget_if("lock", b"mine").await.unwrap());
    }

    #[tokio::test]
    async fn flushing_says_it_cannot_rather_than_pretending() {
        // KV has no flush. Returning `Ok(())` would be a cache that reports it
        // cleared itself and did not.
        let (cache, _) = cache();

        let error = cache.flush().await.unwrap_err();
        assert!(error.message().contains("no flush"), "{}", error.message());
    }
}
