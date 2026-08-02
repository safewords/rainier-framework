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
    Query as SqQuery, SimpleExpr, Value,
};
use rainier_orm::{Dialect, Entity, ShardRoute};

use crate::criteria::{Criteria, DatePart, JoinKind, Projection};

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

/// `SELECT … FROM table` — every row.
pub fn select_all<E: Entity>(dialect: Dialect) -> Prepared {
    let stmt = select_columns::<E>();
    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route: ShardRoute::Global }
}

/// `SELECT … WHERE pk = ? LIMIT 1`.
pub fn select_by_pk<E: Entity>(dialect: Dialect, key: Value) -> Prepared {
    let route = route_for::<E>(E::primary_key(), &key);
    let mut stmt = select_columns::<E>();
    stmt.and_where(Expr::col(alias(E::primary_key())).eq(key)).limit(1);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
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
    if let Some(limit) = limit {
        stmt.limit(limit);
    }

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// Render a [`Projection`] for this dialect.
///
/// The date parts are the reason this is a function and not a string in the
/// caller: MySQL writes `MONTH(x)`, SQLite has no such function and needs
/// `CAST(strftime('%m', x) AS INTEGER)`, and Postgres spells it
/// `date_part('month', x)`. Application code that picks one works on one
/// deployment and 500s on the others — including the SQLite its own tests run.
fn projection_expr<E: Entity>(dialect: Dialect, projection: &Projection) -> SimpleExpr {
    let col = |name: &str| Expr::col(column_ref::<E>(name));

    match projection {
        Projection::Column(c) => col(c).into(),
        Projection::CountAll => Func::count(Expr::col(Asterisk)).into(),
        Projection::Count(c) => Func::count(col(c)).into(),
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

    let route = apply_criteria::<E>(&mut stmt, criteria, true);

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
    let route = apply_criteria::<E>(&mut stmt, criteria, true);

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
    let route = apply_criteria::<E>(&mut stmt, criteria, false);
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
    let route = apply_criteria::<E>(&mut stmt, criteria, false);

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
    stmt: &mut rainier_orm::sea_query::SelectStatement,
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
    // the shape it has in SQL, so there is no precedence to get wrong.
    for group in criteria.or_groups() {
        let mut any = Cond::any();
        for constraint in group {
            any = any.add(constraint.to_expr(column_ref::<E>(constraint.column())));
        }
        condition = condition.add(any);
    }

    stmt.cond_where(condition);

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

/// `UPDATE table SET … WHERE pk = ?` — every non-key column.
pub fn update<E: Entity>(dialect: Dialect, entity: &E) -> Prepared {
    let key = entity.pk_value();
    let route = route_for::<E>(E::primary_key(), &key);

    let mut stmt = SqQuery::update();
    stmt.table(alias(E::table()));
    for (column, value) in entity.update_values() {
        stmt.value(alias(column), value);
    }
    stmt.and_where(Expr::col(alias(E::primary_key())).eq(key));

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `UPDATE table SET … WHERE <criteria>` — only the named columns.
pub fn update_matching<E: Entity>(
    dialect: Dialect,
    criteria: &Criteria,
    set: Vec<(String, Value)>,
) -> Prepared {
    let mut stmt = SqQuery::update();
    stmt.table(alias(E::table()));
    for (column, value) in set {
        stmt.value(alias(&column), value);
    }

    let mut condition = Cond::all();
    let mut route = ShardRoute::Global;
    for constraint in criteria.constraints() {
        if let (Some((column, value)), true) =
            (constraint.as_equality(), matches!(route, ShardRoute::Global))
        {
            route = route_for::<E>(column, value);
        }
        condition = condition.add(constraint.to_expr(column_ref::<E>(constraint.column())));
    }
    // Each `or_where` group is one parenthesised `OR`, `AND`-ed with the rest —
    // the shape it has in SQL, so there is no precedence to get wrong.
    for group in criteria.or_groups() {
        let mut any = Cond::any();
        for constraint in group {
            any = any.add(constraint.to_expr(column_ref::<E>(constraint.column())));
        }
        condition = condition.add(any);
    }

    stmt.cond_where(condition);

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `DELETE FROM table WHERE pk = ?`.
pub fn delete_by_pk<E: Entity>(dialect: Dialect, key: Value) -> Prepared {
    let route = route_for::<E>(E::primary_key(), &key);

    let mut stmt = SqQuery::delete();
    stmt.from_table(alias(E::table()));
    stmt.and_where(Expr::col(alias(E::primary_key())).eq(key));

    let (sql, params) = dialect.build_query(&stmt);
    Prepared { sql, params: params.0, route }
}

/// `DELETE FROM table WHERE <criteria>`.
pub fn delete_matching<E: Entity>(dialect: Dialect, criteria: &Criteria) -> Prepared {
    let mut stmt = SqQuery::delete();
    stmt.from_table(alias(E::table()));

    let mut condition = Cond::all();
    let mut route = ShardRoute::Global;
    for constraint in criteria.constraints() {
        if let (Some((column, value)), true) =
            (constraint.as_equality(), matches!(route, ShardRoute::Global))
        {
            route = route_for::<E>(column, value);
        }
        condition = condition.add(constraint.to_expr(column_ref::<E>(constraint.column())));
    }
    // Each `or_where` group is one parenthesised `OR`, `AND`-ed with the rest —
    // the shape it has in SQL, so there is no precedence to get wrong.
    for group in criteria.or_groups() {
        let mut any = Cond::any();
        for constraint in group {
            any = any.add(constraint.to_expr(column_ref::<E>(constraint.column())));
        }
        condition = condition.add(any);
    }

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
}
