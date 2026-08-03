//! The [`Entity`] trait — the contract `#[derive(Entity)]` fulfils.
//!
//! Everything generic CRUD needs is here: where the rows live ([`table`],
//! [`primary_key_columns`]), what the columns are ([`columns`]), how to decode a
//! row ([`from_row`]), and how to project a value back out for writes
//! ([`insert_values`], [`update_values`], [`pk_values`]). The repository layer
//! ([`crate::repo`]) and schema builder ([`crate::schema`]) are written purely
//! against this trait, so they work for any derived struct with no per-entity
//! code.
//!
//! ## Keys are a *list* of columns
//!
//! A primary key is [`primary_key_columns`] — one column for most tables, more
//! for a join table or a per-bucket aggregate keyed `(parent_id, slot)`. The
//! single-column accessors ([`primary_key`], [`pk_value`]) still exist and are
//! what most code reads, but they answer *"the first key column"*, not *"the
//! key"*. That distinction is the whole safety story of composite keys, so it is
//! worth being precise about:
//!
//! - Anything that builds a **key predicate** — the `WHERE` of a `find`,
//!   `update` or `delete` — must go through [`crate::key::key_condition`], which
//!   `AND`s every part together. A `WHERE` built from [`primary_key`] alone on a
//!   two-column key matches every row sharing that first column, so an `UPDATE`
//!   or `DELETE` written that way silently rewrites rows it was never pointed
//!   at. That is the bug this design exists to make unwritable.
//! - Anything that only needs a *name* or a *routing* key — shard routing,
//!   [`Model::route_key_name`], a relation's default local key — may use
//!   [`primary_key`], because being wrong there is a missed lookup rather than a
//!   corrupted table.
//!
//! The APIs that take **one** key value ([`crate::repo::find_by_pk`],
//! [`crate::repo::delete_by_pk`], [`crate::repo::cursor`]) are bounded on
//! [`SingleKey`], so pointing one at a composite-key entity is a compile error
//! rather than a partial-key `WHERE`. Composite callers use
//! [`crate::repo::find_by_keys`] / [`crate::repo::delete_by_keys`].
//!
//! [`Model::route_key_name`]: https://docs.rs/rainier-database/latest/rainier_database/trait.Model.html
//! [`table`]: Entity::table
//! [`primary_key`]: Entity::primary_key
//! [`primary_key_columns`]: Entity::primary_key_columns
//! [`columns`]: Entity::columns
//! [`from_row`]: Entity::from_row
//! [`insert_values`]: Entity::insert_values
//! [`update_values`]: Entity::update_values
//! [`pk_value`]: Entity::pk_value
//! [`pk_values`]: Entity::pk_values

use crate::{ColumnSpec, ForeignKeySpec, IndexSpec, Result, Row};
use sea_query::Value;

/// A struct that maps to one table. Implemented by `#[derive(Entity)]`; hand
/// impls are possible but the derive is the intended path.
pub trait Entity: Sized {
    /// The table name.
    fn table() -> &'static str;

    /// Column metadata, in declaration order. Drives both DDL generation and
    /// the column list of generated `INSERT`s.
    fn columns() -> &'static [ColumnSpec];

    /// Every primary-key column, **in declaration order**.
    ///
    /// The general form of the key, and the only one that is correct for a
    /// composite key. Order is part of the contract, not an incidental detail:
    /// it is the order [`pk_values`](Self::pk_values) reports its values in, so
    /// the two are zipped positionally to build a predicate. It is also the
    /// column order of the emitted `PRIMARY KEY (a, b)`, which decides which
    /// prefix lookups the index can serve.
    ///
    /// The derive rejects a struct with no `#[orm(pk)]` field, so this is never
    /// empty for a derived entity.
    fn primary_key_columns() -> &'static [&'static str];

    /// The **first** primary-key column — *not* "the key".
    ///
    /// Kept because most tables have one key column and most callers want to
    /// name it, and because it is what shard routing and route-model binding
    /// read. For a composite key it is the first part only, so it must never
    /// reach a `WHERE`; see the module docs for the split.
    ///
    /// Defaulted here so a hand-written impl gets it from
    /// [`primary_key_columns`](Self::primary_key_columns) for free; the derive
    /// emits it directly.
    ///
    /// The empty-key fallback is `""` rather than a panic. It is unreachable
    /// through the derive, and the callers that remain are naming and routing
    /// ones where an unmatched name is a miss — every caller that could turn it
    /// into a predicate is bounded on [`SingleKey`], which such a type would not
    /// have. Taking the process down inside a metadata accessor would be the
    /// larger failure.
    fn primary_key() -> &'static str {
        Self::primary_key_columns().first().copied().unwrap_or_default()
    }

    /// Secondary indexes (single-column and composite, unique or not),
    /// emitted as separate `CREATE INDEX` statements by [`crate::schema`].
    /// Defaults to none.
    fn indexes() -> &'static [IndexSpec] {
        &[]
    }

    /// Foreign-key constraints, emitted inline in the `CREATE TABLE`. These
    /// enforce referential integrity *within one database*; they are not ORM
    /// relationships (no navigation is generated). Defaults to none.
    fn foreign_keys() -> &'static [ForeignKeySpec] {
        &[]
    }

    /// Columns whose values are the **shard key** — a shard-encoded id
    /// (`#[orm(shard_key)]`), the "user id by proxy". When a query constrains
    /// one of these, the repo layer routes the operation to that id's shard.
    ///
    /// This is the *only* per-entity sharding metadata, and it's unavoidable:
    /// the key column differs per table (`id` on the owner, `user_id` on its
    /// rows), and its presence is what marks an entity *sharded* vs *global*.
    /// Everything else about sharding — the family name, the id codec, whether a
    /// connector shards at all — lives on the connector, not here. An entity
    /// with no shard-key column is **global**, and sharding never affects it.
    fn shard_columns() -> &'static [&'static str] {
        &[]
    }

    /// The **tombstone column** of a soft-deleting table, or `None`.
    ///
    /// `Some` for exactly the structs carrying an `#[orm(soft_delete)]` field,
    /// and every read builder in the framework appends `<column> IS NULL` when
    /// it is set — see [`crate::trash`] for what that means and how to suppress
    /// it. A `None` entity builds precisely the SQL it built before soft-delete
    /// scoping existed, which is what makes the feature safe to add underneath
    /// an application that has never heard of it.
    ///
    /// This is metadata rather than a marker trait because the layers above are
    /// entity-erased at the point they need the answer — a repository behind an
    /// `Arc<dyn …>` still has to build the predicate. The compile-time half of
    /// the story is [`SoftDeletes`](crate::SoftDeletes), which is what the APIs
    /// that *ask about* tombstoned rows are bounded on.
    ///
    /// Note what it is not: a search for a column *named* `deleted_at`. Some
    /// tables record a deletion date as domain data rather than as row
    /// lifecycle, and inferring the scope from the name would silently stop such
    /// a table returning most of its rows — the same class of quiet wrongness
    /// this scope exists to remove, arrived at from the other side.
    fn soft_delete_column() -> Option<&'static str> {
        None
    }

    /// Decode one row into `Self`. Generated to read each field through its
    /// [`FromColumn`](crate::FromColumn) impl.
    fn from_row(row: &dyn Row) -> Result<Self>;

    /// `(column, value)` pairs for an `INSERT`. Auto-increment primary keys
    /// are omitted so the database assigns them.
    fn insert_values(&self) -> Vec<(&'static str, Value)>;

    /// `(column, value)` pairs for an `UPDATE` — every column except the
    /// primary key.
    fn update_values(&self) -> Vec<(&'static str, Value)>;

    /// This row's primary-key values, in
    /// [`primary_key_columns`](Self::primary_key_columns) order.
    ///
    /// The general form, and what every key predicate is built from. One value
    /// for an ordinary table, one per key part for a composite one.
    fn pk_values(&self) -> Vec<Value>;

    /// This instance's **first** primary-key value — the counterpart of
    /// [`primary_key`](Self::primary_key), with the same caveat: it is one part
    /// of a composite key, so it belongs in a routing decision and never in a
    /// `WHERE`.
    ///
    /// A keyless hand impl falls back to SQL `NULL`, which compares equal to
    /// nothing — so if one ever did reach a predicate it would match no row
    /// rather than every row. Unreachable through the derive, like
    /// [`primary_key`](Self::primary_key).
    fn pk_value(&self) -> Value {
        self.pk_values().into_iter().next().unwrap_or(Value::Bool(None))
    }

    /// One column's value by name, or `None` if the entity has no such column.
    ///
    /// What relationship loading needs: given a slice of parents, collect the
    /// key column to look children up by. Defaults to a scan of
    /// [`update_values`](Self::update_values) plus the primary key, which is
    /// correct for every entity and costs one allocation of the row's values.
    ///
    /// The key columns have to be checked separately because
    /// [`update_values`](Self::update_values) deliberately omits *all* of them.
    /// Matching only [`primary_key`](Self::primary_key) would therefore report
    /// `None` for the second column of a composite key — a column the entity
    /// plainly has — and a relationship keyed on it would silently load nothing.
    fn value_of(&self, column: &str) -> Option<Value> {
        if let Some(index) = Self::primary_key_columns().iter().position(|name| *name == column) {
            return self.pk_values().into_iter().nth(index);
        }
        self.update_values().into_iter().find(|(name, _)| *name == column).map(|(_, value)| value)
    }
}

/// An [`Entity`] whose primary key is exactly **one** column.
///
/// A marker with no items. Its whole job is to be a bound on the APIs that take
/// a single key value — [`find_by_pk`](crate::repo::find_by_pk),
/// [`delete_by_pk`](crate::repo::delete_by_pk), [`cursor`](crate::repo::cursor),
/// [`Tracked::load`](crate::active::Tracked::load) — so that aiming one at a
/// two-column key fails to compile.
///
/// Without it those functions would still *build*: they would render
/// `WHERE first_column = ?` and quietly operate on every row sharing that value.
/// For a `find` that is the wrong row; for a `delete` it is the wrong rows,
/// gone. A missing trait bound is a diagnostic at the call site, which is the
/// only place with enough context to fix it.
///
/// `#[derive(Entity)]` emits this impl only when the struct has exactly one
/// `#[orm(pk)]` field, so it cannot be claimed by accident.
///
/// A two-column key does not compile against a one-value API:
///
/// ```compile_fail
/// use rainier_orm::{repo, Entity, Executor};
///
/// #[derive(Entity, Clone)]
/// #[orm(table = "memberships")]
/// struct Membership {
///     #[orm(pk)]
///     team_id: u64,
///     #[orm(pk)]
///     user_id: u64,
/// }
///
/// async fn find(db: &impl Executor) {
///     // `Membership` is keyed on (team_id, user_id), so one value cannot name
///     // a row — and `WHERE team_id = ?` would match the whole team.
///     let _ = repo::find_by_pk::<Membership, _, _>(db, 1_u64).await;
/// }
/// ```
///
/// The same call on a one-column key is exactly as it always was — so the
/// failure above is the missing bound, not a mistake in the example:
///
/// ```
/// use rainier_orm::{repo, Entity, Executor};
///
/// #[derive(Entity, Clone)]
/// #[orm(table = "widgets")]
/// struct Widget {
///     #[orm(pk, auto_increment)]
///     id: u64,
///     name: String,
/// }
///
/// async fn find(db: &impl Executor) {
///     let _ = repo::find_by_pk::<Widget, _, _>(db, 1_u64).await;
/// }
/// ```
pub trait SingleKey: Entity {}
