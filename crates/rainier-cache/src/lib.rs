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
//! ## Declaring stores
//!
//! [`Stores`] is the `cache` section as a type: a default store and named ones,
//! each carrying its own settings, built in one call.
//!
//! ```
//! # use rainier_cache::{StoreConfig, Stores};
//! let stores = Stores::new("shared")
//!     .with("shared", StoreConfig::redis("redis://127.0.0.1:6379/1"))
//!     .with("scratch", StoreConfig::memory());
//! # assert_eq!(stores.default_name(), "shared");
//! ```
//!
//! ## Timeouts, and why there is no pool to size
//!
//! The Redis connection **multiplexes**: one socket carries every concurrent
//! command, so a pool on top would add sockets without adding throughput. A
//! `max_connections` on a `redis` store is therefore refused by name rather
//! than accepted and ignored — and a `memcached` store *does* take a
//! `pool_size`, because its protocol has no request ids and one connection
//! really does serve one command at a time. The difference is in the protocols,
//! not in how finished the two are.
//!
//! What a multiplexed connection can honour is a connect timeout, a response
//! timeout and reconnection, declared per store. All three matter more here
//! than anywhere else, because the cache is on the hot path of nearly every
//! request:
//!
//! - without a **response timeout**, a server that accepted a command and never
//!   answered stalls every request that touches a session, a cached value or a
//!   rate limit, all at once;
//! - without **reconnection**, one dropped socket — a proxy reaping an idle
//!   connection is the usual way — breaks this cache for the life of the
//!   process, since a multiplexed connection does not re-open itself.
//!
//! [`stores`] has the whole account.
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
pub mod stores;

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
pub use stores::{
    CacheResources, ConnectionSettings, DynamoDbStore, KvStore, MemcachedStore, MemoryStore,
    RedisClusterStore, RedisStore, StoreConfig, StoreCredentials, Stores,
};

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

    /// Stores reachable by name, beyond the default one above.
    ///
    /// An application that keeps different kinds of value in different places —
    /// a shared store for anything cached for correctness, an in-process one for
    /// a memoised computation — needs more than one, and [`Stores`] is where
    /// they are declared.
    ///
    /// A `BTreeMap` so a dump reads the same each run.
    stores: std::collections::BTreeMap<String, Arc<dyn Cache>>,
}

impl CacheManager {
    /// Wrap a cache.
    pub fn new(store: Arc<dyn Cache>) -> Self {
        Self { store, stores: std::collections::BTreeMap::new() }
    }

    /// An in-process cache — the default, and right for development.
    pub fn memory() -> Self {
        Self::new(Arc::new(MemoryCache::new()))
    }

    /// Register `store` under `name`, beside the default.
    ///
    /// The default is **not** registered under a name by this — [`Stores::build`]
    /// does both, from one declaration, so the two are the same backend rather
    /// than two built from the same settings.
    pub fn with_store(mut self, name: impl Into<String>, store: Arc<dyn Cache>) -> Self {
        self.stores.insert(name.into(), store);
        self
    }

    /// The store registered under `name`, if there is one.
    ///
    /// Named `store_named` rather than overloading [`store`](Self::store),
    /// which is the default one and has callers.
    pub fn store_named(&self, name: &str) -> Option<&Arc<dyn Cache>> {
        self.stores.get(name)
    }

    /// Whether a store is registered under `name`.
    pub fn has_store(&self, name: &str) -> bool {
        self.stores.contains_key(name)
    }

    /// Every registered name, in a stable order.
    pub fn store_names(&self) -> impl Iterator<Item = &str> {
        self.stores.keys().map(String::as_str)
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
        f.debug_struct("CacheManager")
            .field("driver", &self.driver())
            .field("stores", &self.store_names().collect::<Vec<_>>())
            .finish()
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
