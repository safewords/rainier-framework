//! More than one database — [`DatabaseManager`].
//!
//! A [`Database`] is one backend. That is the whole application for most
//! deployments, and this module changes nothing about it. It exists for the
//! ones where it is not: a read replica, a reporting warehouse, a second
//! database a legacy system also writes to.
//!
//! Those cannot be expressed with one handle, and the usual workaround —
//! passing a second `Database` around by hand — puts the choice of which
//! database a query runs against at every call site, where nobody reviewing a
//! diff can see it. A manager makes the choice a **name**, resolved in one
//! place.
//!
//! ## An undeclared name is not the default
//!
//! [`connection`](DatabaseManager::connection) answers `None` for a name nobody
//! declared, and [`resolve`](DatabaseManager::resolve) errors. Neither falls
//! back, and that is the point of the module rather than a detail of it: a
//! query against the wrong database does not raise. It answers — from the wrong
//! rows. A report built on it looks exactly like a report, an admin screen
//! shows numbers that are simply not this tenant's, and a write lands in a
//! table that something else owns.
//!
//! The same rule holds in the queue and filesystem sections for the same
//! reason, and this is the one where the wrong answer is hardest to notice.

use std::collections::BTreeMap;

use rainier_support::{Error, Result};

use crate::connection::Database;

/// A default [`Database`] and the named ones beside it.
///
/// Cheap to clone: every handle is an `Arc` over its connection.
#[derive(Clone)]
pub struct DatabaseManager {
    database: Database,

    /// The connections a query may name, by the name it names them with.
    ///
    /// A `BTreeMap` so an error that lists the declared connections reads the
    /// same each run. The default connection is in here too when it was
    /// declared — see [`Databases::build`](crate::Databases::build) — and is
    /// the *same* handle, not a second pool opened from the same declaration.
    connections: BTreeMap<String, Database>,
}

impl DatabaseManager {
    /// Query `database` when nothing names a connection.
    ///
    /// No name is registered: this is the default, and a query that does not
    /// ask for one goes here. Names come from
    /// [`with_connection`](Self::with_connection).
    pub fn new(database: Database) -> Self {
        Self { database, connections: BTreeMap::new() }
    }

    /// Declare a connection reachable as `name`.
    ///
    /// Also how a backend that no configuration file can describe joins the
    /// set — a `D1Executor` or a `LibSqlExecutor` is built from a
    /// caller-supplied transport rather than from settings, so it is wrapped in
    /// a [`Database`] and registered here.
    ///
    /// The default connection is registered here too when it is declared, under
    /// its own name and as the **same** handle. One built twice would give
    /// `connection("primary")` a second connection pool — and for an in-memory
    /// SQLite database, a second *database*, empty, where a write through one
    /// is invisible through the other.
    pub fn with_connection(mut self, name: impl Into<String>, database: Database) -> Self {
        self.connections.insert(name.into(), database);
        self
    }

    /// The database a query naming no connection runs against.
    pub fn default_connection(&self) -> &Database {
        &self.database
    }

    /// The connection declared as `name`, or `None`.
    ///
    /// `None` rather than the default, which is the whole point: a query
    /// against a connection nobody declared would otherwise run against
    /// whichever database happened to be the default and **come back with
    /// rows**. Nothing raises, nothing retries, and the answer is wrong in a
    /// way no caller can tell from right.
    pub fn connection(&self, name: &str) -> Option<&Database> {
        self.connections.get(name)
    }

    /// Whether `name` is declared.
    pub fn has_connection(&self, name: &str) -> bool {
        self.connections.contains_key(name)
    }

    /// Every declared connection name, in a stable order.
    pub fn connection_names(&self) -> impl Iterator<Item = &str> {
        self.connections.keys().map(String::as_str)
    }

    /// The database a query naming `connection` runs against.
    ///
    /// `None` means the default. A name that is not declared is an error and
    /// never the default — see [`connection`](Self::connection).
    ///
    /// # Errors
    ///
    /// When `connection` names something that was not declared.
    pub fn resolve(&self, connection: Option<&str>) -> Result<&Database> {
        let Some(name) = connection else {
            return Ok(&self.database);
        };

        self.connection(name).ok_or_else(|| {
            Error::internal(format!(
                "no database connection named `{name}` is declared; declared connections are {}. \
                 Querying the default instead would answer from a different database, which \
                 returns rows rather than an error and reads exactly like a correct answer",
                self.declared()
            ))
        })
    }

    /// The declared names, backtick-quoted, for an error message.
    fn declared(&self) -> String {
        if self.connections.is_empty() {
            return "none".to_string();
        }
        self.connection_names().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
    }
}

impl From<Database> for DatabaseManager {
    /// One database, reachable only as the default.
    ///
    /// The single-connection application, which is nearly all of them: nothing
    /// is named because there is nothing to distinguish.
    fn from(database: Database) -> Self {
        Self::new(database)
    }
}

impl std::fmt::Debug for DatabaseManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Database`'s own `Debug` names its dialect and shard family and not
        // its DSN, so this cannot leak a password however many connections it
        // holds — see `Databases` for where the DSN is kept.
        f.debug_struct("DatabaseManager")
            .field("default", &self.database)
            .field("connections", &self.connections)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_database, MemoryConnection};
    use crate::Dialect;

    #[test]
    fn a_declared_name_resolves_and_an_undeclared_one_does_not() {
        let (primary, _) = fake_database(MemoryConnection::new(Dialect::MySql));
        let (reporting, _) = fake_database(MemoryConnection::new(Dialect::Postgres));

        let manager = DatabaseManager::new(primary.clone())
            .with_connection("primary", primary)
            .with_connection("reporting", reporting);

        assert!(manager.connection("primary").is_some());
        assert!(manager.has_connection("reporting"));

        // Not the default. A report built on the default's rows looks like a
        // report.
        assert!(manager.connection("reportng").is_none());
        assert!(!manager.has_connection("reportng"));
    }

    #[tokio::test]
    async fn two_connections_are_two_databases() {
        let (primary, primary_connection) = fake_database(MemoryConnection::new(Dialect::MySql));
        let (reporting, reporting_connection) =
            fake_database(MemoryConnection::new(Dialect::Postgres));

        let manager = DatabaseManager::new(primary.clone())
            .with_connection("primary", primary)
            .with_connection("reporting", reporting);

        manager.connection("reporting").unwrap().statement("SELECT 1").await.unwrap();

        assert_eq!(reporting_connection.statement_count(), 1);
        assert_eq!(primary_connection.statement_count(), 0, "two backends, not one");
    }

    #[test]
    fn the_dialects_are_the_ones_each_connection_declared() {
        let (primary, _) = fake_database(MemoryConnection::new(Dialect::MySql));
        let (reporting, _) = fake_database(MemoryConnection::new(Dialect::Postgres));

        let manager = DatabaseManager::new(primary.clone())
            .with_connection("primary", primary)
            .with_connection("reporting", reporting);

        assert_eq!(manager.connection("primary").unwrap().dialect(), Dialect::MySql);
        assert_eq!(manager.connection("reporting").unwrap().dialect(), Dialect::Postgres);
    }

    #[test]
    fn resolving_nothing_is_the_default_and_resolving_a_typo_is_an_error() {
        let (primary, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let manager = DatabaseManager::new(primary.clone()).with_connection("primary", primary);

        assert_eq!(manager.resolve(None).unwrap().dialect(), Dialect::Sqlite);
        assert!(manager.resolve(Some("primary")).is_ok());

        let err = manager.resolve(Some("reporting")).err().expect("undeclared");
        assert!(err.message().contains("`reporting`"), "{}", err.message());
        assert!(err.message().contains("`primary`"), "{}", err.message());
    }

    #[test]
    fn a_lone_database_is_a_manager_with_no_names() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let manager = DatabaseManager::from(db);

        assert_eq!(manager.connection_names().count(), 0);
        assert!(manager.resolve(None).is_ok());
        assert!(manager.resolve(Some("primary")).is_err());
    }

    #[test]
    fn the_dump_names_the_connections_and_no_dsn() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let manager = DatabaseManager::new(db.clone()).with_connection("primary", db);

        let rendered = format!("{manager:?}");
        assert!(rendered.contains("primary"), "{rendered}");
        assert!(rendered.contains("Sqlite"), "{rendered}");
    }
}
