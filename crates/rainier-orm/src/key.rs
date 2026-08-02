//! The primary-key predicate — built in exactly one place.
//!
//! Every `WHERE` that identifies a row *by its key* comes from
//! [`key_condition`], across both this crate's [`repo`](crate::repo) and
//! `rainier-database`'s statement layer. That centralisation is the point.
//!
//! A composite key is only safe if **every** part reaches the predicate. Miss
//! one and the statement still renders, still runs, and still reports rows
//! affected — it just matches the whole bucket sharing the first column instead
//! of the single row that was named. An `UPDATE` written that way overwrites
//! sibling rows and a `DELETE` removes them, with nothing in the result to say
//! so. Spread that construction across five call sites and it only takes one of
//! them to be written against `primary_key()` out of habit.
//!
//! So there is one implementation, it derives the columns from
//! [`Entity::primary_key_columns`] rather than taking them from the caller, and
//! it refuses to build anything from a value list whose length disagrees.

use crate::route::route_for;
use crate::{Entity, Error, Result, ShardRoute};
use sea_query::{Alias, Cond, Expr, Value};

/// `WHERE a = ? AND b = ?` over the whole of `E`'s primary key.
///
/// `values` are positional against [`Entity::primary_key_columns`], which is why
/// that order is part of the [`Entity`] contract.
///
/// # Errors
///
/// If `values` does not have exactly one entry per key column. A short list is
/// the dangerous case — it is precisely a partial key — and a long one means the
/// caller has the wrong entity in mind, so neither can be interpreted. Returning
/// an error rather than filling in or ignoring the difference keeps a
/// mis-keyed call from becoming a statement that runs against the wrong rows.
///
/// Note the arity is checked, not proven: `values` is a runtime list. Callers
/// that hold a single value should instead be bounded on
/// [`SingleKey`](crate::SingleKey), which moves the same check to compile time.
pub fn key_condition<E: Entity>(values: &[Value]) -> Result<Cond> {
    let columns = E::primary_key_columns();

    if columns.is_empty() {
        return Err(Error::msg(format!(
            "`{}` has no primary key, so no row can be identified by one",
            E::table()
        )));
    }
    if columns.len() != values.len() {
        return Err(Error::msg(format!(
            "`{}` is keyed on {} column(s) ({}), but {} key value(s) were given",
            E::table(),
            columns.len(),
            columns.join(", "),
            values.len(),
        )));
    }

    // `Cond::all` renders as `a = ? AND b = ?`; every part is present by
    // construction because the loop is over the columns, not over the caller's
    // list.
    let mut condition = Cond::all();
    for (column, value) in columns.iter().zip(values) {
        condition = condition.add(Expr::col(Alias::new(*column)).eq(value.clone()));
    }
    Ok(condition)
}

/// `WHERE a = ? AND b = ?` identifying **this row**, from its own key values.
///
/// The infallible counterpart of [`key_condition`], for the writes that take a
/// whole entity ([`repo::update`](crate::repo::update()),
/// `Tracked::save`). There is no arity to get wrong: `primary_key_columns()` and
/// `pk_values()` are emitted by the same derive from the same list of fields, so
/// for any derived entity they agree by construction. Returning a `Result` here
/// would put a `?` on the caller for a branch that cannot be reached, and make
/// the statement layer's one write fallible while its dozen siblings are not.
///
/// A hand-written [`Entity`] impl *could* still disagree, so the mismatch has a
/// defined outcome rather than an undefined one: a condition that matches no
/// row. The statement then reports zero rows affected, which is the direction
/// that loses nothing — the alternative, zipping the two lists and stopping at
/// the shorter, is exactly the partial `WHERE` this module exists to prevent.
pub fn row_key_condition<E: Entity>(entity: &E) -> Cond {
    let columns = E::primary_key_columns();
    let values = entity.pk_values();

    if columns.is_empty() || columns.len() != values.len() {
        return Cond::all().add(Expr::val(1).eq(0));
    }

    let mut condition = Cond::all();
    for (column, value) in columns.iter().zip(values) {
        condition = condition.add(Expr::col(Alias::new(*column)).eq(value));
    }
    condition
}

/// The shard route implied by a whole primary key: the first key column that is
/// shard-encoded and carries a usable value.
///
/// A composite key can mix a shard-encoded part with an ordinary one — a table
/// keyed `(user_id, slot)` routes by the `user_id` half — so the scan runs over
/// every part rather than assuming the first one decides.
pub fn key_route<E: Entity>(values: &[Value]) -> ShardRoute {
    if E::shard_columns().is_empty() {
        return ShardRoute::Global;
    }
    for (column, value) in E::primary_key_columns().iter().zip(values) {
        let route = route_for::<E>(column, value);
        if !matches!(route, ShardRoute::Global) {
            return route;
        }
    }
    ShardRoute::Global
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dialect;
    use sea_query::Query;

    #[derive(crate::Entity, Clone)]
    #[orm(table = "memberships")]
    struct Membership {
        #[orm(pk)]
        team_id: u64,
        #[orm(pk)]
        user_id: u64,
        role: String,
    }

    #[derive(crate::Entity, Clone)]
    #[orm(table = "widgets")]
    struct Widget {
        #[orm(pk, auto_increment)]
        id: u64,
        name: String,
    }

    /// Render a bare `SELECT` carrying only the key predicate, so a test can
    /// assert on the `WHERE` without the rest of a statement's noise.
    fn where_sql(condition: Cond) -> String {
        let mut stmt = Query::select();
        stmt.expr(Expr::val(1)).cond_where(condition);
        Dialect::Sqlite.build_query(&stmt).0
    }

    #[test]
    fn a_composite_key_ands_every_part_together() {
        let condition = key_condition::<Membership>(&[7_u64.into(), 9_u64.into()]).unwrap();
        let sql = where_sql(condition);

        assert!(sql.contains("\"team_id\""), "{sql}");
        assert!(sql.contains("\"user_id\""), "{sql}");
        assert!(sql.contains(" AND "), "both parts must be required, not either: {sql}");
    }

    #[test]
    fn a_single_key_is_one_equality_with_no_and() {
        let sql = where_sql(key_condition::<Widget>(&[1_u64.into()]).unwrap());
        assert!(sql.contains("\"id\""), "{sql}");
        assert!(!sql.contains(" AND "), "a one-column key needs no conjunction: {sql}");
    }

    #[test]
    fn a_partial_key_is_refused_rather_than_rendered() {
        // The dangerous input: one value for a two-column key. Rendering it
        // would produce `WHERE team_id = ?`, which matches the whole team.
        let error = key_condition::<Membership>(&[7_u64.into()]).unwrap_err().to_string();
        assert!(error.contains("memberships"), "{error}");
        assert!(error.contains("team_id, user_id"), "the error names the key it wanted: {error}");
    }

    #[test]
    fn too_many_key_values_are_refused_too() {
        let values = [7_u64.into(), 9_u64.into(), 11_u64.into()];
        assert!(key_condition::<Membership>(&values).is_err());
    }

    #[test]
    fn the_key_columns_keep_their_declared_order() {
        assert_eq!(Membership::primary_key_columns(), &["team_id", "user_id"]);
    }

    #[test]
    fn a_rows_own_key_needs_no_arity_check_to_be_complete() {
        let membership = Membership { team_id: 7, user_id: 9, role: "owner".into() };
        let sql = where_sql(row_key_condition(&membership));

        assert!(sql.contains("\"team_id\""), "{sql}");
        assert!(sql.contains("\"user_id\""), "{sql}");
        assert!(sql.contains(" AND "), "{sql}");
    }

    /// A hand-written impl whose two halves disagree. Unreachable through the
    /// derive, but the outcome has to be defined rather than merely unlikely —
    /// and the defined outcome is "matches nothing", not "matches the bucket".
    struct Inconsistent;

    impl Entity for Inconsistent {
        fn table() -> &'static str {
            "inconsistent"
        }
        fn columns() -> &'static [crate::ColumnSpec] {
            &[]
        }
        fn primary_key_columns() -> &'static [&'static str] {
            &["a", "b"]
        }
        fn from_row(_row: &dyn crate::Row) -> Result<Self> {
            Ok(Self)
        }
        fn insert_values(&self) -> Vec<(&'static str, Value)> {
            Vec::new()
        }
        fn update_values(&self) -> Vec<(&'static str, Value)> {
            Vec::new()
        }
        /// One value for a two-column key — the disagreement.
        fn pk_values(&self) -> Vec<Value> {
            vec![1_i32.into()]
        }
    }

    #[test]
    fn a_key_that_does_not_match_its_columns_matches_no_row() {
        let mut stmt = Query::select();
        stmt.expr(Expr::val(1)).cond_where(row_key_condition(&Inconsistent));
        let (sql, params) = Dialect::Sqlite.build_query(&stmt);

        // Neither column is constrained, so this cannot be a partial key…
        assert!(!sql.contains("\"a\""), "no half-key predicate: {sql}");
        assert!(!sql.contains("\"b\""), "no half-key predicate: {sql}");
        // …and the predicate is unsatisfiable, so it cannot be an unfiltered
        // one. The tail of the bindings, because the `SELECT 1` projection this
        // harness uses binds a parameter of its own ahead of the `WHERE`.
        assert_eq!(
            params.0[params.0.len() - 2..],
            [Value::from(1), Value::from(0)],
            "`1 = 0`: {sql}"
        );
    }
}
