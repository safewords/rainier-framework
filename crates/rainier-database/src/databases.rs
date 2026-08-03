//! Databases as configuration — [`Databases`], [`DatabaseConfig`],
//! [`ServerDatabase`], [`DsnDatabase`], [`SqliteDatabase`].
//!
//! A [`DatabaseManager`] holds a default database and a map of named ones, and
//! something has to put them there. One `DATABASE_URL` puts exactly one there,
//! which is right for nearly every application and cannot express the ones it
//! is not right for: a read replica, a reporting warehouse, a second database
//! some other system also writes to.
//!
//! Doing it imperatively works until two connections live on **different
//! engines** or under different credentials, at which point the loop that
//! builds them all from one DSN produces a connection with the right *name*
//! pointed at the wrong database.
//!
//! That failure is the quietest of the three sections. A disk pointed at the
//! wrong bucket reads back empty and somebody notices a missing file; a job
//! pushed to the wrong queue is at least never run. A query against the wrong
//! database **answers**. The rows come back, the types match, the report
//! renders, and nothing anywhere raises — because from the database's point of
//! view nothing went wrong. It was asked a question and it replied.
//!
//! So a connection declares **its own** settings, and is built from those
//! alone:
//!
//! ```
//! use rainier_database::{Databases, ServerDatabase, SqliteDatabase};
//!
//! let databases = Databases::new("primary")
//!     .with("primary", ServerDatabase::mysql("app").host("db.example.com").credentials("app", "…"))
//!     .with("reporting", SqliteDatabase::new("storage/reporting.sqlite"));
//!
//! assert_eq!(databases.default_name(), "primary");
//! assert!(databases.get("reporting").is_some());
//! ```
//!
//! ## The same thing, from the configuration tree
//!
//! [`Databases`] deserialises from the shape a `database` section already has —
//! a `default` naming one of the entries in `connections`, and each entry
//! naming its own driver:
//!
//! ```
//! # use rainier_database::Databases;
//! # use serde_json::json;
//! let databases: Databases = serde_json::from_value(json!({
//!     "default": "primary",
//!     "connections": {
//!         "primary": {
//!             "driver": "mysql",
//!             "host": "db.example.com",
//!             "database": "app",
//!             "username": "app",
//!             "password": "…",
//!         },
//!         "replica": { "driver": "postgres", "url": "postgres://reader:…@replica.example.com/app" },
//!         "reporting": { "driver": "sqlite", "database": "storage/reporting.sqlite" },
//!     },
//! })).unwrap();
//!
//! assert_eq!(databases.default_name(), "primary");
//! ```
//!
//! Nothing here is an application's business but the values: the framework
//! names no connection, no host, no database and no environment variable.
//!
//! ## Two ways to write down the same connection
//!
//! Both are supported, because deployments genuinely supply both. A platform
//! that injects one secret injects a **DSN** (`DATABASE_URL`, a Kubernetes
//! secret, a managed-database add-on); a configuration file written by hand
//! names **discrete fields**, because a host you can read is a host you can
//! review.
//!
//! They are two shapes rather than two drivers, which is why they are two
//! variants of [`DatabaseConfig`] and not two spellings of one: a
//! [`ServerDatabase`] has a host to set and a [`DsnDatabase`] does not, and a
//! builder method that silently did nothing would be the same silent-ignoring
//! failure this module exists to refuse, one level up.
//!
//! What is *not* supported is both at once. Laravel lets a `url` win over the
//! rest, and that is the resolution this refuses: the setting that loses is
//! still sitting there being read by whoever changes it next, and a connection
//! whose visible `host` is not the host it uses is a change that appears to
//! work and does nothing. Declaring both is a boot failure naming the two.
//!
//! ## What a declaration refuses
//!
//! Every rejection below is a case where accepting the declaration would give a
//! working-looking connection that reads or writes a database other than the
//! one intended, so each is a boot failure instead:
//!
//! | Declaration | Why it is refused |
//! |---|---|
//! | no `driver` | an assumed driver is a connection pointed at whatever the default happens to be |
//! | `host` on a `sqlite` connection | somebody believes these rows reach a server; they reach a file that goes away with the container |
//! | `url` beside `host`, `database`, `username` or `password` | a DSN carries those inline, so one of the two is ignored — and which one is not visible from the file |
//! | a server connection with no `host` | a guessed host is a different database, and `localhost` is one that very often exists |
//! | a server connection with no `database` | connecting without one lands in whatever the server calls that user's default |
//! | `password` without `username` | the password is dropped and the connection authenticates as the ambient user — as somebody else |
//! | a `sqlite` connection with no `database` | there is no file to open, and an assumed one is an empty database that migrates cleanly |
//! | a DSN whose scheme no driver speaks | choosing one on the deployment's behalf is choosing the wrong database in silence |
//! | `default` naming an undeclared connection | the fallback would be silent, and the wrong database |
//! | `charset`, `collation` or `strict` on a driver with no such setting | the driver drops it, and the connection negotiates whatever it would have anyway |
//! | `collation` with no `charset` | a collation orders **one** character set; alone it is matched against whichever one the driver assumes |
//! | `unix_socket` beside `host` or `port` | a socket connection never dials a host, so the host in the file is read by everyone and used by nothing |
//! | a `read` or `write` role that resolves to no host | the role has nowhere to go, and inheriting the *other* role's host would put writes on a replica or reads on a primary without saying so |
//! | an empty `read` or `write` role | it says nothing the connection does not already say, and reads as a split that is not one |
//! | `sticky` with neither `read` nor `write` | there is one endpoint, so there is nothing a read could be stale against |
//! | `read` or `write` beside `unix_socket` | a socket reaches one server on this machine; a role names another one |
//! | a role naming its own `driver`, `database` or `unix_socket` | a replica of a *different* database answers every query, correctly, about the wrong rows |
//! | an `options` key the driver's own URL parser does not read | it is dropped on arrival, and the connection is not configured the way the file says |
//! | an `options` key some other setting already settles | two spellings of one answer, and which one wins is not visible from the file |
//! | an empty `pool` | it names nothing, so it changes nothing, and reads as sizing that was applied |
//! | `max_connections` of `0`, or a `min_connections` above the maximum | a pool that can hand out nothing, and a floor above its own ceiling |
//! | `acquire_timeout` of `0` | every query fails the moment every connection is busy, which is a normal condition and not an error |
//! | a `pool` on an in-memory SQLite database that is not a pool of exactly one, kept forever | the database *is* the connection: a second one is a second, empty database, and reaping the first drops the schema |
//! | `prefix` or `prefix_indexes` | not supported at all — see below, and a half-applied prefix reads as missing data |
//! | `engine` | nothing renders it, and a setting the database never sees is worse than one that was refused |
//!
//! ## Two of these settings are about data, not connectivity
//!
//! Everything above is about reaching the *right* database. `charset` and
//! `strict` are about what happens to a value once it gets there, and both fail
//! by **storing something other than what was sent**:
//!
//! **`charset`.** MySQL's `utf8` is three bytes wide. A connection negotiating
//! it does not reject an emoji, or a good deal of CJK text, or a mathematical
//! symbol — it truncates the value at the first four-byte character, or
//! replaces it, and stores the row. Nothing raises, the write succeeds, and the
//! text is short. `utf8mb4` is the one that holds all of Unicode. The framework
//! still declares no default: an assumed character set is an assumption about
//! every existing row in a database this connection did not create, so an
//! undeclared `charset` leaves the driver and the server to settle it exactly
//! as they did before.
//!
//! **`strict`.** MySQL's strict mode decides whether a value too long or out of
//! range for its column is an **error** or a **truncation**. Non-strict is the
//! dangerous direction, and it is dangerous in the same way: the `INSERT`
//! returns success and the stored value is not the one that was sent. Left
//! undeclared, the server's own `sql_mode` decides — which is not a safe
//! default so much as an unknown one, because a managed database's parameter
//! group is a place strict mode routinely gets turned off. Declaring `strict`
//! settles it for every connection in the pool rather than for whichever
//! connection happened to be checked out.
//!
//! ## Splitting reads from writes
//!
//! A connection may name a `read` role and a `write` role, each with its own
//! hosts and, if it needs them, its own credentials. Everything a role does not
//! name it takes from the connection around it, so the common case is short:
//!
//! ```
//! # use rainier_database::Databases;
//! # use serde_json::json;
//! let databases: Databases = serde_json::from_value(json!({
//!     "default": "primary",
//!     "connections": {
//!         "primary": {
//!             "driver": "mysql",
//!             "host": "writer.example.com",
//!             "read": { "host": ["replica-a.example.com", "replica-b.example.com"] },
//!             "sticky": true,
//!             "database": "app",
//!             "username": "app",
//!             "password": "…",
//!         },
//!     },
//! })).unwrap();
//!
//! assert!(databases.get("primary").unwrap().is_split());
//! ```
//!
//! A connection that names neither role is one connection and behaves exactly
//! as it did before any of this existed — same endpoint, same pool, same
//! connection string.
//!
//! **Which endpoint a statement reaches is decided by the method that ran it**,
//! not by reading the SQL: a fetch reads and an execute writes. Every host of
//! a role is opened at boot and they are used in turn, round-robin — see
//! [`Database::with_endpoints`](crate::Database::with_endpoints) for why that
//! rather than the random pick this shape is ported from.
//!
//! `sticky` is the one setting here whose failure is a *data* failure, and it
//! has a module of its own: a read that follows a write can otherwise land on a
//! replica that has not caught up and answer from before the write, with no
//! error anywhere. [`sticky`](crate::sticky) documents what a scope is, what it
//! covers, and — importantly before declaring it — what a sticky connection
//! does when nothing has entered one.
//!
//! ## Sizing the pool
//!
//! A connection may declare a `pool`, and so may either of its roles. Every
//! field is optional and an absent one keeps the value the connection would
//! have had anyway, so a declaration says only what it is changing:
//!
//! ```
//! # use rainier_database::Databases;
//! # use serde_json::json;
//! let databases: Databases = serde_json::from_value(json!({
//!     "default": "primary",
//!     "connections": {
//!         "primary": {
//!             "driver": "postgres",
//!             "host": "writer.example.com",
//!             "database": "app",
//!             "pool": { "max_connections": 8, "acquire_timeout": 5 },
//!             "read": { "host": "replica.example.com", "pool": { "max_connections": 20 } },
//!         },
//!     },
//! })).unwrap();
//!
//! assert_eq!(databases.get("primary").unwrap().pool().max_connections, 8);
//! assert_eq!(databases.get("primary").unwrap().read_pool().max_connections, 20);
//! ```
//!
//! Durations are whole seconds, which is this family's spelling. `0` means
//! *never* for the two that can be disabled — `idle_timeout` and `max_lifetime`
//! — and is refused for the two where it means "give up instantly".
//!
//! There is deliberately no preset to name here.
//! [`PoolConfig::serverless`](rainier_orm::PoolConfig::serverless) is
//! expressible field by field, and a preset *name* in a configuration file is a
//! value whose meaning moves when the library changes underneath it — where six
//! numbers are six things a review can check against the database in front of
//! it.
//!
//! **The roles are sized separately because they are sized differently.** A
//! primary takes writes from every process and its connection budget is the
//! scarce one; replicas take the read traffic and there are usually several of
//! them. A role that declares no `pool` takes the connection's, so the common
//! case stays one block.
//!
//! Three of these are worth reading before setting them, because each fails in
//! a way that does not look like a pool problem:
//!
//! **`max_connections` is a share of a budget, not a limit on this process.**
//! The database accepts some total number of connections and every app process
//! opens up to its own maximum, so the number to write down is the database's
//! budget divided by the process count — and on a split connection, divided
//! again by the number of hosts in the role, because each host is its own pool.
//! Too high does not show up as slowness: the processes that started first keep
//! working and the next one to start is refused outright, which reads as a
//! partial outage rather than as a setting.
//!
//! **`acquire_timeout` chooses which failure saturation produces.** Too short
//! and requests fail while the database is healthy and merely busy. Too long and
//! they queue past the point the caller gave up, so the pool spends its capacity
//! on work whose answer nobody is waiting for — which keeps the queue full and
//! is how a brief spike becomes a sustained one.
//!
//! **`max_lifetime` is the guard against a connection that is not there.** A
//! load balancer or a database that drops long-lived connections leaves the pool
//! holding sockets that look open and fail on first use, so the failures land on
//! whichever query happened to draw a dead one. Recycling on an age is what
//! stops that presenting as intermittent errors nobody can reproduce.
//!
//! ## What this section deliberately does not carry
//!

//! **A table prefix.** Not supported, and refused rather than accepted,
//! because it cannot be applied *everywhere* a table name is rendered.
//! `Entity::table()` is a `&'static str` with no connection in scope; a
//! foreign key names its parent table as a string; a migration step and
//! [`Database::statement`](crate::Database::statement) take SQL already
//! written. A prefix reaching the first of those and not the rest is the worst
//! outcome available: some statements hit prefixed tables and some hit
//! unprefixed ones, and a query against a table that exists but is not the one
//! holding the rows comes back **empty** rather than failing. That reads as
//! missing data, and it is the same silent-wrong-database failure the rest of
//! this module exists to refuse.
//!
//! The place it stops being a matter of effort is Rainier ORM. A
//! [`Database`] *is* an `Executor`, so `repo::query::<E>()`
//! and the whole `repo::` surface render `E::table()` inside a crate that has
//! never heard of this section and takes no prefix — and they are a documented,
//! first-class way to query. Threading a prefix through every table name
//! [`statement`](crate::statement) renders would therefore still leave half the
//! framework's queries unprefixed, which is precisely the split-brain outcome
//! above rather than a step towards avoiding it. If it is wanted it belongs in
//! the ORM, where every table name is rendered, and not here.
//!
//! **`engine`.** MySQL's table engine is a `CREATE TABLE` clause, and nothing
//! between a declaration and the schema builder carries it: a migration renders
//! from a [`Dialect`] and never sees the connection's configuration. Accepting
//! it would put a value in the file that the database never hears, which is
//! strictly worse than refusing it — the configuration would say `InnoDB` while
//! the server used whatever its default is, and the only way to find out would
//! be to look at the table.
//!
//! **Anything `options` names that the driver would not read.** `options` is
//! the escape hatch for a driver parameter this section has no field for, and
//! it is an allow-list rather than a passthrough. The reason is what the
//! driver does with a parameter it does not recognise: sqlx's MySQL URL parser
//! ignores it outright and its PostgreSQL one logs and moves on. Neither
//! fails. So a passthrough would let a file say `sslmode=verify-full` under a
//! spelling the driver does not read, and the connection would be established
//! unverified — with the setting sitting in the file, reviewed, and doing
//! nothing. Only the keys that engine's own parser reads are accepted, and a
//! key some other setting on this connection already settles is refused as the
//! second answer it would be.
//!
//! **The `d1` and `libsql` drivers.** Their executors are generic over a
//! caller-supplied transport — a `fetch` binding inside a Worker, an HTTP
//! client on a server — and a transport is not a value a configuration tree can
//! hold. They are built in code and registered with
//! [`DatabaseManager::with_connection`], the same way the queue section takes
//! what a file cannot hold through `QueueResources`.

use std::collections::BTreeMap;

use rainier_orm::Dialect;
use rainier_support::{setting_enum, Error, Result};
use serde::{Deserialize, Serialize};

use crate::connection::Database;
use crate::manager::DatabaseManager;

setting_enum! {
    /// Which engine a declared connection speaks.
    ///
    /// ```
    /// use rainier_database::DatabaseDriver;
    /// use rainier_support::Setting;
    ///
    /// // MariaDB speaks MySQL's protocol; it is not a fourth driver.
    /// assert_eq!(DatabaseDriver::parse("mysql").unwrap(), DatabaseDriver::MySql);
    /// ```
    ///
    /// These are the three the shipped executors support. Laravel spells the
    /// third `pgsql` after the PHP extension; this spells it `postgres` after
    /// the engine, matching [`Dialect::Postgres`] and the `postgres://` scheme
    /// the rest of the ecosystem uses. A section ported from Laravel fails on
    /// the name rather than silently choosing something else.
    pub enum DatabaseDriver: "database driver" {
        /// SQLite — a file on this machine, or `:memory:`.
        ///
        /// The default in the sense the closed set needs one, never in the
        /// sense of a substitution: `driver` is required on every declaration.
        #[default]
        Sqlite = "sqlite",

        /// MySQL, and MariaDB, which speaks the same wire protocol.
        MySql = "mysql",

        /// PostgreSQL.
        Postgres = "postgres",
    }
}

impl DatabaseDriver {
    /// The dialect SQL for this engine renders in.
    pub fn dialect(&self) -> Dialect {
        match self {
            Self::Sqlite => Dialect::Sqlite,
            Self::MySql => Dialect::MySql,
            Self::Postgres => Dialect::Postgres,
        }
    }

    /// Whether this engine is reached over a network.
    ///
    /// `false` for [`Sqlite`](Self::Sqlite), which is the distinction that
    /// decides which settings a declaration may carry: a file has no host, no
    /// port and nobody to authenticate to.
    pub fn is_server(&self) -> bool {
        !matches!(self, Self::Sqlite)
    }

    /// The port this engine listens on when a declaration does not say.
    ///
    /// A convention of the engine rather than a guess about the deployment —
    /// unlike a host, which is refused when it is missing. `None` for SQLite.
    pub fn default_port(&self) -> Option<u16> {
        match self {
            Self::Sqlite => None,
            Self::MySql => Some(3306),
            Self::Postgres => Some(5432),
        }
    }

    /// The URL scheme a DSN for this engine is written with.
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::MySql => "mysql",
            Self::Postgres => "postgres",
        }
    }

    /// The driver a DSN's scheme names, or `None` if no driver claims it.
    ///
    /// Deliberately not a fallback to [`Sqlite`](Self::Sqlite): a `postgress://`
    /// typo that quietly became a local file would migrate cleanly, answer
    /// every query with no rows, and look like an empty database rather than a
    /// misconfigured one.
    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme.trim().to_ascii_lowercase().as_str() {
            "sqlite" => Some(Self::Sqlite),
            // MariaDB's own tooling emits `mariadb://`, and it is the same
            // protocol — accepted as a spelling, not as a fourth driver.
            "mysql" | "mariadb" => Some(Self::MySql),
            "postgres" | "postgresql" => Some(Self::Postgres),
            _ => None,
        }
    }
}

/// The connections an application declares, and which of them is the default.
///
/// The `database` section, as a type. Deserialises from the configuration tree
/// and builds a [`DatabaseManager`] in one call, so declaring a replica is a
/// config edit rather than a line of wiring.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Databases {
    /// Which entry of `connections` a query naming none runs against.
    #[serde(default = "conventional_default")]
    default: String,

    /// Every declared connection, by the name callers reach it with.
    ///
    /// A `BTreeMap` so a dump and a build order are stable — a `HashMap` would
    /// make an error that lists the declared connections read differently each
    /// run.
    #[serde(default)]
    connections: BTreeMap<String, DatabaseConfig>,
}

/// The connection name assumed when a `database` section does not say.
///
/// The siblings default to the name of a driver that needs no infrastructure —
/// `local`, `sync`. There is no such database driver: every connection names a
/// DSN or a host, and a `DATABASE_URL` may name any engine, so a convention of
/// `sqlite` would have `DATABASE_URL=postgres://…` declare a connection called
/// `sqlite`. The neutral name is the one that is never a lie.
///
/// A `default` naming a connection that is not declared fails at
/// [`build`](Databases::build) rather than falling back.
fn conventional_default() -> String {
    Databases::DEFAULT_NAME.to_string()
}

impl Databases {
    /// The name [`from_url`](Self::from_url) declares its one connection under,
    /// and what `default` is when a section does not say.
    pub const DEFAULT_NAME: &'static str = "default";

    /// An empty set whose default connection will be `default`.
    ///
    /// The name has to be declared with [`with`](Self::with) before
    /// [`build`](Self::build) will succeed.
    pub fn new(default: impl Into<String>) -> Self {
        Self { default: default.into(), connections: BTreeMap::new() }
    }

    /// One connection, from one DSN — what `DATABASE_URL` declares.
    ///
    /// The single-connection application, which is nearly all of them, in one
    /// call and with no section to write. The driver comes from the URL's
    /// scheme, so `mysql://…`, `postgres://…` and `sqlite://…` each land on
    /// their own engine, and a scheme no driver claims is an error rather than
    /// a fallback.
    ///
    /// The connection is declared under [`DEFAULT_NAME`](Self::DEFAULT_NAME)
    /// *and* is the default, so it is reachable both ways and is one handle
    /// either way.
    ///
    /// # Errors
    ///
    /// When `url` has no scheme, or a scheme no driver speaks. The message
    /// never contains the URL — see [`DsnDatabase::from_url`].
    pub fn from_url(url: &str) -> Result<Self> {
        Ok(Self::new(Self::DEFAULT_NAME).with(Self::DEFAULT_NAME, DsnDatabase::from_url(url)?))
    }

    /// Declare a connection under `name`.
    pub fn with(mut self, name: impl Into<String>, database: impl Into<DatabaseConfig>) -> Self {
        self.connections.insert(name.into(), database.into());
        self
    }

    /// The name of the connection that will be the default.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// The declaration filed under `name`.
    pub fn get(&self, name: &str) -> Option<&DatabaseConfig> {
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

    /// Open every declared connection and assemble them into a
    /// [`DatabaseManager`].
    ///
    /// Each connection is opened from **its own** declaration. There is no
    /// shared DSN to inherit from, which is the entire point: two connections
    /// on two engines with two credential sets are two databases, and the
    /// version of this that built them from one URL produced a second
    /// connection with the right name reading the wrong rows — and answering,
    /// rather than failing.
    ///
    /// A connection is opened **once** and registered under its name *and*, if
    /// it is the default, as the default. Opening it twice would give
    /// `DatabaseManager::connection("primary")` a second pool for the same
    /// declaration — invisible on a server, and for `sqlite::memory:` a second
    /// *database*, empty, because such a database exists only as long as the
    /// connection holding it.
    ///
    /// The default name is checked before anything is opened, so a typo fails
    /// immediately instead of after connecting to databases that were never
    /// going to be used.
    ///
    /// # Connecting at boot rather than at first query
    ///
    /// Every declared connection is opened here, so a replica that is down
    /// stops the process from starting. That is the direction chosen
    /// deliberately, and it is the same one the disk and queue sections take: a
    /// handle that has not connected is a handle that might not be a database,
    /// and the alternative moves every DSN mistake from a boot failure a deploy
    /// can catch to a runtime failure at whatever hour the query first runs.
    ///
    /// # Errors
    ///
    /// When `default` names a connection that is not declared, when a
    /// declaration does not make sense, when no executor was compiled in, or
    /// when a database refuses the connection.
    pub async fn build(&self) -> Result<DatabaseManager> {
        if !self.connections.contains_key(&self.default) {
            return Err(Error::internal(format!(
                "the default database connection `{}` is not declared; declared connections are {}",
                self.default,
                self.declared()
            )));
        }

        let mut opened: Vec<(&str, Database)> = Vec::with_capacity(self.connections.len());
        for (name, connection) in &self.connections {
            let database = connection.build().await.map_err(|e| {
                Error::internal(format!("database connection `{name}`: {}", e.message()))
            })?;
            opened.push((name, database));
        }

        let default = opened
            .iter()
            .find(|(name, _)| *name == self.default)
            .map(|(_, database)| database.clone())
            .expect("the default was checked against the same map");

        let mut manager = DatabaseManager::new(default);
        for (name, database) in opened {
            manager = manager.with_connection(name, database);
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

impl std::fmt::Debug for Databases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Databases")
            .field("default", &self.default)
            .field("connections", &self.connections)
            .finish()
    }
}

/// How a connection's pool is sized, as a declaration rather than as a
/// [`PoolConfig`](rainier_orm::PoolConfig).
///
/// Every field is optional, and that is the whole design: an absent one keeps
/// the value the connection would have had with no `pool` at all. A declaration
/// therefore says only what it is changing, and — the part that matters — a
/// connection that declares nothing is sized exactly as it was before any of
/// this was declarable.
///
/// ```
/// use rainier_database::PoolSettings;
///
/// // Everything else stays as it was.
/// let settings = PoolSettings::new().max_connections(25);
/// assert_eq!(settings.max().unwrap(), 25);
/// assert!(settings.acquire_timeout_period().is_none());
/// ```
///
/// The three settings whose failures do not look like pool failures —
/// `max_connections`, `acquire_timeout` and `max_lifetime` — are written up in
/// this module's header rather than repeated on each method.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_connections: Option<u32>,
    /// Seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acquire_timeout: Option<u64>,
    /// Seconds; `0` disables reaping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_timeout: Option<u64>,
    /// Seconds; `0` disables recycling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_lifetime: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test_before_acquire: Option<bool>,
}

impl PoolSettings {
    /// Change nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// The ceiling on connections **this process** opens to **this endpoint**.
    ///
    /// A share of the database's budget rather than a limit on this process —
    /// see this module's header for what going over it looks like, which is not
    /// slowness.
    pub fn max_connections(mut self, connections: u32) -> Self {
        self.max_connections = Some(connections);
        self
    }

    /// Connections kept open while idle. `0` lets the pool drain between
    /// bursts.
    pub fn min_connections(mut self, connections: u32) -> Self {
        self.min_connections = Some(connections);
        self
    }

    /// How long a query waits for a free connection before failing, in seconds.
    ///
    /// Chooses which failure saturation produces; `0` is refused, because
    /// failing the instant every connection is busy makes an error out of a
    /// normal condition.
    pub fn acquire_timeout(mut self, seconds: u64) -> Self {
        self.acquire_timeout = Some(seconds);
        self
    }

    /// Close a connection idle for longer than this, in seconds. `0` never
    /// reaps.
    pub fn idle_timeout(mut self, seconds: u64) -> Self {
        self.idle_timeout = Some(seconds);
        self
    }

    /// Recycle a connection older than this regardless of use, in seconds. `0`
    /// never recycles.
    ///
    /// The guard against a socket that is open here and closed at the other
    /// end — see this module's header.
    pub fn max_lifetime(mut self, seconds: u64) -> Self {
        self.max_lifetime = Some(seconds);
        self
    }

    /// Ping a connection before handing it out. Costs a round trip and
    /// guarantees liveness.
    pub fn test_before_acquire(mut self, test: bool) -> Self {
        self.test_before_acquire = Some(test);
        self
    }

    /// The declared ceiling, when one was declared.
    pub fn max(&self) -> Option<u32> {
        self.max_connections
    }

    /// The declared floor, when one was declared.
    pub fn min(&self) -> Option<u32> {
        self.min_connections
    }

    /// The declared acquire timeout, when one was declared.
    pub fn acquire_timeout_period(&self) -> Option<std::time::Duration> {
        self.acquire_timeout.map(std::time::Duration::from_secs)
    }

    /// The declared idle timeout: `Some(None)` is "never reap".
    pub fn idle_timeout_period(&self) -> Option<Option<std::time::Duration>> {
        self.idle_timeout.map(disableable)
    }

    /// The declared maximum lifetime: `Some(None)` is "never recycle".
    pub fn max_lifetime_period(&self) -> Option<Option<std::time::Duration>> {
        self.max_lifetime.map(disableable)
    }

    /// Whether a connection is pinged before it is handed out, when declared.
    pub fn tests_before_acquire(&self) -> Option<bool> {
        self.test_before_acquire
    }

    /// Whether this declares anything at all.
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// This declaration over `base`, field by field.
    ///
    /// `base` is what the connection would have used with no `pool` declared,
    /// which is what makes an absent field mean "leave it alone" rather than
    /// "take the library's default": an in-memory SQLite database that declares
    /// only `acquire_timeout` keeps every one of the four settings that make it
    /// survive.
    fn applied_to(&self, mut base: rainier_orm::PoolConfig) -> rainier_orm::PoolConfig {
        if let Some(max) = self.max_connections {
            base.max_connections = max;
        }
        if let Some(min) = self.min_connections {
            base.min_connections = min;
        }
        if let Some(seconds) = self.acquire_timeout {
            base.acquire_timeout = std::time::Duration::from_secs(seconds);
        }
        if let Some(seconds) = self.idle_timeout {
            base.idle_timeout = disableable(seconds);
        }
        if let Some(seconds) = self.max_lifetime {
            base.max_lifetime = disableable(seconds);
        }
        if let Some(test) = self.test_before_acquire {
            base.test_before_acquire = test;
        }
        base
    }
}

/// A declared duration where `0` means "never".
///
/// The two settings that take one are `Option<Duration>` in the pool and there
/// is no other way to write `None` down. `0` is the honest spelling: a timeout
/// of no time at all is not a thing anybody wants, so the value is free to mean
/// the only other thing it could.
fn disableable(seconds: u64) -> Option<std::time::Duration> {
    match seconds {
        0 => None,
        seconds => Some(std::time::Duration::from_secs(seconds)),
    }
}

/// The pool a connection has when it declares none.
///
/// The choice this section made before a pool was declarable, unchanged: one
/// case is not tuning, and it is the silent one. An in-memory SQLite database
/// exists only as long as the connection holding it, so a second pooled
/// connection is a second, *empty* database and a query landing on it returns
/// no rows rather than an error.
fn base_pool(in_memory: bool) -> rainier_orm::PoolConfig {
    if in_memory {
        rainier_orm::PoolConfig::in_memory()
    } else {
        rainier_orm::PoolConfig::default()
    }
}

/// Refuse a resolved pool that cannot work, naming `what` it belongs to.
///
/// Checked against the **resolved** pool rather than the declaration, because
/// the interesting mistake is a cross-field one: `min_connections: 20` is
/// perfectly reasonable text and is a floor above a ceiling nobody restated.
fn check_pool(pool: &rainier_orm::PoolConfig, what: &str) -> Result<()> {
    if pool.max_connections == 0 {
        return Err(Error::internal(format!(
            "the {what} pool declares `max_connections: 0`; a pool that may open no connections \
             has nothing to hand a query, so every statement waits for the acquire timeout and \
             then fails"
        )));
    }
    if pool.min_connections > pool.max_connections {
        return Err(Error::internal(format!(
            "the {what} pool declares `min_connections` of {} above its `max_connections` of {}; \
             the floor cannot be higher than the ceiling, and only one of the two is what somebody \
             meant",
            pool.min_connections, pool.max_connections
        )));
    }
    if pool.acquire_timeout.is_zero() {
        return Err(Error::internal(format!(
            "the {what} pool declares `acquire_timeout: 0`; every connection being busy is a \
             normal condition under load rather than an error, and a pool that gives up instantly \
             turns the first burst of traffic into failed requests against a database that is \
             healthy. Write the number of seconds a caller is willing to wait"
        )));
    }
    Ok(())
}

/// A `pool` that names nothing.
///
/// Refused rather than ignored for the reason every empty block here is: it
/// reads, to whoever opens the file next, as sizing that was thought about and
/// applied.
fn empty_pool() -> Error {
    Error::internal(
        "this connection declares an empty `pool`; it names no size and no timeout, so it changes \
         nothing — while reading as sizing that took effect",
    )
}

/// Refuse a pool an in-memory SQLite database would not survive.
///
/// Every one of these is silent rather than loud, which is why they are refused
/// rather than left to whoever sized it: the database *is* the connection, so a
/// second connection is a second and empty database, and closing the one that
/// exists takes the schema with it. The symptom is a process that migrates
/// cleanly at boot and answers `no such table` to the first real request.
fn check_in_memory_pool(pool: &rainier_orm::PoolConfig) -> Result<()> {
    let refusal = |setting: &str, because: &str| {
        Err(Error::internal(format!(
            "this connection is an in-memory SQLite database and its pool declares {setting}; \
             {because}. An in-memory database exists only as long as the connection holding it, so \
             its pool is exactly one connection kept for the life of the process — declare a file \
             path instead if this needs to be a real pool"
        )))
    };

    if pool.max_connections != 1 {
        return refusal(
            "more than one connection",
            "the second one opens a second, empty database",
        );
    }
    if pool.min_connections != 1 {
        return refusal(
            "a floor below one",
            "the pool closes its only connection while nothing is happening, and the schema goes \
             with it",
        );
    }
    if pool.idle_timeout.is_some() {
        return refusal(
            "an `idle_timeout`",
            "the connection is reaped between queries and every table disappears with it",
        );
    }
    if pool.max_lifetime.is_some() {
        return refusal(
            "a `max_lifetime`",
            "the connection is recycled on an age and the replacement is an empty database",
        );
    }
    Ok(())
}

/// One connection: which driver, and the settings that driver needs.
///
/// An enum rather than a struct of optionals, so the settings a shape does not
/// have cannot be written down: there is no `host` on a SQLite connection to
/// fill in and wonder why it is ignored, and no `host` on a DSN to set and
/// wonder why nothing changed. The wire form is still flat — `driver` beside
/// the rest — because that is what a configuration file wants to be.
///
/// The variants are the three **shapes** a connection is written in rather than
/// the three drivers, because that is what actually differs. MySQL and Postgres
/// share one set of settings exactly, down to the last field; splitting them
/// would duplicate a struct to encode a difference that lives entirely in
/// [`DatabaseDriver`] — the scheme, the default port and the dialect. A DSN, on
/// the other hand, genuinely is a different shape whichever engine it names:
/// one opaque string that already contains the host, the database and the
/// credential.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "RawDatabase", into = "RawDatabase")]
#[allow(
    clippy::large_enum_variant,
    reason = "a server declaration carries every setting a server has and is far larger than a \
              DSN, which is one string. There are as many of these as an application declares \
              connections — usually one — and each is built once at boot and then left alone, so \
              the indirection the lint suggests would buy a few hundred bytes at startup and cost \
              a pointer chase on a public pattern match"
)]
pub enum DatabaseConfig {
    /// A database on a server, from discrete fields.
    Server(ServerDatabase),

    /// A SQLite file on this machine, or `:memory:`.
    Sqlite(SqliteDatabase),

    /// Any driver, from one connection string.
    Dsn(DsnDatabase),
}

impl DatabaseConfig {
    /// A SQLite database in the file at `path`.
    pub fn sqlite(path: impl Into<String>) -> Self {
        Self::Sqlite(SqliteDatabase::new(path))
    }

    /// A SQLite database in memory, for tests.
    ///
    /// Private to the connection that opens it and gone when that connection
    /// closes, which is why it is opened with
    /// [`PoolConfig::in_memory`](rainier_orm::PoolConfig::in_memory) — see
    /// [`pool`](Self::pool).
    pub fn sqlite_in_memory() -> Self {
        Self::Sqlite(SqliteDatabase::in_memory())
    }

    /// One connection, from one DSN — see [`DsnDatabase::from_url`].
    ///
    /// # Errors
    ///
    /// When `url` has no scheme, or one no driver speaks.
    pub fn from_url(url: &str) -> Result<Self> {
        Ok(Self::Dsn(DsnDatabase::from_url(url)?))
    }

    /// Which driver this declares.
    pub fn driver(&self) -> DatabaseDriver {
        match self {
            Self::Server(server) => server.driver(),
            Self::Sqlite(_) => DatabaseDriver::Sqlite,
            Self::Dsn(dsn) => dsn.driver(),
        }
    }

    /// The dialect this connection's SQL renders in.
    pub fn dialect(&self) -> Dialect {
        self.driver().dialect()
    }

    /// This connection as a URL, with anything secret removed.
    ///
    /// The only rendering of a connection that is safe to log, and the one
    /// [`Debug`](std::fmt::Debug) uses. Enough to tell two connections apart,
    /// and not enough to authenticate with.
    pub fn url_without_credentials(&self) -> String {
        match self {
            Self::Server(server) => server.url_without_credentials(),
            Self::Sqlite(sqlite) => sqlite.url_without_credentials(),
            Self::Dsn(dsn) => dsn.url_without_credentials(),
        }
    }

    /// The connection string this declaration opens.
    ///
    /// **Carries the password inline.** It is reachable because opening the
    /// same database somewhere else — a `pg_dump`, a pooler, a one-off script
    /// — needs the same string this does, and reconstructing it by hand is how
    /// two spellings of one connection drift apart. What it must never be is
    /// *rendered*: use
    /// [`url_without_credentials`](Self::url_without_credentials) for anything
    /// that is logged, printed or put in an error.
    ///
    /// # Errors
    ///
    /// When the declaration does not make sense — a server with no host, or
    /// none of a database to open. Checked here as well as when the declaration
    /// is read, so one assembled in code fails the same way with the same
    /// message.
    pub fn dsn(&self) -> Result<String> {
        match self {
            Self::Server(server) => server.dsn(),
            Self::Sqlite(sqlite) => {
                sqlite.validate()?;
                Ok(sqlite.dsn())
            }
            Self::Dsn(dsn) => {
                dsn.validate()?;
                Ok(dsn.dsn().to_string())
            }
        }
    }

    /// Whether this declaration reaches its reads and its writes through
    /// different endpoints.
    ///
    /// True as soon as either a `read` or a `write` role is named — a role that
    /// is absent takes the connection's own host, which still makes two
    /// endpoints if the other role names its own.
    pub fn is_split(&self) -> bool {
        match self {
            Self::Server(server) => server.is_split(),
            Self::Sqlite(_) | Self::Dsn(_) => false,
        }
    }

    /// Whether a write pins this connection's reads for the rest of the
    /// [scope](crate::sticky).
    pub fn is_sticky(&self) -> bool {
        match self {
            Self::Server(server) => server.is_sticky(),
            Self::Sqlite(_) | Self::Dsn(_) => false,
        }
    }

    /// Every connection string a **write** may be sent to. Never empty.
    ///
    /// One entry for an ordinary connection, and it is the same string
    /// [`dsn`](Self::dsn) answers. Several when the `write` role names several
    /// hosts.
    ///
    /// **Carries the password inline**, exactly as [`dsn`](Self::dsn) does, and
    /// with the same rule: never render one.
    ///
    /// # Errors
    ///
    /// When the declaration does not make sense — see [`dsn`](Self::dsn).
    pub fn write_dsns(&self) -> Result<Vec<String>> {
        match self {
            Self::Server(server) => server.role_dsns(Which::Write),
            _ => Ok(vec![self.dsn()?]),
        }
    }

    /// Every connection string a **read** may be sent to.
    ///
    /// **Empty** for a connection that does not split, which means reads go
    /// wherever writes do rather than that there is nowhere to read from.
    ///
    /// **Carries the password inline** — see [`dsn`](Self::dsn).
    ///
    /// # Errors
    ///
    /// When the declaration does not make sense — see [`dsn`](Self::dsn).
    pub fn read_dsns(&self) -> Result<Vec<String>> {
        match self {
            Self::Server(server) if server.is_split() => server.role_dsns(Which::Read),
            _ => Ok(Vec::new()),
        }
    }

    /// SQL this connection runs on **every** connection its pool opens.
    ///
    /// Empty for all but a MySQL connection that declared `strict`, which is
    /// the one setting here that no connection string can carry. Reachable
    /// because a caller opening the same database by hand — through
    /// [`SeaOrmExecutor::connect_with_session`](rainier_drivers::sql::SeaOrmExecutor::connect_with_session)
    /// — needs the same statements this does, and a connection configured half
    /// like this one is a connection that stores different rows for the same
    /// write.
    pub fn session_statements(&self) -> Vec<String> {
        match self {
            Self::Server(server) => server.session_statements(),
            Self::Sqlite(_) => Vec::new(),
            Self::Dsn(dsn) => dsn.session_statements(),
        }
    }

    /// How a pool for this connection's **writes** has to be shaped.
    ///
    /// The connection's own pool, and the only one there is when it does not
    /// split — see [`read_pool`](Self::read_pool) for the other half.
    ///
    /// One case is not tuning, and it is load-bearing in the silent direction:
    /// an in-memory SQLite database exists only as long as the connection
    /// holding it, so a second pooled connection is a second, empty database —
    /// and a query that lands on it returns no rows rather than an error. Read
    /// off the connection string rather than off the shape it was declared in,
    /// so `sqlite::memory:` gets the same treatment whether it arrived as a
    /// `database` or as a `url`. A declaration that would break it is refused
    /// when it is read, not quietly overridden here.
    ///
    /// A connection that declares no `pool` gets exactly what it got before a
    /// pool was declarable.
    pub fn pool(&self) -> rainier_orm::PoolConfig {
        self.pool_for(Which::Write)
    }

    /// How a pool for this connection's **reads** has to be shaped.
    ///
    /// The `read` role's, when it declares one; the connection's otherwise. The
    /// roles are separate because they are sized differently — a primary's
    /// connection budget is the scarce one and there is usually more than one
    /// replica — and because on a split connection each *host* of a role is its
    /// own pool, so a role's ceiling is multiplied by the number of hosts in it.
    pub fn read_pool(&self) -> rainier_orm::PoolConfig {
        self.pool_for(Which::Read)
    }

    /// One role's resolved pool.
    fn pool_for(&self, which: Which) -> rainier_orm::PoolConfig {
        match self {
            Self::Server(server) => server.resolved_pool(which),
            // Neither shape has roles to differ between.
            Self::Sqlite(sqlite) => sqlite.resolved_pool(),
            Self::Dsn(dsn) => dsn.resolved_pool(),
        }
    }

    /// The pool settings this connection declared, ignoring any role.
    pub fn pool_settings(&self) -> Option<&PoolSettings> {
        match self {
            Self::Server(server) => server.pool_settings(),
            Self::Sqlite(sqlite) => sqlite.pool_settings(),
            Self::Dsn(dsn) => dsn.pool_settings(),
        }
    }

    /// Open this connection, and only this connection.
    ///
    /// Every setting it uses comes from this declaration, so two connections
    /// opened from two declarations share nothing — not a pool, not a
    /// credential, not a host.
    ///
    /// A declaration that splits its reads opens **one pool per endpoint**, all
    /// of them here. That is the same choice
    /// [`Databases::build`](Databases::build) makes about connections as a
    /// whole and for the same reason: a replica that cannot be reached is a
    /// boot failure a deploy catches, rather than a query that fails at
    /// whichever hour it first runs.
    ///
    /// # Errors
    ///
    /// When the declaration does not make sense, when no executor was compiled
    /// in, or when any endpoint refuses the connection.
    pub async fn build(&self) -> Result<Database> {
        let writes = self.write_dsns()?;
        let reads = self.read_dsns()?;

        #[cfg(feature = "sea-orm-executor")]
        {
            // The session statements go to the pool, not to a connection: they
            // have to reach every connection it opens, including the ones it
            // opens later to replace a socket the server timed out. Every
            // endpoint gets the same ones — a replica whose `sql_mode` differs
            // from its primary's answers the same query differently.
            let session = self.session_statements();

            // Each role's own sizing, and each *host* of a role its own pool of
            // that size — which is the arithmetic to have in mind when reading
            // `max_connections` off a file: three replicas at twenty is sixty
            // sockets from this process, not twenty.
            let write_pool = self.pool();
            let read_pool = self.read_pool();

            let mut opened_writes = Vec::with_capacity(writes.len());
            for dsn in &writes {
                opened_writes.push(open_endpoint(dsn, &write_pool, &session).await?);
            }
            let mut opened_reads = Vec::with_capacity(reads.len());
            for dsn in &reads {
                opened_reads.push(open_endpoint(dsn, &read_pool, &session).await?);
            }

            Database::with_endpoints(opened_writes, opened_reads, self.is_sticky())
        }

        // Loud, and naming the fix. There is nothing to fall back to that would
        // not be a lie: an in-memory SQLite database would accept every
        // statement, migrate cleanly, and answer every query about the
        // application's own data with no rows.
        #[cfg(not(feature = "sea-orm-executor"))]
        {
            let _ = (writes, reads);
            Err(Error::internal(format!(
                "this connection uses the `{}` driver for `{}`, but rainier-database was built \
                 without the `sea-orm-executor` feature",
                self.driver(),
                self.url_without_credentials()
            )))
        }
    }
}

/// Open one endpoint's pool.
///
/// Every endpoint of a connection is opened the same way, with the same pool
/// shape and the same session statements — a replica configured differently
/// from its primary answers the same query differently, which is the whole
/// class of failure this module is about.
#[cfg(feature = "sea-orm-executor")]
async fn open_endpoint(
    dsn: &str,
    pool: &rainier_orm::PoolConfig,
    session: &[String],
) -> Result<std::sync::Arc<dyn crate::Connection>> {
    let executor =
        rainier_drivers::sql::SeaOrmExecutor::connect_with_session(dsn, pool, session).await?;
    Ok(std::sync::Arc::new(executor))
}

impl From<ServerDatabase> for DatabaseConfig {
    fn from(database: ServerDatabase) -> Self {
        Self::Server(database)
    }
}

impl From<SqliteDatabase> for DatabaseConfig {
    fn from(database: SqliteDatabase) -> Self {
        Self::Sqlite(database)
    }
}

impl From<DsnDatabase> for DatabaseConfig {
    fn from(database: DsnDatabase) -> Self {
        Self::Dsn(database)
    }
}

impl std::fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(database) => std::fmt::Debug::fmt(database, f),
            Self::Sqlite(database) => std::fmt::Debug::fmt(database, f),
            Self::Dsn(database) => std::fmt::Debug::fmt(database, f),
        }
    }
}

/// A database on a server — MySQL, MariaDB or PostgreSQL — from discrete
/// fields.
///
/// Which of those it is falls out of [`DatabaseDriver`]; the settings are the
/// same either way, and so is everything that can go wrong with them.
#[derive(Clone)]
pub struct ServerDatabase {
    driver: DatabaseDriver,
    host: String,
    /// `None` takes the engine's standard port.
    port: Option<u16>,
    /// A local socket path, reached *instead* of `host` and `port`.
    socket: Option<String>,
    database: String,
    credentials: DatabaseCredentials,
    /// `None` leaves the character set to the driver and the server, which is
    /// where it was before this was declarable — see this module's header.
    charset: Option<String>,
    /// `None` takes the character set's own default collation.
    collation: Option<String>,
    /// `None` leaves MySQL's `sql_mode` alone.
    strict: Option<bool>,
    /// A CA certificate to verify the server against.
    ssl_ca: Option<String>,
    /// Where reads go. `None` means "wherever writes go".
    read: Option<DatabaseRole>,
    /// Where writes go. `None` means the host on this struct.
    write: Option<DatabaseRole>,
    /// `None` and `Some(false)` mean the same thing to the connection and
    /// different things to the file, and the file round-trips.
    sticky: Option<bool>,
    /// Extra parameters for the driver's own URL parser.
    ///
    /// A `BTreeMap` so the rendered query string is stable: a connection string
    /// that reorders itself between runs makes two boot logs look like two
    /// different connections.
    options: BTreeMap<String, String>,
    /// How this connection's pool is sized. A role may override it.
    pool: Option<PoolSettings>,
}

/// Which half of a split connection an endpoint belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    /// Where [`Database::fetch`](crate::Database::fetch) goes.
    Read,
    /// Where [`Database::execute`](crate::Database::execute) goes.
    Write,
}

impl Which {
    /// The name this role is written down under.
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// One half of a connection that splits its reads from its writes.
///
/// Holds only what can differ between the two halves: the hosts, the port and
/// the credentials. Everything else — the driver, the database, the character
/// set, the TLS certificate, `strict` — is a property of the *data*, not of
/// which server is being asked, and is taken from the connection the role
/// belongs to. A role that could name its own database would be a replica of
/// something else, answering every query correctly about the wrong rows.
///
/// A role that names no host uses the connection's own, which is what makes
/// `read` alone the short and common spelling: replicas here, and writes
/// wherever they were already going.
#[derive(Clone, Default)]
pub struct DatabaseRole {
    hosts: Vec<String>,
    port: Option<u16>,
    /// `None` inherits the connection's credentials.
    credentials: Option<DatabaseCredentials>,
    /// `None` inherits the connection's pool sizing.
    pool: Option<PoolSettings>,
}

impl DatabaseRole {
    /// This role reaches `host`.
    pub fn on(host: impl Into<String>) -> Self {
        Self { hosts: vec![host.into()], ..Self::default() }
    }

    /// This role reaches each of `hosts`, in turn.
    ///
    /// They must be interchangeable: any query for this role may go to any of
    /// them, so a host that is a *different* database is one that answers some
    /// fraction of the application's queries from the wrong rows.
    pub fn across(hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { hosts: hosts.into_iter().map(Into::into).collect(), ..Self::default() }
    }

    /// This role uses the connection's hosts and only differs in what follows.
    ///
    /// For the arrangement where the replicas are reached as a read-only user:
    /// same servers, different credentials.
    pub fn inherited() -> Self {
        Self::default()
    }

    /// The port these hosts listen on. Defaults to the connection's, then to
    /// the engine's standard one.
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Authenticate to these hosts as `username` with `password`.
    ///
    /// Undeclared, the role authenticates as the connection does — which is
    /// usually right, and is wrong exactly when the replicas have their own
    /// read-only user.
    pub fn credentials(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials = Some(DatabaseCredentials::Password {
            username: username.into(),
            password: password.into(),
        });
        self
    }

    /// Authenticate to these hosts as `username` with no password.
    pub fn user(mut self, username: impl Into<String>) -> Self {
        self.credentials = Some(DatabaseCredentials::User { username: username.into() });
        self
    }

    /// Size this role's pools differently from the connection's.
    ///
    /// Applied over the connection's own sizing field by field, so a role that
    /// only raises `max_connections` keeps everything else. **Per host**: a
    /// role with three hosts opens three pools of this size.
    pub fn pool(mut self, pool: PoolSettings) -> Self {
        self.pool = Some(pool);
        self
    }

    /// The hosts this role names, which is empty when it takes the
    /// connection's.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// The port this role declared, when it declared one.
    pub fn port_number(&self) -> Option<u16> {
        self.port
    }

    /// How this role authenticates, when it says.
    pub fn credential_source(&self) -> Option<&DatabaseCredentials> {
        self.credentials.as_ref()
    }

    /// How this role sizes its pools, when it says.
    pub fn pool_settings(&self) -> Option<&PoolSettings> {
        self.pool.as_ref()
    }

    /// Whether this role says anything at all.
    fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.port.is_none()
            && self.credentials.is_none()
            && self.pool.is_none()
    }
}

/// Names the hosts and never the password.
///
/// Hand-written for the same reason [`ServerDatabase`]'s is: a role can carry
/// its own credential, so a derived `Debug` would put a second password in the
/// boot log of every process that started.
impl std::fmt::Debug for DatabaseRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseRole")
            .field("hosts", &self.hosts)
            .field("port", &self.port)
            // `DatabaseCredentials`' own `Debug` names the username and
            // redacts the rest.
            .field("credentials", &self.credentials)
            .field("pool", &self.pool)
            .finish()
    }
}

/// One endpoint of one role: where to dial, and who as.
struct Endpoint<'a> {
    host: &'a str,
    port: Option<u16>,
    credentials: &'a DatabaseCredentials,
}

impl ServerDatabase {
    /// A MySQL database called `database`, on a host still to be named.
    ///
    /// [`host`](Self::host) is required before this can be opened — a guessed
    /// one is a different database, and `localhost` is one that very often
    /// exists.
    pub fn mysql(database: impl Into<String>) -> Self {
        Self::new(DatabaseDriver::MySql, database)
    }

    /// A PostgreSQL database called `database`, on a host still to be named.
    pub fn postgres(database: impl Into<String>) -> Self {
        Self::new(DatabaseDriver::Postgres, database)
    }

    fn new(driver: DatabaseDriver, database: impl Into<String>) -> Self {
        Self {
            driver,
            host: String::new(),
            port: None,
            socket: None,
            database: database.into(),
            credentials: DatabaseCredentials::None,
            charset: None,
            collation: None,
            strict: None,
            ssl_ca: None,
            read: None,
            write: None,
            sticky: None,
            options: BTreeMap::new(),
            pool: None,
        }
    }

    /// The host to connect to.
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// The port to connect to. Defaults to the engine's standard one.
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Reach the server over a local socket at `path`, rather than over TCP.
    ///
    /// Replaces the host and the port rather than joining them: a connection
    /// over a socket never dials anything, so declaring both leaves a host in
    /// the file that everybody reads and nothing uses. Setting one after the
    /// other is refused when the declaration is checked, not silently resolved.
    pub fn unix_socket(mut self, path: impl Into<String>) -> Self {
        self.socket = Some(path.into());
        self
    }

    /// Authenticate as `username` with `password`.
    pub fn credentials(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials =
            DatabaseCredentials::Password { username: username.into(), password: password.into() };
        self
    }

    /// Authenticate as `username` with no password — trust, peer or IAM auth.
    pub fn user(mut self, username: impl Into<String>) -> Self {
        self.credentials = DatabaseCredentials::User { username: username.into() };
        self
    }

    /// The character set this connection negotiates. MySQL only.
    ///
    /// `utf8mb4` is the one that holds all of Unicode; MySQL's `utf8` is three
    /// bytes wide and truncates or replaces a four-byte character **without
    /// failing the write**. Undeclared leaves it to the driver and the server,
    /// because an assumed character set is an assumption about rows this
    /// connection did not write.
    pub fn charset(mut self, charset: impl Into<String>) -> Self {
        self.charset = Some(charset.into());
        self
    }

    /// The collation this connection sorts and compares with. MySQL only.
    ///
    /// A collation belongs to a character set, so this needs
    /// [`charset`](Self::charset) beside it — a collation alone is matched
    /// against whichever character set the driver assumes, which is a different
    /// answer to the same question. The reverse is fine: a character set with
    /// no collation takes that character set's own default, which is the
    /// engine's convention rather than a guess about the deployment.
    pub fn collation(mut self, collation: impl Into<String>) -> Self {
        self.collation = Some(collation.into());
        self
    }

    /// Whether an over-long or out-of-range value is an error. MySQL only.
    ///
    /// `false` is not "off" so much as "truncate": the write succeeds and the
    /// stored value is not the one that was sent. Undeclared leaves the
    /// server's own `sql_mode` in charge — see this module's header for why
    /// that is an unknown rather than a default.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }

    /// Verify the server's certificate against the CA at `path`.
    ///
    /// What a managed database hands out with its endpoint. The path is read by
    /// the driver at connect time, so a wrong one is a boot failure rather than
    /// an unverified connection.
    pub fn tls_ca(mut self, path: impl Into<String>) -> Self {
        self.ssl_ca = Some(path.into());
        self
    }

    /// Send reads to this role rather than to the connection's own host.
    ///
    /// Declaring either role splits the connection. What the other role does
    /// not name it takes from here, so `read` alone means "replicas there,
    /// writes where they already went".
    ///
    /// A read arriving before its write has replicated is answered from before
    /// the write, silently — [`sticky`](Self::sticky) is the setting for that,
    /// and [`crate::sticky`] is what it costs and covers.
    pub fn read(mut self, role: DatabaseRole) -> Self {
        self.read = Some(role);
        self
    }

    /// Send writes to this role rather than to the connection's own host.
    pub fn write(mut self, role: DatabaseRole) -> Self {
        self.write = Some(role);
        self
    }

    /// After a write, read from the endpoint that took it — within a
    /// [scope](crate::sticky).
    ///
    /// Needs a [`read`](Self::read) or [`write`](Self::write) role beside it:
    /// on a connection with one endpoint there is nothing for a read to be
    /// stale against, and the setting is refused rather than accepted and
    /// ignored.
    ///
    /// Read [`crate::sticky`] before declaring it. It is not free and it is not
    /// automatic: a sticky connection that no scope is tracking serves its
    /// reads from the write endpoint, because the alternative is the stale row
    /// the setting exists to prevent.
    pub fn sticky(mut self, sticky: bool) -> Self {
        self.sticky = Some(sticky);
        self
    }

    /// A parameter for the driver's own URL parser.
    ///
    /// The escape hatch for a setting this section has no field for —
    /// `ssl-mode`, `application_name`, a client certificate. Only the keys the
    /// engine's parser actually reads are accepted: sqlx ignores one it does
    /// not recognise without failing, so a passthrough would let a file declare
    /// a TLS mode under a spelling that never reaches the driver and connect
    /// unverified anyway.
    ///
    /// A key some other setting on this connection already settles — `charset`,
    /// `socket`, `dbname` — is refused as the second answer it would be.
    pub fn option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.insert(key.into(), value.into());
        self
    }

    /// Several [`option`](Self::option)s at once.
    pub fn options(
        mut self,
        options: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.options.extend(options.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Size this connection's pool.
    ///
    /// Applied over what the connection would have used anyway, field by field
    /// — see [`PoolSettings`] and this module's header. A role may override it
    /// for its own half.
    pub fn pool(mut self, pool: PoolSettings) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Which engine this speaks.
    pub fn driver(&self) -> DatabaseDriver {
        self.driver
    }

    /// The host this connects to. Empty when it connects over a socket.
    pub fn host_name(&self) -> &str {
        &self.host
    }

    /// The port, when one was declared.
    pub fn port_number(&self) -> Option<u16> {
        self.port
    }

    /// The local socket this connects over, when it does.
    pub fn socket_path(&self) -> Option<&str> {
        self.socket.as_deref()
    }

    /// The name of the database this opens.
    pub fn database_name(&self) -> &str {
        &self.database
    }

    /// How this connection authenticates.
    pub fn credential_source(&self) -> &DatabaseCredentials {
        &self.credentials
    }

    /// The character set this negotiates, when one was declared.
    pub fn charset_name(&self) -> Option<&str> {
        self.charset.as_deref()
    }

    /// The collation this compares with, when one was declared.
    pub fn collation_name(&self) -> Option<&str> {
        self.collation.as_deref()
    }

    /// Whether this connection was declared strict, when it says either way.
    pub fn strict_mode(&self) -> Option<bool> {
        self.strict
    }

    /// The CA this verifies the server against, when one was declared.
    pub fn tls_ca_path(&self) -> Option<&str> {
        self.ssl_ca.as_deref()
    }

    /// Where this connection's reads go, when they go somewhere of their own.
    pub fn read_role(&self) -> Option<&DatabaseRole> {
        self.read.as_ref()
    }

    /// Where this connection's writes go, when they go somewhere of their own.
    pub fn write_role(&self) -> Option<&DatabaseRole> {
        self.write.as_ref()
    }

    /// Whether this connection reaches its reads and writes through different
    /// endpoints.
    pub fn is_split(&self) -> bool {
        self.read.is_some() || self.write.is_some()
    }

    /// Whether a write pins this connection's reads for the rest of the
    /// [scope](crate::sticky).
    pub fn is_sticky(&self) -> bool {
        self.sticky.unwrap_or(false)
    }

    /// The driver parameters this connection declared.
    pub fn driver_options(&self) -> &BTreeMap<String, String> {
        &self.options
    }

    /// How this connection sizes its pools, ignoring any role.
    pub fn pool_settings(&self) -> Option<&PoolSettings> {
        self.pool.as_ref()
    }

    /// One role's fully resolved pool.
    ///
    /// Three layers, each one only saying what it changes: what a connection
    /// gets with no `pool` at all, then this connection's, then this role's.
    /// That is what lets a role raise `max_connections` alone and keep the
    /// timeouts the connection settled.
    fn resolved_pool(&self, which: Which) -> rainier_orm::PoolConfig {
        // A server connection is never the in-memory SQLite database the base
        // exists to protect.
        let mut pool = base_pool(false);
        if let Some(connection) = &self.pool {
            pool = connection.applied_to(pool);
        }
        let role = match which {
            Which::Read => self.read.as_ref(),
            Which::Write => self.write.as_ref(),
        };
        if let Some(role) = role.and_then(|role| role.pool.as_ref()) {
            pool = role.applied_to(pool);
        }
        pool
    }

    /// The server this connects to, with any credentials removed.
    ///
    /// Enough to tell two connections apart in a log, and not enough to
    /// authenticate with.
    ///
    /// A split connection renders the hosts **writes** go to, comma-separated
    /// where there is more than one — the endpoint that defines which database
    /// this is. It is a description rather than a connection string, which is
    /// all this has ever been; the read hosts are in this type's `Debug`, and
    /// each endpoint's real DSN comes from
    /// [`DatabaseConfig::write_dsns`](DatabaseConfig::write_dsns) and
    /// [`read_dsns`](DatabaseConfig::read_dsns).
    pub fn url_without_credentials(&self) -> String {
        let authority = if self.is_split() {
            self.endpoints(Which::Write)
                .iter()
                .map(|endpoint| self.authority_of(endpoint))
                .collect::<Vec<_>>()
                .join(",")
        } else {
            self.authority()
        };
        format!("{}://{}/{}", self.driver.scheme(), authority, self.database)
    }

    /// `host:port`, with the engine's standard port when none was declared —
    /// or the socket path, percent-encoded, when the connection is local.
    ///
    /// The socket goes in the authority for both engines because that is the
    /// one place both drivers read it from: PostgreSQL takes a host beginning
    /// with `/` as a socket path, and MySQL needs the `socket` parameter
    /// [`query`](Self::query) adds — but a URL with an empty authority does not
    /// parse at all, so there has to be something there, and the socket path is
    /// the only honest candidate. `localhost` would put a host in the boot log
    /// for a connection that never opens one.
    fn authority(&self) -> String {
        if let Some(socket) = &self.socket {
            return encode(socket);
        }
        match self.port.or_else(|| self.driver.default_port()) {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    /// The same, for one endpoint of one role.
    fn authority_of(&self, endpoint: &Endpoint<'_>) -> String {
        if let Some(socket) = &self.socket {
            return encode(socket);
        }
        match endpoint.port.or_else(|| self.driver.default_port()) {
            Some(port) => format!("{}:{port}", endpoint.host),
            None => endpoint.host.to_string(),
        }
    }

    /// Every endpoint of one role, in the order they were declared.
    ///
    /// Resolution is one rule applied three times: what the role names, or what
    /// the connection names. That is what lets `read` name only its hosts, or
    /// only its credentials, and be understood either way — and it is why a
    /// role cannot name a `database`, because the one field where "or what the
    /// connection names" would be a *different database* is not a field a role
    /// has.
    ///
    /// A connection with no roles has exactly one endpoint and it is the
    /// connection itself, which is why nothing below this line behaves
    /// differently for one.
    fn endpoints(&self, which: Which) -> Vec<Endpoint<'_>> {
        let role = match which {
            Which::Read => self.read.as_ref(),
            Which::Write => self.write.as_ref(),
        };

        let credentials =
            role.and_then(|role| role.credentials.as_ref()).unwrap_or(&self.credentials);
        let port = role.and_then(|role| role.port).or(self.port);

        let hosts: &[String] = match role {
            Some(role) if !role.hosts.is_empty() => &role.hosts,
            _ => std::slice::from_ref(&self.host),
        };

        hosts.iter().map(|host| Endpoint { host, port, credentials }).collect()
    }

    /// The settings the driver reads off the connection string's query.
    ///
    /// Each of these is a parameter the underlying driver parses itself, which
    /// is what makes them settings rather than decoration: `charset` and
    /// `collation` become the `SET NAMES … COLLATE …` the driver issues on
    /// every connection it opens, and `ssl-ca` becomes the certificate it
    /// verifies the server against. A setting the driver would *not* read is
    /// refused when the declaration is checked rather than rendered here and
    /// dropped on arrival.
    fn query(&self) -> String {
        let mut params: Vec<String> = Vec::new();

        if let Some(charset) = &self.charset {
            params.push(format!("charset={}", encode(charset)));
        }
        if let Some(collation) = &self.collation {
            params.push(format!("collation={}", encode(collation)));
        }
        if let Some(socket) = &self.socket {
            // PostgreSQL reads the socket off the authority, which already
            // carries it. MySQL only reads this parameter.
            if self.driver == DatabaseDriver::MySql {
                params.push(format!("socket={}", encode(socket)));
            }
        }
        if let Some(ca) = &self.ssl_ca {
            // One spelling for both: `ssl-ca` is MySQL's name for it and one of
            // PostgreSQL's accepted aliases for `sslrootcert`.
            params.push(format!("ssl-ca={}", encode(ca)));
        }

        // Last, and in a stable order. Each of these was checked against the
        // list of parameters this driver's URL parser actually reads when the
        // declaration was validated, so nothing here is rendered and dropped.
        for (key, value) in &self.options {
            params.push(format!("{}={}", encode(key), encode(value)));
        }

        if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        }
    }

    /// SQL run on **every** connection this declaration's pool opens.
    ///
    /// Empty unless `strict` was declared, because `sql_mode` is the one
    /// setting here that no MySQL connection string can carry. It is a
    /// statement rather than a parameter for that reason alone — see
    /// [`SeaOrmExecutor::connect_with_session`](rainier_drivers::sql::SeaOrmExecutor::connect_with_session)
    /// for why it has to reach every connection and not just the first.
    fn session_statements(&self) -> Vec<String> {
        self.strict.map(|strict| vec![strict_sql_mode(strict)]).unwrap_or_default()
    }

    /// Whether this declaration can be opened.
    ///
    /// Checked when a declaration is deserialised so a bad `database` section
    /// fails while the configuration is being read, and again when the
    /// connection is opened so one assembled in code fails the same way with
    /// the same message.
    fn validate(&self) -> Result<()> {
        if self.socket.is_some() && !self.host.trim().is_empty() {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares both a `unix_socket` and a `host`; a socket \
                 connection never dials anything, so the host would be read by everyone who opens \
                 the file and used by nothing",
                self.driver, self.database
            )));
        }
        if self.socket.is_some() && self.is_split() {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares a `unix_socket` and also a `read` or `write` \
                 role; a socket reaches one server on this machine and a role names another one, \
                 so one of the two would be opened and the other read by whoever changes the file \
                 next",
                self.driver, self.database
            )));
        }
        if self.socket.is_none() && !self.is_split() && self.host.trim().is_empty() {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares no `host` or `unix_socket`; a guessed host \
                 is a different database, and the obvious guess is one that very often exists and \
                 answers",
                self.driver, self.database
            )));
        }
        for which in [Which::Write, Which::Read] {
            if self.socket.is_none()
                && self.is_split()
                && self.endpoints(which).iter().any(|endpoint| endpoint.host.trim().is_empty())
            {
                return Err(Error::internal(format!(
                    "the `{}` connection to `{}` splits its reads from its writes, and its `{}` \
                     role resolves to no host: the role names none and neither does the \
                     connection. Taking the other role's host would put writes on a replica, or \
                     reads on the primary the split exists to spare, without saying so",
                    self.driver,
                    self.database,
                    which.name()
                )));
            }
        }
        for (which, role) in
            [(Which::Read, self.read.as_ref()), (Which::Write, self.write.as_ref())]
        {
            let Some(role) = role else { continue };
            if role.is_empty() {
                return Err(Error::internal(format!(
                    "the `{}` connection to `{}` declares an empty `{}`; it names no host, no port \
                     and no credentials, so it says nothing the connection does not already say \
                     — while reading, to everyone who opens the file, as a split that is not one",
                    self.driver,
                    self.database,
                    which.name()
                )));
            }
            if role.hosts.iter().any(|host| host.trim().is_empty()) {
                return Err(Error::internal(format!(
                    "the `{}` connection to `{}` declares an empty host in its `{}`; there is \
                     nothing there to dial",
                    self.driver,
                    self.database,
                    which.name()
                )));
            }
        }
        if self.sticky.is_some() && !self.is_split() {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares `sticky` and neither a `read` nor a `write` \
                 role; `sticky` sends the reads that follow a write to the endpoint that took it, \
                 and this connection has one endpoint — so there is nothing for a read to be \
                 stale against and nothing for the setting to do",
                self.driver, self.database
            )));
        }
        if self.pool.as_ref().is_some_and(PoolSettings::is_empty) {
            return Err(empty_pool());
        }
        for (which, role) in [(Which::Read, &self.read), (Which::Write, &self.write)] {
            if role.as_ref().and_then(|role| role.pool.as_ref()).is_some_and(PoolSettings::is_empty)
            {
                return Err(Error::internal(format!(
                    "the `{}` role declares an empty `pool`; it names nothing, so this role is \
                     sized exactly as the connection is — while reading as sizing of its own",
                    which.name()
                )));
            }
            // Resolved rather than as-declared, because a role that raises only
            // `min_connections` is a floor checked against a ceiling the
            // connection set two lines earlier.
            check_pool(&self.resolved_pool(which), &format!("`{}` role's", which.name()))?;
        }
        self.reject_options_the_driver_would_not_read()?;
        if self.socket.as_ref().is_some_and(|socket| socket.trim().is_empty()) {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares an empty `unix_socket`; there is no path to \
                 open",
                self.driver, self.database
            )));
        }
        if self.database.trim().is_empty() {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares no `database`; connecting without one lands \
                 in whatever that server calls the user's default, which answers queries rather \
                 than refusing them",
                self.driver, self.host
            )));
        }
        if self.collation.is_some() && self.charset.is_none() {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares a `collation` and no `charset`; a collation \
                 orders one character set, so alone it is matched against whichever one the driver \
                 assumes — which is a second answer to the question `charset` exists to settle",
                self.driver, self.database
            )));
        }

        let mysql_only: [(&str, bool); 3] = [
            ("charset", self.charset.is_some()),
            ("collation", self.collation.is_some()),
            ("strict", self.strict.is_some()),
        ];
        let misplaced: Vec<String> = mysql_only
            .iter()
            .filter(|(_, present)| *present && self.driver != DatabaseDriver::MySql)
            .map(|(name, _)| format!("`{name}`"))
            .collect();
        if !misplaced.is_empty() {
            return Err(Error::internal(format!(
                "the `{}` driver has no {}; PostgreSQL stores and compares text by the encoding \
                 and collation the *database* was created with, and refuses an over-long value \
                 either way. Accepting the setting here would put a value in the file that the \
                 server never hears",
                self.driver,
                misplaced.join(" or ")
            )));
        }

        Ok(())
    }

    /// Refuse an `options` key that would not reach the driver, or that some
    /// other setting on this connection already answers.
    ///
    /// The whole reason `options` is checked rather than passed through: sqlx's
    /// MySQL parser drops a parameter it does not recognise and its PostgreSQL
    /// one logs and carries on. Neither refuses. So `sslMode=VERIFY_CA` —
    /// right value, wrong spelling — is a connection that is *not* verifying
    /// anything, with the setting sitting in the file being reviewed.
    fn reject_options_the_driver_would_not_read(&self) -> Result<()> {
        for key in self.options.keys() {
            // Compared exactly, because that is how the driver compares it. Both
            // parsers match a query parameter's name literally, so `sslMode` is
            // not a spelling of `ssl-mode` — it is a parameter neither of them
            // has ever heard of, silently dropped, and a connection that is not
            // verifying anything while the file says `VERIFY_CA`.
            let key = key.as_str();

            if let Some(setting) = setting_that_already_answers(key) {
                return Err(Error::internal(format!(
                    "the `{}` connection to `{}` declares the option `{key}`, which is the same \
                     question `{setting}` answers on this connection. Two spellings of one \
                     setting means one of them is ignored, and which one is not visible from the \
                     file — declare `{setting}`",
                    self.driver, self.database
                )));
            }

            if !reads_option(self.driver, key) {
                return Err(Error::internal(format!(
                    "the `{}` driver does not read the connection parameter `{key}`; it is \
                     dropped on arrival without failing, so the connection would be opened \
                     without it while the file says otherwise. The parameters this driver reads \
                     are {}",
                    self.driver,
                    driver_options(self.driver)
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        Ok(())
    }

    /// The connection string this opens. Carries the password inline.
    ///
    /// The **write** endpoint of a split connection, and the first of them when
    /// the role names several: the one string that can run any statement. Every
    /// endpoint is in [`role_dsns`](Self::role_dsns).
    fn dsn(&self) -> Result<String> {
        self.validate()?;

        let endpoints = self.endpoints(Which::Write);
        let first = endpoints.first().expect("`validate` refuses a role with no endpoint");
        Ok(self.dsn_of(first))
    }

    /// Every connection string one role opens, in declaration order.
    fn role_dsns(&self, which: Which) -> Result<Vec<String>> {
        self.validate()?;
        Ok(self.endpoints(which).iter().map(|endpoint| self.dsn_of(endpoint)).collect())
    }

    /// One endpoint as a connection string. Carries the password inline.
    fn dsn_of(&self, endpoint: &Endpoint<'_>) -> String {
        // Percent-encoded, and this is not cosmetic: a password containing `@`
        // or `/` splits the URL somewhere else, and the host the driver then
        // dials is not the host that was declared.
        let userinfo = match endpoint.credentials {
            DatabaseCredentials::None => String::new(),
            DatabaseCredentials::User { username } => format!("{}@", encode(username)),
            DatabaseCredentials::Password { username, password } => {
                format!("{}:{}@", encode(username), encode(password))
            }
        };

        format!(
            "{}://{userinfo}{}/{}{}",
            self.driver.scheme(),
            self.authority_of(endpoint),
            encode(&self.database),
            self.query()
        )
    }
}

/// The connection parameters a driver's own URL parser reads, beyond the ones
/// this section has a field for.
///
/// Taken from sqlx's `MySqlConnectOptions::from_url` and
/// `PgConnectOptions::from_url`, which is the only list that matters: a
/// parameter outside it is dropped by the parser, and a connection opened
/// without a setting the file declares is exactly what refusing it prevents.
/// It is a short list on purpose — a parameter added here is a promise that
/// this build's driver reads it.
fn driver_options(driver: DatabaseDriver) -> &'static [&'static str] {
    match driver {
        DatabaseDriver::MySql => &[
            "ssl-mode",
            "sslmode",
            "ssl-cert",
            "sslcert",
            "ssl-key",
            "sslkey",
            "statement-cache-capacity",
            "timezone",
            "time-zone",
        ],
        DatabaseDriver::Postgres => &[
            "ssl-mode",
            "sslmode",
            "ssl-cert",
            "sslcert",
            "ssl-key",
            "sslkey",
            "statement-cache-capacity",
            "application_name",
            "options",
        ],
        // Never reached: a SQLite connection is refused an `options` map
        // before this, because its shape has no host to hang parameters off —
        // a file that wants `mode=` or `vfs=` writes them into a `url`.
        DatabaseDriver::Sqlite => &[],
    }
}

/// Whether this driver's parser reads `key`.
fn reads_option(driver: DatabaseDriver, key: &str) -> bool {
    if driver_options(driver).contains(&key) {
        return true;
    }
    // PostgreSQL's per-parameter form, `options[search_path]`, which is a
    // family rather than a name.
    driver == DatabaseDriver::Postgres && key.starts_with("options[") && key.ends_with(']')
}

/// The setting on this connection that already answers `key`, if one does.
///
/// Refused with its own message rather than as an unread parameter, because
/// these *would* reach the driver — which is what makes them dangerous. A
/// `dbname` in `options` beside a `database` field is two names for the
/// database this connection opens, and the one that loses is the one still
/// being read by whoever repoints the connection next.
fn setting_that_already_answers(key: &str) -> Option<&'static str> {
    Some(match key {
        "charset" => "charset",
        "collation" => "collation",
        "socket" => "unix_socket",
        "ssl-ca" | "sslca" | "sslrootcert" | "ssl-root-cert" => "ssl_ca",
        "host" | "hostaddr" => "host",
        "port" => "port",
        "dbname" => "database",
        "user" => "username",
        "password" => "password",
        _ => return None,
    })
}

/// `SET SESSION sql_mode = …`, adding or removing MySQL's strict modes and
/// leaving every other mode in place.
///
/// Written as an edit of `@@sql_mode` rather than an assignment of a fixed
/// list, and that is the load-bearing part. A connection arrives with modes on
/// it that are not this setting's business — the driver appends its own before
/// this runs, and a deployment's parameter group has its own — so assigning a
/// list would silently drop whichever of those were not in it.
///
/// The wrapping in commas is what makes the removal exact: with `@@sql_mode`
/// held as `,A,B,`, every mode is bounded by separators on both sides, so
/// replacing `,B,` with `,` cannot match a mode that merely *ends* in `B` and
/// leaves no doubled separator behind. `TRIM` then takes the wrapping off,
/// including the case where the result is empty.
fn strict_sql_mode(strict: bool) -> String {
    // Both, because they are not the same rule: `STRICT_TRANS_TABLES` still
    // truncates in a non-transactional table, where `STRICT_ALL_TABLES` errors.
    const MODES: [&str; 2] = ["STRICT_TRANS_TABLES", "STRICT_ALL_TABLES"];

    let mut without = "CONCAT(',', @@sql_mode, ',')".to_string();
    for mode in MODES {
        without = format!("REPLACE({without}, ',{mode},', ',')");
    }

    // Removed first even when they are about to be added back, so a connection
    // that already had one does not end up naming it twice.
    let value = if strict { format!("CONCAT({without}, '{}')", MODES.join(",")) } else { without };

    format!("SET SESSION sql_mode = TRIM(BOTH ',' FROM {value})")
}

/// Names the server and never the password.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the credential into whatever logged the connection, which for a
/// configuration dump at boot means the password is in the log of every process
/// that started.
impl std::fmt::Debug for ServerDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("ServerDatabase");
        out.field("driver", &self.driver)
            .field("url", &self.url_without_credentials())
            // The credential is deliberately absent rather than redacted in
            // place: see `DatabaseCredentials`, whose own `Debug` names the
            // username and nothing else.
            .field("credentials", &self.credentials);

        // Only when there is something to say, so an ordinary connection's
        // dump reads exactly as it did before splitting existed. A role's own
        // `Debug` redacts its own password.
        if let Some(read) = &self.read {
            out.field("read", read);
        }
        if let Some(write) = &self.write {
            out.field("write", write);
        }
        if self.is_split() {
            out.field("sticky", &self.is_sticky());
        }
        if !self.options.is_empty() {
            // Keys only. Every value here reached the allow-list, and nothing
            // on it is a secret today — but `options` is the field a password
            // will eventually be smuggled through, and a dump is a log line.
            out.field("options", &self.options.keys().collect::<Vec<_>>());
        }
        out.finish()
    }
}

/// How a [`ServerDatabase`] proves who it is.
///
/// Three cases, because there are three arrangements in the wild. A password is
/// the usual one. A username with no password covers trust, peer and IAM
/// authentication, where the server decides from the connection rather than
/// from a secret. Neither covers a socket that authenticates nobody.
///
/// [`None`](Self::None) is the default, and is the safe one to be wrong about:
/// a connection that should have named a credential is refused by the server,
/// which is loud. The reverse — a password with no username — is the one
/// refused at the declaration, because it would connect as the ambient user and
/// succeed as **somebody else**.
#[derive(Clone, Default)]
pub enum DatabaseCredentials {
    /// Nothing declared: a local socket, or a server that authenticates on
    /// something other than a secret.
    #[default]
    None,

    /// A username with no password — trust, peer or IAM auth.
    User {
        /// Who to connect as.
        username: String,
    },

    /// A username and a password.
    Password {
        /// Who to connect as.
        username: String,
        /// The password. Never rendered — see this type's `Debug`.
        password: String,
    },
}

/// Names who, and never what with.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the password into whatever logged the connection. The username
/// is not a secret and stays visible, because two connections to one host as
/// two users are otherwise indistinguishable in a log.
impl std::fmt::Debug for DatabaseCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::User { username } => write!(f, "User({username})"),
            Self::Password { username, .. } => write!(f, "Password({username}, <redacted>)"),
        }
    }
}

/// A connection written as one string — what `DATABASE_URL` holds.
///
/// The host, the port, the database and the credential are all inside it, which
/// is why this has none of them to set: a builder method that quietly did
/// nothing because the DSN already said otherwise would be the same
/// silently-ignored setting the wire form refuses.
///
/// The one exception is [`strict`](Self::strict), and it is an exception for
/// the reason the rest are not: a MySQL connection string has no parameter for
/// `sql_mode`. There is nothing inside the DSN for it to contradict, so setting
/// it here is not a second answer to a question the URL already answered — and
/// refusing it would leave the shape most deployments actually get, one
/// injected DSN, with no way to say whether an over-long value errors.
#[derive(Clone)]
pub struct DsnDatabase {
    driver: DatabaseDriver,
    url: String,
    /// `None` leaves MySQL's `sql_mode` alone. See [`ServerDatabase::strict`].
    strict: Option<bool>,
    /// How this connection's pool is sized — the second setting no connection
    /// string can carry. See [`ServerDatabase::pool`].
    pool: Option<PoolSettings>,
}

impl DsnDatabase {
    /// A connection to `url`, spoken to as `driver`.
    ///
    /// Use when the driver is already known. [`from_url`](Self::from_url) reads
    /// it off the scheme instead, which is what a bare `DATABASE_URL` needs.
    pub fn new(driver: DatabaseDriver, url: impl Into<String>) -> Self {
        Self { driver, url: url.into(), strict: None, pool: None }
    }

    /// Whether an over-long or out-of-range value is an error. MySQL only.
    ///
    /// The one setting a DSN cannot carry, so the one that may be declared
    /// beside it — see this type's own documentation, and
    /// [`ServerDatabase::strict`] for what the two answers mean.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }

    /// Whether this connection was declared strict, when it says either way.
    pub fn strict_mode(&self) -> Option<bool> {
        self.strict
    }

    /// Size this connection's pool.
    ///
    /// The other setting a DSN cannot carry, and so the other one that may be
    /// declared beside it — which matters here more than anywhere, because one
    /// injected `DATABASE_URL` is the shape most deployments get and a pool
    /// nobody can size is a pool sized for somebody else's process count.
    pub fn pool(mut self, pool: PoolSettings) -> Self {
        self.pool = Some(pool);
        self
    }

    /// How this connection sizes its pool, when it says.
    pub fn pool_settings(&self) -> Option<&PoolSettings> {
        self.pool.as_ref()
    }

    /// This connection's fully resolved pool.
    fn resolved_pool(&self) -> rainier_orm::PoolConfig {
        let base = base_pool(is_in_memory(&self.url));
        match &self.pool {
            Some(pool) => pool.applied_to(base),
            None => base,
        }
    }

    /// SQL run on every connection this declaration's pool opens.
    fn session_statements(&self) -> Vec<String> {
        self.strict.map(|strict| vec![strict_sql_mode(strict)]).unwrap_or_default()
    }

    /// Whether this declaration can be opened.
    fn validate(&self) -> Result<()> {
        if let Some(pool) = &self.pool {
            if pool.is_empty() {
                return Err(empty_pool());
            }
        }
        let resolved = self.resolved_pool();
        check_pool(&resolved, "connection's")?;
        if is_in_memory(&self.url) {
            check_in_memory_pool(&resolved)?;
        }

        if self.strict.is_some() && self.driver != DatabaseDriver::MySql {
            return Err(Error::internal(format!(
                "the `{}` driver has no `strict` mode; it is MySQL's `sql_mode`, and accepting the \
                 setting here would put a value in the file that the server never hears",
                self.driver
            )));
        }
        Ok(())
    }

    /// A connection to `url`, with the driver taken from its scheme.
    ///
    /// `mysql://` and `mariadb://`, `postgres://` and `postgresql://`,
    /// `sqlite:`. Everything else is an error, because the alternative to
    /// refusing an unrecognised scheme is choosing a driver on the deployment's
    /// behalf — and a `postgress://` typo that became a local SQLite file would
    /// migrate cleanly and answer every query with no rows.
    ///
    /// # Errors
    ///
    /// When `url` has no scheme, or one no driver speaks. The message names the
    /// scheme and **not** the URL: a DSN carries its password inline, and an
    /// error is a log line.
    pub fn from_url(url: &str) -> Result<Self> {
        let url = url.trim();

        let Some((scheme, _)) = url.split_once(':') else {
            return Err(Error::internal(format!(
                "`{}` is not a database URL: it names no scheme, so no driver claims it. A DSN \
                 looks like `mysql://…`, `postgres://…` or `sqlite://…`",
                without_credentials(url)
            )));
        };

        let driver = DatabaseDriver::from_scheme(scheme).ok_or_else(|| {
            Error::internal(format!(
                "no database driver speaks the `{scheme}` scheme; the schemes that are understood \
                 are `mysql`, `mariadb`, `postgres`, `postgresql` and `sqlite`"
            ))
        })?;

        Ok(Self::new(driver, url))
    }

    /// Which engine this speaks.
    pub fn driver(&self) -> DatabaseDriver {
        self.driver
    }

    /// The connection string. Carries the password inline — see
    /// [`DatabaseConfig::dsn`].
    pub fn dsn(&self) -> &str {
        &self.url
    }

    /// The server this connects to, with any credentials removed.
    ///
    /// The only rendering of the DSN there is, because a database URL routinely
    /// carries a password in its userinfo —
    /// `postgres://app:hunter2@db.example.com/app`.
    pub fn url_without_credentials(&self) -> String {
        without_credentials(&self.url)
    }
}

/// Names the server and never the password.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the DSN's userinfo into whatever logged the connection, which
/// for a configuration dump at boot means the password is in the log of every
/// process that started.
impl std::fmt::Debug for DsnDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DsnDatabase")
            .field("driver", &self.driver)
            .field("url", &self.url_without_credentials())
            .finish()
    }
}

/// A SQLite database: a file on this machine, or `:memory:`.
///
/// Survives a restart and not a redeploy, which is the distinction that catches
/// people out — a container's disk is not storage, and a database on one is not
/// a database anybody else can read.
#[derive(Clone, Debug)]
pub struct SqliteDatabase {
    database: String,
    /// How this connection's pool is sized. Heavily constrained when the
    /// database is in memory — see [`check_in_memory_pool`].
    pool: Option<PoolSettings>,
}

impl SqliteDatabase {
    /// The database in the file at `path`.
    ///
    /// The file has to exist. No `mode=rwc` is added, because creating one that
    /// is not there turns a mistyped path from a boot failure into a fresh,
    /// empty database that migrates cleanly and answers every query with no
    /// rows — which is this section's whole failure mode, handed out by
    /// default. A deployment that wants the file created says so, as a
    /// [`DsnDatabase`] with the `mode` it means.
    pub fn new(path: impl Into<String>) -> Self {
        Self { database: path.into(), pool: None }
    }

    /// A database in memory, for tests.
    ///
    /// Private to the connection that opens it, and gone when that connection
    /// closes. [`DatabaseConfig::pool`] answers
    /// [`PoolConfig::in_memory`](rainier_orm::PoolConfig::in_memory) for it,
    /// because a pool that opens a second connection opens a second *database*
    /// — empty, and answering rather than failing.
    pub fn in_memory() -> Self {
        Self { database: ":memory:".to_string(), pool: None }
    }

    /// Size this connection's pool.
    ///
    /// Almost entirely constrained when the database is in memory: it *is* the
    /// connection, so the pool is one connection kept for the life of the
    /// process, and a declaration that would change that is refused rather than
    /// applied. On a file it is an ordinary pool like any other.
    pub fn pool(mut self, pool: PoolSettings) -> Self {
        self.pool = Some(pool);
        self
    }

    /// How this connection sizes its pool, when it says.
    pub fn pool_settings(&self) -> Option<&PoolSettings> {
        self.pool.as_ref()
    }

    /// This connection's fully resolved pool.
    fn resolved_pool(&self) -> rainier_orm::PoolConfig {
        let base = base_pool(self.is_in_memory());
        match &self.pool {
            Some(pool) => pool.applied_to(base),
            None => base,
        }
    }

    /// Whether this declaration can be opened.
    fn validate(&self) -> Result<()> {
        if let Some(pool) = &self.pool {
            if pool.is_empty() {
                return Err(empty_pool());
            }
        }
        let resolved = self.resolved_pool();
        check_pool(&resolved, "connection's")?;
        if self.is_in_memory() {
            check_in_memory_pool(&resolved)?;
        }
        Ok(())
    }

    /// The path, or `:memory:`, this was declared with.
    pub fn path(&self) -> &str {
        &self.database
    }

    /// Whether this database lives only as long as its connection.
    pub fn is_in_memory(&self) -> bool {
        is_in_memory(&self.dsn())
    }

    /// This database as a URL. There is nothing in it to redact.
    ///
    /// Present so every declaration answers the same question the same way —
    /// SQLite authenticates nobody, so the safe rendering and the real one are
    /// the same string.
    pub fn url_without_credentials(&self) -> String {
        without_credentials(&self.dsn())
    }

    /// The connection string this opens.
    fn dsn(&self) -> String {
        let declared = self.database.trim();
        if declared == ":memory:" {
            return "sqlite::memory:".to_string();
        }
        format!("sqlite://{declared}")
    }
}

/// Whether a connection string names a database that exists only inside its own
/// connection.
///
/// The one pool decision that is not tuning: a second connection to one of
/// these is a second, *empty* database, and a query that lands on it answers
/// with no rows rather than failing.
fn is_in_memory(dsn: &str) -> bool {
    dsn.contains(":memory:") || dsn.contains("mode=memory")
}

/// A URL with its userinfo and query string removed.
///
/// Anything that does not parse as `scheme://…` is redacted **whole**. That is
/// the safe direction to be wrong in: a host nobody can read is an
/// inconvenience, and a password in a log is an incident. A redactor that gives
/// up and prints the string intact discloses the exact thing it exists to hide,
/// because a DSN carries its password inline.
///
/// The one exception is a `sqlite:` DSN with no authority — `sqlite::memory:`,
/// `sqlite:app.db`. SQLite authenticates nobody, so there is no credential to
/// disclose, and the general rule would redact the single most common DSN in
/// the framework into `<redacted>` — making a boot dump say nothing about the
/// one connection it could safely describe in full.
fn without_credentials(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("sqlite:") {
        if !rest.starts_with("//") {
            return url.to_string();
        }
    }

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

/// Percent-encode everything a URL's userinfo or path segment cannot carry
/// literally.
///
/// Written out rather than pulled in, because it is ten lines and the crate has
/// no other use for a URL library. Conservative on purpose: everything outside
/// RFC 3986's unreserved set is escaped, so a password containing `@`, `/`, `:`
/// or `#` cannot split the URL somewhere else and send the driver to a host
/// nobody declared.
fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// --- the wire form -----------------------------------------------------------

/// A connection as it is written down, before it is known to make sense.
///
/// The flat shape a configuration file wants, which [`DatabaseConfig`] is the
/// checked form of. Everything but `driver` is optional here so the *driver*
/// gets to say which settings apply, and so a misfiled one can be named in the
/// error rather than silently dropped.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDatabase {
    /// Required: an assumed driver is a connection pointed at whichever engine
    /// the default happens to be.
    driver: DatabaseDriver,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssl_ca: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read: Option<RawRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    write: Option<RawRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sticky: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    options: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pool: Option<PoolSettings>,

    // Named here so that declaring one is refused with the reason rather than
    // as an unknown key. A section ported from a framework that has them is
    // going to carry them, and "unknown field `prefix`" says the spelling is
    // wrong when the truth is that the setting is not honoured — which sends
    // somebody looking for the right spelling instead of for what to do
    // instead. They are never produced on the way out: the checked form has
    // nowhere to hold them, because nothing reads them.
    #[serde(default, skip_serializing)]
    prefix: Option<String>,
    #[serde(default, skip_serializing)]
    prefix_indexes: Option<bool>,
    #[serde(default, skip_serializing)]
    engine: Option<String>,
}

/// One half of a split connection, as it is written down.
///
/// The four fields a role may differ in, plus the three it is refused by name
/// — a role that named its own `driver`, `database` or `unix_socket` would be
/// pointed at a different database, which is this module's headline failure
/// wearing a `read` key. `deny_unknown_fields` would refuse them anyway, and
/// with a message about spelling rather than about what would happen.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<RoleHosts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pool: Option<PoolSettings>,

    #[serde(default, skip_serializing)]
    driver: Option<String>,
    #[serde(default, skip_serializing)]
    database: Option<String>,
    #[serde(default, skip_serializing)]
    unix_socket: Option<String>,
}

/// One host, or several. Both spellings are in the wild and both mean the same
/// thing, so both are read; one host is written back as one.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RoleHosts {
    /// `"host": "replica.example.com"`.
    One(String),
    /// `"host": ["replica-a.example.com", "replica-b.example.com"]`.
    Many(Vec<String>),
}

impl RoleHosts {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(host) => vec![host],
            Self::Many(hosts) => hosts,
        }
    }

    fn from_vec(hosts: Vec<String>) -> Option<Self> {
        match hosts.len() {
            0 => None,
            1 => Some(Self::One(hosts.into_iter().next().expect("just checked"))),
            _ => Some(Self::Many(hosts)),
        }
    }
}

impl RawRole {
    /// This role, checked.
    ///
    /// # Errors
    ///
    /// When it names something a role may not name, or a `password` with
    /// nobody to own it.
    fn into_role(self, which: Which) -> Result<DatabaseRole> {
        let misplaced: Vec<&str> = [
            ("driver", self.driver.is_some()),
            ("database", self.database.is_some()),
            ("unix_socket", self.unix_socket.is_some()),
        ]
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| *name)
        .collect();

        if !misplaced.is_empty() {
            return Err(Error::internal(format!(
                "the `{}` role declares {}; a role says which *servers* this half of the \
                 connection reaches, and nothing about what is in them. A role on another driver \
                 renders its SQL in the wrong dialect, and a role on another database answers \
                 every query it is given — correctly, about rows that belong to something else",
                which.name(),
                misplaced.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
            )));
        }

        let credentials = match (self.username, self.password) {
            (None, None) => None,
            (Some(username), None) => Some(DatabaseCredentials::User { username }),
            (Some(username), Some(password)) => {
                Some(DatabaseCredentials::Password { username, password })
            }
            // The same refusal the connection itself makes, for the same
            // reason: there is nobody for the password to belong to, so it is
            // dropped and this half of the connection authenticates as
            // whatever ambient user the environment supplies.
            (None, Some(_)) => {
                return Err(Error::internal(format!(
                    "the `{}` role declares a `password` and no `username`; there is nobody for it \
                     to belong to, so it would be dropped and this half of the connection would \
                     authenticate as whatever ambient user the environment supplies",
                    which.name()
                )))
            }
        };

        Ok(DatabaseRole {
            hosts: self.host.map(RoleHosts::into_vec).unwrap_or_default(),
            port: self.port,
            credentials,
            pool: self.pool,
        })
    }
}

impl From<DatabaseRole> for RawRole {
    fn from(role: DatabaseRole) -> Self {
        let (username, password) = match role.credentials {
            None | Some(DatabaseCredentials::None) => (None, None),
            Some(DatabaseCredentials::User { username }) => (Some(username), None),
            Some(DatabaseCredentials::Password { username, password }) => {
                (Some(username), Some(password))
            }
        };

        Self {
            host: RoleHosts::from_vec(role.hosts),
            port: role.port,
            username,
            password,
            pool: role.pool,
            // Nothing reads these, so the checked form never held one.
            driver: None,
            database: None,
            unix_socket: None,
        }
    }
}

impl RawDatabase {
    /// Refuse settings this driver would ignore.
    ///
    /// A `host` on a `sqlite` connection is not a harmless extra key — it is
    /// somebody believing these rows reach the server they configured when they
    /// reach a file that goes away with the container. Dropping it silently is
    /// how that belief survives to production, where it looks like a database
    /// that has lost everything since the last deploy.
    fn reject_settings_it_ignores(&self, used: &[&str]) -> Result<()> {
        let declared: [(&str, bool); 16] = [
            ("url", self.url.is_some()),
            ("host", self.host.is_some()),
            ("port", self.port.is_some()),
            ("database", self.database.is_some()),
            ("username", self.username.is_some()),
            ("password", self.password.is_some()),
            ("charset", self.charset.is_some()),
            ("collation", self.collation.is_some()),
            ("strict", self.strict.is_some()),
            ("unix_socket", self.unix_socket.is_some()),
            ("ssl_ca", self.ssl_ca.is_some()),
            ("read", self.read.is_some()),
            ("write", self.write.is_some()),
            ("sticky", self.sticky.is_some()),
            ("options", !self.options.is_empty()),
            ("pool", self.pool.is_some()),
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
             that ignores where it was told to look is one that answers from somewhere else",
            self.driver,
            ignored.join(", ")
        )))
    }

    /// Refuse a `url` written down beside the fields it already contains.
    ///
    /// Laravel resolves this by letting the URL win. That is the resolution
    /// refused here: the loser is still in the file being read by whoever
    /// changes it next, so pointing a connection at a new host by editing the
    /// visible `host` is a change that reviews cleanly, deploys cleanly and
    /// does nothing at all.
    fn reject_a_url_beside_its_own_parts(&self) -> Result<()> {
        if self.url.is_none() {
            return Ok(());
        }

        // `strict` is deliberately absent: it is the one setting a connection
        // string has no parameter for, so declaring it beside a `url` is not
        // two answers to one question — see `DsnDatabase`. `read`, `write` and
        // `options` are not in that position: a DSN is one endpoint and carries
        // its own query string.
        let also: Vec<String> = [
            ("host", self.host.is_some()),
            ("port", self.port.is_some()),
            ("database", self.database.is_some()),
            ("username", self.username.is_some()),
            ("password", self.password.is_some()),
            ("charset", self.charset.is_some()),
            ("collation", self.collation.is_some()),
            ("unix_socket", self.unix_socket.is_some()),
            ("ssl_ca", self.ssl_ca.is_some()),
            ("options", !self.options.is_empty()),
        ]
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| format!("`{name}`"))
        .collect();

        if also.is_empty() {
            return Ok(());
        }

        Err(Error::internal(format!(
            "this connection declares `url` and also {}; a DSN carries the host, the database, the \
             credentials and the driver's own parameters inline, so one of the two is ignored — \
             and which one is not visible from the file. Declare either the URL or the fields, not \
             both",
            also.join(", ")
        )))
    }

    /// Refuse a split written down beside a `url`.
    ///
    /// Its own rejection, because this one is not two answers to one question:
    /// a DSN names one endpoint and there is no spelling of a second inside it,
    /// so the roles would simply not happen. A split connection is written as
    /// discrete fields.
    fn reject_a_split_beside_a_url(&self) -> Result<()> {
        if self.url.is_none() {
            return Ok(());
        }

        let also: Vec<String> = [
            ("read", self.read.is_some()),
            ("write", self.write.is_some()),
            ("sticky", self.sticky.is_some()),
        ]
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| format!("`{name}`"))
        .collect();

        if also.is_empty() {
            return Ok(());
        }

        Err(Error::internal(format!(
            "this connection declares `url` and also {}; a connection string names one endpoint \
             and has no room for a second, so the split would be read by everyone who opens the \
             file and would never happen. A connection that separates its reads from its writes \
             is written as `host`, `database` and the roles beside them",
            also.join(", ")
        )))
    }

    /// Refuse the settings this section does not honour, by name.
    ///
    /// The alternative is not "accept them": it is a file that says `prefix`
    /// and a database that never hears about one. Refusing an unknown key
    /// already happens through `deny_unknown_fields`; this exists so the
    /// message is the reason rather than the spelling.
    fn reject_settings_nothing_reads(&self) -> Result<()> {
        if self.prefix.is_some() || self.prefix_indexes.is_some() {
            return Err(Error::internal(
                "this connection declares `prefix` or `prefix_indexes`, and a table prefix is not \
                 supported. It cannot be applied everywhere a table name is rendered — an entity \
                 names its table as a constant, a foreign key names its parent as a string, and a \
                 migration step or a raw statement is SQL already written — and a prefix that \
                 reaches some of those and not the rest sends queries to a table that exists and \
                 is empty, which reads as missing data rather than as a misconfiguration",
            ));
        }

        if self.engine.is_some() {
            return Err(Error::internal(
                "this connection declares `engine`, and the table engine is not settable here. It \
                 is a `CREATE TABLE` clause, and a migration renders from a dialect without ever \
                 seeing the connection's declaration — so accepting it would put a value in the \
                 file that the server never hears, and the table would be created with the \
                 server's default while the configuration said otherwise",
            ));
        }

        Ok(())
    }

    /// Refuse a `host` or a `port` written down beside a `unix_socket`.
    ///
    /// Its own rejection rather than "the driver does not use it", because the
    /// driver does — just not on this connection. A socket connection never
    /// dials anything, so the host sitting in the file is read by whoever
    /// repoints the connection next and used by nothing.
    fn reject_a_socket_beside_a_host(&self) -> Result<()> {
        if self.unix_socket.is_none() {
            return Ok(());
        }

        let also: Vec<String> = [("host", self.host.is_some()), ("port", self.port.is_some())]
            .iter()
            .filter(|(_, present)| *present)
            .map(|(name, _)| format!("`{name}`"))
            .collect();

        if also.is_empty() {
            return Ok(());
        }

        Err(Error::internal(format!(
            "this connection declares `unix_socket` and also {}; a connection over a local socket \
             never dials a host, so the host would be read by everyone who opens the file and used \
             by nothing. Declare either the socket or the host, not both",
            also.join(", ")
        )))
    }
}

impl TryFrom<RawDatabase> for DatabaseConfig {
    type Error = Error;

    fn try_from(raw: RawDatabase) -> Result<Self> {
        // First, because a setting nothing reads is refused whichever driver
        // and whichever shape it was written beside.
        raw.reject_settings_nothing_reads()?;

        // Checked before the driver's own settings, because a `url` beside a
        // `host` has a reason of its own and `does not use` would not be it.
        raw.reject_a_split_beside_a_url()?;
        raw.reject_a_url_beside_its_own_parts()?;
        raw.reject_a_socket_beside_a_host()?;

        // Whichever driver it names, a `url` is the whole connection. The
        // scheme is not re-derived from it: the declaration already said which
        // driver this is, and reading it twice is two answers to one question.
        if let Some(url) = raw.url {
            let mut dsn = DsnDatabase::new(raw.driver, url);
            if let Some(strict) = raw.strict {
                dsn = dsn.strict(strict);
            }
            if let Some(pool) = raw.pool {
                dsn = dsn.pool(pool);
            }
            dsn.validate()?;
            return Ok(Self::Dsn(dsn));
        }

        match raw.driver {
            DatabaseDriver::Sqlite => {
                // `pool` is here rather than refused: a file database pools
                // like any other, and the in-memory one is checked against what
                // it can survive rather than told it may not be sized.
                raw.reject_settings_it_ignores(&["url", "database", "pool"])?;

                let database = raw.database.ok_or_else(|| {
                    Error::internal(
                        "a `sqlite` connection needs a `database` to open — a file path, or \
                         `:memory:`. An assumed one is an empty database that migrates cleanly \
                         and answers every query with no rows",
                    )
                })?;
                let sqlite = SqliteDatabase { database, pool: raw.pool };
                sqlite.validate()?;
                Ok(Self::Sqlite(sqlite))
            }

            driver => {
                // Every setting a server connection can carry. `charset`,
                // `collation` and `strict` are named for MySQL only, so a
                // PostgreSQL connection declaring one is told the driver does
                // not use it rather than having it rendered and dropped.
                let mut used = vec![
                    "url",
                    "host",
                    "port",
                    "database",
                    "username",
                    "password",
                    "unix_socket",
                    "ssl_ca",
                    "read",
                    "write",
                    "sticky",
                    "options",
                    "pool",
                ];
                if driver == DatabaseDriver::MySql {
                    used.extend(["charset", "collation", "strict"]);
                }
                raw.reject_settings_it_ignores(&used)?;

                let read = raw.read.map(|role| role.into_role(Which::Read)).transpose()?;
                let write = raw.write.map(|role| role.into_role(Which::Write)).transpose()?;
                let split = read.is_some() || write.is_some();

                let host = match (raw.host, &raw.unix_socket, split) {
                    (Some(host), _, _) => host,
                    // A socket is the address, so there is no host to require.
                    (None, Some(_), _) => String::new(),
                    // The roles carry the addresses. Whether they carry enough
                    // of them is `ServerDatabase::validate`'s question, and it
                    // answers it per role rather than for the connection.
                    (None, None, true) => String::new(),
                    (None, None, false) => {
                        return Err(Error::internal(format!(
                            "a `{driver}` connection needs a `host` to connect to, a `unix_socket` \
                             to open, or a `url` that names one"
                        )))
                    }
                };
                let database = raw.database.ok_or_else(|| {
                    Error::internal(format!(
                        "a `{driver}` connection needs the name of the `database` to open; \
                         connecting without one lands in whatever that server calls the user's \
                         default"
                    ))
                })?;

                let credentials = match (raw.username, raw.password) {
                    (None, None) => DatabaseCredentials::None,
                    (Some(username), None) => DatabaseCredentials::User { username },
                    (Some(username), Some(password)) => {
                        DatabaseCredentials::Password { username, password }
                    }
                    // A password with no username is the dangerous half, so it
                    // is the one spelled out: there is nobody for it to belong
                    // to, so it is dropped and the connection authenticates as
                    // whatever the ambient environment is — which succeeds, as
                    // somebody else, against the same database.
                    (None, Some(_)) => {
                        return Err(Error::internal(format!(
                            "the `{driver}` connection to `{database}` declares a `password` and \
                             no `username`; there is nobody for it to belong to, so it would be \
                             dropped and the connection would authenticate as whatever ambient \
                             user the environment supplies"
                        )))
                    }
                };

                let server = ServerDatabase {
                    driver,
                    host,
                    port: raw.port,
                    socket: raw.unix_socket,
                    database,
                    credentials,
                    charset: raw.charset,
                    collation: raw.collation,
                    strict: raw.strict,
                    ssl_ca: raw.ssl_ca,
                    read,
                    write,
                    sticky: raw.sticky,
                    options: raw.options,
                    pool: raw.pool,
                };
                server.validate()?;

                Ok(Self::Server(server))
            }
        }
    }
}

impl From<DatabaseConfig> for RawDatabase {
    fn from(database: DatabaseConfig) -> Self {
        let blank = |driver| Self {
            driver,
            url: None,
            host: None,
            port: None,
            database: None,
            username: None,
            password: None,
            charset: None,
            collation: None,
            strict: None,
            unix_socket: None,
            ssl_ca: None,
            read: None,
            write: None,
            sticky: None,
            options: BTreeMap::new(),
            pool: None,
            // Nothing reads these, so the checked form never held one and
            // there is nothing to write back out.
            prefix: None,
            prefix_indexes: None,
            engine: None,
        };

        match database {
            DatabaseConfig::Dsn(dsn) => {
                Self { url: Some(dsn.url), strict: dsn.strict, pool: dsn.pool, ..blank(dsn.driver) }
            }

            DatabaseConfig::Sqlite(sqlite) => Self {
                database: Some(sqlite.database),
                pool: sqlite.pool,
                ..blank(DatabaseDriver::Sqlite)
            },

            DatabaseConfig::Server(server) => {
                let (username, password) = match server.credentials {
                    DatabaseCredentials::None => (None, None),
                    DatabaseCredentials::User { username } => (Some(username), None),
                    DatabaseCredentials::Password { username, password } => {
                        (Some(username), Some(password))
                    }
                };
                // A socket connection has no host to write back: it was never
                // declared, and rendering an empty one would round-trip into a
                // declaration that is refused. Nor has a split whose roles
                // carry every address between them.
                let host = server
                    .socket
                    .is_none()
                    .then_some(server.host)
                    .filter(|host| !host.trim().is_empty());
                Self {
                    host,
                    port: server.port,
                    database: Some(server.database),
                    username,
                    password,
                    charset: server.charset,
                    collation: server.collation,
                    strict: server.strict,
                    unix_socket: server.socket,
                    ssl_ca: server.ssl_ca,
                    read: server.read.map(RawRole::from),
                    write: server.write.map(RawRole::from),
                    sticky: server.sticky,
                    options: server.options,
                    pool: server.pool,
                    ..blank(server.driver)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- reading a declaration ---------------------------------------------

    #[test]
    fn a_section_deserialises_into_the_connections_it_declares() {
        let databases: Databases = serde_json::from_value(json!({
            "default": "primary",
            "connections": {
                "primary": {
                    "driver": "mysql",
                    "host": "db.example.com",
                    "database": "app",
                    "username": "app",
                    "password": "…",
                },
                "replica": { "driver": "postgres", "url": "postgres://replica.example.com/app" },
                "reporting": { "driver": "sqlite", "database": "storage/reporting.sqlite" },
            },
        }))
        .unwrap();

        assert_eq!(databases.default_name(), "primary");
        assert_eq!(databases.names().collect::<Vec<_>>(), vec!["primary", "replica", "reporting"]);
        assert_eq!(databases.get("primary").unwrap().driver(), DatabaseDriver::MySql);
        assert_eq!(databases.get("replica").unwrap().driver(), DatabaseDriver::Postgres);
        assert_eq!(databases.get("reporting").unwrap().driver(), DatabaseDriver::Sqlite);
    }

    #[test]
    fn a_connection_without_a_driver_is_refused() {
        // An assumed driver is a connection pointed at whichever engine the
        // default happens to be, which is the whole failure this module is
        // about.
        let err = serde_json::from_value::<Databases>(json!({
            "connections": { "primary": { "host": "db.example.com", "database": "app" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("driver"), "{err}");
    }

    #[test]
    fn a_misspelled_driver_lists_the_valid_ones() {
        let err = serde_json::from_value::<Databases>(json!({
            "connections": { "primary": { "driver": "pgsql", "database": "app" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`sqlite`"), "{err}");
        assert!(err.contains("`mysql`"), "{err}");
        assert!(err.contains("`postgres`"), "{err}");
    }

    #[test]
    fn a_setting_the_driver_ignores_is_refused_rather_than_dropped() {
        // Someone believes these rows reach the server they configured. They
        // reach a file that goes away with the container.
        let err = serde_json::from_value::<Databases>(json!({
            "connections": {
                "primary": {
                    "driver": "sqlite",
                    "database": "app.sqlite",
                    "host": "db.example.com",
                },
            },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`host`"), "{err}");
        assert!(err.contains("does not use"), "{err}");
    }

    #[test]
    fn an_unknown_setting_is_refused_rather_than_dropped() {
        let err = serde_json::from_value::<Databases>(json!({
            "connections": {
                "primary": { "driver": "mysql", "host": "h", "database": "app", "usernme": "typo" },
            },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("usernme"), "{err}");
    }

    #[test]
    fn a_url_beside_the_fields_it_contains_is_refused_rather_than_resolved() {
        // The setting that loses is still in the file. Repointing the
        // connection by editing the visible `host` would review cleanly, deploy
        // cleanly, and change nothing.
        for declaration in [
            json!({ "driver": "mysql", "url": "mysql://h/app", "host": "other.example.com" }),
            json!({ "driver": "mysql", "url": "mysql://h/app", "password": "shh" }),
            json!({ "driver": "sqlite", "url": "sqlite::memory:", "database": "app.sqlite" }),
        ] {
            let err =
                serde_json::from_value::<DatabaseConfig>(declaration).unwrap_err().to_string();

            assert!(err.contains("declares `url` and also"), "{err}");
            assert!(err.contains("not both"), "{err}");
        }
    }

    #[test]
    fn each_driver_requires_what_it_cannot_work_without() {
        let no_host = serde_json::from_value::<DatabaseConfig>(
            json!({ "driver": "mysql", "database": "app" }),
        )
        .unwrap_err()
        .to_string();
        assert!(no_host.contains("`host`"), "{no_host}");

        let no_database = serde_json::from_value::<DatabaseConfig>(
            json!({ "driver": "postgres", "host": "db.example.com" }),
        )
        .unwrap_err()
        .to_string();
        assert!(no_database.contains("`database`"), "{no_database}");

        let no_file = serde_json::from_value::<DatabaseConfig>(json!({ "driver": "sqlite" }))
            .unwrap_err()
            .to_string();
        assert!(no_file.contains("`database`"), "{no_file}");

        let blank_host = serde_json::from_value::<DatabaseConfig>(
            json!({ "driver": "mysql", "host": "  ", "database": "app" }),
        )
        .unwrap_err()
        .to_string();
        assert!(blank_host.contains("`host`"), "{blank_host}");
    }

    #[test]
    fn a_declaration_round_trips_through_its_wire_form() {
        for original in [
            json!({
                "driver": "mysql",
                "host": "db.example.com",
                "port": 3307,
                "database": "app",
                "username": "app",
                "password": "shh",
            }),
            json!({ "driver": "postgres", "url": "postgres://app:shh@db.example.com/app" }),
            json!({ "driver": "postgres", "host": "db.example.com", "database": "app" }),
            json!({ "driver": "sqlite", "database": "storage/app.sqlite" }),
            json!({ "driver": "sqlite", "url": "sqlite::memory:" }),
        ] {
            let database: DatabaseConfig = serde_json::from_value(original.clone()).unwrap();
            assert_eq!(serde_json::to_value(&database).unwrap(), original);
        }
    }

    #[test]
    fn a_dsn_declaration_keeps_the_driver_that_was_declared() {
        // The scheme is not re-derived from the URL. The declaration already
        // said which driver this is, and two answers to one question is how a
        // connection ends up rendering SQL for an engine it is not talking to.
        let declared: DatabaseConfig = serde_json::from_value(
            json!({ "driver": "mysql", "url": "mysql://db.example.com/app" }),
        )
        .unwrap();

        assert_eq!(declared.driver(), DatabaseDriver::MySql);
        assert_eq!(declared.dialect(), Dialect::MySql);
    }

    // --- one DSN, which is what DATABASE_URL is -----------------------------

    #[test]
    fn a_url_alone_declares_a_working_default_connection() {
        // What every existing application and test has: no section, one DSN.
        let databases = Databases::from_url("mysql://app:shh@db.example.com/app").unwrap();

        assert_eq!(databases.default_name(), Databases::DEFAULT_NAME);
        assert_eq!(databases.names().collect::<Vec<_>>(), vec![Databases::DEFAULT_NAME]);

        let declared = databases.get(Databases::DEFAULT_NAME).unwrap();
        assert_eq!(declared.driver(), DatabaseDriver::MySql);
        assert_eq!(declared.dsn().unwrap(), "mysql://app:shh@db.example.com/app");
    }

    #[test]
    fn the_driver_comes_from_the_scheme_and_a_scheme_nobody_speaks_is_an_error() {
        let cases = [
            ("sqlite::memory:", DatabaseDriver::Sqlite),
            ("sqlite://storage/app.sqlite", DatabaseDriver::Sqlite),
            ("mysql://db.example.com/app", DatabaseDriver::MySql),
            // MariaDB's own tooling emits this, and it is the same protocol.
            ("mariadb://db.example.com/app", DatabaseDriver::MySql),
            ("postgres://db.example.com/app", DatabaseDriver::Postgres),
            ("postgresql://db.example.com/app", DatabaseDriver::Postgres),
        ];
        for (url, driver) in cases {
            assert_eq!(DatabaseConfig::from_url(url).unwrap().driver(), driver, "{url}");
        }

        // A typo that quietly became SQLite would migrate cleanly and answer
        // every query with no rows.
        let err = DatabaseConfig::from_url("postgress://db.example.com/app")
            .err()
            .expect("no driver speaks it");
        assert!(err.message().contains("postgress"), "{}", err.message());

        let bare = DatabaseConfig::from_url("/var/db/app.sqlite").err().expect("no scheme");
        assert!(bare.message().contains("scheme"), "{}", bare.message());
    }

    #[test]
    fn a_dsn_and_the_fields_that_spell_it_out_open_the_same_database() {
        // What makes moving between the two shapes safe.
        let from_url = DatabaseConfig::from_url("postgres://app:shh@db.example.com:5433/app")
            .unwrap()
            .dsn()
            .unwrap();

        let from_parts = DatabaseConfig::from(
            ServerDatabase::postgres("app")
                .host("db.example.com")
                .port(5433)
                .credentials("app", "shh"),
        )
        .dsn()
        .unwrap();

        assert_eq!(from_url, from_parts);
    }

    // --- rendering a connection string --------------------------------------

    #[test]
    fn a_declaration_without_a_port_takes_the_engines_standard_one() {
        let mysql = DatabaseConfig::from(ServerDatabase::mysql("app").host("db.example.com"));
        assert_eq!(mysql.dsn().unwrap(), "mysql://db.example.com:3306/app");

        let postgres = DatabaseConfig::from(ServerDatabase::postgres("app").host("db.example.com"));
        assert_eq!(postgres.dsn().unwrap(), "postgres://db.example.com:5432/app");
    }

    #[test]
    fn a_credential_that_would_split_the_url_is_encoded_rather_than_pasted() {
        // Not cosmetic: unencoded, everything after the `@` in the password
        // becomes the host, and the driver dials a machine nobody declared.
        let dsn = DatabaseConfig::from(
            ServerDatabase::mysql("app")
                .host("db.example.com")
                .credentials("app@corp", "p@ss/word:1"),
        )
        .dsn()
        .unwrap();

        assert_eq!(dsn, "mysql://app%40corp:p%40ss%2Fword%3A1@db.example.com:3306/app");
        assert_eq!(dsn.matches('@').count(), 1, "one authority separator, not three");
    }

    #[test]
    fn a_username_with_no_password_is_a_credential_and_not_a_mistake() {
        // Trust, peer and IAM auth all look like this.
        let declared: DatabaseConfig = serde_json::from_value(json!({
            "driver": "postgres",
            "host": "db.example.com",
            "database": "app",
            "username": "app",
        }))
        .unwrap();

        assert_eq!(declared.dsn().unwrap(), "postgres://app@db.example.com:5432/app");
    }

    #[test]
    fn a_sqlite_path_is_a_file_and_never_created_on_the_way_past() {
        // No `mode=rwc`: a mistyped path must fail, not become a fresh empty
        // database that migrates cleanly.
        let file = DatabaseConfig::sqlite("storage/app.sqlite");
        assert_eq!(file.dsn().unwrap(), "sqlite://storage/app.sqlite");
        assert!(!file.dsn().unwrap().contains("rwc"));

        // A DSN somebody wrote down is passed through, `mode` and all.
        let dsn = DatabaseConfig::from(DsnDatabase::new(
            DatabaseDriver::Sqlite,
            "sqlite://ci.sqlite?mode=rwc",
        ));
        assert_eq!(dsn.dsn().unwrap(), "sqlite://ci.sqlite?mode=rwc");

        assert_eq!(DatabaseConfig::sqlite_in_memory().dsn().unwrap(), "sqlite::memory:");
    }

    #[test]
    fn an_in_memory_database_gets_the_only_pool_it_survives() {
        // Such a database exists only as long as its connection, so a second
        // pooled connection is a second, empty database — which answers.
        for declared in [
            DatabaseConfig::sqlite_in_memory(),
            // …and the same database written as a DSN, which is how
            // `DATABASE_URL=sqlite::memory:` arrives.
            DatabaseConfig::from_url("sqlite::memory:").unwrap(),
        ] {
            assert_eq!(declared.pool().max_connections, 1);
            assert_eq!(declared.pool().min_connections, 1);
            assert_eq!(declared.pool().idle_timeout, None);
        }

        // A file is a file; it does not need coddling.
        assert!(DatabaseConfig::sqlite("storage/app.sqlite").pool().max_connections > 1);
    }

    // --- what happens to a value once it arrives ----------------------------

    #[test]
    fn a_charset_and_collation_reach_the_connection_rather_than_the_file() {
        // The parameter names are the driver's, not this module's: sqlx reads
        // `charset` and `collation` off the connection string and issues
        // `SET NAMES … COLLATE …` on every connection it opens. Spelled any
        // other way they would be parsed as unknown, dropped, and the
        // connection would negotiate whatever it would have anyway — which is
        // the three-byte `utf8` this setting exists to get away from.
        let dsn = DatabaseConfig::from(
            ServerDatabase::mysql("app")
                .host("db.example.com")
                .charset("utf8mb4")
                .collation("utf8mb4_unicode_ci"),
        )
        .dsn()
        .unwrap();

        assert_eq!(
            dsn,
            "mysql://db.example.com:3306/app?charset=utf8mb4&collation=utf8mb4_unicode_ci"
        );
    }

    #[test]
    fn a_charset_and_collation_round_trip_through_the_wire_form() {
        let declared: DatabaseConfig = serde_json::from_value(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "charset": "utf8mb4",
            "collation": "utf8mb4_unicode_ci",
        }))
        .unwrap();

        let DatabaseConfig::Server(ref server) = declared else { panic!("declared as mysql") };
        assert_eq!(server.charset_name(), Some("utf8mb4"));
        assert_eq!(server.collation_name(), Some("utf8mb4_unicode_ci"));

        assert_eq!(
            serde_json::to_value(&declared).unwrap(),
            json!({
                "driver": "mysql",
                "host": "db.example.com",
                "database": "app",
                "charset": "utf8mb4",
                "collation": "utf8mb4_unicode_ci",
            })
        );
    }

    #[test]
    fn a_charset_alone_is_a_declaration_and_a_collation_alone_is_not() {
        // A character set has one default collation, which is the engine's own
        // convention — the same kind of thing as the default port. A collation
        // has no default character set, so alone it is checked against whatever
        // the driver assumes.
        let charset_only: DatabaseConfig = serde_json::from_value(json!({
            "driver": "mysql", "host": "h", "database": "app", "charset": "utf8mb4",
        }))
        .unwrap();
        assert_eq!(charset_only.dsn().unwrap(), "mysql://h:3306/app?charset=utf8mb4");

        let err = serde_json::from_value::<DatabaseConfig>(json!({
            "driver": "mysql", "host": "h", "database": "app", "collation": "utf8mb4_unicode_ci",
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`collation`"), "{err}");
        assert!(err.contains("no `charset`"), "{err}");
    }

    #[test]
    fn a_setting_only_one_engine_has_is_refused_on_the_others() {
        // Accepted, it would render into the connection string, be parsed as an
        // unknown parameter and dropped — a value in the file the server never
        // hears.
        for setting in [
            json!({ "charset": "utf8mb4" }),
            json!({ "charset": "utf8mb4", "collation": "utf8mb4_unicode_ci" }),
            json!({ "strict": true }),
        ] {
            let mut declaration =
                json!({ "driver": "postgres", "host": "db.example.com", "database": "app" });
            let object = declaration.as_object_mut().unwrap();
            for (key, value) in setting.as_object().unwrap() {
                object.insert(key.clone(), value.clone());
            }

            let err =
                serde_json::from_value::<DatabaseConfig>(declaration).unwrap_err().to_string();
            assert!(err.contains("does not use") || err.contains("has no"), "{err}");
        }
    }

    #[test]
    fn strict_edits_the_servers_sql_mode_rather_than_replacing_it() {
        // A connection arrives with modes on it that are not this setting's
        // business — the driver appends its own, and a parameter group has its
        // own. Assigning a list would drop whichever of those were not in it,
        // silently, and this is the assertion that stops somebody simplifying
        // the statement into one.
        let on = strict_sql_mode(true);
        let off = strict_sql_mode(false);

        for statement in [&on, &off] {
            assert!(statement.starts_with("SET SESSION sql_mode = "), "{statement}");
            assert!(
                statement.contains("@@sql_mode"),
                "the server's own modes survive: {statement}"
            );
            // Comma-wrapped, so a removal cannot match a mode that merely ends
            // in the name being removed, and leaves no doubled separator.
            assert!(statement.contains("CONCAT(',', @@sql_mode, ',')"), "{statement}");
            assert!(statement.contains("TRIM(BOTH ','"), "{statement}");
        }

        // Both directions remove first: on, so a mode already present is not
        // named twice; off, because removing is the whole job.
        assert!(on.contains("REPLACE"), "{on}");
        assert!(off.contains("REPLACE"), "{off}");
        assert!(on.ends_with("'STRICT_TRANS_TABLES,STRICT_ALL_TABLES'))"), "{on}");
        assert!(!off.contains("'STRICT_TRANS_TABLES,STRICT_ALL_TABLES'"), "{off}");
    }

    #[test]
    fn strict_is_carried_to_the_pool_because_no_connection_string_holds_it() {
        // The distinction that matters: `charset` is a connection-string
        // parameter and `strict` is not, so one is in the DSN and the other has
        // to reach every connection the pool opens by another route.
        let declared: DatabaseConfig = serde_json::from_value(json!({
            "driver": "mysql", "host": "db.example.com", "database": "app", "strict": true,
        }))
        .unwrap();

        assert_eq!(declared.dsn().unwrap(), "mysql://db.example.com:3306/app");
        assert!(!declared.dsn().unwrap().contains("sql_mode"));

        let session = declared.session_statements();
        assert_eq!(session.len(), 1, "{session:?}");
        assert!(session[0].contains("STRICT_ALL_TABLES"), "{session:?}");

        // Undeclared is undeclared: the server's own `sql_mode` is left alone
        // rather than being set to a guess about which way it should run.
        let silent: DatabaseConfig = serde_json::from_value(
            json!({ "driver": "mysql", "host": "db.example.com", "database": "app" }),
        )
        .unwrap();
        assert!(silent.session_statements().is_empty());

        // …and `false` is a declaration too, not an absence.
        let lenient: DatabaseConfig = serde_json::from_value(json!({
            "driver": "mysql", "host": "db.example.com", "database": "app", "strict": false,
        }))
        .unwrap();
        assert_eq!(lenient.session_statements().len(), 1);
    }

    #[test]
    fn strict_may_be_declared_beside_a_url_because_a_dsn_cannot_carry_it() {
        // The one setting that is not two answers to one question: MySQL
        // connection strings have no `sql_mode` parameter, so the platform's
        // injected DSN and this cannot contradict each other.
        let declared: DatabaseConfig = serde_json::from_value(json!({
            "driver": "mysql",
            "url": "mysql://app:shh@db.example.com/app",
            "strict": true,
        }))
        .unwrap();

        assert_eq!(declared.dsn().unwrap(), "mysql://app:shh@db.example.com/app");
        assert_eq!(declared.session_statements().len(), 1);

        // And it survives the wire form, so a dump and a reload declare the
        // same connection.
        assert_eq!(
            serde_json::to_value(&declared).unwrap(),
            json!({
                "driver": "mysql",
                "url": "mysql://app:shh@db.example.com/app",
                "strict": true,
            })
        );

        // The settings a DSN *does* carry are still refused beside it.
        let err = serde_json::from_value::<DatabaseConfig>(json!({
            "driver": "mysql", "url": "mysql://db.example.com/app", "charset": "utf8mb4",
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("declares `url` and also"), "{err}");
        assert!(err.contains("`charset`"), "{err}");

        // A DSN on an engine with no strict mode is refused rather than
        // carrying a statement nothing would run.
        let wrong_engine = serde_json::from_value::<DatabaseConfig>(json!({
            "driver": "postgres", "url": "postgres://db.example.com/app", "strict": true,
        }))
        .unwrap_err()
        .to_string();
        assert!(wrong_engine.contains("has no `strict`"), "{wrong_engine}");
    }

    // --- reaching the server another way ------------------------------------

    #[test]
    fn a_unix_socket_replaces_the_host_rather_than_standing_beside_it() {
        // Both engines read the socket from a place the URL has to have
        // something in, so the path is the authority. `localhost` there would
        // put a host in the boot log for a connection that never opens one.
        let mysql = DatabaseConfig::from(
            ServerDatabase::mysql("app").unix_socket("/var/run/mysqld/mysqld.sock").user("app"),
        );
        assert_eq!(
            mysql.dsn().unwrap(),
            "mysql://app@%2Fvar%2Frun%2Fmysqld%2Fmysqld.sock/app\
             ?socket=%2Fvar%2Frun%2Fmysqld%2Fmysqld.sock"
        );

        // PostgreSQL takes a host beginning with `/` as a socket path, so the
        // authority is the whole of it; MySQL only reads the parameter, so it
        // gets both.
        let postgres = DatabaseConfig::from(
            ServerDatabase::postgres("app").unix_socket("/var/run/postgresql").user("app"),
        );
        assert_eq!(postgres.dsn().unwrap(), "postgres://app@%2Fvar%2Frun%2Fpostgresql/app");

        // No port, either: there is nothing listening on one.
        assert!(!postgres.dsn().unwrap().contains("5432"));
    }

    #[test]
    fn a_socket_beside_a_host_is_refused_rather_than_one_winning() {
        for beside in [json!({ "host": "db.example.com" }), json!({ "port": 3306 })] {
            let mut declaration = json!({
                "driver": "mysql", "database": "app", "unix_socket": "/var/run/mysqld.sock",
            });
            let object = declaration.as_object_mut().unwrap();
            for (key, value) in beside.as_object().unwrap() {
                object.insert(key.clone(), value.clone());
            }

            let err =
                serde_json::from_value::<DatabaseConfig>(declaration).unwrap_err().to_string();
            assert!(err.contains("declares `unix_socket` and also"), "{err}");
            assert!(err.contains("not both"), "{err}");
        }

        // …and a socket alone satisfies the requirement a host otherwise does,
        // because it is the address.
        let declared: DatabaseConfig = serde_json::from_value(json!({
            "driver": "mysql", "database": "app", "unix_socket": "/var/run/mysqld.sock",
        }))
        .unwrap();
        let DatabaseConfig::Server(ref server) = declared else { panic!("declared as mysql") };
        assert_eq!(server.socket_path(), Some("/var/run/mysqld.sock"));
        assert_eq!(server.host_name(), "");
    }

    #[test]
    fn a_tls_ca_is_one_setting_both_engines_read() {
        // `ssl-ca` is MySQL's spelling and one of PostgreSQL's accepted aliases
        // for `sslrootcert`, so the section names it once.
        let mysql =
            DatabaseConfig::from(ServerDatabase::mysql("app").host("h").tls_ca("/etc/ssl/rds.pem"));
        assert_eq!(mysql.dsn().unwrap(), "mysql://h:3306/app?ssl-ca=%2Fetc%2Fssl%2Frds.pem");

        let postgres = DatabaseConfig::from(
            ServerDatabase::postgres("app").host("h").tls_ca("/etc/ssl/rds.pem"),
        );
        assert_eq!(postgres.dsn().unwrap(), "postgres://h:5432/app?ssl-ca=%2Fetc%2Fssl%2Frds.pem");

        // A CA is not a credential, but the redacted rendering drops the query
        // wholesale, so it is not in a log either.
        assert_eq!(mysql.url_without_credentials(), "mysql://h:3306/app");
    }

    #[test]
    fn a_socket_and_the_settings_beside_it_round_trip_through_the_wire_form() {
        for original in [
            json!({
                "driver": "mysql",
                "database": "app",
                "unix_socket": "/var/run/mysqld/mysqld.sock",
                "username": "app",
            }),
            json!({
                "driver": "mysql",
                "host": "db.example.com",
                "database": "app",
                "charset": "utf8mb4",
                "collation": "utf8mb4_unicode_ci",
                "strict": true,
                "ssl_ca": "/etc/ssl/rds.pem",
            }),
            json!({
                "driver": "postgres",
                "host": "db.example.com",
                "database": "app",
                "ssl_ca": "/etc/ssl/rds.pem",
            }),
        ] {
            let database: DatabaseConfig = serde_json::from_value(original.clone()).unwrap();
            assert_eq!(serde_json::to_value(&database).unwrap(), original);
        }
    }

    // --- settings this section does not honour ------------------------------

    #[test]
    fn a_table_prefix_is_refused_by_name_and_with_the_reason() {
        // Refused rather than accepted, because a prefix that reached the
        // entities and not the raw statements would send some queries to a
        // table that exists and is empty — which reads as missing data.
        for declaration in [
            json!({ "driver": "mysql", "host": "h", "database": "app", "prefix": "app_" }),
            json!({ "driver": "mysql", "host": "h", "database": "app", "prefix_indexes": true }),
            json!({ "driver": "sqlite", "database": "app.sqlite", "prefix": "app_" }),
            json!({ "driver": "mysql", "url": "mysql://h/app", "prefix": "app_" }),
        ] {
            let err =
                serde_json::from_value::<DatabaseConfig>(declaration).unwrap_err().to_string();

            assert!(err.contains("`prefix`"), "{err}");
            assert!(err.contains("not supported"), "{err}");
            // The message is the reason, not the spelling: an unknown-key error
            // sends somebody looking for the right name.
            assert!(err.contains("everywhere a table name is rendered"), "{err}");
        }
    }

    #[test]
    fn a_table_engine_is_refused_because_nothing_would_render_it() {
        // An accepted-and-ignored setting is worse than a refused one: the file
        // would say `InnoDB` and the table would be created with whatever the
        // server's default is.
        let err = serde_json::from_value::<DatabaseConfig>(json!({
            "driver": "mysql", "host": "h", "database": "app", "engine": "InnoDB",
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`engine`"), "{err}");
        assert!(err.contains("not settable here"), "{err}");
        assert!(err.contains("never hears"), "{err}");
    }

    // --- credentials --------------------------------------------------------

    #[test]
    fn credentials_default_to_nothing_declared() {
        // The safe direction: a connection that should have named a credential
        // is refused by the server, which is loud.
        let declared: DatabaseConfig = serde_json::from_value(
            json!({ "driver": "mysql", "host": "db.example.com", "database": "app" }),
        )
        .unwrap();

        let DatabaseConfig::Server(server) = declared else { panic!("declared as mysql") };
        assert!(matches!(server.credential_source(), DatabaseCredentials::None));
    }

    #[test]
    fn a_password_with_nobody_to_own_it_is_refused_rather_than_dropped() {
        let err = serde_json::from_value::<DatabaseConfig>(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "password": "shh",
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("no `username`"), "{err}");
        assert!(err.contains("ambient"), "{err}");
    }

    #[test]
    fn no_debug_rendering_discloses_a_credential() {
        // The one that has to hold whatever else changes: a config dump at boot
        // must not put the password in the log of every process that started.
        // Both of this section's shapes are here — the discrete password, and
        // the one a DSN carries inline in its userinfo.
        let databases = Databases::new("primary")
            .with(
                "primary",
                ServerDatabase::mysql("app_db")
                    .host("primary.example.com")
                    .credentials("app_user", "super-secret"),
            )
            .with(
                "replica",
                DsnDatabase::new(
                    DatabaseDriver::Postgres,
                    "postgres://reader:hunter2@replica.example.com:5432/app_db",
                ),
            )
            .with("reporting", SqliteDatabase::new("storage/reporting.sqlite"));

        let rendered = format!("{databases:?}");

        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("hunter2"), "{rendered}");

        // Still enough to tell three connections apart in a log — which is what
        // stops this passing because it renders nothing at all.
        assert!(rendered.contains("primary.example.com"), "{rendered}");
        assert!(rendered.contains("replica.example.com"), "{rendered}");
        assert!(rendered.contains("app_db"), "{rendered}");
        assert!(rendered.contains("app_user"), "the username is not the secret: {rendered}");
        assert!(rendered.contains("storage/reporting.sqlite"), "{rendered}");
    }

    #[test]
    fn a_password_survives_no_rendering_anywhere_on_the_way_through_a_section() {
        // The same assertion one level down, so a `Debug` added to any of the
        // pieces later cannot reintroduce the disclosure without failing here.
        let server = ServerDatabase::postgres("app")
            .host("db.example.com")
            .credentials("app", "super-secret");
        let dsn = DsnDatabase::new(
            DatabaseDriver::Postgres,
            "postgres://app:super-secret@db.example.com/app",
        );

        for rendered in [
            format!("{server:?}"),
            format!("{dsn:?}"),
            format!("{:?}", DatabaseConfig::from(server.clone())),
            format!("{:?}", DatabaseConfig::from(dsn.clone())),
            format!("{:?}", server.credential_source()),
            server.url_without_credentials(),
            dsn.url_without_credentials(),
        ] {
            assert!(!rendered.contains("super-secret"), "{rendered}");
        }

        // And the DSN — the one string that does carry it — is only ever
        // produced by asking for it by name.
        assert!(DatabaseConfig::from(server).dsn().unwrap().contains("super-secret"));
    }

    #[test]
    fn the_settings_added_beside_a_credential_do_not_bring_it_into_a_log() {
        // The same guarantee as the two tests above, held with every setting a
        // connection can now carry — a new field that widened `Debug`, or a DSN
        // rendering that started being printed, fails here.
        let server = ServerDatabase::mysql("app_db")
            .host("db.example.com")
            .credentials("app_user", "super-secret")
            .charset("utf8mb4")
            .collation("utf8mb4_unicode_ci")
            .strict(true)
            .tls_ca("/etc/ssl/rds.pem");
        let socketed = ServerDatabase::postgres("app_db")
            .unix_socket("/var/run/postgresql")
            .credentials("app_user", "super-secret");
        let dsn = DsnDatabase::new(
            DatabaseDriver::MySql,
            "mysql://app_user:super-secret@db.example.com/app_db",
        )
        .strict(true);

        for rendered in [
            format!("{server:?}"),
            format!("{socketed:?}"),
            format!("{dsn:?}"),
            format!("{:?}", DatabaseConfig::from(server.clone())),
            format!("{:?}", DatabaseConfig::from(socketed.clone())),
            format!("{:?}", DatabaseConfig::from(dsn.clone())),
            server.url_without_credentials(),
            socketed.url_without_credentials(),
            dsn.url_without_credentials(),
            // The statements are SQL about `sql_mode` and nothing else; this is
            // what stops a later one being built out of the declaration.
            DatabaseConfig::from(server.clone()).session_statements().join(" "),
            DatabaseConfig::from(dsn.clone()).session_statements().join(" "),
        ] {
            assert!(!rendered.contains("super-secret"), "{rendered}");
        }

        // Still enough to tell the connections apart — which is what stops this
        // passing because it renders nothing at all.
        assert!(format!("{server:?}").contains("db.example.com"));
        assert!(format!("{socketed:?}").contains("postgresql"));
        assert!(format!("{server:?}").contains("app_user"), "the username is not the secret");
    }

    #[test]
    fn a_url_that_does_not_parse_is_redacted_whole() {
        // The safe direction to be wrong in: a host nobody can read is an
        // inconvenience, a password in a log is an incident.
        assert_eq!(without_credentials("not a url at all"), "<redacted>");
        assert_eq!(without_credentials("postgres:host=db;password=hunter2"), "<redacted>");

        // A password in the query string goes too.
        assert_eq!(
            without_credentials("mysql://db.example.com:3306/app?password=hunter2"),
            "mysql://db.example.com:3306/app"
        );

        // An `@` in the path is not a userinfo.
        assert_eq!(
            without_credentials("postgres://db.example.com:5432/user@host"),
            "postgres://db.example.com:5432/user@host"
        );

        // SQLite authenticates nobody, so the one DSN shape the general rule
        // would redact whole stays readable.
        assert_eq!(without_credentials("sqlite::memory:"), "sqlite::memory:");
        assert_eq!(without_credentials("sqlite:app.db"), "sqlite:app.db");
        assert_eq!(
            without_credentials("sqlite://storage/app.sqlite?mode=rwc"),
            "sqlite://storage/app.sqlite"
        );
    }

    // --- building -----------------------------------------------------------

    #[tokio::test]
    async fn a_default_naming_an_undeclared_connection_fails_instead_of_falling_back() {
        let databases =
            Databases::new("primary").with("reporting", DatabaseConfig::sqlite_in_memory());

        let err = databases.build().await.err().expect("the default is undeclared");
        assert!(err.message().contains("`primary`"), "{}", err.message());
        assert!(err.message().contains("`reporting`"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_build_failure_names_the_connection_it_came_from() {
        // With a dozen connections declared, "needs a host" without a name is a
        // search rather than a fix.
        let databases = Databases::new("primary").with("primary", ServerDatabase::mysql("app"));

        let err = databases.build().await.err().expect("no host to connect to");
        assert!(err.message().starts_with("database connection `primary`:"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_driver_whose_feature_is_off_is_an_error_and_not_a_substitution() {
        // A quiet fallback to an in-memory SQLite database would "work": every
        // statement accepted, every migration applied, and every query about
        // the application's own data answered with no rows.
        let databases =
            Databases::new("primary").with("primary", DatabaseConfig::sqlite_in_memory());

        let built = databases.build().await;

        if cfg!(feature = "sea-orm-executor") {
            let manager = built.expect("sqlite in memory always opens");
            assert_eq!(manager.default_connection().dialect(), Dialect::Sqlite);
        } else {
            let err = built.err().expect("no executor to open with");
            assert!(
                err.message().contains("without the `sea-orm-executor` feature"),
                "{}",
                err.message()
            );
            assert!(err.message().contains("`sqlite`"), "{}", err.message());
        }
    }

    #[test]
    fn connections_on_different_drivers_do_not_collapse_into_one() {
        // Only SQLite opens without a server, so opening a MySQL and a SQLite
        // connection side by side is not something a unit test can do without a
        // network. What it can assert is the thing a shared DSN would have to
        // destroy first: each declaration carries its own driver, its own
        // dialect and its own connection string.
        //
        // The built halves of this property are next door —
        // `two_connections_declared_separately_are_two_databases` opens two and
        // shows a table in one is invisible in the other, and the manager's own
        // tests hold two backends on two dialects.
        let databases: Databases = serde_json::from_value(json!({
            "default": "reporting",
            "connections": {
                "primary": { "driver": "mysql", "host": "db.example.com", "database": "app" },
                "reporting": { "driver": "sqlite", "database": ":memory:" },
            },
        }))
        .unwrap();

        assert_eq!(databases.get("primary").unwrap().dialect(), Dialect::MySql);
        assert_eq!(databases.get("reporting").unwrap().dialect(), Dialect::Sqlite);
        assert_eq!(
            databases.get("primary").unwrap().dsn().unwrap(),
            "mysql://db.example.com:3306/app"
        );
        assert_eq!(databases.get("reporting").unwrap().dsn().unwrap(), "sqlite::memory:");
    }

    #[cfg(feature = "sea-orm-executor")]
    #[tokio::test]
    async fn two_connections_declared_separately_are_two_databases() {
        // Two in-memory SQLite databases are genuinely separate: each exists
        // only inside the connection that opened it. A shared pool, or one
        // declaration opened twice, collapses them — and this is what catches
        // that.
        let databases = Databases::new("primary")
            .with("primary", DatabaseConfig::sqlite_in_memory())
            .with("reporting", DatabaseConfig::sqlite_in_memory());

        let manager = databases.build().await.unwrap();

        manager
            .connection("primary")
            .unwrap()
            .statement("CREATE TABLE widgets (id INTEGER)")
            .await
            .unwrap();

        assert!(manager
            .connection("primary")
            .unwrap()
            .statement("SELECT 1 FROM widgets")
            .await
            .is_ok());
        assert!(
            manager
                .connection("reporting")
                .unwrap()
                .statement("SELECT 1 FROM widgets")
                .await
                .is_err(),
            "two databases, not one"
        );
    }

    #[cfg(feature = "sea-orm-executor")]
    #[tokio::test]
    async fn the_default_connection_is_the_same_database_as_its_name() {
        // Opened once, registered twice. Opening it twice would give
        // `connection("primary")` a second in-memory database, empty, where a
        // table created through one is invisible through the other.
        let manager = Databases::new("primary")
            .with("primary", DatabaseConfig::sqlite_in_memory())
            .build()
            .await
            .unwrap();

        manager.default_connection().statement("CREATE TABLE widgets (id INTEGER)").await.unwrap();

        assert!(manager
            .connection("primary")
            .unwrap()
            .statement("SELECT 1 FROM widgets")
            .await
            .is_ok());
    }

    #[cfg(feature = "sea-orm-executor")]
    #[tokio::test]
    async fn a_url_alone_opens_a_working_default_with_no_section_declared() {
        let manager = Databases::from_url("sqlite::memory:").unwrap().build().await.unwrap();

        assert_eq!(manager.default_connection().dialect(), Dialect::Sqlite);
        manager.default_connection().statement("CREATE TABLE widgets (id INTEGER)").await.unwrap();

        // Reachable under its name too, and it is the same database.
        assert!(manager
            .connection(Databases::DEFAULT_NAME)
            .unwrap()
            .statement("SELECT 1 FROM widgets")
            .await
            .is_ok());

        // And a name nobody declared is still not the default.
        assert!(manager.connection("reporting").is_none());
        assert!(manager.resolve(Some("reporting")).is_err());
    }

    // --- splitting reads from writes ----------------------------------------

    /// The declaration filed under `primary`, or the error reading it.
    fn connection(declaration: serde_json::Value) -> Result<DatabaseConfig> {
        serde_json::from_value::<DatabaseConfig>(declaration)
            .map_err(|e| Error::internal(e.to_string()))
    }

    #[test]
    fn a_connection_that_names_no_role_is_the_connection_it_always_was() {
        // The declaration every existing deployment has. One endpoint, and the
        // same string it rendered before any of this existed.
        let config = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "username": "app",
            "password": "secret",
        }))
        .expect("declared");

        assert!(!config.is_split());
        assert!(!config.is_sticky());
        assert_eq!(config.write_dsns().unwrap(), vec![config.dsn().unwrap()]);
        assert!(config.read_dsns().unwrap().is_empty(), "there is nowhere else to read from");
        assert_eq!(config.dsn().unwrap(), "mysql://app:secret@db.example.com:3306/app");
    }

    #[test]
    fn a_read_role_takes_the_reads_and_leaves_everything_else_where_it_was() {
        let config = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "read": { "host": ["replica-a.example.com", "replica-b.example.com"] },
            "database": "app",
            "username": "app",
            "password": "secret",
        }))
        .expect("declared");

        assert!(config.is_split());

        // The write role named nothing, so it is the connection's own host —
        // and its credentials, its port and its database.
        assert_eq!(
            config.write_dsns().unwrap(),
            vec!["mysql://app:secret@writer.example.com:3306/app"]
        );
        assert_eq!(
            config.read_dsns().unwrap(),
            vec![
                "mysql://app:secret@replica-a.example.com:3306/app",
                "mysql://app:secret@replica-b.example.com:3306/app",
            ]
        );
    }

    #[test]
    fn one_host_and_a_list_of_one_are_the_same_declaration() {
        let string = connection(json!({
            "driver": "postgres",
            "host": "writer.example.com",
            "read": { "host": "replica.example.com" },
            "database": "app",
        }))
        .expect("declared");
        let list = connection(json!({
            "driver": "postgres",
            "host": "writer.example.com",
            "read": { "host": ["replica.example.com"] },
            "database": "app",
        }))
        .expect("declared");

        assert_eq!(string.read_dsns().unwrap(), list.read_dsns().unwrap());
        assert_eq!(string.read_dsns().unwrap(), vec!["postgres://replica.example.com:5432/app"]);
    }

    #[test]
    fn a_role_may_authenticate_as_somebody_else_and_only_that_role_does() {
        // The arrangement the replicas usually arrive in: same servers or
        // different ones, reached as a read-only user.
        let config = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "database": "app",
            "username": "app",
            "password": "secret",
            "read": { "host": "replica.example.com", "username": "reader", "password": "other" },
        }))
        .expect("declared");

        assert_eq!(
            config.read_dsns().unwrap(),
            vec!["mysql://reader:other@replica.example.com:3306/app"]
        );
        assert_eq!(
            config.write_dsns().unwrap(),
            vec!["mysql://app:secret@writer.example.com:3306/app"]
        );
    }

    #[test]
    fn a_role_may_differ_in_nothing_but_its_credentials() {
        let config = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "username": "app",
            "password": "secret",
            "read": { "username": "reader", "password": "other" },
        }))
        .expect("declared");

        assert_eq!(
            config.read_dsns().unwrap(),
            vec!["mysql://reader:other@db.example.com:3306/app"]
        );
    }

    #[test]
    fn a_role_may_be_reached_on_its_own_port() {
        let config = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "port": 3307,
            "database": "app",
            "read": { "host": "replica.example.com", "port": 3308 },
        }))
        .expect("declared");

        assert_eq!(config.write_dsns().unwrap(), vec!["mysql://writer.example.com:3307/app"]);
        assert_eq!(config.read_dsns().unwrap(), vec!["mysql://replica.example.com:3308/app"]);
    }

    #[test]
    fn a_write_role_may_carry_every_address_and_leave_the_connection_none() {
        let config = connection(json!({
            "driver": "postgres",
            "database": "app",
            "read": { "host": ["replica-a.example.com", "replica-b.example.com"] },
            "write": { "host": "writer.example.com" },
        }))
        .expect("declared");

        assert_eq!(config.write_dsns().unwrap(), vec!["postgres://writer.example.com:5432/app"]);
        assert_eq!(config.read_dsns().unwrap().len(), 2);
    }

    #[test]
    fn a_role_that_resolves_to_no_host_is_refused_by_name() {
        // `write` names hosts and `read` does not, and neither does the
        // connection: reads would have to borrow the primary, which is the one
        // server the split exists to spare.
        let err = connection(json!({
            "driver": "mysql",
            "database": "app",
            "write": { "host": "writer.example.com" },
            "read": { "username": "reader" },
        }))
        .expect_err("no host for the reads");

        assert!(err.message().contains("`read`"), "{}", err.message());
        assert!(err.message().contains("resolves to no host"), "{}", err.message());
    }

    #[test]
    fn an_empty_role_is_refused_rather_than_read_as_a_split() {
        let err = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "read": {},
        }))
        .expect_err("an empty role");

        assert!(err.message().contains("empty `read`"), "{}", err.message());
    }

    #[test]
    fn sticky_with_nothing_to_be_sticky_about_is_refused() {
        let err = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "sticky": true,
        }))
        .expect_err("nothing to pin");

        assert!(err.message().contains("`sticky`"), "{}", err.message());
        assert!(err.message().contains("one endpoint"), "{}", err.message());

        // …and beside a role it is a setting.
        let config = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "database": "app",
            "read": { "host": "replica.example.com" },
            "sticky": true,
        }))
        .expect("declared");
        assert!(config.is_sticky());
    }

    #[test]
    fn a_role_that_names_another_database_or_driver_is_refused() {
        for (field, value) in [
            ("database", json!("reporting")),
            ("driver", json!("postgres")),
            ("unix_socket", json!("/tmp/x.sock")),
        ] {
            let err = connection(json!({
                "driver": "mysql",
                "host": "writer.example.com",
                "database": "app",
                "read": { "host": "replica.example.com", field: value },
            }))
            .expect_err("a role may not name it");

            assert!(err.message().contains(field), "{}: {}", field, err.message());
        }
    }

    #[test]
    fn a_roles_password_with_nobody_to_own_it_is_refused_too() {
        let err = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "database": "app",
            "read": { "host": "replica.example.com", "password": "secret" },
        }))
        .expect_err("nobody to own it");

        assert!(err.message().contains("`read`"), "{}", err.message());
        assert!(err.message().contains("ambient user"), "{}", err.message());
    }

    #[test]
    fn a_role_beside_a_socket_is_refused_rather_than_one_winning() {
        let err = connection(json!({
            "driver": "mysql",
            "unix_socket": "/var/run/mysqld/mysqld.sock",
            "database": "app",
            "read": { "host": "replica.example.com" },
        }))
        .expect_err("a socket has no second endpoint");

        assert!(err.message().contains("unix_socket"), "{}", err.message());
        assert!(err.message().contains("read"), "{}", err.message());
    }

    #[test]
    fn a_split_beside_a_url_is_refused_because_a_dsn_names_one_endpoint() {
        for field in ["read", "write"] {
            let err = connection(json!({
                "driver": "mysql",
                "url": "mysql://db.example.com/app",
                field: { "host": "replica.example.com" },
            }))
            .expect_err("a DSN has no room for a second endpoint");

            assert!(err.message().contains(field), "{}", err.message());
            assert!(err.message().contains("one endpoint"), "{}", err.message());
        }
    }

    #[test]
    fn a_split_is_refused_on_a_driver_that_has_no_second_server() {
        let err = connection(json!({
            "driver": "sqlite",
            "database": "storage/app.sqlite",
            "read": { "host": "replica.example.com" },
        }))
        .expect_err("a file has no replica");

        assert!(err.message().contains("`read`"), "{}", err.message());
    }

    #[test]
    fn a_split_round_trips_through_its_wire_form() {
        let declaration = json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "database": "app",
            "username": "app",
            "password": "secret",
            "read": {
                "host": ["replica-a.example.com", "replica-b.example.com"],
                "username": "reader",
                "password": "other",
            },
            "sticky": true,
            "options": { "ssl-mode": "VERIFY_CA" },
        });

        let config = connection(declaration.clone()).expect("declared");
        let written = serde_json::to_value(&config).expect("serialise");
        assert_eq!(
            written, declaration,
            "a section rewritten from a declaration is not the same one"
        );

        // And reading it back gives the same endpoints, credentials included.
        let again = connection(written).expect("declared");
        assert_eq!(again.read_dsns().unwrap(), config.read_dsns().unwrap());
        assert_eq!(again.write_dsns().unwrap(), config.write_dsns().unwrap());
    }

    #[test]
    fn no_rendering_of_a_split_discloses_a_roles_credential() {
        // A role carries its own password, so there is a second one to leak.
        let config = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "database": "app",
            "username": "app",
            "password": "hunter2",
            "read": { "host": "replica.example.com", "username": "reader", "password": "hunter3" },
            "sticky": true,
            "options": { "ssl-mode": "VERIFY_CA" },
        }))
        .unwrap_or_else(|e| panic!("{}", e.message()));

        for rendered in [
            format!("{config:?}"),
            config.url_without_credentials(),
            format!("{:?}", Databases::new("primary").with("primary", config.clone())),
        ] {
            assert!(!rendered.contains("hunter2"), "{rendered}");
            assert!(!rendered.contains("hunter3"), "{rendered}");
            // The usernames are not secrets, and two roles reaching one host as
            // two users are otherwise indistinguishable in a log.
            assert!(rendered.contains("replica.example.com") || !rendered.contains("reader"));
        }

        // The dump still says enough to tell what this connection is.
        let dumped = format!("{config:?}");
        assert!(dumped.contains("writer.example.com"), "{dumped}");
        assert!(dumped.contains("reader"), "{dumped}");
        assert!(dumped.contains("sticky"), "{dumped}");
    }

    #[test]
    fn a_url_that_a_role_could_not_survive_is_still_refused_whole() {
        // The `read` DSN carries the role's own password inline, exactly as the
        // write one does — and the safe rendering drops both.
        let config = connection(json!({
            "driver": "postgres",
            "host": "writer.example.com",
            "database": "app",
            "read": { "host": "replica.example.com", "username": "reader", "password": "hunter3" },
        }))
        .expect("declared");

        let dsn = &config.read_dsns().unwrap()[0];
        assert!(dsn.contains("hunter3"), "the DSN is the one place it belongs");
        assert!(!super::without_credentials(dsn).contains("hunter3"));
    }

    // --- driver options -----------------------------------------------------

    #[test]
    fn an_option_the_driver_reads_reaches_the_connection_string() {
        let config = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "options": { "ssl-mode": "VERIFY_IDENTITY", "statement-cache-capacity": "0" },
        }))
        .expect("declared");

        let dsn = config.dsn().unwrap();
        assert!(dsn.contains("ssl-mode=VERIFY_IDENTITY"), "{dsn}");
        assert!(dsn.contains("statement-cache-capacity=0"), "{dsn}");
    }

    #[test]
    fn an_option_the_driver_would_drop_is_refused_rather_than_rendered() {
        // The failure this exists for: sqlx ignores a parameter it does not
        // recognise, so the connection would be opened unverified while the
        // file said `VERIFY_CA`.
        let err = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "options": { "sslMode": "VERIFY_CA" },
        }))
        .expect_err("a spelling the parser does not read");

        assert!(err.message().contains("sslMode"), "{}", err.message());
        assert!(err.message().contains("dropped on arrival"), "{}", err.message());
        // And it names what it would have accepted.
        assert!(err.message().contains("`ssl-mode`"), "{}", err.message());
    }

    #[test]
    fn an_option_that_another_setting_already_answers_is_refused() {
        for (key, setting) in [
            ("charset", "charset"),
            ("dbname", "database"),
            ("ssl-ca", "ssl_ca"),
            ("password", "password"),
        ] {
            let err = connection(json!({
                "driver": "mysql",
                "host": "db.example.com",
                "database": "app",
                "options": { key: "…" },
            }))
            .expect_err("two answers to one question");

            assert!(err.message().contains(setting), "{key}: {}", err.message());
        }
    }

    #[test]
    fn each_driver_reads_its_own_parameters() {
        // `application_name` is PostgreSQL's and `timezone` is MySQL's, and
        // each is dropped in silence by the other's parser.
        let postgres = connection(json!({
            "driver": "postgres",
            "host": "db.example.com",
            "database": "app",
            "options": { "application_name": "reports", "options[search_path]": "public" },
        }))
        .expect("declared");
        assert!(postgres.dsn().unwrap().contains("application_name=reports"));

        let refused = connection(json!({
            "driver": "postgres",
            "host": "db.example.com",
            "database": "app",
            "options": { "timezone": "+00:00" },
        }))
        .expect_err("PostgreSQL's parser does not read it");
        assert!(refused.message().contains("timezone"), "{}", refused.message());

        assert!(connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "options": { "timezone": "+00:00" },
        }))
        .is_ok());
    }

    #[test]
    fn options_are_refused_on_a_driver_with_no_url_to_hang_them_on() {
        let err = connection(json!({
            "driver": "sqlite",
            "database": "storage/app.sqlite",
            "options": { "mode": "rwc" },
        }))
        .expect_err("a file connection writes them into a url");

        assert!(err.message().contains("`options`"), "{}", err.message());
    }

    #[test]
    fn an_option_value_that_would_split_the_url_is_encoded_rather_than_pasted() {
        let config = connection(json!({
            "driver": "postgres",
            "host": "db.example.com",
            "database": "app",
            "options": { "application_name": "reports&sslmode=disable" },
        }))
        .expect("declared");

        let dsn = config.dsn().unwrap();
        assert!(!dsn.contains("&sslmode=disable"), "a value became a second parameter: {dsn}");
        assert!(dsn.contains("%26sslmode%3Ddisable"), "{dsn}");
    }

    // --- sizing the pool -----------------------------------------------------

    #[test]
    fn a_connection_that_declares_no_pool_is_sized_exactly_as_it_was() {
        // The assertion that every existing application depends on, including
        // every test suite that opens an in-memory database.
        let default = rainier_orm::PoolConfig::default();
        let in_memory = rainier_orm::PoolConfig::in_memory();

        for (declaration, expected) in [
            (json!({ "driver": "mysql", "host": "db.example.com", "database": "app" }), &default),
            (json!({ "driver": "postgres", "url": "postgres://db.example.com/app" }), &default),
            (json!({ "driver": "sqlite", "database": "storage/app.sqlite" }), &default),
            // The one that is not tuning.
            (json!({ "driver": "sqlite", "database": ":memory:" }), &in_memory),
            (json!({ "driver": "sqlite", "url": "sqlite::memory:" }), &in_memory),
        ] {
            let config = connection(declaration.clone()).expect("declared");
            let pool = config.pool();

            assert_eq!(pool.max_connections, expected.max_connections, "{declaration}");
            assert_eq!(pool.min_connections, expected.min_connections, "{declaration}");
            assert_eq!(pool.acquire_timeout, expected.acquire_timeout, "{declaration}");
            assert_eq!(pool.idle_timeout, expected.idle_timeout, "{declaration}");
            assert_eq!(pool.max_lifetime, expected.max_lifetime, "{declaration}");
            assert_eq!(pool.test_before_acquire, expected.test_before_acquire, "{declaration}");

            // And with no split, reads are sized by the same declaration.
            assert_eq!(config.read_pool().max_connections, pool.max_connections);
            assert!(config.pool_settings().is_none());
        }
    }

    #[test]
    fn a_declared_field_changes_that_field_and_leaves_the_rest() {
        let config = connection(json!({
            "driver": "postgres",
            "host": "db.example.com",
            "database": "app",
            "pool": { "max_connections": 40, "acquire_timeout": 3 },
        }))
        .expect("declared");

        let pool = config.pool();
        let untouched = rainier_orm::PoolConfig::default();

        assert_eq!(pool.max_connections, 40);
        assert_eq!(pool.acquire_timeout, std::time::Duration::from_secs(3));
        assert_eq!(pool.min_connections, untouched.min_connections);
        assert_eq!(pool.idle_timeout, untouched.idle_timeout);
        assert_eq!(pool.max_lifetime, untouched.max_lifetime);
        assert_eq!(pool.test_before_acquire, untouched.test_before_acquire);
    }

    #[test]
    fn zero_turns_off_the_two_settings_that_can_be_turned_off() {
        // The only spelling of `None` a whole number of seconds has, and it is
        // needed: a connection with nothing in front of it that drops sockets
        // has no reason to recycle them.
        let config = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "pool": { "idle_timeout": 0, "max_lifetime": 0 },
        }))
        .expect("declared");

        assert_eq!(config.pool().idle_timeout, None);
        assert_eq!(config.pool().max_lifetime, None);

        // And a real number is still a real number.
        let recycled = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "pool": { "max_lifetime": 120 },
        }))
        .expect("declared");
        assert_eq!(recycled.pool().max_lifetime, Some(std::time::Duration::from_secs(120)));
    }

    #[test]
    fn a_role_is_sized_over_the_connection_rather_than_instead_of_it() {
        // The reason the roles are separate: the primary's budget is the scarce
        // one and there are three replicas sharing the read traffic.
        let config = connection(json!({
            "driver": "postgres",
            "host": "writer.example.com",
            "database": "app",
            "pool": { "max_connections": 8, "acquire_timeout": 4 },
            "read": {
                "host": ["replica-a.example.com", "replica-b.example.com"],
                "pool": { "max_connections": 25 },
            },
        }))
        .expect("declared");

        assert_eq!(config.pool().max_connections, 8);
        assert_eq!(config.read_pool().max_connections, 25);

        // The role changed one field; the connection's other answer survived.
        assert_eq!(config.read_pool().acquire_timeout, std::time::Duration::from_secs(4));
        assert_eq!(config.pool().acquire_timeout, std::time::Duration::from_secs(4));
    }

    #[test]
    fn a_role_that_declares_no_pool_is_sized_like_the_connection() {
        let config = connection(json!({
            "driver": "postgres",
            "host": "writer.example.com",
            "database": "app",
            "pool": { "max_connections": 12 },
            "read": { "host": "replica.example.com" },
        }))
        .expect("declared");

        assert_eq!(config.pool().max_connections, 12);
        assert_eq!(config.read_pool().max_connections, 12);
    }

    #[test]
    fn a_pool_that_could_hand_out_nothing_is_refused() {
        let err = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "pool": { "max_connections": 0 },
        }))
        .expect_err("nothing to hand a query");

        assert!(err.message().contains("max_connections"), "{}", err.message());
    }

    #[test]
    fn a_floor_above_its_own_ceiling_is_refused_even_when_the_ceiling_is_elsewhere() {
        // The cross-field mistake that only the *resolved* pool shows: 20 is a
        // perfectly reasonable number, against a maximum nobody restated.
        let err = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "pool": { "min_connections": 20 },
        }))
        .expect_err("a floor above the default ceiling");

        assert!(err.message().contains("min_connections"), "{}", err.message());
        assert!(err.message().contains("max_connections"), "{}", err.message());

        // …and the same mistake made across the two layers.
        let across = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "database": "app",
            "pool": { "max_connections": 4 },
            "read": { "host": "replica.example.com", "pool": { "min_connections": 6 } },
        }))
        .expect_err("the role's floor is above the connection's ceiling");
        assert!(across.message().contains("`read` role's"), "{}", across.message());
    }

    #[test]
    fn an_acquire_timeout_of_no_time_at_all_is_refused() {
        let err = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "pool": { "acquire_timeout": 0 },
        }))
        .expect_err("every connection being busy is not an error");

        assert!(err.message().contains("acquire_timeout"), "{}", err.message());
    }

    #[test]
    fn an_empty_pool_is_refused_wherever_it_is_written() {
        let connection_level = connection(json!({
            "driver": "mysql",
            "host": "db.example.com",
            "database": "app",
            "pool": {},
        }))
        .expect_err("it names nothing");
        assert!(
            connection_level.message().contains("empty `pool`"),
            "{}",
            connection_level.message()
        );

        let role_level = connection(json!({
            "driver": "mysql",
            "host": "writer.example.com",
            "database": "app",
            "read": { "host": "replica.example.com", "pool": {} },
        }))
        .expect_err("it names nothing");
        assert!(role_level.message().contains("`read`"), "{}", role_level.message());
    }

    #[test]
    fn an_in_memory_database_refuses_every_pool_it_would_not_survive() {
        // Each of these is silent: the database *is* the connection, so the
        // symptom is a process that migrates cleanly and then answers
        // `no such table`.
        for pool in [
            json!({ "max_connections": 5 }),
            json!({ "min_connections": 0 }),
            json!({ "idle_timeout": 60 }),
            json!({ "max_lifetime": 60 }),
        ] {
            for declaration in [
                json!({ "driver": "sqlite", "database": ":memory:", "pool": pool.clone() }),
                json!({ "driver": "sqlite", "url": "sqlite::memory:", "pool": pool.clone() }),
            ] {
                let err = connection(declaration).expect_err("an in-memory database survives none");
                assert!(err.message().contains("in-memory"), "{pool}: {}", err.message());
            }
        }

        // The two that change nothing load-bearing are still settings.
        let allowed = connection(json!({
            "driver": "sqlite",
            "database": ":memory:",
            "pool": { "acquire_timeout": 3, "test_before_acquire": true },
        }))
        .expect("neither of these can lose the schema");
        assert_eq!(allowed.pool().max_connections, 1);
        assert_eq!(allowed.pool().acquire_timeout, std::time::Duration::from_secs(3));
    }

    #[test]
    fn a_file_backed_sqlite_database_pools_like_anything_else() {
        let config = connection(json!({
            "driver": "sqlite",
            "database": "storage/app.sqlite",
            "pool": { "max_connections": 4, "idle_timeout": 0 },
        }))
        .expect("a file is not the connection");

        assert_eq!(config.pool().max_connections, 4);
        assert_eq!(config.pool().idle_timeout, None);
    }

    #[test]
    fn a_pool_may_be_declared_beside_a_url_because_a_dsn_cannot_carry_one() {
        // The shape most deployments get — one injected `DATABASE_URL` — and
        // the one that most needs sizing, because the default is sized for
        // somebody else's process count.
        let config = connection(json!({
            "driver": "postgres",
            "url": "postgres://app:secret@db.example.com/app",
            "pool": { "max_connections": 5, "max_lifetime": 300 },
        }))
        .expect("declared");

        assert_eq!(config.pool().max_connections, 5);
        assert_eq!(config.pool().max_lifetime, Some(std::time::Duration::from_secs(300)));
        // And the URL is still the whole connection.
        assert_eq!(config.dsn().unwrap(), "postgres://app:secret@db.example.com/app");
    }

    #[test]
    fn a_pool_round_trips_through_its_wire_form_in_every_shape() {
        for declaration in [
            json!({
                "driver": "mysql",
                "host": "writer.example.com",
                "database": "app",
                "pool": { "max_connections": 8, "idle_timeout": 0 },
                "read": { "host": "replica.example.com", "pool": { "max_connections": 30 } },
            }),
            json!({
                "driver": "postgres",
                "url": "postgres://db.example.com/app",
                "pool": { "acquire_timeout": 2, "test_before_acquire": true },
            }),
            json!({
                "driver": "sqlite",
                "database": "storage/app.sqlite",
                "pool": { "min_connections": 1 },
            }),
        ] {
            let config = connection(declaration.clone()).expect("declared");
            let written = serde_json::to_value(&config).expect("serialise");
            assert_eq!(written, declaration, "a section rewritten is not the one that was read");
        }
    }

    #[test]
    fn a_pool_is_refused_on_a_driver_whose_declaration_cannot_hold_one() {
        // There is no such driver today — every shape pools — so this asserts
        // the opposite: `pool` reaches all three rather than being quietly
        // dropped by the one that forgot to list it.
        for declaration in [
            json!({ "driver": "mysql", "host": "db.example.com", "database": "app",
                    "pool": { "max_connections": 3 } }),
            json!({ "driver": "sqlite", "database": "storage/app.sqlite",
                    "pool": { "max_connections": 3 } }),
            json!({ "driver": "postgres", "url": "postgres://db.example.com/app",
                    "pool": { "max_connections": 3 } }),
        ] {
            let config = connection(declaration.clone()).expect("declared");
            assert_eq!(config.pool().max_connections, 3, "{declaration}");
        }
    }

    // --- assembled in code rather than read from a file ----------------------

    #[test]
    fn a_declaration_assembled_in_code_splits_and_fails_the_same_way() {
        let config = DatabaseConfig::from(
            ServerDatabase::mysql("app")
                .host("writer.example.com")
                .credentials("app", "secret")
                .read(
                    DatabaseRole::across(["replica-a.example.com", "replica-b.example.com"])
                        .user("reader"),
                )
                .sticky(true),
        );

        assert!(config.is_split());
        assert!(config.is_sticky());
        assert_eq!(
            config.read_dsns().unwrap(),
            vec![
                "mysql://reader@replica-a.example.com:3306/app",
                "mysql://reader@replica-b.example.com:3306/app",
            ]
        );

        // The same refusal a file gets, from the same check.
        let refused =
            DatabaseConfig::from(ServerDatabase::mysql("app").host("db.example.com").sticky(true));
        assert!(refused.dsn().is_err());

        let bad_option = DatabaseConfig::from(
            ServerDatabase::mysql("app").host("db.example.com").option("sslMode", "VERIFY_CA"),
        );
        assert!(bad_option.dsn().is_err());
    }

    #[test]
    fn a_pool_assembled_in_code_resolves_and_fails_the_same_way() {
        let split = DatabaseConfig::from(
            ServerDatabase::postgres("app")
                .host("writer.example.com")
                .pool(PoolSettings::new().max_connections(6).max_lifetime(0))
                .read(
                    DatabaseRole::on("replica.example.com")
                        .pool(PoolSettings::new().max_connections(30)),
                ),
        );

        assert_eq!(split.pool().max_connections, 6);
        assert_eq!(split.read_pool().max_connections, 30);
        // The role took the connection's answer for everything it did not name.
        assert_eq!(split.read_pool().max_lifetime, None);

        // The same refusals a file gets, from the same checks.
        let in_memory = DatabaseConfig::from(
            SqliteDatabase::in_memory().pool(PoolSettings::new().max_connections(4)),
        );
        assert!(in_memory.dsn().is_err(), "a second connection is a second, empty database");

        let instant = DatabaseConfig::from(
            ServerDatabase::mysql("app")
                .host("db.example.com")
                .pool(PoolSettings::new().acquire_timeout(0)),
        );
        assert!(instant.dsn().is_err());
    }
}
