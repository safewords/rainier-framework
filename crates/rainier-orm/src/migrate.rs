//! Schema migrations — an ordered list of steps applied **idempotently**,
//! tracked in a `_orm_migrations` table, against one executor or many.
//!
//! For sharding this is the piece that creates each table on every shard: build
//! a [`Migrator`] of the sharded entities and [`run_on_each`](Migrator::run_on_each)
//! it across the shard executors, and a second of the global entities against
//! the global database. Migrations are wasm-safe (they only use [`Executor`]),
//! so the same set runs from a server or inside a Worker.
//!
//! ```ignore
//! use rainier_orm::migrate::Migrator;
//!
//! let shard_schema = Migrator::new()
//!     .create_table::<User>("0001_users")
//!     .create_table::<Token>("0002_tokens")
//!     .add(MyCustomStep);          // impl Migration for anything bespoke
//!
//! shard_schema.run_on_each(&shard_execs).await?;   // every shard
//! global_schema.run(&global_exec).await?;          // the directory DB
//! ```

use crate::{repo, schema, Entity, Executor, Result};
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;

/// A future returned by a migration step — boxed so [`Migration`] stays
/// object-safe (the runner holds `Box<dyn Migration<X>>`).
///
/// # This future is **not** `Send`, and cannot be made so on stable
///
/// Everywhere else in this crate `Send`-ness **leaks**: `repo::` and
/// [`Query`](crate::Query) return unboxed futures, so theirs are `Send`
/// exactly when the concrete executor's are — no bound declared, and both a
/// multi-threaded server and a single-threaded Worker are served by one
/// implementation.
///
/// Boxing behind `dyn` destroys that, because `Box<dyn Future>` is `!Send`
/// whatever was boxed. The bound would have to be written here — and it cannot
/// be satisfied: [`CreateTable`] implements [`Migration<X>`] for *every*
/// `X: Executor`, so its `up` would have to produce a `Send` future for every
/// executor, and whether `X::execute`'s future is `Send` is unknowable in a
/// generic context. Expressing it needs return-type notation
/// (`X: Executor<execute(..): Send>`), still unstable.
///
/// So [`Migrator::run`] is the one API here that cannot be awaited inside a
/// `tokio::spawn`ed task. Two ways round it, both fine in practice:
///
/// - **Run migrations outside a spawned task** — at boot, on the main future.
///   This is what most applications do anyway.
/// - **Render synchronously and execute the strings yourself.**
///   [`schema::schema_ddl`] and [`ddl::Migration::render`](crate::ddl::Migration::render)
///   are ordinary synchronous functions returning `Vec<String>`, so a caller
///   that needs a `Send` future can render first and then await plain
///   `execute` calls — no boxed future anywhere.
///
/// `tests/send_futures.rs` asserts the `Send` property for everything that can
/// hold it, and documents this exception.
pub type StepFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

/// Tracking row: one per applied migration name. A plain global entity, so it
/// lands in whichever single database the migrator runs against.
#[derive(Debug, Clone, Entity)]
#[orm(table = "_orm_migrations")]
struct Applied {
    #[orm(pk)]
    name: String,
}

/// One migration step. The unique `name` orders + dedupes it; `up` performs the
/// change against an executor. Implement this on a struct for bespoke DDL, or
/// use [`Migrator::create_table`] for the common "create this entity's table".
pub trait Migration<X: Executor> {
    fn name(&self) -> &'static str;
    fn up<'a>(&'a self, db: &'a X) -> StepFuture<'a>;
}

/// A migration that creates `E`'s table (+ its indexes) from the derived
/// schema, idempotently (`CREATE TABLE IF NOT EXISTS`).
pub struct CreateTable<E> {
    name: &'static str,
    _entity: PhantomData<E>,
}

impl<E> CreateTable<E> {
    pub fn new(name: &'static str) -> Self {
        Self { name, _entity: PhantomData }
    }
}

impl<E: Entity, X: Executor> Migration<X> for CreateTable<E> {
    fn name(&self) -> &'static str {
        self.name
    }
    fn up<'a>(&'a self, db: &'a X) -> StepFuture<'a> {
        Box::pin(async move {
            for ddl in schema::schema_ddl::<E>(db.dialect()) {
                db.execute(&ddl, vec![]).await?;
            }
            Ok(())
        })
    }
}

/// An ordered, idempotent set of migrations for one executor *type* `X`.
pub struct Migrator<X> {
    steps: Vec<Box<dyn Migration<X>>>,
}

impl<X: Executor> Default for Migrator<X> {
    fn default() -> Self {
        Self::new()
    }
}

impl<X: Executor> Migrator<X> {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Append a step (any `Migration` impl).
    #[allow(clippy::should_implement_trait)] // builder method, not `std::ops::Add`
    pub fn add(mut self, m: impl Migration<X> + 'static) -> Self {
        self.steps.push(Box::new(m));
        self
    }

    /// Append a "create `E`'s table" step.
    pub fn create_table<E: Entity + 'static>(self, name: &'static str) -> Self {
        self.add(CreateTable::<E>::new(name))
    }

    /// The names of every step, in registration order — what a
    /// migration-runner port reports as `defined()`, and the source of
    /// the migration "head" used to dedupe boot-time runs.
    /// The port that reports it is defined by the consumer, not here.
    pub fn names(&self) -> Vec<&'static str> {
        self.steps.iter().map(|s| s.name()).collect()
    }

    /// Apply every not-yet-applied step to `db`, recording each in
    /// `_orm_migrations`. Idempotent: re-running applies only new steps.
    /// Returns the names applied this run.
    ///
    /// When `db` reports it is sharded ([`Executor::is_sharded`]),
    /// the sharding control tables (`shards`, `shard_directory`) are ensured on
    /// its global database first — the ORM owns that metadata, so no flavor
    /// declares it. A no-op on a single, unsharded database.
    pub async fn run(&self, db: &X) -> Result<Vec<&'static str>> {
        if db.is_sharded() {
            crate::sharding::ensure_control_tables(db).await?;
        }
        db.execute(&schema::create_table_ddl::<Applied>(db.dialect()), vec![]).await?;
        let done: Vec<String> =
            repo::all::<Applied, _>(db).await?.into_iter().map(|a| a.name).collect();

        let mut applied = Vec::new();
        for step in &self.steps {
            if done.iter().any(|n| n == step.name()) {
                continue;
            }
            step.up(db).await?;
            repo::insert(db, &Applied { name: step.name().to_string() }).await?;
            applied.push(step.name());
        }
        Ok(applied)
    }

    /// [`run`](Self::run) against each executor in turn — the shard fan-out
    /// (every shard gets the same schema, tracked independently).
    pub async fn run_on_each(&self, dbs: &[&X]) -> Result<()> {
        for db in dbs {
            self.run(db).await?;
        }
        Ok(())
    }
}
