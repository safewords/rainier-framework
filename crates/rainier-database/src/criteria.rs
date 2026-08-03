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

use rainier_orm::sea_query::{ColumnRef, Expr, Func, SimpleExpr, Value};

/// One recorded predicate.
#[derive(Debug, Clone)]
pub enum Constraint {
    /// `LOWER(column) = LOWER(value)`.
    EqCi(String, Value),
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
            Constraint::EqCi(column, _)
            | Constraint::Eq(column, _)
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
            // `LOWER(col) = LOWER(?)`, which is the same in every dialect —
            // unlike a date part, this needs no per-dialect branch.
            Constraint::EqCi(_, value) => SimpleExpr::FunctionCall(Func::lower(expr))
                .eq(SimpleExpr::FunctionCall(Func::lower(Expr::val(value.clone())))),
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

/// How a scalar subquery's one value is compared against a bound value.
///
/// An enum rather than six `where_subquery_gte`-shaped methods: the left-hand
/// operand is a whole [`Subquery`], so every one of those would have to repeat
/// the subquery argument, and the set of operators would still be closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// `=`.
    Eq,
    /// `<>`.
    Ne,
    /// `>`.
    Gt,
    /// `>=`.
    Gte,
    /// `<`.
    Lt,
    /// `<=`.
    Lte,
}

/// A subquery over another table, run once for each row the outer query
/// examines, and tied to that row.
///
/// The tie is the whole point. `EXISTS (SELECT 1 FROM t WHERE t.owner = <the
/// outer row>.id)` compares a column against **another column**, not against a
/// bound value, and a builder that only knows `column = value` cannot say it —
/// which is why the two query shapes this type exists for were the last ones an
/// application still had to write as raw SQL.
///
/// # Why it cannot be built uncorrelated
///
/// Forgetting the correlation produces the worst kind of wrong answer.
/// `EXISTS (SELECT 1 FROM t)` is true for *every* outer row the moment `t` holds
/// a single row, so the predicate silently matches the entire table. Nothing
/// errors, the SQL reads plausibly, and the only symptom is more rows than the
/// caller meant to expose — which, for a visibility filter, is the rows of every
/// other user.
///
/// So a `Subquery` cannot be constructed at all without one:
/// [`Subquery::count`] and [`Subquery::select`] hand back a [`SubqueryDraft`],
/// [`SubqueryDraft::correlate`] is the only way to turn a draft into a
/// `Subquery`, and every method that accepts a subquery accepts the correlated
/// type. The mistake is not caught late — it is unwritable.
///
/// ```
/// # use rainier_database::{Criteria, Subquery};
/// // "rows that have at least one approved child"
/// let scope = Criteria::new().where_exists(
///     Subquery::count("children").correlate("parent_id", "id").where_eq("approved", true),
/// );
/// assert_eq!(scope.subquery_predicates().len(), 1);
/// ```
///
/// # What it deliberately cannot do
///
/// A subquery holds `AND`-combined column-against-value predicates and its
/// correlations, and nothing else: no joins, no `OR` groups, and no subquery of
/// its own. That bound is enforced by the type rather than documented and
/// dropped at render time — there is no closure here through which a nested
/// predicate could be handed in and then quietly ignored.
#[derive(Debug, Clone)]
pub struct Subquery {
    table: String,
    projection: Projection,
    /// `(inner_column, outer_column)`, never empty — see the type's docs.
    correlations: Vec<(String, String)>,
    constraints: Vec<Constraint>,
}

/// A [`Subquery`] that is not correlated yet, and so cannot be used.
///
/// It exists only to be consumed by [`correlate`](Self::correlate). See
/// [`Subquery`] for why the uncorrelated state is worth a type of its own.
#[derive(Debug, Clone)]
#[must_use = "a draft is not a subquery until `correlate` ties it to the outer row"]
pub struct SubqueryDraft {
    table: String,
    projection: Projection,
}

impl SubqueryDraft {
    /// Tie the subquery to the outer row: `inner_column = outer_column`.
    ///
    /// `inner_column` is a column of the subquery's own table.
    /// `outer_column` is read like every other column spec in this module —
    /// `"name"` is a column of the outer query's own table, `"table.name"` one
    /// of a table it joined.
    ///
    /// Call it again on the result to correlate on a second column, which is
    /// what a composite foreign key needs; matching on only half of one is the
    /// same silent over-match a missing correlation is.
    pub fn correlate(
        self,
        inner_column: impl Into<String>,
        outer_column: impl Into<String>,
    ) -> Subquery {
        Subquery {
            table: self.table,
            projection: self.projection,
            correlations: vec![(inner_column.into(), outer_column.into())],
            constraints: Vec::new(),
        }
    }
}

impl Subquery {
    /// `SELECT COUNT(*) FROM table …` — the counting form.
    ///
    /// Returns a [`SubqueryDraft`]; correlate it to get a usable `Subquery`.
    pub fn count(table: impl Into<String>) -> SubqueryDraft {
        Self::select(table, Projection::CountAll)
    }

    /// `SELECT <projection> FROM table …` — the general form.
    ///
    /// Any [`Projection`] the outer query could select is available here, so a
    /// `SUM` or a `MAX` over related rows needs nothing new. Mind the empty-set
    /// behaviour when the result is assigned: `COUNT` of no rows is `0`, but
    /// `SUM`, `MIN`, `MAX` and `AVG` of no rows are `NULL` — which writes `NULL`
    /// into the target column, or fails outright if it is `NOT NULL`.
    pub fn select(table: impl Into<String>, projection: Projection) -> SubqueryDraft {
        SubqueryDraft { table: table.into(), projection }
    }

    /// Correlate on a further column pair — see [`SubqueryDraft::correlate`].
    pub fn correlate(
        mut self,
        inner_column: impl Into<String>,
        outer_column: impl Into<String>,
    ) -> Self {
        self.correlations.push((inner_column.into(), outer_column.into()));
        self
    }

    /// `column = value`, on the subquery's own table.
    pub fn where_eq(self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.and(Constraint::Eq(column.into(), value.into()))
    }

    /// `column <> value`.
    pub fn where_ne(self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.and(Constraint::Ne(column.into(), value.into()))
    }

    /// `LOWER(column) = LOWER(value)` — see [`Criteria::where_eq_ci`].
    pub fn where_eq_ci(self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.and(Constraint::EqCi(column.into(), value.into()))
    }

    /// `column > value`.
    pub fn where_gt(self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.and(Constraint::Gt(column.into(), value.into()))
    }

    /// `column >= value`.
    pub fn where_gte(self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.and(Constraint::Gte(column.into(), value.into()))
    }

    /// `column < value`.
    pub fn where_lt(self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.and(Constraint::Lt(column.into(), value.into()))
    }

    /// `column <= value`.
    pub fn where_lte(self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.and(Constraint::Lte(column.into(), value.into()))
    }

    /// `column LIKE pattern`.
    pub fn where_like(self, column: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.and(Constraint::Like(column.into(), pattern.into()))
    }

    /// `column NOT LIKE pattern`.
    pub fn where_not_like(self, column: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.and(Constraint::NotLike(column.into(), pattern.into()))
    }

    /// `column IN (values)`. An empty set matches nothing.
    pub fn where_in<V: Into<Value>>(
        self,
        column: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let values = values.into_iter().map(Into::into).collect();
        self.and(Constraint::In(column.into(), values))
    }

    /// `column NOT IN (values)`.
    pub fn where_not_in<V: Into<Value>>(
        self,
        column: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let values = values.into_iter().map(Into::into).collect();
        self.and(Constraint::NotIn(column.into(), values))
    }

    /// `column IS NULL`.
    pub fn where_null(self, column: impl Into<String>) -> Self {
        self.and(Constraint::Null(column.into()))
    }

    /// `column IS NOT NULL`.
    pub fn where_not_null(self, column: impl Into<String>) -> Self {
        self.and(Constraint::NotNull(column.into()))
    }

    fn and(mut self, constraint: Constraint) -> Self {
        self.constraints.push(constraint);
        self
    }

    /// The table the subquery reads.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// What it selects — ignored where only existence is asked for.
    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    /// The `(inner_column, outer_column)` pairs tying it to the outer row.
    /// Never empty.
    pub fn correlations(&self) -> &[(String, String)] {
        &self.correlations
    }

    /// Its own predicates, `AND`-combined with the correlations.
    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }
}

/// A predicate whose left-hand side is a whole [`Subquery`] rather than a
/// column.
///
/// Kept apart from [`Constraint`] because a `Constraint` is defined by the
/// column it applies to — [`Constraint::column`] returns one, and shard routing
/// reads it. A subquery predicate has no such column, so folding it in would
/// mean either a lying `column()` or an `Option` at every existing call site.
#[derive(Debug, Clone)]
pub enum SubqueryPredicate {
    /// `EXISTS (…)`.
    Exists(Subquery),
    /// `NOT EXISTS (…)`.
    NotExists(Subquery),
    /// `(…) <op> value` — the scalar form.
    Compare(Subquery, Comparison, Value),
}

impl SubqueryPredicate {
    /// The subquery this predicate runs.
    pub fn subquery(&self) -> &Subquery {
        match self {
            SubqueryPredicate::Exists(sub)
            | SubqueryPredicate::NotExists(sub)
            | SubqueryPredicate::Compare(sub, _, _) => sub,
        }
    }
}

/// What an `UPDATE … SET` writes into a column.
///
/// A bound value is the ordinary case. The subquery case is what makes a bulk
/// counter recomputation one statement: `SET n = (SELECT COUNT(*) … WHERE …
/// = <this row>.id)` visits every row once, and — because a `COUNT` over no
/// rows is `0` rather than no row at all — it writes zero to the rows with no
/// related records instead of leaving whatever was there before.
///
/// The loop it replaces cannot do that. A `GROUP BY` produces no group for a
/// count of zero, so a per-row loop has to zero every counter first and fill
/// them back in, leaving a window in which every row on the platform reads zero.
/// One statement has no such window.
#[derive(Debug, Clone)]
pub enum Assignment {
    /// A bound value.
    Value(Value),
    /// A correlated subquery, re-evaluated for each updated row.
    Subquery(Subquery),
}

impl From<Value> for Assignment {
    fn from(value: Value) -> Self {
        Assignment::Value(value)
    }
}

impl From<Subquery> for Assignment {
    fn from(subquery: Subquery) -> Self {
        Assignment::Subquery(subquery)
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
    /// Predicates whose left side is a correlated subquery, `AND`-ed with the
    /// rest.
    subqueries: Vec<SubqueryPredicate>,
    /// `SELECT DISTINCT`.
    distinct: bool,
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

    /// `LOWER(column) = LOWER(value)` — equality ignoring case.
    ///
    /// Not a nicety. MySQL's usual collations compare text case-insensitively,
    /// while SQLite and Postgres do not, so a plain `where_eq` on a username
    /// behaves differently depending on which database is behind it. An
    /// application ported from MySQL keeps working in production and starts
    /// failing to find rows in its own test suite — the same shape of bug as a
    /// dialect-specific function, arrived at through a default nobody set.
    pub fn where_eq_ci(mut self, column: impl Into<String>, value: impl Into<Value>) -> Self {
        self.constraints.push(Constraint::EqCi(column.into(), value.into()));
        self
    }

    /// `SELECT DISTINCT …`.
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Whether the query is `DISTINCT`.
    pub fn is_distinct(&self) -> bool {
        self.distinct
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

    /// `EXISTS (<subquery>)` — keep the rows a related table has a match for.
    ///
    /// The reason to reach for this over a join: a join that matches twice
    /// duplicates the outer row, so "posts with at least one comment" has to be
    /// followed by a `DISTINCT` that then has to be kept in step with every
    /// column the query selects. `EXISTS` stops at the first matching inner row
    /// and cannot change the outer row count.
    ///
    /// The subquery's own projection is ignored here and `SELECT 1` is emitted
    /// instead — deliberately, because `EXISTS (SELECT COUNT(*) …)` is true for
    /// every row: an aggregate with no `GROUP BY` always returns exactly one
    /// row, even when it counts nothing.
    ///
    /// ```
    /// # use rainier_database::{Criteria, Subquery};
    /// Criteria::new().where_exists(
    ///     Subquery::count("comments").correlate("post_id", "id").where_null("deleted_at"),
    /// );
    /// ```
    pub fn where_exists(mut self, subquery: Subquery) -> Self {
        self.subqueries.push(SubqueryPredicate::Exists(subquery));
        self
    }

    /// `NOT EXISTS (<subquery>)` — keep the rows a related table has *no* match
    /// for.
    ///
    /// The anti-join. Its usual stand-in — `LEFT JOIN … WHERE right.col IS
    /// NULL` — reads "no matching row" off a null, so it is wrong for any
    /// column the matching row could itself hold null in: the row is there and
    /// the test says it is not.
    pub fn where_not_exists(mut self, subquery: Subquery) -> Self {
        self.subqueries.push(SubqueryPredicate::NotExists(subquery));
        self
    }

    /// `(<subquery>) <op> value` — compare a correlated scalar against a bound
    /// value.
    ///
    /// What answers "rows whose related table holds exactly *n* matches" —
    /// which `EXISTS` cannot state, since it stops at the first match. The
    /// alternative is a join with `GROUP BY … HAVING COUNT(*) = ?`, and that
    /// changes the outer query rather than filtering it: the result becomes one
    /// row per group, and every column being selected has to join the `GROUP BY`
    /// to stay legal. A correlated scalar leaves the outer query's shape alone.
    ///
    /// ```
    /// # use rainier_database::{Comparison, Criteria, Subquery};
    /// // rows related to exactly two others
    /// Criteria::new().where_subquery(
    ///     Subquery::count("links").correlate("parent_id", "id"),
    ///     Comparison::Eq,
    ///     2_i64,
    /// );
    /// ```
    pub fn where_subquery(
        mut self,
        subquery: Subquery,
        comparison: Comparison,
        value: impl Into<Value>,
    ) -> Self {
        self.subqueries.push(SubqueryPredicate::Compare(subquery, comparison, value.into()));
        self
    }

    /// The subquery predicates, each `AND`-ed with the rest.
    pub fn subquery_predicates(&self) -> &[SubqueryPredicate] {
        &self.subqueries
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
        // Dropping these would *widen* the result: a scope whose whole purpose
        // is an `EXISTS` — "only the chats I am in" — would merge into an
        // unfiltered query and return everything.
        self.subqueries.extend(other.subqueries);
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
    ///
    /// Every form of filtering counts, including the ones that are not plain
    /// constraints. A criteria whose only predicate is an `OR` group or an
    /// `EXISTS` is filtering just as hard, and reporting it empty invites a
    /// caller to skip the `WHERE` and read the whole table.
    pub fn is_empty(&self) -> bool {
        self.constraints.is_empty()
            && self.joins.is_empty()
            && self.or_groups.is_empty()
            && self.subqueries.is_empty()
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

    // --- correlated subqueries ---------------------------------------------

    fn children() -> Subquery {
        Subquery::count("children").correlate("parent_id", "id")
    }

    #[test]
    fn a_subquery_is_correlated_the_moment_it_exists() {
        // The guarantee the two-type split buys: there is no path to a
        // `Subquery` that skips `correlate`, so `correlations()` is never empty
        // and an `EXISTS` can never degenerate into "the inner table has rows".
        //
        // The compile-time half of this cannot be asserted here — it is that
        // `Criteria::where_exists` takes a `Subquery` and `Subquery::count`
        // returns a `SubqueryDraft`, so an uncorrelated one does not typecheck.
        assert_eq!(children().correlations(), &[("parent_id".to_string(), "id".to_string())]);
    }

    #[test]
    fn a_subquery_records_its_table_projection_and_predicates() {
        let subquery = Subquery::select("children", Projection::Sum("weight".into()))
            .correlate("parent_id", "id")
            .where_eq("approved", true)
            .where_null("deleted_at");

        assert_eq!(subquery.table(), "children");
        assert_eq!(subquery.projection(), &Projection::Sum("weight".into()));
        assert_eq!(subquery.constraints().len(), 2);
        assert_eq!(subquery.constraints()[0].column(), "approved");
        assert_eq!(subquery.constraints()[1].column(), "deleted_at");
    }

    #[test]
    fn correlating_again_adds_a_pair_rather_than_replacing_one() {
        let subquery = children().correlate("tenant_id", "tenant_id");
        assert_eq!(subquery.correlations().len(), 2, "a composite key needs both halves");
    }

    #[test]
    fn a_criteria_filtered_only_by_a_subquery_is_not_empty() {
        // Reporting it empty invites a caller to skip the `WHERE` — and this
        // particular filter is usually the one deciding whose rows these are.
        assert!(!Criteria::new().where_exists(children()).is_empty());
        assert!(!Criteria::new().where_not_exists(children()).is_empty());
        assert!(!Criteria::new().where_subquery(children(), Comparison::Eq, 2_i64).is_empty());
        assert!(!Criteria::new().or_where(|any| any.where_eq("a", 1_i64)).is_empty());
    }

    #[test]
    fn subquery_predicates_are_recorded_in_order_with_their_kind() {
        let criteria = Criteria::new()
            .where_exists(children())
            .where_not_exists(Subquery::count("bans").correlate("parent_id", "id"))
            .where_subquery(children(), Comparison::Gte, 3_i64);

        let recorded = criteria.subquery_predicates();
        assert_eq!(recorded.len(), 3);
        assert!(matches!(recorded[0], SubqueryPredicate::Exists(_)));
        assert!(matches!(recorded[1], SubqueryPredicate::NotExists(_)));
        assert!(matches!(recorded[2], SubqueryPredicate::Compare(_, Comparison::Gte, _)));
        assert_eq!(recorded[1].subquery().table(), "bans");
    }

    #[test]
    fn merging_carries_the_subquery_predicates_across() {
        // Dropping them widens the result, which is the direction that leaks.
        let visible = Criteria::new().where_exists(children());
        let newest = Criteria::new().order_by_desc("id").limit(10);

        assert_eq!(visible.clone().merge(newest.clone()).subquery_predicates().len(), 1);
        assert_eq!(newest.merge(visible).subquery_predicates().len(), 1, "either direction");
    }

    #[test]
    fn an_assignment_converts_from_both_a_value_and_a_subquery() {
        assert!(matches!(Assignment::from(Value::from(1_i64)), Assignment::Value(_)));
        assert!(matches!(Assignment::from(children()), Assignment::Subquery(_)));
    }
}
