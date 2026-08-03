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
//! ## Settings this framework cannot honour
//!
//! A section written against Laravel's `config/queue.php` carries several more.
//! Every one of them is **refused by name**, with what the framework does
//! instead, because the alternative is the failure this whole module is built
//! to avoid wearing a different hat: a setting that is read, understood by the
//! person who wrote it, and then dropped.
//!
//! An ignored setting is worse than a rejected one. A rejected `after_commit`
//! is a boot failure and a five-minute conversation. An accepted one is a
//! configuration file that states, in writing, that jobs wait for their
//! transaction — while they do not, and the person reading it has no reason to
//! doubt it.
//!
//! | Declaration | Why it cannot be honoured | What to write instead |
//! |---|---|---|
//! | `retry_after` | the same setting under another name | `reservation` — or `visibility_timeout` on `sqs`, `lease` on `kafka` |
//! | `after_commit` | this framework has **no transaction API** at all, so there is no commit to wait for | nothing; dispatch after the write returns |
//! | `block_for` | no driver here blocks; `reserve` returns immediately and the **worker** does the waiting | nothing; a worker's own `sleep` |
//! | `table` | the queue's tables are named on their entities at compile time, not per connection | nothing; run the driver's own migrations |
//! | `prefix`, `suffix` on `sqs` | they compose a queue *URL* out of parts, and an `sqs` connection is given the whole URL | `queue_url` |
//! | `connection` on `redis` | nothing in this framework declares a named Redis connection for it to point at | `url`, on the connection itself |
//! | `max_connections`, `min_connections`, `pool_size` | no driver here pools, and the Redis one **multiplexes** — one socket, every command | `response_timeout_ms` and `reconnect`, which address what a pool would have |
//!
//! Each is covered in more detail on [`ConnectionConfig`], which is where a
//! reader who hit the error will look.
//!
//! ## What a `redis` connection waits for
//!
//! Three settings, and the reason they are timeouts rather than a pool is that
//! the connection multiplexes: one socket carries every concurrent command, so
//! there is nothing to size and nothing to queue on.
//!
//! | Setting | What it bounds | What goes wrong without it |
//! |---|---|---|
//! | `connect_timeout_ms` | opening the socket, handshake included | a worker booting against a route that goes nowhere waits minutes before saying anything |
//! | `response_timeout_ms` | one command waiting for its reply | a server that accepted the command and went quiet holds every request open at once, and the symptom names nothing |
//! | `reconnect` | nothing — it *recovers* | a socket a proxy dropped while the queue was idle takes every push with it, permanently, until the process restarts |
//!
//! Milliseconds, unlike `reservation` and `lease` beside them, which are whole
//! seconds because they are periods a *job* waits. A command's budget is not: in
//! seconds the only values available are `0`, which would fail everything, and
//! `1`, which is already longer than a request can afford to spend pushing.
//!
//! ```
//! # use rainier_queue::Connections;
//! # use serde_json::json;
//! let connections: Connections = serde_json::from_value(json!({
//!     "default": "primary",
//!     "connections": {
//!         "primary": {
//!             "driver": "redis",
//!             "url": "redis://localhost:6379/1",
//!             "connect_timeout_ms": 2000,
//!             "response_timeout_ms": 250,
//!             "reconnect": true,
//!             "reconnect_max_backoff_ms": 2000,
//!         },
//!     },
//! })).unwrap();
//! # assert_eq!(connections.default_name(), "primary");
//! ```
//!
//! Every one of them is **off unless declared**, so a section that says nothing
//! behaves as it did before they existed — including the connection that does
//! not reconnect, which is why `reconnect` is the one to reach for first.
//!
//! Two connections on the same server are still two connections with two sets of
//! these, for the same reason they are two connections at all: sharing is what
//! makes one of them quietly inherit the other's settings.
//!
//! ## `reservation` is the one with teeth
//!
//! It is the only setting here whose *plausible* values include one that breaks
//! the queue silently, so it gets a check of its own rather than a warning in
//! prose: see [`Connections::check_reservations`].
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
use crate::worker::WorkerOptions;

/// What this driver calls "how long a claim lasts", or `None` for the two that
/// do not reclaim.
///
/// One place, so the error that refuses `retry_after` and the error that
/// refuses too short a reservation cannot come to disagree about what the
/// setting is called on a given driver — which would send a reader to fix a key
/// that is not the one they have.
fn reservation_setting(driver: QueueDriver) -> Option<&'static str> {
    match driver {
        QueueDriver::Sync | QueueDriver::Memory => None,
        QueueDriver::Database | QueueDriver::Redis => Some("reservation"),
        QueueDriver::Sqs => Some("visibility_timeout"),
        QueueDriver::Kafka => Some("lease"),
    }
}

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

    /// Check every declared reservation against the timeout `worker` will run
    /// jobs under.
    ///
    /// The one setting in this section whose plausible values include one that
    /// breaks the queue **without reporting anything**, so it gets a check
    /// rather than a warning in prose.
    ///
    /// A reservation is how long a worker's claim on a job lasts before the
    /// backend decides that worker is gone and lets another take it. A timeout
    /// is how long the worker will let a job run. If the reservation is the
    /// shorter of the two, then a job that runs past it is handed to a second
    /// worker **while the first is still running it**. Both then hold what each
    /// believes is an exclusive claim, and the job runs twice, concurrently.
    ///
    /// Nothing anywhere raises. There is no failed-job row, because neither
    /// attempt failed; the first worker acknowledges a job the second is still
    /// working on, and the second acknowledges one that is already gone. For a
    /// job that sends mail it is two emails. For one that charges a card it is
    /// two charges. It shows up as a support ticket, weeks later, and the queue
    /// is the last place anyone looks — the metrics say every job succeeded,
    /// and they are telling the truth.
    ///
    /// Equal durations are refused along with shorter ones: a job that runs to
    /// exactly its timeout races the reclaim, and which side wins is a
    /// scheduling detail.
    ///
    /// ```
    /// use rainier_queue::{ConnectionConfig, Connections, WorkerOptions};
    /// use std::time::Duration;
    ///
    /// let connections = Connections::new("primary").with("primary", ConnectionConfig::database());
    ///
    /// // The defaults are safe: a 90s claim outlives a 60s timeout.
    /// assert!(connections.check_reservations(&WorkerOptions::default()).is_ok());
    ///
    /// // Raising the timeout past the claim is the classic misconfiguration.
    /// let slow = WorkerOptions::default().timeout(Some(Duration::from_secs(600)));
    /// assert!(connections.check_reservations(&slow).is_err());
    /// ```
    ///
    /// Call it once at boot, wherever the worker's options and the queue
    /// section are both in scope. It is deliberately not folded into
    /// [`build`](Self::build): a process that only ever *dispatches* has no
    /// worker options to check against, and refusing to build a perfectly good
    /// connection because a caller could not name a timeout would be its own
    /// kind of wrong.
    ///
    /// A connection that declares no reservation is checked against the
    /// **driver's own default**, read from the driver rather than copied here —
    /// otherwise the check would pass exactly the configurations most likely to
    /// be wrong, the ones that declared nothing and got a number they never saw.
    ///
    /// # Errors
    ///
    /// When a connection's reservation does not outlive `worker`'s timeout, or
    /// when `worker` has no timeout at all — an unbounded job outlives every
    /// finite claim, so there is no reservation that would be safe.
    pub fn check_reservations(&self, worker: &WorkerOptions) -> Result<()> {
        for (name, connection) in &self.connections {
            let Some(reservation) = connection.reservation_period() else { continue };

            let setting = reservation_setting(connection.driver())
                .expect("a driver with a reservation names it");

            let Some(timeout) = worker.timeout else {
                return Err(Error::internal(format!(
                    "queue connection `{name}` lets another worker reclaim a job after {}s, and \
                     this worker has no timeout, so a job may run for longer than that — at \
                     which point it is running in two workers at once, and neither knows. Give \
                     the worker a timeout shorter than `{setting}`, or raise `{setting}` above \
                     the longest a job can take",
                    reservation.as_secs()
                )));
            };

            if reservation > timeout {
                continue;
            }

            return Err(Error::internal(format!(
                "queue connection `{name}` lets another worker reclaim a job after {}s, but this \
                 worker will let one run for {}s. A job that outlives the {}s claim is handed to \
                 a second worker while the first is still running it: it runs twice, at the same \
                 time, and nothing reports anything — both workers believe they hold it, and \
                 neither attempt fails. Raise `{setting}` above the worker's timeout, or lower \
                 the timeout below it",
                reservation.as_secs(),
                timeout.as_secs(),
                reservation.as_secs()
            )));
        }
        Ok(())
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
/// Laravel's equivalent section carries one, where it is a **fallback**: the
/// queue a job lands on when the job itself did not name one. That reading is
/// the sensible one, and it is precisely the one this framework cannot express
/// — because "the job did not name one" is not a state a
/// [`QueuedJob`](crate::QueuedJob) can be in.
///
/// [`Job::QUEUE`](crate::Job::QUEUE) is a `&'static str` that defaults to the
/// literal `"default"`, and [`QueuedJob::from_job`](crate::QueuedJob::from_job)
/// resolves it eagerly. By the time a job reaches a connection its `queue` is
/// always some concrete string, and a job that said nothing is byte-for-byte
/// indistinguishable from one that deliberately wrote `QUEUE = "default"`.
///
/// So a fallback here would have to guess, and the guess would be wrong in the
/// direction that hurts: a job that explicitly asked for `default` would be
/// moved to the connection's queue instead, quietly, and land somewhere its
/// worker is not draining. That is the same "accepted, stored, never run"
/// outcome the rest of this module exists to prevent, arrived at from a new
/// direction. A declaration that names a `queue` is refused, and says so.
///
/// **What it would take.** An absent queue has to be representable end to end:
/// [`Job::queue`](crate::Job::queue) returning `Option<String>` (with `QUEUE` an
/// `Option<&'static str>`), `QueuedJob::queue` holding an `Option<String>` on
/// the wire, and resolution moving from `from_job` to the point where a
/// connection is chosen — which is the only place that knows both what the job
/// asked for and what the connection would supply. That is a breaking change to
/// three public types, and worth doing deliberately rather than by adding a
/// field here that reads as if it already works.
///
/// For a second SQS queue, declare a second connection. An SQS queue *is* a
/// URL, so a second URL is what "somewhere else" means.
///
/// # `after_commit` is refused, not defaulted
///
/// Dispatching inside a transaction that later rolls back leaves a job holding
/// an id for a row that does not exist. The job then fails — or, once that id
/// has been handed to some later row, quietly acts on the **wrong record**.
/// Laravel's answer is `after_commit`, which holds the dispatch until the
/// surrounding transaction commits.
///
/// This framework has no transaction API for that to hook into: nothing in it
/// opens, commits or rolls back a transaction, and an application that needs
/// one reaches past the ORM to the underlying driver. There is therefore no
/// commit for a dispatch to wait on, and no way to notice a rollback.
///
/// Accepting the setting and ignoring it is the one option that is clearly
/// worse than the others, so it is refused. A configuration file that says
/// `after_commit` and does not do it is a false statement in the place people
/// go to find out what is true.
///
/// **What it would take.** A transaction abstraction with a notion of "the
/// transaction this task is inside", plus a per-transaction buffer of pending
/// dispatches flushed on commit and discarded on rollback. Neither exists.
///
/// # `block_for` is refused
///
/// [`Queue::reserve`] returns immediately on every driver here. The Redis
/// driver issues `XREADGROUP … COUNT 1` with no `BLOCK` argument, and the
/// underlying client has no blocking form to give it; the waiting is the
/// worker's, between polls, on [`WorkerOptions`]' `sleep`.
///
/// **What it would take.** A `BLOCK` argument threaded through the driver's
/// `xreadgroup_one`, and a worker loop that lets a driver own its own wait
/// instead of sleeping a fixed interval — because a blocking read and a polling
/// sleep would otherwise stack, and the queue would idle for the sum of the two.
///
/// # `table` is refused
///
/// The database driver's rows are ordinary Rainier ORM entities, and an
/// entity's table name is an associated function returning `&'static str` —
/// chosen at compile time, baked into every statement built from it, and shared
/// by the migrations that create it. There is no per-instance table name for a
/// connection to override, so a `table` here could only be read and dropped
/// while the driver went on using the entity's own.
///
/// **What it would take.** An instance-level table name carried through the
/// repository and query builders, and migrations parameterised by it.
///
/// # `prefix` and `suffix` are refused on `sqs`
///
/// In Laravel they are not decoration: a queue's URL is composed as
/// `prefix + queue + suffix`, which is how one deployment addresses
/// per-environment queues from one configuration file. An
/// [`SqsConnection`] is given the whole `queue_url` instead, and with no
/// connection-level `queue` (above) there is no middle for them to sit either
/// side of. They would be read and dropped, and the connection would go on
/// using the URL it was given — which is the right queue, so nobody would
/// notice until the environment that relied on the suffix drained another
/// environment's work.
///
/// # `connection` is refused on `redis`
///
/// It is a cross-section reference: in a Laravel application it names an entry
/// in `config/database.php`'s `redis` block, and naming a *dedicated* one there
/// matters more than it looks. The queue wants its own database index,
/// deliberately not the cache's, because flushing the cache flushes that index
/// — and every job waiting in it goes with it.
///
/// This framework has no such section. `rainier-database` declares no Redis
/// connections at all, and `rainier-cache` builds its Redis store from a URL
/// rather than from a named registry, so there is nothing for a `connection` to
/// point at. A [`RedisConnection`] therefore carries its own `url`, which is
/// also the honest place to keep the queue on its own index: give it a
/// different one from the cache's, in the URL's path.
///
/// Inventing a named-Redis registry inside this crate would be worse than the
/// gap. Two competing notions of "a Redis connection" — one here, one wherever
/// it belongs — is how a queue and a cache end up on the same index by
/// agreeing on a name and disagreeing about what it means.
///
/// # A connection pool is refused
///
/// `max_connections`, `min_connections` and `pool_size` are refused on every
/// driver, and on `redis` the reason is worth stating rather than assuming: the
/// connection **multiplexes**. One socket carries every concurrent command, and
/// the client matches each reply to the request that asked for it, so a pool on
/// top would open more sockets without moving more commands. There is nothing
/// to size.
///
/// That is not the same as saying the failures a pool guards against do not
/// happen here. They do, in a different shape, and two settings on the
/// connection address them:
///
/// - A pool's **acquire timeout** exists because an exhausted pool queues
///   callers on `acquire`, and latency climbs everywhere at once with nothing
///   naming the pool. With one multiplexed socket nothing queues on acquire —
///   but a server that accepted a command and went quiet stalls every request
///   through it just the same, and produces exactly that symptom.
///   `response_timeout_ms` is what converts it into a legible error.
/// - A pool's **maximum lifetime** exists because an idle connection a proxy
///   silently dropped looks open and fails on first use. With one socket there
///   is no fresh connection to hand out instead, and the failure is worse than
///   intermittent: it is permanent, because a multiplexed connection does not
///   re-establish itself. `reconnect` is the guard that applies.
///
/// The third failure a pool has — a maximum set per process while the budget is
/// per cluster, so `maxclients` is exceeded and new connections are refused
/// outright while existing ones keep working — is the one this shape simply
/// does not have. A process holds one connection per client, one per node on a
/// cluster, so the budget is the server's `maxclients` divided by the number of
/// processes and no setting here can move it.
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

    /// How long this connection lets a worker hold a job before another may
    /// take it, whatever the driver calls it, and **including the driver's own
    /// default** when the declaration is silent.
    ///
    /// One accessor across four spellings — `reservation`,
    /// `visibility_timeout`, `lease` — because
    /// [`check_reservations`](Connections::check_reservations) is asking a
    /// question none of those names answer on its own: how long until somebody
    /// else can run this job.
    ///
    /// `None` for `sync` and `memory`, which is not "no limit" but "nothing
    /// reclaims": `sync` has already run the job on the calling thread, and
    /// `memory` never hands a reserved one back. Neither can produce the
    /// concurrent second run the check exists to prevent.
    ///
    /// The default comes from the driver's own constant rather than a copy of
    /// it, so the two cannot drift apart into a check that compares against a
    /// number the driver stopped using.
    pub fn reservation_period(&self) -> Option<Duration> {
        match self {
            Self::Sync | Self::Memory => None,

            Self::Database(connection) => {
                Some(connection.reservation.unwrap_or(DatabaseQueue::DEFAULT_RESERVATION))
            }

            // Where the feature is off the declared value is all there is, and
            // that is enough: such a connection cannot be built, so there is no
            // worker for the check to protect. Falling back to a number this
            // build does not contain would be inventing one.
            #[cfg(feature = "redis")]
            Self::Redis(connection) => Some(
                connection.reservation.unwrap_or(crate::redis::RedisQueue::DEFAULT_RESERVATION),
            ),
            #[cfg(not(feature = "redis"))]
            Self::Redis(connection) => connection.reservation,

            #[cfg(feature = "sqs")]
            Self::Sqs(connection) => Some(
                connection.visibility_timeout.unwrap_or(crate::sqs::SqsQueue::DEFAULT_VISIBILITY),
            ),
            #[cfg(not(feature = "sqs"))]
            Self::Sqs(connection) => connection.visibility_timeout,

            #[cfg(feature = "kafka")]
            Self::Kafka(connection) => {
                Some(connection.lease.unwrap_or(crate::kafka::DEFAULT_LEASE))
            }
            #[cfg(not(feature = "kafka"))]
            Self::Kafka(connection) => connection.lease,
        }
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
    /// **Must exceed the worker's job timeout.** Below it, a job that is still
    /// running is reclaimed and handed to a second worker while the first has
    /// it: the job runs twice, concurrently, and neither worker knows. Nothing
    /// reports it, because nothing failed — for anything that sends or charges,
    /// that is the expensive failure.
    ///
    /// [`Connections::check_reservations`] compares this against a worker's
    /// timeout so the mistake is a boot failure rather than a support ticket.
    /// Defaults to [`DatabaseQueue::DEFAULT_RESERVATION`] when not set.
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
///
/// # Not `rainier_drivers::RedisConnection`
///
/// That one is an **open socket**, and this is a declaration of one — sibling
/// to [`SqsConnection`] and [`KafkaConnection`] here, the way `S3Disk` is
/// sibling to `LocalDisk` in the filesystem's section. Both names are right for
/// their own family, so neither is renamed to avoid the other; nothing imports
/// both today, and anything that starts to should alias one at the `use`.
#[derive(Clone)]
pub struct RedisConnection {
    url: String,
    prefix: Option<String>,
    reservation: Option<Duration>,
    connect_timeout: Option<Duration>,
    response_timeout: Option<Duration>,
    reconnect: bool,
    reconnect_attempts: Option<u32>,
    reconnect_max_backoff: Option<Duration>,
}

impl RedisConnection {
    /// A connection to the server at `url` — `redis://host:port/db`, or
    /// `rediss://` for TLS.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            prefix: None,
            reservation: None,
            connect_timeout: None,
            response_timeout: None,
            reconnect: false,
            reconnect_attempts: None,
            reconnect_max_backoff: None,
        }
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
    ///
    /// **Must exceed the worker's job timeout.** Below it, a job that is still
    /// running is claimed by a second worker while the first has it: the job
    /// runs twice, concurrently, and neither worker knows. Nothing reports it,
    /// because nothing failed.
    ///
    /// [`Connections::check_reservations`] compares this against a worker's
    /// timeout so the mistake is a boot failure rather than a support ticket.
    /// Defaults to `RedisQueue::DEFAULT_RESERVATION` when not set.
    pub fn reservation(mut self, reservation: Duration) -> Self {
        self.reservation = Some(reservation);
        self
    }

    /// How long opening the connection may take before it fails.
    ///
    /// Covers the socket *and* the handshake. Without one the wait is however
    /// long the operating system's connect takes, which for a route that goes
    /// nowhere is minutes — and a worker that boots into that waits all of them
    /// before saying anything.
    ///
    /// **There is no pool here to size.** The Redis connection multiplexes: one
    /// socket carries every concurrent command, so this and
    /// [`response_timeout`](Self::response_timeout) are the timeouts that
    /// apply, and there is no `max_connections` because there is nothing to
    /// exhaust. See
    /// [`RedisSettings`](rainier_drivers::RedisSettings) for the whole account.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// How long a command may wait for its reply before it fails.
    ///
    /// A push that never returns is a request that never returns. Without this
    /// a Redis that accepted the command and went quiet holds the dispatching
    /// request open indefinitely, and because the same server is usually also
    /// the cache and the broadcaster, it holds *every* request open at once —
    /// which presents as the whole application being slow rather than as
    /// anything naming Redis.
    pub fn response_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = Some(timeout);
        self
    }

    /// Re-open the connection when its socket is lost.
    ///
    /// **Off unless asked for**, and worth asking for. A multiplexed connection
    /// does not re-establish itself, so a socket dropped by a proxy that had
    /// seen no traffic for a few minutes takes the queue with it: every push
    /// fails, and goes on failing until the process restarts. A worker on a
    /// quiet queue is the likeliest thing in a deployment to be idle long
    /// enough for that.
    ///
    /// One command still fails per loss — that failure is how the connection
    /// finds out — and [`reconnect_attempts`](Self::reconnect_attempts) and
    /// [`reconnect_max_backoff`](Self::reconnect_max_backoff) shape what
    /// happens after it.
    pub fn reconnect(mut self) -> Self {
        self.reconnect = true;
        self
    }

    /// How many times to retry re-establishing the connection.
    ///
    /// Turns reconnection on, since asking for attempts is asking for it.
    /// Left out, the driver's own default applies rather than a number kept
    /// here — a second copy is a second thing to drift.
    pub fn reconnect_attempts(mut self, attempts: u32) -> Self {
        self.reconnect = true;
        self.reconnect_attempts = Some(attempts);
        self
    }

    /// A ceiling on the wait between reconnection attempts.
    ///
    /// Turns reconnection on. Worth setting: the wait doubles each time, so on
    /// a server that stays down for a while the later attempts are minutes
    /// apart, and when it comes back the worker does not — it is asleep until
    /// whatever wait it last started.
    pub fn reconnect_max_backoff(mut self, ceiling: Duration) -> Self {
        self.reconnect = true;
        self.reconnect_max_backoff = Some(ceiling);
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

    /// The connect timeout, if one was declared.
    pub fn connect_timeout_period(&self) -> Option<Duration> {
        self.connect_timeout
    }

    /// The response timeout, if one was declared.
    pub fn response_timeout_period(&self) -> Option<Duration> {
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

    /// Whether this declaration can be built.
    ///
    /// Checked when a declaration is deserialised so a bad section fails while
    /// the configuration is being read, and again when the connection is built
    /// so one assembled in code fails the same way with the same message.
    fn validate(&self) -> Result<()> {
        if !self.reconnect
            && (self.reconnect_attempts.is_some() || self.reconnect_max_backoff.is_some())
        {
            return Err(Error::internal(format!(
                "the `redis` connection for `{}` shapes a reconnection it never asked for: \
                 `reconnect_attempts` and `reconnect_max_backoff` do nothing without \
                 `reconnect`, and a connection that does not reconnect stops working \
                 permanently the first time its socket is dropped. Add `reconnect`, or remove \
                 the settings that imply it",
                self.url_without_credentials()
            )));
        }

        // Delegated rather than repeated, so the driver's account of why a zero
        // timeout is refused is the only one there is. Only reachable with the
        // feature on; without it there is no connection to build and the
        // declaration is refused for that instead.
        #[cfg(feature = "redis")]
        self.settings().validate()?;

        Ok(())
    }

    /// This declaration as the driver's own settings.
    #[cfg(feature = "redis")]
    fn settings(&self) -> rainier_drivers::RedisSettings {
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
    /// When a setting cannot be honoured, when the URL cannot be parsed, or
    /// when the server cannot be reached. The settings are checked first, so a
    /// connection assembled in code fails on them with the same message a
    /// declaration does rather than reaching the server and ignoring them.
    #[cfg(feature = "redis")]
    pub async fn build(&self) -> Result<crate::redis::RedisQueue> {
        use rainier_drivers::RedisConnector;

        self.validate()?;

        // Per connection and never shared. Sharing one connector is the bug
        // this module exists to make impossible: a second connection inheriting
        // the first's server keeps its own *name*, and every job pushed to it
        // waits in a store the worker that was supposed to drain it is not
        // watching. Its settings travel with it for the same reason: two
        // connections that share a server do not share a timeout.
        let connector = RedisConnector::open_with(&self.url, self.settings())?;
        let queue = crate::redis::RedisQueue::connect(&connector).await?;

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
            .field("connect_timeout", &self.connect_timeout)
            .field("response_timeout", &self.response_timeout)
            .field("reconnect", &self.reconnect)
            .field("reconnect_attempts", &self.reconnect_attempts)
            .field("reconnect_max_backoff", &self.reconnect_max_backoff)
            .finish()
    }
}

/// A duration as whole milliseconds, for the wire form.
///
/// Saturating rather than wrapping: a period long enough to overflow a `u64` of
/// milliseconds is half a billion years, and wrapping it would turn "longer
/// than anyone will wait" into a handful of milliseconds — which is the wrong
/// direction for a timeout to be wrong in.
fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
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
    /// SQS's spelling of a reservation, and the same trap. **Must exceed the
    /// worker's job timeout**, or the message reappears while the first worker
    /// is still running it and a second picks it up — the one reliable way to
    /// get a job executed twice, concurrently, with nothing reporting anything.
    ///
    /// [`Connections::check_reservations`] compares this against a worker's
    /// timeout so the mistake is a boot failure rather than a support ticket.
    /// Defaults to `SqsQueue::DEFAULT_VISIBILITY` when not set.
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
    /// When the declaration does not validate — a missing queue URL, or half a
    /// credential pair. Plain text rather than a link: the check is private, and
    /// a doc link to a private item fails the documentation build rather than
    /// rendering as nothing.
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
    /// Kafka's spelling of a reservation. **Must exceed the worker's job
    /// timeout.** If the lease expires while a job is still running, another
    /// worker takes the partition and reads from a cursor that has not moved —
    /// so the job in flight runs twice, concurrently, and neither worker knows.
    ///
    /// Worth setting explicitly here, more than on the other drivers: the
    /// default lease is **shorter** than a worker's default timeout, so a
    /// connection that declares nothing and a worker that declares nothing are
    /// already the misconfiguration.
    /// [`Connections::check_reservations`] says so at boot.
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

    /// The next six are the same idea as `queue`: settings a real
    /// `config/queue.php` carries that this framework cannot honour, declared
    /// here **only so they can be refused by name**.
    ///
    /// `deny_unknown_fields` already stops them, but it stops them with
    /// `unknown field`, which reads as "you misspelled something" and sends the
    /// reader looking for the right spelling of a feature that is not there.
    /// Each of these gets an error that says what the framework does instead,
    /// so the fix is one edit rather than an afternoon.
    ///
    /// `retry_after` is the odd one out: it is not missing, it is
    /// [`reservation`](DatabaseConnection::reservation) under the name Laravel
    /// gives it, and the error says so rather than implying a gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_commit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    block_for: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suffix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection: Option<String>,

    /// Pool sizing, declared here for the same reason as the six above: to be
    /// refused with the reason rather than with `unknown field`. Redis's
    /// connection multiplexes — one socket carries every concurrent command —
    /// so there is nothing to size, and an accepted `max_connections` would
    /// leave a deployment believing it had bounded something it had not.
    ///
    /// `Value` rather than a number, so `max_connections: "ten"` is refused
    /// with the message about pooling instead of one about types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_connections: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_connections: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pool_size: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    reservation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,

    /// Milliseconds, and named so — unlike `reservation`, `wait_time` and
    /// `lease`, which are whole seconds because they are periods a *job*
    /// waits. A command's budget is not: in seconds the only values available
    /// are `0`, which would fail everything, and `1`, which is already longer
    /// than a request can afford to spend pushing a job.
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

    /// Refuse the settings a real `config/queue.php` carries that nothing here
    /// can act on, whatever the driver.
    ///
    /// Separate from [`reject_settings_it_ignores`](Self::reject_settings_it_ignores)
    /// because the reasons are different in kind, and the difference is the
    /// whole point of the message. "The `sqs` driver does not use `url`" tells
    /// somebody they wrote a setting on the wrong connection. "There are no
    /// transactions" tells them the feature does not exist. Collapsing the two
    /// into one wording would send the first reader hunting for the connection
    /// they meant, and the second hunting for a driver that supports it.
    ///
    /// Each of these is refused rather than accepted-and-dropped because a
    /// dropped one leaves the configuration file **asserting something false**:
    /// that jobs wait for a commit, that a worker blocks, that the queue is in
    /// some other table. Every reader after that believes it.
    fn reject_what_cannot_be_honoured(&self) -> Result<()> {
        if self.retry_after.is_some() {
            // Not a gap — a rename. Saying "unsupported" would be a lie that
            // costs somebody the afternoon they spend looking for the feature.
            let Some(instead) = reservation_setting(self.driver) else {
                return Err(Error::internal(format!(
                    "the `{}` driver does not reserve jobs, so it has no `retry_after`: nothing \
                     holds a job that another worker could take",
                    self.driver
                )));
            };

            return Err(Error::internal(format!(
                "this framework spells `retry_after` as `{instead}` on a `{}` connection; it is \
                 the same setting — how long a worker's claim lasts before another may take the \
                 job — so rename the key rather than looking for a feature that is already here",
                self.driver
            )));
        }

        if self.after_commit.is_some() {
            return Err(Error::internal(
                "a queue connection does not take `after_commit`: this framework has no \
                 transaction API, so there is no commit for a dispatch to wait on and no \
                 rollback for it to notice. `true` cannot be honoured, and `false` is the only \
                 behaviour there is — so the key says nothing either way, and is refused rather \
                 than accepted and dropped. An accepted one would leave the configuration \
                 stating in writing that jobs wait for their transaction while they do not, \
                 which is worse than the boot failure you are reading",
            ));
        }

        if self.block_for.is_some() {
            return Err(Error::internal(format!(
                "the `{}` driver does not take `block_for`: no driver here blocks. `reserve` \
                 returns immediately — the Redis one issues `XREADGROUP … COUNT 1` with no \
                 `BLOCK` argument — and the waiting is the worker's, between polls, on its own \
                 `sleep`. Set that instead",
                self.driver
            )));
        }

        let pooling = [
            ("max_connections", self.max_connections.is_some()),
            ("min_connections", self.min_connections.is_some()),
            ("pool_size", self.pool_size.is_some()),
        ];
        let pooling: Vec<String> = pooling
            .iter()
            .filter(|(_, present)| *present)
            .map(|(name, _)| format!("`{name}`"))
            .collect();

        if !pooling.is_empty() {
            return Err(Error::internal(format!(
                "a queue connection does not take {}: no driver here pools. The Redis \
                 connection **multiplexes** — one socket carries every concurrent command, and \
                 the client matches each reply to its request — so a pool would add sockets \
                 without adding throughput, and there is nothing to size. Accepted, this would \
                 leave the configuration stating in writing that connections are bounded when \
                 they are neither bounded nor plural. What the settings around it do instead: \
                 `response_timeout_ms` bounds how long a command waits, which is the failure a \
                 pool's acquire timeout would have caught, and `reconnect` recovers a dropped \
                 socket, which is what recycling by age would have caught",
                pooling.join(", ")
            )));
        }

        if self.connection.is_some() {
            return Err(Error::internal(format!(
                "the `{}` driver does not take `connection`: elsewhere it names a Redis \
                 connection declared in a `database` section, and this framework has no such \
                 section for it to point at. Give the connection its own `url`, and give the \
                 queue a **different database index from the cache's** in that URL's path — \
                 that separation is what `connection` was buying, and flushing the cache \
                 otherwise takes every queued job with it",
                self.driver
            )));
        }

        Ok(())
    }

    /// Refuse a `table`, which the database driver cannot be pointed at.
    ///
    /// Driver-scoped rather than universal because the reason is: on an `sqs`
    /// connection a `table` is simply not that driver's setting, and the
    /// generic message says so better than this one would.
    fn reject_a_table_name(&self) -> Result<()> {
        if self.table.is_none() {
            return Ok(());
        }

        Err(Error::internal(
            "a `database` connection cannot name its `table`: the driver's rows are ORM \
             entities whose table name is fixed at compile time and shared with the migrations \
             that create it, so there is no per-connection name to override. A `table` here \
             would be read and dropped while the driver went on using the entity's own — run \
             `DatabaseQueue::migrations()` and use the tables it creates",
        ))
    }

    /// Refuse `prefix` and `suffix`, which compose a URL this connection is
    /// handed whole.
    fn reject_url_composition(&self) -> Result<()> {
        let mut parts: Vec<&str> = Vec::with_capacity(2);
        if self.prefix.is_some() {
            parts.push("`prefix`");
        }
        if self.suffix.is_some() {
            parts.push("`suffix`");
        }

        if parts.is_empty() {
            return Ok(());
        }

        Err(Error::internal(format!(
            "an `sqs` connection does not take {}: elsewhere they compose a queue's URL as \
             `prefix + queue + suffix`, and this connection is given the whole `queue_url` \
             instead — with no connection-level `queue` there is no middle for them to sit \
             either side of. Dropping them would look right, because the URL you gave is still \
             the queue it reaches; it would go wrong in the deployment whose suffix was the \
             only thing keeping it off another environment's queue. Write the full `queue_url` \
             per environment",
            parts.join(" or ")
        )))
    }

    /// Refuse settings this driver would ignore.
    ///
    /// A `url` on an `sqs` connection is not a harmless extra key — it is
    /// somebody believing these jobs reach the Redis they configured when they
    /// reach a queue in another account entirely. Dropping it silently is how
    /// that belief survives to production, where it looks like a worker that
    /// has stopped picking things up.
    fn reject_settings_it_ignores(&self, used: &[&str]) -> Result<()> {
        // `table` and `suffix` are here as well as in their own checks: those
        // fire only on the driver the setting belongs to, and on any other
        // driver "the `redis` driver does not use `table`" is the truer of the
        // two messages.
        let declared: [(&str, bool); 21] = [
            ("reservation", self.reservation.is_some()),
            ("url", self.url.is_some()),
            ("prefix", self.prefix.is_some()),
            ("connect_timeout_ms", self.connect_timeout_ms.is_some()),
            ("response_timeout_ms", self.response_timeout_ms.is_some()),
            ("reconnect", self.reconnect.is_some()),
            ("reconnect_attempts", self.reconnect_attempts.is_some()),
            ("reconnect_max_backoff_ms", self.reconnect_max_backoff_ms.is_some()),
            ("table", self.table.is_some()),
            ("suffix", self.suffix.is_some()),
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
        // Before the driver is consulted, because these hold whichever one it
        // is — and because a reader who wrote `after_commit` on the wrong
        // connection is better served by "there are no transactions" than by
        // being told to move it somewhere it also would not work.
        raw.reject_a_connection_queue()?;
        raw.reject_what_cannot_be_honoured()?;

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
                raw.reject_a_table_name()?;
                raw.reject_settings_it_ignores(&["reservation"])?;
                Ok(Self::Database(DatabaseConnection {
                    reservation: raw.reservation.map(Duration::from_secs),
                }))
            }

            QueueDriver::Redis => {
                raw.reject_settings_it_ignores(&[
                    "url",
                    "prefix",
                    "reservation",
                    "connect_timeout_ms",
                    "response_timeout_ms",
                    "reconnect",
                    "reconnect_attempts",
                    "reconnect_max_backoff_ms",
                ])?;

                let url = raw.url.ok_or_else(|| {
                    Error::internal(
                        "a `redis` connection needs a `url` to connect to — \
                         `redis://host:port/db`",
                    )
                })?;

                let connection = RedisConnection {
                    url,
                    prefix: raw.prefix,
                    reservation: raw.reservation.map(Duration::from_secs),
                    connect_timeout: raw.connect_timeout_ms.map(Duration::from_millis),
                    response_timeout: raw.response_timeout_ms.map(Duration::from_millis),
                    reconnect: raw.reconnect.unwrap_or(false),
                    reconnect_attempts: raw.reconnect_attempts,
                    reconnect_max_backoff: raw.reconnect_max_backoff_ms.map(Duration::from_millis),
                };
                connection.validate()?;

                Ok(Self::Redis(connection))
            }

            QueueDriver::Sqs => {
                raw.reject_url_composition()?;
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
        // Every refused setting is `None` here and stays that way: they exist
        // to be rejected on the way in, and a connection that was accepted
        // never carries one, so nothing can round-trip back out.
        let blank = |driver| Self {
            driver,
            queue: None,
            retry_after: None,
            after_commit: None,
            block_for: None,
            table: None,
            suffix: None,
            connection: None,
            max_connections: None,
            min_connections: None,
            pool_size: None,
            reservation: None,
            url: None,
            prefix: None,
            connect_timeout_ms: None,
            response_timeout_ms: None,
            reconnect: None,
            reconnect_attempts: None,
            reconnect_max_backoff_ms: None,
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
                connect_timeout_ms: connection.connect_timeout.map(millis),
                response_timeout_ms: connection.response_timeout.map(millis),
                // Written back only when it was asked for, so a round trip does
                // not say more than the original did, and never as `false`,
                // which is the default and would read as a decision.
                reconnect: connection.reconnect.then_some(true),
                reconnect_attempts: connection.reconnect_attempts,
                reconnect_max_backoff_ms: connection.reconnect_max_backoff.map(millis),
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

    // --- what this framework cannot honour ----------------------------------

    #[test]
    fn retry_after_is_refused_by_naming_this_frameworks_spelling_of_it() {
        // Not a gap, a rename. "Unsupported" would cost somebody the afternoon
        // they spend looking for a feature that is already here.
        for (declaration, expected) in [
            (json!({ "driver": "database", "retry_after": 90 }), "reservation"),
            (
                json!({ "driver": "redis", "url": "redis://localhost:6379", "retry_after": 90 }),
                "reservation",
            ),
            (json!({ "driver": "sqs", "queue_url": "u", "retry_after": 90 }), "visibility_timeout"),
            (json!({ "driver": "kafka", "brokers": ["one:9092"], "retry_after": 90 }), "lease"),
        ] {
            let err =
                serde_json::from_value::<ConnectionConfig>(declaration).unwrap_err().to_string();

            assert!(err.contains(&format!("`{expected}`")), "{err}");
            assert!(err.contains("same setting"), "{err}");
        }
    }

    #[test]
    fn retry_after_on_a_driver_that_reserves_nothing_says_that_instead() {
        for driver in ["sync", "memory"] {
            let err = serde_json::from_value::<ConnectionConfig>(
                json!({ "driver": driver, "retry_after": 90 }),
            )
            .unwrap_err()
            .to_string();

            assert!(err.contains("does not reserve jobs"), "{err}");
        }
    }

    #[test]
    fn after_commit_is_refused_whichever_way_it_is_set() {
        // The decision this pins: with no transaction API, `true` cannot be
        // honoured and `false` is the only behaviour there is. Accepting either
        // would leave the configuration file asserting something about
        // transactions that the framework does not do — which is worse than a
        // boot failure, because the next reader has no reason to doubt it.
        for value in [true, false] {
            let err = serde_json::from_value::<ConnectionConfig>(
                json!({ "driver": "database", "after_commit": value }),
            )
            .unwrap_err()
            .to_string();

            assert!(err.contains("`after_commit`"), "{err}");
            assert!(err.contains("no transaction API"), "{err}");
            // Both halves of the decision, so neither can quietly change.
            assert!(err.contains("`true` cannot be honoured"), "{err}");
            assert!(err.contains("`false` is the only behaviour"), "{err}");
        }
    }

    #[test]
    fn block_for_is_refused_because_no_driver_here_blocks() {
        let err = serde_json::from_value::<ConnectionConfig>(
            json!({ "driver": "redis", "url": "redis://localhost:6379", "block_for": 5 }),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("`block_for`"), "{err}");
        assert!(err.contains("BLOCK"), "{err}");
        assert!(err.contains("sleep"), "{err}");
    }

    #[test]
    fn a_table_name_is_refused_on_the_driver_that_would_have_one() {
        let err = serde_json::from_value::<ConnectionConfig>(
            json!({ "driver": "database", "table": "jobs" }),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("compile time"), "{err}");
        assert!(err.contains("migrations"), "{err}");

        // On a driver with no tables at all, the generic message is the truer
        // one — it says the setting is on the wrong connection, which it is.
        let elsewhere = serde_json::from_value::<ConnectionConfig>(
            json!({ "driver": "sqs", "queue_url": "u", "table": "jobs" }),
        )
        .unwrap_err()
        .to_string();
        assert!(elsewhere.contains("does not use"), "{elsewhere}");
        assert!(elsewhere.contains("`table`"), "{elsewhere}");
    }

    #[test]
    fn the_parts_that_would_compose_an_sqs_url_are_refused() {
        // The failure this avoids is the quiet one: dropping them looks right,
        // because the `queue_url` given is still the queue it reaches. It goes
        // wrong in the deployment whose suffix was the only thing keeping it
        // off another environment's queue.
        for declaration in [
            json!({ "driver": "sqs", "queue_url": "u", "prefix": "https://sqs.example.com/0/" }),
            json!({ "driver": "sqs", "queue_url": "u", "suffix": "-production" }),
        ] {
            let err =
                serde_json::from_value::<ConnectionConfig>(declaration).unwrap_err().to_string();

            assert!(err.contains("queue_url"), "{err}");
            assert!(err.contains("prefix + queue + suffix"), "{err}");
        }
    }

    #[test]
    fn a_cross_section_connection_reference_is_refused_and_says_what_it_bought() {
        // There is no named-Redis section anywhere in the framework for this to
        // point at, and inventing one inside the queue crate would leave two
        // notions of "a Redis connection" to disagree with each other.
        let err = serde_json::from_value::<ConnectionConfig>(
            json!({ "driver": "redis", "url": "redis://localhost:6379", "connection": "queue" }),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("`connection`"), "{err}");
        assert!(err.contains("`url`"), "{err}");
        // The reason a dedicated connection mattered, not just that it is gone.
        assert!(err.contains("database index"), "{err}");
        assert!(err.contains("cache"), "{err}");
    }

    #[test]
    fn a_refused_setting_never_round_trips_back_out() {
        // Belt and braces on the wire form: a connection that was accepted
        // carries none of these, so serialising one cannot re-emit a key the
        // reader would take for supported.
        //
        // The list covers every refused key in the section, not only the ones
        // added alongside this test — a key that is refused on the way in but
        // rendered on the way out is a dump that teaches the reader a setting
        // exists, and the next person copies it from there.
        const REFUSED: [&str; 10] = [
            "queue",
            "retry_after",
            "after_commit",
            "block_for",
            "table",
            "suffix",
            "connection",
            "max_connections",
            "min_connections",
            "pool_size",
        ];

        // Every driver, because `blank` fills the refused keys per variant and
        // a missed `None` would only show on the variant that missed it.
        for declaration in [
            json!({ "driver": "sync" }),
            json!({ "driver": "memory" }),
            json!({ "driver": "database", "reservation": 90 }),
            json!({ "driver": "redis", "url": "redis://localhost:6379", "prefix": "app:" }),
            json!({ "driver": "sqs", "queue_url": "u", "region": "us-east-1" }),
            json!({ "driver": "kafka", "brokers": ["one:9092"] }),
        ] {
            let connection: ConnectionConfig = serde_json::from_value(declaration).unwrap();
            let rendered = serde_json::to_value(&connection).unwrap();
            let object = rendered.as_object().expect("an object");

            for refused in REFUSED {
                assert!(!object.contains_key(refused), "{refused} came back out: {rendered}");
            }
        }
    }

    #[test]
    fn every_refused_key_is_refused_on_the_way_in() {
        // The other half of the pair above. Round-tripping cleanly is worth
        // nothing if a key was quietly accepted rather than rejected — the
        // wire form would be silent about a setting that had already been
        // read and dropped.
        for (key, value) in [
            ("queue", json!("default")),
            ("retry_after", json!(90)),
            ("after_commit", json!(true)),
            ("block_for", json!(5)),
            ("table", json!("jobs")),
            ("suffix", json!("-production")),
            ("connection", json!("queue")),
            ("max_connections", json!(50)),
            ("min_connections", json!(1)),
            ("pool_size", json!(10)),
        ] {
            let mut declaration = json!({ "driver": "redis", "url": "redis://localhost:6379" });
            declaration[key] = value;

            assert!(
                serde_json::from_value::<ConnectionConfig>(declaration).is_err(),
                "`{key}` was accepted rather than refused"
            );
        }
    }

    // --- reservations against a worker's timeout ----------------------------

    #[test]
    fn the_defaults_are_a_safe_pair() {
        // A 90s claim outlives a 60s timeout, so nothing an application has not
        // touched is already broken.
        let connections = Connections::new("primary").with("primary", ConnectionConfig::database());

        assert!(connections.check_reservations(&WorkerOptions::default()).is_ok());
    }

    #[test]
    fn a_reservation_the_worker_can_outlive_is_refused() {
        // The classic misconfiguration: the job runs twice, at the same time,
        // and nothing anywhere reports it.
        let connections = Connections::new("primary")
            .with("primary", DatabaseConnection::new().reservation(Duration::from_secs(30)));

        let worker = WorkerOptions::default().timeout(Some(Duration::from_secs(300)));
        let err = connections.check_reservations(&worker).err().expect("30s < 300s");

        assert!(err.message().contains("`primary`"), "{}", err.message());
        assert!(err.message().contains("runs twice"), "{}", err.message());
        assert!(err.message().contains("`reservation`"), "{}", err.message());
    }

    #[test]
    fn an_equal_reservation_and_timeout_is_refused_too() {
        // A job that runs to exactly its timeout races the reclaim, and which
        // side wins is a scheduling detail rather than a guarantee.
        let connections = Connections::new("primary")
            .with("primary", DatabaseConnection::new().reservation(Duration::from_secs(60)));

        let worker = WorkerOptions::default().timeout(Some(Duration::from_secs(60)));
        assert!(connections.check_reservations(&worker).is_err());
    }

    #[test]
    fn a_connection_that_declared_nothing_is_checked_against_the_drivers_own_default() {
        // Otherwise the check would pass exactly the configurations most likely
        // to be wrong: the ones that declared nothing and got a number nobody
        // ever saw.
        let connections = Connections::new("primary").with("primary", ConnectionConfig::database());

        let worker = WorkerOptions::default().timeout(Some(Duration::from_secs(120)));
        let err = connections.check_reservations(&worker).err().expect("90s default < 120s");

        assert!(err.message().contains("90s"), "{}", err.message());
    }

    #[test]
    fn a_worker_with_no_timeout_has_no_safe_reservation() {
        // An unbounded job outlives every finite claim, so there is no number
        // that would make this configuration correct.
        let connections = Connections::new("primary").with("primary", ConnectionConfig::database());

        let worker = WorkerOptions::default().timeout(None);
        let err = connections.check_reservations(&worker).err().expect("no timeout");

        assert!(err.message().contains("no timeout"), "{}", err.message());
    }

    #[test]
    fn the_drivers_that_reclaim_nothing_have_nothing_to_check() {
        // Not "no limit" but "nothing takes it away": `sync` has already run
        // the job, and `memory` never hands a reserved one back.
        for connection in [ConnectionConfig::sync(), ConnectionConfig::memory()] {
            assert_eq!(connection.reservation_period(), None);
        }

        let connections = Connections::new("inline")
            .with("inline", ConnectionConfig::sync())
            .with("scratch", ConnectionConfig::memory());

        let worker = WorkerOptions::default().timeout(None);
        assert!(connections.check_reservations(&worker).is_ok());
    }

    #[test]
    fn every_driver_reports_its_reservation_under_one_name() {
        // Four spellings, one question: how long until somebody else can run
        // this job.
        let declared: Vec<(serde_json::Value, u64)> = vec![
            (json!({ "driver": "database", "reservation": 30 }), 30),
            (json!({ "driver": "redis", "url": "redis://localhost:6379", "reservation": 45 }), 45),
            (json!({ "driver": "sqs", "queue_url": "u", "visibility_timeout": 120 }), 120),
            (json!({ "driver": "kafka", "brokers": ["one:9092"], "lease": 240 }), 240),
        ];

        for (declaration, expected) in declared {
            let connection: ConnectionConfig = serde_json::from_value(declaration).unwrap();
            assert_eq!(
                connection.reservation_period(),
                Some(Duration::from_secs(expected)),
                "{:?}",
                connection.driver()
            );
        }
    }

    #[cfg(feature = "kafka")]
    #[test]
    fn the_kafka_default_lease_is_already_shorter_than_a_default_timeout() {
        // Worth pinning rather than only documenting: this is a default pair
        // that is wrong out of the box, and the check is what makes it visible.
        let connections =
            Connections::new("events").with("events", KafkaConnection::new(["one:9092"]));

        let err = connections
            .check_reservations(&WorkerOptions::default())
            .err()
            .expect("a 60s lease does not outlive a 60s timeout");

        assert!(err.message().contains("`lease`"), "{}", err.message());
    }

    #[test]
    fn a_refusal_names_no_credential() {
        // A configuration that fails to load gets its error logged, so the
        // error is one more rendering that must not carry a secret.
        let err = serde_json::from_value::<ConnectionConfig>(json!({
            "driver": "redis",
            "url": "redis://default:hunter2@cache.internal:6379/0",
            "connection": "queue",
        }))
        .unwrap_err()
        .to_string();

        assert!(!err.contains("hunter2"), "{err}");
    }

    // --- the connection's own timeouts --------------------------------------

    #[test]
    fn a_redis_connection_declares_its_timeouts_and_its_reconnection() {
        let connection: ConnectionConfig = serde_json::from_value(json!({
            "driver": "redis",
            "url": "redis://localhost:6379",
            "connect_timeout_ms": 2000,
            "response_timeout_ms": 250,
            "reconnect": true,
            "reconnect_attempts": 4,
            "reconnect_max_backoff_ms": 1500,
        }))
        .unwrap();

        let ConnectionConfig::Redis(redis) = connection else { panic!("declared as redis") };
        assert_eq!(redis.connect_timeout_period(), Some(Duration::from_secs(2)));
        assert_eq!(redis.response_timeout_period(), Some(Duration::from_millis(250)));
        assert!(redis.reconnects());
        assert_eq!(redis.reconnect_attempt_limit(), Some(4));
        assert_eq!(redis.reconnect_backoff_ceiling(), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn a_connection_that_declares_none_of_them_has_none_of_them() {
        // The half that matters most: a section written before any of this
        // existed has to mean exactly what it meant then, including a
        // connection that does not reconnect.
        let connection: ConnectionConfig =
            serde_json::from_value(json!({ "driver": "redis", "url": "redis://localhost:6379" }))
                .unwrap();

        let ConnectionConfig::Redis(redis) = connection else { panic!("declared as redis") };
        assert_eq!(redis.connect_timeout_period(), None);
        assert_eq!(redis.response_timeout_period(), None);
        assert!(!redis.reconnects());
        assert_eq!(redis.reconnect_attempt_limit(), None);
        assert_eq!(redis.reconnect_backoff_ceiling(), None);
    }

    #[test]
    fn a_reconnection_shaped_but_never_asked_for_is_refused() {
        // Reads as a deployment that thought it had reconnection. It has a
        // connection that stops working permanently the first time a proxy
        // drops its socket, which is the failure this whole setting is about.
        for shaped in [
            json!({ "driver": "redis", "url": "redis://localhost:6379", "reconnect_attempts": 4 }),
            json!({
                "driver": "redis",
                "url": "redis://localhost:6379",
                "reconnect": false,
                "reconnect_max_backoff_ms": 1500,
            }),
        ] {
            let err = serde_json::from_value::<ConnectionConfig>(shaped).unwrap_err().to_string();
            assert!(err.contains("never asked for"), "{err}");
        }
    }

    #[test]
    fn pooling_is_refused_by_name_with_the_reason() {
        // `deny_unknown_fields` would refuse it as a typo, which sends the
        // reader looking for the right spelling of a feature that is not there
        // — and should not be, because there is nothing to pool.
        for pooled in ["max_connections", "min_connections", "pool_size"] {
            let err = serde_json::from_value::<ConnectionConfig>(json!({
                "driver": "redis",
                "url": "redis://localhost:6379",
                pooled: 50,
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
    fn pooling_is_refused_whatever_was_written_in_it() {
        // Typed as a number it would be a type error, which is a worse message
        // than the one about pooling.
        let err = serde_json::from_value::<ConnectionConfig>(json!({
            "driver": "redis",
            "url": "redis://localhost:6379",
            "max_connections": "fifty",
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("no driver here pools"), "{err}");
    }

    #[test]
    fn a_timeout_on_a_driver_that_has_no_such_thing_is_refused() {
        // SQS is HTTP with its own timeouts and no socket of ours to hold, so
        // this setting would be read and dropped.
        let err = serde_json::from_value::<ConnectionConfig>(json!({
            "driver": "sqs",
            "queue_url": "u",
            "response_timeout_ms": 250,
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`response_timeout_ms`"), "{err}");
        assert!(err.contains("does not use"), "{err}");
    }

    #[test]
    fn the_timeouts_round_trip_through_the_wire_form() {
        let original = json!({
            "driver": "redis",
            "url": "redis://localhost:6379",
            "prefix": "app:q:",
            "connect_timeout_ms": 2000,
            "response_timeout_ms": 250,
            "reconnect": true,
            "reconnect_attempts": 4,
            "reconnect_max_backoff_ms": 1500,
        });

        let connection: ConnectionConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&connection).unwrap(), original);
    }

    #[test]
    fn a_connection_without_them_round_trips_without_inventing_them() {
        // In particular it does not write `reconnect: false` back out, which
        // would read as a decision somebody made.
        let original = json!({ "driver": "redis", "url": "redis://localhost:6379" });

        let connection: ConnectionConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&connection).unwrap(), original);
    }

    #[test]
    fn a_connection_renders_its_timeouts_and_still_not_its_password() {
        let connection = RedisConnection::new("redis://default:hunter2@cache.internal:6379/0")
            .response_timeout(Duration::from_millis(250))
            .reconnect_attempts(4);

        let rendered = format!("{connection:?}");

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("250ms"), "{rendered}");
        assert!(rendered.contains("reconnect: true"), "{rendered}");
    }

    #[test]
    fn asking_for_attempts_asks_for_reconnection() {
        // In code there is no "declared but off" to be ambiguous about, so the
        // builder that shapes it turns it on rather than being silently
        // ignored the way the written form would be.
        assert!(RedisConnection::new("redis://localhost:6379").reconnect_attempts(4).reconnects());
        assert!(RedisConnection::new("redis://localhost:6379")
            .reconnect_max_backoff(Duration::from_secs(2))
            .reconnects());
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    async fn a_zero_timeout_is_refused_when_the_connection_is_built() {
        // Deserialising catches it too; this is the same message for a
        // connection assembled in code, which never went through the wire form.
        let connection =
            RedisConnection::new("redis://localhost:6379").response_timeout(Duration::ZERO);

        let err = connection.build().await.err().expect("a zero timeout fails every command");
        assert!(err.message().contains("`response_timeout` of zero"), "{}", err.message());
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
