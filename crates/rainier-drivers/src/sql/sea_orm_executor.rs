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
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult,
    Statement, Value as SeaValue,
};

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
}

impl SeaOrmExecutor {
    /// Adopt an existing connection, deriving the dialect from the backend it
    /// was opened with.
    pub fn new(db: DatabaseConnection) -> Self {
        let dialect = match db.get_database_backend() {
            DbBackend::MySql => Dialect::MySql,
            DbBackend::Postgres => Dialect::Postgres,
            DbBackend::Sqlite => Dialect::Sqlite,
        };
        Self { db, dialect }
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
                .test_before_acquire(pool.test_before_acquire);
            if let Some(idle) = pool.idle_timeout {
                opts.idle_timeout(idle);
            }
            if let Some(life) = pool.max_lifetime {
                opts.max_lifetime(life);
            }
            let db = Database::connect(opts).await.map_err(Error::from)?;
            return Ok(Self::new(db));
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
            // Never the URL: a DSN carries its password inline, and this is a
            // log line.
            Error::msg(format!("this MySQL connection string cannot be parsed: {e}"))
        })?;

        let mut pool_options = sea_orm::sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(pool.max_connections)
            .min_connections(pool.min_connections)
            .acquire_timeout(pool.acquire_timeout)
            .test_before_acquire(pool.test_before_acquire)
            .idle_timeout(pool.idle_timeout)
            .max_lifetime(pool.max_lifetime);

        pool_options = pool_options.after_connect(move |conn, _meta| {
            let statements = statements.clone();
            Box::pin(async move {
                for statement in &statements {
                    sea_orm::sqlx::Executor::execute(&mut *conn, statement.as_str()).await?;
                }
                Ok(())
            })
        });

        let sqlx_pool = pool_options.connect_with(options).await.map_err(Error::from)?;
        Ok(Self::new(sea_orm::SqlxMySqlConnector::from_sqlx_mysql_pool(sqlx_pool)))
    }

    /// Borrow the underlying connection (for migrations, transactions, or
    /// escape-hatch queries the generic layer doesn't cover).
    pub fn connection(&self) -> &DatabaseConnection {
        &self.db
    }

    fn backend(&self) -> DbBackend {
        match self.dialect {
            Dialect::MySql => DbBackend::MySql,
            Dialect::Postgres => DbBackend::Postgres,
            Dialect::Sqlite => DbBackend::Sqlite,
        }
    }

    fn statement(&self, sql: &str, params: Vec<Value>) -> Statement {
        let values: Vec<SeaValue> = params.into_iter().collect();
        Statement::from_sql_and_values(self.backend(), sql, values)
    }
}

impl Executor for SeaOrmExecutor {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    async fn fetch_all(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        let stmt = self.statement(sql, params);
        let rows = self.db.query_all(stmt).await.map_err(Error::from)?;
        let dialect = self.dialect;
        Ok(rows
            .into_iter()
            .map(|r| Box::new(SeaOrmRow { row: r, dialect }) as Box<dyn Row>)
            .collect())
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
        let stmt = self.statement(sql, params);
        let res = self.db.execute(stmt).await.map_err(Error::from)?;
        Ok(ExecOutcome {
            rows_affected: res.rows_affected(),
            last_insert_id: res.last_insert_id() as i64,
        })
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
