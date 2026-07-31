//! Dialect selection and SQL rendering.
//!
//! One [`Entity`](crate::Entity) yields one `sea_query` statement; this module
//! renders that statement for whichever backend the [`Executor`](crate::Executor)
//! reports. That indirection is the whole "translation between SQLite / D1 /
//! MySQL / Postgres" story — the query is built once, lowered per dialect here.
//!
//! D1 is SQLite-on-the-wire, so it shares [`Dialect::Sqlite`] rendering; what
//! differs for D1 is *transport*, which is the executor's concern, not the
//! SQL's.

use sea_query::{
    MysqlQueryBuilder, PostgresQueryBuilder, QueryStatementBuilder, SchemaStatementBuilder,
    SqliteQueryBuilder, Values,
};

/// The SQL dialect a backend speaks. Cloudflare D1 maps to [`Sqlite`] (it is
/// SQLite over HTTP); the executor handles the transport difference.
///
/// [`Sqlite`]: Dialect::Sqlite
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    MySql,
    Postgres,
    Sqlite,
}

impl Dialect {
    /// Render a data statement (`SELECT`/`INSERT`/`UPDATE`/`DELETE`) to
    /// parameterised SQL plus its ordered bind values.
    pub fn build_query<S: QueryStatementBuilder>(&self, stmt: &S) -> (String, Values) {
        match self {
            Dialect::MySql => stmt.build_any(&MysqlQueryBuilder),
            Dialect::Postgres => stmt.build_any(&PostgresQueryBuilder),
            Dialect::Sqlite => stmt.build_any(&SqliteQueryBuilder),
        }
    }

    /// Render a schema statement (e.g. `CREATE TABLE`) to SQL. Schema
    /// statements carry no bind values.
    pub fn build_schema<S: SchemaStatementBuilder>(&self, stmt: &S) -> String {
        match self {
            Dialect::MySql => stmt.build_any(&MysqlQueryBuilder),
            Dialect::Postgres => stmt.build_any(&PostgresQueryBuilder),
            Dialect::Sqlite => stmt.build_any(&SqliteQueryBuilder),
        }
    }
}
