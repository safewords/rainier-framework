//! Test doubles — [`MemoryConnection`] and [`fake_database`].
//!
//! A repository test should not need a database. This fake records the SQL a
//! repository emits and replays queued rows, so the behaviour *around* the
//! query — the hooks, the pagination arithmetic, the not-found handling, the
//! shard route chosen — is testable with no server, schema or migration.
//!
//! It is **not** a SQL engine: it never parses the statements it is given. Use
//! it to test the layer above the ORM, and a real SQLite executor to test the
//! SQL itself.

use std::sync::{Arc, Mutex};

use rainier_orm::sea_query::Value;
use rainier_orm::{Dialect, ExecOutcome, Row, ShardRoute};
use rainier_support::{BoxFuture, Error, Result};

use crate::connection::{Connection, Database};
use crate::row::{ColumnRequest, OwnedRow};

/// One recorded statement.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedStatement {
    /// The SQL that was run.
    pub sql: String,
    /// Its bind values.
    pub params: Vec<Value>,
    /// The shard it was routed to.
    pub route: ShardRoute,
}

/// A [`Connection`] that records statements and replays queued rows.
#[derive(Default)]
pub struct MemoryConnection {
    dialect: Option<Dialect>,
    shard_family: Option<String>,
    allocated_id: Option<u64>,
    recorded: Mutex<Vec<RecordedStatement>>,
    queued: Mutex<Vec<Vec<OwnedRow>>>,
    outcome: Mutex<ExecOutcome>,
    failure: Option<String>,
}

impl MemoryConnection {
    /// A connection speaking `dialect` that returns no rows and reports one
    /// affected row per write.
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect: Some(dialect),
            outcome: Mutex::new(ExecOutcome { rows_affected: 1, last_insert_id: 1 }),
            ..Self::default()
        }
    }

    /// Queue the rows the next query should return.
    ///
    /// Answers are consumed in order; once the queue is empty, queries return
    /// nothing.
    pub fn returning(self, rows: impl IntoIterator<Item = OwnedRow>) -> Self {
        self.queued.lock().expect("queue lock poisoned").push(rows.into_iter().collect());
        self
    }

    /// Set the outcome every write reports.
    pub fn with_outcome(self, rows_affected: u64, last_insert_id: i64) -> Self {
        *self.outcome.lock().expect("outcome lock poisoned") =
            ExecOutcome { rows_affected, last_insert_id };
        self
    }

    /// Present as a sharded connector in the named family.
    pub fn sharded(mut self, family: impl Into<String>) -> Self {
        self.shard_family = Some(family.into());
        self
    }

    /// Mint this id whenever the repository asks for a shard-encoded key.
    pub fn allocating(mut self, id: u64) -> Self {
        self.allocated_id = Some(id);
        self
    }

    /// Make every statement fail with `message`.
    pub fn failing(mut self, message: impl Into<String>) -> Self {
        self.failure = Some(message.into());
        self
    }

    /// Every statement run so far, in order.
    pub fn recorded(&self) -> Vec<RecordedStatement> {
        self.recorded.lock().expect("recorded lock poisoned").clone()
    }

    /// The SQL of every statement run so far.
    pub fn statements(&self) -> Vec<String> {
        self.recorded().into_iter().map(|r| r.sql).collect()
    }

    /// The most recent statement's SQL.
    pub fn last_statement(&self) -> Option<String> {
        self.recorded().last().map(|r| r.sql.clone())
    }

    /// The most recent statement's shard route.
    pub fn last_route(&self) -> Option<ShardRoute> {
        self.recorded().last().map(|r| r.route)
    }

    /// The bind values of every statement run so far.
    pub fn bindings(&self) -> Vec<Vec<Value>> {
        self.recorded().into_iter().map(|r| r.params).collect()
    }

    /// How many statements have run.
    pub fn statement_count(&self) -> usize {
        self.recorded.lock().expect("recorded lock poisoned").len()
    }

    fn record(&self, route: ShardRoute, sql: &str, params: Vec<Value>) -> Result<()> {
        self.recorded.lock().expect("recorded lock poisoned").push(RecordedStatement {
            sql: sql.to_string(),
            params,
            route,
        });
        match &self.failure {
            Some(message) => Err(Error::internal(message.clone())),
            None => Ok(()),
        }
    }

    fn take_rows(&self) -> Vec<OwnedRow> {
        let mut queued = self.queued.lock().expect("queue lock poisoned");
        if queued.is_empty() {
            return Vec::new();
        }
        queued.remove(0)
    }
}

impl std::fmt::Debug for MemoryConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryConnection")
            .field("dialect", &self.dialect)
            .field("statements", &self.statement_count())
            .finish()
    }
}

impl Connection for MemoryConnection {
    fn dialect(&self) -> Dialect {
        self.dialect.unwrap_or(Dialect::Sqlite)
    }

    fn shard_family(&self) -> Option<String> {
        self.shard_family.clone()
    }

    fn allocate_id(&self, _shard_key: u64) -> Option<u64> {
        self.allocated_id
    }

    fn fetch<'a>(
        &'a self,
        route: ShardRoute,
        sql: &'a str,
        params: Vec<Value>,
        _columns: Vec<ColumnRequest>,
    ) -> BoxFuture<'a, Result<Vec<OwnedRow>>> {
        Box::pin(async move {
            self.record(route, sql, params)?;
            // Queued rows are handed back as-is rather than re-snapshotted,
            // so a test can queue exactly the cells it wants a decoder to see.
            Ok(self.take_rows())
        })
    }

    fn execute<'a>(
        &'a self,
        route: ShardRoute,
        sql: &'a str,
        params: Vec<Value>,
    ) -> BoxFuture<'a, Result<Vec<ExecOutcome>>> {
        Box::pin(async move {
            self.record(route, sql, params)?;
            Ok(vec![*self.outcome.lock().expect("outcome lock poisoned")])
        })
    }

    fn fetch_raw<'a>(
        &'a self,
        route: ShardRoute,
        sql: &'a str,
        params: Vec<Value>,
    ) -> BoxFuture<'a, Result<Vec<Box<dyn Row>>>> {
        Box::pin(async move {
            self.record(route, sql, params)?;
            Ok(self.take_rows().into_iter().map(|row| Box::new(row) as Box<dyn Row>).collect())
        })
    }
}

/// A [`Database`] over a [`MemoryConnection`], plus a handle to the connection
/// for assertions.
pub fn fake_database(connection: MemoryConnection) -> (Database, Arc<MemoryConnection>) {
    let connection = Arc::new(connection);
    (Database::from_arc(connection.clone()), connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statement;

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
        title: String,
    }

    #[tokio::test]
    async fn records_the_statement_bindings_and_route() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));

        db.execute(statement::select_by_pk::<Post>(Dialect::Sqlite, 7_i64.into())).await.unwrap();

        let recorded = connection.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].sql.contains("posts"));
        // The key, plus the `LIMIT 1` sea-query parameterises.
        assert_eq!(recorded[0].params.len(), 2);
        assert_eq!(recorded[0].params[0], 7_i64.into());
        assert_eq!(recorded[0].route, ShardRoute::Global);
    }

    #[tokio::test]
    async fn queued_rows_are_served_once_each() {
        let connection = MemoryConnection::new(Dialect::Sqlite)
            .returning([OwnedRow::new().with("id", 1_u64).with("title", "a")])
            .returning([
                OwnedRow::new().with("id", 2_u64).with("title", "b"),
                OwnedRow::new().with("id", 3_u64).with("title", "c"),
            ]);
        let (db, _) = fake_database(connection);

        let first: Vec<Post> =
            db.fetch_all(statement::select_all::<Post>(Dialect::Sqlite)).await.unwrap();
        assert_eq!(first.len(), 1);

        let second: Vec<Post> =
            db.fetch_all(statement::select_all::<Post>(Dialect::Sqlite)).await.unwrap();
        assert_eq!(second.len(), 2);

        let third: Vec<Post> =
            db.fetch_all(statement::select_all::<Post>(Dialect::Sqlite)).await.unwrap();
        assert!(third.is_empty(), "the queue is exhausted");
    }

    #[tokio::test]
    async fn write_outcomes_are_configurable() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite).with_outcome(5, 42));
        let outcome = db.statement("UPDATE posts SET title = 'x'").await.unwrap();

        assert_eq!(outcome.rows_affected, 5);
        assert_eq!(outcome.last_insert_id, 42);
    }

    #[tokio::test]
    async fn a_failing_connection_still_records_the_attempt() {
        let (db, connection) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).failing("boom"));

        assert!(db.statement("SELECT 1").await.is_err());
        assert_eq!(connection.statement_count(), 1);
    }

    #[test]
    fn a_sharded_fake_reports_its_family_and_mints_ids() {
        let connection = MemoryConnection::new(Dialect::Sqlite).sharded("users").allocating(4242);
        assert_eq!(connection.shard_family().as_deref(), Some("users"));
        assert_eq!(connection.allocate_id(1), Some(4242));

        let (db, _) = fake_database(connection);
        assert!(db.is_sharded());
    }
}
