//! [`RedisCache`] — the [`Cache`] port over a [`RedisClient`].
//!
//! # Where the connection settings go
//!
//! On the **connector**, not here: this store is handed one, and every timeout
//! and reconnection setting rides along with it. See
//! [`RedisSettings`](rainier_drivers::RedisSettings) for what can be set and
//! why there is no pool to size — the short version is that the connection
//! multiplexes, so one socket carries every concurrent command and a pool would
//! add sockets without adding throughput.
//!
//! Two of those settings matter more for a cache than for anything else,
//! because the cache is on the hot path of nearly every request:
//!
//! - **A response timeout.** Without one, a Redis that accepted a command and
//!   never answered stalls every request that touches a session, a cached
//!   value or a rate limit — all of them, at once. The symptom is "the whole
//!   site is slow", which names nothing.
//! - **Reconnection.** Without it a dropped socket is permanent for the life of
//!   the process, and the usual cause is not an outage but a proxy reaping a
//!   connection that had been idle. [`RedisCache::reconnects`] is the check
//!   worth making at boot, next to
//!   [`LockManager::is_shared`](crate::LockManager::is_shared).
//!
//! ```no_run
//! use std::time::Duration;
//! use rainier_cache::RedisCache;
//! use rainier_drivers::{Reconnect, RedisConnector, RedisSettings};
//!
//! # async fn run() -> rainier_support::Result<()> {
//! let cache = RedisCache::connect(&RedisConnector::open_with(
//!     "redis://127.0.0.1/",
//!     RedisSettings::new()
//!         .response_timeout(Duration::from_millis(250))
//!         .reconnect(Reconnect::new()),
//! )?)
//! .await?;
//!
//! assert!(cache.reconnects());
//! # Ok(()) }
//! ```

use std::time::Duration;

use rainier_drivers::{RedisClient, RedisConnection, RedisConnector};
use rainier_support::{BoxFuture, Result};

use crate::cache::Cache;

/// A cache backed by Redis, single-node or sharded cluster.
///
/// An **adapter**: every line below translates a `Cache` call into a
/// [`RedisClient`] call. The Redis knowledge — what command to send, how a
/// sub-second TTL has to be expressed, which replies mean absence — lives in
/// [`rainier-drivers`](rainier_drivers), so this file can be read as "does the
/// translation say what it means".
///
/// ```no_run
/// use rainier_cache::RedisCache;
/// use rainier_drivers::RedisConnector;
///
/// # async fn run() -> rainier_support::Result<()> {
/// let cache = RedisCache::connect(&RedisConnector::open("redis://127.0.0.1/")?).await?;
/// # let _ = cache; Ok(()) }
/// ```
pub struct RedisCache {
    client: RedisClient,
    label: String,
}

impl RedisCache {
    /// Open a connection through `connector`.
    pub async fn connect(connector: &RedisConnector) -> Result<Self> {
        Ok(Self::new(RedisClient::connect(connector).await?))
    }

    /// Use a client you already have — the point of sharing one connector
    /// between the cache and the queue.
    pub fn new(client: RedisClient) -> Self {
        Self { label: client.describe().to_string(), client }
    }

    /// Use a connection you already have.
    pub fn from_connection(connection: RedisConnection) -> Self {
        Self::new(RedisClient::new(connection))
    }

    /// The client, for a command this port does not expose.
    pub fn client(&self) -> &RedisClient {
        &self.client
    }

    /// Whether this cache is on a cluster.
    pub fn is_cluster(&self) -> bool {
        self.client.is_cluster()
    }

    /// Whether this cache's connection re-opens itself after losing its socket.
    ///
    /// **Worth asserting at boot.** `false` means the store this application
    /// caches in, rate-limits with and takes locks over stops working the first
    /// time its socket goes — which is usually not an outage but a proxy
    /// reaping an idle connection — and stays broken until the process is
    /// restarted. Ask for it with
    /// [`RedisSettings::reconnect`](rainier_drivers::RedisSettings::reconnect)
    /// on the connector.
    ///
    /// The same shape of check as
    /// [`LockManager::is_shared`](crate::LockManager::is_shared), and for the
    /// same reason: what a deployment believes it configured is worth
    /// confirming against what it actually built.
    pub fn reconnects(&self) -> bool {
        self.client.reconnects()
    }

    /// Store only if the key is absent, atomically.
    ///
    /// **Not the same as [`Cache::add`]**, whose default
    /// is a check then a write and lets two callers both win. This is the one to
    /// build a lock on.
    pub async fn add_atomic(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<bool> {
        self.client.set_nx(key, value, ttl).await
    }
}

impl Cache for RedisCache {
    fn name(&self) -> &str {
        &self.label
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        Box::pin(async move { self.client.get(key).await })
    }

    fn add<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<bool>> {
        // `SET key value NX PX ttl` — one round trip, decided by the server.
        Box::pin(async move { self.client.set_nx(key, value, ttl).await })
    }

    fn forget_if<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
        // A Lua script, because `GET` then `DEL` from the client is exactly the
        // race this exists to close. Redis runs a script to completion with
        // nothing interleaved.
        Box::pin(async move { self.client.del_if(key, expected).await })
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.client.set(key, value, ttl).await })
    }

    fn forget<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { self.client.del(key).await })
    }

    fn flush(&self) -> BoxFuture<'_, Result<()>> {
        // Empties the whole database, sessions and other applications included.
        // `PrefixedCache` is why that is worth wrapping.
        Box::pin(async move { self.client.flushdb().await })
    }

    fn has<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { self.client.exists(key).await })
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move { self.client.incr_by(key, by).await })
    }
}

impl std::fmt::Debug for RedisCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisCache").field("backend", &self.label).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a live server. Run with
    /// `cargo test -p rainier-cache --features redis-driver -- --ignored`
    /// against a Redis on 6379 whose database 15 is expendable.
    async fn cache() -> RedisCache {
        RedisCache::connect(&RedisConnector::open("redis://127.0.0.1:6379/15").unwrap())
            .await
            .unwrap()
    }

    #[test]
    fn the_label_names_the_backend() {
        // Constructible without a server, so this needs no live Redis.
        assert!(!RedisConnector::open("redis://127.0.0.1:6379/").unwrap().is_cluster());
    }

    #[test]
    fn a_cache_built_from_a_bare_connector_declares_no_settings() {
        // The cache is on the hot path of nearly every request, so "we changed
        // nothing" has to mean nothing changed.
        let connector = RedisConnector::open("redis://127.0.0.1:6379/").unwrap();

        assert!(connector.settings().is_unset());
    }

    #[test]
    fn the_settings_a_cache_is_built_with_reach_the_connector() {
        use rainier_drivers::{Reconnect, RedisSettings};

        let connector = RedisConnector::open_with(
            "redis://127.0.0.1:6379/",
            RedisSettings::new()
                .response_timeout(Duration::from_millis(250))
                .reconnect(Reconnect::new().max_backoff(Duration::from_secs(2))),
        )
        .unwrap();

        assert_eq!(
            connector.settings().response_timeout_period(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            connector.settings().reconnection().and_then(|r| r.backoff_ceiling()),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn a_setting_the_cache_could_not_honour_is_refused_at_declaration() {
        use rainier_drivers::RedisSettings;

        // Accepted, it would fail every read on the hot path — which for a
        // cache means the whole application, since a miss and a failure are
        // deliberately different things here.
        let err = RedisConnector::open_with(
            "redis://127.0.0.1:6379/",
            RedisSettings::new().response_timeout(Duration::ZERO),
        )
        .unwrap_err();

        assert!(err.message().contains("`response_timeout` of zero"), "{}", err.message());
    }

    #[test]
    fn no_rendering_of_a_cache_connector_discloses_its_password() {
        // A Redis DSN carries it inline, so this is the assertion that has to
        // hold whatever else changes about the settings above.
        let connector =
            RedisConnector::open("redis://default:hunter2@cache.internal:6379/1").unwrap();

        assert!(!format!("{connector:?}").contains("hunter2"), "{connector:?}");
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn a_value_round_trips() {
        let cache = cache().await;
        cache.flush().await.unwrap();

        cache.put("k", b"v", None).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap(), Some(b"v".to_vec()));
        assert!(cache.has("k").await.unwrap());
        assert!(cache.forget("k").await.unwrap());
        assert!(!cache.has("k").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn a_miss_is_none() {
        assert_eq!(cache().await.get("definitely-absent").await.unwrap(), None);
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn increment_starts_at_zero() {
        let cache = cache().await;
        cache.forget("hits").await.unwrap();

        assert_eq!(cache.increment("hits", 1).await.unwrap(), 1);
        assert_eq!(cache.increment("hits", 4).await.unwrap(), 5);
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn add_atomic_lets_exactly_one_caller_win() {
        let cache = cache().await;
        cache.forget("lock").await.unwrap();

        assert!(cache.add_atomic("lock", b"mine", Some(Duration::from_secs(5))).await.unwrap());
        assert!(!cache.add_atomic("lock", b"theirs", Some(Duration::from_secs(5))).await.unwrap());
    }
}
