//! A declarative, dialect-portable schema-change language — the *single* way
//! to express an **incremental** migration step.
//!
//! [`crate::schema`] renders an [`Entity`]'s whole `CREATE TABLE` from its
//! derived metadata: the create-from-scratch path. But a live schema *evolves*
//! — columns are added, indexes created, tables renamed or dropped — and those
//! changes can't come from the entity alone (the entity only describes the
//! *current* shape, not the diff from the last one). This module is that
//! incremental complement: a small builder, [`Migration`], that accumulates
//! operations and renders each to the executor's dialect **at apply time**, so
//! one migration definition runs on MySQL, Postgres, SQLite, and Cloudflare D1
//! alike — the "write it once" promise extended from CRUD to schema evolution.
//!
//! A [`Migration`] *is* a [`migrate::Migration`](crate::migrate::Migration)
//! step, so you compose it straight into a [`Migrator`](crate::migrate::Migrator)
//! next to [`create_table`](crate::migrate::Migrator::create_table) steps:
//!
//! ```ignore
//! use rainier_orm::ddl::{Column, Migration};
//! use rainier_orm::{migrate::Migrator, ColumnType};
//!
//! let m = Migration::new("0002_users_add_phone")
//!     .add_column("users", Column::new("phone", ColumnType::Text))
//!     .create_index("idx_users_phone", "users", ["phone"]);
//!
//! Migrator::new()
//!     .create_table::<User>("0001_users")   // create-from-entity
//!     .add(m)                               // incremental change
//!     .run(&exec)
//!     .await?;
//! ```
//!
//! **Portability.** Every built-in operation lowers to SQL that MySQL,
//! Postgres, and SQLite/D1 all accept (one statement per operation — SQLite
//! permits a single change per `ALTER TABLE`). The exceptions are column *type*
//! changes (`MODIFY`/`ALTER COLUMN` has no portable, SQLite-supported form) —
//! reach for [`raw`](Migration::raw) there, scoped to the dialect that needs it.

use crate::{schema, ColumnType, Dialect, Entity};
use sea_query::{Alias, ColumnDef, Index, Table, Value};

/// A column to **add**, described portably (the dialect-specific type rendering
/// is deferred to apply time, exactly as [`crate::schema`] does for `CREATE`).
///
/// New columns default to **nullable**: adding a `NOT NULL` column to a table
/// that already has rows fails unless a default backfills them, so call
/// [`not_null`](Self::not_null) together with [`default`](Self::default).
pub struct Column {
    name: String,
    ty: ColumnType,
    nullable: bool,
    default: Option<Value>,
    keyed: bool,
}

impl Column {
    /// A nullable column of `ty`.
    pub fn new(name: impl Into<String>, ty: ColumnType) -> Self {
        Self { name: name.into(), ty, nullable: true, default: None, keyed: false }
    }

    /// Mark the column `NOT NULL`. Pair with [`default`](Self::default) when the
    /// table may already hold rows.
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    /// Mark the column nullable (the default).
    pub fn null(mut self) -> Self {
        self.nullable = true;
        self
    }

    /// A column default — also backfills existing rows when adding `NOT NULL`.
    pub fn default(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Hint that this (text) column will be indexed, so it renders as a bounded
    /// `VARCHAR` rather than `TEXT` on MySQL (which can't index an unbounded
    /// `TEXT`). No effect on SQLite/D1, where both are `TEXT` affinity.
    pub fn keyed(mut self) -> Self {
        self.keyed = true;
        self
    }

    fn to_def(&self) -> ColumnDef {
        let mut def = ColumnDef::new(Alias::new(self.name.clone()));
        schema::apply_type(&mut def, self.ty, self.keyed);
        if self.nullable {
            def.null();
        } else {
            def.not_null();
        }
        if let Some(v) = &self.default {
            def.default(v.clone());
        }
        def
    }
}

/// An ordered set of schema operations under one migration `name` — the unit
/// the [`Migrator`](crate::migrate::Migrator) records in `_orm_migrations` and
/// applies once.
///
/// Build it fluently; it renders to dialect SQL only when applied (or via
/// [`render`](Self::render) for inspection/tests). Each builder method appends
/// one operation, and operations apply in call order.
pub struct Migration {
    name: &'static str,
    // Each op renders itself to zero or more statements for a given dialect.
    // A boxed renderer (rather than a stored sea-query statement) lets the
    // entity-create and dialect-scoped-raw ops live in the same list as the
    // plain schema statements, all resolved at the executor's dialect.
    ops: Vec<Box<dyn Fn(Dialect) -> Vec<String>>>,
}

impl Migration {
    /// A new, empty migration identified by `name` (the ordering + dedupe key).
    pub fn new(name: &'static str) -> Self {
        Self { name, ops: Vec::new() }
    }

    /// This migration's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    fn push_stmt<S>(mut self, stmt: S) -> Self
    where
        S: sea_query::SchemaStatementBuilder + 'static,
    {
        self.ops.push(Box::new(move |d: Dialect| vec![d.build_schema(&stmt)]));
        self
    }

    /// Create `E`'s table **and its derived indexes**, idempotently
    /// (`CREATE TABLE IF NOT EXISTS`) — the same DDL
    /// [`Migrator::create_table`](crate::migrate::Migrator::create_table)
    /// emits, available here so a create and subsequent alters can share one
    /// migration step.
    pub fn create_table<E: Entity + 'static>(mut self) -> Self {
        self.ops.push(Box::new(|d| schema::schema_ddl::<E>(d)));
        self
    }

    /// Append an explicit `sea_query` schema statement — a `CREATE TABLE`,
    /// `CREATE INDEX`, `ALTER TABLE`, … built directly with the re-exported
    /// [`sea_query`] module. The bridge for schemas authored as
    /// sea-query statements rather than through the typed ops above (or derived
    /// from an [`Entity`]); rendered for the executor's dialect at apply time,
    /// so one definition still runs on every backend.
    pub fn statement<S>(self, stmt: S) -> Self
    where
        S: sea_query::SchemaStatementBuilder + 'static,
    {
        self.push_stmt(stmt)
    }

    /// Add a column to `table`. See [`Column`] for nullability/default rules.
    pub fn add_column(self, table: &str, column: Column) -> Self {
        let mut def = column.to_def();
        let mut stmt = Table::alter();
        stmt.table(Alias::new(table)).add_column(&mut def);
        self.push_stmt(stmt)
    }

    /// Drop `column` from `table` (SQLite ≥ 3.35 / modern D1 support this).
    pub fn drop_column(self, table: &str, column: &str) -> Self {
        let mut stmt = Table::alter();
        stmt.table(Alias::new(table)).drop_column(Alias::new(column));
        self.push_stmt(stmt)
    }

    /// Rename `from` → `to` on `table`.
    pub fn rename_column(self, table: &str, from: &str, to: &str) -> Self {
        let mut stmt = Table::alter();
        stmt.table(Alias::new(table)).rename_column(Alias::new(from), Alias::new(to));
        self.push_stmt(stmt)
    }

    /// Rename a table.
    pub fn rename_table(self, from: &str, to: &str) -> Self {
        let mut stmt = Table::rename();
        stmt.table(Alias::new(from), Alias::new(to));
        self.push_stmt(stmt)
    }

    /// Drop a table if it exists.
    pub fn drop_table(self, table: &str) -> Self {
        let mut stmt = Table::drop();
        stmt.table(Alias::new(table)).if_exists();
        self.push_stmt(stmt)
    }

    /// Create a non-unique secondary index (`IF NOT EXISTS`).
    pub fn create_index<'c, I>(self, name: &str, table: &str, columns: I) -> Self
    where
        I: IntoIterator<Item = &'c str>,
    {
        self.index(name, table, columns, false)
    }

    /// Create a `UNIQUE` index (`IF NOT EXISTS`).
    pub fn create_unique_index<'c, I>(self, name: &str, table: &str, columns: I) -> Self
    where
        I: IntoIterator<Item = &'c str>,
    {
        self.index(name, table, columns, true)
    }

    fn index<'c, I>(self, name: &str, table: &str, columns: I, unique: bool) -> Self
    where
        I: IntoIterator<Item = &'c str>,
    {
        let mut stmt = Index::create();
        stmt.name(name).table(Alias::new(table)).if_not_exists();
        if unique {
            stmt.unique();
        }
        for c in columns {
            stmt.col(Alias::new(c));
        }
        self.push_stmt(stmt)
    }

    /// Drop a named index off `table`.
    pub fn drop_index(self, name: &str, table: &str) -> Self {
        let mut stmt = Index::drop();
        stmt.name(name).table(Alias::new(table));
        self.push_stmt(stmt)
    }

    /// Raw SQL emitted **only** for `dialect` — the escape hatch for anything
    /// the portable ops can't express (e.g. a MySQL `MODIFY COLUMN`, a Postgres
    /// `ALTER COLUMN … TYPE`). Ignored on every other dialect, so a migration
    /// can carry per-dialect variants of the same change side by side.
    pub fn raw(mut self, dialect: Dialect, sql: impl Into<String>) -> Self {
        let sql = sql.into();
        self.ops.push(Box::new(move |d| if d == dialect { vec![sql.clone()] } else { Vec::new() }));
        self
    }

    /// Raw SQL emitted for **every** dialect — only sound for statements that
    /// are byte-identical across backends. Prefer the typed ops or
    /// [`raw`](Self::raw); this is the last resort.
    pub fn raw_all(mut self, sql: impl Into<String>) -> Self {
        let sql = sql.into();
        self.ops.push(Box::new(move |_| vec![sql.clone()]));
        self
    }

    /// Every statement this migration runs on `dialect`, in apply order — for
    /// inspection, tests, or dumping a `.sql` file.
    pub fn render(&self, dialect: Dialect) -> Vec<String> {
        self.ops.iter().flat_map(|op| op(dialect)).collect()
    }
}

impl<X: crate::Executor> crate::migrate::Migration<X> for Migration {
    fn name(&self) -> &'static str {
        self.name
    }

    fn up<'a>(&'a self, db: &'a X) -> crate::migrate::StepFuture<'a> {
        Box::pin(async move {
            for sql in self.render(db.dialect()) {
                db.execute(&sql, Vec::new()).await?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_column_renders_per_dialect() {
        let m = Migration::new("0002_add_phone")
            .add_column("users", Column::new("phone", ColumnType::Text).keyed());
        let sqlite = m.render(Dialect::Sqlite).join("\n");
        let mysql = m.render(Dialect::MySql).join("\n");
        assert!(sqlite.contains("ALTER TABLE"), "{sqlite}");
        assert!(sqlite.contains("phone"));
        // MySQL renders a keyed text column as a bounded VARCHAR, SQLite as text.
        assert!(mysql.contains("varchar") || mysql.contains("VARCHAR"), "{mysql}");
        assert!(mysql.contains('`'), "mysql quoting");
        assert!(sqlite.contains('"'), "sqlite quoting");
    }

    #[test]
    fn not_null_with_default_backfills() {
        let m = Migration::new("0003_add_flag").add_column(
            "users",
            Column::new("verified", ColumnType::Bool).not_null().default(false),
        );
        let sql = m.render(Dialect::Sqlite).join("\n");
        assert!(sql.to_uppercase().contains("NOT NULL"), "{sql}");
        assert!(sql.to_uppercase().contains("DEFAULT"), "{sql}");
    }

    #[test]
    fn index_ops_render() {
        let create = Migration::new("a")
            .create_index("idx_users_phone", "users", ["phone"])
            .render(Dialect::Sqlite)
            .join("\n");
        assert!(create.contains("CREATE INDEX"), "{create}");
        assert!(create.contains("idx_users_phone"));

        let uniq = Migration::new("b")
            .create_unique_index("uq_users_phone", "users", ["phone", "region"])
            .render(Dialect::MySql)
            .join("\n");
        assert!(uniq.to_uppercase().contains("UNIQUE"), "{uniq}");

        let drop = Migration::new("c")
            .drop_index("idx_users_phone", "users")
            .render(Dialect::Sqlite)
            .join("\n");
        assert!(drop.to_uppercase().contains("DROP INDEX"), "{drop}");
    }

    #[test]
    fn rename_and_drop_table() {
        let rename = Migration::new("a")
            .rename_table("old_users", "users")
            .render(Dialect::MySql)
            .join("\n");
        assert!(rename.to_uppercase().contains("RENAME"), "{rename}");

        let drop = Migration::new("b").drop_table("dead").render(Dialect::Sqlite).join("\n");
        assert!(drop.to_uppercase().contains("DROP TABLE"), "{drop}");
        assert!(drop.to_uppercase().contains("IF EXISTS"), "{drop}");
    }

    #[test]
    fn statement_bridges_sea_query() {
        use sea_query::{Alias, ColumnDef, Table};
        let mut create = Table::create();
        create
            .table(Alias::new("widgets"))
            .if_not_exists()
            .col(ColumnDef::new(Alias::new("id")).big_integer().primary_key());
        let m = Migration::new("0001").statement(create);
        let sqlite = m.render(Dialect::Sqlite).join("\n");
        assert!(sqlite.to_uppercase().contains("CREATE TABLE"), "{sqlite}");
        assert!(sqlite.contains("widgets"));
        // Same statement, MySQL quoting.
        assert!(m.render(Dialect::MySql).join("\n").contains('`'));
    }

    #[test]
    fn raw_is_dialect_scoped() {
        let m = Migration::new("a")
            .raw(Dialect::MySql, "ALTER TABLE t MODIFY c TEXT")
            .raw(Dialect::Sqlite, "-- noop on sqlite");
        assert_eq!(m.render(Dialect::MySql), vec!["ALTER TABLE t MODIFY c TEXT"]);
        assert_eq!(m.render(Dialect::Sqlite), vec!["-- noop on sqlite"]);
        assert!(m.render(Dialect::Postgres).is_empty());
    }

    #[test]
    fn ops_apply_in_call_order() {
        let m = Migration::new("ordered")
            .add_column("t", Column::new("a", ColumnType::Int))
            .add_column("t", Column::new("b", ColumnType::Int));
        let rendered = m.render(Dialect::Sqlite);
        assert_eq!(rendered.len(), 2);
        assert!(rendered[0].contains("\"a\""), "{:?}", rendered);
        assert!(rendered[1].contains("\"b\""), "{:?}", rendered);
    }
}
