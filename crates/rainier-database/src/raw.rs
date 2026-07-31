//! Raw SQL, bound safely — [`RawQuery`].
//!
//! ```ignore
//! let stale = database
//!     .query("DELETE FROM sessions WHERE last_seen_at < ?")
//!     .bind(cutoff)
//!     .execute()
//!     .await?;
//!
//! let names: Vec<String> = database
//!     .query("SELECT name FROM users WHERE team_id = ? ORDER BY name")
//!     .bind(team_id)
//!     .route_by(team_id)
//!     .column("name")
//!     .await?;
//! ```
//!
//! Every ORM needs this door: a
//! recursive CTE, a window function, a `LATERAL` join, an `EXPLAIN`, a
//! migration's one-off backfill. Without it the escape hatch is
//! [`Database::statement`], which takes no bindings at all — and a query that
//! needs a value with no way to bind one is a query somebody is about to build
//! with `format!`.
//!
//! # Placeholders are always `?`
//!
//! MySQL and SQLite spell a placeholder `?`; Postgres spells it `$1`, `$2`.
//! Writing SQL that runs on all three otherwise means writing it twice, which
//! is the exact madness a DBAL exists to absorb. So `?` is the spelling here
//! and it is rewritten to `$n` for Postgres, in order, skipping anything
//! inside a string literal or a quoted identifier.
//!
//! The one place that bites is Postgres's JSON `?` operator (`data ? 'key'`)
//! and its `??`/`?|`/`?&` relatives — genuinely `?` characters that are not
//! placeholders. Reach for [`raw_placeholders`](RawQuery::raw_placeholders)
//! there and write `$1` yourself.
//!
//! # This is the unsafe door, and it is unsafe in one specific way
//!
//! **Values** are always bound — that is what [`bind`](RawQuery::bind) is for,
//! and a bound value can never be SQL. The **statement** is not: it is
//! whatever string you passed. Building that string out of anything a request
//! supplied is an injection, and no amount of binding downstream repairs it.
//! A table or column name that has to vary belongs in a `match` over a closed
//! set, never in a `format!`.

use rainier_orm::sea_query::Value;
use rainier_orm::{Dialect, Entity, ExecOutcome, ShardRoute};
use rainier_support::{Error, Result};

use crate::connection::Database;
use crate::row::{ColumnRequest, OwnedRow};
use crate::statement::{routing_key, Prepared};

/// A statement being built: the SQL, its bindings, and where to run it.
///
/// Nothing happens until one of the terminal methods is awaited.
#[derive(Debug)]
pub struct RawQuery<'a> {
    database: &'a Database,
    sql: String,
    params: Vec<Value>,
    route: ShardRoute,
    rewrite_placeholders: bool,
}

impl<'a> RawQuery<'a> {
    /// Start from `sql`. Prefer [`Database::query`].
    pub fn new(database: &'a Database, sql: impl Into<String>) -> Self {
        Self {
            database,
            sql: sql.into(),
            params: Vec::new(),
            route: ShardRoute::Global,
            rewrite_placeholders: true,
        }
    }

    /// Bind the next `?`.
    ///
    /// Order matters and nothing checks the count until the driver does — an
    /// over- or under-bound statement is an error from the database, not from
    /// here, because only it knows how many placeholders it found.
    #[must_use = "this returns the query rather than modifying it in place"]
    pub fn bind(mut self, value: impl Into<Value>) -> Self {
        self.params.push(value.into());
        self
    }

    /// Bind several, in order.
    #[must_use = "this returns the query rather than modifying it in place"]
    pub fn bind_all(mut self, values: impl IntoIterator<Item = impl Into<Value>>) -> Self {
        self.params.extend(values.into_iter().map(Into::into));
        self
    }

    /// Send this to the shard that owns `key`.
    ///
    /// Only meaningful on a sharded connection; ignored by everything else.
    /// Without it a query goes to the global route, which for a sharded
    /// deployment means "the one that is not shard-specific" — so a query
    /// touching a sharded table **must** say which shard, or it will look in
    /// the wrong place and quietly find nothing.
    ///
    /// Takes the same values the ORM routes by: a shard-encoded id as-is, a
    /// string key hashed the same stable way, so the same key lands on the
    /// same shard from any process.
    #[must_use = "this returns the query rather than modifying it in place"]
    pub fn route_by(mut self, key: impl Into<Value>) -> Self {
        let key = key.into();
        self.route = match routing_key(&key) {
            Some(key) => ShardRoute::Key(key),
            // A value with no routing meaning — a float, a null. Global is the
            // honest answer rather than a hash of something arbitrary.
            None => ShardRoute::Global,
        };
        self
    }

    /// Send this to the shard holding `key`, already resolved.
    #[must_use = "this returns the query rather than modifying it in place"]
    pub fn on_shard_key(mut self, key: u64) -> Self {
        self.route = ShardRoute::Key(key);
        self
    }

    /// Leave the placeholders exactly as written.
    ///
    /// For Postgres's JSON `?` operators, or for SQL that is already written
    /// in `$n` form. The statement is then dialect-specific, which is the
    /// trade.
    #[must_use = "this returns the query rather than modifying it in place"]
    pub fn raw_placeholders(mut self) -> Self {
        self.rewrite_placeholders = false;
        self
    }

    /// The statement as it will be sent — SQL, bindings and route.
    ///
    /// For logging it, asserting on it in a test, or handing it to
    /// [`Database::fetch`] directly.
    pub fn prepared(self) -> Prepared {
        let sql = if self.rewrite_placeholders && self.database.dialect() == Dialect::Postgres {
            to_numbered_placeholders(&self.sql)
        } else {
            self.sql
        };

        Prepared { sql, params: self.params, route: self.route }
    }

    // --- running it --------------------------------------------------------

    /// Run it as a write, and report what happened.
    pub async fn execute(self) -> Result<ExecOutcome> {
        let database = self.database;
        database.execute(self.prepared()).await
    }

    /// Run it and read back the named columns.
    pub async fn fetch(self, columns: Vec<ColumnRequest>) -> Result<Vec<OwnedRow>> {
        let database = self.database;
        database.fetch(self.prepared(), columns).await
    }

    /// Run it and decode every row into `E`.
    ///
    /// The statement has to return `E`'s columns by their own names —
    /// `SELECT *`, or an explicit list with matching aliases.
    pub async fn fetch_all<E: Entity>(self) -> Result<Vec<E>> {
        let database = self.database;
        database.fetch_all::<E>(self.prepared()).await
    }

    /// Run it and decode the first row into `E`.
    pub async fn fetch_one<E: Entity>(self) -> Result<Option<E>> {
        let database = self.database;
        database.fetch_one::<E>(self.prepared()).await
    }

    /// Read one integer out of the first row — a `COUNT`, a `SUM`, a `MAX`.
    ///
    /// `None` when there was no row, or the value was `NULL`. Those are
    /// different from `0`, which is why this does not flatten them: `SUM` over
    /// no rows is `NULL`, not zero, and rounding that to zero is how a total
    /// silently becomes wrong.
    pub async fn scalar_i64(self, column: &str) -> Result<Option<i64>> {
        let requested = vec![ColumnRequest::new(column, rainier_orm::ColumnType::BigInt)];
        let rows = self.fetch(requested).await?;

        match rows.first() {
            Some(row) => Ok(rainier_orm::Row::get_i64(row, column).map_err(Error::from)?),
            None => Ok(None),
        }
    }

    /// Read one text column out of the first row.
    pub async fn scalar_string(self, column: &str) -> Result<Option<String>> {
        let requested = vec![ColumnRequest::new(column, rainier_orm::ColumnType::Text)];
        let rows = self.fetch(requested).await?;

        match rows.first() {
            Some(row) => Ok(rainier_orm::Row::get_string(row, column).map_err(Error::from)?),
            None => Ok(None),
        }
    }

    /// Read one text column out of every row.
    ///
    /// A `NULL` is skipped rather than becoming an empty string — the two mean
    /// different things and only one of them is a name.
    pub async fn column(self, column: &str) -> Result<Vec<String>> {
        let requested = vec![ColumnRequest::new(column, rainier_orm::ColumnType::Text)];
        let rows = self.fetch(requested).await?;

        rows.iter()
            .map(|row| rainier_orm::Row::get_string(row, column).map_err(Error::from))
            .filter_map(Result::transpose)
            .collect()
    }
}

/// Rewrite `?` placeholders as `$1`, `$2`, … for Postgres.
///
/// A `?` inside `'a string'`, `"a quoted identifier"` or a `$$dollar quote$$`
/// is left alone — it is data or a name, not a placeholder. Doubled quotes
/// inside a literal (`'it''s'`) are handled by the state machine falling out
/// and straight back in, which lands in the right place.
fn to_numbered_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut next = 1;
    let mut quote: Option<char> = None;

    for c in sql.chars() {
        match quote {
            Some(open) => {
                out.push(c);
                if c == open {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    out.push(c);
                }
                '?' => {
                    out.push('$');
                    out.push_str(&next.to_string());
                    next += 1;
                }
                _ => out.push(c),
            },
        }
    }

    out
}

impl Database {
    /// Start a raw statement.
    ///
    /// ```ignore
    /// database.query("UPDATE posts SET views = views + 1 WHERE id = ?")
    ///     .bind(post_id)
    ///     .execute()
    ///     .await?;
    /// ```
    ///
    /// See the [module docs](crate::raw) for what is and is not safe to put in
    /// the statement itself.
    pub fn query(&self, sql: impl Into<String>) -> RawQuery<'_> {
        RawQuery::new(self, sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MemoryConnection;

    fn database(dialect: Dialect) -> Database {
        Database::new(MemoryConnection::new(dialect))
    }

    #[test]
    fn sqlite_and_mysql_keep_question_marks() {
        for dialect in [Dialect::Sqlite, Dialect::MySql] {
            let prepared = database(dialect)
                .query("SELECT * FROM users WHERE team_id = ? AND name = ?")
                .bind(7)
                .bind("Ada")
                .prepared();

            assert_eq!(prepared.sql, "SELECT * FROM users WHERE team_id = ? AND name = ?");
            assert_eq!(prepared.params.len(), 2);
        }
    }

    #[test]
    fn postgres_gets_numbered_placeholders() {
        let prepared = database(Dialect::Postgres)
            .query("SELECT * FROM users WHERE team_id = ? AND name = ?")
            .bind(7)
            .bind("Ada")
            .prepared();

        assert_eq!(prepared.sql, "SELECT * FROM users WHERE team_id = $1 AND name = $2");
    }

    #[test]
    fn a_question_mark_inside_a_literal_is_not_a_placeholder() {
        // Otherwise `'why?'` becomes `'why$1'` and the query returns nothing,
        // which is the kind of bug that survives review.
        let prepared = database(Dialect::Postgres)
            .query("SELECT * FROM posts WHERE title = 'why?' AND author_id = ?")
            .bind(1)
            .prepared();

        assert_eq!(prepared.sql, "SELECT * FROM posts WHERE title = 'why?' AND author_id = $1");
    }

    #[test]
    fn a_question_mark_inside_a_quoted_identifier_is_left_alone() {
        let prepared = database(Dialect::Postgres)
            .query(r#"SELECT "odd?column" FROM t WHERE id = ?"#)
            .prepared();

        assert_eq!(prepared.sql, r#"SELECT "odd?column" FROM t WHERE id = $1"#);
    }

    #[test]
    fn raw_placeholders_leaves_postgres_alone() {
        // For `data ? 'key'`, where the `?` really is an operator.
        let sql = "SELECT * FROM events WHERE payload ? 'user_id' AND id = $1";
        let prepared = database(Dialect::Postgres).query(sql).bind(1).raw_placeholders().prepared();

        assert_eq!(prepared.sql, sql);
    }

    #[test]
    fn a_query_is_global_until_it_is_routed() {
        let prepared = database(Dialect::Sqlite).query("SELECT 1").prepared();
        assert_eq!(prepared.route, ShardRoute::Global);

        let routed = database(Dialect::Sqlite).query("SELECT 1").route_by(4096u64).prepared();
        assert_eq!(routed.route, ShardRoute::Key(4096));
    }

    #[test]
    fn a_string_key_routes_the_same_way_the_orm_routes_it() {
        let routed = database(Dialect::Sqlite).query("SELECT 1").route_by("tenant-a").prepared();

        assert_eq!(routed.route, ShardRoute::Key(rainier_orm::stable_hash(b"tenant-a")));
    }

    #[test]
    fn a_key_with_no_routing_meaning_stays_global() {
        // Better than hashing something arbitrary and landing on a shard that
        // has nothing to do with the row.
        let routed = database(Dialect::Sqlite).query("SELECT 1").route_by(1.5f64).prepared();

        assert_eq!(routed.route, ShardRoute::Global);
    }

    #[test]
    fn bind_all_keeps_its_order() {
        let prepared = database(Dialect::Sqlite)
            .query("SELECT * FROM users WHERE id IN (?, ?, ?)")
            .bind_all([1, 2, 3])
            .prepared();

        assert_eq!(prepared.params.len(), 3);
        assert_eq!(prepared.params[0], Value::Int(Some(1)));
        assert_eq!(prepared.params[2], Value::Int(Some(3)));
    }
}
