//! Composable query scopes — [`Criteria`].
//!
//! A repository method cannot take "the filters" as an argument unless filters
//! are a *value*. `Criteria` is that value: it records predicates, joins,
//! ordering and paging, and [`statement`](crate::statement) replays them into
//! SQL at execution time.
//!
//! Making the filters data rather than a partially-applied builder buys three
//! things: a scope can be named and reused, two scopes can be merged by code
//! that owns neither, and [`Repository`](crate::Repository) stays `dyn`-safe
//! because its methods take one concrete parameter instead of a closure.
//!
//! ```
//! # use rainier_database::Criteria;
//! let published = Criteria::new().where_eq("published", true);
//! let newest = Criteria::new().order_by_desc("created_at").limit(10);
//!
//! let front_page = published.merge(newest);
//! assert_eq!(front_page.limit_value(), Some(10));
//! ```

use rainier_orm::sea_query::{ColumnRef, Expr, SimpleExpr, Value};

/// One recorded predicate.
#[derive(Debug, Clone)]
pub enum Constraint {
    /// `column = value`.
    Eq(String, Value),
    /// `column <> value`.
    Ne(String, Value),
    /// `column > value`.
    Gt(String, Value),
    /// `column >= value`.
    Gte(String, Value),
    /// `column < value`.
    Lt(String, Value),
    /// `column <= value`.
    Lte(String, Value),
    /// `column LIKE pattern`.
    Like(String, String),
    /// `column NOT LIKE pattern`.
    NotLike(String, String),
    /// `column IN (values)`.
    In(String, Vec<Value>),
    /// `column NOT IN (values)`.
    NotIn(String, Vec<Value>),
    /// `column IS NULL`.
    Null(String),
    /// `column IS NOT NULL`.
    NotNull(String),
}

impl Constraint {
    /// The column this constraint applies to.
    pub fn column(&self) -> &str {
        match self {
            Constraint::Eq(column, _)
            | Constraint::Ne(column, _)
            | Constraint::Gt(column, _)
            | Constraint::Gte(column, _)
            | Constraint::Lt(column, _)
            | Constraint::Lte(column, _)
            | Constraint::Like(column, _)
            | Constraint::NotLike(column, _)
            | Constraint::In(column, _)
            | Constraint::NotIn(column, _)
            | Constraint::Null(column)
            | Constraint::NotNull(column) => column,
        }
    }

    /// The `(column, value)` pair if this is an equality, else `None`.
    ///
    /// Shard routing keys off equalities specifically: `user_id = 42` names one
    /// shard, whereas `user_id > 42` could span all of them.
    pub fn as_equality(&self) -> Option<(&str, &Value)> {
        match self {
            Constraint::Eq(column, value) => Some((column, value)),
            _ => None,
        }
    }

    /// Render as a `sea_query` expression against an already-resolved column.
    pub fn to_expr(&self, column: ColumnRef) -> SimpleExpr {
        let expr = Expr::col(column);
        match self {
            Constraint::Eq(_, value) => expr.eq(value.clone()),
            Constraint::Ne(_, value) => expr.ne(value.clone()),
            Constraint::Gt(_, value) => expr.gt(value.clone()),
            Constraint::Gte(_, value) => expr.gte(value.clone()),
            Constraint::Lt(_, value) => expr.lt(value.clone()),
            Constraint::Lte(_, value) => expr.lte(value.clone()),
            Constraint::Like(_, pattern) => expr.like(pattern.as_str()),
            Constraint::NotLike(_, pattern) => expr.not_like(pattern.as_str()),
            Constraint::In(_, values) => expr.is_in(values.clone()),
            Constraint::NotIn(_, values) => expr.is_not_in(values.clone()),
            Constraint::Null(_) => expr.is_null(),
            Constraint::NotNull(_) => expr.is_not_null(),
        }
    }
}

/// A part of a date, extracted in whatever way the dialect spells it.
///
/// The spelling is the whole reason this exists as a value rather than a
/// string: MySQL writes `MONTH(x)`, SQLite `CAST(strftime('%m', x) AS INTEGER)`
/// and Postgres `EXTRACT(MONTH FROM x)`. An application that writes one of
/// those by hand works on one dialect and fails on the others — including on
/// the SQLite its own test suite runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePart {
    /// The calendar year.
    Year,
    /// The month, 1–12.
    Month,
    /// The day of the month.
    Day,
}

/// Something a query can select or group by that is not simply a column.
///
/// Enough to express the aggregate reporting queries that would otherwise be
/// written as raw SQL — which is the point: raw SQL in a handler is a query
/// nothing can check, and it silently picks a dialect.
#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// A plain column.
    Column(String),
    /// Part of a date column, extracted per dialect.
    DatePart(DatePart, String),
    /// A timestamp truncated to its calendar date, per dialect.
    ///
    /// Distinct from [`Projection::DatePart`] with [`DatePart::Day`], and the
    /// difference matters: a day-of-month is 1–31, so grouping by it collapses
    /// the same day of different months into one bucket. A daily revenue chart
    /// grouped that way silently adds January to February.
    DateOf(String),
    /// `COUNT(*)`.
    CountAll,
    /// `COUNT(column)` — nulls excluded, as SQL defines it.
    Count(String),
    /// `SUM(column)`.
    Sum(String),
    /// `MIN(column)` / `MAX(column)` / `AVG(column)`.
    Min(String),
    /// See [`Projection::Min`].
    Max(String),
    /// See [`Projection::Min`].
    Avg(String),
    /// `SUM(CASE WHEN column IN (values) THEN 1 ELSE 0 END)` — counting the
    /// rows in a group that match, which is the shape every "how many of these
    /// were resolved" report needs.
    CountWhenIn(String, Vec<Value>),
}

impl Projection {
    /// The column this reads, for qualification against the model's table.
    pub fn column(&self) -> Option<&str> {
        match self {
            Projection::CountAll => None,
            Projection::Column(c)
            | Projection::DatePart(_, c)
            | Projection::Count(c)
            | Projection::Sum(c)
            | Projection::Min(c)
            | Projection::Max(c)
            | Projection::Avg(c)
            | Projection::DateOf(c)
            | Projection::CountWhenIn(c, _) => Some(c),
        }
    }
}

/// How two tables are joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// Rows must match on both sides.
    Inner,
    /// Keep the left row when the right side has none — which is what makes a
    /// count of "reports, and how many have a closed case" answerable in one
    /// query rather than two.
    Left,
}

/// A recorded, replayable set of query constraints.
///
/// Predicates are AND-combined. A column is `"name"` (qualified to the model's
/// table) or `"table.name"` (a joined table).
#[derive(Debug, Clone, Default)]
pub struct Criteria {
    constraints: Vec<Constraint>,
    /// `(table, local_column, foreign_column)`.
    joins: Vec<(String, String, String)>,
    /// `(table, local, foreign, kind)` — the join list that can express an
    /// outer join. Kept beside `joins` rather than replacing it so existing
    /// callers and `joins()` keep working unchanged.
    typed_joins: Vec<(String, String, String, JoinKind)>,
    /// `(projection, alias)` — an empty list means "every column of the model".
    projections: Vec<(Projection, String)>,
    /// What to group by.
    groups: Vec<Projection>,
    /// Predicate groups combined with `OR` internally, `AND`-ed with the rest.
    or_groups: Vec<Vec<Constraint>>,
    /// `(alias, descending)` — ordering by a selected projection's alias.
    alias_orders: Vec<(String, bool)>,
    /// `(column, descending)`.
    orders: Vec<(String, bool)>,
    limit: Option<u64>,
    offset: Option<u64>,
}

impl Criteria {
    /// No constraints — every row.
    pub fn new() -> Self {
        Self::default()
    }

    /// `column = value`.
    pub fn where_eq(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.constraints.push(Constraint::Eq(column.into(), value.into()));
        self
    }

    /// `column <> value`.
    pub fn where_ne(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.constraints.push(Constraint::Ne(column.into(), value.into()));
        self
    }

    /// `column > value`.
    pub fn where_gt(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.constraints.push(Constraint::Gt(column.into(), value.into()));
        self
    }

    /// `column >= value`.
    pub fn where_gte(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.constraints.push(Constraint::Gte(column.into(), value.into()));
        self
    }

    /// `column < value`.
    pub fn where_lt(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.constraints.push(Constraint::Lt(column.into(), value.into()));
        self
    }

    /// `column <= value`.
    pub fn where_lte(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.constraints.push(Constraint::Lte(column.into(), value.into()));
        self
    }

    /// `column LIKE pattern` — use `%` and `_` wildcards.
    pub fn where_like(mut self, column: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.constraints.push(Constraint::Like(column.into(), pattern.into()));
        self
    }

    /// `column NOT LIKE pattern`.
    pub fn where_not_like(mut self, column: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.constraints.push(Constraint::NotLike(column.into(), pattern.into()));
        self
    }

    /// `column IN (values)`. An empty set matches nothing.
    pub fn where_in<V: Into<Value>>(
        mut self,
        column: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let values = values.into_iter().map(Into::into).collect();
        self.constraints.push(Constraint::In(column.into(), values));
        self
    }

    /// `column NOT IN (values)`.
    pub fn where_not_in<V: Into<Value>>(
        mut self,
        column: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let values = values.into_iter().map(Into::into).collect();
        self.constraints.push(Constraint::NotIn(column.into(), values));
        self
    }

    /// `column IS NULL`.
    pub fn where_null(mut self, column: impl Into<String>) -> Self {
        self.constraints.push(Constraint::Null(column.into()));
        self
    }

    /// `column IS NOT NULL`.
    pub fn where_not_null(mut self, column: impl Into<String>) -> Self {
        self.constraints.push(Constraint::NotNull(column.into()));
        self
    }

    /// `INNER JOIN table ON model.local = table.foreign`.
    ///
    /// For filtering the root model only — the joined table is not decoded, and
    /// the join cannot span backends. Compose across entities in your own code
    /// instead; see Rainier ORM's notes on why relationships stay explicit.
    pub fn join(
        mut self,
        table: impl Into<String>,
        local: impl Into<String>,
        foreign: impl Into<String>,
    ) -> Self {
        self.joins.push((table.into(), local.into(), foreign.into()));
        self
    }

    /// `LEFT JOIN table ON model.local = table.foreign`.
    pub fn left_join(
        mut self,
        table: impl Into<String>,
        local: impl Into<String>,
        foreign: impl Into<String>,
    ) -> Self {
        self.typed_joins.push((table.into(), local.into(), foreign.into(), JoinKind::Left));
        self
    }

    /// Select a projection under an alias.
    ///
    /// Selecting anything at all switches the query from "the model's columns"
    /// to exactly what was asked for, so a result is read by alias rather than
    /// decoded into the entity.
    pub fn select(mut self, projection: Projection, alias: impl Into<String>) -> Self {
        self.projections.push((projection, alias.into()));
        self
    }

    /// Add a group of predicates combined with `OR`, `AND`-ed with the rest.
    ///
    /// `Criteria`'s own predicates are `AND`-combined, which covers most
    /// filtering and cannot express "matches either of these" — a search over
    /// two columns, most obviously. Rather than make every predicate carry a
    /// combinator, an `OR` is a nested group: the shape it actually has in SQL,
    /// and impossible to write ambiguously.
    ///
    /// ```ignore
    /// Criteria::new()
    ///     .where_eq("state", "active")
    ///     .or_where(|any| {
    ///         any.where_like("username", "%ada%").where_like("display_name", "%ada%")
    ///     })
    /// ```
    ///
    /// renders `state = ? AND (username LIKE ? OR display_name LIKE ?)`.
    pub fn or_where(mut self, group: impl FnOnce(Criteria) -> Criteria) -> Self {
        let built = group(Criteria::new());
        if !built.constraints.is_empty() {
            self.or_groups.push(built.constraints);
        }
        self
    }

    /// The `OR` groups, each `AND`-ed with the top-level predicates.
    pub fn or_groups(&self) -> &[Vec<Constraint>] {
        &self.or_groups
    }

    /// `GROUP BY projection`.
    pub fn group_by(mut self, projection: Projection) -> Self {
        self.groups.push(projection);
        self
    }

    /// `ORDER BY alias` — ordering by something [`select`](Self::select) named.
    pub fn order_by_alias(mut self, alias: impl Into<String>, descending: bool) -> Self {
        self.alias_orders.push((alias.into(), descending));
        self
    }

    /// The selected projections, if any.
    pub fn projections(&self) -> &[(Projection, String)] {
        &self.projections
    }

    /// What the query groups by.
    pub fn groups(&self) -> &[Projection] {
        &self.groups
    }

    /// Ordering expressed against selected aliases.
    pub fn alias_orders(&self) -> &[(String, bool)] {
        &self.alias_orders
    }

    /// Joins that carry a kind, including outer ones.
    pub fn typed_joins(&self) -> impl Iterator<Item = (&str, &str, &str, JoinKind)> {
        self.typed_joins.iter().map(|(t, l, f, k)| (t.as_str(), l.as_str(), f.as_str(), *k))
    }

    /// `ORDER BY column ASC`.
    pub fn order_by(mut self, column: impl Into<String>) -> Self {
        self.orders.push((column.into(), false));
        self
    }

    /// `ORDER BY column DESC`.
    pub fn order_by_desc(mut self, column: impl Into<String>) -> Self {
        self.orders.push((column.into(), true));
        self
    }

    /// `LIMIT n`.
    ///
    /// Clamped to `i64::MAX`. Drivers bind these as a signed 64-bit integer and
    /// **panic** on anything larger — so `limit(u64::MAX)`, the obvious way to
    /// write "no limit", used to take the process down inside sea-query rather
    /// than return an error. No table has more than `i64::MAX` rows, so the
    /// clamp changes no result.
    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n.min(i64::MAX as u64));
        self
    }

    /// `OFFSET n`. Clamped like [`limit`](Self::limit), and for the same
    /// reason.
    pub fn offset(mut self, n: u64) -> Self {
        self.offset = Some(n.min(i64::MAX as u64));
        self
    }

    /// Combine with `other`: its constraints, joins and orders are appended,
    /// and its paging wins wherever it sets any.
    pub fn merge(mut self, other: Criteria) -> Self {
        self.constraints.extend(other.constraints);
        self.joins.extend(other.joins);
        self.orders.extend(other.orders);
        self.limit = other.limit.or(self.limit);
        self.offset = other.offset.or(self.offset);
        self
    }

    /// Apply `scope` only if `condition` holds.
    pub fn when(self, condition: bool, scope: impl FnOnce(Self) -> Self) -> Self {
        if condition {
            scope(self)
        } else {
            self
        }
    }

    /// Whether any filter or join is recorded. Ordering and paging alone do
    /// not make a criteria non-empty.
    pub fn is_empty(&self) -> bool {
        self.constraints.is_empty() && self.joins.is_empty()
    }

    /// The recorded predicates.
    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    /// The recorded joins, as `(table, local, foreign)`.
    pub fn joins(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.joins.iter().map(|(t, l, f)| (t.as_str(), l.as_str(), f.as_str()))
    }

    /// The recorded ordering, as `(column, descending)`.
    pub fn orders(&self) -> &[(String, bool)] {
        &self.orders
    }

    /// The recorded limit.
    pub fn limit_value(&self) -> Option<u64> {
        self.limit
    }

    /// The recorded offset.
    pub fn offset_value(&self) -> Option<u64> {
        self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_empty() {
        let criteria = Criteria::new();
        assert!(criteria.is_empty());
        assert_eq!(criteria.limit_value(), None);
        assert!(criteria.constraints().is_empty());
    }

    #[test]
    fn a_limit_beyond_what_a_driver_can_bind_is_clamped() {
        // `u64::MAX` is how you write "everything", and the sqlite and postgres
        // binders both `unwrap` the conversion to `i64` — so this used to be a
        // panic from inside the driver, several layers below the caller.
        let criteria = Criteria::new().limit(u64::MAX).offset(u64::MAX);

        assert_eq!(criteria.limit_value(), Some(i64::MAX as u64));
        assert_eq!(criteria.offset_value(), Some(i64::MAX as u64));
    }

    #[test]
    fn records_constraints_in_order() {
        let criteria = Criteria::new().where_eq("published", true).where_gt("views", 100_i64);

        assert!(!criteria.is_empty());
        assert_eq!(criteria.constraints().len(), 2);
        assert_eq!(criteria.constraints()[0].column(), "published");
        assert_eq!(criteria.constraints()[1].column(), "views");
    }

    #[test]
    fn only_equalities_report_a_routable_pair() {
        let criteria = Criteria::new().where_eq("user_id", 1_u64).where_gt("user_id", 1_u64);

        assert!(criteria.constraints()[0].as_equality().is_some());
        assert!(
            criteria.constraints()[1].as_equality().is_none(),
            "a range could span every shard"
        );
    }

    #[test]
    fn ordering_and_paging_alone_leave_it_empty() {
        let criteria = Criteria::new().order_by_desc("id").limit(5);
        assert!(criteria.is_empty(), "nothing is being filtered");
        assert_eq!(criteria.limit_value(), Some(5));
    }

    #[test]
    fn merge_appends_and_prefers_the_others_paging() {
        let base = Criteria::new().where_eq("a", 1_i64).limit(5).offset(10);
        let extra = Criteria::new().where_eq("b", 2_i64).limit(20);

        let merged = base.merge(extra);
        assert_eq!(merged.constraints().len(), 2);
        assert_eq!(merged.limit_value(), Some(20), "the merged-in limit wins");
        assert_eq!(merged.offset_value(), Some(10), "the original offset survives");
    }

    #[test]
    fn merge_keeps_the_original_paging_when_the_other_sets_none() {
        let merged = Criteria::new().limit(5).merge(Criteria::new().where_eq("a", 1_i64));
        assert_eq!(merged.limit_value(), Some(5));
    }

    #[test]
    fn when_applies_a_scope_conditionally() {
        assert_eq!(Criteria::new().when(true, |c| c.where_eq("a", 1_i64)).constraints().len(), 1);
        assert!(Criteria::new().when(false, |c| c.where_eq("a", 1_i64)).is_empty());
    }

    #[test]
    fn joins_are_recorded_with_both_sides() {
        let criteria = Criteria::new().join("authors", "author_id", "id");
        assert!(!criteria.is_empty());
        assert_eq!(criteria.joins().collect::<Vec<_>>(), vec![("authors", "author_id", "id")]);
    }

    #[test]
    fn ordering_records_its_direction() {
        let criteria = Criteria::new().order_by("a").order_by_desc("b");
        assert_eq!(criteria.orders(), &[("a".to_string(), false), ("b".to_string(), true)]);
    }
}
