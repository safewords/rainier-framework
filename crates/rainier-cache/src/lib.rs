//! # rainier-cache
//!
//! A [`Cache`] port and its drivers — the cache surface an MVC framework
//! ships, with the drivers you would expect.
//!
//! ```
//! use rainier_cache::{Cache, CacheExt, MemoryCache};
//! use std::time::Duration;
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let cache = MemoryCache::new();
//!
//! cache.put_string("greeting", "hello", Some(Duration::from_secs(60))).await?;
//! assert_eq!(cache.get_string("greeting").await?.as_deref(), Some("hello"));
//!
//! // A counter, incremented atomically.
//! assert_eq!(cache.increment("hits", 1).await?, 1);
//! # Ok(()) }
//! ```
//!
//! ## Drivers
//!
//! | Driver | Feature | Shared between instances |
//! |---|---|---|
//! | [`MemoryCache`] | — | no |
//! | `RedisCache` | `redis-driver` | yes |
//! | `RedisCache` on a cluster | `redis-cluster` | yes, sharded |
//! | `MemcachedCache` | `memcached` | yes |
//! | `DynamoDbCache` | `dynamodb` | yes, and no server to run |
//!
//! The transports live in [`rainier-drivers`](https://docs.rs/rainier-drivers)
//! rather than here, because Redis is wanted by the queue too and an
//! application should configure it once.
//!
//! Needs the matching feature, so this one is not compiled as a doctest:
//!
//! ```ignore
//! // One server…
//! let cache = RedisCache::connect(RedisConnector::open("redis://127.0.0.1/")?).await?;
//!
//! // …or a sharded cluster, with no change to anything downstream.
//! let cache = RedisCache::connect(
//!     RedisConnector::open_cluster(["redis://10.0.0.1:6379", "redis://10.0.0.2:6379"])?,
//! ).await?;
//! ```
//!
//! ## A miss is not a failure
//!
//! [`get`](Cache::get) returns `Ok(None)` for an absent key and `Err` only when
//! the cache could not be reached. Conflating the two is how a cache outage
//! becomes an application outage — and a cache is the one dependency an
//! application should be able to lose.
//!
//! Driver errors are [`ServiceUnavailable`](rainier_support::ErrorKind::ServiceUnavailable),
//! so they land in the right bucket on a dashboard rather than looking like
//! bugs.
//!
//! ## Namespace a shared cache
//!
//! Two applications caching `user:1` on one Redis database read each other's
//! values, and the symptom is a user seeing another application's data.
//! [`PrefixedCache`] fixes it, and makes "flush mine" refuse rather than empty
//! the server:
//!
//! ```
//! # use rainier_cache::{Cache, MemoryCache, PrefixedCache};
//! # use std::sync::Arc;
//! let shared: Arc<dyn Cache> = Arc::new(MemoryCache::new());
//! let mine = PrefixedCache::new(shared, "billing");
//! ```
//!
//! ## What not to cache in memory
//!
//! A [`MemoryCache`] is per-process, so anything cached **for correctness**
//! rather than for speed is wrong in it: a rate-limit counter becomes `N ×
//! limit` across `N` instances, and a lock is not a lock.
//!
//! [`Cache::add`] and [`Cache::forget_if`] are atomic on every driver, so a
//! [`Lock`] over a shared one is a real lock. Over a `MemoryCache` it is a real
//! lock *within this process* and nothing at all between two —
//! [`LockManager::is_shared`] is the check worth making at boot.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod cache;
pub mod driver;
#[cfg(feature = "cloudflare-kv")]
pub mod kv;
pub mod lock;
pub mod memory;
pub mod prefixed;
pub mod rate_limit;

#[cfg(feature = "dynamodb")]
pub mod dynamodb;
#[cfg(feature = "memcached")]
pub mod memcached;
#[cfg(feature = "redis-driver")]
pub mod redis;

pub use cache::{Cache, CacheExt};
pub use driver::CacheDriver;
#[cfg(feature = "cloudflare-kv")]
pub use kv::{KvCache, KvTransport, MIN_TTL as KV_MIN_TTL};
pub use lock::{Lock, LockGuard, LockManager};
pub use memory::MemoryCache;
pub use prefixed::PrefixedCache;
pub use rate_limit::CacheRateLimiter;

#[cfg(feature = "dynamodb")]
pub use dynamodb::DynamoDbCache;
#[cfg(feature = "memcached")]
pub use memcached::MemcachedCache;
#[cfg(feature = "redis-driver")]
pub use redis::RedisCache;

use std::sync::Arc;

/// The application's cache, as one container-storable value.
///
/// A newtype over the port rather than binding `Arc<dyn Cache>` directly, so
/// swapping a driver does not change the type every call site names — the same
/// shape as `rainier-framework`'s `Views`.
#[derive(Clone)]
pub struct CacheManager {
    store: Arc<dyn Cache>,
}

impl CacheManager {
    /// Wrap a cache.
    pub fn new(store: Arc<dyn Cache>) -> Self {
        Self { store }
    }

    /// An in-process cache — the default, and right for development.
    pub fn memory() -> Self {
        Self::new(Arc::new(MemoryCache::new()))
    }

    /// The cache underneath.
    pub fn store(&self) -> &Arc<dyn Cache> {
        &self.store
    }

    /// The driver's name — `"memory"`, `"redis"`, `"redis-cluster"`,
    /// `"memcached"`.
    pub fn driver(&self) -> &str {
        self.store.name()
    }
}

impl std::ops::Deref for CacheManager {
    type Target = Arc<dyn Cache>;

    /// So `Cache::instance().get(..)` works without naming `store()`.
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl std::fmt::Debug for CacheManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheManager").field("driver", &self.driver()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn the_manager_delegates_to_its_store() {
        let cache = CacheManager::memory();

        cache.put_string("k", "v", None).await.unwrap();
        assert_eq!(cache.get_string("k").await.unwrap().as_deref(), Some("v"));
        assert_eq!(cache.driver(), "memory");
    }

    #[tokio::test]
    async fn the_manager_reaches_the_typed_helpers_through_deref() {
        let cache = CacheManager::memory();

        cache.put_json("n", &vec![1u8, 2, 3], Some(Duration::from_secs(30))).await.unwrap();
        assert_eq!(cache.get_json::<Vec<u8>>("n").await.unwrap(), Some(vec![1, 2, 3]));
    }

    #[tokio::test]
    async fn a_prefixed_cache_can_be_the_manager() {
        let shared: Arc<dyn Cache> = Arc::new(MemoryCache::new());
        let cache = CacheManager::new(Arc::new(PrefixedCache::new(shared, "app")));

        cache.put_string("k", "v", None).await.unwrap();
        assert_eq!(cache.get_string("k").await.unwrap().as_deref(), Some("v"));
        assert_eq!(cache.driver(), "memory", "the prefix is not a driver");
    }
}
