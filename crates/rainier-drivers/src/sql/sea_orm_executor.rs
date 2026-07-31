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
        Ok(Self::new(db))
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
