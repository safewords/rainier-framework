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
}

impl Default for PoolConfig {
    /// Long-running-server defaults: up to 10 connections, drain when idle,
    /// recycle every half hour, and ping before handing one out.
    ///
    /// # Why the ping is on
    ///
    /// This was `false` until 2026-08-30, on the reasoning that a server —
    /// unlike a frozen serverless isolate — keeps its sockets alive, so the
    /// round-trip bought nothing. That holds while the *database* stays up,
    /// and databases do not.
    ///
    /// On 2026-08-31 a Galera rolling restart took lewd.net's MariaDB through
    /// its three replicas. The pool went on holding connections to a server
    /// that had closed them, handed one to the next request that asked, and
    /// the query came back `peer closed connection without sending TLS
    /// close_notify` — a `500` on a write somebody had just submitted. The
    /// module docs on `rainier-database` already describe this exact shape
    /// ("sockets that look open and fail on first use… intermittent errors
    /// nobody can reproduce") and name `max_lifetime` as the guard, but
    /// `max_lifetime` only retires a connection for being *old*. A connection
    /// the server closed a second ago is neither old nor alive, and only a
    /// pre-acquire ping can tell.
    ///
    /// The cost is one round-trip per acquire, which against a database in the
    /// same datacentre is sub-millisecond and is what `sqlx` itself does by
    /// default — this type was overriding that default to `false`, so every
    /// Rainier server was less resilient than the pool underneath it.
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
        // MariaDB rolling restart became `500`s on lewd.net writes, and why
        // this default changed. See `Default`'s own doc comment.
        //
        // `max_lifetime` is NOT the same guard and does not replace it: it
        // retires a connection for being old, and a connection closed a second
        // ago is young.
        let pool = PoolConfig::default();

        assert!(pool.test_before_acquire, "a server pool must not hand out a dead socket");
        assert!(pool.max_lifetime.is_some(), "ageing connections out is still worth doing");
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
