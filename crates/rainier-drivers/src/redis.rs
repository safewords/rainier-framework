//! Redis transport — a connector that speaks to a single server or a sharded
//! cluster behind one type.

use std::time::Duration;

use rainier_support::{Error, Result};
use redis::aio::MultiplexedConnection;
use redis::{Cmd, FromRedisValue};

/// How the connector reaches Redis.
#[derive(Clone)]
enum Backend {
    Single(redis::Client),
    #[cfg(feature = "redis-cluster")]
    Cluster(redis::cluster::ClusterClient),
}

/// A live connection, single-node or cluster.
///
/// Both variants multiplex, so this is cheap to clone and does not need a pool:
/// concurrent commands share one socket per node and are matched up by the
/// client. A pool on top would add sockets without adding throughput.
#[derive(Clone)]
pub enum RedisConnection {
    /// A multiplexed connection to one server.
    Single(MultiplexedConnection),
    /// A connection to a cluster, routing each command to the owning shard.
    #[cfg(feature = "redis-cluster")]
    Cluster(redis::cluster_async::ClusterConnection),
}

impl RedisConnection {
    /// Run a command and decode its reply.
    ///
    /// Takes `&mut self` because the underlying connections do; clone the
    /// connection rather than sharing one behind a lock, which is what makes
    /// concurrent use possible at all.
    pub async fn query<T: FromRedisValue>(&mut self, command: &Cmd) -> Result<T> {
        let outcome = match self {
            RedisConnection::Single(connection) => command.query_async(connection).await,
            #[cfg(feature = "redis-cluster")]
            RedisConnection::Cluster(connection) => command.query_async(connection).await,
        };

        outcome.map_err(redis_error)
    }

    /// Run a command, ignoring its reply.
    pub async fn run(&mut self, command: &Cmd) -> Result<()> {
        self.query::<()>(command).await
    }

    /// Whether this connection is talking to a cluster.
    pub fn is_cluster(&self) -> bool {
        match self {
            RedisConnection::Single(_) => false,
            #[cfg(feature = "redis-cluster")]
            RedisConnection::Cluster(_) => true,
        }
    }
}

impl std::fmt::Debug for RedisConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.is_cluster() {
            "RedisConnection::Cluster"
        } else {
            "RedisConnection::Single"
        })
    }
}

/// Opens connections to Redis.
///
/// Shared by [`rainier-cache`] and [`rainier-queue`] rather than each building
/// its own, so an application configures Redis once and both use the same
/// client — one version of the protocol code, one set of connections, one place
/// where a URL is parsed.
///
/// ```no_run
/// use rainier_drivers::RedisConnector;
///
/// # async fn run() -> rainier_support::Result<()> {
/// let redis = RedisConnector::open("redis://127.0.0.1/")?;
/// # Ok(()) }
/// ```
///
/// A sharded cluster needs the `redis-cluster` feature — see
/// [`open_cluster`](RedisConnector::open_cluster), whose example cannot live
/// here because this one compiles without it.
///
/// [`rainier-cache`]: https://docs.rs/rainier-cache
/// [`rainier-queue`]: https://docs.rs/rainier-queue
#[derive(Clone)]
pub struct RedisConnector {
    backend: Backend,
    description: String,
}

impl RedisConnector {
    /// Open a connector to a single server.
    ///
    /// `redis://host:port/db`, or `rediss://` for TLS.
    pub fn open(url: &str) -> Result<Self> {
        let client = redis::Client::open(url).map_err(|e| {
            // The URL frequently contains a password, so it is not echoed.
            Error::internal(format!("could not open a Redis client: {e}"))
        })?;

        Ok(Self { backend: Backend::Single(client), description: "redis".to_string() })
    }

    /// Open a connector to a sharded cluster.
    ///
    /// Every URL is a **seed**: the client asks one of them for the cluster's
    /// shape and routes each key to the node that owns its slot. Give it more
    /// than one, or a single dead seed makes the whole cluster unreachable.
    ///
    /// ```no_run
    /// use rainier_drivers::RedisConnector;
    ///
    /// # fn run() -> rainier_support::Result<()> {
    /// let cluster = RedisConnector::open_cluster([
    ///     "redis://10.0.0.1:6379",
    ///     "redis://10.0.0.2:6379",
    ///     "redis://10.0.0.3:6379",
    /// ])?;
    /// # Ok(()) }
    /// ```
    #[cfg(feature = "redis-cluster")]
    pub fn open_cluster(urls: impl IntoIterator<Item = impl Into<String>>) -> Result<Self> {
        let seeds: Vec<String> = urls.into_iter().map(Into::into).collect();
        if seeds.is_empty() {
            return Err(Error::internal("a Redis cluster needs at least one seed node"));
        }
        let count = seeds.len();

        let client = redis::cluster::ClusterClient::new(seeds)
            .map_err(|e| Error::internal(format!("could not open a Redis cluster client: {e}")))?;

        Ok(Self {
            backend: Backend::Cluster(client),
            description: format!("redis-cluster({count} seeds)"),
        })
    }

    /// A label for diagnostics.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Whether this connector talks to a cluster.
    pub fn is_cluster(&self) -> bool {
        match self.backend {
            Backend::Single(_) => false,
            #[cfg(feature = "redis-cluster")]
            Backend::Cluster(_) => true,
        }
    }

    /// Open a connection.
    ///
    /// Call it once and clone the result for concurrent use — each call opens a
    /// socket, and a caller that connects per operation will exhaust the
    /// server's connection limit under load.
    pub async fn connect(&self) -> Result<RedisConnection> {
        match &self.backend {
            Backend::Single(client) => client
                .get_multiplexed_async_connection()
                .await
                .map(RedisConnection::Single)
                .map_err(redis_error),
            #[cfg(feature = "redis-cluster")]
            Backend::Cluster(client) => client
                .get_async_connection()
                .await
                .map(RedisConnection::Cluster)
                .map_err(redis_error),
        }
    }

    /// Whether the server answers.
    pub async fn ping(&self) -> Result<bool> {
        let mut connection = self.connect().await?;
        let reply: String = connection.query(&redis::cmd("PING")).await?;
        Ok(reply.eq_ignore_ascii_case("pong"))
    }
}

impl std::fmt::Debug for RedisConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConnector").field("backend", &self.description).finish()
    }
}

/// Turn a Redis error into a framework one.
///
/// Everything is `ServiceUnavailable` rather than `Internal`, because a cache
/// or queue being unreachable is a dependency outage: it is retryable, it is
/// somebody's to page about, and it is not a bug in the request that hit it.
fn redis_error(error: redis::RedisError) -> Error {
    Error::new(
        rainier_support::ErrorKind::ServiceUnavailable,
        format!("Redis: {}", error.category_message()),
    )
    .with_source(anyhow_from(error))
}

/// `redis::RedisError` is `std::error::Error`, so anyhow can carry it.
fn anyhow_from(error: redis::RedisError) -> anyhow::Error {
    anyhow::Error::new(error)
}

/// A short description of what went wrong, for the message.
trait Category {
    fn category_message(&self) -> String;
}

impl Category for redis::RedisError {
    fn category_message(&self) -> String {
        // The full error frequently includes the connection string. The kind
        // and the detail are what a log line needs; the address is not.
        match self.kind() {
            redis::ErrorKind::IoError => "the server could not be reached".to_string(),
            redis::ErrorKind::AuthenticationFailed => "authentication failed".to_string(),
            redis::ErrorKind::TypeError => "the reply was not the expected type".to_string(),
            redis::ErrorKind::ResponseError => {
                self.detail().unwrap_or("the server rejected the command").to_string()
            }
            _ => self.detail().unwrap_or("the command failed").to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_url_opens() {
        let connector = RedisConnector::open("redis://127.0.0.1:6379/").unwrap();

        assert!(!connector.is_cluster());
        assert_eq!(connector.description(), "redis");
    }

    #[test]
    fn a_rubbish_url_is_refused_without_echoing_it() {
        let err = RedisConnector::open("postgres://user:hunter2@host/db").unwrap_err();

        assert!(!err.message().contains("hunter2"), "{}", err.message());
    }

    #[test]
    #[cfg(feature = "redis-cluster")]
    fn a_cluster_needs_at_least_one_seed() {
        let err = RedisConnector::open_cluster(Vec::<String>::new()).unwrap_err();
        assert!(err.message().contains("at least one seed"), "{}", err.message());
    }

    #[test]
    #[cfg(feature = "redis-cluster")]
    fn a_cluster_reports_its_seed_count() {
        let connector =
            RedisConnector::open_cluster(["redis://10.0.0.1:6379", "redis://10.0.0.2:6379"])
                .unwrap();

        assert!(connector.is_cluster());
        assert!(connector.description().contains('2'), "{}", connector.description());
    }

    #[tokio::test]
    async fn connecting_to_nothing_is_a_503_not_a_500() {
        // A dependency being down is retryable and somebody's to page about;
        // it is not a bug in the request that happened to hit it.
        let connector = RedisConnector::open("redis://127.0.0.1:1/").unwrap();
        let err = connector.connect().await.unwrap_err();

        assert_eq!(err.status(), 503, "{}", err.message());
    }
}

/// Redis key/value operations.
///
/// Service-shaped, not cache-shaped: `set_px`, `set_nx`, `incr_by`, `flushdb`.
/// It has never heard of a cache, a session or a queue — every command here
/// touches exactly **one key**, which is also what makes it safe against a
/// sharded cluster, where a multi-key command needs its keys in the same slot.
#[derive(Clone)]
pub struct RedisClient {
    connection: RedisConnection,
}

impl RedisClient {
    /// Open a client through `connector`.
    ///
    /// Connects once. The connection multiplexes, so clone this rather than
    /// connecting per operation — a caller that connects per call exhausts the
    /// server's connection limit under any real load.
    pub async fn connect(connector: &RedisConnector) -> Result<Self> {
        Ok(Self { connection: connector.connect().await? })
    }

    /// Use a connection you already have — the point of sharing one connector
    /// between the cache and the queue.
    pub fn new(connection: RedisConnection) -> Self {
        Self { connection }
    }

    /// Whether this client is talking to a cluster.
    pub fn is_cluster(&self) -> bool {
        self.connection.is_cluster()
    }

    /// A label for diagnostics.
    pub fn describe(&self) -> &'static str {
        if self.is_cluster() {
            "redis-cluster"
        } else {
            "redis"
        }
    }

    /// The connection, for a command this does not wrap.
    pub fn connection(&self) -> RedisConnection {
        self.connection.clone()
    }

    /// `GET`.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.connection.clone().query(redis::cmd("GET").arg(key)).await
    }

    /// `SET`, with an optional expiry.
    ///
    /// Milliseconds rather than seconds: `EX 0` from a sub-second duration means
    /// *no expiry* to Redis, which is the opposite of what was asked for.
    pub async fn set(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<()> {
        let mut command = redis::cmd("SET");
        command.arg(key).arg(value);

        if let Some(ttl) = ttl {
            command.arg("PX").arg((ttl.as_millis() as u64).max(1));
        }

        self.connection.clone().run(&command).await
    }

    /// `SET … NX` — store only if the key is absent. `true` if it was stored.
    ///
    /// **Atomic**, which a check-then-write is not. This is the one to build a
    /// lock on.
    pub async fn set_nx(&self, key: &str, value: &[u8], ttl: Option<Duration>) -> Result<bool> {
        let mut command = redis::cmd("SET");
        command.arg(key).arg(value).arg("NX");

        if let Some(ttl) = ttl {
            command.arg("PX").arg((ttl.as_millis() as u64).max(1));
        }

        // `SET NX` replies with `OK` on success and nil on a collision, so an
        // absent reply means somebody else got there first.
        let reply: Option<String> = self.connection.clone().query(&command).await?;
        Ok(reply.is_some())
    }

    /// Delete `key`, but only if it currently holds `expected`.
    ///
    /// The compare and the delete have to happen with nothing in between, so
    /// this is a script rather than a `GET` and a `DEL`: Redis runs a script to
    /// completion without interleaving another client's commands.
    ///
    /// What it prevents is the release half of the classic lock bug. A holder
    /// that stalled past its TTL no longer owns the key — somebody else does —
    /// and an unconditional `DEL` would hand that third party's lock away.
    pub async fn del_if(&self, key: &str, expected: &[u8]) -> Result<bool> {
        const SCRIPT: &str = r#"
            if redis.call('GET', KEYS[1]) == ARGV[1] then
                return redis.call('DEL', KEYS[1])
            else
                return 0
            end
        "#;

        let mut command = redis::cmd("EVAL");
        command.arg(SCRIPT).arg(1).arg(key).arg(expected);

        let removed: i64 = self.connection.clone().query(&command).await?;
        Ok(removed == 1)
    }

    /// Extend `key`'s expiry, but only if it currently holds `expected`.
    ///
    /// For a lock holder that is still working and wants to keep it. Same
    /// reasoning as [`del_if`](Self::del_if): renewing a lock you no longer own
    /// is worse than losing it, because it takes it from whoever does.
    pub async fn pexpire_if(&self, key: &str, expected: &[u8], ttl: Duration) -> Result<bool> {
        const SCRIPT: &str = r#"
            if redis.call('GET', KEYS[1]) == ARGV[1] then
                return redis.call('PEXPIRE', KEYS[1], ARGV[2])
            else
                return 0
            end
        "#;

        let mut command = redis::cmd("EVAL");
        command.arg(SCRIPT).arg(1).arg(key).arg(expected).arg((ttl.as_millis() as u64).max(1));

        let extended: i64 = self.connection.clone().query(&command).await?;
        Ok(extended == 1)
    }

    /// `DEL`. `true` if the key was there.
    pub async fn del(&self, key: &str) -> Result<bool> {
        let removed: i64 = self.connection.clone().query(redis::cmd("DEL").arg(key)).await?;
        Ok(removed > 0)
    }

    /// `PUBLISH` — fan a message out to whoever is subscribed **right now**.
    ///
    /// Returns how many subscribers received it. Zero is not an error: pub/sub
    /// has no queue and no retry, so a message published while the relay is
    /// restarting is simply gone. Anything that must survive that belongs in a
    /// queue, not here.
    pub async fn publish(&self, channel: &str, payload: &[u8]) -> Result<u64> {
        self.connection.clone().query(redis::cmd("PUBLISH").arg(channel).arg(payload)).await
    }

    /// `EXISTS`.
    pub async fn exists(&self, key: &str) -> Result<bool> {
        let count: i64 = self.connection.clone().query(redis::cmd("EXISTS").arg(key)).await?;
        Ok(count > 0)
    }

    /// `INCRBY`, which creates the key at zero and is atomic server-side.
    pub async fn incr_by(&self, key: &str, delta: i64) -> Result<i64> {
        self.connection.clone().query(redis::cmd("INCRBY").arg(key).arg(delta)).await
    }

    /// `FLUSHDB` — empties the **whole database**.
    ///
    /// Including other applications' keys and anybody's sessions, if they share
    /// it. A caller that means "mine" wants a key prefix, not this.
    pub async fn flushdb(&self) -> Result<()> {
        self.connection.clone().run(&redis::cmd("FLUSHDB")).await
    }

    /// `PING`.
    pub async fn ping(&self) -> Result<bool> {
        let reply: String = self.connection.clone().query(&redis::cmd("PING")).await?;
        Ok(reply.eq_ignore_ascii_case("pong"))
    }
}

impl std::fmt::Debug for RedisClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisClient").field("backend", &self.describe()).finish()
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;

    /// These need a live server. Run them with
    /// `cargo test -p rainier-drivers --features redis-driver -- --ignored`
    /// against a Redis on 6379 whose database 15 is expendable.
    async fn client() -> RedisClient {
        RedisClient::connect(&RedisConnector::open("redis://127.0.0.1:6379/15").unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn a_value_round_trips() {
        let client = client().await;
        client.flushdb().await.unwrap();

        client.set("k", b"v", None).await.unwrap();
        assert_eq!(client.get("k").await.unwrap(), Some(b"v".to_vec()));
        assert!(client.exists("k").await.unwrap());
        assert!(client.del("k").await.unwrap());
        assert!(!client.del("k").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn a_sub_second_ttl_expires_rather_than_never() {
        // Seconds-based EX would round 1ms to 0, which Redis reads as no expiry.
        let client = client().await;
        client.set("brief", b"v", Some(Duration::from_millis(1))).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!client.exists("brief").await.unwrap());
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn set_nx_lets_exactly_one_caller_win() {
        let client = client().await;
        client.del("lock").await.unwrap();

        assert!(client.set_nx("lock", b"mine", Some(Duration::from_secs(5))).await.unwrap());
        assert!(!client.set_nx("lock", b"theirs", Some(Duration::from_secs(5))).await.unwrap());
        assert_eq!(client.get("lock").await.unwrap(), Some(b"mine".to_vec()));
    }

    #[tokio::test]
    #[ignore = "needs a live Redis"]
    async fn incr_by_starts_at_zero_and_accumulates() {
        let client = client().await;
        client.del("hits").await.unwrap();

        assert_eq!(client.incr_by("hits", 1).await.unwrap(), 1);
        assert_eq!(client.incr_by("hits", 4).await.unwrap(), 5);
        assert_eq!(client.incr_by("hits", -2).await.unwrap(), 3);
    }
}
