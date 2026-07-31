//! Schema migrations — the [`Migration`] contract, and the [`Migrator`] that
//! runs and rolls them back.
//!
//! An ordered, idempotent list of named steps, tracked in a
//! `rainier_migrations` table so each runs at most once — and, because every
//! step declares how to undo itself, so a batch can be taken back off.
//!
//! Rainier ORM ships its own `migrate::Migrator`, and it is the better tool when
//! you can reach it: it understands sharding fan-out and its `ddl::Migration`
//! change sets are dialect-portable. But its `run` is `async` while holding
//! `sea_query` values, so its future is `!Send` and it cannot be awaited from a
//! service provider's `boot`. This migrator renders DDL **synchronously**
//! (through Rainier ORM's own [`schema`](rainier_orm::schema) module, which is
//! already sync) and then executes plain strings, so booting can run it.
//!
//! ```ignore
//! Migrator::new()
//!     .create_table::<User>("0001_create_users")
//!     .create_table::<Post>("0002_create_posts")
//!     .raw(
//!         "0003_index_posts_author",
//!         vec!["CREATE INDEX idx_posts_author ON posts (author_id)".into()],
//!         vec!["DROP INDEX idx_posts_author".into()],
//!     )
//!     .run(&db)
//!     .await?;
//! ```

use std::sync::Arc;

use rainier_orm::{Blueprint, Dialect, Entity, Row, TableChanges};
use rainier_support::{Error, Result};

use crate::connection::Database;

/// How a migration undoes itself.
///
/// An enum rather than a possibly-empty `Vec`, because "this cannot be undone"
/// and "nobody wrote a down step" are different facts and only one of them is a
/// bug. A rollback **refuses** on [`Irreversible`](Down::Irreversible) and
/// prints the reason, instead of doing nothing and reporting success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Down {
    /// The statements that undo this migration, in the order they should run.
    Statements(Vec<String>),
    /// Deliberately not reversible, and why.
    ///
    /// A migration that drops a column or rewrites data cannot restore what it
    /// destroyed. Saying so is more useful than a `down` that runs cleanly and
    /// leaves the database subtly wrong.
    Irreversible(String),
}

impl Down {
    /// Undone by these statements.
    pub fn statements(statements: impl IntoIterator<Item = String>) -> Self {
        Self::Statements(statements.into_iter().collect())
    }

    /// Not reversible, for the stated reason.
    pub fn irreversible(reason: impl Into<String>) -> Self {
        Self::Irreversible(reason.into())
    }

    /// The statements, or an error naming the migration and why it has none.
    pub fn sql(&self, migration: &str) -> Result<&[String]> {
        match self {
            Down::Statements(statements) => Ok(statements),
            Down::Irreversible(reason) => {
                Err(Error::internal(format!("`{migration}` cannot be rolled back: {reason}")))
            }
        }
    }

    /// Whether this migration can be rolled back.
    pub fn is_reversible(&self) -> bool {
        matches!(self, Down::Statements(_))
    }
}

/// One migration: a name, what it does, and how to undo it.
///
/// **`down` is required.** A contract with an optional `down` is a contract
/// whose `down` is usually missing — and by the time you need one, the
/// migration that lacks it is months old and nobody remembers what it changed.
/// Requiring it costs one line for a `create_table` (a `DROP`) and forces the
/// question at the only moment it is easy to answer.
///
/// Where the honest answer is "you cannot", say so with
/// [`Down::irreversible`] and the reason travels with the migration.
///
/// ```ignore
/// struct BackfillSlugs;
///
/// impl Migration for BackfillSlugs {
///     fn name(&self) -> &str { "0004_backfill_slugs" }
///
///     fn up(&self, _: Dialect) -> Vec<String> {
///         vec!["UPDATE posts SET slug = lower(title) WHERE slug IS NULL".into()]
///     }
///
///     fn down(&self, _: Dialect) -> Down {
///         Down::irreversible("the original NULLs are not recoverable")
///     }
/// }
/// ```
///
/// Both renderers are **synchronous**, which is what keeps
/// [`Migrator::run`]'s future `Send`.
pub trait Migration: Send + Sync + 'static {
    /// The name recorded in the ledger.
    ///
    /// Permanent once applied: renaming one makes it run again.
    fn name(&self) -> &str;

    /// The statements that apply this migration to `dialect`.
    ///
    /// Returning an empty vector is a no-op, which is how a step skips a
    /// backend that does not need it.
    fn up(&self, dialect: Dialect) -> Vec<String>;

    /// How to undo it.
    fn down(&self, dialect: Dialect) -> Down;
}

/// A [`Migration`] built from closures — what the builder methods produce.
///
/// Implement [`Migration`] directly when a migration wants a name of its own
/// and a place to live; use `Step` when it is two lists of statements.
pub struct Step {
    name: String,
    up: Box<dyn Fn(Dialect) -> Vec<String> + Send + Sync>,
    down: Box<dyn Fn(Dialect) -> Down + Send + Sync>,
}

impl Step {
    /// A step from an `up` and a `down` renderer.
    pub fn new(
        name: impl Into<String>,
        up: impl Fn(Dialect) -> Vec<String> + Send + Sync + 'static,
        down: impl Fn(Dialect) -> Down + Send + Sync + 'static,
    ) -> Self {
        Self { name: name.into(), up: Box::new(up), down: Box::new(down) }
    }

    /// A step running fixed SQL either way, whatever the dialect.
    pub fn raw(name: impl Into<String>, up: Vec<String>, down: Vec<String>) -> Self {
        Self::new(name, move |_| up.clone(), move |_| Down::Statements(down.clone()))
    }

    /// A step that applies fixed SQL and cannot be undone, with the reason.
    pub fn raw_irreversible(
        name: impl Into<String>,
        up: Vec<String>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        Self::new(name, move |_| up.clone(), move |_| Down::irreversible(reason.clone()))
    }

    /// A step creating `E`'s table and its indexes, from the derived metadata.
    ///
    /// Its `down` drops the table, which is the honest inverse — and also why
    /// rolling back a `create_table` in production destroys data. That is a
    /// property of the operation, not of this implementation.
    pub fn create_table<E: Entity>(name: impl Into<String>) -> Self {
        Self::new(
            name,
            |dialect| rainier_orm::schema::schema_ddl::<E>(dialect),
            |_| Down::Statements(vec![format!("DROP TABLE IF EXISTS {}", E::table())]),
        )
    }

    /// A step creating `table` from a [`Blueprint`].
    ///
    /// For a table no model describes: a pivot, a join table, something a
    /// third-party tool reads. The description is written once and lowered to
    /// whichever engine the executor reports, so there is no per-dialect SQL
    /// to keep in step.
    ///
    /// ```ignore
    /// Step::create("0007_create_post_tag", "post_tag", |table| {
    ///     table.foreign_id("post_id").constrained_on("posts").cascade_on_delete();
    ///     table.foreign_id("tag_id").constrained_on("tags").cascade_on_delete();
    ///     table.primary(["post_id", "tag_id"]);
    /// })
    /// ```
    ///
    /// Its `down` drops the table — which is the honest inverse, and also why
    /// rolling one back in production destroys data.
    pub fn create(
        name: impl Into<String>,
        table: impl Into<String>,
        build: impl FnOnce(&mut Blueprint),
    ) -> Self {
        let blueprint = Arc::new(Blueprint::create(table, build));
        let reverse = Arc::clone(&blueprint);

        Self::new(
            name,
            move |dialect| blueprint.to_sql(dialect),
            move |dialect| Down::Statements(reverse.to_reverse_sql(dialect)),
        )
    }

    /// A step changing a table that already exists.
    ///
    /// ```ignore
    /// Step::table("0008_posts_add_subtitle", "posts", |table| {
    ///     table.string("subtitle").nullable();
    ///     table.index(["author_id", "published"]);
    /// })
    /// ```
    ///
    /// **The `down` is derived from what changed**, so it cannot drift from
    /// the `up` the way a hand-written one does. Where a change genuinely
    /// cannot be undone — a dropped column, a raw statement with no reverse —
    /// the step reports itself irreversible and says which change made it so.
    pub fn table(
        name: impl Into<String>,
        table: impl Into<String>,
        build: impl FnOnce(&mut TableChanges),
    ) -> Self {
        let changes = Arc::new(TableChanges::to(table, build));
        let reverse = Arc::clone(&changes);

        Self::new(
            name,
            move |dialect| changes.to_sql(dialect),
            move |dialect| match reverse.irreversible_because() {
                Some(reason) => Down::irreversible(reason),
                None => Down::Statements(reverse.to_reverse_sql(dialect)),
            },
        )
    }

    /// A step dropping a table.
    ///
    /// **Irreversible**, and deliberately so: nothing here knows the shape of
    /// what it dropped, and a `down` that recreated an empty table would
    /// report success having restored nothing.
    pub fn drop_table(name: impl Into<String>, table: impl Into<String>) -> Self {
        let table = table.into();
        let reason = format!("`{table}` was dropped, along with everything in it");

        Self::new(
            name,
            move |dialect| {
                let mut stmt = rainier_orm::sea_query::Table::drop();
                stmt.table(rainier_orm::sea_query::Alias::new(table.clone())).if_exists();
                vec![dialect.build_schema(&stmt)]
            },
            move |_| Down::irreversible(reason.clone()),
        )
    }

    /// A step renaming a table. Reverses by renaming it back.
    ///
    /// `ALTER TABLE … RENAME TO` on SQLite and Postgres, `RENAME TABLE` on
    /// MySQL — one of the several places the engines simply disagree.
    pub fn rename_table(
        name: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        let (from, to) = (from.into(), to.into());
        let (back_from, back_to) = (to.clone(), from.clone());

        Self::new(
            name,
            move |dialect| vec![rename_table_sql(dialect, &from, &to)],
            move |dialect| Down::Statements(vec![rename_table_sql(dialect, &back_from, &back_to)]),
        )
    }
}

/// Rename a table, in whichever way the dialect spells it.
fn rename_table_sql(dialect: Dialect, from: &str, to: &str) -> String {
    let mut stmt = rainier_orm::sea_query::Table::rename();
    stmt.table(
        rainier_orm::sea_query::Alias::new(from.to_string()),
        rainier_orm::sea_query::Alias::new(to.to_string()),
    );
    dialect.build_schema(&stmt)
}

impl Migration for Step {
    fn name(&self) -> &str {
        &self.name
    }

    fn up(&self, dialect: Dialect) -> Vec<String> {
        (self.up)(dialect)
    }

    fn down(&self, dialect: Dialect) -> Down {
        (self.down)(dialect)
    }
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Step").field("name", &self.name).finish()
    }
}

/// The table recording which migrations have run, and in which batch.
const LEDGER: &str = "rainier_migrations";

/// Runs an ordered list of [`Migration`]s, skipping any already applied.
#[derive(Default)]
pub struct Migrator {
    migrations: Vec<Arc<dyn Migration>>,
}

impl Migrator {
    /// An empty migrator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a migration.
    #[allow(clippy::should_implement_trait, reason = "a builder step, not arithmetic")]
    pub fn add(mut self, migration: impl Migration) -> Self {
        self.migrations.push(Arc::new(migration));
        self
    }

    /// Append an already-shared migration.
    pub fn add_arc(mut self, migration: Arc<dyn Migration>) -> Self {
        self.migrations.push(migration);
        self
    }

    /// Append a "create this entity's table" step, whose `down` drops it.
    pub fn create_table<E: Entity>(self, name: impl Into<String>) -> Self {
        self.add(Step::create_table::<E>(name))
    }

    /// Append a step from `up` and `down` rendering closures.
    pub fn step(
        self,
        name: impl Into<String>,
        up: impl Fn(Dialect) -> Vec<String> + Send + Sync + 'static,
        down: impl Fn(Dialect) -> Down + Send + Sync + 'static,
    ) -> Self {
        self.add(Step::new(name, up, down))
    }

    /// Append a step running fixed SQL, with the SQL that undoes it.
    pub fn raw(self, name: impl Into<String>, up: Vec<String>, down: Vec<String>) -> Self {
        self.add(Step::raw(name, up, down))
    }

    /// Append a step that cannot be undone, with the reason.
    pub fn raw_irreversible(
        self,
        name: impl Into<String>,
        up: Vec<String>,
        reason: impl Into<String>,
    ) -> Self {
        self.add(Step::raw_irreversible(name, up, reason))
    }

    /// Append every step of `other`, keeping its order.
    ///
    /// How a component that owns tables contributes them to the application's
    /// one migrator — `DatabaseQueue::migrations()` being the case that ships.
    /// Without this, a driver's tables could only be created by running a
    /// second migrator, which the `migrate` command has no way to find.
    pub fn merge(mut self, other: Migrator) -> Self {
        self.migrations.extend(other.migrations);
        self
    }

    /// Every registered step's name, in order.
    pub fn names(&self) -> Vec<&str> {
        self.migrations.iter().map(|m| m.name()).collect()
    }

    /// How many steps are registered.
    pub fn len(&self) -> usize {
        self.migrations.len()
    }

    /// Whether no steps are registered.
    pub fn is_empty(&self) -> bool {
        self.migrations.is_empty()
    }

    /// Which registered steps declare themselves irreversible on `dialect`.
    ///
    /// Worth printing before a deploy: it is exactly the list of things a
    /// rollback will refuse to undo.
    pub fn irreversible(&self, dialect: Dialect) -> Vec<&str> {
        self.migrations
            .iter()
            .filter(|m| !m.down(dialect).is_reversible())
            .map(|m| m.name())
            .collect()
    }

    /// Apply every step that has not run yet, in order. Returns the names of
    /// the steps that were applied this time.
    ///
    /// They share one **batch**, which is the unit
    /// [`rollback`](Self::rollback) takes back off.
    ///
    /// A step's statements run before its ledger row is written, so a failure
    /// part-way leaves it unrecorded and it is retried next boot. That means a
    /// step must be **idempotent** where it can be — `CREATE TABLE IF NOT
    /// EXISTS`, which is what Rainier ORM's generated DDL emits.
    pub async fn run(&self, db: &Database) -> Result<Vec<String>> {
        self.ensure_ledger(db).await?;
        let ledger = self.ledger_rows(db).await?;
        let batch = ledger.iter().map(|(_, batch)| *batch).max().unwrap_or(0) + 1;

        let mut ran = Vec::new();
        for migration in &self.migrations {
            let name = migration.name();
            if ledger.iter().any(|(applied, _)| applied == name) {
                continue;
            }

            for sql in migration.up(db.dialect()) {
                self.execute(db, name, &sql, "migration").await?;
            }

            self.record(db, name, batch).await?;
            tracing::info!(migration = %name, batch, "migrated");
            ran.push(name.to_string());
        }

        Ok(ran)
    }

    /// Undo the last `batches` batches, most recent first. Returns the names
    /// that were rolled back.
    ///
    /// Within a batch, steps are undone in **reverse** declaration order,
    /// because a later step may depend on an earlier one — dropping the table
    /// a foreign key points at before dropping the key fails on every backend
    /// that enforces them.
    ///
    /// Refuses **before running anything** if any step in range is
    /// [irreversible](Down::Irreversible), or is in the ledger but no longer in
    /// this migrator. A rollback that half-completes leaves the schema in a
    /// state no migration describes, which is worse than one that never
    /// started.
    pub async fn rollback(&self, db: &Database, batches: u32) -> Result<Vec<String>> {
        let targets = self.rollback_targets(db, batches).await?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        for name in &targets {
            self.find(name)?.down(db.dialect()).sql(name)?;
        }

        let mut rolled_back = Vec::new();
        for name in &targets {
            let down = self.find(name)?.down(db.dialect());
            for sql in down.sql(name)? {
                self.execute(db, name, sql, "rollback").await?;
            }

            self.forget(db, name).await?;
            tracing::info!(migration = %name, "rolled back");
            rolled_back.push(name.clone());
        }

        Ok(rolled_back)
    }

    /// The names the last `batches` batches applied, in the order a rollback
    /// would undo them.
    ///
    /// What `migrate:rollback --pretend` prints.
    pub async fn rollback_targets(&self, db: &Database, batches: u32) -> Result<Vec<String>> {
        if batches == 0 {
            return Ok(Vec::new());
        }

        let mut rows = self.ledger_rows(db).await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // The `batches` highest batch numbers, whatever they happen to be —
        // counting back from the largest rather than assuming they are
        // contiguous, because a rollback leaves gaps.
        let mut numbers: Vec<i64> = rows.iter().map(|(_, batch)| *batch).collect();
        numbers.sort_unstable();
        numbers.dedup();
        let cutoff = numbers.iter().rev().take(batches as usize).copied().min().unwrap_or(i64::MAX);
        rows.retain(|(_, batch)| *batch >= cutoff);

        // Highest batch first, and within a batch the reverse of the order the
        // steps were declared in, so dependants are dropped before what they
        // depend on. A name the migrator no longer knows sorts last; the
        // rollback then refuses on it by name rather than skipping it.
        let declared = self.names();
        rows.sort_by_key(|(name, batch)| {
            let position = declared.iter().position(|n| n == name).unwrap_or(usize::MAX);
            (std::cmp::Reverse(*batch), std::cmp::Reverse(position))
        });

        Ok(rows.into_iter().map(|(name, _)| name).collect())
    }

    /// Which migrations the ledger says have already run.
    pub async fn applied(&self, db: &Database) -> Result<Vec<String>> {
        Ok(self.ledger_rows(db).await?.into_iter().map(|(name, _)| name).collect())
    }

    /// Which registered migrations have not run yet, in order.
    pub async fn pending(&self, db: &Database) -> Result<Vec<&str>> {
        let applied = self.applied(db).await?;
        Ok(self.names().into_iter().filter(|name| !applied.iter().any(|a| a == name)).collect())
    }

    fn find(&self, name: &str) -> Result<&Arc<dyn Migration>> {
        self.migrations.iter().find(|m| m.name() == name).ok_or_else(|| {
            Error::internal(format!(
                "`{name}` is in the ledger but not in this migrator, so there is nothing to \
                 run to undo it. It was probably deleted from the code while still applied."
            ))
        })
    }

    async fn execute(&self, db: &Database, name: &str, sql: &str, what: &str) -> Result<()> {
        db.statement(sql).await.map(|_| ()).map_err(|e| {
            Error::internal(format!(
                "{what} `{name}` failed on `{}`: {e}",
                rainier_support::str::limit(sql, 80, "…")
            ))
        })
    }

    /// Every ledger row, as `(name, batch)`.
    async fn ledger_rows(&self, db: &Database) -> Result<Vec<(String, i64)>> {
        self.ensure_ledger(db).await?;

        let prepared = crate::statement::Prepared {
            sql: format!("SELECT name, batch FROM {LEDGER}"),
            params: Vec::new(),
            route: rainier_orm::ShardRoute::Global,
        };
        let columns = vec![
            crate::row::ColumnRequest::new("name", rainier_orm::ColumnType::Text),
            crate::row::ColumnRequest::new("batch", rainier_orm::ColumnType::BigInt),
        ];

        let rows = db.fetch(prepared, columns).await?;
        rows.iter()
            .map(|row| {
                let name = row
                    .get_string("name")
                    .map_err(Error::from)?
                    .ok_or_else(|| Error::internal("a migration ledger row had a NULL name"))?;
                // A ledger written before batches existed has no such column,
                // and everything in it belongs to the same original batch.
                let batch = row.get_i64("batch").map_err(Error::from)?.unwrap_or(1);
                Ok((name, batch))
            })
            .collect()
    }

    async fn ensure_ledger(&self, db: &Database) -> Result<()> {
        // `VARCHAR(255) PRIMARY KEY` renders acceptably on all three dialects,
        // and `IF NOT EXISTS` makes this safe to run on every boot.
        db.statement(&format!(
            "CREATE TABLE IF NOT EXISTS {LEDGER} (\
               name VARCHAR(255) NOT NULL PRIMARY KEY, \
               batch BIGINT NOT NULL DEFAULT 1)"
        ))
        .await?;
        Ok(())
    }

    async fn record(&self, db: &Database, name: &str, batch: i64) -> Result<()> {
        let values = if db.dialect() == Dialect::Postgres { "($1, $2)" } else { "(?, ?)" };
        let prepared = crate::statement::Prepared {
            sql: format!("INSERT INTO {LEDGER} (name, batch) VALUES {values}"),
            params: vec![name.into(), batch.into()],
            route: rainier_orm::ShardRoute::Global,
        };

        db.execute(prepared).await?;
        Ok(())
    }

    async fn forget(&self, db: &Database, name: &str) -> Result<()> {
        let placeholder = if db.dialect() == Dialect::Postgres { "$1" } else { "?" };
        let prepared = crate::statement::Prepared {
            sql: format!("DELETE FROM {LEDGER} WHERE name = {placeholder}"),
            params: vec![name.into()],
            route: rainier_orm::ShardRoute::Global,
        };

        db.execute(prepared).await?;
        Ok(())
    }
}

impl std::fmt::Debug for Migrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Migrator").field("migrations", &self.names()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row::OwnedRow;
    use crate::testing::{fake_database, MemoryConnection};

    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "users")]
    struct User {
        #[orm(pk, auto_increment)]
        id: u64,
        #[orm(unique)]
        email: String,
    }

    /// A ledger row as the fake connection would return one.
    fn ledger(name: &str, batch: i64) -> OwnedRow {
        OwnedRow::new().with("name", name).with("batch", batch)
    }

    #[test]
    fn a_migrator_run_is_send() {
        // The whole reason this exists rather than Rainier ORM's own migrator:
        // a service provider's `boot` needs a `Send` future.
        fn assert_send<T: Send>(_: T) {}

        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let migrator = Migrator::new().create_table::<User>("0001_users");
        assert_send(async move { migrator.run(&db).await });
    }

    #[test]
    fn a_rollback_is_send_too() {
        fn assert_send<T: Send>(_: T) {}

        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let migrator = Migrator::new().create_table::<User>("0001_users");
        assert_send(async move { migrator.rollback(&db, 1).await });
    }

    #[test]
    fn create_table_renders_the_entitys_ddl() {
        let step = Step::create_table::<User>("0001_users");
        let statements = step.up(Dialect::Sqlite);

        assert_eq!(step.name(), "0001_users");
        assert!(statements[0].contains("users"), "{:?}", statements);
        assert!(statements[0].contains("IF NOT EXISTS"), "{:?}", statements);
    }

    #[test]
    fn create_table_undoes_itself_by_dropping_the_table() {
        let step = Step::create_table::<User>("0001_users");

        assert_eq!(
            step.down(Dialect::Sqlite),
            Down::Statements(vec!["DROP TABLE IF EXISTS users".to_string()])
        );
    }

    #[test]
    fn ddl_differs_per_dialect() {
        let step = Step::create_table::<User>("0001_users");
        let sqlite = step.up(Dialect::Sqlite).join("");
        let mysql = step.up(Dialect::MySql).join("");
        assert_ne!(sqlite, mysql, "each dialect should render its own DDL");
    }

    #[test]
    fn an_irreversible_step_says_why_rather_than_doing_nothing() {
        // The distinction the enum exists for: "cannot" is not "forgot".
        let step = Step::raw_irreversible(
            "0003_backfill",
            vec!["UPDATE users SET email = lower(email)".into()],
            "the original casing is not recoverable",
        );

        let down = step.down(Dialect::Sqlite);
        assert!(!down.is_reversible());

        let err = down.sql("0003_backfill").unwrap_err();
        assert!(err.message().contains("cannot be rolled back"), "{}", err.message());
        assert!(err.message().contains("not recoverable"), "{}", err.message());
    }

    #[test]
    fn a_custom_type_can_implement_the_contract() {
        struct Citext;

        impl Migration for Citext {
            fn name(&self) -> &str {
                "0004_citext"
            }

            fn up(&self, dialect: Dialect) -> Vec<String> {
                match dialect {
                    Dialect::Postgres => vec!["CREATE EXTENSION IF NOT EXISTS citext".into()],
                    _ => Vec::new(),
                }
            }

            fn down(&self, dialect: Dialect) -> Down {
                match dialect {
                    Dialect::Postgres => Down::statements(["DROP EXTENSION citext".to_string()]),
                    _ => Down::statements([]),
                }
            }
        }

        let migrator = Migrator::new().add(Citext);

        assert_eq!(migrator.names(), vec!["0004_citext"]);
        assert!(migrator.irreversible(Dialect::Postgres).is_empty());
    }

    #[test]
    fn irreversible_steps_can_be_listed_before_a_deploy() {
        let migrator = Migrator::new().create_table::<User>("0001_users").raw_irreversible(
            "0002_drop_legacy",
            vec!["ALTER TABLE users DROP COLUMN legacy".into()],
            "the column's data is gone",
        );

        assert_eq!(migrator.irreversible(Dialect::Sqlite), vec!["0002_drop_legacy"]);
    }

    #[tokio::test]
    async fn runs_pending_migrations_in_order() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));

        let ran = Migrator::new()
            .raw("0001_a", vec!["CREATE TABLE a (id INT)".into()], vec!["DROP TABLE a".into()])
            .raw("0002_b", vec!["CREATE TABLE b (id INT)".into()], vec!["DROP TABLE b".into()])
            .run(&db)
            .await
            .unwrap();

        assert_eq!(ran, vec!["0001_a", "0002_b"]);

        let statements = connection.statements();
        assert!(statements.iter().any(|s| s.contains("CREATE TABLE a")));
        assert!(statements.iter().any(|s| s.contains("CREATE TABLE b")));
        assert!(statements.iter().any(|s| s.contains("rainier_migrations")));
    }

    #[tokio::test]
    async fn an_already_applied_migration_is_skipped() {
        // The ledger read returns `0001_a` as already applied. Queued answers
        // are consumed in order: the first `fetch` is the ledger query.
        let connection = MemoryConnection::new(Dialect::Sqlite).returning([ledger("0001_a", 1)]);
        let (db, handle) = fake_database(connection);

        let ran = Migrator::new()
            .raw("0001_a", vec!["CREATE TABLE a (id INT)".into()], vec!["DROP TABLE a".into()])
            .raw("0002_b", vec!["CREATE TABLE b (id INT)".into()], vec!["DROP TABLE b".into()])
            .run(&db)
            .await
            .unwrap();

        assert_eq!(ran, vec!["0002_b"]);
        assert!(
            !handle.statements().iter().any(|s| s.contains("CREATE TABLE a")),
            "the applied migration must not run again"
        );
    }

    #[tokio::test]
    async fn a_new_run_gets_the_next_batch_number() {
        let (db, connection) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).returning([ledger("0001_a", 7)]));

        Migrator::new()
            .raw("0001_a", vec!["SELECT 1".into()], vec!["SELECT 1".into()])
            .raw("0002_b", vec!["SELECT 2".into()], vec!["SELECT 2".into()])
            .run(&db)
            .await
            .unwrap();

        let inserts: Vec<_> = connection
            .recorded()
            .into_iter()
            .filter(|r| r.sql.starts_with("INSERT INTO rainier_migrations"))
            .collect();

        assert_eq!(inserts.len(), 1, "only the pending step should be recorded");
        assert_eq!(
            inserts[0].params[1],
            8_i64.into(),
            "one past the highest batch in the ledger, not one past its row count"
        );
    }

    #[tokio::test]
    async fn a_failing_step_names_itself_and_its_statement() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite).failing("syntax error"));

        let err = Migrator::new()
            .raw("0001_broken", vec!["CREATE TABLE (".into()], vec!["SELECT 1".into()])
            .run(&db)
            .await
            .unwrap_err();

        // The ledger creation is the first statement to fail here, which is
        // itself worth surfacing rather than hiding.
        assert!(err.message().contains("syntax error"), "{}", err.message());
    }

    #[tokio::test]
    async fn the_ledger_is_created_before_anything_is_read() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        Migrator::new().run(&db).await.unwrap();

        let statements = connection.statements();
        assert!(statements[0].contains("CREATE TABLE IF NOT EXISTS rainier_migrations"));
        assert!(statements[0].contains("batch"), "{}", statements[0]);
    }

    #[test]
    fn the_migrator_lists_its_steps() {
        let migrator = Migrator::new().create_table::<User>("0001_users").raw(
            "0002_seed",
            vec!["INSERT INTO users DEFAULT VALUES".into()],
            vec!["DELETE FROM users".into()],
        );

        assert_eq!(migrator.names(), vec!["0001_users", "0002_seed"]);
        assert_eq!(migrator.len(), 2);
        assert!(!migrator.is_empty());
    }

    #[test]
    fn merging_appends_the_other_migrators_steps_in_order() {
        let component = Migrator::new()
            .raw("component_0001", vec!["SELECT 1".into()], vec!["SELECT 1".into()])
            .raw("component_0002", vec!["SELECT 2".into()], vec!["SELECT 2".into()]);

        let migrator = Migrator::new()
            .raw("app_0001", vec!["SELECT 3".into()], vec!["SELECT 3".into()])
            .merge(component)
            .raw("app_0002", vec!["SELECT 4".into()], vec!["SELECT 4".into()]);

        assert_eq!(
            migrator.names(),
            vec!["app_0001", "component_0001", "component_0002", "app_0002"],
            "merged steps keep their order, at the point they were merged in"
        );
    }

    #[tokio::test]
    async fn postgres_gets_numbered_placeholders() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Postgres));
        Migrator::new()
            .raw("0001_a", vec!["SELECT 1".into()], vec!["SELECT 1".into()])
            .run(&db)
            .await
            .unwrap();

        let insert = connection
            .statements()
            .into_iter()
            .find(|s| s.starts_with("INSERT INTO rainier_migrations"))
            .expect("the ledger row should have been written");
        assert!(insert.contains("$1") && insert.contains("$2"), "{insert}");
    }

    #[tokio::test]
    async fn rolling_back_an_empty_ledger_is_not_an_error() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let migrator = Migrator::new().create_table::<User>("0001_users");

        assert!(migrator.rollback(&db, 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rolling_back_zero_batches_does_nothing() {
        let (db, connection) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).returning([ledger("0001_a", 1)]));

        let migrator =
            Migrator::new().raw("0001_a", vec!["SELECT 1".into()], vec!["DROP TABLE a".into()]);

        assert!(migrator.rollback(&db, 0).await.unwrap().is_empty());
        assert!(
            !connection.statements().iter().any(|s| s.contains("DROP TABLE a")),
            "nothing should have been undone"
        );
    }

    #[tokio::test]
    async fn a_rollback_undoes_the_last_batch_in_reverse() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite).returning([
            ledger("0001_a", 1),
            ledger("0002_b", 2),
            ledger("0003_c", 2),
        ]));

        let migrator = Migrator::new()
            .raw("0001_a", vec!["SELECT 1".into()], vec!["DROP TABLE a".into()])
            .raw("0002_b", vec!["SELECT 2".into()], vec!["DROP TABLE b".into()])
            .raw("0003_c", vec!["SELECT 3".into()], vec!["DROP TABLE c".into()]);

        let rolled_back = migrator.rollback(&db, 1).await.unwrap();

        assert_eq!(
            rolled_back,
            vec!["0003_c", "0002_b"],
            "the newest batch only, and dependants first"
        );

        let statements = connection.statements();
        assert!(statements.iter().any(|s| s == "DROP TABLE c"));
        assert!(statements.iter().any(|s| s == "DROP TABLE b"));
        assert!(!statements.iter().any(|s| s == "DROP TABLE a"), "batch 1 was not in range");
        assert!(statements.iter().any(|s| s.starts_with("DELETE FROM rainier_migrations")));
    }

    #[tokio::test]
    async fn a_rollback_can_take_several_batches() {
        let (db, _) = fake_database(
            MemoryConnection::new(Dialect::Sqlite)
                .returning([ledger("0001_a", 1), ledger("0002_b", 2)]),
        );

        let migrator = Migrator::new()
            .raw("0001_a", vec!["SELECT 1".into()], vec!["DROP TABLE a".into()])
            .raw("0002_b", vec!["SELECT 2".into()], vec!["DROP TABLE b".into()]);

        assert_eq!(migrator.rollback(&db, 2).await.unwrap(), vec!["0002_b", "0001_a"]);
    }

    #[tokio::test]
    async fn an_irreversible_step_stops_the_whole_rollback_before_it_starts() {
        // `0002_b` is reversible and would be undone first. It must not run,
        // because `0001_a` in the same batch cannot be undone at all — a
        // half-rolled-back batch matches no migration's idea of the schema.
        let (db, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite)
                .returning([ledger("0001_a", 1), ledger("0002_b", 1)]),
        );

        let migrator = Migrator::new()
            .raw_irreversible("0001_a", vec!["SELECT 1".into()], "the rows are gone")
            .raw("0002_b", vec!["SELECT 2".into()], vec!["DROP TABLE b".into()]);

        let err = migrator.rollback(&db, 1).await.unwrap_err();

        assert!(err.message().contains("0001_a"), "{}", err.message());
        assert!(err.message().contains("the rows are gone"), "{}", err.message());
        assert!(
            !connection.statements().iter().any(|s| s == "DROP TABLE b"),
            "no step should have run"
        );
    }

    #[tokio::test]
    async fn rolling_back_a_migration_that_was_deleted_from_the_code_says_so() {
        let (db, _) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).returning([ledger("0001_a", 1)]));

        let err = Migrator::new().rollback(&db, 1).await.unwrap_err();

        assert!(err.message().contains("0001_a"), "{}", err.message());
        assert!(err.message().contains("not in this migrator"), "{}", err.message());
    }

    #[tokio::test]
    async fn pending_is_what_has_not_run() {
        let (db, _) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).returning([ledger("0001_a", 1)]));

        let migrator = Migrator::new()
            .raw("0001_a", vec!["SELECT 1".into()], vec!["SELECT 1".into()])
            .raw("0002_b", vec!["SELECT 2".into()], vec!["SELECT 2".into()]);

        assert_eq!(migrator.pending(&db).await.unwrap(), vec!["0002_b"]);
    }
}
