//! A fluent query builder over an [`Entity`] — `where` / `where_not` / `like` /
//! `in` / null checks, ordering, paging, single-backend joins, and the
//! terminals `all` / `first` / `count` / `delete` / `first_or_create`.
//!
//! It accumulates conditions and renders through the same dialect machinery as
//! [`crate::repo`], so it runs on every backend and decodes straight into `E`.
//! For anything past its surface, drop to the re-exported [`sea_query`] — the
//! builder holds a [`sea_query::Cond`] you can extend via [`Query::filter`].
//!
//! ```ignore
//! use rainier_orm::repo;
//!
//! // active authors named like "al%", newest first, first page
//! let authors: Vec<Author> = repo::query::<Author>()
//!     .where_eq("active", true)
//!     .where_like("name", "al%")
//!     .order_by_desc("created_at")
//!     .limit(20)
//!     .all(&db)
//!     .await?;
//!
//! // posts whose author is active (join filters; we still select/decode posts)
//! let posts: Vec<Post> = repo::query::<Post>()
//!     .join("authors", "author_id", "id")
//!     .where_eq("authors.active", true)
//!     .all(&db)
//!     .await?;
//!
//! // get-or-insert
//! let tag = repo::query::<Tag>()
//!     .where_eq("slug", "rust")
//!     .first_or_create(&db, Tag { id: 0, slug: "rust".into() })
//!     .await?;
//! ```
//!
//! **Joins are single-backend.** A join targets another table *in the same
//! executor*, for filtering or selecting the root entity — it never spans
//! backends (that's impossible) and it does not decode the joined table. To
//! assemble data across entities (or across backends), query each and compose
//! in your code; see [`crate::repo::find_by`].

use crate::route::route_for;
use crate::{repo, Entity, Executor, Result, ShardRoute};
use core::future::Future;
use core::marker::PhantomData;
use sea_query::{
    Alias, Asterisk, ColumnRef, Cond, Expr, Func, IntoColumnRef, JoinType, Order, Query as SqQuery,
    SimpleExpr, Value,
};

/// A builder for reads (and `WHERE`-scoped deletes) over `E`. Conditions are
/// AND-combined; use [`filter`](Self::filter) to add an OR group.
pub struct Query<E> {
    cond: Cond,
    joins: Vec<(JoinType, String, SimpleExpr)>,
    orders: Vec<(ColumnRef, Order)>,
    limit: Option<u64>,
    offset: Option<u64>,
    route: ShardRoute,
    _entity: PhantomData<E>,
}

impl<E: Entity> Default for Query<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Entity> Query<E> {
    /// An unfiltered query over `E`'s table. Prefer [`repo::query`] for brevity.
    pub fn new() -> Self {
        Self {
            cond: Cond::all(),
            joins: Vec::new(),
            orders: Vec::new(),
            limit: None,
            offset: None,
            route: ShardRoute::Global,
            _entity: PhantomData,
        }
    }

    fn add(mut self, expr: SimpleExpr) -> Self {
        let cond = core::mem::replace(&mut self.cond, Cond::all());
        self.cond = cond.add(expr);
        self
    }

    // --- predicates -------------------------------------------------------
    // A column is `"name"` (qualified to `E`'s table) or `"table.name"` (for a
    // joined table).

    /// `column = value`. An equality on a shard-encoded column also fixes the
    /// query's shard route (transparently).
    pub fn where_eq(mut self, column: &str, value: impl Into<Value>) -> Self {
        let val: Value = value.into();
        if matches!(self.route, ShardRoute::Global) {
            self.route = route_for::<E>(column, &val);
        }
        self.add(Expr::col(col_ref::<E>(column)).eq(val))
    }

    /// `column <> value`.
    pub fn where_ne(self, column: &str, value: impl Into<Value>) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).ne(value.into()))
    }

    /// `column > value`.
    pub fn where_gt(self, column: &str, value: impl Into<Value>) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).gt(value.into()))
    }

    /// `column >= value`.
    pub fn where_gte(self, column: &str, value: impl Into<Value>) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).gte(value.into()))
    }

    /// `column < value`.
    pub fn where_lt(self, column: &str, value: impl Into<Value>) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).lt(value.into()))
    }

    /// `column <= value`.
    pub fn where_lte(self, column: &str, value: impl Into<Value>) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).lte(value.into()))
    }

    /// `column LIKE pattern` (use `%`/`_` wildcards).
    pub fn where_like(self, column: &str, pattern: &str) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).like(pattern))
    }

    /// `column NOT LIKE pattern`.
    pub fn where_not_like(self, column: &str, pattern: &str) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).not_like(pattern))
    }

    /// `column IN (values)`. An empty set matches nothing.
    pub fn where_in<V: Into<Value>>(
        self,
        column: &str,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let vals: Vec<Value> = values.into_iter().map(Into::into).collect();
        self.add(Expr::col(col_ref::<E>(column)).is_in(vals))
    }

    /// `column NOT IN (values)`.
    pub fn where_not_in<V: Into<Value>>(
        self,
        column: &str,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let vals: Vec<Value> = values.into_iter().map(Into::into).collect();
        self.add(Expr::col(col_ref::<E>(column)).is_not_in(vals))
    }

    /// `column IS NULL`.
    pub fn where_null(self, column: &str) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).is_null())
    }

    /// `column IS NOT NULL`.
    pub fn where_not_null(self, column: &str) -> Self {
        self.add(Expr::col(col_ref::<E>(column)).is_not_null())
    }

    /// Add a raw [`sea_query::Cond`] (AND-combined with the rest). The escape
    /// hatch for OR groups and any predicate the helpers don't cover:
    /// `.filter(Cond::any().add(a).add(b))`.
    pub fn filter(mut self, cond: Cond) -> Self {
        let existing = core::mem::replace(&mut self.cond, Cond::all());
        self.cond = existing.add(cond);
        self
    }

    // --- joins (single backend, root-entity selection only) ---------------

    /// `INNER JOIN foreign_table ON E.local_col = foreign_table.foreign_col`.
    /// For filtering/selecting `E` only — the joined table is not decoded.
    pub fn join(self, foreign_table: &str, local_col: &str, foreign_col: &str) -> Self {
        self.join_with(JoinType::InnerJoin, foreign_table, local_col, foreign_col)
    }

    /// As [`join`](Self::join) but a `LEFT JOIN`.
    pub fn left_join(self, foreign_table: &str, local_col: &str, foreign_col: &str) -> Self {
        self.join_with(JoinType::LeftJoin, foreign_table, local_col, foreign_col)
    }

    fn join_with(
        mut self,
        kind: JoinType,
        foreign_table: &str,
        local_col: &str,
        foreign_col: &str,
    ) -> Self {
        let on = Expr::col((Alias::new(E::table()), Alias::new(local_col)))
            .equals((Alias::new(foreign_table.to_owned()), Alias::new(foreign_col.to_owned())));
        self.joins.push((kind, foreign_table.to_owned(), on));
        self
    }

    // --- ordering / paging ------------------------------------------------

    /// `ORDER BY column ASC`.
    pub fn order_by_asc(mut self, column: &str) -> Self {
        self.orders.push((col_ref::<E>(column), Order::Asc));
        self
    }

    /// `ORDER BY column DESC`.
    pub fn order_by_desc(mut self, column: &str) -> Self {
        self.orders.push((col_ref::<E>(column), Order::Desc));
        self
    }

    /// `LIMIT n`.
    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }

    /// `OFFSET n`.
    pub fn offset(mut self, n: u64) -> Self {
        self.offset = Some(n);
        self
    }

    // --- rendering --------------------------------------------------------
    //
    // Each terminal renders through one of these, which **consume** the
    // builder. That is what keeps the terminals' futures `Send`: a `Query<E>`
    // holds `Cond` and `ColumnRef`, both of which contain `Rc`, so a builder
    // still alive across the executor's `.await` would be captured by the
    // generated future and make it `!Send` — even though nothing reads it
    // again. Consuming `self` here drops all of it before the await.
    //
    // See `repo`'s module docs for the full argument, and
    // `tests/send_futures.rs` for the compile-time guard.

    /// Render the `SELECT` this query describes.
    fn render_select(self, dialect: crate::Dialect) -> (ShardRoute, String, Vec<Value>) {
        let mut stmt = SqQuery::select();
        stmt.from(Alias::new(E::table()));
        for c in E::columns() {
            stmt.column((Alias::new(E::table()), Alias::new(c.name)));
        }
        for (kind, table, on) in &self.joins {
            stmt.join(*kind, Alias::new(table.clone()), on.clone());
        }
        stmt.cond_where(self.cond.clone());
        for (col, ord) in &self.orders {
            stmt.order_by(col.clone(), ord.clone());
        }
        if let Some(l) = self.limit {
            stmt.limit(l);
        }
        if let Some(o) = self.offset {
            stmt.offset(o);
        }

        let (sql, params) = dialect.build_query(&stmt);
        (self.route, sql, params.0)
    }

    /// Render the `SELECT COUNT(*)` this query describes.
    fn render_count(self, dialect: crate::Dialect) -> (ShardRoute, String, Vec<Value>) {
        let mut stmt = SqQuery::select();
        stmt.from(Alias::new(E::table()));
        for (kind, table, on) in &self.joins {
            stmt.join(*kind, Alias::new(table.clone()), on.clone());
        }
        stmt.cond_where(self.cond.clone());
        stmt.expr_as(Func::count(Expr::col(Asterisk)), Alias::new("cnt"));

        let (sql, params) = dialect.build_query(&stmt);
        (self.route, sql, params.0)
    }

    /// Render an `UPDATE` setting `set` under this query's filters.
    fn render_update(
        self,
        dialect: crate::Dialect,
        set: Vec<(&str, Value)>,
    ) -> (ShardRoute, String, Vec<Value>) {
        let mut stmt = SqQuery::update();
        stmt.table(Alias::new(E::table()));
        for (c, v) in set {
            stmt.value(Alias::new(c.to_owned()), v);
        }
        stmt.cond_where(self.cond.clone());

        let (sql, params) = dialect.build_query(&stmt);
        (self.route, sql, params.0)
    }

    /// Render an atomic `SET column = column + by` under this query's filters.
    fn render_increment(
        self,
        dialect: crate::Dialect,
        column: &str,
        by: i64,
    ) -> (ShardRoute, String, Vec<Value>) {
        let mut stmt = SqQuery::update();
        stmt.table(Alias::new(E::table()));
        stmt.value(Alias::new(column.to_owned()), Expr::col(Alias::new(column.to_owned())).add(by));
        stmt.cond_where(self.cond.clone());

        let (sql, params) = dialect.build_query(&stmt);
        (self.route, sql, params.0)
    }

    /// Render a `DELETE` under this query's filters.
    fn render_delete(self, dialect: crate::Dialect) -> (ShardRoute, String, Vec<Value>) {
        let mut stmt = SqQuery::delete();
        stmt.from_table(Alias::new(E::table()));
        stmt.cond_where(self.cond.clone());

        let (sql, params) = dialect.build_query(&stmt);
        (self.route, sql, params.0)
    }

    // --- terminals --------------------------------------------------------
    //
    // These are `fn … -> impl Future`, not `async fn`, and that is load-bearing
    // rather than stylistic.
    //
    // The future returned by an `async fn` captures **every argument**, whether
    // or not the body moves it out before the first `.await`. `self` is a
    // `Query<E>`, which holds `Cond` and `ColumnRef` — both containing `Rc` —
    // so an `async fn` terminal is `!Send` no matter how carefully its body is
    // written, and could not be awaited in a handler a multi-threaded server
    // will spawn.
    //
    // Consuming `self` *outside* the `async move` block leaves it out of the
    // future entirely, so only the rendered `String` and `Vec<Value>` are
    // captured. `tests/send_futures.rs` guards this at compile time.

    /// Run the query and decode every matching row into `E`.
    pub fn all<'a, X: Executor>(self, exec: &'a X) -> impl Future<Output = Result<Vec<E>>> + 'a
    where
        E: 'a,
    {
        let (route, sql, params) = self.render_select(exec.dialect());
        async move {
            let rows = exec.fetch_all_routed(route, &sql, params).await?;
            rows.iter().map(|r| E::from_row(r.as_ref())).collect()
        }
    }

    /// Run with `LIMIT 1` and decode the first match, if any.
    pub fn first<'a, X: Executor>(self, exec: &'a X) -> impl Future<Output = Result<Option<E>>> + 'a
    where
        E: 'a,
    {
        let rows = self.limit(1).all(exec);
        async move { Ok(rows.await?.into_iter().next()) }
    }

    /// `SELECT COUNT(*)` under the same filters and joins.
    pub fn count<'a, X: Executor>(self, exec: &'a X) -> impl Future<Output = Result<u64>> + 'a
    where
        E: 'a,
    {
        let (route, sql, params) = self.render_count(exec.dialect());
        async move {
            let rows = exec.fetch_all_routed(route, &sql, params).await?;
            let n = match rows.first() {
                Some(r) => r.get_i64("cnt")?.unwrap_or(0),
                None => 0,
            };
            Ok(n.max(0) as u64)
        }
    }

    /// `UPDATE` the given `column = value` pairs on every row matching the
    /// filters — a *partial* update (only the listed columns), unlike
    /// [`repo::update`](crate::repo::update()) which writes the whole row.
    /// Joins are ignored. Returns rows affected.
    ///
    /// ```ignore
    /// repo::query::<Token>()
    ///     .where_eq("id", id)
    ///     .update(&db, vec![("last_used_at", now.into())]).await?;
    /// ```
    pub fn update<'a, X: Executor>(
        self,
        exec: &'a X,
        set: Vec<(&str, Value)>,
    ) -> impl Future<Output = Result<u64>> + 'a {
        let (route, sql, params) = self.render_update(exec.dialect(), set);
        async move { Ok(exec.execute_routed(route, &sql, params).await?.rows_affected) }
    }

    /// Atomically add `by` to `column` (`SET column = column + by`) on every
    /// matching row — a column-relative update no value-binding can express
    /// (e.g. `attempts = attempts + 1`). Returns rows affected.
    pub fn increment<'a, X: Executor>(
        self,
        exec: &'a X,
        column: &str,
        by: i64,
    ) -> impl Future<Output = Result<u64>> + 'a {
        let (route, sql, params) = self.render_increment(exec.dialect(), column, by);
        async move { Ok(exec.execute_routed(route, &sql, params).await?.rows_affected) }
    }

    /// `DELETE` every row matching the filters. Joins are ignored (portable
    /// `DELETE` takes no joins); express the condition with `WHERE`/subqueries
    /// instead. Returns rows affected.
    pub fn delete<'a, X: Executor>(self, exec: &'a X) -> impl Future<Output = Result<u64>> + 'a {
        let (route, sql, params) = self.render_delete(exec.dialect());
        async move { Ok(exec.execute_routed(route, &sql, params).await?.rows_affected) }
    }

    /// Return the first match, or insert `default` and return it. Not atomic —
    /// two racing callers can both insert; add a `UNIQUE` constraint on the
    /// lookup column(s) if you need the database to arbitrate.
    pub fn first_or_create<'a, X: Executor>(
        self,
        exec: &'a X,
        default: E,
    ) -> impl Future<Output = Result<E>> + 'a
    where
        E: 'a,
    {
        // `self` is consumed here, outside the block, so the builder never
        // enters the future. See the note above the terminals.
        let existing = self.first(exec);

        async move {
            if let Some(found) = existing.await? {
                return Ok(found);
            }
            let id = repo::insert(exec, &default).await?;
            if id > 0 {
                if let Some(created) = repo::find_by_pk(exec, id).await? {
                    return Ok(created);
                }
            }
            Ok(default)
        }
    }
}

/// `"name"` → `E.table.name`; `"table.name"` → `table.name`.
fn col_ref<E: Entity>(spec: &str) -> ColumnRef {
    if let Some((t, c)) = spec.split_once('.') {
        (Alias::new(t.to_owned()), Alias::new(c.to_owned())).into_column_ref()
    } else {
        (Alias::new(E::table()), Alias::new(spec.to_owned())).into_column_ref()
    }
}
