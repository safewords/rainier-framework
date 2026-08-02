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
//!
//! ## What this section deliberately does not carry
//!
//! **Pool settings.** [`PoolConfig`](rainier_orm::PoolConfig) is chosen from
//! the connection rather than declared, because the one case where getting it
//! wrong is silent — an in-memory SQLite database with more than one connection
//! is more than one *database* — has exactly one right answer and no reason to
//! be spelled out. Sizing a pool is a tuning decision with no wrong-data
//! failure mode; when it needs to be declarable it can be added here without
//! changing anything else.
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
            Self::Sqlite(sqlite) => Ok(sqlite.dsn()),
            Self::Dsn(dsn) => Ok(dsn.dsn().to_string()),
        }
    }

    /// How a pool for this connection has to be shaped.
    ///
    /// Only one case is load-bearing, and it is load-bearing in the silent
    /// direction: an in-memory SQLite database exists only as long as the
    /// connection holding it, so a second pooled connection is a second, empty
    /// database — and a query that lands on it returns no rows rather than an
    /// error. Read off the connection string rather than off the shape it was
    /// declared in, so `sqlite::memory:` gets the same treatment whether it
    /// arrived as a `database` or as a `url`.
    pub fn pool(&self) -> rainier_orm::PoolConfig {
        match self.dsn() {
            Ok(dsn) if is_in_memory(&dsn) => rainier_orm::PoolConfig::in_memory(),
            _ => rainier_orm::PoolConfig::default(),
        }
    }

    /// Open this connection, and only this connection.
    ///
    /// Every setting it uses comes from this declaration, so two connections
    /// opened from two declarations share nothing — not a pool, not a
    /// credential, not a host.
    ///
    /// # Errors
    ///
    /// When the declaration does not make sense, when no executor was compiled
    /// in, or when the database refuses the connection.
    pub async fn build(&self) -> Result<Database> {
        let dsn = self.dsn()?;

        #[cfg(feature = "sea-orm-executor")]
        {
            let executor =
                rainier_drivers::sql::SeaOrmExecutor::connect(&dsn, &self.pool()).await?;
            Ok(Database::new(executor))
        }

        // Loud, and naming the fix. There is nothing to fall back to that would
        // not be a lie: an in-memory SQLite database would accept every
        // statement, migrate cleanly, and answer every query about the
        // application's own data with no rows.
        #[cfg(not(feature = "sea-orm-executor"))]
        {
            let _ = dsn;
            Err(Error::internal(format!(
                "this connection uses the `{}` driver for `{}`, but rainier-database was built \
                 without the `sea-orm-executor` feature",
                self.driver(),
                self.url_without_credentials()
            )))
        }
    }
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
    database: String,
    credentials: DatabaseCredentials,
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
            database: database.into(),
            credentials: DatabaseCredentials::None,
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

    /// Which engine this speaks.
    pub fn driver(&self) -> DatabaseDriver {
        self.driver
    }

    /// The host this connects to.
    pub fn host_name(&self) -> &str {
        &self.host
    }

    /// The port, when one was declared.
    pub fn port_number(&self) -> Option<u16> {
        self.port
    }

    /// The name of the database this opens.
    pub fn database_name(&self) -> &str {
        &self.database
    }

    /// How this connection authenticates.
    pub fn credential_source(&self) -> &DatabaseCredentials {
        &self.credentials
    }

    /// The server this connects to, with any credentials removed.
    ///
    /// Enough to tell two connections apart in a log, and not enough to
    /// authenticate with.
    pub fn url_without_credentials(&self) -> String {
        format!("{}://{}/{}", self.driver.scheme(), self.authority(), self.database)
    }

    /// `host:port`, with the engine's standard port when none was declared.
    fn authority(&self) -> String {
        match self.port.or_else(|| self.driver.default_port()) {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    /// Whether this declaration can be opened.
    ///
    /// Checked when a declaration is deserialised so a bad `database` section
    /// fails while the configuration is being read, and again when the
    /// connection is opened so one assembled in code fails the same way with
    /// the same message.
    fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(Error::internal(format!(
                "the `{}` connection to `{}` declares no `host`; a guessed one is a different \
                 database, and the obvious guess is one that very often exists and answers",
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
        Ok(())
    }

    /// The connection string this opens. Carries the password inline.
    fn dsn(&self) -> Result<String> {
        self.validate()?;

        // Percent-encoded, and this is not cosmetic: a password containing `@`
        // or `/` splits the URL somewhere else, and the host the driver then
        // dials is not the host that was declared.
        let userinfo = match &self.credentials {
            DatabaseCredentials::None => String::new(),
            DatabaseCredentials::User { username } => format!("{}@", encode(username)),
            DatabaseCredentials::Password { username, password } => {
                format!("{}:{}@", encode(username), encode(password))
            }
        };

        Ok(format!(
            "{}://{userinfo}{}/{}",
            self.driver.scheme(),
            self.authority(),
            encode(&self.database)
        ))
    }
}

/// Names the server and never the password.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the credential into whatever logged the connection, which for a
/// configuration dump at boot means the password is in the log of every process
/// that started.
impl std::fmt::Debug for ServerDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerDatabase")
            .field("driver", &self.driver)
            .field("url", &self.url_without_credentials())
            // The credential is deliberately absent rather than redacted in
            // place: see `DatabaseCredentials`, whose own `Debug` names the
            // username and nothing else.
            .field("credentials", &self.credentials)
            .finish()
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
#[derive(Clone)]
pub struct DsnDatabase {
    driver: DatabaseDriver,
    url: String,
}

impl DsnDatabase {
    /// A connection to `url`, spoken to as `driver`.
    ///
    /// Use when the driver is already known. [`from_url`](Self::from_url) reads
    /// it off the scheme instead, which is what a bare `DATABASE_URL` needs.
    pub fn new(driver: DatabaseDriver, url: impl Into<String>) -> Self {
        Self { driver, url: url.into() }
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
        Self { database: path.into() }
    }

    /// A database in memory, for tests.
    ///
    /// Private to the connection that opens it, and gone when that connection
    /// closes. [`DatabaseConfig::pool`] answers
    /// [`PoolConfig::in_memory`](rainier_orm::PoolConfig::in_memory) for it,
    /// because a pool that opens a second connection opens a second *database*
    /// — empty, and answering rather than failing.
    pub fn in_memory() -> Self {
        Self { database: ":memory:".to_string() }
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
        let declared: [(&str, bool); 6] = [
            ("url", self.url.is_some()),
            ("host", self.host.is_some()),
            ("port", self.port.is_some()),
            ("database", self.database.is_some()),
            ("username", self.username.is_some()),
            ("password", self.password.is_some()),
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

        let also: Vec<String> = [
            ("host", self.host.is_some()),
            ("port", self.port.is_some()),
            ("database", self.database.is_some()),
            ("username", self.username.is_some()),
            ("password", self.password.is_some()),
        ]
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| format!("`{name}`"))
        .collect();

        if also.is_empty() {
            return Ok(());
        }

        Err(Error::internal(format!(
            "this connection declares `url` and also {}; a DSN carries the host, the database and \
             the credentials inline, so one of the two is ignored — and which one is not visible \
             from the file. Declare either the URL or the fields, not both",
            also.join(", ")
        )))
    }
}

impl TryFrom<RawDatabase> for DatabaseConfig {
    type Error = Error;

    fn try_from(raw: RawDatabase) -> Result<Self> {
        // Checked before the driver's own settings, because a `url` beside a
        // `host` has a reason of its own and `does not use` would not be it.
        raw.reject_a_url_beside_its_own_parts()?;

        // Whichever driver it names, a `url` is the whole connection. The
        // scheme is not re-derived from it: the declaration already said which
        // driver this is, and reading it twice is two answers to one question.
        if let Some(url) = raw.url {
            return Ok(Self::Dsn(DsnDatabase::new(raw.driver, url)));
        }

        match raw.driver {
            DatabaseDriver::Sqlite => {
                raw.reject_settings_it_ignores(&["url", "database"])?;

                let database = raw.database.ok_or_else(|| {
                    Error::internal(
                        "a `sqlite` connection needs a `database` to open — a file path, or \
                         `:memory:`. An assumed one is an empty database that migrates cleanly \
                         and answers every query with no rows",
                    )
                })?;
                Ok(Self::Sqlite(SqliteDatabase::new(database)))
            }

            driver => {
                let host = raw.host.ok_or_else(|| {
                    Error::internal(format!(
                        "a `{driver}` connection needs a `host` to connect to, or a `url` that \
                         names one"
                    ))
                })?;
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

                let server = ServerDatabase { driver, host, port: raw.port, database, credentials };
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
        };

        match database {
            DatabaseConfig::Dsn(dsn) => Self { url: Some(dsn.url), ..blank(dsn.driver) },

            DatabaseConfig::Sqlite(sqlite) => {
                Self { database: Some(sqlite.database), ..blank(DatabaseDriver::Sqlite) }
            }

            DatabaseConfig::Server(server) => {
                let (username, password) = match server.credentials {
                    DatabaseCredentials::None => (None, None),
                    DatabaseCredentials::User { username } => (Some(username), None),
                    DatabaseCredentials::Password { username, password } => {
                        (Some(username), Some(password))
                    }
                };
                Self {
                    host: Some(server.host),
                    port: server.port,
                    database: Some(server.database),
                    username,
                    password,
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
}
