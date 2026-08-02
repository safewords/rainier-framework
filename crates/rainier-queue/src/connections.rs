//! Connections as configuration — [`Connections`], [`ConnectionConfig`],
//! [`QueueResources`].
//!
//! A [`QueueManager`] dispatches onto a default connection and, once an
//! application has more than one backend, onto named ones. Something has to put
//! them there. Doing it imperatively works until two connections live on
//! **different drivers**, at which point the loop that builds them all from one
//! client produces a connection with the right name pointed at the wrong store.
//!
//! That failure is quieter than the filesystem's. A disk pointed at the wrong
//! bucket at least reads back empty, and somebody notices a missing file. A job
//! dispatched to the wrong backend is *accepted*: the push succeeds, an id comes
//! back, the caller carries on — and the job lands in a store no worker drains.
//! Nothing raises. Nothing retries. There is no failed-job row, because the job
//! never failed; it was never run.
//!
//! So a connection declares **its own** settings, and is built from those alone:
//!
//! ```
//! use rainier_container::Container;
//! use rainier_queue::{ConnectionConfig, Connections, JobRegistry, QueueResources};
//! use std::sync::Arc;
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let connections = Connections::new("primary")
//!     .with("primary", ConnectionConfig::memory())
//!     .with("bulk", ConnectionConfig::memory());
//!
//! let resources = QueueResources::new(Arc::new(JobRegistry::new()), Arc::new(Container::new()));
//! let queue = connections.build(&resources).await?;
//!
//! assert!(queue.connection("bulk").is_some());
//! assert!(queue.connection("nowhere").is_none());
//! # Ok(()) }
//! ```
//!
//! ## The same thing, from the configuration tree
//!
//! [`Connections`] deserialises from the shape a `queue` section already has — a
//! `default` naming one of the entries in `connections`, and each entry naming
//! its own driver:
//!
//! ```
//! # use rainier_queue::Connections;
//! # use serde_json::json;
//! let connections: Connections = serde_json::from_value(json!({
//!     "default": "primary",
//!     "connections": {
//!         "primary": { "driver": "database", "reservation": 90 },
//!         "bulk": {
//!             "driver": "sqs",
//!             "queue_url": "https://sqs.example.com/000000000000/bulk",
//!             "region": "us-east-1",
//!             "wait_time": 20,
//!         },
//!     },
//! })).unwrap();
//!
//! assert_eq!(connections.default_name(), "primary");
//! ```
//!
//! Nothing here is an application's business but the values: the framework names
//! no connection, no queue, no broker and no environment variable.
//!
//! ## What a declaration refuses
//!
//! Every rejection below is a case where accepting the declaration would give a
//! working-looking connection that stores jobs somewhere other than the one
//! intended, so each is a boot failure instead:
//!
//! | Declaration | Why it is refused |
//! |---|---|
//! | no `driver` | an assumed driver is a connection pointed at whatever the default happens to be |
//! | `queue` on any connection | the queue is the job's to name, and one here would be a decoy — see [`ConnectionConfig`] |
//! | `url` on an `sqs` connection | somebody believes these jobs reach Redis; they reach SQS |
//! | `key` without `secret` | falls back to the ambient chain, and drains a **different account's** queue |
//! | `key` and `secret` with no `region` | a signed request has to name one, and a guess is a wrong one |
//! | `brokers: []` | a Kafka client with no bootstrap broker has nowhere to connect |
//! | `default` naming an undeclared connection | the fallback would be silent, and the wrong backend |
//!
//! ## What a connection cannot declare
//!
//! Three of the drivers are built from something no configuration file can hold:
//! `sync` needs the job registry and the container it resolves dependencies
//! from, `database` needs the application's own [`Database`], and `kafka` needs
//! a shared lock store. Those arrive through [`QueueResources`] rather than
//! through the config tree, which is the one place this module departs from its
//! filesystem sibling — a disk needs nothing from the running application, and a
//! queue does.
//!
//! They are still per-connection in the sense that matters: a driver that needs
//! one and was not given it is a boot failure naming the missing piece, not a
//! connection that quietly becomes something else.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rainier_cache::{Cache, LockManager};
use rainier_container::Container;
use rainier_database::Database;
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::database::DatabaseQueue;
use crate::driver::QueueDriver;
use crate::job::JobRegistry;
use crate::manager::{QueueManager, SyncQueue};
use crate::queue::{MemoryQueue, Queue};

/// The queue connections an application declares, and which of them is the
/// default.
///
/// The `queue` section, as a type. Deserialises from the configuration tree and
/// builds a [`QueueManager`] in one call, so declaring a second backend is a
/// config edit rather than a line of wiring.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connections {
    /// Which entry of `connections` a plain dispatch goes to.
    #[serde(default = "conventional_default")]
    default: String,

    /// Every declared connection, by the name callers reach it with.
    ///
    /// A `BTreeMap` so a dump and a build order are stable — a `HashMap` would
    /// make an error that lists the declared connections read differently each
    /// run.
    #[serde(default)]
    connections: BTreeMap<String, ConnectionConfig>,
}

/// The connection name assumed when a `queue` section does not say.
///
/// A convention rather than a guess at the application's naming: `sync` is what
/// the equivalent section elsewhere defaults to, and it is the one driver that
/// cannot leave a job somewhere nobody drains — it has already run it. A
/// `default` naming a connection that is not declared fails at
/// [`build`](Connections::build) rather than falling back.
fn conventional_default() -> String {
    "sync".to_string()
}

impl Connections {
    /// An empty set whose default connection will be `default`.
    ///
    /// The name has to be declared with [`with`](Self::with) before
    /// [`build`](Self::build) will succeed.
    pub fn new(default: impl Into<String>) -> Self {
        Self { default: default.into(), connections: BTreeMap::new() }
    }

    /// Declare a connection under `name`.
    pub fn with(
        mut self,
        name: impl Into<String>,
        connection: impl Into<ConnectionConfig>,
    ) -> Self {
        self.connections.insert(name.into(), connection.into());
        self
    }

    /// The name of the connection that will be the default.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// The declaration filed under `name`.
    pub fn get(&self, name: &str) -> Option<&ConnectionConfig> {
        self.connections.get(name)
    }

    /// Every declared name, in a stable order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.connections.keys().map(String::as_str)
    }

    /// Whether anything is declared at all.
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Build every declared connection and assemble them into a
    /// [`QueueManager`].
    ///
    /// Each connection is built from **its own** declaration. There is no shared
    /// client to inherit from, which is the entire point: two connections on two
    /// stores with two credential sets are two backends, and the version of this
    /// that built them from one client produced a second connection with the
    /// right name pointed at the wrong store — accepting every job pushed to it
    /// and running none of them.
    ///
    /// A connection is built **once** and registered under its name *and*, if it
    /// is the default, as the default. Building it twice would give
    /// `QueueManager::connection("primary")` a different backend from the
    /// default even though both name one declaration — for `memory` a dispatch
    /// through one that is invisible to a worker draining the other, which is
    /// the same "queued and never run" outcome from a different direction.
    ///
    /// The default name is checked before anything is built, so a typo fails
    /// immediately instead of after opening connections nothing was going to
    /// use.
    ///
    /// # Errors
    ///
    /// When the `default` names a connection that is not declared, when a
    /// declaration cannot be built, or when a driver needs something
    /// [`QueueResources`] was not given.
    pub async fn build(&self, resources: &QueueResources) -> Result<QueueManager> {
        if !self.connections.contains_key(&self.default) {
            return Err(Error::internal(format!(
                "the default queue connection `{}` is not declared; declared connections are {}",
                self.default,
                self.declared()
            )));
        }

        let mut built: Vec<(&str, Arc<dyn Queue>)> = Vec::with_capacity(self.connections.len());
        for (name, connection) in &self.connections {
            let queue = connection.build(resources).await.map_err(|e| {
                Error::internal(format!("queue connection `{name}`: {}", e.message()))
            })?;
            built.push((name, queue));
        }

        let default = built
            .iter()
            .find(|(name, _)| *name == self.default)
            .map(|(_, queue)| Arc::clone(queue))
            .expect("the default was checked against the same map");

        let mut manager = QueueManager::new(default, Arc::clone(resources.registry()));

        // The same store the Kafka driver leases partitions from, if there is
        // one: a lock store that can exclude another process is exactly what
        // `Job::unique_id` needs, and leaving it unwired here would mean an
        // application whose config asks for uniqueness silently not getting it.
        if let Some(cache) = resources.lock_store() {
            manager = manager.with_locks(LockManager::new(Arc::clone(cache)));
        }

        for (name, queue) in built {
            manager = manager.with_connection(name, queue);
        }
        Ok(manager)
    }

    /// The declared names, backtick-quoted, for an error message.
    fn declared(&self) -> String {
        if self.connections.is_empty() {
            return "none".to_string();
        }
        self.names().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
    }
}

// Deliberately no `Default`: an empty set declares no connections, so its
// default name cannot resolve and `build` fails. A constructor whose result does
// not work is worse than one that asks for the one thing it needs.

impl std::fmt::Debug for Connections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connections")
            .field("default", &self.default)
            .field("connections", &self.connections)
            .finish()
    }
}

/// What a connection needs that a configuration file cannot hold.
///
/// A disk is built from its declaration and nothing else. A queue is not: the
/// `sync` driver runs jobs, so it needs the registry that maps a name back to
/// code and the container that job resolves its dependencies from; the
/// `database` driver stores jobs in the application's own tables; the `kafka`
/// driver decides partition ownership with a lock store shared between workers.
/// None of those are values — they are the running application.
///
/// Passing them here rather than reaching for a global keeps the same property
/// the declarations have: a driver that needs one and was not given it says so,
/// by name, instead of resolving to whatever a global happened to hold.
pub struct QueueResources {
    registry: Arc<JobRegistry>,
    container: Arc<Container>,
    database: Option<Database>,
    lock_store: Option<Arc<dyn Cache>>,
}

impl QueueResources {
    /// The two every application has: the job registry and the container.
    pub fn new(registry: Arc<JobRegistry>, container: Arc<Container>) -> Self {
        Self { registry, container, database: None, lock_store: None }
    }

    /// The database a `database` connection stores its jobs in.
    pub fn with_database(mut self, database: Database) -> Self {
        self.database = Some(database);
        self
    }

    /// The store locks are taken in.
    ///
    /// Two things want it: the `kafka` driver, which leases partitions so two
    /// workers do not drain the same one, and [`Job::unique_id`](crate::Job::unique_id).
    /// It must be shared between processes to be worth anything — an in-process
    /// cache excludes nobody, and the Kafka driver refuses one outright rather
    /// than let every worker own every partition.
    pub fn with_lock_store(mut self, cache: Arc<dyn Cache>) -> Self {
        self.lock_store = Some(cache);
        self
    }

    /// The job registry.
    pub fn registry(&self) -> &Arc<JobRegistry> {
        &self.registry
    }

    /// The container a `sync` connection resolves job dependencies from.
    pub fn container(&self) -> &Arc<Container> {
        &self.container
    }

    /// The database, if one was given.
    pub fn database(&self) -> Option<&Database> {
        self.database.as_ref()
    }

    /// The lock store, if one was given.
    pub fn lock_store(&self) -> Option<&Arc<dyn Cache>> {
        self.lock_store.as_ref()
    }
}

impl std::fmt::Debug for QueueResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // What is present, not what is in it: a database holds a DSN and a lock
        // store holds a Redis client, and both would put a password in whatever
        // logged this.
        f.debug_struct("QueueResources")
            .field("jobs", &self.registry.names())
            .field("database", &self.database.is_some())
            .field("lock_store", &self.lock_store.is_some())
            .finish()
    }
}

/// One connection: which driver, and the settings that driver needs.
///
/// An enum rather than a struct of optionals, so the settings a driver does not
/// have cannot be written down: there is no `brokers` on an SQS connection to
/// fill in and wonder why it is ignored. The wire form is still flat — `driver`
/// beside the rest — because that is what a configuration file wants to be.
///
/// # There is no connection-level `queue`
///
/// Laravel's equivalent section carries one, and this deliberately does not.
/// Every [`QueuedJob`](crate::QueuedJob) already has its queue set by the time
/// it reaches a connection — [`Job::QUEUE`](crate::Job::QUEUE) or
/// [`Job::queue()`](crate::Job::queue) resolves it at dispatch, and
/// `on_queue` overrides it — so a name here could only be ignored or override
/// what the job asked for. The first is a decoy: a setting that reads like it
/// routes work and does not. The second silently moves jobs off the queue their
/// worker is draining. A declaration that names one is refused, and says this.
///
/// For a second SQS queue, declare a second connection. An SQS queue *is* a
/// URL, so a second URL is what "somewhere else" means.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "RawConnection", into = "RawConnection")]
pub enum ConnectionConfig {
    /// Run the job inline, on the thread that dispatched it.
    Sync,

    /// A queue in this process's memory.
    Memory,

    /// Two tables in the application's own database.
    Database(DatabaseConnection),

    /// Redis streams.
    Redis(RedisConnection),

    /// An Amazon SQS queue.
    Sqs(SqsConnection),

    /// A Kafka topic.
    Kafka(KafkaConnection),
}

impl ConnectionConfig {
    /// Jobs run inline. The shorthand.
    pub fn sync() -> Self {
        Self::Sync
    }

    /// Jobs in this process's memory. The shorthand.
    pub fn memory() -> Self {
        Self::Memory
    }

    /// Jobs in the application's own database, with the driver's default
    /// reservation.
    pub fn database() -> Self {
        Self::Database(DatabaseConnection::new())
    }

    /// Which driver this declares.
    pub fn driver(&self) -> QueueDriver {
        match self {
            Self::Sync => QueueDriver::Sync,
            Self::Memory => QueueDriver::Memory,
            Self::Database(_) => QueueDriver::Database,
            Self::Redis(_) => QueueDriver::Redis,
            Self::Sqs(_) => QueueDriver::Sqs,
            Self::Kafka(_) => QueueDriver::Kafka,
        }
    }

    /// Build this connection, and only this connection.
    ///
    /// Every setting it uses comes from this declaration, so two connections
    /// built from two declarations share nothing — not a client, not a
    /// credential, not an endpoint.
    ///
    /// # Errors
    ///
    /// When the driver's feature is off, when a required resource is missing, or
    /// when the backend refuses the connection.
    pub async fn build(&self, resources: &QueueResources) -> Result<Arc<dyn Queue>> {
        match self {
            Self::Sync => Ok(Arc::new(SyncQueue::new(
                Arc::clone(resources.registry()),
                Arc::clone(resources.container()),
            ))),

            Self::Memory => Ok(Arc::new(MemoryQueue::new())),

            Self::Database(connection) => {
                let database = resources.database().ok_or_else(|| {
                    Error::internal(
                        "this connection uses the `database` driver, but no database was given \
                         to build it with; pass one with `QueueResources::with_database`",
                    )
                })?;
                Ok(Arc::new(connection.build(database.clone())))
            }

            #[cfg(feature = "redis")]
            Self::Redis(connection) => Ok(Arc::new(connection.build().await?)),

            // Loud, and naming the fix. Falling back to an in-memory queue would
            // "work": every dispatch would be accepted, and every job would be
            // invisible to the worker process that was supposed to run it.
            #[cfg(not(feature = "redis"))]
            Self::Redis(connection) => Err(Error::internal(format!(
                "this connection uses the `redis` driver for `{}`, but rainier-queue was built \
                 without the `redis` feature",
                connection.url_without_credentials()
            ))),

            #[cfg(feature = "sqs")]
            Self::Sqs(connection) => Ok(Arc::new(connection.build().await?)),

            #[cfg(not(feature = "sqs"))]
            Self::Sqs(connection) => Err(Error::internal(format!(
                "this connection uses the `sqs` driver for `{}`, but rainier-queue was built \
                 without the `sqs` feature",
                connection.queue_url()
            ))),

            #[cfg(feature = "kafka")]
            Self::Kafka(connection) => {
                let cache = resources.lock_store().ok_or_else(|| {
                    Error::internal(
                        "this connection uses the `kafka` driver, which needs a lock store \
                         shared between workers to decide which of them owns which partition; \
                         pass one with `QueueResources::with_lock_store`",
                    )
                })?;
                Ok(Arc::new(connection.build(LockManager::new(Arc::clone(cache))).await?))
            }

            #[cfg(not(feature = "kafka"))]
            Self::Kafka(connection) => Err(Error::internal(format!(
                "this connection uses the `kafka` driver on `{}`, but rainier-queue was built \
                 without the `kafka` feature",
                connection.brokers().join(", ")
            ))),
        }
    }
}

impl From<DatabaseConnection> for ConnectionConfig {
    fn from(connection: DatabaseConnection) -> Self {
        Self::Database(connection)
    }
}

impl From<RedisConnection> for ConnectionConfig {
    fn from(connection: RedisConnection) -> Self {
        Self::Redis(connection)
    }
}

impl From<SqsConnection> for ConnectionConfig {
    fn from(connection: SqsConnection) -> Self {
        Self::Sqs(connection)
    }
}

impl From<KafkaConnection> for ConnectionConfig {
    fn from(connection: KafkaConnection) -> Self {
        Self::Kafka(connection)
    }
}

impl std::fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sync => f.write_str("Sync"),
            Self::Memory => f.write_str("Memory"),
            Self::Database(connection) => std::fmt::Debug::fmt(connection, f),
            Self::Redis(connection) => std::fmt::Debug::fmt(connection, f),
            Self::Sqs(connection) => std::fmt::Debug::fmt(connection, f),
            Self::Kafka(connection) => std::fmt::Debug::fmt(connection, f),
        }
    }
}

/// Jobs in two tables of the application's own database.
///
/// Durable, shared, and needs no new infrastructure, which is why it is the
/// usual production answer. Which database is not declared here — it is the
/// application's, and arrives through [`QueueResources::with_database`].
#[derive(Clone, Debug, Default)]
pub struct DatabaseConnection {
    reservation: Option<Duration>,
}

impl DatabaseConnection {
    /// The driver's own defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// How long a worker's claim on a job lasts.
    ///
    /// Must exceed the worker's job timeout. Below it, a job still running is
    /// reclaimed by a second worker and runs twice — which for anything that
    /// sends or charges is the expensive failure.
    pub fn reservation(mut self, reservation: Duration) -> Self {
        self.reservation = Some(reservation);
        self
    }

    /// The reservation period, if one was declared.
    pub fn reservation_period(&self) -> Option<Duration> {
        self.reservation
    }

    /// Build the connection, as its concrete driver.
    pub fn build(&self, database: Database) -> DatabaseQueue {
        let queue = DatabaseQueue::new(database);
        match self.reservation {
            Some(reservation) => queue.with_reservation(reservation),
            None => queue,
        }
    }
}

/// Jobs on Redis streams.
///
/// Read [the driver's docs](crate::redis) before choosing it: Redis
/// acknowledges a write before it is on disk and before a replica has it, so a
/// job it accepted can still be lost.
#[derive(Clone)]
pub struct RedisConnection {
    url: String,
    prefix: Option<String>,
    reservation: Option<Duration>,
}

impl RedisConnection {
    /// A connection to the server at `url` — `redis://host:port/db`, or
    /// `rediss://` for TLS.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), prefix: None, reservation: None }
    }

    /// Prefix every key this connection writes.
    ///
    /// What keeps two applications on one Redis from draining each other's
    /// jobs, and what makes two connections on the same server genuinely
    /// separate queues rather than one queue with two names.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// How long a reserved job may be held before another worker may claim it.
    pub fn reservation(mut self, reservation: Duration) -> Self {
        self.reservation = Some(reservation);
        self
    }

    /// The key prefix, if one was declared.
    pub fn prefix_name(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// The reservation period, if one was declared.
    pub fn reservation_period(&self) -> Option<Duration> {
        self.reservation
    }

    /// The server this connects to, with any credentials removed.
    ///
    /// The only reading of the URL there is, because a Redis URL routinely
    /// carries a password in its userinfo — `redis://default:hunter2@host:6379`
    /// — and the driver underneath deliberately never echoes one either. Enough
    /// to tell two connections apart in a log, and not enough to authenticate
    /// with.
    pub fn url_without_credentials(&self) -> String {
        without_credentials(&self.url)
    }

    /// Connect, and build the connection as its concrete driver.
    ///
    /// # Errors
    ///
    /// When the URL cannot be parsed or the server cannot be reached.
    #[cfg(feature = "redis")]
    pub async fn build(&self) -> Result<crate::redis::RedisQueue> {
        use rainier_drivers::RedisConnector;

        // Per connection and never shared. Sharing one connector is the bug
        // this module exists to make impossible: a second connection inheriting
        // the first's server keeps its own *name*, and every job pushed to it
        // waits in a store the worker that was supposed to drain it is not
        // watching.
        let queue = crate::redis::RedisQueue::connect(&RedisConnector::open(&self.url)?).await?;

        let queue = match &self.prefix {
            Some(prefix) => queue.with_prefix(prefix),
            None => queue,
        };
        Ok(match self.reservation {
            Some(reservation) => queue.with_reservation(reservation),
            None => queue,
        })
    }
}

/// Names the server and never the password.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the URL's userinfo into whatever logged the connection, which for
/// a configuration dump at boot means the password is in the log of every
/// process that started.
impl std::fmt::Debug for RedisConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConnection")
            .field("url", &self.url_without_credentials())
            .field("prefix", &self.prefix)
            .field("reservation", &self.reservation)
            .finish()
    }
}

/// A URL with its userinfo and query string removed.
///
/// Anything that does not parse as `scheme://…` is redacted whole. That is the
/// safe direction to be wrong in: a host nobody can read is an inconvenience,
/// and a password in a log is an incident.
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

/// Jobs on an Amazon SQS queue.
///
/// One connection is one queue, because an SQS queue *is* a URL. Two queues are
/// two connections, which is the honest way to say "somewhere else" rather than
/// a name the driver would silently disregard.
#[derive(Clone)]
pub struct SqsConnection {
    queue_url: String,
    region: Option<String>,
    endpoint: Option<String>,
    visibility_timeout: Option<Duration>,
    wait_time: Option<Duration>,
    credentials: SqsCredentials,
}

impl SqsConnection {
    /// The queue at `queue_url`, authenticating with the [ambient credential
    /// chain](SqsCredentials::Chain).
    pub fn new(queue_url: impl Into<String>) -> Self {
        Self {
            queue_url: queue_url.into(),
            region: None,
            endpoint: None,
            visibility_timeout: None,
            wait_time: None,
            credentials: SqsCredentials::Chain,
        }
    }

    /// The region to sign for.
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Talk to something other than AWS — ElasticMQ, LocalStack, a test double.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// How long a received message stays invisible to other workers.
    ///
    /// Must exceed how long a job takes, or a second worker picks it up while
    /// the first is still running it — the one reliable way to get a job
    /// executed twice.
    pub fn visibility_timeout(mut self, visibility: Duration) -> Self {
        self.visibility_timeout = Some(visibility);
        self
    }

    /// Wait up to this long for a message rather than returning immediately.
    ///
    /// Long polling, and worth turning on: at zero a worker on an empty queue
    /// makes a billed request every time round its loop.
    pub fn wait_time(mut self, wait_time: Duration) -> Self {
        self.wait_time = Some(wait_time);
        self
    }

    /// Authenticate with an explicit key pair rather than the ambient chain.
    ///
    /// A [`region`](Self::region) becomes required — see [`SqsCredentials`].
    pub fn credentials(
        mut self,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        self.credentials = SqsCredentials::Static {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
        };
        self
    }

    /// The queue's URL.
    pub fn queue_url(&self) -> &str {
        &self.queue_url
    }

    /// The region, if one was declared.
    pub fn region_name(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// The endpoint override, if one was declared.
    pub fn endpoint_url(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// The visibility timeout, if one was declared.
    pub fn visibility_period(&self) -> Option<Duration> {
        self.visibility_timeout
    }

    /// The long-poll wait, if one was declared.
    pub fn wait_period(&self) -> Option<Duration> {
        self.wait_time
    }

    /// How this connection authenticates.
    pub fn credential_source(&self) -> &SqsCredentials {
        &self.credentials
    }

    /// Whether this declaration can be built.
    ///
    /// Checked when a declaration is deserialised so a bad `queue` section fails
    /// while the configuration is being read, and again when the connection is
    /// built so one assembled in code fails the same way with the same message.
    fn validate(&self) -> Result<()> {
        if matches!(self.credentials, SqsCredentials::Static { .. }) && self.region.is_none() {
            return Err(Error::internal(format!(
                "the queue `{}` is declared with `key` and `secret` but no `region`; a signed \
                 request has to name one",
                self.queue_url
            )));
        }
        Ok(())
    }
}

#[cfg(feature = "sqs")]
impl SqsConnection {
    /// The connector this connection signs with.
    ///
    /// Built per connection and never shared. Sharing one is the bug this module
    /// exists to make impossible: a second connection inheriting the first's
    /// endpoint and credentials keeps its own queue *URL*, and a URL that
    /// resolves against the wrong account is a queue nobody drains.
    pub async fn connector(&self) -> Result<rainier_drivers::AwsConnector> {
        use rainier_drivers::AwsConnector;

        self.validate()?;

        let mut connector = match &self.credentials {
            SqsCredentials::Chain => match &self.region {
                Some(region) => AwsConnector::in_region(region.clone()).await,
                None => AwsConnector::from_env().await,
            },
            SqsCredentials::Static { access_key_id, secret_access_key } => {
                let region =
                    self.region.clone().expect("validate rejects a static pair without one");
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
        Ok(connector)
    }

    /// Build the connection, as its concrete driver.
    ///
    /// # Errors
    ///
    /// When the declaration does not [validate](Self::validate).
    pub async fn build(&self) -> Result<crate::sqs::SqsQueue> {
        let queue = crate::sqs::SqsQueue::new(&self.connector().await?, &self.queue_url);

        let queue = match self.visibility_timeout {
            Some(visibility) => queue.with_visibility_timeout(visibility),
            None => queue,
        };
        Ok(match self.wait_time {
            Some(wait_time) => queue.with_wait_time(wait_time),
            None => queue,
        })
    }
}

impl std::fmt::Debug for SqsConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqsConnection")
            .field("queue_url", &self.queue_url)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("visibility_timeout", &self.visibility_timeout)
            .field("wait_time", &self.wait_time)
            // The key pair is deliberately absent, not redacted-in-place: see
            // `SqsCredentials`, whose own `Debug` names the source and nothing
            // else.
            .field("credentials", &self.credentials)
            .finish()
    }
}

/// How an [`SqsConnection`] proves who it is.
///
/// Two cases, because there are two kinds of queue. AWS itself hands out
/// temporary credentials through a chain that has to be *discovered and
/// refreshed* — an instance role, an EKS service account, an SSO cache, a
/// profile — and a process that pins one at boot starts answering `403` a few
/// hours later. An ElasticMQ or a LocalStack has no chain at all.
///
/// [`Chain`](Self::Chain) is the default, and is the safe one to be wrong about:
/// a connection that should have named a key pair fails to authenticate, which
/// is loud. The reverse — a connection that names a key pair for the wrong
/// account — authenticates successfully against a queue nobody is draining.
#[derive(Clone, Default)]
pub enum SqsCredentials {
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
/// would print the key pair into whatever logged the connection, which for a
/// configuration dump at boot means the secret is in the log of every process
/// that started.
impl std::fmt::Debug for SqsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Chain => f.write_str("Chain"),
            Self::Static { .. } => f.write_str("Static(<redacted>)"),
        }
    }
}

/// Jobs on a Kafka topic.
///
/// Read [the driver's docs](crate::kafka) before choosing it: a log is not a
/// queue, and the differences are the kind that cost money.
#[derive(Clone, Debug)]
pub struct KafkaConnection {
    brokers: Vec<String>,
    group: Option<String>,
    topic_prefix: Option<String>,
    lease: Option<Duration>,
}

impl KafkaConnection {
    /// Bootstrap from `brokers`.
    ///
    /// Every one is a **seed**: the client asks whichever answers for the
    /// cluster's shape. Give it more than one, or a single dead broker makes the
    /// whole cluster unreachable.
    pub fn new(brokers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            brokers: brokers.into_iter().map(Into::into).collect(),
            group: None,
            topic_prefix: None,
            lease: None,
        }
    }

    /// Which set of cursors this connection's workers share.
    ///
    /// A consumer group by another name: two deployments reading the same topic
    /// under different groups each get every job, and under the same group they
    /// share it out.
    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Prefix the topic a queue name maps to.
    pub fn topic_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.topic_prefix = Some(prefix.into());
        self
    }

    /// How long a partition lease lasts.
    ///
    /// **Must exceed the longest job.** If it expires while one is still
    /// running, another worker takes the partition and runs from a cursor that
    /// has not moved — so the job in flight runs twice.
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = Some(lease);
        self
    }

    /// The bootstrap brokers.
    pub fn brokers(&self) -> &[String] {
        &self.brokers
    }

    /// The consumer group, if one was declared.
    pub fn group_name(&self) -> Option<&str> {
        self.group.as_deref()
    }

    /// The topic prefix, if one was declared.
    pub fn topic_prefix_name(&self) -> Option<&str> {
        self.topic_prefix.as_deref()
    }

    /// The partition lease, if one was declared.
    pub fn lease_period(&self) -> Option<Duration> {
        self.lease
    }

    /// Whether this declaration can be built.
    fn validate(&self) -> Result<()> {
        if self.brokers.is_empty() {
            return Err(Error::internal(
                "a `kafka` connection needs at least one bootstrap `broker` to connect to",
            ));
        }
        Ok(())
    }

    /// Connect, and build the connection as its concrete driver.
    ///
    /// # Errors
    ///
    /// When no broker is declared, when the cluster cannot be reached, or when
    /// `locks` cannot exclude another worker — see
    /// [`require_shared`](crate::kafka::require_shared).
    #[cfg(feature = "kafka")]
    pub async fn build(&self, locks: LockManager) -> Result<crate::kafka::KafkaQueue> {
        use rainier_drivers::{KafkaClient, KafkaConnector};

        self.validate()?;

        // Per connection and never shared, for the same reason every other
        // driver here builds its own: a second connection inheriting the first's
        // cluster keeps its own topic prefix and group, and a group reading a
        // topic nobody produces to is a worker that never runs anything.
        let client = KafkaClient::connect(&KafkaConnector::new(self.brokers.clone())).await?;
        let queue = crate::kafka::KafkaQueue::new(Arc::new(client), locks)?;

        let queue = match &self.group {
            Some(group) => queue.in_group(group),
            None => queue,
        };
        let queue = match &self.topic_prefix {
            Some(prefix) => queue.with_topic_prefix(prefix),
            None => queue,
        };
        Ok(match self.lease {
            Some(lease) => queue.with_lease(lease),
            None => queue,
        })
    }
}

// --- the wire form -----------------------------------------------------------

/// A connection as it is written down, before it is known to make sense.
///
/// The flat shape a configuration file wants, which [`ConnectionConfig`] is the
/// checked form of. Everything but `driver` is optional here so the *driver*
/// gets to say which settings apply, and so a misfiled one can be named in the
/// error rather than silently dropped.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConnection {
    /// Required: an assumed driver is a connection pointed at whichever backend
    /// the default happens to be.
    driver: QueueDriver,

    /// Declared by nobody, and present only to be refused with a reason — see
    /// [`ConnectionConfig`]. Without it, a section ported from Laravel fails
    /// with `unknown field \`queue\``, which explains nothing about why the
    /// framework does not have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queue: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    reservation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queue_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    visibility_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wait_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    brokers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease: Option<u64>,
}

impl RawConnection {
    /// Refuse a connection-level `queue`, whatever the driver.
    ///
    /// Not one of the settings a driver "does not use" — no driver here uses
    /// one, and the reason is a design decision rather than a mismatch, so it
    /// gets its own message.
    fn reject_a_connection_queue(&self) -> Result<()> {
        if self.queue.is_none() {
            return Ok(());
        }

        Err(Error::internal(
            "a queue connection does not name a `queue`: which queue a job goes on is the job's \
             (`Job::QUEUE`, or `Job::queue()` when it is computed) or the dispatch's \
             (`on_queue`), and it is always set by the time the job reaches a connection. A name \
             here could only be ignored — a setting that reads like it routes work and does not \
             — or override the one the worker is draining. For a second SQS queue, declare a \
             second connection: an SQS queue is a URL, so that is what `somewhere else` means",
        ))
    }

    /// Refuse settings this driver would ignore.
    ///
    /// A `url` on an `sqs` connection is not a harmless extra key — it is
    /// somebody believing these jobs reach the Redis they configured when they
    /// reach a queue in another account entirely. Dropping it silently is how
    /// that belief survives to production, where it looks like a worker that
    /// has stopped picking things up.
    fn reject_settings_it_ignores(&self, used: &[&str]) -> Result<()> {
        let declared: [(&str, bool); 14] = [
            ("reservation", self.reservation.is_some()),
            ("url", self.url.is_some()),
            ("prefix", self.prefix.is_some()),
            ("queue_url", self.queue_url.is_some()),
            ("region", self.region.is_some()),
            ("endpoint", self.endpoint.is_some()),
            ("key", self.key.is_some()),
            ("secret", self.secret.is_some()),
            ("visibility_timeout", self.visibility_timeout.is_some()),
            ("wait_time", self.wait_time.is_some()),
            ("brokers", self.brokers.is_some()),
            ("group", self.group.is_some()),
            ("topic_prefix", self.topic_prefix.is_some()),
            ("lease", self.lease.is_some()),
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
            "the `{}` driver does not use {}; that setting would be ignored, and a connection \
             that ignores where it was told to put a job is one that puts it somewhere else",
            self.driver,
            ignored.join(", ")
        )))
    }
}

impl TryFrom<RawConnection> for ConnectionConfig {
    type Error = Error;

    fn try_from(raw: RawConnection) -> Result<Self> {
        raw.reject_a_connection_queue()?;

        match raw.driver {
            QueueDriver::Sync => {
                raw.reject_settings_it_ignores(&[])?;
                Ok(Self::Sync)
            }

            QueueDriver::Memory => {
                raw.reject_settings_it_ignores(&[])?;
                Ok(Self::Memory)
            }

            QueueDriver::Database => {
                raw.reject_settings_it_ignores(&["reservation"])?;
                Ok(Self::Database(DatabaseConnection {
                    reservation: raw.reservation.map(Duration::from_secs),
                }))
            }

            QueueDriver::Redis => {
                raw.reject_settings_it_ignores(&["url", "prefix", "reservation"])?;

                let url = raw.url.ok_or_else(|| {
                    Error::internal(
                        "a `redis` connection needs a `url` to connect to — \
                         `redis://host:port/db`",
                    )
                })?;

                Ok(Self::Redis(RedisConnection {
                    url,
                    prefix: raw.prefix,
                    reservation: raw.reservation.map(Duration::from_secs),
                }))
            }

            QueueDriver::Sqs => {
                raw.reject_settings_it_ignores(&[
                    "queue_url",
                    "region",
                    "endpoint",
                    "key",
                    "secret",
                    "visibility_timeout",
                    "wait_time",
                ])?;

                let queue_url = raw.queue_url.ok_or_else(|| {
                    Error::internal("an `sqs` connection needs the `queue_url` of its queue")
                })?;

                let credentials = match (raw.key, raw.secret) {
                    (None, None) => SqsCredentials::Chain,
                    (Some(access_key_id), Some(secret_access_key)) => {
                        SqsCredentials::Static { access_key_id, secret_access_key }
                    }
                    // Half a key pair is the dangerous case, so it is the one
                    // spelled out: the missing half would fall back to the
                    // ambient chain, which means signing as *this* account
                    // against a URL that belongs to another one — a push that
                    // is refused if you are lucky and accepted into a queue
                    // nobody drains if you are not.
                    (Some(_), None) | (None, Some(_)) => {
                        return Err(Error::internal(format!(
                            "the `sqs` connection for `{queue_url}` declares one of `key` and \
                             `secret` but not the other; with only one it would authenticate \
                             from the ambient credential chain instead, against whatever that \
                             account can reach"
                        )))
                    }
                };

                let connection = SqsConnection {
                    queue_url,
                    region: raw.region,
                    endpoint: raw.endpoint,
                    visibility_timeout: raw.visibility_timeout.map(Duration::from_secs),
                    wait_time: raw.wait_time.map(Duration::from_secs),
                    credentials,
                };
                connection.validate()?;

                Ok(Self::Sqs(connection))
            }

            QueueDriver::Kafka => {
                raw.reject_settings_it_ignores(&["brokers", "group", "topic_prefix", "lease"])?;

                let brokers = raw.brokers.ok_or_else(|| {
                    Error::internal("a `kafka` connection needs its bootstrap `brokers`")
                })?;

                let connection = KafkaConnection {
                    brokers,
                    group: raw.group,
                    topic_prefix: raw.topic_prefix,
                    lease: raw.lease.map(Duration::from_secs),
                };
                connection.validate()?;

                Ok(Self::Kafka(connection))
            }
        }
    }
}

impl From<ConnectionConfig> for RawConnection {
    fn from(connection: ConnectionConfig) -> Self {
        let blank = |driver| Self {
            driver,
            queue: None,
            reservation: None,
            url: None,
            prefix: None,
            queue_url: None,
            region: None,
            endpoint: None,
            key: None,
            secret: None,
            visibility_timeout: None,
            wait_time: None,
            brokers: None,
            group: None,
            topic_prefix: None,
            lease: None,
        };

        match connection {
            ConnectionConfig::Sync => blank(QueueDriver::Sync),
            ConnectionConfig::Memory => blank(QueueDriver::Memory),

            ConnectionConfig::Database(connection) => Self {
                reservation: connection.reservation.map(|d| d.as_secs()),
                ..blank(QueueDriver::Database)
            },

            ConnectionConfig::Redis(connection) => Self {
                url: Some(connection.url),
                prefix: connection.prefix,
                reservation: connection.reservation.map(|d| d.as_secs()),
                ..blank(QueueDriver::Redis)
            },

            ConnectionConfig::Sqs(connection) => {
                let (key, secret) = match connection.credentials {
                    SqsCredentials::Chain => (None, None),
                    SqsCredentials::Static { access_key_id, secret_access_key } => {
                        (Some(access_key_id), Some(secret_access_key))
                    }
                };
                Self {
                    queue_url: Some(connection.queue_url),
                    region: connection.region,
                    endpoint: connection.endpoint,
                    visibility_timeout: connection.visibility_timeout.map(|d| d.as_secs()),
                    wait_time: connection.wait_time.map(|d| d.as_secs()),
                    key,
                    secret,
                    ..blank(QueueDriver::Sqs)
                }
            }

            ConnectionConfig::Kafka(connection) => Self {
                brokers: Some(connection.brokers),
                group: connection.group,
                topic_prefix: connection.topic_prefix,
                lease: connection.lease.map(|d| d.as_secs()),
                ..blank(QueueDriver::Kafka)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, JobContext, QueuedJob};
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Serialize, Deserialize)]
    struct Ping;

    #[async_trait::async_trait]
    impl Job for Ping {
        const NAME: &'static str = "test.ping";
        async fn handle(&self, _: &JobContext) -> Result<()> {
            Ok(())
        }
    }

    fn resources() -> QueueResources {
        QueueResources::new(Arc::new(JobRegistry::new()), Arc::new(Container::new()))
    }

    // --- reading a declaration ---------------------------------------------

    #[test]
    fn a_section_deserialises_into_the_connections_it_declares() {
        let connections: Connections = serde_json::from_value(json!({
            "default": "primary",
            "connections": {
                "primary": { "driver": "database", "reservation": 90 },
                "scratch": { "driver": "memory" },
                "bulk": {
                    "driver": "sqs",
                    "queue_url": "https://sqs.example.com/000000000000/bulk",
                    "region": "us-east-1",
                },
            },
        }))
        .unwrap();

        assert_eq!(connections.default_name(), "primary");
        assert_eq!(connections.names().collect::<Vec<_>>(), vec!["bulk", "primary", "scratch"]);
        assert_eq!(connections.get("primary").unwrap().driver(), QueueDriver::Database);
        assert_eq!(connections.get("scratch").unwrap().driver(), QueueDriver::Memory);
        assert_eq!(connections.get("bulk").unwrap().driver(), QueueDriver::Sqs);
    }

    #[test]
    fn a_connection_without_a_driver_is_refused() {
        // An assumed driver is a connection pointed at whichever backend the
        // default happens to be, which is the whole failure this module is
        // about.
        let err = serde_json::from_value::<Connections>(json!({
            "connections": { "primary": { "url": "redis://localhost:6379" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("driver"), "{err}");
    }

    #[test]
    fn a_misspelled_driver_lists_the_valid_ones() {
        let err = serde_json::from_value::<Connections>(json!({
            "connections": { "primary": { "driver": "sqz", "queue_url": "u" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`sync`"), "{err}");
        assert!(err.contains("`redis`"), "{err}");
        assert!(err.contains("`kafka`"), "{err}");
    }

    #[test]
    fn a_setting_the_driver_ignores_is_refused_rather_than_dropped() {
        // Someone believes these jobs reach the Redis they configured. They
        // reach a queue in another account entirely.
        let err = serde_json::from_value::<Connections>(json!({
            "connections": {
                "bulk": { "driver": "sqs", "queue_url": "u", "url": "redis://localhost:6379" },
            },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`url`"), "{err}");
        assert!(err.contains("does not use"), "{err}");
    }

    #[test]
    fn an_unknown_setting_is_refused_rather_than_dropped() {
        let err = serde_json::from_value::<Connections>(json!({
            "connections": { "primary": { "driver": "redis", "url": "u", "prefx": "typo" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("prefx"), "{err}");
    }

    #[test]
    fn a_connection_level_queue_is_refused_with_the_reason() {
        // The Laravel section has one. This does not, because a name here could
        // only be ignored or override the queue the worker is draining — and
        // `unknown field` would explain neither.
        for declaration in [
            json!({ "driver": "sync", "queue": "default" }),
            json!({ "driver": "redis", "url": "redis://localhost:6379", "queue": "bulk" }),
        ] {
            let err =
                serde_json::from_value::<ConnectionConfig>(declaration).unwrap_err().to_string();

            assert!(err.contains("does not name a `queue`"), "{err}");
            assert!(err.contains("on_queue"), "{err}");
        }
    }

    #[test]
    fn each_driver_requires_what_it_cannot_work_without() {
        let no_url = serde_json::from_value::<ConnectionConfig>(json!({ "driver": "redis" }))
            .unwrap_err()
            .to_string();
        assert!(no_url.contains("`url`"), "{no_url}");

        let no_queue_url = serde_json::from_value::<ConnectionConfig>(json!({ "driver": "sqs" }))
            .unwrap_err()
            .to_string();
        assert!(no_queue_url.contains("`queue_url`"), "{no_queue_url}");

        let no_brokers = serde_json::from_value::<ConnectionConfig>(json!({ "driver": "kafka" }))
            .unwrap_err()
            .to_string();
        assert!(no_brokers.contains("`brokers`"), "{no_brokers}");

        let empty_brokers =
            serde_json::from_value::<ConnectionConfig>(json!({ "driver": "kafka", "brokers": [] }))
                .unwrap_err()
                .to_string();
        assert!(empty_brokers.contains("broker"), "{empty_brokers}");
    }

    #[test]
    fn a_declaration_round_trips_through_its_wire_form() {
        for original in [
            json!({
                "driver": "sqs",
                "queue_url": "https://sqs.example.com/000000000000/bulk",
                "region": "us-east-1",
                "endpoint": "https://sqs.example.invalid",
                "visibility_timeout": 120,
                "wait_time": 20,
                "key": "id",
                "secret": "shh",
            }),
            json!({ "driver": "redis", "url": "redis://localhost:6379", "prefix": "app:" }),
            json!({ "driver": "kafka", "brokers": ["one:9092"], "group": "workers", "lease": 60 }),
            json!({ "driver": "database", "reservation": 90 }),
            json!({ "driver": "sync" }),
        ] {
            let connection: ConnectionConfig = serde_json::from_value(original.clone()).unwrap();
            assert_eq!(serde_json::to_value(&connection).unwrap(), original);
        }
    }

    // --- credentials --------------------------------------------------------

    #[test]
    fn credentials_default_to_the_ambient_chain() {
        // The safe direction: a connection that should have named a key pair
        // fails to authenticate. The reverse authenticates against a queue
        // nobody drains.
        let connection: ConnectionConfig =
            serde_json::from_value(json!({ "driver": "sqs", "queue_url": "u" })).unwrap();

        let ConnectionConfig::Sqs(sqs) = connection else { panic!("declared as sqs") };
        assert!(matches!(sqs.credential_source(), SqsCredentials::Chain));
    }

    #[test]
    fn half_a_key_pair_is_refused_rather_than_falling_back_to_the_chain() {
        for half in [
            json!({ "driver": "sqs", "queue_url": "u", "key": "id", "region": "us-east-1" }),
            json!({ "driver": "sqs", "queue_url": "u", "secret": "shh", "region": "us-east-1" }),
        ] {
            let err = serde_json::from_value::<ConnectionConfig>(half).unwrap_err().to_string();
            assert!(err.contains("ambient credential chain"), "{err}");
        }
    }

    #[test]
    fn an_explicit_key_pair_needs_a_region_to_sign_for() {
        let err = serde_json::from_value::<ConnectionConfig>(
            json!({ "driver": "sqs", "queue_url": "u", "key": "id", "secret": "shh" }),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("`region`"), "{err}");
    }

    #[test]
    fn no_debug_rendering_discloses_a_credential() {
        // The one that has to hold whatever else changes: a config dump at boot
        // must not put the secret in the log of every process that started.
        // Both of this section's secrets are here — the SQS key pair, and the
        // password a Redis URL carries in its userinfo.
        let connections = Connections::new("bulk")
            .with(
                "bulk",
                SqsConnection::new("https://sqs.example.com/000000000000/bulk")
                    .region("us-east-1")
                    .credentials("AKIA-visible", "super-secret"),
            )
            .with("primary", RedisConnection::new("redis://default:hunter2@cache.internal:6379/0"));

        let rendered = format!("{connections:?}");

        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("AKIA-visible"), "{rendered}");
        assert!(!rendered.contains("hunter2"), "{rendered}");

        // Still enough to tell two connections apart in a log.
        assert!(rendered.contains("bulk"), "{rendered}");
        assert!(rendered.contains("cache.internal:6379"), "{rendered}");
    }

    #[test]
    fn a_url_that_does_not_parse_is_redacted_whole() {
        // The safe direction to be wrong in: a host nobody can read is an
        // inconvenience, a password in a log is an incident.
        assert_eq!(without_credentials("not a url at all"), "<redacted>");

        // A password in the query string goes too.
        assert_eq!(
            without_credentials("redis://cache.internal:6379/0?password=hunter2"),
            "redis://cache.internal:6379/0"
        );

        // An `@` in the path is not a userinfo.
        assert_eq!(
            without_credentials("redis://cache.internal:6379/user@host"),
            "redis://cache.internal:6379/user@host"
        );
    }

    #[test]
    fn the_resource_dump_names_no_secret_either() {
        let rendered = format!("{:?}", resources());
        assert!(rendered.contains("database"), "{rendered}");
        assert!(rendered.contains("lock_store"), "{rendered}");
    }

    // --- building -----------------------------------------------------------

    #[tokio::test]
    async fn two_connections_are_two_backends() {
        // The bug this module exists for, in its queue form: a second
        // connection that shares the first's store accepts every job pushed to
        // it and hands them to whichever worker is draining the other one.
        let connections = Connections::new("primary")
            .with("primary", ConnectionConfig::memory())
            .with("bulk", ConnectionConfig::memory());

        let manager = connections.build(&resources()).await.unwrap();

        let primary = manager.connection("primary").expect("declared");
        let bulk = manager.connection("bulk").expect("declared");

        primary.push(QueuedJob::from_job(&Ping).unwrap()).await.unwrap();

        assert_eq!(primary.size("default").await.unwrap(), 1);
        assert_eq!(bulk.size("default").await.unwrap(), 0, "two stores, not one");
    }

    #[tokio::test]
    async fn connections_on_different_drivers_resolve_to_different_backends() {
        let connections: Connections = serde_json::from_value(json!({
            "default": "primary",
            "connections": {
                "primary": { "driver": "memory" },
                "inline": { "driver": "sync" },
            },
        }))
        .unwrap();

        let manager = connections.build(&resources()).await.unwrap();

        assert_eq!(manager.connection("primary").unwrap().name(), "memory");
        assert_eq!(manager.connection("inline").unwrap().name(), "sync");
    }

    #[tokio::test]
    async fn an_undeclared_name_is_still_none() {
        let manager = Connections::new("primary")
            .with("primary", ConnectionConfig::memory())
            .build(&resources())
            .await
            .unwrap();

        assert!(manager.connection("primary").is_some());
        assert!(manager.connection("bulk").is_none());
        assert!(!manager.has_connection("bulk"));
    }

    #[tokio::test]
    async fn the_default_connection_is_the_same_backend_as_its_name() {
        // Built once, registered twice. Building it twice would give
        // `connection("primary")` a different store from the default, and a job
        // dispatched through one would be invisible to a worker draining the
        // other — queued, and never run.
        let manager = Connections::new("primary")
            .with("primary", ConnectionConfig::memory())
            .build(&resources())
            .await
            .unwrap();

        manager.dispatch(Ping).await.unwrap();

        assert_eq!(manager.connection("primary").unwrap().size("default").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_default_naming_an_undeclared_connection_fails_instead_of_falling_back() {
        let connections = Connections::new("primary").with("bulk", ConnectionConfig::memory());

        let err = connections.build(&resources()).await.err().expect("the default is undeclared");
        assert!(err.message().contains("`primary`"), "{}", err.message());
        assert!(err.message().contains("`bulk`"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_build_failure_names_the_connection_it_came_from() {
        // With a dozen connections declared, "needs a database" without a name
        // is a search rather than a fix.
        let connections = Connections::new("primary").with("primary", ConnectionConfig::database());

        let err = connections.build(&resources()).await.err().expect("no database was given");
        assert!(err.message().starts_with("queue connection `primary`:"), "{}", err.message());
        assert!(err.message().contains("with_database"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_driver_whose_feature_is_off_is_an_error_and_not_a_substitution() {
        // A quiet fallback to memory would "work": every dispatch accepted,
        // every job invisible to the worker that was supposed to run it.
        let connections = Connections::new("primary")
            .with("primary", RedisConnection::new("redis://localhost:6379"));

        let built = connections.build(&resources()).await;

        if cfg!(feature = "redis") {
            // Nothing is listening on that port in a test run, so the failure
            // is a connection failure — which is still not a substitution.
            if let Ok(manager) = built {
                assert_eq!(manager.connection("primary").unwrap().name(), "redis");
            }
        } else {
            let err = built.err().expect("no redis driver to build with");
            assert!(err.message().contains("without the `redis` feature"), "{}", err.message());
        }
    }

    #[tokio::test]
    async fn a_kafka_connection_without_a_lock_store_says_which_one_is_missing() {
        let connections =
            Connections::new("events").with("events", KafkaConnection::new(["localhost:9092"]));

        let err = connections.build(&resources()).await.err().expect("no lock store was given");

        if cfg!(feature = "kafka") {
            assert!(err.message().contains("with_lock_store"), "{}", err.message());
        } else {
            assert!(err.message().contains("without the `kafka` feature"), "{}", err.message());
        }
    }

    #[tokio::test]
    async fn the_settings_a_declaration_carries_reach_the_driver() {
        let connection: ConnectionConfig = serde_json::from_value(json!({
            "driver": "database",
            "reservation": 120,
        }))
        .unwrap();

        let ConnectionConfig::Database(database) = connection else {
            panic!("declared as database")
        };
        assert_eq!(database.reservation_period(), Some(Duration::from_secs(120)));
    }
}
