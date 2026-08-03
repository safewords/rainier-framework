//! Soft deletes as a **scope every read applies for you** — [`TrashScope`], the
//! [`SoftDeletes`] marker, and the one predicate builder both query layers call.
//!
//! A soft-deleting table does not remove a row; it stamps a tombstone column
//! (`deleted_at`) and leaves the row where it was. Every read of that table then
//! has to say `AND deleted_at IS NULL`, and *that* is the part no design can
//! rely on a human to get right. Forgetting it raises nothing. The query runs,
//! the rows decode, the page renders — with the deleted rows on it. The failure
//! is silent and it is always in the direction of showing too much, which is the
//! direction that matters for anything a soft delete was used to hide.
//!
//! So the predicate is not the caller's to write. An entity marks its tombstone
//! column once, and from then on every read builder in
//! [`repo`](crate::repo), [`Query`](crate::Query) and the statement layer above
//! them appends the predicate itself.
//!
//! ```
//! use rainier_orm::{Entity, SoftDeletes};
//!
//! #[derive(Entity, Clone)]
//! #[orm(table = "documents")]
//! struct Document {
//!     #[orm(pk, auto_increment)]
//!     id: u64,
//!     title: String,
//!     #[orm(soft_delete)]
//!     deleted_at: Option<chrono::DateTime<chrono::Utc>>,
//! }
//!
//! assert_eq!(Document::soft_delete_column(), Some("deleted_at"));
//! fn only_for_marked<E: SoftDeletes>() {}
//! only_for_marked::<Document>();
//! ```
//!
//! # Why the marker is explicit
//!
//! Nothing here looks for a column *called* `deleted_at`. Inferring the scope
//! from a column name would mean a table that happens to record a deletion date
//! as domain data — when a customer's account was closed, when a document was
//! retracted by its author — silently stops returning most of its rows, and the
//! only evidence is that some report got smaller. Worse, it would happen on the
//! upgrade that introduced the inference rather than on a change anybody wrote.
//!
//! An explicit `#[orm(soft_delete)]` puts the behaviour change on one reviewable
//! line, in one table, at a moment a person chose. **An entity without the
//! marker builds exactly the SQL it did before this existed**, which is the
//! property that makes the feature safe to add to a running application.
//!
//! # What the declaration refuses
//!
//! Both rejections below are cases where accepting the marker would give a
//! working-looking entity whose every read returns the wrong rows, so each is a
//! compile error instead.
//!
//! A `NOT NULL` tombstone is never `NULL`, so `deleted_at IS NULL` matches no
//! row and **every** scoped read of the table comes back empty — with no error
//! to explain it:
//!
//! ```compile_fail
//! use rainier_orm::Entity;
//!
//! #[derive(Entity, Clone)]
//! #[orm(table = "documents")]
//! struct Document {
//!     #[orm(pk, auto_increment)]
//!     id: u64,
//!     #[orm(soft_delete)]
//!     deleted_at: chrono::DateTime<chrono::Utc>,   // not an `Option`
//! }
//! ```
//!
//! Two tombstones has no meaning, and a derive that picked the first would be
//! flipping a coin over what the whole table returns:
//!
//! ```compile_fail
//! use rainier_orm::Entity;
//!
//! #[derive(Entity, Clone)]
//! #[orm(table = "documents")]
//! struct Document {
//!     #[orm(pk, auto_increment)]
//!     id: u64,
//!     #[orm(soft_delete)]
//!     deleted_at: Option<chrono::DateTime<chrono::Utc>>,
//!     #[orm(soft_delete)]
//!     archived_at: Option<chrono::DateTime<chrono::Utc>>,
//! }
//! ```
//!
//! # What it does not do
//!
//! It scopes **reads**. It does not turn a delete into an update: nothing here
//! rewrites [`repo::delete_by_pk`](crate::repo::delete_by_pk) or
//! [`Query::delete`](crate::Query::delete()) into a tombstone write, and a
//! `DELETE` still removes the row. Two reasons, and the second is the real one:
//!
//! - A hard delete names its rows with a predicate the caller wrote. A
//!   tombstoned row that matches it is still a row of that table, and refusing
//!   to remove it would leave a purge unable to purge.
//! - A scoped `DELETE` fails **silently and without bound**. "Remove everything
//!   tombstoned more than thirty days ago" is `where_lt("deleted_at", cutoff)`
//!   followed by a delete — under a scope that hides tombstoned rows it matches
//!   nothing, forever, and the only symptom is a table that never stops growing.
//!
//! Writing the tombstone is likewise the application's: set the column and save.
//! It is one visible line, and the alternative — `delete` quietly meaning
//! `update` — is exactly the sort of unannounced behaviour swap this module's
//! explicit marker exists to avoid.
//!
//! # Reading the deleted rows on purpose
//!
//! An admin trash view, a restore endpoint, a purge job: all of them mean to see
//! tombstoned rows, and under an automatic scope all of them would come back
//! empty with no error. That is the one hazard this feature introduces, so
//! saying so is a first-class part of the API rather than a footnote —
//! [`TrashScope::WithTrashed`] and [`TrashScope::OnlyTrashed`], reached through
//! `with_trashed()` / `only_trashed()` on
//! [`Query`](crate::Query::with_trashed) and
//! [`Cursor`](crate::repo::Cursor::with_trashed).
//!
//! Both of those are bounded on [`SoftDeletes`], so asking for the trashed rows
//! of a table that has no tombstone column does not compile. It is the same
//! trick [`SingleKey`](crate::SingleKey) plays for composite keys, for the same
//! reason: the alternative is a call that builds, runs, and answers a question
//! nobody asked.

use crate::Entity;
use sea_query::{ColumnRef, Expr, SimpleExpr, Value};

/// Which rows of a soft-deleting table a read may see.
///
/// [`Active`](Self::Active) is the default everywhere, and it is the default
/// *because* the other two are the ones somebody has to ask for. A read that
/// forgets to choose gets the safe answer; a read that wants the tombstones has
/// to say so in the source, where a reviewer can see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrashScope {
    /// Only rows that are not tombstoned — `deleted_at IS NULL`. The default.
    #[default]
    Active,
    /// Every row, tombstoned or not. No predicate is added.
    WithTrashed,
    /// Only tombstoned rows — `deleted_at IS NOT NULL`. What a trash view and a
    /// purge job select.
    OnlyTrashed,
}

/// An [`Entity`] that marks rows deleted instead of removing them.
///
/// A marker with no items, emitted by `#[derive(Entity)]` for exactly the
/// structs carrying an `#[orm(soft_delete)]` field. Its whole job is to be a
/// bound on the APIs that ask about tombstoned rows, so that pointing one at a
/// table with no tombstone column fails to compile:
///
/// ```compile_fail
/// use rainier_orm::{repo, Entity};
///
/// #[derive(Entity, Clone)]
/// #[orm(table = "widgets")]
/// struct Widget {
///     #[orm(pk, auto_increment)]
///     id: u64,
/// }
///
/// // `Widget` has no tombstone column, so "only the deleted widgets" is not a
/// // question it can answer — and answering it anyway would return either every
/// // widget or none, with nothing to say which.
/// let _ = repo::query::<Widget>().only_trashed();
/// ```
///
/// The same call on a marked entity is ordinary:
///
/// ```
/// use rainier_orm::{repo, Entity};
///
/// #[derive(Entity, Clone)]
/// #[orm(table = "documents")]
/// struct Document {
///     #[orm(pk, auto_increment)]
///     id: u64,
///     #[orm(soft_delete)]
///     deleted_at: Option<chrono::DateTime<chrono::Utc>>,
/// }
///
/// let _ = repo::query::<Document>().only_trashed();
/// ```
pub trait SoftDeletes: Entity {}

/// The predicate `scope` implies for `E`, or `None` when the read needs none.
///
/// The single place either query layer decides what soft-delete scoping means,
/// so a `SELECT`, a `COUNT` and a grouped count over the same entity cannot
/// disagree about which rows exist. `resolve` turns the column name into a
/// reference the calling builder can use — qualified to the entity's table where
/// a join could make a bare name ambiguous, bare where the builder writes its
/// other columns bare.
///
/// The `None` cases are the ones that must stay byte-identical to a build
/// without this feature: an unmarked entity adds nothing, and so does an
/// explicit [`WithTrashed`](TrashScope::WithTrashed).
pub fn scope_predicate<E: Entity>(
    scope: TrashScope,
    resolve: impl FnOnce(&str) -> ColumnRef,
) -> Option<SimpleExpr> {
    match (scope, E::soft_delete_column()) {
        // Nothing to add: either the caller asked for everything, or the table
        // has no notion of a tombstone and never did.
        (TrashScope::WithTrashed, _) | (TrashScope::Active, None) => None,

        (TrashScope::Active, Some(column)) => Some(Expr::col(resolve(column)).is_null()),
        (TrashScope::OnlyTrashed, Some(column)) => Some(Expr::col(resolve(column)).is_not_null()),

        // "Only the deleted rows" of a table that cannot delete rows. The empty
        // set is the honest answer and not a fudge — but it is worth being
        // deliberate about, because the alternative reading ("no tombstone
        // column, so no predicate") would return *every* row to a caller who
        // asked for the deleted ones, which is how a trash view ends up showing
        // live data.
        //
        // Unreachable through [`Query`](crate::Query) and
        // [`Cursor`](crate::repo::Cursor), whose `only_trashed` is bounded on
        // [`SoftDeletes`]. It is reachable from the layers above that carry the
        // scope in an entity-erased value, which is why it has an answer at all.
        (TrashScope::OnlyTrashed, None) => Some(matches_nothing()),
    }
}

/// `1 = 0` — a predicate no row satisfies, in every dialect.
///
/// [`SimpleExpr::Constant`] rather than [`Expr::val`], so the two literals are
/// rendered inline instead of pushed into the parameter list. A bound `1` and
/// `0` would sit between the caller's own values and the ones that follow, which
/// is a confusing thing to find in a log and a needless difference in the
/// prepared-statement cache. Nothing about it is caller-supplied, so nothing
/// about it is injectable.
fn matches_nothing() -> SimpleExpr {
    Expr::expr(SimpleExpr::Constant(Value::Int(Some(1))))
        .eq(SimpleExpr::Constant(Value::Int(Some(0))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dialect;
    use sea_query::{Alias, IntoColumnRef, Query};

    #[derive(crate::Entity, Clone, Debug)]
    #[orm(table = "documents")]
    struct Document {
        #[orm(pk, auto_increment)]
        id: u64,
        title: String,
        #[orm(soft_delete)]
        deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    #[derive(crate::Entity, Clone, Debug)]
    #[orm(table = "widgets")]
    struct Widget {
        #[orm(pk, auto_increment)]
        id: u64,
    }

    /// A table whose `deleted_at` is domain data, not row lifecycle — the case
    /// that makes inferring the scope from a column name unacceptable.
    #[derive(crate::Entity, Clone, Debug)]
    #[orm(table = "retractions")]
    struct Retraction {
        #[orm(pk, auto_increment)]
        id: u64,
        /// When the author retracted the item this row describes. Every row has
        /// one; none of them is a tombstone.
        deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    }

    fn bare(name: &str) -> ColumnRef {
        Alias::new(name.to_owned()).into_column_ref()
    }

    /// Render a predicate on its own, so the test reads the SQL and the binds it
    /// produces.
    fn render(expr: Option<SimpleExpr>) -> Option<(String, Vec<Value>)> {
        let expr = expr?;
        let mut stmt = Query::select();
        stmt.from(Alias::new("t")).expr(SimpleExpr::Constant(Value::Int(Some(1)))).and_where(expr);
        let (sql, params) = Dialect::Sqlite.build_query(&stmt);
        Some((sql, params.0))
    }

    #[test]
    fn a_marked_entity_reports_its_tombstone_column() {
        assert_eq!(Document::soft_delete_column(), Some("deleted_at"));
    }

    #[test]
    fn an_unmarked_entity_reports_none_even_with_a_deleted_at_column() {
        assert_eq!(Widget::soft_delete_column(), None);
        assert_eq!(
            Retraction::soft_delete_column(),
            None,
            "a `deleted_at` the entity did not mark is domain data, and scoping \
             on it would silently change what this table returns"
        );
    }

    #[test]
    fn the_active_scope_excludes_tombstoned_rows() {
        let (sql, _) = render(scope_predicate::<Document>(TrashScope::Active, bare)).unwrap();
        assert!(sql.contains(r#""deleted_at" IS NULL"#), "{sql}");
    }

    #[test]
    fn only_trashed_selects_them_instead() {
        let (sql, _) = render(scope_predicate::<Document>(TrashScope::OnlyTrashed, bare)).unwrap();
        assert!(sql.contains(r#""deleted_at" IS NOT NULL"#), "{sql}");
    }

    #[test]
    fn with_trashed_adds_nothing_at_all() {
        assert!(scope_predicate::<Document>(TrashScope::WithTrashed, bare).is_none());
    }

    #[test]
    fn an_unmarked_entity_adds_nothing_at_all() {
        // The property every existing caller depends on: no marker, no change.
        assert!(scope_predicate::<Widget>(TrashScope::Active, bare).is_none());
        assert!(scope_predicate::<Widget>(TrashScope::WithTrashed, bare).is_none());
    }

    #[test]
    fn only_trashed_on_an_unmarked_entity_matches_nothing_rather_than_everything() {
        let (sql, params) =
            render(scope_predicate::<Widget>(TrashScope::OnlyTrashed, bare)).unwrap();
        assert!(sql.contains("1 = 0"), "{sql}");
        assert!(params.is_empty(), "the contradiction is inline, not two stray binds: {params:?}");
    }

    #[test]
    fn the_predicate_can_be_qualified_to_the_entitys_table() {
        // What a criteria with a join needs: an unqualified `deleted_at` is
        // ambiguous the moment the joined table has one too, and an ambiguous
        // column is an error from the database rather than a wrong answer.
        let qualified =
            |name: &str| (Alias::new("documents"), Alias::new(name.to_owned())).into_column_ref();
        let (sql, _) = render(scope_predicate::<Document>(TrashScope::Active, qualified)).unwrap();
        assert!(sql.contains(r#""documents"."deleted_at" IS NULL"#), "{sql}");
    }

    #[test]
    fn the_default_scope_is_the_safe_one() {
        assert_eq!(TrashScope::default(), TrashScope::Active);
    }
}
