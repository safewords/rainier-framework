//! Connection-pool configuration — and an honest account of where pooling
//! does and doesn't apply.
//!
//! Pooling lives **inside** an executor, not in the [`Executor`](crate::Executor)
//! trait, because the backends differ fundamentally:
//!
//! - **`SeaOrmExecutor` (native MySQL/Postgres/SQLite)** — a
//!   `sea_orm::DatabaseConnection` *is* an `sqlx` pool. Cloning the executor
//!   shares that pool; `SeaOrmExecutor::connect` (in `rainier-drivers`)
//!   applies a [`PoolConfig`]. This is a long-lived-process model: a background
//!   task keeps idle connections warm and hands them out across concurrent
//!   requests.
//!
//! - **`D1Executor` (Cloudflare D1)** — there is **no pool**. D1 is
//!   request/response over a binding or the HTTP `/query` API; there is no
//!   socket to keep open between calls. `PoolConfig` is meaningless here, and
//!   that's not a gap to fill — it's the shape of the platform.
//!
//! ## Why serverless can't pool the usual way
//!
//! The classic pool assumes one long-running process owning a fixed set of
//! sockets. Serverless breaks both halves of that assumption:
//!
//! - **No shared process.** Each concurrent Lambda / Cloud Run / Worker
//!   instance is its own isolate. A pool is per-instance, so `max_connections`
//!   multiplies by the platform's concurrency: 100 warm instances × a pool of
//!   10 = 1000 sockets, which blows past a Postgres/MySQL `max_connections`
//!   ceiling almost immediately. The fix is a **small** per-instance pool
//!   (often exactly 1) plus a *server-side* pooler — RDS Proxy, PgBouncer, or
//!   Cloudflare Hyperdrive — that does the real fan-in. [`PoolConfig::serverless`]
//!   encodes the per-instance half.
//!
//! - **Frozen sockets.** Between invocations the platform may freeze the
//!   instance; a pooled socket can be dead on the next thaw. `serverless()`
//!   sets `test_before_acquire` so a stale connection is detected and replaced
//!   rather than handed to a query that then fails.
//!
//! - **No sockets at all (Workers).** A Cloudflare Worker has no `tokio` and no
//!   raw TCP, so `SeaOrmExecutor` cannot even compile to `wasm32` — it is
//!   feature-gated off. Inside a Worker the only database path is
//!   `D1Executor`, which pools nothing. Reaching a *Postgres* from a Worker
//!   means fronting it with Hyperdrive (which pools on Cloudflare's side) and a
//!   wasm-capable client — out of scope for this crate's native executor.
//!
//! So `PoolConfig` is a *server* concern. On serverless, prefer
//! [`PoolConfig::serverless`] and push real pooling to a proxy; on Workers,
//! pooling does not enter into it.

use core::time::Duration;

/// Tuning for the connection pool a native executor opens. The defaults suit a
/// long-running server; use [`PoolConfig::serverless`] for per-instance
/// serverless environments.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Hard ceiling on open connections. On serverless this should be tiny
    /// (the proxy does the real pooling); on a server, size it to the
    /// database's connection budget divided by the number of app processes.
    pub max_connections: u32,
    /// Connections kept open even while idle. `0` lets the pool drain fully
    /// between bursts.
    pub min_connections: u32,
    /// How long `acquire` waits for a free connection before erroring.
    pub acquire_timeout: Duration,
    /// Close a connection idle for longer than this. `None` keeps idle
    /// connections indefinitely.
    pub idle_timeout: Option<Duration>,
    /// Recycle a connection older than this regardless of use — guards against
    /// server-side timeouts and load-balancer connection caps. `None` disables.
    pub max_lifetime: Option<Duration>,
    /// Ping a connection before handing it out. Costs a round-trip and is what
    /// stops a socket the server has already closed reaching a query.
    pub test_before_acquire: bool,
    /// With [`test_before_acquire`](Self::test_before_acquire) on, ping only a
    /// connection that has sat idle in the pool at least this long. `None`
    /// pings on every acquire.
    ///
    /// A connection the pool handed out a moment ago was alive a moment ago;
    /// the ones a database restart or a proxy leaves dead are the ones that
    /// were sitting idle while it happened. Pinging only those keeps the
    /// guarantee that matters — no dead socket served after an outage — without
    /// a round trip in front of every query on a busy pool. The window it
    /// leaves (a connection used under this long ago whose server has since
    /// gone) is the one a reader retries: a read that fails because its
    /// connection was closed is run again once on a fresh one.
    pub test_after_idle: Option<Duration>,
}

impl Default for PoolConfig {
    /// Long-running-server defaults: up to 10 connections, drain when idle,
    /// recycle every half hour, and ping a connection that has been idle for a
    /// second or more before handing it out.
    ///
    /// # Why the ping is on
    ///
    /// A database that restarts — a rolling restart of a replicated cluster, a
    /// failover — closes every connection it holds. A pool that goes on holding
    /// those sockets hands one to the next query, and the query fails with a
    /// closed-connection error from a database that is healthy again.
    /// `max_lifetime` does not catch it: it retires a connection for being
    /// *old*, and a connection the server closed a second ago is neither old
    /// nor alive. Only a ping can tell.
    ///
    /// # Why only after a second idle
    ///
    /// A ping on every acquire is a round trip in front of every query, and a
    /// request that runs fifteen queries pays it fifteen times. The connections
    /// a restart leaves dead are the ones that were idle while it happened, so
    /// [`test_after_idle`](Self::test_after_idle) pings those and lets a
    /// connection used within the last second through. A read that still draws
    /// a closed connection is retried once on a fresh one by the executor.
    ///
    /// It does not make a restart invisible: a connection that dies *mid*
    /// query still fails that query, and a database that is unreachable is
    /// unreachable. What it removes is the failure that outlives the outage —
    /// the pool serving dead sockets after the server is healthy again.
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(10 * 60)),
            max_lifetime: Some(Duration::from_secs(30 * 60)),
            test_before_acquire: true,
            test_after_idle: Some(Duration::from_secs(1)),
        }
    }
}

impl PoolConfig {
    /// A single-connection pool tuned for a serverless instance: `max=1`,
    /// short idle timeout, and `test_before_acquire` so a frozen-then-thawed
    /// socket is caught. Pair this with a server-side pooler (RDS Proxy /
    /// PgBouncer / Hyperdrive) that fans many instances onto a bounded set of
    /// real database connections. See the [module docs](crate::pool) for why.
    pub fn serverless() -> Self {
        Self {
            max_connections: 1,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(10),
            idle_timeout: Some(Duration::from_secs(2)),
            max_lifetime: Some(Duration::from_secs(60)),
            test_before_acquire: true,
            // Every acquire: a thawed instance cannot trust any socket, however
            // recently the pool thinks it was used.
            test_after_idle: None,
        }
    }

    /// The only configuration an **in-memory SQLite** database survives.
    ///
    /// Such a database exists only as long as the connection holding it, so a
    /// pool must keep exactly one and must never reap it:
    ///
    /// - `max_connections: 1`, or the second connection is a second, empty
    ///   database;
    /// - `min_connections: 1` and **no idle timeout**, or the pool closes the
    ///   connection while nothing is happening and takes the schema with it.
    ///
    /// That second half is the one that bites. [`serverless`](Self::serverless)
    /// looks right — it is a pool of one — and works for the length of a test,
    /// then drops the tables two seconds after the last query. The symptom is
    /// a server that migrates cleanly at boot and answers `no such table` to
    /// the first request a human makes.
    pub fn in_memory() -> Self {
        Self {
            max_connections: 1,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(10),
            idle_timeout: None,
            max_lifetime: None,
            test_before_acquire: false,
            test_after_idle: None,
        }
    }

    /// Builder-style override of [`max_connections`](Self::max_connections).
    pub fn with_max_connections(mut self, n: u32) -> Self {
        self.max_connections = n;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_in_memory_pool_keeps_its_one_connection_forever() {
        // Every field here is load-bearing: the database *is* the connection.
        let pool = PoolConfig::in_memory();

        assert_eq!(pool.max_connections, 1, "a second connection is a second database");
        assert_eq!(pool.min_connections, 1, "dropping to zero drops the schema");
        assert_eq!(pool.idle_timeout, None, "reaping it while idle loses everything");
        assert_eq!(pool.max_lifetime, None, "so does recycling it");
    }

    #[test]
    fn the_server_default_pings_before_handing_a_connection_out() {
        // The guard against a database that restarted under a live pool. With
        // this off, the pool keeps sockets the server has already closed and
        // the failure lands on whichever query draws one — which is how a
        // rolling restart of a replicated database becomes `500`s on writes,
        // and why this default changed. See `Default`'s own doc comment.
        //
        // `max_lifetime` is NOT the same guard and does not replace it: it
        // retires a connection for being old, and a connection closed a second
        // ago is young.
        let pool = PoolConfig::default();

        assert!(pool.test_before_acquire, "a server pool must not hand out a dead socket");
        assert!(pool.max_lifetime.is_some(), "ageing connections out is still worth doing");
    }

    #[test]
    fn a_server_pool_pings_only_connections_that_sat_idle() {
        // A dead socket is one that was idle while its server went away; a
        // connection handed out a moment ago is not one. Pinging every acquire
        // put a round trip in front of every query.
        let pool = PoolConfig::default();
        assert_eq!(pool.test_after_idle, Some(Duration::from_secs(1)));
    }

    #[test]
    fn a_serverless_pool_pings_every_acquire() {
        // A thawed instance's pool thinks a socket was used a moment ago when it
        // was frozen for an hour.
        assert_eq!(PoolConfig::serverless().test_after_idle, None);
        assert!(PoolConfig::serverless().test_before_acquire);
    }

    #[test]
    fn an_in_memory_pool_does_not_pay_for_a_ping() {
        // Nothing can close this connection but us — the database is the
        // connection — so the round-trip would be pure cost.
        assert!(!PoolConfig::in_memory().test_before_acquire);
    }

    #[test]
    fn the_serverless_preset_is_a_pool_of_one_that_still_reaps() {
        // Which is right for a serverless database and wrong for an in-memory
        // one — the distinction `in_memory` exists to make.
        let pool = PoolConfig::serverless();

        assert_eq!(pool.max_connections, 1);
        assert!(pool.idle_timeout.is_some());
    }
}

/// What a pool holds right now, and — on MySQL and MariaDB — what the server
/// says about this application's connections. See
/// [`Executor::pool_usage`](crate::Executor::pool_usage).
///
/// The pool's half costs nothing: it is the pool's own bookkeeping, read
/// without a connection, so it is there even when every connection is
/// checked out — which is when it matters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolUsage {
    /// Connections the pool has open, checked out or idle.
    pub open: u32,
    /// Of those, how many are idle.
    pub idle: u32,
    /// How many it may open (`max_connections`).
    pub max: u32,
    /// What the server says, when it was asked and answered.
    pub server: Option<ServerConnections>,
    /// Why the server was not asked or did not answer: no idle connection
    /// to ask on (`busy`), or the error. `None` when it answered, and for a
    /// backend with nothing to ask.
    pub server_error: Option<String>,
}

/// One server's connections, as this application's user sees them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConnections {
    /// Connections open as this user, from every process that connects as it.
    pub user: u64,
    /// The user's cap (`MAX_USER_CONNECTIONS`), when it has one; `None` is
    /// none, which leaves only the server's.
    pub user_limit: Option<u64>,
    /// Connections open on the server, every user's (`Threads_connected`).
    pub total: u64,
    /// The server's cap (`max_connections`).
    pub max: u64,
}

/// The cap in a `SHOW GRANTS` line (`… WITH MAX_USER_CONNECTIONS 20`), or
/// `None` without one or at `0` (no cap).
pub fn max_user_connections_in(grant: &str) -> Option<u64> {
    let upper = grant.to_ascii_uppercase();
    let at = upper.find("MAX_USER_CONNECTIONS")?;
    let n: u64 = upper[at + "MAX_USER_CONNECTIONS".len()..]
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    (n > 0).then_some(n)
}

#[cfg(test)]
mod usage_tests {
    use super::max_user_connections_in;

    #[test]
    fn the_cap_is_read_from_a_grant() {
        let grant = "GRANT USAGE ON *.* TO `dungeon`@`%` IDENTIFIED BY PASSWORD '*AB' WITH MAX_USER_CONNECTIONS 20";
        assert_eq!(max_user_connections_in(grant), Some(20));
        assert_eq!(max_user_connections_in("GRANT USAGE ON *.* TO `x`@`%` WITH MAX_QUERIES_PER_HOUR 5 MAX_USER_CONNECTIONS 100"), Some(100));
        assert_eq!(max_user_connections_in("GRANT ALL PRIVILEGES ON `x`.* TO `x`@`%`"), None);
        assert_eq!(
            max_user_connections_in("GRANT USAGE ON *.* TO `x`@`%` WITH MAX_USER_CONNECTIONS 0"),
            None
        );
    }
}
