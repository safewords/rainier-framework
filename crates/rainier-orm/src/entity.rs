//! The [`Entity`] trait — the contract `#[derive(Entity)]` fulfils.
//!
//! Everything generic CRUD needs is here: where the rows live ([`table`],
//! [`primary_key`]), what the columns are ([`columns`]), how to decode a row
//! ([`from_row`]), and how to project a value back out for writes
//! ([`insert_values`], [`update_values`], [`pk_value`]). The repository layer
//! ([`crate::repo`]) and schema builder ([`crate::schema`]) are written purely
//! against this trait, so they work for any derived struct with no per-entity
//! code.
//!
//! [`table`]: Entity::table
//! [`primary_key`]: Entity::primary_key
//! [`columns`]: Entity::columns
//! [`from_row`]: Entity::from_row
//! [`insert_values`]: Entity::insert_values
//! [`update_values`]: Entity::update_values
//! [`pk_value`]: Entity::pk_value

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

    /// The primary-key column name. Used by `find_by_pk` / `update` /
    /// `delete_by_pk`.
    fn primary_key() -> &'static str;

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

    /// Decode one row into `Self`. Generated to read each field through its
    /// [`FromColumn`](crate::FromColumn) impl.
    fn from_row(row: &dyn Row) -> Result<Self>;

    /// `(column, value)` pairs for an `INSERT`. Auto-increment primary keys
    /// are omitted so the database assigns them.
    fn insert_values(&self) -> Vec<(&'static str, Value)>;

    /// `(column, value)` pairs for an `UPDATE` — every column except the
    /// primary key.
    fn update_values(&self) -> Vec<(&'static str, Value)>;

    /// This instance's primary-key value, for `WHERE pk = ?`.
    fn pk_value(&self) -> Value;

    /// One column's value by name, or `None` if the entity has no such column.
    ///
    /// What relationship loading needs: given a slice of parents, collect the
    /// key column to look children up by. Defaults to a scan of
    /// [`update_values`](Self::update_values) plus the primary key, which is
    /// correct for every entity and costs one allocation of the row's values.
    fn value_of(&self, column: &str) -> Option<Value> {
        if column == Self::primary_key() {
            return Some(self.pk_value());
        }
        self.update_values().into_iter().find(|(name, _)| *name == column).map(|(_, value)| value)
    }
}
