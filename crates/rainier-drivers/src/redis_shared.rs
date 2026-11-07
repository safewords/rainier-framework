//! A shared, self-healing Redis connection.
//!
//! [`RedisConnection`] is a multiplexed handle: clone it freely and every clone
//! shares one socket (or, on a cluster, one set of node connections). What it
//! does when that socket is gone for good depends on how it was opened, and a
//! cluster whose topology moved or a client that has spent its reconnect
//! attempts can leave a long-lived handle failing every command until the
//! process restarts.
//!
//! [`SharedRedis`] is the handle to keep in a `static` or the container
//! instead. It opens lazily, on first use; it drops its connection when a
//! command fails *because of the connection* — I/O, a refused or dropped
//! socket, a timeout, a cluster node it cannot reach — so the next use opens a
//! fresh one; and when opening fails it backs off rather than dialling on every
//! call. A command the server *refused* (a wrong type, a script error, a
//! cross-slot call) keeps the connection: it is healthy, the command was not.
//!
//! It is not a pool and adds no buffering: a call made while the server is
//! down fails, quickly, with [`ErrorKind::ServiceUnavailable`]. What the caller
//! does about that — retry, drop the work, degrade — is the caller's decision.
//!
//! [`ErrorKind::ServiceUnavailable`]: rainier_support::ErrorKind::ServiceUnavailable

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rainier_support::{Error, ErrorKind, Result};
use redis::{Cmd, FromRedisValue};

use crate::redis::redis_error;
use crate::{RedisConnection, RedisConnector, RedisScript};

/// How long a failed open is trusted before the next use tries again.
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(5);

/// A lazily opened Redis connection that replaces itself after a connection
/// failure. Cheap to clone; clones share the connection. See the module docs.
#[derive(Clone)]
pub struct SharedRedis {
    inner: Arc<Inner>,
}

struct Inner {
    connector: RedisConnector,
    backoff: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    connection: Option<RedisConnection>,
    failed_at: Option<Instant>,
}

impl SharedRedis {
    /// A shared connection to what `connector` names, opened on first use.
    pub fn new(connector: RedisConnector) -> Self {
        Self::with_backoff(connector, DEFAULT_BACKOFF)
    }

    /// As [`new`](Self::new), waiting `backoff` after a failed open before
    /// trying again.
    pub fn with_backoff(connector: RedisConnector, backoff: Duration) -> Self {
        Self { inner: Arc::new(Inner { connector, backoff, state: Mutex::new(State::default()) }) }
    }

    /// Whether this names a cluster.
    pub fn is_cluster(&self) -> bool {
        self.inner.connector.is_cluster()
    }

    /// Whether a connection is open now. `false` before first use and after a
    /// failure dropped it.
    pub fn is_connected(&self) -> bool {
        self.state().connection.is_some()
    }

    /// The connection, opening one if there is none.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::ServiceUnavailable`] when opening fails, and without trying
    /// when the last attempt failed less than the back-off ago.
    pub async fn connection(&self) -> Result<RedisConnection> {
        {
            let state = self.state();
            if let Some(connection) = &state.connection {
                return Ok(connection.clone());
            }
            if let Some(at) = state.failed_at {
                let waited = at.elapsed();
                if waited < self.inner.backoff {
                    return Err(Error::new(
                        ErrorKind::ServiceUnavailable,
                        format!(
                            "Redis: the last connection attempt failed; retrying in {}s",
                            (self.inner.backoff - waited).as_secs().max(1)
                        ),
                    ));
                }
            }
        }

        // Opened outside the lock: a slow connect must not block every other
        // caller's check. Two callers can race to open; the second's
        // connection replaces the first's, and both work.
        let opened = self.inner.connector.connect().await;
        let mut state = self.state();
        match opened {
            Ok(connection) => {
                state.connection = Some(connection.clone());
                state.failed_at = None;
                Ok(connection)
            }
            Err(error) => {
                state.failed_at = Some(Instant::now());
                Err(error)
            }
        }
    }

    /// Run a command and decode its reply. A connection failure drops the
    /// connection for the next call to replace.
    pub async fn query<T: FromRedisValue>(&self, command: &Cmd) -> Result<T> {
        let mut connection = self.connection().await?;
        let outcome = connection.query_raw(command).await;
        self.settle(outcome)
    }

    /// Run a command, ignoring its reply.
    pub async fn run(&self, command: &Cmd) -> Result<()> {
        self.query::<redis::Value>(command).await.map(|_| ())
    }

    /// Run `script` — see [`RedisConnection::eval`].
    ///
    /// # Errors
    ///
    /// When `keys` span more than one hash slot (before anything is sent, and
    /// without touching the connection), and otherwise as [`query`](Self::query).
    pub async fn eval<T, K, A>(&self, script: &RedisScript, keys: &[K], args: &[A]) -> Result<T>
    where
        T: FromRedisValue,
        K: AsRef<str>,
        A: redis::ToRedisArgs,
    {
        crate::check_one_slot(keys)?;
        let mut connection = self.connection().await?;
        let outcome = connection.eval_raw(script, keys, args).await?;
        self.settle(outcome)
    }

    /// Drop the connection, so the next call opens a new one. Not a back-off.
    pub fn forget(&self) {
        self.state().connection = None;
    }

    fn settle<T>(&self, outcome: redis::RedisResult<T>) -> Result<T> {
        outcome.map_err(|error| {
            if is_connection_failure(&error) {
                self.forget();
            }
            redis_error(error)
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding this lock leaves a connection or a timestamp,
        // either of which is still a sound state to carry on from.
        self.inner.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether `error` says the connection is the problem, rather than the
/// command.
fn is_connection_failure(error: &redis::RedisError) -> bool {
    error.is_io_error()
        || error.is_connection_refusal()
        || error.is_connection_dropped()
        || error.is_timeout()
        || error.is_unrecoverable_error()
        || matches!(error.kind(), redis::ErrorKind::ClusterConnectionNotFound)
}

impl std::fmt::Debug for SharedRedis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRedis")
            .field("connector", &self.inner.connector.description())
            .field("connected", &self.is_connected())
            .field("backoff", &self.inner.backoff)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing listens on port 1.
    fn unreachable() -> SharedRedis {
        let settings = crate::RedisSettings::new().connect_timeout(Duration::from_millis(250));
        SharedRedis::with_backoff(
            RedisConnector::open_with("redis://127.0.0.1:1/", settings).expect("a URL"),
            Duration::from_secs(60),
        )
    }

    #[tokio::test]
    async fn a_failed_open_backs_off_instead_of_dialling_again() {
        let redis = unreachable();
        let first = redis.connection().await.unwrap_err();
        assert_eq!(first.kind(), ErrorKind::ServiceUnavailable);
        assert!(!redis.is_connected());

        let second = redis.connection().await.unwrap_err();
        assert!(second.message().contains("retrying in"), "{}", second.message());
    }

    #[tokio::test]
    async fn a_cross_slot_script_is_refused_without_a_connection() {
        let redis = unreachable();
        let script = RedisScript::new("return 1");
        // Refused by the slot check before any connection is tried: the
        // unreachable server would say "could not be reached".
        let refused =
            redis.eval::<i64, _, &str>(&script, &["{a}:1", "{b}:1"], &[]).await.unwrap_err();
        assert!(refused.message().contains("hash slots"), "{}", refused.message());
        assert!(!redis.is_connected());
    }

    /// Needs a live Redis on 6379 whose database 15 is expendable:
    /// `cargo test -p rainier-drivers --features redis-driver -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn a_dropped_connection_is_replaced_on_the_next_call() {
        let redis = SharedRedis::new(RedisConnector::open("redis://127.0.0.1:6379/15").unwrap());
        let mut set = redis::cmd("SET");
        set.arg("shared:k").arg("v");
        redis.run(&set).await.unwrap();
        assert!(redis.is_connected());

        // Kill our own connection, by its id, so a test running alongside
        // keeps its own.
        let id: i64 = redis.query(&redis::cmd("CLIENT").arg("ID").clone()).await.unwrap();
        let mut kill = redis::cmd("CLIENT");
        kill.arg("KILL").arg("ID").arg(id).arg("SKIPME").arg("no");
        let _ = redis.run(&kill).await;

        // The first call after may fail on the dead socket; it must not take
        // the handle with it.
        let mut get = redis::cmd("GET");
        get.arg("shared:k");
        let mut answered = None;
        for _ in 0..3 {
            if let Ok(value) = redis.query::<Option<String>>(&get).await {
                answered = Some(value);
                break;
            }
        }
        assert_eq!(answered, Some(Some("v".to_string())));
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn a_script_runs_by_hash_and_a_refused_command_keeps_the_connection() {
        let redis = SharedRedis::new(RedisConnector::open("redis://127.0.0.1:6379/15").unwrap());
        let script = RedisScript::new("return redis.call('INCRBY', KEYS[1], ARGV[1])");
        let mut del = redis::cmd("DEL");
        del.arg("{s}:n");
        redis.run(&del).await.unwrap();

        // Twice: the first loads it, the second runs by hash.
        let _: i64 = redis.eval(&script, &["{s}:n"], &[2]).await.unwrap();
        let total: i64 = redis.eval(&script, &["{s}:n"], &[3]).await.unwrap();
        assert_eq!(total, 5);

        // A wrong-type error is the command's, not the connection's.
        let mut hget = redis::cmd("HGET");
        hget.arg("{s}:n").arg("f");
        assert!(redis.query::<Option<String>>(&hget).await.is_err());
        assert!(redis.is_connected());
    }
}
