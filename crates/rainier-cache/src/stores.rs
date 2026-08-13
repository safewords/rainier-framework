//! Stores as configuration — [`Stores`], [`StoreConfig`], [`RedisStore`],
//! [`CacheResources`].
//!
//! A [`CacheManager`] holds a default store and, once an application has more
//! than one backend, named ones. Something has to put them there. Doing it
//! imperatively works until two stores live on **different backends**, at which
//! point the loop that builds them all from one connector produces a store with
//! the right name pointed at the wrong server.
//!
//! That failure is quiet in the way a cache's failures always are. A cache is
//! the one dependency an application is supposed to be able to lose, so
//! everything downstream is built to treat absence as normal: a miss is not an
//! error, a value that is not there is simply recomputed. A store pointed at the
//! wrong server is therefore not an outage — it is a permanent cache miss, which
//! looks like a slow application and reports nothing at all. And when the thing
//! cached was a rate-limit counter or a lock, it is not slow, it is wrong.
//!
//! So a store declares **its own** settings, and is built from those alone:
//!
//! ```
//! use rainier_cache::{StoreConfig, Stores};
//!
//! let stores = Stores::new("default")
//!     .with("default", StoreConfig::memory())
//!     .with("shared", StoreConfig::redis("redis://127.0.0.1:6379/1"));
//!
//! assert_eq!(stores.default_name(), "default");
//! assert!(stores.get("shared").is_some());
//! ```
//!
//! Declaring is separate from building, which is what lets the example above run
//! anywhere: [`build`](Stores::build) on a `redis` store needs the
//! `redis-driver` feature and **fails without it** rather than quietly
//! substituting an in-process cache. That is the right behaviour and it makes
//! `build` the wrong thing to put in a doc example — this one demonstrates the
//! shape, and the tests below build.
//!
//! ## The same thing, from the configuration tree
//!
//! [`Stores`] deserialises from the shape a `cache` section already has — a
//! `default` naming one of the entries in `stores`, and each entry naming its
//! own driver:
//!
//! ```
//! # use rainier_cache::Stores;
//! # use serde_json::json;
//! let stores: Stores = serde_json::from_value(json!({
//!     "default": "shared",
//!     "stores": {
//!         "shared": {
//!             "driver": "redis",
//!             "url": "redis://127.0.0.1:6379/1",
//!             "response_timeout_ms": 250,
//!             "reconnect": true,
//!         },
//!         "scratch": { "driver": "memory" },
//!     },
//! })).unwrap();
//!
//! assert_eq!(stores.default_name(), "shared");
//! ```
//!
//! Nothing here is an application's business but the values: the framework names
//! no store, no server and no environment variable.
//!
//! ## What a `redis` store waits for, and why it has no pool
//!
//! **The Redis connection multiplexes.** One socket carries every concurrent
//! command and the client matches each reply to the request that asked for it,
//! so a pool on top would open more sockets without moving more commands. There
//! is nothing to size, and `max_connections` on a `redis` store is refused by
//! name rather than accepted and ignored.
//!
//! What that shape of connection can honour instead is three settings, and on a
//! cache they matter more than anywhere else, because the cache is on the hot
//! path of nearly every request:
//!
//! | Setting | What it bounds | What goes wrong without it |
//! |---|---|---|
//! | `connect_timeout_ms` | opening the socket, handshake included | a process booting against a route that goes nowhere waits minutes before saying anything |
//! | `response_timeout_ms` | one command waiting for its reply | a server that accepted the command and went quiet stalls every request that touches a session, a cached value or a rate limit — all at once, and the symptom names nothing |
//! | `reconnect` | nothing — it *recovers* | **the important one**: a multiplexed connection does not re-open itself, so one socket dropped by an idle proxy breaks the cache for the life of the process |
//!
//! Milliseconds, and named so: a command's budget on the hot path cannot be
//! written in whole seconds, where the only values available are `0`, which
//! would fail everything, and `1`, which is already longer than a request can
//! afford to wait for a cache read.
//!
//! All three are **off unless declared**, so a section that says nothing behaves
//! as it did before they existed — including the store that does not reconnect,
//! which is why `reconnect` is the one to reach for first.
//!
//! ## Memcached does pool, and says so
//!
//! The contrast is worth stating, because it is what makes the Redis answer a
//! design rather than a gap. A Memcached connection has no request ids: replies
//! are matched to requests by order, so one connection serves one command at a
//! time and concurrency genuinely needs more of them. A `memcached` store
//! therefore takes a `pool_size` and a `redis` store does not — the difference
//! is in the protocols, not in how finished the two are.
//!
//! ## What a declaration refuses
//!
//! | Declaration | Why it is refused |
//! |---|---|
//! | no `driver` | an assumed driver is a store pointed at whatever the default happens to be |
//! | `url` on a `memory` store | somebody believes this cache is shared between processes; it is not |
//! | `max_connections`, `min_connections`, `acquire_timeout`, `idle_timeout`, `max_lifetime`, `test_before_acquire` | no store here has a pool of that shape — see above |
//! | `pool_size` on anything but `memcached` | only Memcached has a pool to size |
//! | `key` without `secret` | falls back to the ambient chain, and reads a **different account's** table |
//! | `key` and `secret` with no `region` | a signed request has to name one, and a guess is a wrong one |
//! | `reconnect_attempts` without `reconnect` | reads as reconnection being on and behaves as it being off |
//! | `default` naming an undeclared store | the fallback would be silent, and the wrong store |
//!
//! ## What a store cannot declare
//!
//! One driver is built from something no configuration file can hold: `kv` needs
//! a [`KvTransport`](crate::kv::KvTransport), which is a binding inside a Worker
//! and an API client outside one. It arrives through [`CacheResources`] rather
//! than through the config tree.
//!
//! It is still per-store in the sense that matters: a driver that needs one and
//! was not given it is a boot failure naming the missing piece, not a store that
//! quietly becomes something else.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::cache::Cache;
use crate::driver::CacheDriver;
use crate::prefixed::PrefixedCache;
use crate::{CacheManager, MemoryCache};

/// The stores an application declares, and which of them is the default.
///
/// The `cache` section, as a type. Deserialises from the configuration tree and
/// builds a [`CacheManager`] in one call, so declaring a store is a config edit
/// rather than a line of wiring.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stores {
    /// Which entry of `stores` the manager's default store is.
    #[serde(default = "conventional_default")]
    default: String,

    /// Every declared store, by the name callers reach it with.
    ///
    /// A `BTreeMap` so a dump and a build order are stable — a `HashMap` would
    /// make an error that lists the declared stores read differently each run.
    #[serde(default)]
    stores: BTreeMap<String, StoreConfig>,
}

/// The store name assumed when a `cache` section does not say.
///
/// A convention rather than a guess at the application's naming, and the same
/// one [`CacheDriver`]'s default names. A `default` naming a store that is not
/// declared fails at [`build`](Stores::build) rather than falling back.
fn conventional_default() -> String {
    "memory".to_string()
}

impl Stores {
    /// An empty set whose default store will be `default`.
    ///
    /// The name has to be declared with [`with`](Self::with) before
    /// [`build`](Self::build) will succeed.
    pub fn new(default: impl Into<String>) -> Self {
        Self { default: default.into(), stores: BTreeMap::new() }
    }

    /// Declare a store under `name`.
    pub fn with(mut self, name: impl Into<String>, store: impl Into<StoreConfig>) -> Self {
        self.stores.insert(name.into(), store.into());
        self
    }

    /// The name of the store that will be the default.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// The declaration filed under `name`.
    pub fn get(&self, name: &str) -> Option<&StoreConfig> {
        self.stores.get(name)
    }

    /// Every declared name, in a stable order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.stores.keys().map(String::as_str)
    }

    /// Whether anything is declared at all.
    pub fn is_empty(&self) -> bool {
        self.stores.is_empty()
    }

    /// Build every declared store and assemble them into a [`CacheManager`].
    ///
    /// Each store is built from **its own** declaration. There is no shared
    /// connector to inherit from, which is the entire point: two stores on two
    /// servers with two sets of timeouts are two backends, and a second store
    /// that inherited the first's connector would keep its own *name* while
    /// reading and writing somebody else's keys.
    ///
    /// A store is built **once** and registered under its name *and*, if it is
    /// the default, as the default. Building it twice would give
    /// `store_named("scratch")` a different backend from the default store even
    /// though both name one declaration — invisible for `redis`, and for
    /// `memory` a write through one that cannot be read through the other.
    ///
    /// The default name is checked before anything is built, so a typo fails
    /// immediately instead of after opening connections that were never going
    /// to be used.
    ///
    /// # Errors
    ///
    /// When the default names an undeclared store, when a driver's feature is
    /// off, when a resource a driver needs was not given, or when a backend
    /// refuses the connection.
    pub async fn build(&self, resources: &CacheResources) -> Result<CacheManager> {
        if !self.stores.contains_key(&self.default) {
            return Err(Error::internal(format!(
                "the default cache store `{}` is not declared; declared stores are {}",
                self.default,
                self.declared()
            )));
        }

        let mut built: Vec<(&str, Arc<dyn Cache>)> = Vec::with_capacity(self.stores.len());
        for (name, store) in &self.stores {
            // Named, so that "needs a region" with a dozen stores declared is a
            // fix rather than a search — but the **kind is kept**. A server that
            // cannot be reached is a `ServiceUnavailable` here as everywhere
            // else in this crate, and re-wrapping it as an internal error would
            // put a dependency outage in the bucket reserved for our own bugs.
            let cache = store.build(resources).await.map_err(|e| {
                Error::new(e.kind(), format!("cache store `{name}`: {}", e.message()))
            })?;
            built.push((name, cache));
        }

        let default = built
            .iter()
            .find(|(name, _)| *name == self.default)
            .map(|(_, store)| Arc::clone(store))
            .expect("the default was checked against the same map");

        let mut manager = CacheManager::new(default);
        for (name, store) in built {
            manager = manager.with_store(name, store);
        }
        Ok(manager)
    }

    /// The declared names, backtick-quoted, for an error message.
    fn declared(&self) -> String {
        if self.stores.is_empty() {
            return "none".to_string();
        }
        self.names().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
    }
}

// Deliberately no `Default`: an empty set declares no stores, so its default
// name cannot resolve and `build` fails. A constructor whose result does not
// work is worse than one that asks for the one thing it needs.

impl std::fmt::Debug for Stores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stores")
            .field("default", &self.default)
            .field("stores", &self.stores)
            .finish()
    }
}

/// What a store needs that the configuration tree cannot hold.
///
/// Only `kv` needs anything: its transport is a binding inside a Worker and an
/// API client outside one, and neither is a value a configuration file can
/// carry. Everything else builds from its declaration alone.
///
/// The same shape as the queue's `QueueResources`, and for the same reason — a
/// driver that needs one and was not given it is a boot failure naming the
/// missing piece rather than a store that quietly becomes something else.
#[derive(Clone, Default)]
pub struct CacheResources {
    #[cfg(feature = "cloudflare-kv")]
    kv: Option<Arc<dyn crate::kv::KvTransport>>,
}

impl CacheResources {
    /// Nothing supplied, which is right for every driver but `kv`.
    pub fn new() -> Self {
        Self::default()
    }

    /// The transport a `kv` store is built over.
    #[cfg(feature = "cloudflare-kv")]
    pub fn with_kv_transport(mut self, transport: Arc<dyn crate::kv::KvTransport>) -> Self {
        self.kv = Some(transport);
        self
    }

    /// The KV transport, if one was given.
    #[cfg(feature = "cloudflare-kv")]
    pub fn kv_transport(&self) -> Option<&Arc<dyn crate::kv::KvTransport>> {
        self.kv.as_ref()
    }
}

impl std::fmt::Debug for CacheResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(feature = "cloudflare-kv")]
        let kv = self.kv.is_some();
        #[cfg(not(feature = "cloudflare-kv"))]
        let kv = false;

        f.debug_struct("CacheResources").field("kv_transport", &kv).finish()
    }
}

/// One store: which driver, and the settings that driver needs.
///
/// An enum rather than a struct of optionals, so the settings a driver does not
/// have cannot be written down: there is no `url` on a memory store to fill in
/// and wonder why it is ignored. The wire form is still flat — `driver` beside
/// the rest — because that is what a configuration file wants to be.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "RawStore", into = "RawStore")]
pub enum StoreConfig {
    /// This process only.
    Memory(MemoryStore),

    /// One Redis server, or a Sentinel-fronted primary.
    Redis(RedisStore),

    /// A sharded Redis Cluster.
    RedisCluster(RedisClusterStore),

    /// Memcached over its text protocol.
    Memcached(MemcachedStore),

    /// A DynamoDB table, with its TTL doing the expiry.
    DynamoDb(DynamoDbStore),

    /// Cloudflare Workers KV, over a transport from [`CacheResources`].
    Kv(KvStore),
}

impl StoreConfig {
    /// Values in this process's memory. The shorthand.
    pub fn memory() -> Self {
        Self::Memory(MemoryStore::default())
    }

    /// Values on the Redis server at `url`. The shorthand.
    pub fn redis(url: impl Into<String>) -> Self {
        Self::Redis(RedisStore::new(url))
    }

    /// Which driver this declares.
    pub fn driver(&self) -> CacheDriver {
        match self {
            Self::Memory(_) => CacheDriver::Memory,
            Self::Redis(_) => CacheDriver::Redis,
            Self::RedisCluster(_) => CacheDriver::RedisCluster,
            Self::Memcached(_) => CacheDriver::Memcached,
            Self::DynamoDb(_) => CacheDriver::DynamoDb,
            Self::Kv(_) => CacheDriver::Kv,
        }
    }

    /// The key prefix this store namespaces with, if one was declared.
    pub fn prefix(&self) -> Option<&str> {
        match self {
            Self::Memory(store) => store.prefix.as_deref(),
            Self::Redis(store) => store.prefix.as_deref(),
            Self::RedisCluster(store) => store.prefix.as_deref(),
            Self::Memcached(store) => store.prefix.as_deref(),
            Self::DynamoDb(store) => store.prefix.as_deref(),
            Self::Kv(store) => store.prefix.as_deref(),
        }
    }

    /// Build this store, and only this store.
    ///
    /// Every setting it uses comes from this declaration, so two stores built
    /// from two declarations share nothing — not a connector, not a credential,
    /// not a timeout.
    ///
    /// # Errors
    ///
    /// When the driver's feature is off, when a required resource is missing, or
    /// when the backend refuses the connection.
    pub async fn build(&self, resources: &CacheResources) -> Result<Arc<dyn Cache>> {
        let store = self.build_unprefixed(resources).await?;

        // Applied here rather than in each driver, so "namespace this store"
        // means one thing across all of them — including refusing to flush,
        // which `PrefixedCache` does because emptying a shared server is not
        // what "flush mine" was asking for.
        Ok(match self.prefix() {
            Some(prefix) => Arc::new(PrefixedCache::new(store, prefix)),
            None => store,
        })
    }

    /// The store itself, before any namespacing.
    async fn build_unprefixed(&self, resources: &CacheResources) -> Result<Arc<dyn Cache>> {
        let _ = resources;

        match self {
            Self::Memory(_) => Ok(Arc::new(MemoryCache::new())),

            #[cfg(feature = "redis-driver")]
            Self::Redis(store) => Ok(Arc::new(store.build().await?)),

            // Loud, and naming the fix. Falling back to an in-process cache
            // would "work": every read and write would succeed, in a store no
            // other process can see — so a rate limiter counts to `N ×` its
            // limit across `N` replicas and a lock is not a lock.
            #[cfg(not(feature = "redis-driver"))]
            Self::Redis(store) => Err(Error::internal(format!(
                "this store uses the `redis` driver for `{}`, but rainier-cache was built \
                 without the `redis-driver` feature",
                store.url_without_credentials()
            ))),

            #[cfg(feature = "redis-cluster")]
            Self::RedisCluster(store) => Ok(Arc::new(store.build().await?)),

            #[cfg(not(feature = "redis-cluster"))]
            Self::RedisCluster(store) => Err(Error::internal(format!(
                "this store uses the `redis-cluster` driver across {} seeds, but rainier-cache \
                 was built without the `redis-cluster` feature",
                store.seeds.len()
            ))),

            #[cfg(feature = "memcached")]
            Self::Memcached(store) => Ok(Arc::new(store.build())),

            #[cfg(not(feature = "memcached"))]
            Self::Memcached(store) => Err(Error::internal(format!(
                "this store uses the `memcached` driver for `{}`, but rainier-cache was built \
                 without the `memcached` feature",
                store.url
            ))),

            #[cfg(feature = "dynamodb")]
            Self::DynamoDb(store) => Ok(Arc::new(store.build().await?)),

            #[cfg(not(feature = "dynamodb"))]
            Self::DynamoDb(store) => Err(Error::internal(format!(
                "this store uses the `dynamodb` driver for table `{}`, but rainier-cache was \
                 built without the `dynamodb` feature",
                store.table
            ))),

            #[cfg(feature = "cloudflare-kv")]
            Self::Kv(_) => {
                let transport = resources.kv_transport().ok_or_else(|| {
                    Error::internal(
                        "this store uses the `kv` driver, but no transport was given to build it \
                         with: KV is reached through a binding inside a Worker and an API client \
                         outside one, and neither is something a configuration file can hold. \
                         Pass one with `CacheResources::with_kv_transport`",
                    )
                })?;
                Ok(Arc::new(crate::kv::KvCache::new(Arc::clone(transport))))
            }

            #[cfg(not(feature = "cloudflare-kv"))]
            Self::Kv(_) => Err(Error::internal(
                "this store uses the `kv` driver, but rainier-cache was built without the \
                 `cloudflare-kv` feature",
            )),
        }
    }
}

impl From<MemoryStore> for StoreConfig {
    fn from(store: MemoryStore) -> Self {
        Self::Memory(store)
    }
}

impl From<RedisStore> for StoreConfig {
    fn from(store: RedisStore) -> Self {
        Self::Redis(store)
    }
}

impl From<RedisClusterStore> for StoreConfig {
    fn from(store: RedisClusterStore) -> Self {
        Self::RedisCluster(store)
    }
}

impl From<MemcachedStore> for StoreConfig {
    fn from(store: MemcachedStore) -> Self {
        Self::Memcached(store)
    }
}

impl From<DynamoDbStore> for StoreConfig {
    fn from(store: DynamoDbStore) -> Self {
        Self::DynamoDb(store)
    }
}

impl From<KvStore> for StoreConfig {
    fn from(store: KvStore) -> Self {
        Self::Kv(store)
    }
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(store) => std::fmt::Debug::fmt(store, f),
            Self::Redis(store) => std::fmt::Debug::fmt(store, f),
            Self::RedisCluster(store) => std::fmt::Debug::fmt(store, f),
            Self::Memcached(store) => std::fmt::Debug::fmt(store, f),
            Self::DynamoDb(store) => std::fmt::Debug::fmt(store, f),
            Self::Kv(store) => std::fmt::Debug::fmt(store, f),
        }
    }
}

/// Values in this process's memory.
///
/// Fast, and shares nothing between instances. Anything cached **for
/// correctness** rather than for speed is wrong in it: a rate-limit counter
/// becomes `N ×` its limit across `N` instances, and a lock is not a lock.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    prefix: Option<String>,
}

impl MemoryStore {
    /// An in-process store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Namespace every key.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }
}

/// Values on one Redis server.
///
/// **No pool, by design** — see the [module docs](self). The three settings
/// below are what a multiplexed connection can honour, and
/// [`reconnect`](Self::reconnect) is the one that decides whether this store
/// survives a dropped socket.
#[derive(Clone)]
pub struct RedisStore {
    url: String,
    prefix: Option<String>,
    connection: ConnectionSettings,
}

impl RedisStore {
    /// A store on the server at `url` — `redis://host:port/db`, or `rediss://`
    /// for TLS.
    ///
    /// Give it a **different database index from the queue's**, in the URL's
    /// path. Flushing a cache empties its whole database, and every job waiting
    /// in that index goes with it.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), prefix: None, connection: ConnectionSettings::default() }
    }

    /// Namespace every key.
    ///
    /// What keeps two applications on one Redis database from reading each
    /// other's values, and the symptom of not having it is a user seeing another
    /// application's data.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// How long opening the connection may take before it fails.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connection.connect_timeout = Some(timeout);
        self
    }

    /// How long a command may wait for its reply before it fails.
    ///
    /// **The one to set on a cache.** Without it a server that accepted the
    /// command and went quiet stalls every request that touches a session, a
    /// cached value or a rate limit, all at once — and the symptom is "the whole
    /// site is slow", which names nothing.
    pub fn response_timeout(mut self, timeout: Duration) -> Self {
        self.connection.response_timeout = Some(timeout);
        self
    }

    /// Re-open the connection when its socket is lost.
    ///
    /// **Off unless asked for, and the most important setting here.** A
    /// multiplexed connection does not re-establish itself, so a socket dropped
    /// by a proxy that had seen no traffic for a few minutes breaks this store
    /// for the life of the process — every read, every write, every lock and
    /// every rate limit, until it is restarted.
    pub fn reconnect(mut self) -> Self {
        self.connection.reconnect = true;
        self
    }

    /// Whether this store's connection re-opens itself after losing its socket.
    ///
    /// Exposed so a deployment can assert what it believes it configured. A
    /// store that does not reconnect works until its first dropped socket and
    /// then fails every command for the life of the process.
    pub fn reconnects(&self) -> bool {
        self.connection.reconnects()
    }

    /// How many times to retry re-establishing the connection. Turns
    /// reconnection on.
    pub fn reconnect_attempts(mut self, attempts: u32) -> Self {
        self.connection.reconnect = true;
        self.connection.reconnect_attempts = Some(attempts);
        self
    }

    /// A ceiling on the wait between reconnection attempts. Turns reconnection
    /// on.
    pub fn reconnect_max_backoff(mut self, ceiling: Duration) -> Self {
        self.connection.reconnect = true;
        self.connection.reconnect_max_backoff = Some(ceiling);
        self
    }

    /// The key prefix, if one was declared.
    pub fn prefix_name(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// The connection settings this store was declared with.
    pub fn connection_settings(&self) -> &ConnectionSettings {
        &self.connection
    }

    /// The server this connects to, with any credentials removed.
    ///
    /// The only reading of the URL there is, because a Redis URL routinely
    /// carries a password in its userinfo — `redis://default:hunter2@host:6379`
    /// — and the driver underneath deliberately never echoes one either.
    pub fn url_without_credentials(&self) -> String {
        without_credentials(&self.url)
    }

    /// Connect, and build the store as its concrete driver.
    ///
    /// # Errors
    ///
    /// When a setting cannot be honoured, when the URL cannot be parsed, or when
    /// the server cannot be reached.
    #[cfg(feature = "redis-driver")]
    pub async fn build(&self) -> Result<crate::redis::RedisCache> {
        use rainier_drivers::RedisConnector;

        self.connection.validate()?;

        // Per store and never shared. Sharing one connector is the bug this
        // module exists to make impossible: a second store inheriting the
        // first's server keeps its own *name* and reads somebody else's keys.
        let connector = RedisConnector::open_with(&self.url, self.connection.driver_settings())?;
        crate::redis::RedisCache::connect(&connector).await
    }
}

/// Names the server and never the password.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the URL's userinfo into whatever logged the store, which for a
/// configuration dump at boot means the password is in the log of every process
/// that started.
impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore")
            .field("url", &self.url_without_credentials())
            .field("prefix", &self.prefix)
            .field("connection", &self.connection)
            .finish()
    }
}

/// Values on a sharded Redis Cluster.
///
/// Every URL is a **seed**: the client asks one of them for the cluster's shape
/// and routes each key to the node that owns its slot. Give it more than one, or
/// a single dead seed makes the whole cluster unreachable.
#[derive(Clone)]
pub struct RedisClusterStore {
    seeds: Vec<String>,
    prefix: Option<String>,
    connection: ConnectionSettings,
}

impl RedisClusterStore {
    /// A store across `seeds`.
    pub fn new(seeds: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            seeds: seeds.into_iter().map(Into::into).collect(),
            prefix: None,
            connection: ConnectionSettings::default(),
        }
    }

    /// Namespace every key.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Re-open connections when their sockets are lost.
    ///
    /// **The most important setting here, and more so than on a single node.**
    /// A cluster client holds a connection per shard, so it has several
    /// sockets to lose and several ways to end up half-working: the shards
    /// whose sockets survived keep answering, which is exactly what makes the
    /// failure hard to see. A single-key health check reads one shard and
    /// reports the cache healthy while the others hang.
    pub fn reconnect(mut self) -> Self {
        self.connection.reconnect = true;
        self
    }

    /// How long opening a connection may take before it fails.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connection.connect_timeout = Some(timeout);
        self
    }

    /// How long a command may wait for its reply before it fails.
    pub fn response_timeout(mut self, timeout: Duration) -> Self {
        self.connection.response_timeout = Some(timeout);
        self
    }

    /// Tighten the retry policy the cluster client already has.
    ///
    /// Unlike a single server, a cluster connection re-establishes itself
    /// without being asked. What it does by default is retry sixteen times with
    /// a wait that grows to roughly **eleven minutes**, which is a long time for
    /// a command on a hot path to be neither answered nor failed — so here this
    /// tightens a policy rather than turning one on.
    pub fn reconnect_attempts(mut self, attempts: u32) -> Self {
        self.connection.reconnect = true;
        self.connection.reconnect_attempts = Some(attempts);
        self
    }

    /// A ceiling on the wait between attempts.
    pub fn reconnect_max_backoff(mut self, ceiling: Duration) -> Self {
        self.connection.reconnect = true;
        self.connection.reconnect_max_backoff = Some(ceiling);
        self
    }

    /// Whether this store's connection re-opens itself after losing its socket.
    ///
    /// Exposed so a deployment can assert what it believes it configured. A
    /// store that does not reconnect works until its first dropped socket and
    /// then fails every command for the life of the process.
    pub fn reconnects(&self) -> bool {
        self.connection.reconnects()
    }

    /// How many seeds were declared.
    pub fn seed_count(&self) -> usize {
        self.seeds.len()
    }

    /// The key prefix, if one was declared.
    pub fn prefix_name(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// The connection settings this store was declared with.
    pub fn connection_settings(&self) -> &ConnectionSettings {
        &self.connection
    }

    /// Whether this declaration can be built.
    fn validate(&self) -> Result<()> {
        if self.seeds.is_empty() {
            return Err(Error::internal(
                "a `redis-cluster` store needs at least one seed node to ask for the cluster's \
                 shape; declare them as `seeds`",
            ));
        }
        self.connection.validate()
    }

    /// Connect, and build the store as its concrete driver.
    ///
    /// # Errors
    ///
    /// When there are no seeds, when a setting cannot be honoured, or when the
    /// cluster cannot be reached.
    #[cfg(feature = "redis-cluster")]
    pub async fn build(&self) -> Result<crate::redis::RedisCache> {
        use rainier_drivers::RedisConnector;

        self.validate()?;

        let connector = RedisConnector::open_cluster_with(
            self.seeds.clone(),
            self.connection.driver_settings(),
        )?;
        crate::redis::RedisCache::connect(&connector).await
    }
}

/// Names the seeds' hosts and never their passwords.
impl std::fmt::Debug for RedisClusterStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seeds: Vec<String> = self.seeds.iter().map(|seed| without_credentials(seed)).collect();

        f.debug_struct("RedisClusterStore")
            .field("seeds", &seeds)
            .field("prefix", &self.prefix)
            .field("connection", &self.connection)
            .finish()
    }
}

/// What a Redis connection waits for, and what it does when its socket goes.
///
/// Mirrors `rainier_drivers::RedisSettings` rather than holding one, so a store
/// can be **declared** in a build without the `redis-driver` feature and refused
/// at build time with a message about the feature rather than failing to
/// compile. The conversion happens where the store is built, which is the only
/// place the driver's type exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectionSettings {
    connect_timeout: Option<Duration>,
    response_timeout: Option<Duration>,
    reconnect: bool,
    reconnect_attempts: Option<u32>,
    reconnect_max_backoff: Option<Duration>,
}

impl ConnectionSettings {
    /// The connect timeout, if one was declared.
    pub fn connect_timeout(&self) -> Option<Duration> {
        self.connect_timeout
    }

    /// The response timeout, if one was declared.
    pub fn response_timeout(&self) -> Option<Duration> {
        self.response_timeout
    }

    /// Whether this connection re-opens itself after losing its socket.
    pub fn reconnects(&self) -> bool {
        self.reconnect
    }

    /// The retry limit, if one was declared.
    pub fn reconnect_attempt_limit(&self) -> Option<u32> {
        self.reconnect_attempts
    }

    /// The ceiling on the wait between attempts, if one was declared.
    pub fn reconnect_backoff_ceiling(&self) -> Option<Duration> {
        self.reconnect_max_backoff
    }

    /// Whether nothing at all is declared.
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// Whether these settings can be honoured.
    ///
    /// The zero-timeout checks are the driver's, delegated rather than copied,
    /// so its account of why is the only one there is. Only reachable with the
    /// feature on; without it there is no store to build and the declaration is
    /// refused for that instead.
    fn validate(&self) -> Result<()> {
        if !self.reconnect
            && (self.reconnect_attempts.is_some() || self.reconnect_max_backoff.is_some())
        {
            return Err(Error::internal(
                "this store shapes a reconnection it never asked for: `reconnect_attempts` and \
                 `reconnect_max_backoff` do nothing without `reconnect`, and a store that does \
                 not reconnect stops working permanently the first time its socket is dropped. \
                 Add `reconnect`, or remove the settings that imply it",
            ));
        }

        #[cfg(feature = "redis-driver")]
        self.driver_settings().validate()?;

        Ok(())
    }

    /// These settings as the driver's own.
    #[cfg(feature = "redis-driver")]
    fn driver_settings(&self) -> rainier_drivers::RedisSettings {
        use rainier_drivers::{Reconnect, RedisSettings};

        let mut settings = RedisSettings::new();
        if let Some(timeout) = self.connect_timeout {
            settings = settings.connect_timeout(timeout);
        }
        if let Some(timeout) = self.response_timeout {
            settings = settings.response_timeout(timeout);
        }
        if self.reconnect {
            let mut reconnect = Reconnect::new();
            if let Some(attempts) = self.reconnect_attempts {
                reconnect = reconnect.attempts(attempts);
            }
            if let Some(ceiling) = self.reconnect_max_backoff {
                reconnect = reconnect.max_backoff(ceiling);
            }
            settings = settings.reconnect(reconnect);
        }
        settings
    }
}

/// Values on a Memcached server.
///
/// **This one does pool**, unlike Redis, and the reason is in the protocol: the
/// text protocol has no request ids, so replies are matched to requests by order
/// and one connection serves one command at a time. Concurrency therefore needs
/// more connections, which is exactly what is not true next door.
#[derive(Clone, Debug)]
pub struct MemcachedStore {
    url: String,
    prefix: Option<String>,
    pool_size: Option<usize>,
}

impl MemcachedStore {
    /// A store on the server at `url` — `host:port`, or `tcp://host:port`.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), prefix: None, pool_size: None }
    }

    /// Namespace every key.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// How many connections to keep for reuse.
    ///
    /// A **cap on reuse, not on concurrency**: past the limit, connections are
    /// still opened and simply dropped rather than returned. That keeps a burst
    /// from queueing behind a lock, at the cost of some churn — the right trade
    /// for a cache, and the reason there is no acquire timeout to set.
    pub fn pool_size(mut self, size: usize) -> Self {
        self.pool_size = Some(size);
        self
    }

    /// The server address.
    pub fn address(&self) -> &str {
        &self.url
    }

    /// The key prefix, if one was declared.
    pub fn prefix_name(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// The pool size, if one was declared.
    pub fn pool_limit(&self) -> Option<usize> {
        self.pool_size
    }

    /// Build the store, as its concrete driver.
    #[cfg(feature = "memcached")]
    pub fn build(&self) -> crate::memcached::MemcachedCache {
        use rainier_drivers::MemcachedConnector;

        let connector = match self.pool_size {
            Some(size) => MemcachedConnector::with_pool_size(&self.url, size),
            None => MemcachedConnector::open(&self.url),
        };
        crate::memcached::MemcachedCache::new(connector)
    }
}

/// Values in a DynamoDB table.
///
/// No server to run, and no sockets to pool: every operation is a signed HTTP
/// request, and the SDK's own client handles connection reuse.
#[derive(Clone)]
pub struct DynamoDbStore {
    table: String,
    prefix: Option<String>,
    region: Option<String>,
    endpoint: Option<String>,
    credentials: StoreCredentials,
}

impl DynamoDbStore {
    /// A store in `table`, authenticating with the ambient credential chain.
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            prefix: None,
            region: None,
            endpoint: None,
            credentials: StoreCredentials::Chain,
        }
    }

    /// Namespace every key.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// The region to sign for.
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Talk to something other than AWS — a local DynamoDB, for tests.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Authenticate with an explicit key pair rather than the ambient chain.
    ///
    /// A [`region`](Self::region) becomes required — see [`StoreCredentials`].
    pub fn credentials(
        mut self,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        self.credentials = StoreCredentials::Static {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
        };
        self
    }

    /// The table name.
    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// The key prefix, if one was declared.
    pub fn prefix_name(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// The region, if one was declared.
    pub fn region_name(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// How this store authenticates.
    pub fn credential_source(&self) -> &StoreCredentials {
        &self.credentials
    }

    /// Whether this declaration can be built.
    fn validate(&self) -> Result<()> {
        if matches!(self.credentials, StoreCredentials::Static { .. }) && self.region.is_none() {
            return Err(Error::internal(format!(
                "the table `{}` is declared with `key` and `secret` but no `region`; a signed \
                 request has to name one",
                self.table
            )));
        }
        Ok(())
    }

    /// Build the store, as its concrete driver.
    ///
    /// # Errors
    ///
    /// When a key pair was declared without a region to sign for.
    #[cfg(feature = "dynamodb")]
    pub async fn build(&self) -> Result<crate::dynamodb::DynamoDbCache> {
        use rainier_drivers::AwsConnector;

        self.validate()?;

        let mut connector = match &self.credentials {
            StoreCredentials::Chain => match &self.region {
                Some(region) => AwsConnector::in_region(region.clone()).await,
                None => AwsConnector::from_env().await,
            },
            StoreCredentials::Static { access_key_id, secret_access_key } => {
                let region = self.region.clone().expect("validate rejects a pair without one");
                AwsConnector::with_credentials(
                    access_key_id.clone(),
                    secret_access_key.clone(),
                    region,
                )
                .await
            }
        };

        if let Some(endpoint) = &self.endpoint {
            connector = connector.endpoint(endpoint);
        }

        Ok(crate::dynamodb::DynamoDbCache::new(&connector, &self.table))
    }
}

impl std::fmt::Debug for DynamoDbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamoDbStore")
            .field("table", &self.table)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            // The key pair is deliberately absent, not redacted in place: see
            // `StoreCredentials`, whose own `Debug` names the source and
            // nothing else.
            .field("credentials", &self.credentials)
            .finish()
    }
}

/// How a [`DynamoDbStore`] proves who it is.
///
/// [`Chain`](Self::Chain) is the default, and is the safe one to be wrong about:
/// a store that should have named a key pair fails to authenticate, which is
/// loud. The reverse authenticates successfully against somebody else's table.
#[derive(Clone, Default)]
pub enum StoreCredentials {
    /// Whatever the environment provides, discovered and refreshed by the SDK.
    #[default]
    Chain,

    /// An explicit key pair, for a service with no chain to discover.
    Static {
        /// The access key id.
        access_key_id: String,
        /// The secret access key.
        secret_access_key: String,
    },
}

/// Names the source and never the values.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the key pair into whatever logged the store, which for a
/// configuration dump at boot means the secret is in the log of every process
/// that started.
impl std::fmt::Debug for StoreCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Chain => f.write_str("Chain"),
            Self::Static { .. } => f.write_str("Static(<redacted>)"),
        }
    }
}

/// Values in Cloudflare Workers KV.
///
/// **Read-heavy and eventually consistent**, with no compare-and-set at all, so
/// it cannot hold a lock or count anything that has to be exact. Read
/// [`kv`](crate::kv)'s docs before declaring one.
///
/// Its transport is not declared here — it is a binding inside a Worker and an
/// API client outside one, and arrives through [`CacheResources`].
#[derive(Clone, Debug, Default)]
pub struct KvStore {
    prefix: Option<String>,
}

impl KvStore {
    /// A store over the transport in [`CacheResources`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Namespace every key.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }
}

// --- the wire form -----------------------------------------------------------

/// A store as it is written down, before it is known to make sense.
///
/// The flat shape a configuration file wants, which [`StoreConfig`] is the
/// checked form of. Everything but `driver` is optional here so the *driver*
/// gets to say which settings apply, and so a misfiled one can be named in the
/// error rather than silently dropped.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStore {
    /// Required: an assumed driver is a store pointed at whichever backend the
    /// default happens to be.
    driver: CacheDriver,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seeds: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,

    /// Milliseconds, and named so: a command's budget on the hot path cannot be
    /// written in whole seconds, where the only values available are `0`, which
    /// would fail everything, and `1`, which is already longer than a request
    /// can afford to wait for a cache read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connect_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reconnect: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reconnect_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reconnect_max_backoff_ms: Option<u64>,

    /// Honoured on `memcached`, which has a real pool, and refused everywhere
    /// else — see [`reject_a_pool`](RawStore::reject_a_pool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pool_size: Option<usize>,

    /// The rest of a pool's settings, declared **only so they can be refused
    /// with the reason** rather than with `unknown field`, which reads as a
    /// misspelling and sends the reader looking for the right spelling of a
    /// feature that is not there.
    ///
    /// `Value` rather than a number or a bool, so `max_connections: "ten"` is
    /// refused with the message about pooling instead of one about types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_connections: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_connections: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acquire_timeout: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_timeout: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_lifetime: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test_before_acquire: Option<serde_json::Value>,
}

impl RawStore {
    /// Refuse pool settings no store here has.
    ///
    /// Three different reasons, so three different messages. The Redis one is
    /// the one worth reading: its connection multiplexes, so a pool would add
    /// sockets without adding throughput, and the failures a pool guards against
    /// are addressed by the timeouts beside it instead.
    fn reject_a_pool(&self) -> Result<()> {
        let declared = [
            ("max_connections", self.max_connections.is_some()),
            ("min_connections", self.min_connections.is_some()),
            ("acquire_timeout", self.acquire_timeout.is_some()),
            ("idle_timeout", self.idle_timeout.is_some()),
            ("max_lifetime", self.max_lifetime.is_some()),
            ("test_before_acquire", self.test_before_acquire.is_some()),
        ];

        let mut named: Vec<String> = declared
            .iter()
            .filter(|(_, present)| *present)
            .map(|(name, _)| format!("`{name}`"))
            .collect();

        // `pool_size` is the one Memcached genuinely has, so it joins the list
        // only on the drivers that do not.
        if self.pool_size.is_some() && self.driver != CacheDriver::Memcached {
            named.push("`pool_size`".to_string());
        }

        if named.is_empty() {
            return Ok(());
        }
        let named = named.join(", ");

        Err(Error::internal(match self.driver {
            CacheDriver::Redis | CacheDriver::RedisCluster => format!(
                "a `{}` store does not take {named}: the Redis connection **multiplexes** — one \
                 socket carries every concurrent command, and the client matches each reply to \
                 the request that asked for it — so a pool would open more sockets without \
                 moving more commands, and there is nothing to size. Accepted, this would leave \
                 the configuration stating in writing that connections are bounded when they \
                 are neither bounded nor plural. What addresses the same failures: \
                 `response_timeout_ms`, which bounds how long a command waits and is what a \
                 pool's acquire timeout would have caught, and `reconnect`, which recovers a \
                 dropped socket and is what recycling by age would have caught",
                self.driver
            ),

            CacheDriver::Memcached => format!(
                "a `memcached` store does not take {named}: its pool is a cap on **reuse**, not \
                 on concurrency — past `pool_size`, connections are still opened and simply \
                 dropped rather than returned — so nothing ever queues to acquire one and there \
                 is no acquire, idle or lifetime behaviour to configure. `pool_size` is the \
                 whole of it"
            ),

            CacheDriver::Memory => format!(
                "a `memory` store does not take {named}: it is a map in this process, with no \
                 connection to anything"
            ),

            CacheDriver::DynamoDb | CacheDriver::Kv => format!(
                "a `{}` store does not take {named}: every operation is an HTTP request, so \
                 there is no long-lived socket of ours to pool and the SDK underneath handles \
                 connection reuse itself",
                self.driver
            ),
        }))
    }

    /// Refuse a reconnection that was shaped but never asked for.
    fn reject_an_unasked_reconnection(&self) -> Result<()> {
        let shaped = self.reconnect_attempts.is_some() || self.reconnect_max_backoff_ms.is_some();

        if shaped && self.reconnect != Some(true) {
            return Err(Error::internal(
                "this store shapes a reconnection it never asked for: `reconnect_attempts` and \
                 `reconnect_max_backoff_ms` do nothing without `reconnect`, and a store that \
                 does not reconnect stops working permanently the first time its socket is \
                 dropped. Add `reconnect`, or remove the settings that imply it",
            ));
        }
        Ok(())
    }

    /// Refuse settings this driver would ignore.
    ///
    /// A `url` on a `memory` store is not a harmless extra key — it is somebody
    /// believing this cache is shared between processes when it is not, which is
    /// the difference between a rate limit and `N ×` a rate limit.
    fn reject_settings_it_ignores(&self, used: &[&str]) -> Result<()> {
        let declared: [(&str, bool); 14] = [
            ("prefix", self.prefix.is_some()),
            ("url", self.url.is_some()),
            ("seeds", self.seeds.is_some()),
            ("table", self.table.is_some()),
            ("region", self.region.is_some()),
            ("endpoint", self.endpoint.is_some()),
            ("key", self.key.is_some()),
            ("secret", self.secret.is_some()),
            ("connect_timeout_ms", self.connect_timeout_ms.is_some()),
            ("response_timeout_ms", self.response_timeout_ms.is_some()),
            ("reconnect", self.reconnect.is_some()),
            ("reconnect_attempts", self.reconnect_attempts.is_some()),
            ("reconnect_max_backoff_ms", self.reconnect_max_backoff_ms.is_some()),
            ("pool_size", self.pool_size.is_some()),
        ];

        let ignored: Vec<String> = declared
            .iter()
            .filter(|(name, present)| *present && !used.contains(name))
            .map(|(name, _)| format!("`{name}`"))
            .collect();

        if ignored.is_empty() {
            return Ok(());
        }

        Err(Error::internal(format!(
            "the `{}` driver does not use {}; that setting would be ignored, and a store that \
             ignores where it was told to keep things is one that keeps them somewhere else",
            self.driver,
            ignored.join(", ")
        )))
    }

    /// The connection settings, as the checked form.
    fn connection_settings(&self) -> ConnectionSettings {
        ConnectionSettings {
            connect_timeout: self.connect_timeout_ms.map(Duration::from_millis),
            response_timeout: self.response_timeout_ms.map(Duration::from_millis),
            reconnect: self.reconnect.unwrap_or(false),
            reconnect_attempts: self.reconnect_attempts,
            reconnect_max_backoff: self.reconnect_max_backoff_ms.map(Duration::from_millis),
        }
    }

    /// The credentials, or why half a pair cannot be one.
    fn credentials(&self) -> Result<StoreCredentials> {
        match (&self.key, &self.secret) {
            (None, None) => Ok(StoreCredentials::Chain),
            (Some(access_key_id), Some(secret_access_key)) => Ok(StoreCredentials::Static {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
            }),
            // Half a key pair is the dangerous case, so it is the one spelled
            // out: the missing half would fall back to the ambient chain, which
            // means signing as *this* account against a table of the same name
            // somewhere else — and reading it empty, which for a cache is
            // indistinguishable from a cold one.
            (Some(_), None) | (None, Some(_)) => Err(Error::internal(
                "this store declares one of `key` and `secret` but not the other; with only one \
                 it would authenticate from the ambient credential chain instead, against \
                 whatever table of that name the chain's account can reach",
            )),
        }
    }
}

impl TryFrom<RawStore> for StoreConfig {
    type Error = Error;

    fn try_from(raw: RawStore) -> Result<Self> {
        // Before the driver's own settings are read, because these hold
        // whichever one it is.
        raw.reject_a_pool()?;
        raw.reject_an_unasked_reconnection()?;

        match raw.driver {
            CacheDriver::Memory => {
                raw.reject_settings_it_ignores(&["prefix"])?;
                Ok(Self::Memory(MemoryStore { prefix: raw.prefix }))
            }

            CacheDriver::Redis => {
                raw.reject_settings_it_ignores(&[
                    "prefix",
                    "url",
                    "connect_timeout_ms",
                    "response_timeout_ms",
                    "reconnect",
                    "reconnect_attempts",
                    "reconnect_max_backoff_ms",
                ])?;

                let url = raw.url.clone().ok_or_else(|| {
                    Error::internal(
                        "a `redis` store needs a `url` to connect to — `redis://host:port/db`, \
                         and a different database index from the queue's",
                    )
                })?;

                let store = RedisStore {
                    url,
                    prefix: raw.prefix.clone(),
                    connection: raw.connection_settings(),
                };
                store.connection.validate()?;

                Ok(Self::Redis(store))
            }

            CacheDriver::RedisCluster => {
                raw.reject_settings_it_ignores(&[
                    "prefix",
                    "seeds",
                    "connect_timeout_ms",
                    "response_timeout_ms",
                    "reconnect",
                    "reconnect_attempts",
                    "reconnect_max_backoff_ms",
                ])?;

                let store = RedisClusterStore {
                    seeds: raw.seeds.clone().unwrap_or_default(),
                    prefix: raw.prefix.clone(),
                    connection: raw.connection_settings(),
                };
                store.validate()?;

                Ok(Self::RedisCluster(store))
            }

            CacheDriver::Memcached => {
                raw.reject_settings_it_ignores(&["prefix", "url", "pool_size"])?;

                let url = raw.url.clone().ok_or_else(|| {
                    Error::internal("a `memcached` store needs a `url` — `host:port`")
                })?;

                Ok(Self::Memcached(MemcachedStore {
                    url,
                    prefix: raw.prefix.clone(),
                    pool_size: raw.pool_size,
                }))
            }

            CacheDriver::DynamoDb => {
                raw.reject_settings_it_ignores(&[
                    "prefix", "table", "region", "endpoint", "key", "secret",
                ])?;

                let table = raw
                    .table
                    .clone()
                    .ok_or_else(|| Error::internal("a `dynamodb` store needs a `table`"))?;

                let store = DynamoDbStore {
                    table,
                    prefix: raw.prefix.clone(),
                    region: raw.region.clone(),
                    endpoint: raw.endpoint.clone(),
                    credentials: raw.credentials()?,
                };
                store.validate()?;

                Ok(Self::DynamoDb(store))
            }

            CacheDriver::Kv => {
                raw.reject_settings_it_ignores(&["prefix"])?;
                Ok(Self::Kv(KvStore { prefix: raw.prefix }))
            }
        }
    }
}

impl From<StoreConfig> for RawStore {
    fn from(store: StoreConfig) -> Self {
        // Every refused setting is `None` here and stays that way: they exist to
        // be rejected on the way in, and a store that was accepted never carries
        // one, so nothing can round-trip back out.
        let blank = |driver| Self {
            driver,
            prefix: None,
            url: None,
            seeds: None,
            table: None,
            region: None,
            endpoint: None,
            key: None,
            secret: None,
            connect_timeout_ms: None,
            response_timeout_ms: None,
            reconnect: None,
            reconnect_attempts: None,
            reconnect_max_backoff_ms: None,
            pool_size: None,
            max_connections: None,
            min_connections: None,
            acquire_timeout: None,
            idle_timeout: None,
            max_lifetime: None,
            test_before_acquire: None,
        };

        /// A connection's settings, written back into the flat form.
        ///
        /// Never `reconnect: false`, which is the default and would read as a
        /// decision somebody made rather than one they never took.
        fn connection(raw: &mut RawStore, settings: ConnectionSettings) {
            raw.connect_timeout_ms = settings.connect_timeout.map(millis);
            raw.response_timeout_ms = settings.response_timeout.map(millis);
            raw.reconnect = settings.reconnect.then_some(true);
            raw.reconnect_attempts = settings.reconnect_attempts;
            raw.reconnect_max_backoff_ms = settings.reconnect_max_backoff.map(millis);
        }

        match store {
            StoreConfig::Memory(store) => {
                Self { prefix: store.prefix, ..blank(CacheDriver::Memory) }
            }

            StoreConfig::Redis(store) => {
                let mut raw = Self {
                    url: Some(store.url),
                    prefix: store.prefix,
                    ..blank(CacheDriver::Redis)
                };
                connection(&mut raw, store.connection);
                raw
            }

            StoreConfig::RedisCluster(store) => {
                let mut raw = Self {
                    seeds: Some(store.seeds),
                    prefix: store.prefix,
                    ..blank(CacheDriver::RedisCluster)
                };
                connection(&mut raw, store.connection);
                raw
            }

            StoreConfig::Memcached(store) => Self {
                url: Some(store.url),
                prefix: store.prefix,
                pool_size: store.pool_size,
                ..blank(CacheDriver::Memcached)
            },

            StoreConfig::DynamoDb(store) => {
                let (key, secret) = match store.credentials {
                    StoreCredentials::Chain => (None, None),
                    StoreCredentials::Static { access_key_id, secret_access_key } => {
                        (Some(access_key_id), Some(secret_access_key))
                    }
                };
                Self {
                    table: Some(store.table),
                    prefix: store.prefix,
                    region: store.region,
                    endpoint: store.endpoint,
                    key,
                    secret,
                    ..blank(CacheDriver::DynamoDb)
                }
            }

            StoreConfig::Kv(store) => Self { prefix: store.prefix, ..blank(CacheDriver::Kv) },
        }
    }
}

/// A duration as whole milliseconds, for the wire form.
///
/// Saturating rather than wrapping: a period long enough to overflow a `u64` of
/// milliseconds is half a billion years, and wrapping it would turn "longer than
/// anyone will wait" into a handful of milliseconds — the wrong direction for a
/// timeout to be wrong in.
fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

/// A URL with its userinfo and query string removed.
///
/// Anything that does not parse as `scheme://…` is redacted **whole**. That is
/// the safe direction to be wrong in: a host nobody can read is an
/// inconvenience, and a password in a log is an incident — and a Redis DSN
/// carries its password inline.
fn without_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<redacted>".to_string();
    };

    // Split the authority off first, so an `@` or a `?` further along the path
    // cannot be mistaken for the end of a userinfo.
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };

    let host = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };

    // A password can also arrive as `?password=…`, so the query goes too.
    let path = match path.split_once('?') {
        Some((path, _query)) => path,
        None => path,
    };

    format!("{scheme}://{host}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- reading a declaration ---------------------------------------------

    #[test]
    fn a_section_deserialises_into_the_stores_it_declares() {
        let stores: Stores = serde_json::from_value(json!({
            "default": "shared",
            "stores": {
                "shared": { "driver": "redis", "url": "redis://127.0.0.1:6379/1" },
                "scratch": { "driver": "memory" },
                "counters": { "driver": "memcached", "url": "127.0.0.1:11211" },
            },
        }))
        .unwrap();

        assert_eq!(stores.default_name(), "shared");
        assert_eq!(stores.names().collect::<Vec<_>>(), vec!["counters", "scratch", "shared"]);
        assert_eq!(stores.get("shared").unwrap().driver(), CacheDriver::Redis);
        assert_eq!(stores.get("scratch").unwrap().driver(), CacheDriver::Memory);
        assert_eq!(stores.get("counters").unwrap().driver(), CacheDriver::Memcached);
    }

    #[test]
    fn a_store_without_a_driver_is_refused() {
        let err = serde_json::from_value::<Stores>(json!({
            "stores": { "shared": { "url": "redis://127.0.0.1:6379/1" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("driver"), "{err}");
    }

    #[test]
    fn a_misspelled_driver_lists_the_valid_ones() {
        let err = serde_json::from_value::<Stores>(json!({
            "stores": { "shared": { "driver": "redys", "url": "u" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`memory`"), "{err}");
        assert!(err.contains("`redis`"), "{err}");
    }

    #[test]
    fn a_setting_the_driver_ignores_is_refused_rather_than_dropped() {
        // Someone believes this cache is shared between processes. It is a map
        // in one of them, and a rate limit over it counts to `N ×` its limit.
        let err = serde_json::from_value::<Stores>(json!({
            "stores": { "scratch": { "driver": "memory", "url": "redis://127.0.0.1/" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`url`"), "{err}");
        assert!(err.contains("does not use"), "{err}");
    }

    #[test]
    fn an_unknown_setting_is_refused_rather_than_dropped() {
        let err = serde_json::from_value::<Stores>(json!({
            "stores": { "shared": { "driver": "redis", "url": "u", "urll": "typo" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("urll"), "{err}");
    }

    #[test]
    fn each_driver_needs_the_one_thing_it_cannot_be_built_without() {
        for (declaration, needed) in [
            (json!({ "driver": "redis" }), "`url`"),
            (json!({ "driver": "memcached" }), "`url`"),
            (json!({ "driver": "dynamodb" }), "`table`"),
            (json!({ "driver": "redis-cluster" }), "seed"),
        ] {
            let err = serde_json::from_value::<StoreConfig>(declaration).unwrap_err().to_string();
            assert!(err.contains(needed), "{err}");
        }
    }

    // --- pooling ------------------------------------------------------------

    #[test]
    fn a_pool_is_refused_on_redis_with_the_reason() {
        // The whole point. `deny_unknown_fields` would refuse it as a typo,
        // which sends the reader looking for the right spelling of a feature
        // that should not exist.
        for pooled in [
            "max_connections",
            "min_connections",
            "acquire_timeout",
            "idle_timeout",
            "max_lifetime",
            "test_before_acquire",
            "pool_size",
        ] {
            let err = serde_json::from_value::<StoreConfig>(json!({
                "driver": "redis",
                "url": "redis://127.0.0.1:6379/1",
                pooled: 10,
            }))
            .unwrap_err()
            .to_string();

            assert!(err.contains(&format!("`{pooled}`")), "{err}");
            assert!(err.contains("multiplexes"), "{err}");
            // And it names what to reach for instead, so the fix is one edit.
            assert!(err.contains("response_timeout_ms"), "{err}");
            assert!(err.contains("reconnect"), "{err}");
        }
    }

    #[test]
    fn memcached_does_have_a_pool_and_takes_its_size() {
        // The contrast that makes the Redis answer a design rather than a gap:
        // Memcached's protocol has no request ids, so concurrency really does
        // need more connections.
        let store: StoreConfig = serde_json::from_value(json!({
            "driver": "memcached",
            "url": "127.0.0.1:11211",
            "pool_size": 16,
        }))
        .unwrap();

        let StoreConfig::Memcached(memcached) = store else { panic!("declared as memcached") };
        assert_eq!(memcached.pool_limit(), Some(16));
    }

    #[test]
    fn memcached_still_refuses_the_parts_of_a_pool_it_does_not_have() {
        // Its pool caps reuse rather than concurrency, so nothing ever queues
        // to acquire one and an acquire timeout would be read and dropped.
        let err = serde_json::from_value::<StoreConfig>(json!({
            "driver": "memcached",
            "url": "127.0.0.1:11211",
            "acquire_timeout": 30,
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("cap on **reuse**"), "{err}");
    }

    #[test]
    fn a_pool_on_a_store_with_no_connection_at_all_says_so() {
        let memory = serde_json::from_value::<StoreConfig>(
            json!({ "driver": "memory", "max_connections": 10 }),
        )
        .unwrap_err()
        .to_string();
        assert!(memory.contains("no connection to anything"), "{memory}");

        let dynamo = serde_json::from_value::<StoreConfig>(
            json!({ "driver": "dynamodb", "table": "cache", "max_connections": 10 }),
        )
        .unwrap_err()
        .to_string();
        assert!(dynamo.contains("HTTP request"), "{dynamo}");
    }

    #[test]
    fn a_pool_is_refused_whatever_was_written_in_it() {
        // Typed as a number it would be a type error, which is a worse message
        // than the one about pooling.
        let err = serde_json::from_value::<StoreConfig>(json!({
            "driver": "redis",
            "url": "redis://127.0.0.1:6379/1",
            "max_connections": "ten",
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("multiplexes"), "{err}");
    }

    // --- the connection's own settings --------------------------------------

    #[test]
    fn a_redis_store_declares_its_timeouts_and_its_reconnection() {
        let store: StoreConfig = serde_json::from_value(json!({
            "driver": "redis",
            "url": "redis://127.0.0.1:6379/1",
            "connect_timeout_ms": 2000,
            "response_timeout_ms": 250,
            "reconnect": true,
            "reconnect_attempts": 4,
            "reconnect_max_backoff_ms": 1500,
        }))
        .unwrap();

        let StoreConfig::Redis(redis) = store else { panic!("declared as redis") };
        let settings = redis.connection_settings();

        assert_eq!(settings.connect_timeout(), Some(Duration::from_secs(2)));
        assert_eq!(settings.response_timeout(), Some(Duration::from_millis(250)));
        assert!(settings.reconnects());
        assert_eq!(settings.reconnect_attempt_limit(), Some(4));
        assert_eq!(settings.reconnect_backoff_ceiling(), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn a_store_that_declares_none_of_them_has_none_of_them() {
        // The half that matters most: a store declared before any of this
        // existed means exactly what it meant then, including one that does not
        // reconnect.
        let store: StoreConfig =
            serde_json::from_value(json!({ "driver": "redis", "url": "redis://127.0.0.1/" }))
                .unwrap();

        let StoreConfig::Redis(redis) = store else { panic!("declared as redis") };
        assert!(redis.connection_settings().is_unset());
        assert!(!redis.connection_settings().reconnects());
    }

    #[test]
    fn a_reconnection_shaped_but_never_asked_for_is_refused() {
        for shaped in [
            json!({ "driver": "redis", "url": "u", "reconnect_attempts": 4 }),
            json!({ "driver": "redis", "url": "u", "reconnect": false,
                    "reconnect_max_backoff_ms": 1500 }),
        ] {
            let err = serde_json::from_value::<StoreConfig>(shaped).unwrap_err().to_string();
            assert!(err.contains("never asked for"), "{err}");
        }
    }

    #[cfg(feature = "redis-driver")]
    #[test]
    fn a_zero_timeout_is_refused_at_declaration() {
        let err = serde_json::from_value::<StoreConfig>(json!({
            "driver": "redis",
            "url": "redis://127.0.0.1/",
            "response_timeout_ms": 0,
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`response_timeout` of zero"), "{err}");
    }

    #[test]
    fn a_timeout_on_a_driver_that_has_no_socket_is_refused() {
        let err = serde_json::from_value::<StoreConfig>(json!({
            "driver": "memcached",
            "url": "127.0.0.1:11211",
            "response_timeout_ms": 250,
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`response_timeout_ms`"), "{err}");
        assert!(err.contains("does not use"), "{err}");
    }

    // --- round trips --------------------------------------------------------

    #[test]
    fn a_declaration_round_trips_through_its_wire_form() {
        let original = json!({
            "driver": "redis",
            "url": "redis://127.0.0.1:6379/1",
            "prefix": "billing",
            "connect_timeout_ms": 2000,
            "response_timeout_ms": 250,
            "reconnect": true,
            "reconnect_attempts": 4,
            "reconnect_max_backoff_ms": 1500,
        });

        let store: StoreConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&store).unwrap(), original);
    }

    #[test]
    fn a_store_without_settings_round_trips_without_inventing_them() {
        // In particular it does not write `reconnect: false` back out, which
        // would read as a decision somebody made.
        let original = json!({ "driver": "redis", "url": "redis://127.0.0.1/" });

        let store: StoreConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&store).unwrap(), original);
    }

    #[test]
    fn a_dynamodb_declaration_round_trips() {
        let original = json!({
            "driver": "dynamodb",
            "table": "cache",
            "region": "us-east-1",
            "key": "id",
            "secret": "shh",
        });

        let store: StoreConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&store).unwrap(), original);
    }

    // --- credentials --------------------------------------------------------

    #[test]
    fn credentials_default_to_the_ambient_chain() {
        let store: StoreConfig =
            serde_json::from_value(json!({ "driver": "dynamodb", "table": "cache" })).unwrap();

        let StoreConfig::DynamoDb(dynamo) = store else { panic!("declared as dynamodb") };
        assert!(matches!(dynamo.credential_source(), StoreCredentials::Chain));
    }

    #[test]
    fn half_a_key_pair_is_refused_rather_than_falling_back_to_the_chain() {
        for half in [
            json!({ "driver": "dynamodb", "table": "c", "key": "id", "region": "us-east-1" }),
            json!({ "driver": "dynamodb", "table": "c", "secret": "shh", "region": "us-east-1" }),
        ] {
            let err = serde_json::from_value::<StoreConfig>(half).unwrap_err().to_string();
            assert!(err.contains("ambient credential chain"), "{err}");
        }
    }

    #[test]
    fn an_explicit_key_pair_needs_a_region_to_sign_for() {
        let err = serde_json::from_value::<StoreConfig>(
            json!({ "driver": "dynamodb", "table": "c", "key": "id", "secret": "shh" }),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("`region`"), "{err}");
    }

    #[test]
    fn no_debug_rendering_discloses_a_credential() {
        // The one that has to hold whatever else changes: a config dump at boot
        // must not put the secret in the log of every process that started.
        // Both of this section's secrets are here — the DynamoDB key pair, and
        // the password a Redis URL carries in its userinfo.
        let stores = Stores::new("shared")
            .with("shared", RedisStore::new("redis://default:hunter2@cache.internal:6379/1"))
            .with(
                "items",
                DynamoDbStore::new("cache")
                    .region("us-east-1")
                    .credentials("AKIA-visible", "super-secret"),
            )
            .with("sharded", RedisClusterStore::new(["redis://user:swordfish@10.0.0.1:6379"]));

        let rendered = format!("{stores:?}");

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("swordfish"), "{rendered}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("AKIA-visible"), "{rendered}");

        // Still enough to tell two stores apart in a log.
        assert!(rendered.contains("cache.internal:6379"), "{rendered}");
        assert!(rendered.contains("10.0.0.1:6379"), "{rendered}");
    }

    #[test]
    fn a_url_that_does_not_parse_is_redacted_whole() {
        // The safe direction to be wrong in.
        assert_eq!(without_credentials("localhost:6379,password=hunter2"), "<redacted>");
        assert_eq!(
            without_credentials("redis://cache.internal:6379/1?password=hunter2"),
            "redis://cache.internal:6379/1"
        );
    }

    // --- building -----------------------------------------------------------

    #[tokio::test]
    async fn a_default_naming_an_undeclared_store_fails_instead_of_falling_back() {
        let stores = Stores::new("shared").with("scratch", StoreConfig::memory());

        let err = stores.build(&CacheResources::new()).await.err().expect("not declared");
        assert!(err.message().contains("`shared`"), "{}", err.message());
        assert!(err.message().contains("`scratch`"), "{}", err.message());
    }

    #[tokio::test]
    async fn the_default_store_is_the_same_backend_as_its_name() {
        // Built once, registered twice. Building it twice would give
        // `store_named("scratch")` a different map from the default, and a write
        // through one would be invisible through the other.
        use crate::CacheExt as _;

        let cache = Stores::new("scratch")
            .with("scratch", StoreConfig::memory())
            .build(&CacheResources::new())
            .await
            .unwrap();

        cache.put_string("k", "v", None).await.unwrap();
        assert_eq!(
            cache.store_named("scratch").unwrap().get_string("k").await.unwrap().as_deref(),
            Some("v")
        );
    }

    #[tokio::test]
    async fn two_stores_declared_separately_do_not_share_anything() {
        use crate::CacheExt as _;

        let cache = Stores::new("first")
            .with("first", StoreConfig::memory())
            .with("second", StoreConfig::memory())
            .build(&CacheResources::new())
            .await
            .unwrap();

        cache.store_named("first").unwrap().put_string("k", "one", None).await.unwrap();

        assert!(!cache.store_named("second").unwrap().has("k").await.unwrap());
        assert!(cache.has_store("second"));
        assert!(!cache.has_store("third"));
    }

    #[tokio::test]
    async fn a_prefix_namespaces_the_store_that_declared_it() {
        // Two applications caching `user:1` on one server read each other's
        // values, and the symptom is a user seeing another application's data.
        use crate::CacheExt as _;

        let cache = Stores::new("mine")
            .with("mine", MemoryStore::new().prefix("billing"))
            .build(&CacheResources::new())
            .await
            .unwrap();

        cache.put_string("user:1", "v", None).await.unwrap();
        assert_eq!(cache.get_string("user:1").await.unwrap().as_deref(), Some("v"));
        assert_eq!(cache.driver(), "memory", "the prefix is not a driver");
    }

    #[tokio::test]
    async fn a_build_failure_names_the_store_it_came_from() {
        // With a dozen stores declared, "needs a region" without a name is a
        // search rather than a fix.
        let stores =
            Stores::new("items").with("items", DynamoDbStore::new("c").credentials("id", "shh"));

        let err = stores.build(&CacheResources::new()).await.err().expect("no region");
        assert!(err.message().starts_with("cache store `items`:"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_driver_whose_feature_is_off_says_so_rather_than_becoming_memory() {
        // The failure this crate cares most about: a shared store that silently
        // becomes a per-process one takes every lock and rate limit with it.
        let stores = Stores::new("shared").with("shared", RedisStore::new("redis://127.0.0.1:1/"));

        let outcome = stores.build(&CacheResources::new()).await;

        if cfg!(feature = "redis-driver") {
            // The feature is on, so it genuinely tries to connect — to nothing.
            let err = outcome.err().expect("nothing is listening on port 1");
            assert_eq!(err.status(), 503, "{}", err.message());
        } else {
            let err = outcome.err().expect("no redis driver to build with");
            assert!(
                err.message().contains("without the `redis-driver` feature"),
                "{}",
                err.message()
            );
        }
    }

    #[tokio::test]
    async fn a_kv_store_without_its_transport_names_what_is_missing() {
        let stores = Stores::new("edge").with("edge", KvStore::new());

        let err = stores.build(&CacheResources::new()).await.err().expect("no transport");

        if cfg!(feature = "cloudflare-kv") {
            assert!(err.message().contains("with_kv_transport"), "{}", err.message());
        } else {
            assert!(err.message().contains("`cloudflare-kv` feature"), "{}", err.message());
        }
    }
}
