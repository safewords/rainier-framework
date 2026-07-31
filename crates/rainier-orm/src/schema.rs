//! Generic DDL: turn an [`Entity`]'s metadata into `CREATE TABLE` +
//! `CREATE INDEX` statements, rendered for any [`Dialect`].
//!
//! The same metadata the derive produces for CRUD also describes the schema —
//! columns, a primary key, single-column `UNIQUE`s, foreign keys (inline), and
//! secondary indexes (separate statements). So callers can stand up the
//! backing schema for a struct without hand-writing a migration — handy for
//! tests, ephemeral D1 databases, and bootstrap. Production migrations can
//! still be authored explicitly; this is the convenience path.
//!
//! Use [`schema_ddl`] to get every statement in dependency order (table first,
//! then its indexes); [`create_table_ddl`] returns just the table.
//!
//! **Foreign keys & SQLite/D1.** FK clauses are emitted inline in the
//! `CREATE TABLE` (the only form SQLite accepts). SQLite/D1 do not *enforce*
//! them unless `PRAGMA foreign_keys = ON` is set on the connection — run that
//! once after connecting if you want enforcement.

use crate::{ColumnType, Dialect, Entity, RefAction};
use sea_query::{
    Alias, ColumnDef, ForeignKey, ForeignKeyAction, ForeignKeyCreateStatement, Index, Table,
    TableCreateStatement,
};

/// The type a column is actually declared as.
///
/// Identical to the declared one except for an **auto-increment** key, where
/// an unsigned type is downgraded to its signed equivalent. Three reasons, and
/// the first is decisive:
///
/// 1. Postgres has no unsigned integers, and sea-query *panics* rather than
///    rendering `BIGSERIAL` for a `BigUnsigned` marked auto-increment. A `u64`
///    key would make the table impossible to create there at all.
/// 2. A sequence is signed everywhere that has one, so the portable range of
///    a generated key is a positive `i64` regardless of what the struct says.
/// 3. `BIGINT` still holds 9.2 × 10^18 rows. The lost half of the `u64` range
///    is not a range anyone reaches.
///
/// The entity's field stays `u64`; only the column is narrower. Decoding a
/// positive `i64` into it is lossless.
fn effective_type(col: &crate::column::ColumnSpec) -> ColumnType {
    if !col.auto_increment {
        return col.ty;
    }

    match col.ty {
        ColumnType::BigUint => ColumnType::BigInt,
        ColumnType::Uint => ColumnType::Int,
        other => other,
    }
}

/// Build the `CREATE TABLE` statement for `E` — columns, primary key,
/// single-column uniques, and inline foreign keys. Secondary indexes are
/// separate; see [`create_index_ddls`].
pub fn create_table_stmt<E: Entity>() -> TableCreateStatement {
    let mut stmt = Table::create();
    stmt.table(Alias::new(E::table())).if_not_exists();

    for col in E::columns() {
        // "Keyed" = the column participates in any index/constraint: the primary
        // key, a single-column unique, or any (single or composite) secondary
        // index. MySQL can't index a `TEXT` column without a prefix length, so a
        // keyed text column renders as `VARCHAR` instead (see `apply_type`).
        let keyed =
            col.pk || col.unique || E::indexes().iter().any(|ix| ix.columns.contains(&col.name));
        let mut def = ColumnDef::new(Alias::new(col.name));
        apply_type(&mut def, effective_type(col), keyed);

        if col.pk {
            def.primary_key();
            if col.auto_increment {
                def.auto_increment();
            }
        }
        if col.nullable {
            def.null();
        } else {
            def.not_null();
        }
        if col.unique {
            def.unique_key();
        }
        stmt.col(&mut def);
    }

    for fk in E::foreign_keys() {
        stmt.foreign_key(&mut foreign_key_stmt::<E>(fk));
    }
    stmt
}

/// Render `E`'s `CREATE TABLE` (without secondary indexes) as SQL for
/// `dialect`.
pub fn create_table_ddl<E: Entity>(dialect: Dialect) -> String {
    dialect.build_schema(&create_table_stmt::<E>())
}

/// Render `E`'s secondary indexes as `CREATE [UNIQUE] INDEX` statements for
/// `dialect`, one string each.
pub fn create_index_ddls<E: Entity>(dialect: Dialect) -> Vec<String> {
    E::indexes()
        .iter()
        .map(|ix| {
            let mut stmt = Index::create();
            stmt.name(ix.name).table(Alias::new(E::table())).if_not_exists();
            if ix.unique {
                stmt.unique();
            }
            for c in ix.columns {
                stmt.col(Alias::new(*c));
            }
            dialect.build_schema(&stmt)
        })
        .collect()
}

/// Every DDL statement to create `E`'s schema, in dependency order: the table
/// (with its inline uniques and foreign keys) first, then each secondary
/// index. Execute them in order.
pub fn schema_ddl<E: Entity>(dialect: Dialect) -> Vec<String> {
    let mut out = Vec::with_capacity(1 + E::indexes().len());
    out.push(create_table_ddl::<E>(dialect));
    out.extend(create_index_ddls::<E>(dialect));
    out
}

fn foreign_key_stmt<E: Entity>(fk: &crate::ForeignKeySpec) -> ForeignKeyCreateStatement {
    let mut stmt = ForeignKey::create();
    stmt.name(fk.name).from_tbl(Alias::new(E::table()));
    for c in fk.columns {
        stmt.from_col(Alias::new(*c));
    }
    stmt.to_tbl(Alias::new(fk.foreign_table));
    for c in fk.foreign_columns {
        stmt.to_col(Alias::new(*c));
    }
    if let Some(a) = fk.on_delete {
        stmt.on_delete(map_action(a));
    }
    if let Some(a) = fk.on_update {
        stmt.on_update(map_action(a));
    }
    stmt
}

fn map_action(a: RefAction) -> ForeignKeyAction {
    match a {
        RefAction::Cascade => ForeignKeyAction::Cascade,
        RefAction::Restrict => ForeignKeyAction::Restrict,
        RefAction::SetNull => ForeignKeyAction::SetNull,
        RefAction::SetDefault => ForeignKeyAction::SetDefault,
        RefAction::NoAction => ForeignKeyAction::NoAction,
    }
}

/// Apply the column's SQL type to a sea-query [`ColumnDef`]. sea-query maps
/// each of these to the per-dialect concrete type when rendered (e.g.
/// `BigUnsigned` → `BIGINT UNSIGNED` on MySQL, `INTEGER` on SQLite).
///
/// `keyed` is whether the column participates in an index/constraint: a `Text`
/// column that does is rendered as `VARCHAR(255)` instead of `TEXT`, because
/// MySQL rejects indexing a `BLOB`/`TEXT` column without a prefix length. On
/// SQLite/D1 a `VARCHAR(255)` is just `TEXT` affinity, so nothing is lost there.
///
/// `pub(crate)` so the incremental DDL builder ([`crate::ddl`]) renders an
/// added column's type identically to how `CREATE TABLE` rendered it.
pub fn apply_type(def: &mut ColumnDef, ty: ColumnType, keyed: bool) {
    /// Longest VARCHAR that stays under InnoDB's index key-prefix limit at
    /// utf8mb4 (255 × 4 = 1020 B < 3072 B), so a keyed string is always indexable.
    const KEYED_TEXT_LEN: u32 = 255;
    match ty {
        ColumnType::Bool => def.boolean(),
        ColumnType::Int => def.integer(),
        ColumnType::BigInt => def.big_integer(),
        ColumnType::Uint => def.unsigned(),
        ColumnType::BigUint => def.big_unsigned(),
        ColumnType::Double => def.double(),
        ColumnType::Text if keyed => def.string_len(KEYED_TEXT_LEN),
        ColumnType::Text => def.text(),
        ColumnType::Binary => def.blob(),
        ColumnType::Timestamp => def.timestamp(),
        ColumnType::Date => def.date(),
    };
}

#[cfg(test)]
mod auto_increment_tests {
    use super::*;

    #[derive(crate::Entity, Clone)]
    #[orm(table = "widgets")]
    struct Widget {
        #[orm(pk, auto_increment)]
        id: u64,
        name: String,
    }

    #[derive(crate::Entity, Clone)]
    #[orm(table = "counters")]
    struct Counter {
        #[orm(pk)]
        id: u64,
        hits: u64,
    }

    #[test]
    fn an_auto_increment_u64_key_renders_on_every_dialect() {
        // sea-query panics on `BigUnsigned` + auto-increment for Postgres, so
        // before the downgrade this table simply could not be created there.
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let ddl = create_table_ddl::<Widget>(dialect);
            assert!(ddl.contains("widgets"), "{dialect:?}: {ddl}");
        }
    }

    #[test]
    fn postgres_gets_a_sequence_backed_key() {
        let ddl = create_table_ddl::<Widget>(Dialect::Postgres);
        assert!(ddl.contains("serial"), "{ddl}");
    }

    #[test]
    fn a_non_generated_unsigned_column_keeps_its_type() {
        // The downgrade is scoped to auto-increment. An ordinary `u64` column
        // is still unsigned where the backend has unsigned types.
        let ddl = create_table_ddl::<Counter>(Dialect::MySql);
        assert!(ddl.to_lowercase().contains("unsigned"), "{ddl}");
    }
}
