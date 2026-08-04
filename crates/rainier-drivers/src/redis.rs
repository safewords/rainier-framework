//! Redis transport — a connector that speaks to a single server or a sharded
//! cluster behind one type.
//!
//! # There is no pool here, and that is not a gap
//!
//! Both backends **multiplex**: one socket carries every concurrent command,
//! and the client matches each reply to the request that asked for it. A
//! [`RedisConnection`] is a handle to that socket, so cloning it costs nothing
//! and opens nothing — and a pool on top would add sockets without adding
//! throughput.
//!
//! Worth saying plainly, because the database layer next door *does* pool and
//! the difference is not an oversight. A SQL connection is **exclusive** for the
//! length of a statement, so concurrency there means more connections. A Redis
//! connection is not, so concurrency here means more requests in flight on the
//! one connection.
//!
//! Two things follow, and they are what [`RedisSettings`] is for.
//!
//! **There is nothing to size.** A process holds one connection per client —
//! one per node, on a cluster — so a server's `maxclients` divided by the number
//! of processes is the whole calculation, and no setting here moves it. Nothing
//! to exhaust means no `acquire` to queue on either: the hot-path failure a
//! pool's acquire timeout converts into a legible error arrives here in a
//! different shape, as a command that never returns, and
//! [`response_timeout`](RedisSettings::response_timeout) is what converts *that*
//! one.
//!
//! **There is nothing to recycle.** A pool guards against the socket a proxy
//! silently dropped by retiring connections after a while and opening fresh
//! ones. With one socket there is no fresh one to hand out, and a
//! `MultiplexedConnection` does not re-establish itself: once the socket is
//! gone, every command on every clone fails **for the life of the process**.
//! [`reconnect`](RedisSettings::reconnect) is the guard that applies to this
//! shape, and the one worth setting.

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

/// What a connection may wait for, and what it does when its socket goes away.
///
/// **Not a pool**, because there is nothing here to pool — see the [module
/// docs](self) for why one socket is the right answer for Redis and what
/// changes as a result. These are the settings that shape of connection can
/// actually honour.
///
/// Every field is optional and **nothing is set by default**, so a connector
/// built without settings behaves exactly as it did before this type existed:
/// no timeouts, and no recovery from a dropped socket. Each one changes *when*
/// an application is told about a failure rather than whether it has one, which
/// is a deployment's decision rather than a default worth imposing on
/// everybody's tests.
///
/// ```
/// use std::time::Duration;
/// use rainier_drivers::{Reconnect, RedisSettings};
///
/// let settings = RedisSettings::new()
///     .connect_timeout(Duration::from_secs(2))
///     .response_timeout(Duration::from_millis(500))
///     .reconnect(Reconnect::new().max_backoff(Duration::from_secs(2)));
/// # let _ = settings;
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RedisSettings {
    connect_timeout: Option<Duration>,
    response_timeout: Option<Duration>,
    reconnect: Option<Reconnect>,
}

impl RedisSettings {
    /// Nothing set — the connector's behaviour with no settings at all.
    pub fn new() -> Self {
        Self::default()
    }

    /// How long opening a connection may take before it fails.
    ///
    /// Covers the socket *and* the handshake, which matters: a server that
    /// accepts the connection and then does not answer is indistinguishable
    /// from one that is merely slow, and without this the wait is however long
    /// the operating system's own connect takes — which for a silently dropped
    /// route is minutes.
    ///
    /// Applies to reconnection attempts too, so a connection being
    /// re-established cannot stall longer than this per attempt.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// How long a command may wait for its reply before it fails.
    ///
    /// **The one to set on a hot path.** Sessions, cache reads, rate limits,
    /// queue pushes and broadcast publishes all go through Redis, so a server
    /// that accepted a command and never answered stalls every one of them at
    /// once — and the symptom is "the whole site is slow", which names nothing
    /// and points at nothing. A response timeout turns that into a
    /// [`ServiceUnavailable`](rainier_support::ErrorKind::ServiceUnavailable)
    /// that says Redis, which is the difference between an outage somebody can
    /// act on and an afternoon spent reading dashboards.
    ///
    /// Size it against what the call can afford, not against what Redis
    /// normally takes: a request that waits five seconds for a cache read has
    /// already failed the person waiting for it.
    pub fn response_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = Some(timeout);
        self
    }

    /// Re-establish the connection when its socket is lost.
    ///
    /// **The guard that replaces a pool's recycling-by-age.** Without it a
    /// dropped socket is permanent: the connection does not re-open itself, and
    /// every clone of it — the cache's, the queue's, the broadcaster's — fails
    /// every command until the process restarts. The usual cause is not a Redis
    /// outage at all but a proxy or a NAT table dropping a connection that had
    /// been idle, which is why the failure arrives at three in the morning and
    /// looks like nothing.
    ///
    /// The cost is spelled out in [`Reconnect`]: at least one command fails per
    /// loss, always. What this buys is that the *next* one does not.
    pub fn reconnect(mut self, reconnect: Reconnect) -> Self {
        self.reconnect = Some(reconnect);
        self
    }

    /// The connect timeout, if one was declared.
    pub fn connect_timeout_period(&self) -> Option<Duration> {
        self.connect_timeout
    }

    /// The response timeout, if one was declared.
    pub fn response_timeout_period(&self) -> Option<Duration> {
        self.response_timeout
    }

    /// The reconnection policy, if one was declared.
    pub fn reconnection(&self) -> Option<Reconnect> {
        self.reconnect
    }

    /// Whether nothing at all is declared.
    ///
    /// The connector takes a different path for these, so that an application
    /// which declares no settings runs the same code it ran before settings
    /// existed rather than a configured path that happens to be configured with
    /// nothing.
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// Whether these settings can be honoured, and a message when they cannot.
    ///
    /// Checked where a connector is built, so a bad declaration fails at boot
    /// rather than becoming a connection that quietly ignores what it was told.
    ///
    /// # Errors
    ///
    /// When a timeout is zero, or a reconnection policy would never reconnect.
    pub fn validate(&self) -> Result<()> {
        if self.connect_timeout == Some(Duration::ZERO) {
            return Err(Error::internal(
                "a `connect_timeout` of zero expires before the socket is open, so every \
                 connection fails; leave it unset to wait as long as the operating system does",
            ));
        }

        if self.response_timeout == Some(Duration::ZERO) {
            return Err(Error::internal(
                "a `response_timeout` of zero expires before the reply can arrive, so every \
                 command fails; leave it unset to wait indefinitely",
            ));
        }

        if let Some(reconnect) = self.reconnect {
            reconnect.validate()?;
        }
        Ok(())
    }

    /// These settings as the client's own connection options.
    fn async_config(&self) -> redis::AsyncConnectionConfig {
        let mut config = redis::AsyncConnectionConfig::new();
        if let Some(timeout) = self.connect_timeout {
            config = config.set_connection_timeout(timeout);
        }
        if let Some(timeout) = self.response_timeout {
            config = config.set_response_timeout(timeout);
        }
        config
    }

    /// These settings as the client's own reconnection options.
    ///
    /// Only the fields that were declared are set, so the rest stay at the
    /// client's defaults rather than at a second set of numbers kept here.
    fn manager_config(&self, reconnect: Reconnect) -> redis::aio::ConnectionManagerConfig {
        let mut config = redis::aio::ConnectionManagerConfig::new()
            .set_number_of_retries(reconnect.attempts as usize);

        if let Some(ceiling) = reconnect.max_backoff {
            // Milliseconds, which is the unit this option is in.
            config = config.set_max_delay(ceiling.as_millis().min(u64::MAX as u128) as u64);
        }
        if let Some(timeout) = self.connect_timeout {
            config = config.set_connection_timeout(timeout);
        }
        if let Some(timeout) = self.response_timeout {
            config = config.set_response_timeout(timeout);
        }
        config
    }
}

/// How hard to try to get the connection back.
///
/// **One command still fails per loss**, always: the failure is how the
/// connection finds out it is gone. Everything here is about the commands after
/// that one — whether they fail too, and for how long.
///
/// The defaults are the client's own: six attempts with an exponentially
/// growing wait between them, about six seconds in total. That is a deliberate
/// choice not to invent numbers, so a connector that declares
/// [`Reconnect::new`] and one that reaches the client directly agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconnect {
    attempts: u32,
    max_backoff: Option<Duration>,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self { attempts: DEFAULT_RECONNECT_ATTEMPTS, max_backoff: None }
    }
}

/// What the client retries a lost connection by default.
///
/// Mirrored rather than invented — the client's own
/// `DEFAULT_NUMBER_OF_CONNECTION_RETRIES` — so that declaring the default
/// explicitly and leaving it out mean the same thing.
const DEFAULT_RECONNECT_ATTEMPTS: u32 = 6;

impl Reconnect {
    /// The client's own retry policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times to retry re-establishing the connection.
    ///
    /// Once these are spent the connection stays down and commands go on
    /// failing, so this is not "give up and lose Redis" — a later attempt can
    /// still succeed — but the wait between attempts grows exponentially, and a
    /// large number is mostly a long tail of waiting.
    pub fn attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }

    /// A ceiling on the wait between attempts.
    ///
    /// Worth setting. The wait doubles each time, so on a server that stays
    /// down the later attempts are minutes apart — and when it comes back, the
    /// application does not: it is asleep until whatever wait it last started.
    /// A ceiling bounds how stale that can get.
    pub fn max_backoff(mut self, ceiling: Duration) -> Self {
        self.max_backoff = Some(ceiling);
        self
    }

    /// How many retries this policy allows.
    pub fn attempt_limit(&self) -> u32 {
        self.attempts
    }

    /// The ceiling on the wait between attempts, if one was declared.
    pub fn backoff_ceiling(&self) -> Option<Duration> {
        self.max_backoff
    }

    /// Whether this policy can reconnect at all.
    ///
    /// # Errors
    ///
    /// When it allows no attempts, which reads as reconnection being on and
    /// behaves as it being off.
    fn validate(&self) -> Result<()> {
        if self.attempts == 0 {
            return Err(Error::internal(
                "a reconnection policy of zero attempts never reconnects, which is what leaving \
                 `reconnect` out already means; give it attempts or leave it out",
            ));
        }

        if self.max_backoff == Some(Duration::ZERO) {
            return Err(Error::internal(
                "a `max_backoff` of zero retries with no wait at all, which spends every attempt \
                 in the instant the connection dropped and reconnects to nothing",
            ));
        }
        Ok(())
    }
}

/// A live connection, single-node or cluster.
///
/// Both variants multiplex, so this is cheap to clone and does not need a pool:
/// concurrent commands share one socket per node and are matched up by the
/// client. A pool on top would add sockets without adding throughput.
///
/// # Not `rainier_queue::RedisConnection`
///
/// That one is a **declaration** — a URL and some settings, in the queue's
/// `connections` section, sibling to its `SqsConnection` and `KafkaConnection`.
/// This one is an open socket. Both names are right for their own family
/// ([`MemcachedConnector`](crate::MemcachedConnector) and
/// [`MemcachedConnection`](crate::MemcachedConnection) are the same pair here),
/// so neither is renamed to avoid the other — but nothing imports both today,
/// and anything that starts to should alias one at the `use`.
#[derive(Clone)]
pub enum RedisConnection {
    /// A multiplexed connection to one server.
    ///
    /// **Does not re-establish itself.** When the socket goes, every clone of
    /// this fails every command until the process restarts — see
    /// [`Reconnecting`](Self::Reconnecting), which is the same connection with
    /// that one difference.
    Single(MultiplexedConnection),

    /// The same multiplexed connection, re-opened when it is lost.
    ///
    /// What [`RedisSettings::reconnect`] asks for. One command still fails per
    /// loss — that failure is how the connection learns it is gone — and the
    /// ones after it do not.
    Reconnecting(redis::aio::ConnectionManager),

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
            RedisConnection::Reconnecting(connection) => command.query_async(connection).await,
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
            RedisConnection::Single(_) | RedisConnection::Reconnecting(_) => false,
            #[cfg(feature = "redis-cluster")]
            RedisConnection::Cluster(_) => true,
        }
    }

    /// Whether this connection re-opens itself after losing its socket.
    ///
    /// A cluster connection does, of its own accord. A single-server one does
    /// only where [`RedisSettings::reconnect`] asked for it, and this is the
    /// way to check at boot that it was asked for.
    pub fn reconnects(&self) -> bool {
        match self {
            RedisConnection::Single(_) => false,
            RedisConnection::Reconnecting(_) => true,
            #[cfg(feature = "redis-cluster")]
            RedisConnection::Cluster(_) => true,
        }
    }
}

impl std::fmt::Debug for RedisConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RedisConnection::Single(_) => "RedisConnection::Single",
            RedisConnection::Reconnecting(_) => "RedisConnection::Reconnecting",
            #[cfg(feature = "redis-cluster")]
            RedisConnection::Cluster(_) => "RedisConnection::Cluster",
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
    settings: RedisSettings,
    /// Where [`subscribe`](Self::subscribe) opens its own connection.
    ///
    /// A subscriber cannot share the multiplexed connection: once a Redis
    /// connection subscribes it accepts nothing but more subscribe and
    /// unsubscribe commands, so a shared one would take the cache and the
    /// queue down with it.
    ///
    /// One node's address even on a cluster. Ordinary `PUBLISH` is propagated
    /// across the whole cluster — it is not slot-routed, which is why cluster
    /// mode has a separate `SPUBLISH` for the sharded kind — so a subscriber
    /// on any single node sees everything published anywhere.
    pubsub_url: String,
}

impl RedisConnector {
    /// Open a connector to a single server.
    ///
    /// `redis://host:port/db`, or `rediss://` for TLS.
    ///
    /// No timeouts and no recovery from a dropped socket — see
    /// [`open_with`](Self::open_with), which is this with settings.
    pub fn open(url: &str) -> Result<Self> {
        Self::open_with(url, RedisSettings::new())
    }

    /// Open a connector to a single server, with settings.
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use rainier_drivers::{Reconnect, RedisConnector, RedisSettings};
    ///
    /// # fn run() -> rainier_support::Result<()> {
    /// let redis = RedisConnector::open_with(
    ///     "redis://127.0.0.1/",
    ///     RedisSettings::new()
    ///         .response_timeout(Duration::from_millis(500))
    ///         .reconnect(Reconnect::new()),
    /// )?;
    /// # let _ = redis; Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// When the URL cannot be parsed, or the settings cannot be honoured. Both
    /// fail here rather than at the first command, so a mistake is a boot
    /// failure rather than a connection that quietly ignores what it was told.
    pub fn open_with(url: &str, settings: RedisSettings) -> Result<Self> {
        settings.validate()?;

        let client = redis::Client::open(url).map_err(|e| {
            // The URL frequently contains a password, so it is not echoed.
            Error::internal(format!("could not open a Redis client: {e}"))
        })?;

        Ok(Self {
            backend: Backend::Single(client),
            description: "redis".to_string(),
            settings,
            pubsub_url: url.to_string(),
        })
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
        Self::open_cluster_with(urls, RedisSettings::new())
    }

    /// Open a connector to a sharded cluster, with settings.
    ///
    /// A cluster connection already re-establishes itself and already retries,
    /// so [`reconnect`](RedisSettings::reconnect) here **tightens** a policy
    /// rather than turning one on. That is worth knowing before leaving it out:
    /// the client's own default is sixteen retries with a wait that grows to
    /// roughly eleven minutes, which is a long time for a command on a hot path
    /// to be neither answered nor failed.
    ///
    /// # Errors
    ///
    /// When there are no seeds, when a URL cannot be parsed, or when the
    /// settings cannot be honoured.
    #[cfg(feature = "redis-cluster")]
    pub fn open_cluster_with(
        urls: impl IntoIterator<Item = impl Into<String>>,
        settings: RedisSettings,
    ) -> Result<Self> {
        settings.validate()?;

        let seeds: Vec<String> = urls.into_iter().map(Into::into).collect();
        if seeds.is_empty() {
            return Err(Error::internal("a Redis cluster needs at least one seed node"));
        }
        let count = seeds.len();
        let first_seed = seeds[0].clone();

        // The builder even when nothing is declared: `ClusterClient::new` is
        // this with no options set, so an application that declares nothing
        // gets the client it got before.
        let mut builder = redis::cluster::ClusterClient::builder(seeds);
        if let Some(timeout) = settings.connect_timeout_period() {
            builder = builder.connection_timeout(timeout);
        }
        if let Some(timeout) = settings.response_timeout_period() {
            builder = builder.response_timeout(timeout);
        }
        if let Some(reconnect) = settings.reconnection() {
            builder = builder.retries(reconnect.attempt_limit());
            if let Some(ceiling) = reconnect.backoff_ceiling() {
                builder = builder.max_retry_wait(ceiling.as_millis().min(u64::MAX as u128) as u64);
            }
        }

        let client = builder
            .build()
            .map_err(|e| Error::internal(format!("could not open a Redis cluster client: {e}")))?;

        Ok(Self {
            backend: Backend::Cluster(client),
            description: format!("redis-cluster({count} seeds)"),
            settings,
            // The first seed, because a subscriber wants one node and any node
            // will do — see `pubsub_url`. `seeds` is non-empty, checked above.
            pubsub_url: first_seed,
        })
    }

    /// Listen for everything published to channels matching `pattern`.
    ///
    /// `PSUBSCRIBE`, on a **new connection of its own** — see
    /// [`pubsub_url`](Self::pubsub_url). A pattern rather than a channel list
    /// because the interesting subscribers do not know their channels up
    /// front: a socket server learns them as clients arrive and forgets them as
    /// they leave, and re-issuing `SUBSCRIBE` on every one of those would race
    /// the events it exists to deliver.
    ///
    /// Glob syntax is Redis's own, so a prefix pattern ends in `*`:
    /// `"lewd-production:*"`.
    ///
    /// # Cluster
    ///
    /// One node is enough. `PUBLISH` is broadcast across the cluster rather
    /// than routed to a slot — the sharded variant is a separate command,
    /// `SPUBLISH` — so a subscriber attached to any single node receives what
    /// every node publishes.
    ///
    /// # Errors
    ///
    /// When the connection cannot be opened or the subscribe is refused.
    pub async fn subscribe(&self, pattern: &str) -> Result<RedisSubscription> {
        // Built here rather than reusing `backend`: a cluster client cannot
        // hand out a plain pub/sub connection, and this needs a plain one.
        let client = redis::Client::open(self.pubsub_url.as_str()).map_err(|e| {
            // Never echoed — a Redis URL routinely carries a password.
            Error::internal(format!("could not open a Redis subscriber: {e}"))
        })?;

        let mut pubsub = client.get_async_pubsub().await.map_err(|e| {
            Error::service_unavailable(format!("could not reach Redis to subscribe: {e}"))
        })?;

        pubsub.psubscribe(pattern).await.map_err(|e| {
            Error::service_unavailable(format!("could not subscribe to `{pattern}`: {e}"))
        })?;

        Ok(RedisSubscription { stream: pubsub.into_on_message() })
    }

    /// Where [`subscribe`](Self::subscribe) connects.
    ///
    /// The connector's own URL for a single server, and the first seed for a
    /// cluster.
    pub fn pubsub_url(&self) -> &str {
        &self.pubsub_url
    }

    /// A label for diagnostics.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The settings every connection from this connector is opened with.
    pub fn settings(&self) -> &RedisSettings {
        &self.settings
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
            // Three paths, and the first is the one that matters: with nothing
            // declared this is the call it has always been, not a configured
            // call that happens to be configured with nothing.
            Backend::Single(client) if self.settings.is_unset() => client
                .get_multiplexed_async_connection()
                .await
                .map(RedisConnection::Single)
                .map_err(redis_error),

            Backend::Single(client) => match self.settings.reconnection() {
                Some(reconnect) => client
                    .get_connection_manager_with_config(self.settings.manager_config(reconnect))
                    .await
                    .map(RedisConnection::Reconnecting)
                    .map_err(redis_error),
                None => client
                    .get_multiplexed_async_connection_with_config(&self.settings.async_config())
                    .await
                    .map(RedisConnection::Single)
                    .map_err(redis_error),
            },

            // The cluster's settings were baked into the client when it was
            // opened, because that is where its builder takes them.
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

/// Names the backend and the settings, and never the URL.
///
/// Hand-written rather than derived, and it stays that way: a Redis URL carries
/// its password inline — `redis://default:hunter2@host:6379` — so a derived
/// `Debug` would put it in the log of every process that dumped its
/// configuration at boot. [`RedisSettings`] holds no credential, so it is
/// rendered whole.
impl std::fmt::Debug for RedisConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConnector")
            .field("backend", &self.description)
            .field("settings", &self.settings)
            .finish()
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

    // --- settings -----------------------------------------------------------

    #[test]
    fn a_connector_with_no_settings_declares_none() {
        // The condition the connect path branches on: this is what keeps an
        // application that configured nothing on the code it ran before.
        assert!(RedisConnector::open("redis://127.0.0.1:6379/").unwrap().settings().is_unset());
    }

    #[test]
    fn the_settings_reach_the_connector_that_will_open_with_them() {
        let settings = RedisSettings::new()
            .connect_timeout(Duration::from_secs(2))
            .response_timeout(Duration::from_millis(250))
            .reconnect(Reconnect::new().attempts(3).max_backoff(Duration::from_secs(5)));

        let connector = RedisConnector::open_with("redis://127.0.0.1:6379/", settings).unwrap();

        assert!(!connector.settings().is_unset());
        assert_eq!(connector.settings().connect_timeout_period(), Some(Duration::from_secs(2)));
        assert_eq!(
            connector.settings().response_timeout_period(),
            Some(Duration::from_millis(250))
        );
        let reconnect = connector.settings().reconnection().expect("declared");
        assert_eq!(reconnect.attempt_limit(), 3);
        assert_eq!(reconnect.backoff_ceiling(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn a_reconnection_policy_reaches_the_clients_own_options() {
        // One step further than the accessors: this is the value handed to the
        // client, so a setting that stopped being applied fails here.
        let settings = RedisSettings::new()
            .connect_timeout(Duration::from_secs(2))
            .response_timeout(Duration::from_millis(250));
        let reconnect = Reconnect::new().attempts(3).max_backoff(Duration::from_millis(1500));

        let rendered = format!("{:?}", settings.manager_config(reconnect));

        assert!(rendered.contains("number_of_retries: 3"), "{rendered}");
        assert!(rendered.contains("max_delay: Some(1500)"), "{rendered}");
        assert!(rendered.contains("connection_timeout: Some(2s)"), "{rendered}");
        assert!(rendered.contains("response_timeout: Some(250ms)"), "{rendered}");
    }

    #[test]
    fn an_undeclared_setting_is_left_at_the_clients_default_rather_than_ours() {
        // A second set of numbers kept here is a second set to drift.
        let rendered = format!("{:?}", RedisSettings::new().manager_config(Reconnect::new()));

        assert!(rendered.contains("max_delay: None"), "{rendered}");
        assert!(rendered.contains("connection_timeout: None"), "{rendered}");
        assert!(rendered.contains("response_timeout: None"), "{rendered}");
        assert!(
            rendered.contains(&format!("number_of_retries: {DEFAULT_RECONNECT_ATTEMPTS}")),
            "{rendered}"
        );
    }

    #[test]
    fn a_zero_timeout_is_refused_rather_than_accepted_and_disabling_everything() {
        // Both read as "no waiting" and mean "nothing works", which is the
        // worst kind of setting to accept.
        let connect = RedisConnector::open_with(
            "redis://127.0.0.1:6379/",
            RedisSettings::new().connect_timeout(Duration::ZERO),
        )
        .unwrap_err();
        assert!(connect.message().contains("`connect_timeout` of zero"), "{}", connect.message());

        let response = RedisConnector::open_with(
            "redis://127.0.0.1:6379/",
            RedisSettings::new().response_timeout(Duration::ZERO),
        )
        .unwrap_err();
        assert!(
            response.message().contains("`response_timeout` of zero"),
            "{}",
            response.message()
        );
    }

    #[test]
    fn a_reconnection_that_would_never_reconnect_is_refused() {
        // It reads as reconnection being on and behaves as it being off, which
        // is exactly the belief that survives to production.
        let err = RedisConnector::open_with(
            "redis://127.0.0.1:6379/",
            RedisSettings::new().reconnect(Reconnect::new().attempts(0)),
        )
        .unwrap_err();

        assert!(err.message().contains("never reconnects"), "{}", err.message());
    }

    #[test]
    fn settings_are_refused_before_the_url_is_even_parsed() {
        // So a bad declaration cannot be masked by a URL that also happens to
        // be wrong, and so the message names the setting.
        let err = RedisConnector::open_with(
            "postgres://user:hunter2@host/db",
            RedisSettings::new().response_timeout(Duration::ZERO),
        )
        .unwrap_err();

        assert!(err.message().contains("`response_timeout`"), "{}", err.message());
        assert!(!err.message().contains("hunter2"), "{}", err.message());
    }

    #[test]
    fn no_debug_rendering_of_a_connector_discloses_the_url() {
        // A Redis DSN carries its password inline, so a configuration dump at
        // boot must not put it in the log of every process that started.
        let connector = RedisConnector::open_with(
            "redis://default:hunter2@cache.internal:6379/0",
            RedisSettings::new().response_timeout(Duration::from_millis(250)),
        )
        .unwrap();

        let rendered = format!("{connector:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("cache.internal"), "{rendered}");
        assert!(rendered.contains("250ms"), "{rendered}");
    }

    /// A server that accepts the connection and then says nothing.
    ///
    /// Which is the case a connect timeout is for, and the reason this is not
    /// tested against a closed port: a closed port refuses immediately, so it
    /// cannot tell a timeout that fired from a timeout that was never set.
    async fn a_silent_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let held = tokio::spawn(async move {
            // Accepted and held open. The client's handshake waits for a reply
            // that never comes.
            let mut sockets = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                sockets.push(socket);
            }
        });

        (format!("redis://127.0.0.1:{port}/"), held)
    }

    #[tokio::test]
    async fn a_connect_timeout_reaches_the_socket_it_configures() {
        let (url, held) = a_silent_server().await;

        let connector = RedisConnector::open_with(
            &url,
            RedisSettings::new().connect_timeout(Duration::from_millis(100)),
        )
        .unwrap();

        let err = tokio::time::timeout(Duration::from_secs(5), connector.connect())
            .await
            .expect("the declared timeout should have fired long before this one")
            .expect_err("a server that never answers cannot be connected to");
        assert_eq!(err.status(), 503, "{}", err.message());

        held.abort();
    }

    // --- losing the socket --------------------------------------------------

    /// A server that speaks just enough RESP to answer, and hangs up once.
    ///
    /// Which is what a proxy reaping a connection that had been idle looks like
    /// from this side: the socket closes with nothing said about it. Returns the
    /// URL and a count of how many times it has been connected to, so a test can
    /// tell "recovered" from "never noticed".
    async fn a_server_that_hangs_up_once(
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);

        let serving = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
                tokio::spawn(serve_until_bored(socket, first));
            }
        });

        (format!("redis://127.0.0.1:{port}/"), connections, serving)
    }

    /// Answer `+PONG` to every command; on the first connection, hang up after
    /// the handshake and one command.
    async fn serve_until_bored(socket: tokio::net::TcpStream, hang_up: bool) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (reading, mut writing) = socket.into_split();
        let mut reader = BufReader::new(reading);
        let mut line = String::new();
        let mut answered = 0;

        loop {
            line.clear();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }

            // `*n` opens a command of `n` arguments, each a `$len` line and a
            // payload line. Anything else is not a command and is skipped.
            let Some(arguments) =
                line.trim().strip_prefix('*').and_then(|count| count.parse::<usize>().ok())
            else {
                continue;
            };

            for _ in 0..arguments * 2 {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
            }

            if writing.write_all(b"+PONG\r\n").await.is_err() {
                return;
            }

            // Two handshake commands, then the caller's first. Enough to prove
            // the connection worked before it did not.
            answered += 1;
            if hang_up && answered >= 3 {
                return;
            }
        }
    }

    /// Ping until it answers, or give up. The reconnection happens in the
    /// background, so the recovery is not the very next command.
    async fn ping_until_it_answers(client: &RedisClient) -> bool {
        for _ in 0..40 {
            if client.ping().await.is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_dropped_socket_is_permanent_without_reconnection() {
        // The finding this whole type exists for, asserted rather than
        // asserted-about: there is no pool to recycle from, and a multiplexed
        // connection does not re-open itself, so one dropped socket ends the
        // process's Redis — silently, and usually because something idle was
        // reaped rather than because Redis went anywhere.
        use std::sync::atomic::Ordering;

        let (url, connections, serving) = a_server_that_hangs_up_once().await;
        let client = RedisClient::connect(&RedisConnector::open(&url).unwrap()).await.unwrap();

        assert!(client.ping().await.is_ok(), "the connection works before the socket goes");
        assert!(!client.reconnects());

        assert!(
            !ping_until_it_answers(&client).await,
            "a connection with no reconnection should stay down"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1, "and should never reconnect");

        serving.abort();
    }

    #[tokio::test]
    async fn declaring_reconnection_is_what_gets_it_back() {
        use std::sync::atomic::Ordering;

        let (url, connections, serving) = a_server_that_hangs_up_once().await;
        let client = RedisClient::connect(
            &RedisConnector::open_with(
                &url,
                RedisSettings::new()
                    .reconnect(Reconnect::new().max_backoff(Duration::from_millis(50))),
            )
            .unwrap(),
        )
        .await
        .unwrap();

        assert!(client.ping().await.is_ok(), "the connection works before the socket goes");
        assert!(client.reconnects());

        assert!(ping_until_it_answers(&client).await, "reconnection should get the client back");
        assert!(
            connections.load(Ordering::SeqCst) > 1,
            "which means a second connection, not a first that never noticed"
        );

        serving.abort();
    }

    #[tokio::test]
    async fn without_one_the_same_connect_waits_as_it_always_did() {
        // The other half, and the one that matters for "absent settings are
        // today's behaviour": the connect that used to hang still hangs.
        let (url, held) = a_silent_server().await;

        let connector = RedisConnector::open(&url).unwrap();

        let outcome = tokio::time::timeout(Duration::from_millis(300), connector.connect()).await;
        assert!(outcome.is_err(), "an unset connect timeout should still wait indefinitely");

        held.abort();
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

    /// Whether this client's connection re-opens itself after losing its
    /// socket.
    ///
    /// Worth asserting at boot on anything long-lived. `false` means a dropped
    /// socket — a proxy reaping an idle connection is the usual way — takes the
    /// cache, the queue and the broadcaster with it until the process is
    /// restarted.
    pub fn reconnects(&self) -> bool {
        self.connection.reconnects()
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

/// A live `PSUBSCRIBE`, as a stream of published messages.
///
/// Holds its own connection for as long as it exists. Dropping it unsubscribes
/// and closes that connection, which is the only way to stop: there is no
/// cancel token, because the useful lifetime of a subscriber is the lifetime of
/// whatever is reading from it.
pub struct RedisSubscription {
    stream: redis::aio::PubSubStream,
}

impl RedisSubscription {
    /// The next message, or `None` when the connection has ended.
    ///
    /// `None` is not "nothing has been published" — this waits for that. It
    /// means the socket is gone, and a long-lived reader should treat it as the
    /// signal to resubscribe rather than as the end of the work.
    pub async fn next_message(&mut self) -> Option<PublishedMessage> {
        use futures_util::StreamExt;

        let message = self.stream.next().await?;
        Some(PublishedMessage {
            channel: message.get_channel_name().to_string(),
            payload: message.get_payload_bytes().to_vec(),
        })
    }
}

impl std::fmt::Debug for RedisSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The stream has no useful rendering and the URL behind it carries a
        // password, so neither appears.
        f.debug_struct("RedisSubscription").finish_non_exhaustive()
    }
}

/// One message off a [`RedisSubscription`].
#[derive(Clone, Debug)]
pub struct PublishedMessage {
    /// The channel it was published to — the concrete one, not the pattern.
    pub channel: String,
    /// The body, exactly as published.
    pub payload: Vec<u8>,
}

impl PublishedMessage {
    /// The body as UTF-8, when it is.
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.payload).ok()
    }
}
