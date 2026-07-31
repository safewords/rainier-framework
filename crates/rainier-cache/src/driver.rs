//! Which cache store to build — [`CacheDriver`].

use rainier_support::setting_enum;

setting_enum! {
    /// Where cached values live.
    ///
    /// The enum names every store this crate knows how to build, whether or not
    /// the cargo feature behind it is on. That is deliberate: `CACHE_DRIVER=redis`
    /// in a build without the `redis-driver` feature is a *build* mistake, and
    /// it deserves a message saying so — not the same "unknown driver" message a
    /// typo gets.
    ///
    /// ```
    /// use rainier_cache::CacheDriver;
    /// use rainier_support::Setting;
    ///
    /// assert_eq!(CacheDriver::parse("redis-cluster").unwrap(), CacheDriver::RedisCluster);
    /// assert!(CacheDriver::parse("redys").is_err());
    /// ```
    pub enum CacheDriver: "cache driver" {
        /// This process only. Fast, and shares nothing between instances.
        ///
        /// The default because it needs no server, and the right choice for
        /// tests and single-process development. Anything cached **for
        /// correctness** rather than for speed is wrong in it — a rate-limit
        /// counter becomes `N ×` its limit across `N` instances, and a lock is
        /// not a lock.
        #[default]
        Memory = "memory",

        /// One Redis server, or a Sentinel-fronted primary.
        Redis = "redis",

        /// A sharded Redis Cluster. Keys distribute across the seed nodes.
        RedisCluster = "redis-cluster",

        /// Memcached over its text protocol.
        Memcached = "memcached",

        /// DynamoDB, with TTL doing the expiry. No server to run.
        DynamoDb = "dynamodb",

        /// Cloudflare Workers KV — the edge key-value store.
        ///
        /// **Read-heavy and eventually consistent.** A write reaches other
        /// edges in roughly a minute, and there is no compare-and-set at all,
        /// so it cannot hold a lock or count anything that has to be exact.
        /// Right for a configuration blob or a feature-flag set; wrong for a
        /// session, a rate limit or `on_one_server`.
        ///
        /// See [`KvCache`](crate::kv::KvCache), which says the same thing at
        /// more length and is worth reading before choosing this.
        Kv = "kv",
    }
}

impl CacheDriver {
    /// Whether this driver is shared between instances of the application.
    ///
    /// The question a production checklist is really asking. A rate limiter, a
    /// lock, or a cached authorisation decision needs `true` here; a memoised
    /// computation does not.
    pub fn is_shared(&self) -> bool {
        !matches!(self, Self::Memory)
    }

    /// Whether this driver can hold a lock.
    ///
    /// Shared and lock-capable are **different questions**, and Workers KV is
    /// the reason: it is visible to every replica on earth and has no
    /// compare-and-set, so two callers both win the `add` a lock is built
    /// from and both believe they hold it.
    ///
    /// The check that matters is
    /// [`LockManager::is_shared`](crate::LockManager::is_shared), which asks
    /// the built store rather than the configured name; this answers the same
    /// question about a driver nobody has built yet, for a boot-time
    /// configuration check.
    pub fn can_hold_a_lock(&self) -> bool {
        self.is_shared() && !matches!(self, Self::Kv)
    }

    /// The cargo feature that has to be on for this driver to be built.
    ///
    /// `None` for the ones that are always available.
    pub fn feature(&self) -> Option<&'static str> {
        match self {
            Self::Memory => None,
            Self::Redis => Some("redis-driver"),
            Self::RedisCluster => Some("redis-cluster"),
            Self::Memcached => Some("memcached"),
            Self::DynamoDb => Some("dynamodb"),
            Self::Kv => Some("cloudflare-kv"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn only_memory_is_per_process() {
        assert!(!CacheDriver::Memory.is_shared());
        for driver in CacheDriver::ALL.iter().filter(|d| **d != CacheDriver::Memory) {
            assert!(driver.is_shared(), "{driver} should be shared");
        }
    }

    #[test]
    fn every_driver_but_memory_names_a_feature() {
        // The message "enable the `redis-driver` feature" is only possible if
        // the driver knows which feature that is.
        assert_eq!(CacheDriver::Memory.feature(), None);
        for driver in CacheDriver::ALL.iter().filter(|d| **d != CacheDriver::Memory) {
            assert!(driver.feature().is_some(), "{driver} should name its feature");
        }
    }

    #[test]
    fn the_wire_spellings_are_what_a_dotenv_would_hold() {
        assert_eq!(CacheDriver::RedisCluster.as_str(), "redis-cluster");
        assert_eq!(CacheDriver::DynamoDb.as_str(), "dynamodb");
    }

    #[test]
    fn shared_and_lock_capable_are_different_questions() {
        // The whole reason `can_hold_a_lock` exists. KV is visible to every
        // replica on earth and still cannot hold a lock.
        assert!(CacheDriver::Kv.is_shared());
        assert!(!CacheDriver::Kv.can_hold_a_lock());

        // And the ones that can.
        for driver in [CacheDriver::Redis, CacheDriver::RedisCluster, CacheDriver::Memcached] {
            assert!(driver.can_hold_a_lock(), "{driver}");
        }

        // Memory is neither, for the ordinary reason.
        assert!(!CacheDriver::Memory.can_hold_a_lock());
    }

    #[test]
    fn every_driver_names_the_feature_it_needs() {
        // Which is what lets the "not built with that" error say which line to
        // add to Cargo.toml.
        for driver in CacheDriver::ALL {
            if *driver != CacheDriver::Memory {
                assert!(driver.feature().is_some(), "{driver}");
            }
        }
    }
}
