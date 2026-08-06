//! The schema builder — a table described once, rendered per dialect.
//!
//! ```ignore
//! Blueprint::create("posts", |table| {
//!     table.id();
//!     table.string("slug").unique();
//!     table.string("title");
//!     table.text("body");
//!     table.boolean("published").default(false);
//!     table.foreign_id("author_id").constrained_on("users").cascade_on_delete();
//!     table.timestamps();
//!
//!     table.index(["published", "created_at"]);
//! });
//! ```
//!
//! # Why this exists
//!
//! Writing `CREATE TABLE` by hand in a migration means writing it *per engine*:
//! `AUTOINCREMENT` on SQLite, `AUTO_INCREMENT` on MySQL and `BIGSERIAL` on
//! Postgres; `BLOB` against `BYTEA`; `CREATE INDEX IF NOT EXISTS` everywhere
//! except MySQL, which rejects it; `DROP INDEX i` against `DROP INDEX i ON t`.
//! Translating that is the entire job of a DBAL, and asking the application to
//! do it is asking it to do the job twice.
//!
//! So a table is described **once**, as data, and lowered to the dialect the
//! executor reports at apply time.
//!
//! # It also knows how to undo itself
//!
//! Every blueprint can produce the statements that reverse it, which is what
//! lets a migration satisfy the [`Down`](crate::migrate) half of its contract
//! without the author writing the same schema backwards — the version of it
//! that goes stale first.

use sea_query::{
    Alias, ColumnDef, ForeignKey, ForeignKeyAction, Index, IndexCreateStatement, Table,
    TableCreateStatement, Value,
};

use crate::{schema, ColumnType, Dialect};

/// The default length of a `string` column:
/// the largest a MySQL utf8mb4 index could cover in the 767-byte prefix
/// era, and nobody has had cause to change it since.
pub const DEFAULT_STRING_LEN: u32 = 255;

/// What a column holds.
///
/// The shared subset delegates to the same mapping an
/// [`Entity`](crate::Entity)'s columns use, so a table built here and a table
/// derived from a model cannot disagree about what `Text` renders as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnKind {
    /// One of the types an entity field can have.
    Basic(ColumnType),
    /// `VARCHAR(n)` — a length an entity cannot express.
    Varchar(u32),
    /// A JSON document. `JSONB` on Postgres, `JSON` on MySQL, `TEXT` on SQLite.
    Json,
    /// A fixed-point number.
    ///
    /// `DECIMAL(p, s)` on MySQL and Postgres. **SQLite has no such type** and
    /// renders `REAL`, which is a float — so an amount stored there is subject
    /// to the rounding this type exists to avoid. Store money as integer minor
    /// units if SQLite is a target you care about.
    Decimal {
        /// Total digits.
        precision: u32,
        /// Digits after the point.
        scale: u32,
    },
}

impl ColumnKind {
    fn apply(&self, def: &mut ColumnDef) {
        match self {
            ColumnKind::Basic(ty) => schema::apply_type(def, *ty, false),
            ColumnKind::Varchar(len) => {
                def.string_len(*len);
            }
            ColumnKind::Json => {
                def.json_binary();
            }
            ColumnKind::Decimal { precision, scale } => {
                def.decimal_len(*precision, *scale);
            }
        }
    }
}

/// What to do to rows on the other side when a referenced row goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Delete or update them too.
    Cascade,
    /// Refuse, if any exist.
    Restrict,
    /// Set the referencing column to `NULL`. Needs a nullable column.
    SetNull,
    /// Do nothing, and let the constraint be checked at the end of the
    /// statement.
    NoAction,
}

impl Action {
    fn to_sea(self) -> ForeignKeyAction {
        match self {
            Action::Cascade => ForeignKeyAction::Cascade,
            Action::Restrict => ForeignKeyAction::Restrict,
            Action::SetNull => ForeignKeyAction::SetNull,
            Action::NoAction => ForeignKeyAction::NoAction,
        }
    }
}

/// One column, described.
///
/// Returned by every `Blueprint` column method so modifiers chain:
///
/// ```ignore
/// table.string("email").unique();
/// table.text("bio").nullable();
/// table.integer("hits").default(0);
/// ```
#[derive(Debug, Clone)]
pub struct Column {
    name: String,
    kind: ColumnKind,
    nullable: bool,
    default: Option<Value>,
    auto_increment: bool,
    primary: bool,
    unique: bool,
    indexed: bool,
    /// Set by `if_not_exists()`. See that method for why an add-column
    /// migration needs it at all.
    if_not_exists: bool,
    /// Set by `foreign_id().constrained_on(..)`, so the common case is one
    /// call rather than a column and a matching constraint.
    references: Option<(String, String, Option<Action>, Option<Action>)>,
}

impl Column {
    fn new(name: impl Into<String>, kind: ColumnKind) -> Self {
        Self {
            name: name.into(),
            kind,
            nullable: false,
            default: None,
            auto_increment: false,
            primary: false,
            unique: false,
            indexed: false,
            if_not_exists: false,
            references: None,
        }
    }

    /// The column's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Allow `NULL`.
    ///
    /// Columns are `NOT NULL` by default, which is the opposite of SQL's
    /// default — nullability should be a decision
    /// somebody made, not one they got by omission.
    pub fn nullable(&mut self) -> &mut Self {
        self.nullable = true;
        self
    }

    /// A default value. Also backfills existing rows when a `NOT NULL` column
    /// is added to a table that already has some.
    pub fn default(&mut self, value: impl Into<Value>) -> &mut Self {
        self.default = Some(value.into());
        self
    }

    /// Make it the primary key.
    pub fn primary(&mut self) -> &mut Self {
        self.primary = true;
        self
    }

    /// Let the database assign it. Implies [`primary`](Self::primary).
    pub fn auto_increment(&mut self) -> &mut Self {
        self.auto_increment = true;
        self.primary = true;
        self
    }

    /// A single-column `UNIQUE` constraint.
    pub fn unique(&mut self) -> &mut Self {
        self.unique = true;
        self
    }

    /// A single-column index.
    /// Tolerate the column already existing.
    ///
    /// # The problem this exists for
    ///
    /// `Step::create_table::<E>` renders the **current** model, so a fresh
    /// install gets today's schema in one statement. Every add-column
    /// migration written afterwards then replays against a table that already
    /// has the column, and fails:
    ///
    /// ```text
    /// duplicate column name: internal
    /// ```
    ///
    /// So the same migration is required against a database that predates the
    /// column and impossible against one created after it. This makes it a
    /// no-op in the second case, which is the only way both can hold.
    ///
    /// # Not free on every dialect
    ///
    /// MySQL/MariaDB and Postgres have `ADD COLUMN IF NOT EXISTS` and it is
    /// rendered. SQLite has no such guard and no conditional DDL, so the
    /// statement is **skipped entirely** there.
    ///
    /// That is correct for the case SQLite is used in here — a database
    /// created from the model, which therefore already has the column — and
    /// wrong for a long-lived SQLite database that predates it. Say so out
    /// loud rather than let it be discovered: if you need to add a column to
    /// an existing SQLite database, write the `ALTER` as a raw step for that
    /// dialect.
    pub fn if_not_exists(&mut self) -> &mut Self {
        self.if_not_exists = true;
        self
    }

    pub fn index(&mut self) -> &mut Self {
        self.indexed = true;
        self
    }

    /// Point this column at `table`'s primary key.
    ///
    /// The referenced column defaults to `id`; say otherwise with
    /// [`references`](Self::references).
    pub fn constrained_on(&mut self, table: impl Into<String>) -> &mut Self {
        self.references = Some((table.into(), "id".to_string(), None, None));
        self
    }

    /// Point this column at `table`.`column`.
    pub fn references(&mut self, table: impl Into<String>, column: impl Into<String>) -> &mut Self {
        self.references = Some((table.into(), column.into(), None, None));
        self
    }

    /// Delete the referencing rows when the referenced row goes.
    pub fn cascade_on_delete(&mut self) -> &mut Self {
        self.on_delete(Action::Cascade)
    }

    /// `NULL` the referencing column when the referenced row goes. The column
    /// has to be [`nullable`](Self::nullable).
    pub fn null_on_delete(&mut self) -> &mut Self {
        self.on_delete(Action::SetNull)
    }

    /// What happens to this row when the referenced one is deleted.
    pub fn on_delete(&mut self, action: Action) -> &mut Self {
        if let Some(reference) = &mut self.references {
            reference.2 = Some(action);
        }
        self
    }

    /// What happens to this row when the referenced key is updated.
    pub fn on_update(&mut self, action: Action) -> &mut Self {
        if let Some(reference) = &mut self.references {
            reference.3 = Some(action);
        }
        self
    }

    /// The column definition, ready for a `CREATE` or an `ALTER`.
    fn to_def(&self) -> ColumnDef {
        let mut def = ColumnDef::new(Alias::new(self.name.clone()));

        // An auto-increment `BIGINT UNSIGNED` is not a thing Postgres will
        // create, and the key it generates is a positive `i64` on every
        // backend anyway — so the unsigned types narrow when they are keys.
        // The same rule the entity path applies, for the same reason.
        let kind = match (self.auto_increment, self.kind) {
            (true, ColumnKind::Basic(ColumnType::BigUint)) => ColumnKind::Basic(ColumnType::BigInt),
            (true, ColumnKind::Basic(ColumnType::Uint)) => ColumnKind::Basic(ColumnType::Int),
            (_, kind) => kind,
        };
        kind.apply(&mut def);

        if self.primary {
            def.primary_key();
        }
        if self.auto_increment {
            def.auto_increment();
        }
        if self.nullable {
            def.null();
        } else {
            def.not_null();
        }
        if let Some(value) = &self.default {
            def.default(value.clone());
        }
        if self.unique {
            def.unique_key();
        }
        def
    }
}

/// One index, described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    name: String,
    columns: Vec<String>,
    unique: bool,
}

impl IndexDef {
    /// The index's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The columns it covers.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Whether it enforces uniqueness.
    pub fn is_unique(&self) -> bool {
        self.unique
    }

    /// Name it something other than the convention.
    pub fn named(&mut self, name: impl Into<String>) -> &mut Self {
        self.name = name.into();
        self
    }

    fn to_stmt(&self, table: &str) -> IndexCreateStatement {
        let mut stmt = Index::create();
        stmt.name(&self.name).table(Alias::new(table)).if_not_exists();
        if self.unique {
            stmt.unique();
        }
        for column in &self.columns {
            stmt.col(Alias::new(column.clone()));
        }
        stmt
    }
}

/// Conventional index naming: `posts_author_id_index`, `users_email_unique`.
///
/// Predictable rather than clever, because a `down` has to be able to name the
/// index it is dropping without being told.
fn conventional_name(table: &str, columns: &[String], suffix: &str) -> String {
    format!("{table}_{}_{suffix}", columns.join("_"))
}

/// A table being created.
#[derive(Debug, Clone)]
pub struct Blueprint {
    table: String,
    columns: Vec<Column>,
    indexes: Vec<IndexDef>,
    primary: Option<Vec<String>>,
    if_not_exists: bool,
}

impl Blueprint {
    /// A blueprint for `table`.
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            columns: Vec::new(),
            indexes: Vec::new(),
            primary: None,
            if_not_exists: true,
        }
    }

    /// Describe `table` with `build`, and hand back the blueprint.
    pub fn create(table: impl Into<String>, build: impl FnOnce(&mut Blueprint)) -> Self {
        let mut blueprint = Self::new(table);
        build(&mut blueprint);
        blueprint
    }

    /// The table's name.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Emit a plain `CREATE TABLE`, so creating one that exists is an error
    /// rather than a no-op.
    ///
    /// The default is `IF NOT EXISTS`, which makes a migration safe to re-run
    /// by hand against a database somebody has already touched.
    pub fn strict(&mut self) -> &mut Self {
        self.if_not_exists = false;
        self
    }

    // --- columns -----------------------------------------------------------

    /// A column of any kind. The typed helpers below all come through here.
    pub fn column(&mut self, name: impl Into<String>, kind: ColumnKind) -> &mut Column {
        self.columns.push(Column::new(name, kind));
        self.columns.last_mut().expect("just pushed")
    }

    /// An auto-incrementing primary key named `id`.
    pub fn id(&mut self) -> &mut Column {
        self.big_increments("id")
    }

    /// An auto-incrementing 64-bit primary key.
    pub fn big_increments(&mut self, name: impl Into<String>) -> &mut Column {
        let column = self.column(name, ColumnKind::Basic(ColumnType::BigUint));
        column.auto_increment();
        column
    }

    /// An auto-incrementing 32-bit primary key.
    pub fn increments(&mut self, name: impl Into<String>) -> &mut Column {
        let column = self.column(name, ColumnKind::Basic(ColumnType::Uint));
        column.auto_increment();
        column
    }

    /// `VARCHAR(255)`.
    pub fn string(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Varchar(DEFAULT_STRING_LEN))
    }

    /// `VARCHAR(len)`.
    pub fn string_len(&mut self, name: impl Into<String>, len: u32) -> &mut Column {
        self.column(name, ColumnKind::Varchar(len))
    }

    /// Unbounded text.
    ///
    /// **Not indexable on MySQL** without a prefix length, so reach for
    /// [`string`](Self::string) for anything you will search or sort by.
    pub fn text(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Text))
    }

    /// A 32-bit signed integer.
    pub fn integer(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Int))
    }

    /// A 64-bit signed integer.
    pub fn big_integer(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::BigInt))
    }

    /// A 32-bit unsigned integer.
    pub fn unsigned_integer(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Uint))
    }

    /// A 64-bit unsigned integer.
    pub fn unsigned_big_integer(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::BigUint))
    }

    /// A foreign key column: unsigned 64-bit and indexed, ready for
    /// [`constrained_on`](Column::constrained_on).
    ///
    /// ```ignore
    /// table.foreign_id("author_id").constrained_on("users").cascade_on_delete();
    /// ```
    pub fn foreign_id(&mut self, name: impl Into<String>) -> &mut Column {
        let column = self.column(name, ColumnKind::Basic(ColumnType::BigUint));
        column.index();
        column
    }

    /// A boolean. An integer on SQLite and MySQL, which have no boolean type.
    pub fn boolean(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Bool))
    }

    /// A double-precision float. Not for money — see
    /// [`decimal`](Self::decimal).
    pub fn double(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Double))
    }

    /// A fixed-point number, for money and anything else that must not round.
    ///
    /// Read [`ColumnKind::Decimal`] before using it on SQLite, which has no
    /// fixed-point type and will store a float.
    pub fn decimal(&mut self, name: impl Into<String>, precision: u32, scale: u32) -> &mut Column {
        self.column(name, ColumnKind::Decimal { precision, scale })
    }

    /// A timestamp.
    pub fn timestamp(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Timestamp))
    }

    /// A calendar date, with no time.
    pub fn date(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Date))
    }

    /// Raw bytes.
    pub fn binary(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Binary))
    }

    /// A JSON document.
    pub fn json(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Json)
    }

    /// `created_at` and `updated_at`, both nullable.
    ///
    /// Nullable because nothing here writes them for you — a row inserted by a
    /// migration or by hand would otherwise fail. A model that always sets
    /// them can declare its own `NOT NULL` columns instead.
    pub fn timestamps(&mut self) -> &mut Self {
        self.timestamp("created_at").nullable();
        self.timestamp("updated_at").nullable();
        self
    }

    /// A nullable `deleted_at`, for soft deletes.
    pub fn soft_deletes(&mut self) -> &mut Column {
        let column = self.timestamp("deleted_at");
        column.nullable();
        column
    }

    // --- keys and indexes --------------------------------------------------

    /// A composite primary key.
    ///
    /// What a pivot table wants: the pair is the key, so the same link cannot
    /// be inserted twice.
    pub fn primary<I, S>(&mut self, columns: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.primary = Some(columns.into_iter().map(Into::into).collect());
        self
    }

    /// A secondary index, named by convention.
    pub fn index<I, S>(&mut self, columns: I) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push_index(columns, false)
    }

    /// A `UNIQUE` index over one or more columns.
    pub fn unique<I, S>(&mut self, columns: I) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push_index(columns, true)
    }

    fn push_index<I, S>(&mut self, columns: I, unique: bool) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let columns: Vec<String> = columns.into_iter().map(Into::into).collect();
        let suffix = if unique { "unique" } else { "index" };

        self.indexes.push(IndexDef {
            name: conventional_name(&self.table, &columns, suffix),
            columns,
            unique,
        });
        self.indexes.last_mut().expect("just pushed")
    }

    // --- rendering ---------------------------------------------------------

    /// The `CREATE TABLE` statement, before rendering.
    pub fn to_statement(&self) -> TableCreateStatement {
        let mut stmt = Table::create();
        stmt.table(Alias::new(self.table.clone()));
        if self.if_not_exists {
            stmt.if_not_exists();
        }

        for column in &self.columns {
            // An inline `PRIMARY KEY` on a column *and* a composite one is two
            // primary keys, which no engine accepts. The composite wins,
            // because it is the more specific thing to have asked for.
            let mut def = column.to_def();
            if self.primary.is_some() {
                strip_inline_primary(&mut def, column);
            }
            stmt.col(&mut def);
        }

        if let Some(columns) = &self.primary {
            let mut key = Index::create();
            for column in columns {
                key.col(Alias::new(column.clone()));
            }
            stmt.primary_key(&mut key);
        }

        for column in &self.columns {
            let Some((table, referenced, on_delete, on_update)) = &column.references else {
                continue;
            };

            let mut constraint = ForeignKey::create();
            constraint
                .name(conventional_name(&self.table, std::slice::from_ref(&column.name), "foreign"))
                .from(Alias::new(self.table.clone()), Alias::new(column.name.clone()))
                .to(Alias::new(table.clone()), Alias::new(referenced.clone()));

            if let Some(action) = on_delete {
                constraint.on_delete(action.to_sea());
            }
            if let Some(action) = on_update {
                constraint.on_update(action.to_sea());
            }
            stmt.foreign_key(&mut constraint);
        }

        stmt
    }

    /// Every statement that creates this table on `dialect`, in order: the
    /// table, then its indexes.
    pub fn to_sql(&self, dialect: Dialect) -> Vec<String> {
        let mut statements = vec![dialect.build_schema(&self.to_statement())];

        // Single-column `index()` modifiers become real indexes here rather
        // than at declaration time, so `table.string("slug").index()` and
        // `table.index(["slug"])` produce the same name.
        for column in &self.columns {
            if column.indexed {
                let columns = vec![column.name.clone()];
                let index = IndexDef {
                    name: conventional_name(&self.table, &columns, "index"),
                    columns,
                    unique: false,
                };
                statements.push(dialect.build_schema(&index.to_stmt(&self.table)));
            }
        }

        for index in &self.indexes {
            statements.push(dialect.build_schema(&index.to_stmt(&self.table)));
        }
        statements
    }

    /// The statements that undo it: drop the table.
    ///
    /// The indexes go with it — no engine keeps an index on a table that is no
    /// longer there — so one statement is the whole reversal.
    pub fn to_reverse_sql(&self, dialect: Dialect) -> Vec<String> {
        let mut stmt = Table::drop();
        stmt.table(Alias::new(self.table.clone())).if_exists();
        vec![dialect.build_schema(&stmt)]
    }
}

/// A change to a table that already exists.
///
/// ```ignore
/// TableChanges::to("posts", |table| {
///     table.string("subtitle").nullable();
///     table.index(["author_id", "published"]);
///     table.rename_column("body", "content");
/// });
/// ```
///
/// # Reversal
///
/// Each change knows its own opposite, so the migration's `down` is derived
/// rather than written twice: an added column is dropped, a created index is
/// dropped, a rename is renamed back.
///
/// Two are **not** reversible, and say so rather than pretending:
/// [`drop_column`](Self::drop_column), because the type and the data are gone;
/// and [`raw`](Self::raw), because nothing here knows what it did.
#[derive(Debug, Clone)]
pub struct TableChanges {
    table: String,
    changes: Vec<Change>,
}

#[derive(Debug, Clone)]
enum Change {
    AddColumn(Column),
    DropColumn(String),
    RenameColumn(String, String),
    AddIndex(IndexDef),
    DropIndex(IndexDef),
    Raw(Option<Dialect>, String, Option<String>),
}

impl TableChanges {
    /// Changes to `table`.
    pub fn new(table: impl Into<String>) -> Self {
        Self { table: table.into(), changes: Vec::new() }
    }

    /// Describe changes to `table` with `build`.
    pub fn to(table: impl Into<String>, build: impl FnOnce(&mut TableChanges)) -> Self {
        let mut changes = Self::new(table);
        build(&mut changes);
        changes
    }

    /// The table being changed.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Whether anything was asked for.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    // --- adding columns ----------------------------------------------------

    /// Add a column of any kind.
    ///
    /// A column added to a table that already has rows must either be
    /// [`nullable`](Column::nullable) or carry a [`default`](Column::default) —
    /// otherwise the existing rows have no value for it and the engine refuses
    /// the change.
    pub fn column(&mut self, name: impl Into<String>, kind: ColumnKind) -> &mut Column {
        self.changes.push(Change::AddColumn(Column::new(name, kind)));
        match self.changes.last_mut() {
            Some(Change::AddColumn(column)) => column,
            _ => unreachable!("just pushed an AddColumn"),
        }
    }

    /// Add a `VARCHAR(255)`.
    pub fn string(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Varchar(DEFAULT_STRING_LEN))
    }

    /// Add a `VARCHAR(len)`.
    pub fn string_len(&mut self, name: impl Into<String>, len: u32) -> &mut Column {
        self.column(name, ColumnKind::Varchar(len))
    }

    /// Add an unbounded text column.
    pub fn text(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Text))
    }

    /// Add a 32-bit signed integer.
    pub fn integer(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Int))
    }

    /// Add a 64-bit signed integer.
    pub fn big_integer(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::BigInt))
    }

    /// Add a 64-bit unsigned integer.
    pub fn unsigned_big_integer(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::BigUint))
    }

    /// Add a foreign key column.
    ///
    /// The **constraint** is not added: no engine adds one to an existing
    /// table portably (SQLite cannot at all), and a column that silently gained
    /// no constraint would be worse than one that never claimed to. Declare it
    /// in the `CREATE TABLE`, or accept the index this gives you.
    pub fn foreign_id(&mut self, name: impl Into<String>) -> &mut Column {
        let column = self.column(name, ColumnKind::Basic(ColumnType::BigUint));
        column.index();
        column
    }

    /// Add a boolean.
    pub fn boolean(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Bool))
    }

    /// Add a timestamp.
    pub fn timestamp(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Basic(ColumnType::Timestamp))
    }

    /// Add a JSON document column.
    pub fn json(&mut self, name: impl Into<String>) -> &mut Column {
        self.column(name, ColumnKind::Json)
    }

    // --- everything else ---------------------------------------------------

    /// Drop a column.
    ///
    /// Makes the whole step **irreversible**: the type and the data are gone,
    /// and a `down` that recreated the column empty would report success while
    /// having restored nothing.
    pub fn drop_column(&mut self, name: impl Into<String>) -> &mut Self {
        self.changes.push(Change::DropColumn(name.into()));
        self
    }

    /// Rename a column. Reverses by renaming it back.
    pub fn rename_column(&mut self, from: impl Into<String>, to: impl Into<String>) -> &mut Self {
        self.changes.push(Change::RenameColumn(from.into(), to.into()));
        self
    }

    /// Create an index, named by the same convention a `CREATE TABLE` uses.
    pub fn index<I, S>(&mut self, columns: I) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push_index(columns, false, true)
    }

    /// Create a `UNIQUE` index.
    pub fn unique<I, S>(&mut self, columns: I) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push_index(columns, true, true)
    }

    /// Drop an index over `columns`.
    ///
    /// The columns rather than the name, so the reversal can recreate it —
    /// and because the name is derivable from them anyway. Use
    /// [`IndexDef::named`] on the returned value if it was named by hand.
    pub fn drop_index<I, S>(&mut self, columns: I) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push_index(columns, false, false)
    }

    /// Drop a `UNIQUE` index over `columns`.
    pub fn drop_unique<I, S>(&mut self, columns: I) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.push_index(columns, true, false)
    }

    fn push_index<I, S>(&mut self, columns: I, unique: bool, adding: bool) -> &mut IndexDef
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let columns: Vec<String> = columns.into_iter().map(Into::into).collect();
        let suffix = if unique { "unique" } else { "index" };
        let index =
            IndexDef { name: conventional_name(&self.table, &columns, suffix), columns, unique };

        self.changes.push(if adding { Change::AddIndex(index) } else { Change::DropIndex(index) });
        match self.changes.last_mut() {
            Some(Change::AddIndex(index) | Change::DropIndex(index)) => index,
            _ => unreachable!("just pushed an index change"),
        }
    }

    /// Raw SQL, for one dialect or all of them.
    ///
    /// The escape hatch for what no portable form covers — a `MODIFY COLUMN`,
    /// a generated column, a partial index. Pass `down` when you can undo it;
    /// `None` makes the step irreversible and says why.
    ///
    /// Reach for a typed change first. This one is the reason a schema stops
    /// being portable.
    pub fn raw(
        &mut self,
        dialect: Option<Dialect>,
        up: impl Into<String>,
        down: Option<String>,
    ) -> &mut Self {
        self.changes.push(Change::Raw(dialect, up.into(), down));
        self
    }

    // --- rendering ---------------------------------------------------------

    /// Every statement that applies these changes on `dialect`, in order.
    pub fn to_sql(&self, dialect: Dialect) -> Vec<String> {
        let mut statements = Vec::new();

        for change in &self.changes {
            match change {
                Change::AddColumn(column) => {
                    // SQLite has no `ADD COLUMN IF NOT EXISTS` and no
                    // conditional DDL, so a tolerant add cannot be expressed
                    // and is skipped. See `Column::if_not_exists`: the case
                    // that reaches SQLite here is a database created from the
                    // model, which already has the column.
                    if column.if_not_exists && dialect == Dialect::Sqlite {
                        continue;
                    }

                    let mut stmt = Table::alter();
                    stmt.table(Alias::new(self.table.clone())).add_column(column.to_def());

                    let mut sql = dialect.build_schema(&stmt);

                    // Rendered as text because `sea-query` has no builder for
                    // it. Anchored on `ADD COLUMN ` so a change in how that
                    // clause is built fails loudly here rather than silently
                    // dropping the guard.
                    if column.if_not_exists {
                        let anchor = "ADD COLUMN ";
                        debug_assert!(sql.contains(anchor), "no `{anchor}` to guard in: {sql}");
                        sql = sql.replacen(anchor, "ADD COLUMN IF NOT EXISTS ", 1);
                    }

                    statements.push(sql);

                    // `foreign_id("x").index()` in an alter has to become a
                    // real index here, the same as it does in a create.
                    if column.indexed {
                        statements.push(
                            dialect.build_schema(&self.column_index(column).to_stmt(&self.table)),
                        );
                    }
                }
                Change::DropColumn(name) => {
                    let mut stmt = Table::alter();
                    stmt.table(Alias::new(self.table.clone()))
                        .drop_column(Alias::new(name.clone()));
                    statements.push(dialect.build_schema(&stmt));
                }
                Change::RenameColumn(from, to) => {
                    statements.push(dialect.build_schema(&rename_column_stmt(
                        &self.table,
                        from,
                        to,
                    )));
                }
                Change::AddIndex(index) => {
                    statements.push(dialect.build_schema(&index.to_stmt(&self.table)));
                }
                Change::DropIndex(index) => {
                    statements.push(dialect.build_schema(&drop_index_stmt(&self.table, index)));
                }
                Change::Raw(only, sql, _) => {
                    if only.is_none() || *only == Some(dialect) {
                        statements.push(sql.clone());
                    }
                }
            }
        }
        statements
    }

    /// Why these changes cannot be undone, or `None` if they can.
    ///
    /// Checked before a rollback runs, so a batch that contains one
    /// irreversible step refuses up front rather than half-unwinding.
    pub fn irreversible_because(&self) -> Option<String> {
        for change in &self.changes {
            match change {
                Change::DropColumn(name) => {
                    return Some(format!(
                        "`{}`.`{name}` was dropped, and its type and data went with it",
                        self.table
                    ))
                }
                Change::Raw(_, sql, None) => {
                    return Some(format!("a raw statement with no reverse: {sql}"))
                }
                _ => {}
            }
        }
        None
    }

    /// The statements that undo these changes on `dialect`, in reverse order.
    ///
    /// Empty when [`irreversible_because`](Self::irreversible_because) has an
    /// answer — ask that first.
    pub fn to_reverse_sql(&self, dialect: Dialect) -> Vec<String> {
        let mut statements = Vec::new();

        // Backwards: a column added after an index over it has to go first.
        for change in self.changes.iter().rev() {
            match change {
                Change::AddColumn(column) => {
                    // Mirrors the forward pass: nothing was added on SQLite,
                    // so there is nothing to drop.
                    if column.if_not_exists && dialect == Dialect::Sqlite {
                        continue;
                    }
                    if column.indexed {
                        statements.push(dialect.build_schema(&drop_index_stmt(
                            &self.table,
                            &self.column_index(column),
                        )));
                    }
                    let mut stmt = Table::alter();
                    stmt.table(Alias::new(self.table.clone()))
                        .drop_column(Alias::new(column.name.clone()));
                    statements.push(dialect.build_schema(&stmt));
                }
                Change::RenameColumn(from, to) => {
                    statements.push(dialect.build_schema(&rename_column_stmt(
                        &self.table,
                        to,
                        from,
                    )));
                }
                Change::AddIndex(index) => {
                    statements.push(dialect.build_schema(&drop_index_stmt(&self.table, index)));
                }
                Change::DropIndex(index) => {
                    statements.push(dialect.build_schema(&index.to_stmt(&self.table)));
                }
                Change::Raw(only, _, Some(down)) => {
                    if only.is_none() || *only == Some(dialect) {
                        statements.push(down.clone());
                    }
                }
                // Handled by `irreversible_because`, which the caller asks
                // first — reaching here means it did not.
                Change::DropColumn(_) | Change::Raw(_, _, None) => {}
            }
        }
        statements
    }

    fn column_index(&self, column: &Column) -> IndexDef {
        let columns = vec![column.name.clone()];
        IndexDef { name: conventional_name(&self.table, &columns, "index"), columns, unique: false }
    }
}

/// `ALTER TABLE t RENAME COLUMN a TO b`, per dialect.
fn rename_column_stmt(table: &str, from: &str, to: &str) -> sea_query::TableAlterStatement {
    let mut stmt = Table::alter();
    stmt.table(Alias::new(table.to_string()))
        .rename_column(Alias::new(from.to_string()), Alias::new(to.to_string()));
    stmt
}

/// `DROP INDEX` — with `ON table` where the dialect wants it.
fn drop_index_stmt(table: &str, index: &IndexDef) -> sea_query::IndexDropStatement {
    let mut stmt = Index::drop();
    stmt.name(&index.name).table(Alias::new(table.to_string()));
    stmt
}

/// Remove the inline `PRIMARY KEY` a column asked for, keeping everything else.
///
/// `ColumnDef` has no "unset" for this, so the definition is rebuilt from the
/// column with `primary` cleared.
fn strip_inline_primary(def: &mut ColumnDef, column: &Column) {
    let mut without = column.clone();
    without.primary = false;
    without.auto_increment = false;
    *def = without.to_def();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tolerant_add_guards_the_statement_where_the_dialect_can() {
        // `Step::create_table::<E>` renders the current model, so a fresh
        // install already has the column and the migration that adds it fails
        // with `duplicate column name`. This is what lets the same migration
        // run against both.
        let changes = TableChanges::to("api_clients", |table| {
            table.boolean("internal").default(false).if_not_exists();
        });

        for dialect in [Dialect::MySql, Dialect::Postgres] {
            let sql = changes.to_sql(dialect).join(
                "
",
            );
            assert!(sql.contains("ADD COLUMN IF NOT EXISTS"), "{dialect:?}: {sql}");
        }
    }

    #[test]
    fn a_tolerant_add_is_skipped_on_sqlite() {
        // SQLite has no guard and no conditional DDL. Skipping is right for
        // the case it is used in here — a database created from the model —
        // and is documented on `Column::if_not_exists` as wrong for a
        // long-lived SQLite database that predates the column.
        let changes = TableChanges::to("api_clients", |table| {
            table.boolean("internal").default(false).if_not_exists();
        });

        assert!(changes.to_sql(Dialect::Sqlite).is_empty());
        assert!(changes.to_reverse_sql(Dialect::Sqlite).is_empty());
    }

    #[test]
    fn an_ordinary_add_is_left_alone() {
        // The guard is opt-in. A plain add must still fail loudly on a column
        // that is already there, because that is a migration written against
        // the wrong table.
        let changes = TableChanges::to("api_clients", |table| {
            table.boolean("internal").default(false);
        });

        let sql = changes.to_sql(Dialect::MySql).join(
            "
",
        );

        assert!(sql.contains("ADD COLUMN"), "{sql}");
        assert!(!sql.contains("IF NOT EXISTS"), "{sql}");
        assert!(!changes.to_sql(Dialect::Sqlite).is_empty());
    }

    fn posts() -> Blueprint {
        Blueprint::create("posts", |table| {
            table.id();
            table.string("slug").unique();
            table.text("body");
            table.boolean("published").default(false);
            table.foreign_id("author_id").constrained_on("users").cascade_on_delete();
            table.timestamps();
            table.index(["published", "created_at"]);
        })
    }

    #[test]
    fn one_description_renders_for_every_engine() {
        // The whole point: no `if dialect ==` anywhere in the caller.
        for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
            let sql = posts().to_sql(dialect).join("\n");

            assert!(sql.contains("posts"), "{dialect:?}: {sql}");
            assert!(sql.to_uppercase().contains("CREATE TABLE"), "{dialect:?}: {sql}");
            assert!(sql.contains("author_id"), "{dialect:?}: {sql}");
        }
    }

    #[test]
    fn auto_increment_is_spelled_the_way_each_engine_spells_it() {
        let sqlite = posts().to_sql(Dialect::Sqlite).join("\n");
        let mysql = posts().to_sql(Dialect::MySql).join("\n");
        let postgres = posts().to_sql(Dialect::Postgres).join("\n");

        assert!(sqlite.contains("AUTOINCREMENT"), "{sqlite}");
        assert!(mysql.contains("AUTO_INCREMENT"), "{mysql}");
        // Postgres has no keyword — it is a serial type.
        assert!(postgres.contains("serial"), "{postgres}");
    }

    #[test]
    fn a_key_never_renders_as_unsigned() {
        // Postgres has no unsigned serial, and the key a database generates is
        // a positive i64 on every backend anyway.
        let postgres = posts().to_sql(Dialect::Postgres).join("\n");
        assert!(!postgres.to_lowercase().contains("unsigned"), "{postgres}");
    }

    #[test]
    fn mysql_gets_no_if_not_exists_on_an_index_because_it_rejects_one() {
        // The trap this builder exists to close: `CREATE INDEX IF NOT EXISTS`
        // is valid on SQLite and Postgres and a syntax error on MySQL.
        let mysql = posts().to_sql(Dialect::MySql).join("\n");
        let index = mysql.lines().find(|line| line.contains("CREATE INDEX")).expect("an index");

        assert!(!index.contains("IF NOT EXISTS"), "{index}");

        let sqlite = posts().to_sql(Dialect::Sqlite).join("\n");
        assert!(sqlite.contains("CREATE INDEX IF NOT EXISTS"), "{sqlite}");
    }

    #[test]
    fn indexes_are_named_by_convention_so_a_rollback_can_find_them() {
        let sql = posts().to_sql(Dialect::Sqlite).join("\n");

        assert!(sql.contains("posts_published_created_at_index"), "{sql}");
        assert!(sql.contains("posts_author_id_index"), "the foreign id is indexed: {sql}");
    }

    #[test]
    fn a_column_index_and_a_table_index_agree_on_the_name() {
        let by_column = Blueprint::create("t", |table| {
            table.string("a").index();
        });
        let by_table = Blueprint::create("t", |table| {
            table.string("a");
            table.index(["a"]);
        });

        assert_eq!(by_column.to_sql(Dialect::Sqlite), by_table.to_sql(Dialect::Sqlite));
    }

    #[test]
    fn columns_are_not_null_unless_they_say_otherwise() {
        let sql = Blueprint::create("t", |table| {
            table.string("required");
            table.string("optional").nullable();
        })
        .to_sql(Dialect::Sqlite)
        .join("\n");

        assert!(sql.contains("\"required\" varchar(255) NOT NULL"), "{sql}");
        assert!(sql.contains("\"optional\" varchar(255) NULL"), "{sql}");
    }

    #[test]
    fn a_composite_primary_key_replaces_the_inline_one() {
        // Two primary keys is not valid anywhere, and a pivot declares its
        // pair after the columns.
        let sql = Blueprint::create("post_tag", |table| {
            table.foreign_id("post_id").constrained_on("posts").cascade_on_delete();
            table.foreign_id("tag_id").constrained_on("tags").cascade_on_delete();
            table.primary(["post_id", "tag_id"]);
        })
        .to_sql(Dialect::Sqlite)
        .join("\n");

        assert_eq!(sql.matches("PRIMARY KEY").count(), 1, "{sql}");
        assert!(sql.contains("PRIMARY KEY (\"post_id\", \"tag_id\")"), "{sql}");
    }

    #[test]
    fn foreign_keys_carry_their_action() {
        let sql = posts().to_sql(Dialect::Sqlite).join("\n");
        assert!(sql.contains("ON DELETE CASCADE"), "{sql}");
        assert!(sql.contains("REFERENCES \"users\" (\"id\")"), "{sql}");
    }

    #[test]
    fn the_reverse_of_a_create_is_a_drop() {
        let sql = posts().to_reverse_sql(Dialect::Sqlite).join("\n");
        assert_eq!(sql, "DROP TABLE IF EXISTS \"posts\"");
    }

    #[test]
    fn json_becomes_each_engines_own_json_type() {
        let blueprint = Blueprint::create("t", |table| {
            table.json("payload");
        });
        let rendered = |d| blueprint.to_sql(d).join("\n").to_lowercase();

        assert!(rendered(Dialect::Postgres).contains("jsonb"));
        assert!(rendered(Dialect::MySql).contains("json"));
        // SQLite has no JSON type; text is what it has.
        assert!(rendered(Dialect::Sqlite).contains("jsonb_text"));
    }

    #[test]
    fn decimal_is_fixed_point_where_that_exists_and_a_float_where_it_does_not() {
        // Documented rather than papered over: SQLite has no DECIMAL, so an
        // amount stored there rounds. Silently swapping in TEXT instead would
        // break every read of it.
        let blueprint = Blueprint::create("t", |table| {
            table.decimal("amount", 10, 2);
        });
        let rendered = |d| blueprint.to_sql(d).join("\n").to_lowercase();

        assert!(rendered(Dialect::Postgres).contains("decimal(10, 2)"));
        assert!(rendered(Dialect::MySql).contains("decimal(10, 2)"));
        assert!(rendered(Dialect::Sqlite).contains("real(10, 2)"));
    }

    #[test]
    fn binary_is_bytea_on_postgres_and_blob_elsewhere() {
        let blueprint = Blueprint::create("t", |table| {
            table.binary("bytes");
        });

        assert!(blueprint.to_sql(Dialect::Postgres).join("").contains("bytea"));
        assert!(blueprint.to_sql(Dialect::MySql).join("").contains("blob"));
    }
}
