//! Where session state lives — [`SessionDriver`].

use rainier_support::setting_enum;

setting_enum! {
    /// Which [`SessionStore`](crate::SessionStore) to build.
    ///
    /// ```
    /// use rainier_session::SessionDriver;
    /// use rainier_support::Setting;
    ///
    /// assert_eq!(SessionDriver::parse("cookie").unwrap(), SessionDriver::Cookie);
    /// assert!(SessionDriver::parse("redis").is_err(), "sessions go in the cache, not a driver of their own");
    /// ```
    ///
    /// Note what is *not* here: `redis`. Sessions in Redis are the `cache`
    /// driver pointed at Redis, which is one store to configure rather than
    /// two, and one connection pool rather than two.
    pub enum SessionDriver: "session driver" {
        /// This process only. Sessions vanish on restart and are not shared
        /// between instances, so a second instance logs everyone out.
        ///
        /// The default because it needs nothing, and wrong for anything with
        /// more than one process.
        #[default]
        Memory = "memory",

        /// A table in the application's own database.
        ///
        /// Shared, durable, and revocable. Needs the migration —
        /// `DatabaseSessionStore::migrations()` — and something to call
        /// `prune()` periodically.
        Database = "database",

        /// Whatever the cache is pointed at: Redis, a Redis Cluster, or
        /// Memcached, all behind one port.
        ///
        /// The cache expires them itself, so nothing has to sweep.
        Cache = "cache",

        /// The whole session, encrypted, in the cookie.
        ///
        /// No server state at all — and therefore **no way to revoke a
        /// session** short of rotating the key, and a hard size limit. Read
        /// the docs before choosing it.
        Cookie = "cookie",
    }
}

impl SessionDriver {
    /// Whether sessions survive a restart of the process that made them.
    pub fn is_durable(&self) -> bool {
        matches!(self, Self::Database | Self::Cookie)
    }

    /// Whether the server can end a session it has already issued.
    ///
    /// `false` for [`Cookie`](Self::Cookie), and that is the property that
    /// decides whether it is usable: a logout, a password change and a stolen
    /// cookie all need this.
    pub fn is_revocable(&self) -> bool {
        !matches!(self, Self::Cookie)
    }

    /// Whether every instance of the application sees the same sessions.
    pub fn is_shared(&self) -> bool {
        !matches!(self, Self::Memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn the_cookie_driver_is_the_one_that_cannot_be_revoked() {
        assert!(!SessionDriver::Cookie.is_revocable());
        for driver in SessionDriver::ALL.iter().filter(|d| **d != SessionDriver::Cookie) {
            assert!(driver.is_revocable(), "{driver} should be revocable");
        }
    }

    #[test]
    fn memory_is_the_only_unshared_driver() {
        assert!(!SessionDriver::Memory.is_shared());
        assert!(SessionDriver::Cache.is_shared());
        assert!(SessionDriver::Database.is_shared());
        assert!(SessionDriver::Cookie.is_shared());
    }

    #[test]
    fn cache_sessions_are_shared_but_not_durable() {
        // A cache is allowed to evict. That is the distinction the two
        // predicates exist to keep apart.
        assert!(SessionDriver::Cache.is_shared());
        assert!(!SessionDriver::Cache.is_durable());
    }
}
