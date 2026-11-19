//! [`Executor`] over a `sea_orm::DatabaseConnection` — one impl covering
//! native MySQL, Postgres, and SQLite (whatever the connection was opened
//! against).
//!
//! Feature-gated (`sea-orm-executor`) and off by default so the core crate
//! stays wasm-safe: `sea-orm` pulls in `sqlx`/`tokio`, which the Worker/D1
//! path must not. On a server surface this is the default backend.

use chrono::{DateTime, NaiveDate, Utc};
use rainier_orm::sea_query::Value;
use rainier_orm::{Dialect, Error, ExecOutcome, Executor, PoolConfig, Result, Row};
use std::sync::{Arc, Mutex};

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DatabaseTransaction, DbBackend,
    QueryResult, Statement, TransactionTrait, Value as SeaValue,
};

/// The level sqlx logs every statement at.
///
/// sqlx's default is `INFO`, under the `sqlx::query` target, so an
/// application at `RUST_LOG=info` wrote one log line per statement — the SQL,
/// its timing and its row counts, which Rainier's own `db.query` spans already
/// record. A worker polling a database queue wrote one a second, forever. At
/// `DEBUG` it is still there for `RUST_LOG=sqlx::query=debug`. sqlx's
/// slow-statement warning is a separate setting and is left as it is.
const STATEMENT_LOG_LEVEL: log::LevelFilter = log::LevelFilter::Debug;

/// Wraps a live sea-orm connection and runs rendered SQL through it.
///
/// The inner `DatabaseConnection` *is* a connection pool; `Clone` is cheap and
/// shares it, so one executor can live in shared application state and be
/// cloned per request without opening new connections. See [`rainier_orm::pool`] for
/// how pooling differs on serverless.
#[derive(Clone)]
pub struct SeaOrmExecutor {
    db: DatabaseConnection,
    dialect: Dialect,
    /// How many connections a read may find closed before it gives up: the
    /// pool's size, so one fresh connection is always reached. See
    /// [`is_dead_connection`].
    dead_connection_retries: u32,
}

/// Retries for an executor adopted with [`SeaOrmExecutor::new`], whose pool
/// size it was not told: the server default's ceiling.
const DEFAULT_DEAD_CONNECTION_RETRIES: u32 = 10;

impl SeaOrmExecutor {
    /// Adopt an existing connection, deriving the dialect from the backend it
    /// was opened with.
    pub fn new(db: DatabaseConnection) -> Self {
        let dialect = match db.get_database_backend() {
            DbBackend::MySql => Dialect::MySql,
            DbBackend::Postgres => Dialect::Postgres,
            DbBackend::Sqlite => Dialect::Sqlite,
            // `DatabaseBackend` is non-exhaustive since sea-orm 2; this crate
            // compiles only the three sqlx backends, so nothing else reaches here.
            other => panic!("rainier has no SQL dialect for the {other:?} backend"),
        };
        Self { db, dialect, dead_connection_retries: DEFAULT_DEAD_CONNECTION_RETRIES }
    }

    fn sized(mut self, pool: &PoolConfig) -> Self {
        self.dead_connection_retries = pool.max_connections.max(1);
        self
    }

    /// Open a pooled connection to `url` with the given [`PoolConfig`], then
    /// wrap it. The single entry point for getting a *pooled* executor; the
    /// pool lives inside the returned value and is shared by every clone.
    ///
    /// On serverless prefer `PoolConfig::serverless()` and front the database
    /// with a server-side pooler — see [`rainier_orm::pool`].
    pub async fn connect(url: &str, pool: &PoolConfig) -> Result<Self> {
        Self::connect_with_session(url, pool, &[]).await
    }

    /// The same, plus SQL run on **every** connection the pool opens.
    ///
    /// For session variables a connection string cannot carry. MySQL's
    /// `sql_mode` is the case this exists for: it decides whether an over-long
    /// value is an error or a silent truncation, and no MySQL DSN parameter
    /// sets it.
    ///
    /// *Every* connection is the whole point. A pool opens more than one, and
    /// opens replacements for the ones a server times out; a `SET SESSION`
    /// issued once through the pool lands on whichever connection was checked
    /// out at the time, leaving the rest on the server's setting. That is worse
    /// than not supporting it, because the same write then behaves differently
    /// depending on which connection served it. So the statements are attached
    /// to the pool's own connect hook, which runs once per connection for the
    /// life of the pool.
    ///
    /// # Errors
    ///
    /// When `session` is non-empty and `url` is not a MySQL DSN — the hook is
    /// wired for that backend only, and accepting the statements silently would
    /// be the ignored setting this exists to avoid. When the database refuses
    /// the connection, or refuses one of the statements.
    pub async fn connect_with_session(
        url: &str,
        pool: &PoolConfig,
        session: &[String],
    ) -> Result<Self> {
        if session.is_empty() {
            let mut opts = ConnectOptions::new(url.to_owned());
            opts.max_connections(pool.max_connections)
                .min_connections(pool.min_connections)
                .acquire_timeout(pool.acquire_timeout)
                .test_before_acquire(pool.test_before_acquire)
                .sqlx_logging_level(STATEMENT_LOG_LEVEL);
            // After `test_before_acquire`, which it turns off: sea-orm runs the
            // idle check in sqlx's `before_acquire` instead of pinging every time.
            if let (true, Some(idle)) = (pool.test_before_acquire, pool.test_after_idle) {
                opts.test_before_acquire_if_idle_for(idle);
            }
            if let Some(idle) = pool.idle_timeout {
                opts.idle_timeout(idle);
            }
            if let Some(life) = pool.max_lifetime {
                opts.max_lifetime(life);
            }
            let db = Database::connect(opts).await.map_err(Error::from)?;
            return Ok(Self::new(db).sized(pool));
        }

        if !url.starts_with("mysql://") && !url.starts_with("mariadb://") {
            return Err(Error::msg(
                "per-connection session SQL is only wired for the MySQL backend; this connection \
                 string names another engine, and running the statements on one connection out of \
                 a pool would leave the rest on the server's own setting",
            ));
        }

        // Only this path is hand-built. A connection that asks for nothing extra
        // still goes through sea-orm's own connector above, so the pool every
        // existing deployment opens is byte-for-byte the one it opened before.
        let statements: Vec<String> = session.to_vec();
        let options = url.parse::<sea_orm::sqlx::mysql::MySqlConnectOptions>().map_err(|e| {
            // Never the URL: a DSN carries its password inline, and this
            // is a log line.
            Error::msg(format!("this MySQL connection string cannot be parsed: {e}"))
        })?;
        let options = sea_orm::sqlx::ConnectOptions::log_statements(options, STATEMENT_LOG_LEVEL);

        let mut pool_options = sea_orm::sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(pool.max_connections)
            .min_connections(pool.min_connections)
            .acquire_timeout(pool.acquire_timeout)
            .test_before_acquire(pool.test_before_acquire)
            .idle_timeout(pool.idle_timeout)
            .max_lifetime(pool.max_lifetime);

        // The same idle-only ping the sea-orm path gets from
        // `test_before_acquire_if_idle_for`, written out: sqlx runs both its own
        // per-acquire ping and this hook, so the flag goes off when the hook
        // takes over.
        if let (true, Some(idle)) = (pool.test_before_acquire, pool.test_after_idle) {
            pool_options =
                pool_options.test_before_acquire(false).before_acquire(move |conn, meta| {
                    Box::pin(async move {
                        if meta.idle_for >= idle {
                            sea_orm::sqlx::Connection::ping(conn).await?;
                        }
                        Ok(true)
                    })
                });
        }

        pool_options = pool_options.after_connect(move |conn, _meta| {
            let statements = statements.clone();
            Box::pin(async move {
                for statement in &statements {
                    // Owned SQL: sqlx 0.9 wants the text to outlive the connection borrow.
                    // These are the configured session statements, not user input.
                    sea_orm::sqlx::raw_sql(sea_orm::sqlx::AssertSqlSafe(statement.clone()))
                        .execute(&mut *conn)
                        .await?;
                }
                Ok(())
            })
        });

        let sqlx_pool = pool_options.connect_with(options).await.map_err(Error::from)?;
        Ok(Self::new(sea_orm::SqlxMySqlConnector::from_sqlx_mysql_pool(sqlx_pool)).sized(pool))
    }

    /// Begin a transaction on one of the pool's connections.
    ///
    /// The connection is the transaction's until [`SeaOrmTransaction::commit`]
    /// or [`SeaOrmTransaction::rollback`]; dropped without either, it rolls
    /// back. Every statement run through the returned executor runs inside it.
    pub async fn begin(&self) -> Result<SeaOrmTransaction> {
        let txn = self.db.begin().await.map_err(Error::from)?;
        Ok(SeaOrmTransaction { txn: Mutex::new(Some(Arc::new(txn))), dialect: self.dialect })
    }

    /// Borrow the underlying connection (for migrations, transactions, or
    /// escape-hatch queries the generic layer doesn't cover).
    pub fn connection(&self) -> &DatabaseConnection {
        &self.db
    }

    fn backend(&self) -> DbBackend {
        backend_of(self.dialect)
    }

    fn statement(&self, sql: &str, params: Vec<Value>) -> Statement {
        let values: Vec<SeaValue> = params.into_iter().collect();
        Statement::from_sql_and_values(self.backend(), sql, values)
    }
}

fn backend_of(dialect: Dialect) -> DbBackend {
    match dialect {
        Dialect::MySql => DbBackend::MySql,
        Dialect::Postgres => DbBackend::Postgres,
        Dialect::Sqlite => DbBackend::Sqlite,
    }
}

fn rows_of(rows: Vec<QueryResult>, dialect: Dialect) -> Vec<Box<dyn Row>> {
    rows.into_iter().map(|r| Box::new(SeaOrmRow { row: r, dialect }) as Box<dyn Row>).collect()
}

fn outcome_of(res: &sea_orm::ExecResult, dialect: Dialect) -> ExecOutcome {
    // Postgres has no "last insert id" — an insert that wants its key asks
    // with `RETURNING` — and sea-orm 2 panics rather than answering 0 when
    // asked anyway.
    let last_insert_id = match dialect {
        Dialect::Postgres => 0,
        Dialect::MySql | Dialect::Sqlite => res.last_insert_id() as i64,
    };
    ExecOutcome { rows_affected: res.rows_affected(), last_insert_id }
}

/// A transaction on one pooled connection, as an [`Executor`]. From
/// [`SeaOrmExecutor::begin`].
///
/// Every statement runs inside the transaction until [`commit`](Self::commit)
/// or [`rollback`](Self::rollback); after either, statements fail rather than
/// running outside it. Dropped without either, the transaction rolls back.
///
/// A read is **not** retried on a closed connection the way the pool's reads
/// are: a transaction whose connection is gone is gone, and running the rest of
/// it on another connection would be running half of it.
pub struct SeaOrmTransaction {
    txn: Mutex<Option<Arc<DatabaseTransaction>>>,
    dialect: Dialect,
}

impl SeaOrmTransaction {
    fn current(&self) -> Result<Arc<DatabaseTransaction>> {
        self.txn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| Error::msg("this transaction has already been committed or rolled back"))
    }

    fn take(&self) -> Result<DatabaseTransaction> {
        let txn =
            self.txn.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take().ok_or_else(
                || Error::msg("this transaction has already been committed or rolled back"),
            )?;
        Arc::try_unwrap(txn).map_err(|_| {
            Error::msg(
                "a statement is still running in this transaction; await it before finishing",
            )
        })
    }

    /// Commit. The transaction's statements become visible to everyone.
    pub async fn commit(&self) -> Result<()> {
        self.take()?.commit().await.map_err(Error::from)
    }

    /// Roll back. Nothing the transaction wrote remains.
    pub async fn rollback(&self) -> Result<()> {
        self.take()?.rollback().await.map_err(Error::from)
    }

    fn statement(&self, sql: &str, params: Vec<Value>) -> Statement {
        Statement::from_sql_and_values(backend_of(self.dialect), sql, params)
    }
}

impl Executor for SeaOrmTransaction {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    async fn fetch_all(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        let txn = self.current()?;
        let rows = txn.query_all_raw(self.statement(sql, params)).await.map_err(Error::from)?;
        Ok(rows_of(rows, self.dialect))
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
        let txn = self.current()?;
        let res = txn.execute_raw(self.statement(sql, params)).await.map_err(Error::from)?;
        Ok(outcome_of(&res, self.dialect))
    }
}

impl Executor for SeaOrmExecutor {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    async fn fetch_all(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        // A read that draws a connection the server has already closed — used
        // under `test_after_idle` ago, so not pinged — runs again. After a
        // database restart every idle connection in the pool is closed, and
        // each one fails at once, so the read goes on until it reaches a live
        // one: at most the pool's size, after which the pool has opened a new
        // connection. Only a plain `SELECT`: a statement that writes cannot be
        // known not to have run.
        let rows = if is_plain_read(sql) {
            let mut attempts_left = self.dead_connection_retries;
            loop {
                match self.db.query_all_raw(self.statement(sql, params.clone())).await {
                    Err(e) if attempts_left > 0 && is_dead_connection(&e) => attempts_left -= 1,
                    result => break result.map_err(Error::from)?,
                }
            }
        } else {
            self.db.query_all_raw(self.statement(sql, params)).await.map_err(Error::from)?
        };
        Ok(rows_of(rows, self.dialect))
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
        let stmt = self.statement(sql, params);
        let res = self.db.execute_raw(stmt).await.map_err(Error::from)?;
        Ok(outcome_of(&res, self.dialect))
    }
}

/// Whether `sql` is a statement that only reads, and so can be run again.
///
/// Deliberately narrow: a leading `SELECT` and nothing else. `WITH` can carry a
/// data-modifying CTE on Postgres, and `INSERT … RETURNING` comes through
/// `fetch_all` too. A locking read (`FOR UPDATE`) is excluded as well — outside
/// a transaction it locks nothing worth retrying for, and inside one it never
/// reaches this pool executor.
fn is_plain_read(sql: &str) -> bool {
    let head = sql.trim_start();
    let is_select = head.get(..6).is_some_and(|word| word.eq_ignore_ascii_case("select"));
    is_select && !sql.to_ascii_lowercase().contains(" for update")
}

/// Whether `error` says the connection was gone before the statement ran,
/// rather than that the statement failed.
///
/// An I/O error on the socket (reset, broken pipe, the peer closing it — TLS
/// reports that as an unexpected EOF), or the driver noticing the stream had
/// ended. A query error from the server — a syntax error, a deadlock, a
/// timeout — is not one, and is never retried here.
fn is_dead_connection(error: &sea_orm::DbErr) -> bool {
    use sea_orm::{DbErr, RuntimeErr};
    use std::io::ErrorKind;

    let sqlx = match error {
        DbErr::Query(RuntimeErr::SqlxError(e))
        | DbErr::Exec(RuntimeErr::SqlxError(e))
        | DbErr::Conn(RuntimeErr::SqlxError(e)) => e,
        _ => return false,
    };
    match &**sqlx {
        sea_orm::sqlx::Error::Io(io) => matches!(
            io.kind(),
            ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::UnexpectedEof
                | ErrorKind::NotConnected
        ),
        sea_orm::sqlx::Error::Tls(_) => true,
        sea_orm::sqlx::Error::Protocol(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("closed") || message.contains("eof")
        }
        _ => false,
    }
}

/// A `sea_orm::QueryResult` behind the dialect-agnostic [`Row`] interface.
///
/// Reads go through `try_get::<Option<T>>` so a SQL NULL surfaces as `None`
/// rather than an error.
///
/// Unsigned integers are the one place dialects genuinely diverge. MySQL has
/// real `… UNSIGNED` columns that sqlx-mysql *insists* be read as `u64`/`u32`
/// (reading `BIGINT UNSIGNED` as `i64` is rejected at the type boundary).
/// SQLite and Postgres have
/// no unsigned integer types, so sea-query rendered them as signed and the
/// driver hands back `i64`/`i32`; sqlx-sqlite doesn't implement `u64` at all.
/// So the unsigned getters branch on dialect: read native on MySQL, read the
/// signed counterpart and cast everywhere else.
struct SeaOrmRow {
    row: QueryResult,
    dialect: Dialect,
}

macro_rules! getter {
    ($name:ident, $t:ty) => {
        fn $name(&self, col: &str) -> Result<Option<$t>> {
            self.row.try_get::<Option<$t>>("", col).map_err(Error::from)
        }
    };
}

impl Row for SeaOrmRow {
    getter!(get_bool, bool);
    getter!(get_i32, i32);
    getter!(get_i64, i64);
    getter!(get_string, String);
    getter!(get_bytes, Vec<u8>);
    getter!(get_datetime, DateTime<Utc>);
    getter!(get_naive_date, NaiveDate);

    fn get_u32(&self, col: &str) -> Result<Option<u32>> {
        match self.dialect {
            Dialect::MySql => self.row.try_get::<Option<u32>>("", col).map_err(Error::from),
            _ => {
                Ok(self.row.try_get::<Option<i64>>("", col).map_err(Error::from)?.map(|v| v as u32))
            }
        }
    }

    fn get_u64(&self, col: &str) -> Result<Option<u64>> {
        match self.dialect {
            Dialect::MySql => self.row.try_get::<Option<u64>>("", col).map_err(Error::from),
            _ => {
                Ok(self.row.try_get::<Option<i64>>("", col).map_err(Error::from)?.map(|v| v as u64))
            }
        }
    }

    /// Accept `DOUBLE`/`REAL` (native `f64`) and, on dialects that have a
    /// distinct `DECIMAL` (MySQL/Postgres), a `DECIMAL` column read as
    /// `rust_decimal::Decimal` and widened to `f64`.
    fn get_f64(&self, col: &str) -> Result<Option<f64>> {
        if let Ok(v) = self.row.try_get::<Option<f64>>("", col) {
            return Ok(v);
        }
        use std::str::FromStr;
        let dec =
            self.row.try_get::<Option<rust_decimal::Decimal>>("", col).map_err(Error::from)?;
        Ok(dec.map(|d| f64::from_str(&d.to_string()).unwrap_or(0.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_select_is_a_read_that_can_run_twice() {
        assert!(is_plain_read("SELECT * FROM posts"));
        assert!(is_plain_read("  select id from users where id = ?"));
        assert!(!is_plain_read("INSERT INTO posts (id) VALUES (1) RETURNING id"));
        assert!(!is_plain_read("WITH gone AS (DELETE FROM posts RETURNING id) SELECT * FROM gone"));
        assert!(!is_plain_read("SELECT * FROM posts WHERE id = 1 FOR UPDATE"));
        assert!(!is_plain_read("UPDATE posts SET title = 'selected'"));
        assert!(!is_plain_read("sel"));
    }

    #[test]
    fn a_closed_socket_is_a_dead_connection_and_a_query_error_is_not() {
        use sea_orm::{DbErr, RuntimeErr};
        use std::sync::Arc;

        let io = |kind| {
            DbErr::Query(RuntimeErr::SqlxError(Arc::new(sea_orm::sqlx::Error::Io(
                std::io::Error::new(kind, "gone"),
            ))))
        };
        assert!(is_dead_connection(&io(std::io::ErrorKind::UnexpectedEof)));
        assert!(is_dead_connection(&io(std::io::ErrorKind::ConnectionReset)));
        assert!(is_dead_connection(&io(std::io::ErrorKind::BrokenPipe)));
        assert!(!is_dead_connection(&io(std::io::ErrorKind::TimedOut)));

        let protocol = DbErr::Query(RuntimeErr::SqlxError(Arc::new(
            sea_orm::sqlx::Error::Protocol("syntax error near FROM".into()),
        )));
        assert!(!is_dead_connection(&protocol));
        assert!(!is_dead_connection(&DbErr::RecordNotFound("posts".into())));
    }

    #[tokio::test]
    async fn session_sql_on_a_backend_that_cannot_run_it_is_refused_rather_than_dropped() {
        // The hook is wired for MySQL only. Accepting the statements against
        // another backend and quietly not running them would be the ignored
        // setting the configuration layer exists to refuse, one level down —
        // and the caller would have a connection it believed was configured.
        let opened = SeaOrmExecutor::connect_with_session(
            "sqlite::memory:",
            &PoolConfig::in_memory(),
            &["SET SESSION sql_mode = ''".to_string()],
        )
        .await;

        let Err(err) = opened else { panic!("no hook for this backend") };
        assert!(err.to_string().contains("only wired for the MySQL backend"), "{err}");
    }

    #[tokio::test]
    async fn no_session_sql_leaves_the_ordinary_path_untouched() {
        // The path every existing declaration takes, and the reason the
        // hand-built pool is reached only by asking for it.
        let executor =
            SeaOrmExecutor::connect_with_session("sqlite::memory:", &PoolConfig::in_memory(), &[])
                .await
                .expect("open");

        assert_eq!(executor.dialect(), Dialect::Sqlite);
        executor.execute("CREATE TABLE widgets (id INTEGER)", vec![]).await.expect("run");
    }
}
