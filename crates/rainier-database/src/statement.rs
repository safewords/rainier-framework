//! Statement preparation — building SQL **synchronously**, so the futures that
//! run it are `Send`.
//!
//! ## Why this module exists
//!
//! Rainier ORM's `repo::` functions build a `sea_query` statement and then
//! `await` the executor with that statement still in scope. `sea_query`'s
//! statement types hold `Rc<dyn Iden>`, so the statement is `!Send`, so the
//! generated future is `!Send` — and a future that is not `Send` cannot be
//! awaited inside a handler that a multi-threaded server will `tokio::spawn`.
//!
//! The fix is one line upstream (`drop(stmt);`, or scoping the build, before
//! the `await`). Until Rainier ORM does that, Rainier prepares statements here
//! instead: every `sea_query` value is created **and dropped inside a
//! synchronous function**, which hands back a [`Prepared`] holding only
//! `String` and `Value` — both `Send`. The `await` then touches nothing that
//! is not `Send`.
//!
//! ## What is *not* re-implemented
//!
//! The metadata, the dialect rendering, the column types, the row decoding and
//! the shard-routing *rule* all still come from Rainier ORM. This module is a
//! thin re-statement of `repo::`'s SQL shapes against the same
//! [`Entity`] metadata — not a second ORM. In particular,
//! routing behaviour is deliberately identical to Rainier ORM's, so a sharded
//! deployment routes the same way whether a query goes through here or through
//! `repo::` directly.

use rainier_orm::sea_query::{
    Alias, Asterisk, ColumnRef, Cond, Expr, Func, IntoColumnRef, JoinType, OnConflict, Order,
    Query as SqQuery, SelectStatement, SimpleExpr, SubQueryStatement, Value,
};
use rainier_orm::{
    key_condition, key_route, row_key_condition, ColumnType, Dialect, Entity, Result, ShardRoute,
    SingleKey, TrashScope, Upsert,
};

use crate::criteria::{
    Assignment, Comparison, Criteria, DatePart, JoinKind, Projection, Subquery, SubqueryPredicate,
};

/// A rendered statement: SQL, its ordered bind values, and where to run it.
///
/// Every field is `Send`, which is the entire point — see the module docs.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The rendered SQL.
    pub sql: String,
    /// The bind values, in the order the placeholders appear.
    pub params: Vec<Value>,
    /// The shard this operation targets. Ignored by single-database backends.
    pub route: ShardRoute,
}

/// Reduce a bound value to a `u64` routing key.
///
/// Mirrors Rainier ORM's own (crate-private) rule exactly: a numeric id is taken
/// as-is, because [`ShardCodec`](rainier_orm::ShardCodec) packs the shard into
/// its high bits; a string key is hashed with the ORM's `stable_hash`, which is
/// deliberately *not* `std`'s randomised hasher so the same key always routes
/// to the same shard across processes and builds.
pub fn routing_key(value: &Value) -> Option<u64> {
    Some(match value {
        Value::TinyInt(Some(n)) => *n as u64,
        Value::SmallInt(Some(n)) => *n as u64,
        Value::Int(Some(n)) => *n as u64,
        Value::BigInt(Some(n)) => *n as u64,
        Value::TinyUnsigned(Some(n)) => *n as u64,
        Value::SmallUnsigned(Some(n)) => *n as u64,
        Value::Unsigned(Some(n)) => *n as u64,
        Value::BigUnsigned(Some(n)) => *n,
        Value::String(Some(s)) => rainier_orm::stable_hash(s.as_bytes()),
        Value::Char(Some(c)) => rainier_orm::stable_hash(c.encode_utf8(&mut [0u8; 4]).as_bytes()),
        _ => return None,
    })
}

/// The route for constraining `column` to `value` on `E`.
pub fn route_for<E: Entity>(column: &str, value: &Value) -> ShardRoute {
    if E::shard_columns().contains(&column) {
        if let Some(key) = routing_key(value) {
            return ShardRoute::Key(key);
        }
    }
    ShardRoute::Global
}

/// The route implied by a whole row's `(column, value)` pairs: the first
/// shard-encoded column that carries a usable key.
fn route_from_pairs<E: Entity>(pairs: &[(&'static str, Value)]) -> ShardRoute {
    if E::shard_columns().is_empty() {
        return ShardRoute::Global;
    }
    for (column, value) in pairs {
        let route = route_for::<E>(column, value);
        if !matches!(route, ShardRoute::Global) {
            return route;
        }
    }
    ShardRoute::Global
}

fn alias(name: &str) -> Alias {
    Alias::new(name)
}

/// `"name"` → `E.table.name`; `"table.name"` → `table.name`.
fn column_ref<E: Entity>(spec: &str) -> ColumnRef {
    match spec.split_once('.') {
        Some((table, column)) => (alias(table), alias(column)).into_column_ref(),
        None => (alias(E::table()), alias(spec)).into_column_ref(),
    }
}

/// Start a `SELECT` of every column of `E`, qualified to its table.
fn select_columns<E: Entity>() -> rainier_orm::sea_query::SelectStatement {
    let mut stmt = SqQuery::select();
    stmt.from(alias(E::table()));
    for column in E::columns() {
        stmt.column((alias(E::table()), alias(column.name)));
    }
    stmt
}

/// Append the soft-delete predicate `scope` implies for `E`, if any.
///
/// **Every read builder in this module calls this**, and that uniformity is the
/// point rather than a tidiness: a `SELECT` and its `COUNT` disagreeing about
/// which rows exist is a paginator reporting a total it cannot produce, and one
/// builder honouring the scope while its neighbour does not is a difference no
/// call site can see.
///
/// The builders that name no [`Criteria`] pass [`TrashScope::Active`], because
/// there is nowhere for a caller to have said otherwise. The ones that take a
/// criteria pass **its** scope, which is where `with_trashed` and
/// `only_trashed` take effect — `apply_criteria` is the single seam, so it
/// cannot be honoured by `select_matching` and skipped by `count_matching`.
///
/// The write builders deliberately do not call this. A scoped `DELETE` leaves a
/// purge unable to purge, and a scoped `UPDATE` makes a bulk restore match
/// nothing — see [`rainier_orm::trash`], which sets the reads-only policy out
/// in full.
///
/// `no_select_builder_is_left_unscoped` in this crate's `tests/soft_deletes.rs`
/// fails if a `SELECT` is built here without going through this helper. Its
/// sibling in `rainier-orm` guards that crate's own two query modules; the
/// missing half of that pair is how this module came to be unscoped while
/// reading as covered.
///
/// The column is qualified to `E`'s table, like everything else this module
/// writes. That is load-bearing rather than stylistic: a criteria may join, and
/// a bare `deleted_at` is ambiguous the moment the joined table has one too — an
/// error out of the database, and only on the deployments whose schema happens
/// to collide.
fn scope_select<E: Entity>(stmt: &mut SelectStatement, scope: TrashScope) {
    if let Some(predicate) = rainier_orm::scope_predicate::<E>(scope, column_ref::<E>) {
        stmt.and_where(predicate);
    }
}

/// `SELECT … FROM table` — every row.
///
/// Every row that is not tombstoned, on a soft-deleting entity. So are the rest
/// of the readers below — they all route through the same private scoping step,
/// which is what stops one of them being left unscoped.
pub fn select_all<E: Entity>(dialect: Dialect) -> Prepared {
    let mut stmt = select_columns::<E>();
    scope_select::<E>(&mut stmt, TrashScope::Active);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route: ShardRoute::Global }
}

/// `SELECT … WHERE pk = ? LIMIT 1`.
///
/// Bounded on [`SingleKey`], because one [`Value`] cannot name a row in a table
/// keyed on two columns; see [`select_by_keys`].
pub fn select_by_pk<E: Entity + SingleKey>(dialect: Dialect, key: Value) -> Prepared {
    let route = route_for::<E>(E::primary_key(), &key);
    let mut stmt = select_columns::<E>();
    stmt.and_where(Expr::col(alias(E::primary_key())).eq(key)).limit(1);
    scope_select::<E>(&mut stmt, TrashScope::Active);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `SELECT … WHERE a = ? AND b = ? LIMIT 1` — the composite counterpart of
/// [`select_by_pk`].
///
/// `keys` are positional against [`Entity::primary_key_columns`]; a list of the
/// wrong length is an error rather than a partial match.
pub fn select_by_keys<E: Entity>(dialect: Dialect, keys: &[Value]) -> Result<Prepared> {
    let route = key_route::<E>(keys);
    let mut stmt = select_columns::<E>();
    stmt.cond_where(key_condition::<E>(keys)?).limit(1);
    scope_select::<E>(&mut stmt, TrashScope::Active);

    let (sql, params) = dialect.build_query(&stmt);
    Ok(Prepared { sql, params: params.0, route })
}

/// `SELECT … WHERE column = ?`, optionally limited.
pub fn select_by_column<E: Entity>(
    dialect: Dialect,
    column: &str,
    value: Value,
    limit: Option<u64>,
) -> Prepared {
    let route = route_for::<E>(column, &value);
    let mut stmt = select_columns::<E>();
    stmt.and_where(Expr::col(alias(column)).eq(value));
    scope_select::<E>(&mut stmt, TrashScope::Active);
    if let Some(limit) = limit {
        stmt.limit(limit);
    }

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// Render a [`Projection`] against the outer query's own table.
///
/// The date parts are the reason this is a function and not a string in the
/// caller: MySQL writes `MONTH(x)`, SQLite has no such function and needs
/// `CAST(strftime('%m', x) AS INTEGER)`, and Postgres spells it
/// `date_part('month', x)`. Application code that picks one works on one
/// deployment and 500s on the others — including the SQLite its own tests run.
fn projection_expr<E: Entity>(dialect: Dialect, projection: &Projection) -> SimpleExpr {
    projection_expr_in(dialect, projection, &column_ref::<E>, &integral_column::<E>)
}

/// Whether one of the entity's own columns holds a whole number.
///
/// Only consulted to decide whether a `SUM` needs a cast — see the MySQL note
/// in [`projection_expr_in`]. A column this entity does not declare, or a
/// qualified one belonging to a joined table, answers `false`: not casting is
/// the conservative choice, since a wrong cast truncates.
fn integral_column<E: Entity>(name: &str) -> bool {
    if name.contains('.') {
        return false;
    }
    E::columns().iter().find(|c| c.name == name).is_some_and(|c| {
        matches!(
            c.ty,
            ColumnType::Int | ColumnType::BigInt | ColumnType::Uint | ColumnType::BigUint
        )
    })
}

/// [`projection_expr`] with the scope its columns resolve against left open.
///
/// A subquery's projection reads *its* table, not the outer one, so the two
/// callers differ by nothing but that resolver. Duplicating the dialect branches
/// to say so is how one copy quietly keeps a `MONTH()` that SQLite cannot run.
fn projection_expr_in(
    dialect: Dialect,
    projection: &Projection,
    resolve: &dyn Fn(&str) -> ColumnRef,
    integral: &dyn Fn(&str) -> bool,
) -> SimpleExpr {
    let col = |name: &str| Expr::col(resolve(name));

    match projection {
        Projection::Column(c) => col(c).into(),
        Projection::CountAll => Func::count(Expr::col(Asterisk)).into(),
        Projection::Count(c) => Func::count(col(c)).into(),
        // `SUM` of an integer column, read back as an integer.
        //
        // MySQL types a `SUM` as `DECIMAL` whatever it summed, and the driver
        // will not hand a `DECIMAL` to an `i64` — "Rust type `Option<i64>` (as
        // SQL type `BIGINT`) is not compatible with SQL type `DECIMAL`". That
        // is a 500 on the whole query, not a wrong number, and it fires the
        // moment there is a single row to sum. Every application hitting it
        // wrote `CAST(... AS SIGNED)` into raw SQL and stopped using the
        // aggregate API, which is the wrong place for the fix to live.
        //
        // Only over a column declared as a whole number: casting a sum of
        // decimals to an integer would truncate it, and a silently wrong total
        // is worse than the error this avoids. Postgres and SQLite already
        // return an integer for an integer sum and need nothing.
        Projection::Sum(c) if dialect == Dialect::MySql && integral(c) => {
            Func::cast_as(Func::sum(col(c)), Alias::new("SIGNED")).into()
        }
        Projection::Sum(c) => Func::sum(col(c)).into(),
        Projection::Min(c) => Func::min(col(c)).into(),
        Projection::Max(c) => Func::max(col(c)).into(),
        Projection::Avg(c) => Func::avg(col(c)).into(),

        // `SUM(CASE WHEN col IN (…) THEN 1 ELSE 0 END)`, which counts the
        // matching rows of each group in the same pass as the total.
        Projection::CountWhenIn(c, values) => {
            let case = Expr::case(col(c).is_in(values.clone()), Expr::val(1)).finally(Expr::val(0));
            Func::sum(SimpleExpr::Case(Box::new(case))).into()
        }

        // The whole calendar date, not one component of it.
        Projection::DateOf(c) => match dialect {
            // `date(x)` yields `YYYY-MM-DD`; SQLite has no DATE type to cast to.
            Dialect::Sqlite => Func::cust(Alias::new("date")).arg(col(c)).into(),
            // MySQL and Postgres both accept the standard cast, and it keeps
            // the value ordering correctly as a date rather than as text.
            _ => Func::cast_as(col(c), Alias::new("DATE")).into(),
        },

        Projection::DatePart(part, c) => {
            let (mysql, sqlite, postgres) = match part {
                DatePart::Year => ("YEAR", "%Y", "year"),
                DatePart::Month => ("MONTH", "%m", "month"),
                DatePart::Day => ("DAY", "%d", "day"),
            };

            match dialect {
                // SQLite has no date functions of its own; `strftime` returns
                // text, so the cast is what makes `month` comparable and
                // sortable as a number rather than as "01" < "02" < "10".
                Dialect::Sqlite => Func::cast_as(
                    Func::cust(Alias::new("strftime"))
                        .args([SimpleExpr::from(Expr::val(sqlite)), col(c).into()]),
                    Alias::new("INTEGER"),
                )
                .into(),
                // `date_part('month', x)` rather than `EXTRACT(MONTH FROM x)`:
                // identical in Postgres, and an ordinary function call, so it
                // goes through the same builder path as the other two instead
                // of needing raw SQL for its unusual argument syntax.
                Dialect::Postgres => Func::cust(Alias::new("date_part"))
                    .args([SimpleExpr::from(Expr::val(postgres)), col(c).into()])
                    .into(),
                _ => Func::cust(Alias::new(mysql)).arg(col(c)).into(),
            }
        }
    }
}

/// The alias every subquery's table is given.
///
/// Always applied, and never chosen by the caller. A correlation is only a
/// correlation because its two sides name *different* scopes, and an unaliased
/// subquery over the outer query's own table has only one name for both — so a
/// self-correlated `EXISTS`, which is what any parent/child tree needs, would
/// render `t.parent_id = t.id` with both sides bound to the inner scope. The
/// predicate stops mentioning the outer row at all and the `EXISTS` collapses to
/// a constant: true for every row, or false for every row, with no error either
/// way. A fixed alias makes that unreachable, and leaving it out of the API
/// means no caller can reintroduce it by picking the outer table's name.
///
/// Reserved-looking on purpose: it only has to differ from the tables the outer
/// query names, and every one of those is a real table. Sibling subqueries may
/// share it — each is its own `FROM` scope — and a subquery cannot contain
/// another, so there is no nesting for it to shadow.
const SUBQUERY_ALIAS: &str = "_rainier_sub";

/// `"name"` → the subquery's own table; `"table.name"` → that table.
///
/// The same rule as [`column_ref`], so the outer half of a correlation can be
/// written the way every other column spec in this layer is.
fn subquery_column(spec: &str) -> ColumnRef {
    match spec.split_once('.') {
        Some((table, column)) => (alias(table), alias(column)).into_column_ref(),
        None => (alias(SUBQUERY_ALIAS), alias(spec)).into_column_ref(),
    }
}

/// The inner `SELECT` of a [`Subquery`], correlated to `E`'s row.
///
/// `existence_only` swaps the projection for `SELECT 1`. That is not a
/// shortcut: `EXISTS (SELECT COUNT(*) …)` is true for every outer row, because
/// an aggregate with no `GROUP BY` returns one row even when it counts nothing.
/// Emitting the caller's projection inside an `EXISTS` would turn the most
/// natural way to write the subquery into a filter that matches everything.
fn subquery_select<E: Entity>(
    dialect: Dialect,
    subquery: &Subquery,
    existence_only: bool,
) -> SelectStatement {
    let mut stmt = SqQuery::select();
    stmt.from_as(alias(subquery.table()), alias(SUBQUERY_ALIAS));

    if existence_only {
        // A constant, not `Expr::val(1)`, which sea-query would *bind* — every
        // `EXISTS` would then push a meaningless `1` into the parameter list,
        // between the caller's own values and in front of the ones that follow.
        // Nothing about it is caller-supplied, so nothing about it is injectable.
        stmt.expr(SimpleExpr::Constant(Value::Int(Some(1))));
    } else {
        // A subquery's projection is compared inside SQL rather than decoded,
        // so nothing needs the integer cast above — and the entity whose
        // columns would say whether to is the outer one, not this table's.
        stmt.expr(projection_expr_in(dialect, subquery.projection(), &subquery_column, &|_| false));
    }

    let mut condition = Cond::all();
    // The correlation first, so it is the visible head of the predicate in a
    // query plan and in anything that logs the SQL.
    for (inner, outer) in subquery.correlations() {
        condition = condition.add(Expr::col(subquery_column(inner)).equals(column_ref::<E>(outer)));
    }
    for constraint in subquery.constraints() {
        condition = condition.add(constraint.to_expr(subquery_column(constraint.column())));
    }
    stmt.cond_where(condition);

    stmt
}

/// A [`Subquery`] as a scalar expression: `(SELECT … )`.
fn subquery_scalar<E: Entity>(dialect: Dialect, subquery: &Subquery) -> SimpleExpr {
    SimpleExpr::SubQuery(
        None,
        Box::new(SubQueryStatement::SelectStatement(subquery_select::<E>(
            dialect, subquery, false,
        ))),
    )
}

/// Render one [`SubqueryPredicate`] against `E`'s row.
fn subquery_predicate_expr<E: Entity>(
    dialect: Dialect,
    predicate: &SubqueryPredicate,
) -> SimpleExpr {
    match predicate {
        SubqueryPredicate::Exists(subquery) => {
            Expr::exists(subquery_select::<E>(dialect, subquery, true))
        }
        SubqueryPredicate::NotExists(subquery) => {
            Expr::exists(subquery_select::<E>(dialect, subquery, true)).not()
        }
        SubqueryPredicate::Compare(subquery, comparison, value) => {
            let scalar = Expr::expr(subquery_scalar::<E>(dialect, subquery));
            let value = value.clone();
            match comparison {
                Comparison::Eq => scalar.eq(value),
                Comparison::Ne => scalar.ne(value),
                Comparison::Gt => scalar.gt(value),
                Comparison::Gte => scalar.gte(value),
                Comparison::Lt => scalar.lt(value),
                Comparison::Lte => scalar.lte(value),
            }
        }
    }
}

/// The `WHERE` a [`Criteria`] implies, and the shard route its equalities pin.
///
/// One function rather than the copy every statement kind used to keep: a
/// `SELECT`, an `UPDATE` and a `DELETE` built from the same criteria have to
/// filter identically, and four copies of the loop is four places for a new
/// predicate kind to be added to three of.
fn criteria_condition<E: Entity>(dialect: Dialect, criteria: &Criteria) -> (Cond, ShardRoute) {
    let mut condition = Cond::all();
    let mut route = ShardRoute::Global;

    for constraint in criteria.constraints() {
        // An equality on a shard-encoded column pins the query to one shard,
        // exactly as Rainier ORM's query builder does.
        if let (Some((column, value)), true) =
            (constraint.as_equality(), matches!(route, ShardRoute::Global))
        {
            route = route_for::<E>(column, value);
        }
        condition = condition.add(constraint.to_expr(column_ref::<E>(constraint.column())));
    }
    // Each `or_where` group is one parenthesised `OR`, `AND`-ed with the rest —
    // the shape it has in SQL, so there is no precedence to get wrong. Both
    // kinds of predicate are branches of that `OR`: a group that dropped its
    // `EXISTS` here would narrow, and one that held nothing else would render as
    // no group at all and match rows the caller excluded.
    for group in criteria.or_groups() {
        let mut any = Cond::any();
        for constraint in group.constraints() {
            any = any.add(constraint.to_expr(column_ref::<E>(constraint.column())));
        }
        for predicate in group.subquery_predicates() {
            any = any.add(subquery_predicate_expr::<E>(dialect, predicate));
        }
        condition = condition.add(any);
    }
    // Subqueries never move the route. The shard is chosen by the outer row's
    // own key, and the inner table's rows for that row live wherever they live —
    // a correlated subquery cannot name a shard, so pretending it could would
    // send the whole statement somewhere the outer rows are not.
    for predicate in criteria.subquery_predicates() {
        condition = condition.add(subquery_predicate_expr::<E>(dialect, predicate));
    }

    (condition, route)
}

/// `SELECT <projections> … GROUP BY …` under a [`Criteria`].
///
/// The escape hatch that means an application never has to write raw SQL for
/// an aggregate report — see [`Projection`].
pub fn select_aggregate<E: Entity>(dialect: Dialect, criteria: &Criteria) -> Prepared {
    let mut stmt = SqQuery::select();
    stmt.from(alias(E::table()));

    for (projection, name) in criteria.projections() {
        stmt.expr_as(projection_expr::<E>(dialect, projection), alias(name));
    }

    let route = apply_criteria::<E>(dialect, &mut stmt, criteria, true);

    for projection in criteria.groups() {
        stmt.add_group_by([projection_expr::<E>(dialect, projection)]);
    }

    for (name, descending) in criteria.alias_orders() {
        let order = if *descending { Order::Desc } else { Order::Asc };
        stmt.order_by(alias(name), order);
    }

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `SELECT <model columns>` under a [`Criteria`]'s filters, joins and paging.
pub fn select_matching<E: Entity>(dialect: Dialect, criteria: &Criteria) -> Prepared {
    let mut stmt = select_columns::<E>();
    let route = apply_criteria::<E>(dialect, &mut stmt, criteria, true);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `SELECT COUNT(*) AS cnt` under a [`Criteria`]'s filters and joins only.
///
/// Ordering and paging are dropped: counting a page's rows with the page's own
/// `LIMIT` applied would always report the page size.
pub fn count_matching<E: Entity>(dialect: Dialect, criteria: &Criteria) -> Prepared {
    let mut stmt = SqQuery::select();
    stmt.from(alias(E::table()));
    let route = apply_criteria::<E>(dialect, &mut stmt, criteria, false);
    stmt.expr_as(Func::count(Expr::col(Asterisk)), alias("cnt"));

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `SELECT column, COUNT(*) AS cnt … GROUP BY column`.
///
/// One row per distinct value, which is what turns "how many children does each
/// of these parents have" into a single query.
pub fn count_grouped<E: Entity>(dialect: Dialect, column: &str, criteria: &Criteria) -> Prepared {
    let mut stmt = SqQuery::select();
    stmt.from(alias(E::table()));
    let route = apply_criteria::<E>(dialect, &mut stmt, criteria, false);

    stmt.column(column_ref::<E>(column));
    stmt.expr_as(Func::count(Expr::col(Asterisk)), alias("cnt"));
    stmt.add_group_by(vec![SimpleExpr::Column(column_ref::<E>(column))]);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `SELECT parent, related FROM pivot WHERE parent IN (…)`.
///
/// A pivot table has no [`Entity`], because two key columns do not need one.
/// The route is global: a pivot spanning shards cannot be resolved from one
/// side's keys, and quietly picking a shard would silently lose the links held
/// on the others.
pub fn select_pivot(dialect: Dialect, query: &crate::relation::PivotQuery) -> Prepared {
    let mut stmt = SqQuery::select();
    stmt.from(alias(&query.table));
    stmt.column((alias(&query.table), alias(&query.parent_column)));
    stmt.column((alias(&query.table), alias(&query.related_column)));
    stmt.and_where(Expr::col(alias(&query.parent_column)).is_in(query.parent_keys.iter().cloned()));

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route: ShardRoute::Global }
}

/// Apply a criteria to a select. Returns the route the filters imply.
fn apply_criteria<E: Entity>(
    dialect: Dialect,
    stmt: &mut SelectStatement,
    criteria: &Criteria,
    with_paging: bool,
) -> ShardRoute {
    if criteria.is_distinct() {
        stmt.distinct();
    }

    for (table, local, foreign) in criteria.joins() {
        let on =
            Expr::col((alias(E::table()), alias(local))).equals((alias(table), alias(foreign)));
        stmt.join(JoinType::InnerJoin, alias(table), on);
    }

    for (table, local, foreign, kind) in criteria.typed_joins() {
        let on =
            Expr::col((alias(E::table()), alias(local))).equals((alias(table), alias(foreign)));
        let join = match kind {
            JoinKind::Inner => JoinType::InnerJoin,
            JoinKind::Left => JoinType::LeftJoin,
        };
        stmt.join(join, alias(table), on);
    }

    let (condition, route) = criteria_condition::<E>(dialect, criteria);
    stmt.cond_where(condition);

    // The criteria's own scope, not `Active`: a caller that said `with_trashed`
    // or `only_trashed` said it here, and this is the only place that reading
    // is consumed. Applied after the filters rather than inside
    // `criteria_condition`, because that function is shared with the `UPDATE`
    // and `DELETE` builders — and scoping a write is what would leave a purge
    // unable to purge and a bulk restore matching nothing. See
    // [`rainier_orm::trash`] for why the policy is reads-only.
    scope_select::<E>(stmt, criteria.trash_scope());

    for (column, descending) in criteria.orders() {
        let order = if *descending { Order::Desc } else { Order::Asc };
        stmt.order_by(column_ref::<E>(column), order);
    }

    if with_paging {
        if let Some(limit) = criteria.limit_value() {
            stmt.limit(limit);
        }
        if let Some(offset) = criteria.offset_value() {
            stmt.offset(offset);
        }
    }

    route
}

/// `INSERT INTO table (…) VALUES (…)`.
///
/// `assigned_id` is a shard-encoded primary key minted by the connector, for
/// the sharded data tier where the key cannot be auto-increment.
pub fn insert<E: Entity>(dialect: Dialect, entity: &E, assigned_id: Option<u64>) -> Prepared {
    let mut pairs = entity.insert_values();
    if let Some(id) = assigned_id {
        let pk = E::primary_key();
        for (column, value) in pairs.iter_mut() {
            if *column == pk {
                *value = id.into();
            }
        }
    }

    let route = route_from_pairs::<E>(&pairs);
    let mut stmt = SqQuery::insert();
    stmt.into_table(alias(E::table()));
    stmt.columns(pairs.iter().map(|(column, _)| alias(column)));
    stmt.values_panic(pairs.into_iter().map(|(_, value)| SimpleExpr::from(Expr::val(value))));

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// Whether `E` needs a connector-minted primary key for this row, and the
/// shard key to mint it from.
///
/// Mirrors Rainier ORM: the primary key must itself be a shard key, must not be
/// auto-increment, and must currently be unset.
pub fn shard_key_for_insert<E: Entity>(entity: &E) -> Option<u64> {
    // A minted id is the whole key, so this only applies to a one-column key;
    // there is nowhere to put an allocated id in a composite one.
    if E::primary_key_columns().len() != 1 {
        return None;
    }
    let pk = E::primary_key();
    if !E::shard_columns().contains(&pk) {
        return None;
    }
    if E::columns().iter().find(|c| c.name == pk)?.auto_increment {
        return None;
    }

    let pairs = entity.insert_values();
    let pk_value = pairs.iter().find(|(column, _)| *column == pk).map(|(_, value)| value)?;
    if routing_key(pk_value) != Some(0) {
        return None; // already set
    }

    pairs
        .iter()
        .find(|(column, value)| {
            *column != pk && E::shard_columns().contains(column) && routing_key(value).is_some()
        })
        .and_then(|(_, value)| routing_key(value))
}

/// `INSERT … ON CONFLICT (…) DO UPDATE`.
pub fn upsert<E: Entity>(
    dialect: Dialect,
    entity: &E,
    conflict_columns: &[&str],
    update_columns: &[&str],
) -> Prepared {
    let pairs = entity.insert_values();
    let route = route_from_pairs::<E>(&pairs);

    let mut stmt = SqQuery::insert();
    stmt.into_table(alias(E::table()));
    stmt.columns(pairs.iter().map(|(column, _)| alias(column)));
    stmt.values_panic(pairs.into_iter().map(|(_, value)| SimpleExpr::from(Expr::val(value))));

    let mut on_conflict = OnConflict::columns(conflict_columns.iter().map(|c| alias(c)));
    if !update_columns.is_empty() {
        on_conflict.update_columns(update_columns.iter().map(|c| alias(c)));
    } else if let Some(first) = conflict_columns.first() {
        // Insert-or-ignore. MySQL has no portable `DO NOTHING`, so a no-op
        // self-update is used instead — valid everywhere, changes nothing.
        on_conflict.value(alias(first), Expr::col(alias(first)));
    } else {
        on_conflict.do_nothing();
    }
    stmt.on_conflict(on_conflict);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `INSERT … ON CONFLICT (…) DO UPDATE` from an [`Upsert`] plan.
///
/// The general form of [`upsert`]: the plan can say a column *accumulates*
/// (`n = n + <incoming>`) rather than being overwritten, which is the only
/// single-statement way to keep a counter. Read-then-write loses increments
/// under concurrency and reports nothing — the total is merely too low.
///
/// Rendering is Rainier ORM's, so this routes and renders identically to
/// [`rainier_orm::repo::upsert_with`]; see [`Upsert`] for the dialect
/// differences and for why the conflict columns are required.
///
/// # Errors
///
/// If the plan cannot be rendered — see [`Upsert::to_on_conflict`].
pub fn upsert_with<E: Entity>(dialect: Dialect, entity: &E, plan: &Upsert) -> Result<Prepared> {
    let pairs = entity.insert_values();
    let route = route_from_pairs::<E>(&pairs);
    let columns: Vec<&str> = pairs.iter().map(|(column, _)| *column).collect();

    // Before anything is built, so a rejected plan cannot half-render.
    let on_conflict = plan.to_on_conflict(dialect, E::table(), &columns)?;

    let mut stmt = SqQuery::insert();
    stmt.into_table(alias(E::table()));
    stmt.columns(pairs.iter().map(|(column, _)| alias(column)));
    stmt.values_panic(pairs.iter().map(|(_, value)| SimpleExpr::from(Expr::val(value.clone()))));
    stmt.on_conflict(on_conflict);

    let (sql, params) = dialect.build_query(&stmt);
    Ok(Prepared { sql, params: params.0, route })
}

/// `UPDATE table SET … WHERE pk = ?` — every non-key column.
///
/// The key comes off the entity, so a composite one is `AND`-ed together in full
/// and no caller can supply a partial one — see
/// [`rainier_orm::row_key_condition`] for why that makes this
/// infallible.
pub fn update<E: Entity>(dialect: Dialect, entity: &E) -> Prepared {
    let route = key_route::<E>(&entity.pk_values());

    let mut stmt = SqQuery::update();
    stmt.table(alias(E::table()));
    for (column, value) in entity.update_values() {
        stmt.value(alias(column), value);
    }
    stmt.cond_where(row_key_condition(entity));

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `UPDATE table SET … WHERE <criteria>` — only the named columns.
pub fn update_matching<E: Entity>(
    dialect: Dialect,
    criteria: &Criteria,
    set: Vec<(String, Value)>,
) -> Prepared {
    let set = set.into_iter().map(|(column, value)| (column, Assignment::Value(value))).collect();
    update_matching_with::<E>(dialect, criteria, set)
}

/// `UPDATE table SET … WHERE <criteria>`, where a column may be assigned a
/// **correlated subquery** or an **increment** rather than a value.
///
/// The general form of [`update_matching`], and the only single-statement way to
/// recompute a denormalised counter across a table:
///
/// ```
/// # use rainier_database::{Assignment, Criteria, Projection, Subquery};
/// # use rainier_orm::Dialect;
/// # #[derive(rainier_orm::Entity, Clone, Debug)]
/// # #[orm(table = "parents")]
/// # struct Parent {
/// #     #[orm(pk, auto_increment)]
/// #     id: u64,
/// #     children_count: i64,
/// # }
/// let recount = Assignment::Subquery(
///     Subquery::count("children").correlate("parent_id", "id").where_eq("approved", true),
/// );
///
/// let prepared = rainier_database::statement::update_matching_with::<Parent>(
///     Dialect::Sqlite,
///     &Criteria::new(),
///     vec![("children_count".to_string(), recount)],
/// );
/// assert!(prepared.sql.contains("SELECT COUNT(*)"), "{}", prepared.sql);
/// assert_eq!(prepared.params.len(), 1, "only `approved`; the correlation binds nothing");
/// ```
///
/// Why one statement rather than a loop over the counts: a `COUNT` of zero
/// produces no grouped row to drive an update, so a loop has to zero every
/// counter first and fill the non-zero ones back in — and between those two
/// steps every row in the table reads zero. The correlated subquery has no such
/// window, because a scalar `COUNT` over no rows *is* `0` and is written like
/// any other result.
///
/// [`Assignment::Increment`] covers the other half of the same problem: raising
/// a stored number by a step rather than recomputing it. `SET n = n + ?` is not
/// a value the caller can bind, because the new total is not known until the
/// stored one is — and computing it in the process loses additions under
/// concurrency, silently. It is not a subquery either: reading the table being
/// updated is MySQL error 1093.
///
/// ```
/// # use rainier_database::{Assignment, Criteria};
/// # use rainier_orm::Dialect;
/// # #[derive(rainier_orm::Entity, Clone, Debug)]
/// # #[orm(table = "parents")]
/// # struct Parent {
/// #     #[orm(pk, auto_increment)]
/// #     id: u64,
/// #     children_count: i64,
/// # }
/// let prepared = rainier_database::statement::update_matching_with::<Parent>(
///     Dialect::Sqlite,
///     &Criteria::new().where_eq("id", 3_u64),
///     // The amount is signed, so `Increment(-1)` is the decrement — one
///     // rendering rather than two.
///     vec![("children_count".to_string(), Assignment::Increment(1))],
/// );
/// assert!(prepared.sql.contains(r#""children_count" = "children_count" + ?"#), "{}", prepared.sql);
/// ```
///
/// A subquery assignment does not move the shard route, and cannot: it is
/// evaluated on whatever shard the outer rows are on, so — like a join — it
/// reaches only the inner rows that live there.
pub fn update_matching_with<E: Entity>(
    dialect: Dialect,
    criteria: &Criteria,
    set: Vec<(String, Assignment)>,
) -> Prepared {
    let mut stmt = SqQuery::update();
    stmt.table(alias(E::table()));
    // `SET` before `WHERE`, which is also the order the placeholders come out
    // in — a subquery in the `SET` binds its values ahead of the filter's.
    for (column, assignment) in set {
        match assignment {
            Assignment::Value(value) => stmt.value(alias(&column), value),
            // `n = n + ?`, with the amount bound like any other value. The
            // column reference stays unqualified: inside an `UPDATE` it can
            // only mean the row being written, and unlike the `excluded` half
            // of an upsert there is no second value it could be confused with.
            Assignment::Increment(amount) => {
                stmt.value(alias(&column), Expr::col(alias(&column)).add(amount))
            }
            Assignment::Subquery(subquery) => {
                stmt.value(alias(&column), subquery_scalar::<E>(dialect, &subquery))
            }
        };
    }

    let (condition, route) = criteria_condition::<E>(dialect, criteria);
    stmt.cond_where(condition);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `DELETE FROM table WHERE pk = ?`.
///
/// Bounded on [`SingleKey`]; see [`delete_by_keys`].
pub fn delete_by_pk<E: Entity + SingleKey>(dialect: Dialect, key: Value) -> Prepared {
    let route = route_for::<E>(E::primary_key(), &key);

    let mut stmt = SqQuery::delete();
    stmt.from_table(alias(E::table()));
    stmt.and_where(Expr::col(alias(E::primary_key())).eq(key));

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `DELETE FROM table WHERE a = ? AND b = ?` — the composite counterpart of
/// [`delete_by_pk`].
///
/// `keys` are positional against [`Entity::primary_key_columns`] and all are
/// required; a short list errors instead of deleting every row sharing the parts
/// that were given.
pub fn delete_by_keys<E: Entity>(dialect: Dialect, keys: &[Value]) -> Result<Prepared> {
    let route = key_route::<E>(keys);

    let mut stmt = SqQuery::delete();
    stmt.from_table(alias(E::table()));
    stmt.cond_where(key_condition::<E>(keys)?);

    let (sql, params) = dialect.build_query(&stmt);
    Ok(Prepared { sql, params: params.0, route })
}

/// `DELETE FROM table WHERE <criteria>`.
pub fn delete_matching<E: Entity>(dialect: Dialect, criteria: &Criteria) -> Prepared {
    let mut stmt = SqQuery::delete();
    stmt.from_table(alias(E::table()));

    let (condition, route) = criteria_condition::<E>(dialect, criteria);
    stmt.cond_where(condition);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
        title: String,
        published: bool,
    }

    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "tokens")]
    struct Token {
        #[orm(pk, shard_key)]
        id: u64,
        #[orm(shard_key)]
        user_id: u64,
        hash: String,
    }

    fn post() -> Post {
        Post { id: 0, title: "Hello".into(), published: true }
    }

    #[test]
    fn a_prepared_statement_is_send() {
        // The whole reason this module exists. If `Prepared` ever stopped
        // being `Send`, every handler that touches the database would stop
        // compiling with a much more confusing error than this.
        fn assert_send<T: Send>() {}
        assert_send::<Prepared>();
    }

    #[test]
    fn select_all_lists_every_column() {
        let prepared = select_all::<Post>(Dialect::Sqlite);
        assert!(prepared.sql.contains("\"posts\""), "{}", prepared.sql);
        assert!(prepared.sql.contains("\"title\""), "{}", prepared.sql);
        assert!(prepared.params.is_empty());
        assert_eq!(prepared.route, ShardRoute::Global);
    }

    #[test]
    fn select_by_pk_binds_and_limits() {
        let prepared = select_by_pk::<Post>(Dialect::Sqlite, 7_i64.into());
        assert!(prepared.sql.contains("WHERE"), "{}", prepared.sql);
        assert!(prepared.sql.contains("LIMIT"), "{}", prepared.sql);
        // The key and the limit: sea-query parameterises `LIMIT` too, rather
        // than inlining it as a literal.
        assert_eq!(prepared.params.len(), 2);
        assert_eq!(prepared.params[0], 7_i64.into());
    }

    #[test]
    fn criteria_become_predicates_ordering_and_paging() {
        let criteria = Criteria::new()
            .where_eq("published", true)
            .where_gt("id", 10_i64)
            .order_by_desc("id")
            .limit(5)
            .offset(10);

        let prepared = select_matching::<Post>(Dialect::Sqlite, &criteria);
        assert!(prepared.sql.contains("ORDER BY"), "{}", prepared.sql);
        assert!(prepared.sql.contains("LIMIT"), "{}", prepared.sql);
        assert!(prepared.sql.contains("OFFSET"), "{}", prepared.sql);
        // Two predicates plus the limit and the offset, all parameterised.
        assert_eq!(prepared.params.len(), 4);
    }

    #[test]
    fn counting_drops_the_paging_but_keeps_the_filters() {
        let criteria = Criteria::new().where_eq("published", true).limit(5).offset(10);
        let prepared = count_matching::<Post>(Dialect::Sqlite, &criteria);

        assert!(prepared.sql.contains("COUNT"), "{}", prepared.sql);
        assert!(prepared.sql.contains("cnt"), "{}", prepared.sql);
        assert!(!prepared.sql.contains("LIMIT"), "{}", prepared.sql);
        assert!(!prepared.sql.contains("ORDER BY"), "{}", prepared.sql);
        assert_eq!(prepared.params.len(), 1);
    }

    #[test]
    fn insert_omits_an_auto_increment_key() {
        let prepared = insert::<Post>(Dialect::Sqlite, &post(), None);
        assert!(prepared.sql.starts_with("INSERT INTO"), "{}", prepared.sql);
        assert!(!prepared.sql.contains("\"id\""), "auto-increment keys are the DB's job");
        assert_eq!(prepared.params.len(), 2);
    }

    #[test]
    fn update_writes_every_column_but_the_key() {
        let prepared = update::<Post>(Dialect::Sqlite, &Post { id: 3, ..post() });
        assert!(prepared.sql.starts_with("UPDATE"), "{}", prepared.sql);
        assert!(prepared.sql.contains("WHERE"), "{}", prepared.sql);
        // title + published, then the key in the WHERE.
        assert_eq!(prepared.params.len(), 3);
    }

    #[test]
    fn upsert_renders_a_conflict_clause() {
        let prepared = upsert::<Post>(Dialect::Sqlite, &post(), &["title"], &["published"]);
        assert!(prepared.sql.contains("ON CONFLICT"), "{}", prepared.sql);
    }

    #[test]
    fn an_upsert_with_no_updates_is_insert_or_ignore() {
        let prepared = upsert::<Post>(Dialect::Sqlite, &post(), &["title"], &[]);
        // A no-op self-update rather than DO NOTHING, which MySQL cannot render.
        assert!(prepared.sql.contains("ON CONFLICT"), "{}", prepared.sql);
        assert!(!prepared.sql.contains("DO NOTHING"), "{}", prepared.sql);
    }

    #[test]
    fn an_upsert_plan_renders_an_increment_per_dialect() {
        let plan = Upsert::on(["title"]).increment(["published"]);

        // MySQL infers the conflicting key and reads the incoming row through
        // `VALUES()`; the other two name the target and read `excluded`.
        assert!(upsert_with::<Post>(Dialect::MySql, &post(), &plan)
            .unwrap()
            .sql
            .ends_with("ON DUPLICATE KEY UPDATE `published` = `published` + VALUES(`published`)"));

        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            let sql = upsert_with::<Post>(dialect, &post(), &plan).unwrap().sql;
            assert!(
                sql.ends_with(
                    r#"ON CONFLICT ("title") DO UPDATE SET "published" = "posts"."published" + "excluded"."published""#
                ),
                "{dialect:?}: {sql}"
            );
        }
    }

    #[test]
    fn an_upsert_plan_binds_its_values_rather_than_inlining_them() {
        // The injection this layer exists to make unwritable: a value pasted
        // into the statement instead of bound.
        let post = Post { id: 0, title: "'); DROP TABLE posts; --".into(), published: true };
        let prepared =
            upsert_with::<Post>(Dialect::Sqlite, &post, &Upsert::on(["title"]).increment(["id"]))
                .unwrap_err();
        // `id` is auto-increment, so it is not in the insert and the plan is
        // refused before anything renders — which is the other half of the same
        // guarantee.
        assert!(prepared.to_string().contains("id"), "{prepared}");

        let prepared = upsert_with::<Post>(
            Dialect::Sqlite,
            &post,
            &Upsert::on(["title"]).replace(["published"]),
        )
        .unwrap();
        assert!(!prepared.sql.contains("DROP TABLE"), "{}", prepared.sql);
        assert_eq!(prepared.params.len(), 2, "title and published, both bound");
    }

    #[test]
    fn an_upsert_plan_with_no_conflict_columns_is_refused() {
        // Renders on MySQL, syntax error on SQLite and Postgres — so it must
        // not render at all.
        let plan = Upsert::on(Vec::<String>::new()).replace(["published"]);
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            assert!(upsert_with::<Post>(dialect, &post(), &plan).is_err(), "{dialect:?}");
        }
    }

    #[test]
    fn delete_targets_the_key() {
        let prepared = delete_by_pk::<Post>(Dialect::Sqlite, 3_i64.into());
        assert!(prepared.sql.starts_with("DELETE FROM"), "{}", prepared.sql);
        assert_eq!(prepared.params.len(), 1);
    }

    #[test]
    fn every_dialect_renders() {
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let prepared = select_by_pk::<Post>(dialect, 1_i64.into());
            assert!(prepared.sql.contains("posts"), "{dialect:?}: {}", prepared.sql);
        }
        // Postgres uses numbered placeholders, MySQL/SQLite use `?`.
        assert!(select_by_pk::<Post>(Dialect::Postgres, 1_i64.into()).sql.contains("$1"));
    }

    // --- routing -----------------------------------------------------------

    #[test]
    fn a_global_entity_always_routes_globally() {
        assert_eq!(route_for::<Post>("id", &7_i64.into()), ShardRoute::Global);
        assert_eq!(select_by_pk::<Post>(Dialect::Sqlite, 7_i64.into()).route, ShardRoute::Global);
    }

    #[test]
    fn a_shard_key_column_routes_by_its_value() {
        assert_eq!(route_for::<Token>("id", &7_u64.into()), ShardRoute::Key(7));
        assert_eq!(route_for::<Token>("user_id", &9_u64.into()), ShardRoute::Key(9));
        assert_eq!(
            route_for::<Token>("hash", &"abc".into()),
            ShardRoute::Global,
            "a non-shard column never routes"
        );
    }

    #[test]
    fn a_criteria_equality_on_a_shard_column_pins_the_query() {
        let criteria = Criteria::new().where_eq("user_id", 42_u64);
        assert_eq!(select_matching::<Token>(Dialect::Sqlite, &criteria).route, ShardRoute::Key(42));
    }

    #[test]
    fn string_routing_keys_hash_stably() {
        // Must match Rainier ORM's `stable_hash`, not a randomised hasher, or the
        // same key would land on a different shard in a different process.
        let expected = rainier_orm::stable_hash(b"user@example.com");
        assert_eq!(routing_key(&"user@example.com".into()), Some(expected));
    }

    #[test]
    fn a_null_or_unroutable_value_has_no_key() {
        assert_eq!(routing_key(&Value::BigInt(None)), None);
        assert_eq!(routing_key(&Value::Bool(Some(true))), None);
    }

    #[test]
    fn an_insert_routes_by_the_rows_shard_key() {
        let token = Token { id: 100, user_id: 42, hash: "x".into() };
        assert_eq!(insert::<Token>(Dialect::Sqlite, &token, None).route, ShardRoute::Key(100));
    }

    #[test]
    fn an_unset_shard_encoded_key_asks_the_connector_to_mint_one() {
        let token = Token { id: 0, user_id: 42, hash: "x".into() };
        assert_eq!(shard_key_for_insert::<Token>(&token), Some(42));

        // Already set: the connector is not consulted.
        let assigned = Token { id: 100, user_id: 42, hash: "x".into() };
        assert_eq!(shard_key_for_insert::<Token>(&assigned), None);

        // A global entity never allocates.
        assert_eq!(shard_key_for_insert::<Post>(&post()), None);
    }

    #[test]
    fn a_minted_id_is_written_into_the_insert() {
        let token = Token { id: 0, user_id: 42, hash: "x".into() };
        let prepared = insert::<Token>(Dialect::Sqlite, &token, Some(4242));
        assert_eq!(prepared.route, ShardRoute::Key(4242));
    }

    // --- composite keys ----------------------------------------------------

    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "memberships")]
    struct Membership {
        #[orm(pk)]
        team_id: u64,
        #[orm(pk)]
        user_id: u64,
        role: String,
    }

    /// A composite key routed by its first half — a `(user_id, slot)` table on
    /// the sharded tier, where only one part of the key names a shard.
    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "slots")]
    struct Slot {
        #[orm(pk, shard_key)]
        user_id: u64,
        #[orm(pk)]
        slot: i64,
        payload: String,
    }

    fn membership() -> Membership {
        Membership { team_id: 7, user_id: 9, role: "owner".into() }
    }

    #[test]
    fn a_composite_update_constrains_every_key_column() {
        let prepared = update::<Membership>(Dialect::Sqlite, &membership());

        assert!(prepared.sql.contains("\"team_id\""), "{}", prepared.sql);
        assert!(prepared.sql.contains("\"user_id\""), "{}", prepared.sql);
        assert!(prepared.sql.contains("AND"), "both parts, not either: {}", prepared.sql);
        // `role`, then both halves of the key in the WHERE.
        assert_eq!(prepared.params.len(), 3, "{:?}", prepared.params);
    }

    #[test]
    fn a_composite_select_and_delete_constrain_every_key_column() {
        let keys = [7_u64.into(), 9_u64.into()];

        for (name, sql) in [
            ("select", select_by_keys::<Membership>(Dialect::Sqlite, &keys).unwrap().sql),
            ("delete", delete_by_keys::<Membership>(Dialect::Sqlite, &keys).unwrap().sql),
        ] {
            assert!(sql.contains("\"team_id\""), "{name}: {sql}");
            assert!(sql.contains("\"user_id\""), "{name}: {sql}");
            assert!(sql.contains("AND"), "{name}: both parts, not either: {sql}");
        }
    }

    #[test]
    fn a_partial_key_is_refused_rather_than_prepared() {
        // The dangerous shape: one value for a two-column key would render
        // `DELETE FROM memberships WHERE team_id = ?` — the whole team.
        assert!(delete_by_keys::<Membership>(Dialect::Sqlite, &[7_u64.into()]).is_err());
        assert!(select_by_keys::<Membership>(Dialect::Sqlite, &[7_u64.into()]).is_err());
    }

    #[test]
    fn a_composite_key_renders_on_every_dialect() {
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let prepared = update::<Membership>(dialect, &membership());
            assert!(prepared.sql.contains("memberships"), "{dialect:?}: {}", prepared.sql);
            assert!(prepared.sql.contains("AND"), "{dialect:?}: {}", prepared.sql);
            assert_eq!(prepared.params.len(), 3, "{dialect:?}: {:?}", prepared.params);
        }
    }

    #[test]
    fn a_composite_key_routes_by_whichever_part_is_shard_encoded() {
        // `slot` is not a shard column, so the scan has to run past the first
        // key part rather than assuming it decides.
        let slot = Slot { user_id: 42, slot: 3, payload: "x".into() };
        assert_eq!(update::<Slot>(Dialect::Sqlite, &slot).route, ShardRoute::Key(42));

        let keys = [42_u64.into(), 3_i64.into()];
        assert_eq!(
            delete_by_keys::<Slot>(Dialect::Sqlite, &keys).unwrap().route,
            ShardRoute::Key(42)
        );

        // A composite key with no shard column at all stays global.
        assert_eq!(update::<Membership>(Dialect::Sqlite, &membership()).route, ShardRoute::Global);
    }

    #[test]
    fn a_composite_conflict_target_carries_every_column() {
        // A counter keyed on a pair is the main reason to want an upsert with
        // an increment, so the target has to survive as a pair. Half of it
        // names a constraint that does not exist, which SQLite and Postgres
        // reject — but only at runtime, on whichever row first collides.
        let plan = Upsert::on(["team_id", "user_id"]).increment(["role"]);

        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            let sql = upsert_with::<Membership>(dialect, &membership(), &plan).unwrap().sql;
            assert!(sql.contains(r#"ON CONFLICT ("team_id", "user_id")"#), "{dialect:?}: {sql}");
        }

        // MySQL infers the key, so the pair is carried by the constraint rather
        // than by the statement — and emitting a target there would be a syntax
        // error, so its absence is the correct rendering, not a dropped column.
        let mysql = upsert_with::<Membership>(Dialect::MySql, &membership(), &plan).unwrap().sql;
        assert!(!mysql.contains("ON CONFLICT"), "{mysql}");
        assert!(mysql.contains("ON DUPLICATE KEY UPDATE"), "{mysql}");
    }

    #[test]
    fn a_composite_key_upsert_routes_like_the_rest_of_the_layer() {
        // The plan must not lose the shard route the row's own values imply, or
        // an upsert would land on a different shard than the `SELECT` that
        // reads it back.
        let slot = Slot { user_id: 42, slot: 3, payload: "x".into() };
        let plan = Upsert::on(["user_id", "slot"]).replace(["payload"]);

        assert_eq!(
            upsert_with::<Slot>(Dialect::Sqlite, &slot, &plan).unwrap().route,
            ShardRoute::Key(42)
        );
    }

    #[test]
    fn a_composite_key_never_asks_the_connector_to_mint_an_id() {
        // There is nowhere to put an allocated id in a two-column key, and
        // writing it into the first half would leave the second unset.
        let slot = Slot { user_id: 0, slot: 3, payload: "x".into() };
        assert_eq!(shard_key_for_insert::<Slot>(&slot), None);
    }

    #[test]
    fn a_single_key_update_is_unchanged() {
        // The control: one equality, no conjunction, the same parameter count
        // this module has always produced.
        let prepared = update::<Post>(Dialect::Sqlite, &Post { id: 3, ..post() });
        assert_eq!(
            prepared.sql,
            r#"UPDATE "posts" SET "title" = ?, "published" = ? WHERE "id" = ?"#
        );
        assert_eq!(prepared.params.len(), 3);
    }

    // --- correlated subqueries ---------------------------------------------

    /// A parent whose counter is denormalised — the shape a correlated
    /// `UPDATE … SET` exists to recompute.
    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "parents")]
    struct Parent {
        #[orm(pk, auto_increment)]
        id: u64,
        children_count: i64,
    }

    /// Self-referential, so the inner and outer table are the same name — the
    /// case an alias has to keep apart.
    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "nodes")]
    struct Node {
        #[orm(pk, auto_increment)]
        id: u64,
        parent_id: u64,
    }

    fn children() -> Subquery {
        Subquery::count("children").correlate("parent_id", "id")
    }

    #[test]
    fn an_exists_renders_its_correlation_on_every_dialect() {
        let criteria = Criteria::new().where_exists(children());

        assert_eq!(
            select_matching::<Parent>(Dialect::Sqlite, &criteria).sql,
            concat!(
                r#"SELECT "parents"."id", "parents"."children_count" FROM "parents" WHERE "#,
                r#"EXISTS(SELECT 1 FROM "children" AS "_rainier_sub" "#,
                r#"WHERE "_rainier_sub"."parent_id" = "parents"."id")"#,
            )
        );
        assert_eq!(
            select_matching::<Parent>(Dialect::MySql, &criteria).sql,
            concat!(
                "SELECT `parents`.`id`, `parents`.`children_count` FROM `parents` WHERE ",
                "EXISTS(SELECT 1 FROM `children` AS `_rainier_sub` ",
                "WHERE `_rainier_sub`.`parent_id` = `parents`.`id`)",
            )
        );
        assert_eq!(
            select_matching::<Parent>(Dialect::Postgres, &criteria).sql,
            concat!(
                r#"SELECT "parents"."id", "parents"."children_count" FROM "parents" WHERE "#,
                r#"EXISTS(SELECT 1 FROM "children" AS "_rainier_sub" "#,
                r#"WHERE "_rainier_sub"."parent_id" = "parents"."id")"#,
            )
        );
    }

    #[test]
    fn the_correlation_names_two_different_scopes() {
        // The single assertion the whole feature rests on. Both sides of
        // `_rainier_sub.parent_id = parents.id` are columns, and they are
        // qualified to *different* tables — an inner-only predicate would read
        // `"children"."parent_id" = "children"."id"` and match every outer row.
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let sql =
                select_matching::<Parent>(dialect, &Criteria::new().where_exists(children())).sql;

            let inner = sql.find("_rainier_sub").expect("the inner scope");
            let outer = sql.rfind("parents").expect("the outer scope");
            assert!(inner < outer, "{dialect:?}: inner = outer, not inner = value: {sql}");
        }
    }

    #[test]
    fn an_exists_ignores_the_projection_and_selects_a_constant() {
        // `EXISTS (SELECT COUNT(*) …)` is true for *every* row — an aggregate
        // with no GROUP BY always returns one row, even counting nothing. So a
        // counting subquery in existence position must not render its count.
        let sql = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_exists(Subquery::count("children").correlate("parent_id", "id")),
        )
        .sql;

        assert!(sql.contains("EXISTS(SELECT 1 FROM"), "{sql}");
        assert!(!sql.contains("COUNT"), "a count inside EXISTS matches everything: {sql}");
    }

    #[test]
    fn a_constant_projection_costs_no_parameter() {
        // `Expr::val(1)` would bind, pushing a meaningless value into the list
        // ahead of every parameter that follows it.
        let prepared =
            select_matching::<Parent>(Dialect::Sqlite, &Criteria::new().where_exists(children()));
        assert!(prepared.params.is_empty(), "{:?}", prepared.params);
    }

    #[test]
    fn a_not_exists_negates_the_same_subquery() {
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let sql =
                select_matching::<Parent>(dialect, &Criteria::new().where_not_exists(children()))
                    .sql;
            assert!(sql.contains("NOT EXISTS(SELECT 1 FROM"), "{dialect:?}: {sql}");
        }
    }

    #[test]
    fn a_scalar_subquery_compares_against_a_bound_value() {
        let criteria = Criteria::new().where_subquery(children(), Comparison::Eq, 2_i64);

        assert_eq!(
            select_matching::<Parent>(Dialect::Sqlite, &criteria).sql,
            concat!(
                r#"SELECT "parents"."id", "parents"."children_count" FROM "parents" WHERE "#,
                r#"(SELECT COUNT(*) FROM "children" AS "_rainier_sub" "#,
                r#"WHERE "_rainier_sub"."parent_id" = "parents"."id") = ?"#,
            )
        );
        // Postgres numbers the placeholder rather than repeating `?`, and the
        // subquery must not throw the numbering off.
        assert_eq!(
            select_matching::<Parent>(Dialect::Postgres, &criteria).sql,
            concat!(
                r#"SELECT "parents"."id", "parents"."children_count" FROM "parents" WHERE "#,
                r#"(SELECT COUNT(*) FROM "children" AS "_rainier_sub" "#,
                r#"WHERE "_rainier_sub"."parent_id" = "parents"."id") = $1"#,
            )
        );
        assert_eq!(
            select_matching::<Parent>(Dialect::MySql, &criteria).params,
            vec![Value::from(2_i64)]
        );
    }

    #[test]
    fn every_comparison_renders_its_operator() {
        for (comparison, operator) in [
            (Comparison::Eq, "= ?"),
            (Comparison::Ne, "<> ?"),
            (Comparison::Gt, "> ?"),
            (Comparison::Gte, ">= ?"),
            (Comparison::Lt, "< ?"),
            (Comparison::Lte, "<= ?"),
        ] {
            let sql = select_matching::<Parent>(
                Dialect::Sqlite,
                &Criteria::new().where_subquery(children(), comparison, 2_i64),
            )
            .sql;
            assert!(sql.ends_with(operator), "{comparison:?}: {sql}");
        }
    }

    #[test]
    fn a_subquerys_parameters_interleave_with_the_outer_querys_in_sql_order() {
        // The failure this catches is silent and total: the values are all
        // present and all bound, but one lands in another's placeholder, so the
        // query runs and answers about the wrong rows.
        let criteria = Criteria::new()
            .where_eq("children_count", 1_i64)
            .where_exists(children().where_eq("kind", "a"))
            .where_gt("id", 10_u64)
            .where_subquery(children().where_eq("kind", "b"), Comparison::Gte, 3_i64)
            .where_ne("children_count", 99_i64)
            .limit(5);

        let prepared = select_matching::<Parent>(Dialect::Sqlite, &criteria);

        // The two subquery predicates render *after* the plain constraints, so
        // their values do too — and the inner `kind` sits inside its own
        // subquery, before that subquery's own comparison value.
        assert_eq!(
            prepared.params,
            vec![
                Value::from(1_i64),  // children_count = ?
                Value::from(10_u64), // id > ?
                Value::from(99_i64), // children_count <> ?
                Value::from("a"),    // EXISTS (… kind = ?)
                Value::from("b"),    // (SELECT … kind = ?)
                Value::from(3_i64),  // … ) >= ?
                Value::from(5_u64),  // LIMIT ?
            ],
            "{}",
            prepared.sql
        );
        assert_eq!(prepared.sql.matches('?').count(), prepared.params.len());
    }

    #[test]
    fn a_subquery_binds_its_values_rather_than_inlining_them() {
        // The injection this layer exists to make unwritable. A subquery
        // assembled by concatenation is the single most dangerous thing here,
        // because it is the one place a caller's string would sit next to
        // structural SQL.
        let hostile = "'); DROP TABLE parents; --";
        let prepared = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_exists(children().where_eq("kind", hostile)),
        );

        assert!(!prepared.sql.contains("DROP TABLE"), "{}", prepared.sql);
        assert_eq!(prepared.params, vec![Value::from(hostile)]);
    }

    #[test]
    fn a_correlation_can_name_a_joined_outer_table() {
        // `"table.column"` on the outer side, read exactly as everywhere else
        // in this layer — so a subquery can correlate to something the outer
        // query joined rather than only to the model's own table.
        let sql = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new()
                .join("owners", "id", "parent_id")
                .where_exists(Subquery::count("audits").correlate("owner_id", "owners.id")),
        )
        .sql;

        assert!(sql.contains(r#""_rainier_sub"."owner_id" = "owners"."id""#), "{sql}");
    }

    #[test]
    fn a_self_correlated_subquery_keeps_the_two_scopes_apart() {
        // The alias earns its keep here. Without it both sides would render as
        // `"nodes"."…"`, the predicate would stop mentioning the outer row, and
        // the `EXISTS` would collapse to a constant — true for every node, or
        // false for every node, with no error either way.
        let sql = select_matching::<Node>(
            Dialect::Sqlite,
            &Criteria::new().where_exists(Subquery::count("nodes").correlate("parent_id", "id")),
        )
        .sql;

        assert!(sql.contains(r#"FROM "nodes" AS "_rainier_sub""#), "{sql}");
        assert!(sql.contains(r#""_rainier_sub"."parent_id" = "nodes"."id""#), "{sql}");
    }

    #[test]
    fn a_subquery_can_carry_more_than_one_correlation() {
        // A composite foreign key. Matching on half of it over-matches exactly
        // the way no correlation at all does, only less obviously.
        let sql = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_exists(
                Subquery::count("children").correlate("parent_id", "id").correlate("tenant", "id"),
            ),
        )
        .sql;

        assert!(sql.contains(r#""_rainier_sub"."parent_id" = "parents"."id""#), "{sql}");
        assert!(sql.contains(r#""_rainier_sub"."tenant" = "parents"."id""#), "{sql}");
    }

    #[test]
    fn a_subquerys_own_projection_resolves_against_its_own_table() {
        // Not the outer one — a `SUM` inside the subquery reads the inner
        // table's column, and qualifying it to the outer table would either
        // error or, worse, silently sum the wrong column of the wrong row.
        let sql = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_subquery(
                Subquery::select("children", Projection::Sum("weight".into()))
                    .correlate("parent_id", "id"),
                Comparison::Gt,
                10_i64,
            ),
        )
        .sql;

        assert!(sql.contains(r#"SUM("_rainier_sub"."weight")"#), "{sql}");
    }

    #[test]
    fn a_dialect_specific_projection_inside_a_subquery_is_still_per_dialect() {
        // The refactor's real risk: a second copy of the date-part branches
        // that keeps a `MONTH()` SQLite cannot run.
        let criteria = |dialect| {
            select_matching::<Parent>(
                dialect,
                &Criteria::new().where_subquery(
                    Subquery::select(
                        "children",
                        Projection::DatePart(DatePart::Month, "born_at".into()),
                    )
                    .correlate("parent_id", "id"),
                    Comparison::Eq,
                    3_i64,
                ),
            )
            .sql
        };

        assert!(criteria(Dialect::Sqlite).contains("strftime"), "{}", criteria(Dialect::Sqlite));
        assert!(criteria(Dialect::MySql).contains("MONTH("), "{}", criteria(Dialect::MySql));
        assert!(
            criteria(Dialect::Postgres).contains("date_part"),
            "{}",
            criteria(Dialect::Postgres)
        );
    }

    #[test]
    fn a_delete_filters_by_subquery_too() {
        // The condition is built once for every statement kind, so a `DELETE`
        // scoped by an `EXISTS` cannot quietly drop the predicate and remove
        // the whole table.
        let sql = delete_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_not_exists(children()),
        )
        .sql;
        assert!(sql.starts_with(r#"DELETE FROM "parents" WHERE"#), "{sql}");
        assert!(sql.contains("NOT EXISTS(SELECT 1 FROM"), "{sql}");
    }

    #[test]
    fn a_subquery_never_moves_the_shard_route() {
        // It cannot name a shard, and guessing would send the statement where
        // the outer rows are not.
        let pinned = Criteria::new()
            .where_eq("user_id", 42_u64)
            .where_exists(Subquery::count("audits").correlate("token_id", "id"));
        assert_eq!(
            select_matching::<Token>(Dialect::Sqlite, &pinned).route,
            ShardRoute::Key(42),
            "the equality still pins it"
        );

        let unpinned =
            Criteria::new().where_exists(Subquery::count("audits").correlate("token_id", "id"));
        assert_eq!(select_matching::<Token>(Dialect::Sqlite, &unpinned).route, ShardRoute::Global);
    }

    // --- OR groups ----------------------------------------------------------

    #[test]
    fn an_or_group_renders_its_subquery_branch_beside_its_columns() {
        // A mixed group. Losing the `EXISTS` branch here narrows the result:
        // rows that matched only through the subquery stop coming back, and the
        // SQL that remains is perfectly valid.
        let sql = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new()
                .where_eq("id", 1_u64)
                .or_where(|any| any.where_gt("children_count", 0_i64).where_exists(children())),
        )
        .sql;

        assert!(sql.contains(r#""parents"."id" = ?"#), "{sql}");
        assert!(
            sql.contains(r#"("parents"."children_count" > ? OR EXISTS(SELECT 1 FROM"#),
            "the subquery has to be a branch of the OR, not a separate AND: {sql}"
        );
    }

    #[test]
    fn a_group_of_only_a_subquery_still_renders_a_group() {
        // The silent over-match, at the layer that would have produced it. An
        // empty group is skipped, so a group whose only member was dropped
        // becomes no `WHERE` clause at all — and the statement returns every
        // row instead of the filtered ones.
        let prepared = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().or_where(|any| any.where_not_exists(children())),
        );

        assert!(prepared.sql.contains("NOT EXISTS(SELECT 1 FROM"), "{}", prepared.sql);
        assert!(
            !prepared.sql.ends_with("WHERE TRUE"),
            "an unfiltered SELECT is the failure this guards: {}",
            prepared.sql
        );
    }

    #[test]
    fn an_or_groups_subquery_binds_its_own_values_in_order() {
        // Two branches, two binds, and the group's values sit between the
        // top-level predicate's and the paging — so a dropped branch shows up
        // as a missing parameter rather than as a wrong answer.
        let prepared = select_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_eq("id", 1_u64).or_where(|any| {
                any.where_gt("children_count", 5_i64)
                    .where_exists(children().where_eq("approved", true))
            }),
        );

        assert_eq!(
            prepared.params,
            vec![Value::from(1_u64), Value::from(5_i64), Value::from(true)],
            "{}",
            prepared.sql
        );
    }

    #[test]
    fn an_or_groups_subquery_does_not_move_the_shard_route() {
        // Same rule as an `AND`-ed one, and more so: a branch of an `OR` need
        // not hold for a matching row at all, so pinning a shard from inside one
        // would send the statement away from rows that match the other branch.
        let criteria = Criteria::new().where_eq("user_id", 42_u64).or_where(|any| {
            any.where_exists(Subquery::count("audits").correlate("token_id", "id"))
        });

        assert_eq!(
            select_matching::<Token>(Dialect::Sqlite, &criteria).route,
            ShardRoute::Key(42),
            "the top-level equality still pins it"
        );
    }

    // --- UPDATE … SET <subquery> -------------------------------------------

    fn recount() -> Vec<(String, Assignment)> {
        vec![(
            "children_count".to_string(),
            Assignment::Subquery(children().where_eq("approved", true)),
        )]
    }

    #[test]
    fn an_update_can_assign_a_correlated_subquery_on_every_dialect() {
        // The bulk recompute in full, with no outer filter — every row, one
        // statement. The trailing `WHERE TRUE` is what an empty criteria has
        // always rendered here, and it is what "every row" means.
        assert_eq!(
            update_matching_with::<Parent>(Dialect::Sqlite, &Criteria::new(), recount()).sql,
            concat!(
                r#"UPDATE "parents" SET "children_count" = "#,
                r#"(SELECT COUNT(*) FROM "children" AS "_rainier_sub" "#,
                r#"WHERE "_rainier_sub"."parent_id" = "parents"."id" "#,
                r#"AND "_rainier_sub"."approved" = ?) WHERE TRUE"#,
            )
        );
        assert_eq!(
            update_matching_with::<Parent>(Dialect::MySql, &Criteria::new(), recount()).sql,
            concat!(
                "UPDATE `parents` SET `children_count` = ",
                "(SELECT COUNT(*) FROM `children` AS `_rainier_sub` ",
                "WHERE `_rainier_sub`.`parent_id` = `parents`.`id` ",
                "AND `_rainier_sub`.`approved` = ?) WHERE TRUE",
            )
        );
        assert_eq!(
            update_matching_with::<Parent>(Dialect::Postgres, &Criteria::new(), recount()).sql,
            concat!(
                r#"UPDATE "parents" SET "children_count" = "#,
                r#"(SELECT COUNT(*) FROM "children" AS "_rainier_sub" "#,
                r#"WHERE "_rainier_sub"."parent_id" = "parents"."id" "#,
                r#"AND "_rainier_sub"."approved" = $1) WHERE TRUE"#,
            )
        );
    }

    #[test]
    fn an_assigned_subquerys_parameters_come_before_the_filters() {
        // `SET` is rendered before `WHERE`, so its binds are too. Getting this
        // backwards swaps a filter value into the subquery and vice versa —
        // both are integers often enough for it to run and answer wrongly.
        let mut set = recount();
        set.push(("id".to_string(), Assignment::Value(7_u64.into())));

        let prepared = update_matching_with::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_gt("id", 100_u64),
            set,
        );

        assert_eq!(
            prepared.params,
            vec![
                Value::from(true),    // inside the assigned subquery
                Value::from(7_u64),   // the plain assignment beside it
                Value::from(100_u64), // the filter
            ],
            "{}",
            prepared.sql
        );
    }

    // --- UPDATE … SET n = n + ? --------------------------------------------

    #[test]
    fn an_increment_assignment_reads_the_column_it_writes_on_every_dialect() {
        // The shape no bound value can express, and the one an application
        // otherwise drops to raw SQL for — which then only runs on the dialect
        // it was written against.
        for (dialect, expected) in [
            (
                Dialect::Sqlite,
                r#"UPDATE "parents" SET "children_count" = "children_count" + ? WHERE TRUE"#
                    .to_string(),
            ),
            (
                Dialect::MySql,
                "UPDATE `parents` SET `children_count` = `children_count` + ? WHERE TRUE"
                    .to_string(),
            ),
            (
                Dialect::Postgres,
                r#"UPDATE "parents" SET "children_count" = "children_count" + $1 WHERE TRUE"#
                    .to_string(),
            ),
        ] {
            let prepared = update_matching_with::<Parent>(
                dialect,
                &Criteria::new(),
                vec![("children_count".to_string(), Assignment::Increment(1))],
            );

            assert_eq!(prepared.sql, expected, "{dialect:?}");
            assert_eq!(
                prepared.params,
                vec![Value::from(1_i64)],
                "{dialect:?}: the amount is bound, not pasted in"
            );
        }
    }

    #[test]
    fn an_increment_is_never_a_plain_assignment() {
        // The regression guard. `SET n = ?` renders, runs and reports the same
        // row count as `SET n = n + ?`; the only difference is that the stored
        // total is replaced by the step instead of raised by it. So the
        // assertion is that the column appears on *both* sides.
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let incremented = update_matching_with::<Parent>(
                dialect,
                &Criteria::new(),
                vec![("children_count".to_string(), Assignment::Increment(1))],
            )
            .sql;
            let assigned = update_matching_with::<Parent>(
                dialect,
                &Criteria::new(),
                vec![("children_count".to_string(), Assignment::Value(1_i64.into()))],
            )
            .sql;

            assert!(incremented.contains('+'), "{dialect:?} lost the addition: {incremented}");
            assert!(!assigned.contains('+'), "{dialect:?}: a value assignment must not add");
            assert_ne!(incremented, assigned, "{dialect:?}");
        }
    }

    #[test]
    fn a_negative_increment_is_the_decrement() {
        // Signed on purpose: one rendering, so there is one place for the sign
        // to be right, and no unsigned subtraction to wrap underneath.
        let prepared = update_matching_with::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_eq("id", 3_u64),
            vec![("children_count".to_string(), Assignment::Increment(-2))],
        );

        assert_eq!(
            prepared.sql,
            r#"UPDATE "parents" SET "children_count" = "children_count" + ? WHERE "parents"."id" = ?"#
        );
        assert_eq!(prepared.params, vec![Value::from(-2_i64), Value::from(3_u64)]);
    }

    #[test]
    fn an_increment_composes_with_the_other_assignments_in_one_statement() {
        // An increment beside a plain value, in one `UPDATE`. Two statements
        // would leave the row half-written in between, and the bind order is
        // what proves the increment's amount did not displace the filter's.
        let prepared = update_matching_with::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_gt("id", 100_u64),
            vec![
                ("children_count".to_string(), Assignment::Increment(3)),
                ("id".to_string(), Assignment::Value(7_u64.into())),
            ],
        );

        assert_eq!(
            prepared.sql,
            concat!(
                r#"UPDATE "parents" SET "children_count" = "children_count" + ?, "#,
                r#""id" = ? WHERE "parents"."id" > ?"#,
            )
        );
        assert_eq!(
            prepared.params,
            vec![Value::from(3_i64), Value::from(7_u64), Value::from(100_u64)],
            "SET binds before WHERE, in declaration order"
        );
    }

    #[test]
    fn a_plain_value_update_is_unchanged_by_the_assignment_form() {
        // `update_matching` now delegates, so this is the proof the delegation
        // did not alter what every existing caller renders.
        let prepared = update_matching::<Parent>(
            Dialect::Sqlite,
            &Criteria::new().where_eq("id", 3_u64),
            vec![("children_count".to_string(), Value::from(9_i64))],
        );

        assert_eq!(
            prepared.sql,
            r#"UPDATE "parents" SET "children_count" = ? WHERE "parents"."id" = ?"#
        );
        assert_eq!(prepared.params, vec![Value::from(9_i64), Value::from(3_u64)]);
    }

    #[test]
    fn a_sum_over_an_integer_column_is_cast_on_mysql() {
        // Without this the aggregate API is unusable on MySQL for the thing it
        // is most often asked to do: MySQL types a SUM as DECIMAL whatever it
        // summed, and the driver refuses to decode that into an i64. Every
        // application that hit it dropped to raw SQL with a CAST, which is how
        // a page ends up with eight hand-written queries.
        let criteria = Criteria::new().select(Projection::Sum("id".into()), "total");
        let sql = select_aggregate::<Post>(Dialect::MySql, &criteria).sql;

        assert!(sql.contains("CAST"), "no cast on mysql: {sql}");
        assert!(sql.to_uppercase().contains("SIGNED"), "cast is not to an integer: {sql}");
    }

    #[test]
    fn other_dialects_sum_without_a_cast() {
        // Postgres and SQLite already give an integer back for an integer sum,
        // and a cast there is noise in every query plan and every log.
        for dialect in [Dialect::Postgres, Dialect::Sqlite] {
            let criteria = Criteria::new().select(Projection::Sum("id".into()), "total");
            let sql = select_aggregate::<Post>(dialect, &criteria).sql;

            assert!(!sql.contains("CAST"), "{dialect:?} cast a sum it did not need to: {sql}");
        }
    }

    #[test]
    fn a_sum_over_a_non_integer_column_is_never_cast() {
        // Truncating a sum of decimals to a whole number is a silently wrong
        // total, which is worse than the decode error the cast exists to avoid.
        let criteria = Criteria::new().select(Projection::Sum("title".into()), "total");
        let sql = select_aggregate::<Post>(Dialect::MySql, &criteria).sql;

        assert!(!sql.contains("CAST"), "cast a sum over a non-integer column: {sql}");
    }
}
