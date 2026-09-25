//! SQL expressions and predicates, built as values and rendered per dialect.
//!
//! [`Criteria`](crate::Criteria)'s `where_*` methods compare one column against
//! one bound value, and for most queries that is the whole story. The rest —
//! `LOWER(TRIM(location)) = ?`, `COALESCE(a, b, c) <= ?`,
//! `(a AND b) OR (c AND d)`, a self-join, an anti-join,
//! `HAVING COUNT(*) >= ?`, `SET n = GREATEST(n - ?, 0)`, a `CASE` in an
//! `ORDER BY` — used to leave the builder for a raw string, and took the
//! dialect handling, the shard routing and the soft-delete scope with it. This
//! module is where those live instead.
//!
//! ```
//! use rainier_database::expression::*;
//!
//! // `(decommissioned_at IS NOT NULL AND decommissioned_at < ?)
//! //   OR (decommissioned_at IS NULL AND last_heartbeat_at < ?)`
//! # let cutoff = 0_i64;
//! let stale = any([
//!     all([col("decommissioned_at").is_not_null(), col("decommissioned_at").lt(cutoff)]),
//!     all([col("decommissioned_at").is_null(), col("last_heartbeat_at").lt(cutoff)]),
//! ]);
//!
//! // `LOWER(TRIM(location)) = ?`
//! let same_place = lower(trim(col("location"))).eq("berlin");
//!
//! // `GREATEST(CAST(ref_count AS SIGNED) - ?, 0)` on MySQL, `MAX(…)` on SQLite.
//! let released = greatest([cast(col("ref_count"), CastAs::Integer).minus(1_i64), val(0_i64)]);
//! ```
//!
//! # Two rules that keep it safe
//!
//! **Values are always bound.** Anything that is not a column name — a number,
//! a string, a pattern — becomes a placeholder parameter. `From<T: Into<Value>>`
//! makes a bare literal a *value*, never a column: `col("x").eq("y")` compares
//! against the string `'y'`. A column on the right-hand side is spelled
//! [`col`], so there is no string that can be read as either.
//!
//! **Identifiers are quoted, never interpolated.** A column spec is split on
//! its dot and each half is rendered as a quoted identifier by the dialect, so
//! there is no column name that reaches the SQL as syntax.
//!
//! # Column specs
//!
//! Exactly as everywhere else in this crate: `"name"` is a column of the
//! query's own table (inside a [`SubSelect`], of the sub-select's table),
//! `"table.name"` or `"alias.name"` a column of a table or alias the query
//! names. A self-join is two aliases of one table — see
//! [`Criteria::join_as`](crate::Criteria::join_as).

use rainier_orm::sea_query::{
    Alias, Asterisk, BinOper, ColumnRef, Cond, Expr, ExprTrait as _, Func, IntoColumnRef, LikeExpr,
    Query as SqQuery, SelectStatement, SubQueryStatement, Value,
};
use rainier_orm::Dialect;

use crate::criteria::{AggregateFn, Comparison, DatePart};

/// A value-producing SQL expression.
///
/// Build it with the free functions in this module ([`col`], [`val`],
/// [`lower`], [`coalesce`], [`case`], [`count_all`], …) and the methods on
/// this type ([`plus`](Self::plus), [`eq`](Self::eq), [`like`](Self::like), …).
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    /// A column — see the module docs for how a spec resolves.
    Column(String),
    /// A bound value.
    Value(Value),
    /// A portable function call, rendered however each dialect spells it.
    Function(Function, Vec<Expression>),
    /// `left <op> right`, parenthesised by construction.
    Arithmetic(Box<Expression>, Arithmetic, Box<Expression>),
    /// `CAST(expr AS …)`, with the type named the way each dialect names it.
    Cast(Box<Expression>, CastAs),
    /// `CASE WHEN … THEN … [ELSE …] END`.
    Case(Vec<(Predicate, Expression)>, Option<Box<Expression>>),
    /// An aggregate. `argument: None` is `COUNT(*)`.
    Aggregate {
        /// Which aggregate.
        function: AggregateFn,
        /// What it reads; `None` only for `COUNT(*)`.
        argument: Option<Box<Expression>>,
        /// `COUNT(DISTINCT …)` and friends.
        distinct: bool,
    },
    /// A part of a date — see [`DatePart`].
    DatePart(DatePart, Box<Expression>),
    /// A window function: `ROW_NUMBER() OVER (PARTITION BY … ORDER BY …)`.
    Window(Box<Window>),
    /// A scalar sub-select: `(SELECT … )`, which must produce one value.
    SubSelect(Box<SubSelect>),
}

/// An arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arithmetic {
    /// `+`.
    Add,
    /// `-`.
    Subtract,
    /// `*`.
    Multiply,
    /// `/`. Integer division on integers, on every dialect but Postgres's
    /// `numeric`; cast one side to [`CastAs::Real`] for a fraction.
    Divide,
    /// `%`.
    Modulo,
}

/// A portable function. Each is rendered the way its dialect spells it, which
/// is the point of it being a closed set rather than a function name string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Function {
    /// `LOWER(x)`.
    Lower,
    /// `UPPER(x)`.
    Upper,
    /// `TRIM(x)` — both ends, spaces.
    Trim,
    /// Length in **characters**: `CHAR_LENGTH` on MySQL, whose `LENGTH`
    /// counts bytes; `LENGTH` elsewhere.
    Length,
    /// `ABS(x)`.
    Abs,
    /// `ROUND(x)` or `ROUND(x, digits)`.
    Round,
    /// `COALESCE(a, b, …)` — the first non-null.
    Coalesce,
    /// `NULLIF(a, b)` — `NULL` when equal, else `a`.
    NullIf,
    /// The largest of its arguments. `GREATEST` on MySQL and Postgres, the
    /// multi-argument `MAX` on SQLite. A `NULL` argument makes the result
    /// `NULL` on MySQL and SQLite; Postgres ignores it. Wrap an argument in
    /// [`coalesce`] if that difference matters.
    Greatest,
    /// The smallest of its arguments — the counterpart of [`Greatest`](Self::Greatest),
    /// with the same `NULL` caveat.
    Least,
    /// String concatenation. `CONCAT` on MySQL, `||` on Postgres and SQLite —
    /// which keeps `NULL` absorbing everywhere, where Postgres's own `CONCAT`
    /// would skip it.
    Concat,
}

/// A type to [`cast`] to, named the way each dialect names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastAs {
    /// A signed whole number: `SIGNED` / `BIGINT` / `INTEGER`. Casting an
    /// unsigned column here before subtracting is what keeps `n - 1` from
    /// wrapping when `n` is `0` on MySQL.
    Integer,
    /// An unsigned whole number on MySQL; `BIGINT` / `INTEGER` elsewhere,
    /// which have no unsigned type.
    Unsigned,
    /// A floating-point number: `DOUBLE` / `DOUBLE PRECISION` / `REAL`.
    Real,
    /// Text: `CHAR` / `TEXT` / `TEXT`.
    Text,
    /// A calendar date: `DATE`, or `date(x)` on SQLite, which has no DATE type.
    Date,
}

/// A window function.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    function: WindowFunction,
    partition_by: Vec<Expression>,
    order_by: Vec<(Expression, bool)>,
}

/// Which window function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFunction {
    /// `ROW_NUMBER()` — 1, 2, 3… within the partition, no ties.
    RowNumber,
    /// `RANK()` — ties share a rank, and leave a gap after.
    Rank,
    /// `DENSE_RANK()` — ties share a rank, no gap.
    DenseRank,
}

impl Window {
    /// `PARTITION BY` these — the groups the numbering restarts in.
    pub fn partition_by(mut self, expressions: impl IntoIterator<Item = Expression>) -> Self {
        self.partition_by.extend(expressions);
        self
    }

    /// `ORDER BY expr ASC` within each partition.
    pub fn order_by(mut self, expression: impl Into<Expression>) -> Self {
        self.order_by.push((expression.into(), false));
        self
    }

    /// `ORDER BY expr DESC` within each partition.
    pub fn order_by_desc(mut self, expression: impl Into<Expression>) -> Self {
        self.order_by.push((expression.into(), true));
        self
    }
}

impl From<Window> for Expression {
    fn from(window: Window) -> Self {
        Expression::Window(Box::new(window))
    }
}

/// A boolean condition — what a `WHERE`, an `ON`, a `HAVING` or a `CASE WHEN`
/// holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `left <op> right`, where either side is any expression — which is what
    /// makes column-to-column comparisons sayable.
    Compare(Expression, Comparison, Expression),
    /// `expr [NOT] LIKE ? [ESCAPE '!']`.
    Like {
        /// What is matched.
        expression: Expression,
        /// The pattern, bound as a value.
        pattern: String,
        /// The escape character the pattern was written with, if any.
        escape: Option<char>,
        /// `NOT LIKE`.
        negated: bool,
    },
    /// `expr [NOT] IN (a, b, …)`.
    In {
        /// What is tested.
        expression: Expression,
        /// The list — values or expressions.
        list: Vec<Expression>,
        /// `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] IN (SELECT …)`.
    InSelect {
        /// What is tested.
        expression: Expression,
        /// The one-column sub-select.
        select: Box<SubSelect>,
        /// `NOT IN`.
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high` — inclusive at both ends.
    Between {
        /// What is tested.
        expression: Expression,
        /// Lower bound.
        low: Expression,
        /// Upper bound.
        high: Expression,
        /// `NOT BETWEEN`.
        negated: bool,
    },
    /// `expr IS [NOT] NULL`.
    Null {
        /// What is tested.
        expression: Expression,
        /// `IS NOT NULL`.
        negated: bool,
    },
    /// `[NOT] EXISTS (SELECT … )`.
    Exists {
        /// The sub-select.
        select: Box<SubSelect>,
        /// `NOT EXISTS`.
        negated: bool,
    },
    /// Every one of these — `(a AND b AND …)`. Empty is true.
    All(Vec<Predicate>),
    /// Any one of these — `(a OR b OR …)`. Empty is false.
    Any(Vec<Predicate>),
    /// `NOT (…)`.
    Not(Box<Predicate>),
}

/// A `SELECT` used inside an expression: `EXISTS (…)`, `IN (…)`, or a scalar
/// `(…)`.
///
/// Uncorrelated by default. Correlate it by naming the outer query's columns
/// qualified — `col("posts.id")` — in its predicates; its own columns are
/// unqualified, or qualified with its [`alias`](Self::alias). Always aliased:
/// a sub-select over the outer query's own table is otherwise the same name in
/// two scopes, and `t.id = t.parent_id` would compare the inner row with
/// itself. See `SUBQUERY_ALIAS` in the statement builder for that history.
#[derive(Debug, Clone, PartialEq)]
pub struct SubSelect {
    table: String,
    alias: String,
    select: Vec<Expression>,
    filter: Vec<Predicate>,
    group_by: Vec<Expression>,
    limit: Option<u64>,
}

/// The alias a [`SubSelect`] gets unless it is given one.
const DEFAULT_SUB_ALIAS: &str = "_rainier_sub";

impl SubSelect {
    /// `SELECT … FROM table AS _rainier_sub`.
    pub fn from(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            alias: DEFAULT_SUB_ALIAS.to_string(),
            select: Vec::new(),
            filter: Vec::new(),
            group_by: Vec::new(),
            limit: None,
        }
    }

    /// Name the sub-select's table. Needed when two sub-selects in one query
    /// must refer to each other, or for readability in a self-correlation.
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = alias.into();
        self
    }

    /// Add a selected expression. An `EXISTS` needs none; an `IN` or a scalar
    /// needs exactly one.
    pub fn select(mut self, expression: impl Into<Expression>) -> Self {
        self.select.push(expression.into());
        self
    }

    /// `AND` a predicate.
    pub fn filter(mut self, predicate: Predicate) -> Self {
        self.filter.push(predicate);
        self
    }

    /// `GROUP BY`.
    pub fn group_by(mut self, expression: impl Into<Expression>) -> Self {
        self.group_by.push(expression.into());
        self
    }

    /// `LIMIT`.
    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }

    /// The table read.
    pub fn table(&self) -> &str {
        &self.table
    }
}

impl From<SubSelect> for Expression {
    fn from(select: SubSelect) -> Self {
        Expression::SubSelect(Box::new(select))
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

/// A column — `"name"` or `"table.name"`.
pub fn col(spec: impl Into<String>) -> Expression {
    Expression::Column(spec.into())
}

/// A bound value.
pub fn val(value: impl Into<Value>) -> Expression {
    Expression::Value(value.into())
}

/// A bare value converts to a *bound value*, never to a column. See the
/// module docs.
impl<T: Into<Value>> From<T> for Expression {
    fn from(value: T) -> Self {
        Expression::Value(value.into())
    }
}

fn function(f: Function, args: impl IntoIterator<Item = Expression>) -> Expression {
    Expression::Function(f, args.into_iter().collect())
}

/// `LOWER(x)`.
pub fn lower(x: impl Into<Expression>) -> Expression {
    function(Function::Lower, [x.into()])
}

/// `UPPER(x)`.
pub fn upper(x: impl Into<Expression>) -> Expression {
    function(Function::Upper, [x.into()])
}

/// `TRIM(x)`.
pub fn trim(x: impl Into<Expression>) -> Expression {
    function(Function::Trim, [x.into()])
}

/// Length in characters — see [`Function::Length`].
pub fn length(x: impl Into<Expression>) -> Expression {
    function(Function::Length, [x.into()])
}

/// `ABS(x)`.
pub fn abs(x: impl Into<Expression>) -> Expression {
    function(Function::Abs, [x.into()])
}

/// `ROUND(x, digits)`.
pub fn round(x: impl Into<Expression>, digits: i64) -> Expression {
    function(Function::Round, [x.into(), val(digits)])
}

/// `COALESCE(a, b, …)`.
pub fn coalesce(args: impl IntoIterator<Item = Expression>) -> Expression {
    function(Function::Coalesce, args)
}

/// `NULLIF(a, b)`.
pub fn nullif(a: impl Into<Expression>, b: impl Into<Expression>) -> Expression {
    function(Function::NullIf, [a.into(), b.into()])
}

/// The largest argument — see [`Function::Greatest`].
pub fn greatest(args: impl IntoIterator<Item = Expression>) -> Expression {
    function(Function::Greatest, args)
}

/// The smallest argument — see [`Function::Least`].
pub fn least(args: impl IntoIterator<Item = Expression>) -> Expression {
    function(Function::Least, args)
}

/// String concatenation — see [`Function::Concat`].
pub fn concat(args: impl IntoIterator<Item = Expression>) -> Expression {
    function(Function::Concat, args)
}

/// `CAST(x AS …)`.
pub fn cast(x: impl Into<Expression>, to: CastAs) -> Expression {
    Expression::Cast(Box::new(x.into()), to)
}

/// A date part — see [`DatePart`].
pub fn date_part(part: DatePart, x: impl Into<Expression>) -> Expression {
    Expression::DatePart(part, Box::new(x.into()))
}

fn aggregate(function: AggregateFn, argument: Expression, distinct: bool) -> Expression {
    Expression::Aggregate { function, argument: Some(Box::new(argument)), distinct }
}

/// `COUNT(*)`.
pub fn count_all() -> Expression {
    Expression::Aggregate { function: AggregateFn::Count, argument: None, distinct: false }
}

/// `COUNT(x)` — non-null values.
pub fn count(x: impl Into<Expression>) -> Expression {
    aggregate(AggregateFn::Count, x.into(), false)
}

/// `COUNT(DISTINCT x)`.
pub fn count_distinct(x: impl Into<Expression>) -> Expression {
    aggregate(AggregateFn::Count, x.into(), true)
}

/// `SUM(x)`.
pub fn sum(x: impl Into<Expression>) -> Expression {
    aggregate(AggregateFn::Sum, x.into(), false)
}

/// `MIN(x)`.
pub fn min(x: impl Into<Expression>) -> Expression {
    aggregate(AggregateFn::Min, x.into(), false)
}

/// `MAX(x)`.
pub fn max(x: impl Into<Expression>) -> Expression {
    aggregate(AggregateFn::Max, x.into(), false)
}

/// `AVG(x)`.
pub fn avg(x: impl Into<Expression>) -> Expression {
    aggregate(AggregateFn::Avg, x.into(), false)
}

fn window(function: WindowFunction) -> Window {
    Window { function, partition_by: Vec::new(), order_by: Vec::new() }
}

/// `ROW_NUMBER() OVER (…)`.
pub fn row_number() -> Window {
    window(WindowFunction::RowNumber)
}

/// `RANK() OVER (…)`.
pub fn rank() -> Window {
    window(WindowFunction::Rank)
}

/// `DENSE_RANK() OVER (…)`.
pub fn dense_rank() -> Window {
    window(WindowFunction::DenseRank)
}

/// A `CASE` expression under construction.
#[derive(Debug, Clone, PartialEq, Default)]
#[must_use = "a CASE is an expression; finish it with `otherwise` or `end`"]
pub struct CaseBuilder {
    arms: Vec<(Predicate, Expression)>,
}

/// Start `CASE WHEN condition THEN result …`.
pub fn case(condition: Predicate, result: impl Into<Expression>) -> CaseBuilder {
    CaseBuilder { arms: vec![(condition, result.into())] }
}

impl CaseBuilder {
    /// Another `WHEN … THEN …`, tried in order.
    pub fn when(mut self, condition: Predicate, result: impl Into<Expression>) -> Self {
        self.arms.push((condition, result.into()));
        self
    }

    /// `ELSE result END`.
    pub fn otherwise(self, result: impl Into<Expression>) -> Expression {
        Expression::Case(self.arms, Some(Box::new(result.into())))
    }

    /// `END` with no `ELSE` — `NULL` when no arm matches.
    pub fn end(self) -> Expression {
        Expression::Case(self.arms, None)
    }
}

/// Every predicate holds.
pub fn all(predicates: impl IntoIterator<Item = Predicate>) -> Predicate {
    Predicate::All(predicates.into_iter().collect())
}

/// At least one predicate holds.
pub fn any(predicates: impl IntoIterator<Item = Predicate>) -> Predicate {
    Predicate::Any(predicates.into_iter().collect())
}

/// The predicate does not hold.
pub fn not(predicate: Predicate) -> Predicate {
    Predicate::Not(Box::new(predicate))
}

/// `EXISTS (SELECT …)`.
pub fn exists(select: SubSelect) -> Predicate {
    Predicate::Exists { select: Box::new(select), negated: false }
}

/// `NOT EXISTS (SELECT …)`.
pub fn not_exists(select: SubSelect) -> Predicate {
    Predicate::Exists { select: Box::new(select), negated: true }
}

/// The escape character [`Expression::contains`] and friends write with.
///
/// Not a backslash: MySQL treats a backslash inside a string literal as an
/// escape of its own, so `ESCAPE '\'` needs different quoting there than on
/// the other two, and one character that means nothing to any of them keeps
/// the rendering identical.
pub const LIKE_ESCAPE: char = '!';

/// `term` with every LIKE wildcard and the escape character escaped, so it
/// matches itself literally.
///
/// What any user-typed search term needs before it is put in a pattern: left
/// alone, `%` and `_` in the term are wildcards, and a search for `50%` or
/// `a_b` matches things it should not.
pub fn escape_like(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if c == LIKE_ESCAPE || c == '%' || c == '_' {
            out.push(LIKE_ESCAPE);
        }
        out.push(c);
    }
    out
}

impl Expression {
    fn compare(self, op: Comparison, right: impl Into<Expression>) -> Predicate {
        Predicate::Compare(self, op, right.into())
    }

    /// `self = right`.
    pub fn eq(self, right: impl Into<Expression>) -> Predicate {
        self.compare(Comparison::Eq, right)
    }

    /// `self <> right`.
    pub fn ne(self, right: impl Into<Expression>) -> Predicate {
        self.compare(Comparison::Ne, right)
    }

    /// `self > right`.
    pub fn gt(self, right: impl Into<Expression>) -> Predicate {
        self.compare(Comparison::Gt, right)
    }

    /// `self >= right`.
    pub fn gte(self, right: impl Into<Expression>) -> Predicate {
        self.compare(Comparison::Gte, right)
    }

    /// `self < right`.
    pub fn lt(self, right: impl Into<Expression>) -> Predicate {
        self.compare(Comparison::Lt, right)
    }

    /// `self <= right`.
    pub fn lte(self, right: impl Into<Expression>) -> Predicate {
        self.compare(Comparison::Lte, right)
    }

    /// `self IS NULL`.
    pub fn is_null(self) -> Predicate {
        Predicate::Null { expression: self, negated: false }
    }

    /// `self IS NOT NULL`.
    pub fn is_not_null(self) -> Predicate {
        Predicate::Null { expression: self, negated: true }
    }

    /// `self IN (…)`. An empty list matches nothing, on every dialect.
    pub fn is_in<T: Into<Expression>>(self, list: impl IntoIterator<Item = T>) -> Predicate {
        Predicate::In {
            expression: self,
            list: list.into_iter().map(Into::into).collect(),
            negated: false,
        }
    }

    /// `self NOT IN (…)`. An empty list matches everything.
    pub fn not_in<T: Into<Expression>>(self, list: impl IntoIterator<Item = T>) -> Predicate {
        Predicate::In {
            expression: self,
            list: list.into_iter().map(Into::into).collect(),
            negated: true,
        }
    }

    /// `self IN (SELECT …)`.
    pub fn in_select(self, select: SubSelect) -> Predicate {
        Predicate::InSelect { expression: self, select: Box::new(select), negated: false }
    }

    /// `self NOT IN (SELECT …)`. Mind `NULL`: one `NULL` in the sub-select's
    /// result makes this match nothing. [`not_exists`] has no such trap.
    pub fn not_in_select(self, select: SubSelect) -> Predicate {
        Predicate::InSelect { expression: self, select: Box::new(select), negated: true }
    }

    /// `self BETWEEN low AND high`, inclusive.
    pub fn between(self, low: impl Into<Expression>, high: impl Into<Expression>) -> Predicate {
        Predicate::Between { expression: self, low: low.into(), high: high.into(), negated: false }
    }

    /// `self NOT BETWEEN low AND high`.
    pub fn not_between(self, low: impl Into<Expression>, high: impl Into<Expression>) -> Predicate {
        Predicate::Between { expression: self, low: low.into(), high: high.into(), negated: true }
    }

    /// `self LIKE pattern`, the pattern used as written — `%` and `_` are
    /// wildcards. For a user-typed term use [`contains`](Self::contains),
    /// [`starts_with`](Self::starts_with) or [`ends_with`](Self::ends_with).
    pub fn like(self, pattern: impl Into<String>) -> Predicate {
        Predicate::Like { expression: self, pattern: pattern.into(), escape: None, negated: false }
    }

    /// `self NOT LIKE pattern`, used as written.
    pub fn not_like(self, pattern: impl Into<String>) -> Predicate {
        Predicate::Like { expression: self, pattern: pattern.into(), escape: None, negated: true }
    }

    fn like_escaped(self, pattern: String) -> Predicate {
        Predicate::Like { expression: self, pattern, escape: Some(LIKE_ESCAPE), negated: false }
    }

    /// `self` contains `term` literally — wildcards in `term` are escaped.
    pub fn contains(self, term: &str) -> Predicate {
        self.like_escaped(format!("%{}%", escape_like(term)))
    }

    /// `self` starts with `term` literally.
    pub fn starts_with(self, term: &str) -> Predicate {
        self.like_escaped(format!("{}%", escape_like(term)))
    }

    /// `self` ends with `term` literally.
    pub fn ends_with(self, term: &str) -> Predicate {
        self.like_escaped(format!("%{}", escape_like(term)))
    }

    fn arithmetic(self, op: Arithmetic, right: impl Into<Expression>) -> Expression {
        Expression::Arithmetic(Box::new(self), op, Box::new(right.into()))
    }

    /// `self + right`.
    pub fn plus(self, right: impl Into<Expression>) -> Expression {
        self.arithmetic(Arithmetic::Add, right)
    }

    /// `self - right`.
    pub fn minus(self, right: impl Into<Expression>) -> Expression {
        self.arithmetic(Arithmetic::Subtract, right)
    }

    /// `self * right`.
    pub fn times(self, right: impl Into<Expression>) -> Expression {
        self.arithmetic(Arithmetic::Multiply, right)
    }

    /// `self / right`.
    pub fn divided_by(self, right: impl Into<Expression>) -> Expression {
        self.arithmetic(Arithmetic::Divide, right)
    }

    /// `self % right`.
    pub fn modulo(self, right: impl Into<Expression>) -> Expression {
        self.arithmetic(Arithmetic::Modulo, right)
    }

    /// Every column spec this expression reads, in order, repeats included.
    pub fn columns(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expression::Column(c) => out.push(c),
            Expression::Value(_) | Expression::SubSelect(_) => {}
            Expression::Function(_, args) => args.iter().for_each(|a| a.collect_columns(out)),
            Expression::Arithmetic(l, _, r) => {
                l.collect_columns(out);
                r.collect_columns(out);
            }
            Expression::Cast(e, _) | Expression::DatePart(_, e) => e.collect_columns(out),
            Expression::Case(arms, otherwise) => {
                for (_, result) in arms {
                    result.collect_columns(out);
                }
                if let Some(e) = otherwise {
                    e.collect_columns(out);
                }
            }
            Expression::Aggregate { argument, .. } => {
                if let Some(e) = argument {
                    e.collect_columns(out);
                }
            }
            Expression::Window(w) => {
                w.partition_by.iter().for_each(|e| e.collect_columns(out));
                w.order_by.iter().for_each(|(e, _)| e.collect_columns(out));
            }
        }
    }
}

impl Predicate {
    /// `self AND other`.
    pub fn and(self, other: Predicate) -> Predicate {
        match self {
            Predicate::All(mut list) => {
                list.push(other);
                Predicate::All(list)
            }
            first => Predicate::All(vec![first, other]),
        }
    }

    /// `self OR other`.
    pub fn or(self, other: Predicate) -> Predicate {
        match self {
            Predicate::Any(mut list) => {
                list.push(other);
                Predicate::Any(list)
            }
            first => Predicate::Any(vec![first, other]),
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// How the columns of one query scope resolve.
///
/// The outer query resolves a bare name against its model's table; a
/// sub-select against its own alias. Qualified names mean what they say in
/// both.
pub(crate) type Resolve<'a> = &'a dyn Fn(&str) -> ColumnRef;

fn qualified(spec: &str, default_table: &str) -> ColumnRef {
    match spec.split_once('.') {
        Some((table, column)) => (Alias::new(table), Alias::new(column)).into_column_ref(),
        None => (Alias::new(default_table), Alias::new(spec)).into_column_ref(),
    }
}

/// A template placeholder for `n` (1-based) in this dialect.
fn placeholder(dialect: Dialect, n: usize) -> String {
    match dialect {
        Dialect::Postgres => format!("${n}"),
        _ => "?".to_string(),
    }
}

/// Render an [`Expression`].
pub(crate) fn render_expression(dialect: Dialect, e: &Expression, resolve: Resolve<'_>) -> Expr {
    let r = |e: &Expression| render_expression(dialect, e, resolve);
    match e {
        Expression::Column(spec) => Expr::col(resolve(spec)),
        Expression::Value(v) => Expr::val(v.clone()),
        Expression::Function(f, args) => render_function(dialect, *f, args, resolve),
        Expression::Arithmetic(left, op, right) => {
            let (left, right) = (r(left), r(right));
            match op {
                Arithmetic::Add => left.add(right),
                Arithmetic::Subtract => left.sub(right),
                Arithmetic::Multiply => left.mul(right),
                Arithmetic::Divide => left.div(right),
                Arithmetic::Modulo => left.modulo(right),
            }
        }
        Expression::Cast(inner, to) => render_cast(dialect, r(inner), *to),
        Expression::Case(arms, otherwise) => {
            let mut arms = arms.iter();
            let (first_when, first_then) = arms.next().expect("a CASE is built with one arm");
            let mut case =
                Expr::case(render_predicate(dialect, first_when, resolve), r(first_then));
            for (when, then) in arms {
                case = case.case(render_predicate(dialect, when, resolve), r(then));
            }
            match otherwise {
                Some(e) => case.finally(r(e)).into(),
                None => case.into(),
            }
        }
        Expression::Aggregate { function, argument, distinct } => {
            let argument = match argument {
                Some(a) => r(a),
                None => return Func::count(Expr::col(Asterisk)).into(),
            };
            match (function, distinct) {
                (AggregateFn::Count, true) => Func::count_distinct(argument).into(),
                (AggregateFn::Count, false) => Func::count(argument).into(),
                (AggregateFn::Sum, true) => Expr::cust_with_exprs(
                    format!("SUM(DISTINCT {})", placeholder(dialect, 1)),
                    [argument],
                ),
                (AggregateFn::Sum, false) => Func::sum(argument).into(),
                (AggregateFn::Min, _) => Func::min(argument).into(),
                (AggregateFn::Max, _) => Func::max(argument).into(),
                (AggregateFn::Avg, true) => Expr::cust_with_exprs(
                    format!("AVG(DISTINCT {})", placeholder(dialect, 1)),
                    [argument],
                ),
                (AggregateFn::Avg, false) => Func::avg(argument).into(),
            }
        }
        Expression::DatePart(part, inner) => render_date_part(dialect, *part, r(inner)),
        Expression::Window(window) => render_window(dialect, window, resolve),
        Expression::SubSelect(select) => Expr::SubQuery(
            None,
            Box::new(SubQueryStatement::SelectStatement(render_sub_select(dialect, select))),
        ),
    }
}

fn render_function(
    dialect: Dialect,
    f: Function,
    args: &[Expression],
    resolve: Resolve<'_>,
) -> Expr {
    let args: Vec<Expr> = args.iter().map(|a| render_expression(dialect, a, resolve)).collect();
    let call =
        |name: &str, args: Vec<Expr>| -> Expr { Func::cust(Alias::new(name)).args(args).into() };

    match f {
        Function::Lower => call("LOWER", args),
        Function::Upper => call("UPPER", args),
        Function::Trim => call("TRIM", args),
        Function::Length => match dialect {
            Dialect::MySql => call("CHAR_LENGTH", args),
            _ => call("LENGTH", args),
        },
        Function::Abs => call("ABS", args),
        Function::Round => call("ROUND", args),
        Function::Coalesce => call("COALESCE", args),
        Function::NullIf => call("NULLIF", args),
        Function::Greatest => match dialect {
            Dialect::Sqlite => call("MAX", args),
            _ => call("GREATEST", args),
        },
        Function::Least => match dialect {
            Dialect::Sqlite => call("MIN", args),
            _ => call("LEAST", args),
        },
        Function::Concat => match dialect {
            Dialect::MySql => call("CONCAT", args),
            _ => {
                let mut args = args.into_iter();
                let first = args.next().unwrap_or_else(|| Expr::val(""));
                args.fold(first, |acc, next| acc.binary(BinOper::Custom("||"), next))
            }
        },
    }
}

fn render_cast(dialect: Dialect, inner: Expr, to: CastAs) -> Expr {
    let name = match (dialect, to) {
        (Dialect::Sqlite, CastAs::Date) => {
            return Func::cust(Alias::new("date")).arg(inner).into();
        }
        (Dialect::MySql, CastAs::Integer) => "SIGNED",
        (Dialect::MySql, CastAs::Unsigned) => "UNSIGNED",
        (Dialect::MySql, CastAs::Real) => "DOUBLE",
        (Dialect::MySql, CastAs::Text) => "CHAR",
        (Dialect::Postgres, CastAs::Integer | CastAs::Unsigned) => "BIGINT",
        (Dialect::Postgres, CastAs::Real) => "DOUBLE PRECISION",
        (Dialect::Postgres, CastAs::Text) => "TEXT",
        (Dialect::Sqlite, CastAs::Integer | CastAs::Unsigned) => "INTEGER",
        (Dialect::Sqlite, CastAs::Real) => "REAL",
        (Dialect::Sqlite, CastAs::Text) => "TEXT",
        (_, CastAs::Date) => "DATE",
    };
    Func::cast_as(inner, Alias::new(name)).into()
}

fn render_date_part(dialect: Dialect, part: DatePart, inner: Expr) -> Expr {
    let (mysql, sqlite, postgres) = match part {
        DatePart::Year => ("YEAR", "%Y", "year"),
        DatePart::Month => ("MONTH", "%m", "month"),
        DatePart::Day => ("DAY", "%d", "day"),
    };
    match dialect {
        Dialect::Sqlite => Func::cast_as(
            Func::cust(Alias::new("strftime")).args([Expr::val(sqlite), inner]),
            Alias::new("INTEGER"),
        )
        .into(),
        Dialect::Postgres => {
            Func::cust(Alias::new("date_part")).args([Expr::val(postgres), inner]).into()
        }
        _ => Func::cust(Alias::new(mysql)).arg(inner).into(),
    }
}

/// `FN() OVER (PARTITION BY … ORDER BY …)`, as a template whose only text is
/// fixed keywords — every expression in it goes in as a placeholder argument,
/// so nothing a caller supplies is written into the SQL.
fn render_window(dialect: Dialect, window: &Window, resolve: Resolve<'_>) -> Expr {
    let name = match window.function {
        WindowFunction::RowNumber => "ROW_NUMBER()",
        WindowFunction::Rank => "RANK()",
        WindowFunction::DenseRank => "DENSE_RANK()",
    };
    let mut args = Vec::new();
    let mut n = 0;
    let mut next = |dialect| {
        n += 1;
        placeholder(dialect, n)
    };

    let mut over = Vec::new();
    if !window.partition_by.is_empty() {
        let parts: Vec<String> = window
            .partition_by
            .iter()
            .map(|e| {
                args.push(render_expression(dialect, e, resolve));
                next(dialect)
            })
            .collect();
        over.push(format!("PARTITION BY {}", parts.join(", ")));
    }
    if !window.order_by.is_empty() {
        let parts: Vec<String> = window
            .order_by
            .iter()
            .map(|(e, descending)| {
                args.push(render_expression(dialect, e, resolve));
                format!("{}{}", next(dialect), if *descending { " DESC" } else { " ASC" })
            })
            .collect();
        over.push(format!("ORDER BY {}", parts.join(", ")));
    }
    Expr::cust_with_exprs(format!("{name} OVER ({})", over.join(" ")), args)
}

/// Render a [`Predicate`].
pub(crate) fn render_predicate(dialect: Dialect, p: &Predicate, resolve: Resolve<'_>) -> Expr {
    let e = |x: &Expression| render_expression(dialect, x, resolve);
    match p {
        Predicate::Compare(left, op, right) => {
            let (left, right) = (e(left), e(right));
            match op {
                Comparison::Eq => left.eq(right),
                Comparison::Ne => left.ne(right),
                Comparison::Gt => left.gt(right),
                Comparison::Gte => left.gte(right),
                Comparison::Lt => left.lt(right),
                Comparison::Lte => left.lte(right),
            }
        }
        Predicate::Like { expression, pattern, escape, negated } => {
            let mut like = LikeExpr::new(pattern.clone());
            if let Some(c) = escape {
                like = like.escape(*c);
            }
            if *negated {
                e(expression).not_like(like)
            } else {
                e(expression).like(like)
            }
        }
        Predicate::In { expression, list, negated } => {
            // An empty list is `FALSE` for IN and `TRUE` for NOT IN on every
            // dialect, rather than `IN ()`, which MySQL and Postgres reject.
            if list.is_empty() {
                return Expr::cust(if *negated { "1 = 1" } else { "1 = 0" });
            }
            let list: Vec<Expr> = list.iter().map(e).collect();
            if *negated {
                e(expression).is_not_in(list)
            } else {
                e(expression).is_in(list)
            }
        }
        Predicate::InSelect { expression, select, negated } => {
            let select = render_sub_select(dialect, select);
            if *negated {
                e(expression).not_in_subquery(select)
            } else {
                e(expression).in_subquery(select)
            }
        }
        Predicate::Between { expression, low, high, negated } => {
            if *negated {
                e(expression).not_between(e(low), e(high))
            } else {
                e(expression).between(e(low), e(high))
            }
        }
        Predicate::Null { expression, negated } => {
            if *negated {
                e(expression).is_not_null()
            } else {
                e(expression).is_null()
            }
        }
        Predicate::Exists { select, negated } => {
            let exists = Expr::exists(render_exists_select(dialect, select));
            if *negated {
                exists.not()
            } else {
                exists
            }
        }
        Predicate::All(list) => render_condition(dialect, false, list, resolve).into(),
        Predicate::Any(list) => render_condition(dialect, true, list, resolve).into(),
        Predicate::Not(inner) => render_predicate(dialect, inner, resolve).not(),
    }
}

fn render_condition(dialect: Dialect, any: bool, list: &[Predicate], resolve: Resolve<'_>) -> Cond {
    if list.is_empty() {
        // `Cond::all()` of nothing renders nothing, which inside a larger
        // condition would silently vanish. Say what an empty group means.
        let constant = if any { "1 = 0" } else { "1 = 1" };
        return Cond::all().add(Expr::cust(constant));
    }
    let mut cond = if any { Cond::any() } else { Cond::all() };
    for p in list {
        cond = cond.add(render_predicate(dialect, p, resolve));
    }
    cond
}

/// A [`Predicate`] as a `Cond`, to `AND` into a statement.
pub(crate) fn predicate_condition(dialect: Dialect, p: &Predicate, resolve: Resolve<'_>) -> Cond {
    match p {
        Predicate::All(list) => render_condition(dialect, false, list, resolve),
        Predicate::Any(list) => render_condition(dialect, true, list, resolve),
        other => Cond::all().add(render_predicate(dialect, other, resolve)),
    }
}

fn sub_select_statement(
    dialect: Dialect,
    select: &SubSelect,
    existence_only: bool,
) -> SelectStatement {
    let alias = select.alias.clone();
    let resolve = move |spec: &str| qualified(spec, &alias);

    let mut stmt = SqQuery::select();
    stmt.from_as(Alias::new(&select.table), Alias::new(&select.alias));
    if existence_only || select.select.is_empty() {
        // A constant rather than a bound `1`: see `subquery_select` in the
        // statement builder — a bound one would be a stray parameter.
        stmt.expr(Expr::Constant(Value::Int(Some(1))));
    } else {
        for e in &select.select {
            stmt.expr(render_expression(dialect, e, &resolve));
        }
    }
    let mut cond = Cond::all();
    for p in &select.filter {
        cond = cond.add(predicate_condition(dialect, p, &resolve));
    }
    stmt.cond_where(cond);
    for g in &select.group_by {
        stmt.add_group_by([render_expression(dialect, g, &resolve)]);
    }
    if let Some(n) = select.limit {
        stmt.limit(n);
    }
    stmt
}

fn render_sub_select(dialect: Dialect, select: &SubSelect) -> SelectStatement {
    sub_select_statement(dialect, select, false)
}

fn render_exists_select(dialect: Dialect, select: &SubSelect) -> SelectStatement {
    sub_select_statement(dialect, select, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outer(spec: &str) -> ColumnRef {
        qualified(spec, "posts")
    }

    fn sql(dialect: Dialect, p: &Predicate) -> (String, Vec<Value>) {
        let mut stmt = SqQuery::select();
        stmt.from(Alias::new("posts")).expr(Expr::Constant(Value::Int(Some(1))));
        stmt.cond_where(predicate_condition(dialect, p, &outer));
        let (sql, values) = dialect.build_query(&stmt);
        (sql, values.0)
    }

    #[test]
    fn a_bare_literal_is_a_bound_value_not_a_column() {
        let (sql, params) = sql(Dialect::Sqlite, &col("title").eq("id"));
        assert!(sql.contains(r#""posts"."title" = ?"#), "{sql}");
        assert_eq!(params, vec![Value::from("id")]);
    }

    #[test]
    fn column_to_column_comparison() {
        let (sql, params) = sql(Dialect::MySql, &col("a").ne(col("b.c")));
        assert!(sql.contains("`posts`.`a` <> `b`.`c`"), "{sql}");
        assert!(params.is_empty());
    }

    #[test]
    fn nested_boolean_groups_keep_their_shape() {
        let p = any([
            all([col("x").is_not_null(), col("x").lt(1_i64)]),
            all([col("x").is_null(), col("y").lt(2_i64)]),
        ]);
        let (sql, params) = sql(Dialect::Sqlite, &p);
        assert!(
            sql.contains(
                r#"("posts"."x" IS NOT NULL AND "posts"."x" < ?) OR ("posts"."x" IS NULL AND "posts"."y" < ?)"#
            ),
            "{sql}"
        );
        assert_eq!(params, vec![Value::from(1_i64), Value::from(2_i64)]);
    }

    #[test]
    fn greatest_is_max_on_sqlite_and_greatest_elsewhere() {
        let e = greatest([cast(col("n"), CastAs::Integer).minus(1_i64), val(0_i64)]).gt(5_i64);
        assert!(sql(Dialect::Sqlite, &e).0.contains(r#"MAX(CAST("posts"."n" AS INTEGER) - ?, ?)"#));
        assert!(sql(Dialect::MySql, &e).0.contains("GREATEST(CAST(`posts`.`n` AS SIGNED) - ?, ?)"));
        assert!(sql(Dialect::Postgres, &e)
            .0
            .contains(r#"GREATEST(CAST("posts"."n" AS BIGINT) - $1, $2)"#));
    }

    #[test]
    fn concat_keeps_null_absorbing_on_every_dialect() {
        let p = concat([col("a"), val("-"), col("b")]).eq("x");
        assert!(sql(Dialect::MySql, &p).0.contains("CONCAT(`posts`.`a`, ?, `posts`.`b`)"));
        let sqlite = sql(Dialect::Sqlite, &p).0;
        // Parenthesised pairwise, which `||` being associative makes the same thing.
        assert!(sqlite.contains(r#"(("posts"."a" || ?) || "posts"."b")"#), "{sqlite}");
    }

    #[test]
    fn a_user_term_is_escaped_before_it_becomes_a_pattern() {
        assert_eq!(escape_like("50%_off!"), "50!%!_off!!");
        let (sql, params) = sql(Dialect::Sqlite, &col("username").contains("a_b"));
        assert!(sql.contains("LIKE ? ESCAPE '!'"), "{sql}");
        assert_eq!(params, vec![Value::from("%a!_b%")]);
    }

    #[test]
    fn an_empty_in_list_is_false_and_an_empty_not_in_is_true() {
        let none: Vec<i64> = Vec::new();
        assert!(sql(Dialect::MySql, &col("id").is_in(none.clone())).0.contains("1 = 0"));
        assert!(sql(Dialect::MySql, &col("id").not_in(none)).0.contains("1 = 1"));
    }

    #[test]
    fn a_correlated_not_exists_names_both_scopes() {
        let p = not_exists(
            SubSelect::from("followers")
                .alias("f")
                .filter(col("f.followed_profile_id").eq(col("posts.profile_id")))
                .filter(col("profile_id").eq(7_i64)),
        );
        let (sql, params) = sql(Dialect::Sqlite, &p);
        assert!(sql.contains(r#"NOT EXISTS(SELECT 1 FROM "followers" AS "f""#), "{sql}");
        assert!(sql.contains(r#""f"."followed_profile_id" = "posts"."profile_id""#), "{sql}");
        assert!(sql.contains(r#""f"."profile_id" = ?"#), "{sql}");
        assert_eq!(params, vec![Value::from(7_i64)]);
    }

    #[test]
    fn a_case_orders_matches_before_the_rest() {
        let e = case(col("username").eq("amy"), 0_i64)
            .when(col("username").starts_with("amy"), 1_i64)
            .otherwise(2_i64);
        let (sql, _) = sql(Dialect::Sqlite, &e.lt(2_i64));
        assert!(sql.contains("CASE WHEN"), "{sql}");
        assert!(sql.contains("ELSE ? END"), "{sql}");
    }

    #[test]
    fn a_window_numbers_within_its_partition_with_every_expression_bound() {
        let e: Expression =
            row_number().partition_by([col("profile_id")]).order_by_desc(col("score")).into();
        let p = e.lte(3_i64);
        let (mysql, _) = sql(Dialect::MySql, &p);
        assert!(
            mysql.contains("ROW_NUMBER() OVER (PARTITION BY `posts`.`profile_id` ORDER BY `posts`.`score` DESC)"),
            "{mysql}"
        );
        let (pg, _) = sql(Dialect::Postgres, &p);
        assert!(
            pg.contains(r#"ROW_NUMBER() OVER (PARTITION BY "posts"."profile_id" ORDER BY "posts"."score" DESC)"#),
            "{pg}"
        );
    }

    #[test]
    fn length_counts_characters_on_mysql() {
        assert!(sql(Dialect::MySql, &length(col("t")).gt(3_i64)).0.contains("CHAR_LENGTH("));
        assert!(sql(Dialect::Sqlite, &length(col("t")).gt(3_i64)).0.contains("LENGTH("));
    }

    #[test]
    fn columns_lists_what_an_expression_reads() {
        let e = coalesce([col("a"), col("b.c")]).plus(val(1_i64));
        assert_eq!(e.columns(), vec!["a", "b.c"]);
    }
}
