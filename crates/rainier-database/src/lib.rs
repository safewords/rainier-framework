//! # rainier-database
//!
//! Rainier's data layer, built on **[Rainier ORM]** — the multi-dialect DBAL
//! where one `#[derive(Entity)]` serves SQLite, Cloudflare D1, MySQL and
//! Postgres, and where the stack of hand-written per-engine repositories
//! collapses to zero.
//!
//! This crate adds the five things a framework needs on top of that:
//!
//! | | |
//! |---|---|
//! | [`Connection`] / [`Database`] | a `dyn`-safe port over Rainier ORM's `Executor`, so a backend can live in the IoC container |
//! | [`Databases`] / [`DatabaseManager`] | the `database` section as a type: a default connection and the named ones beside it |
//! | [`Model`] | an entity the framework manages, with lifecycle **hooks** |
//! | [`Repository`] / [`EntityRepository`] | a contract to depend on, implemented once for every model |
//! | [`Criteria`] / [`Paginated`] | composable scopes and paging |
//!
//! ```ignore
//! use rainier_orm::{repo, Entity, SeaOrmExecutor, PoolConfig};
//! use rainier_database::{bind_executor, Criteria, Database, EntityRepository, Model, Repository};
//!
//! #[derive(Entity, Clone)]
//! #[orm(table = "posts")]
//! struct Post {
//!     #[orm(pk, auto_increment)] id: u64,
//!     title: String,
//!     published: bool,
//! }
//! impl Model for Post {}
//!
//! bind_executor!(SeaOrmExecutor);
//!
//! # async fn run() -> rainier_support::Result<()> {
//! let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::default()).await?;
//! let db = Database::new(executor);
//! let posts = EntityRepository::<Post>::new(db.clone());
//!
//! // Through the repository contract…
//! let page = posts
//!     .paginate_matching(Criteria::new().where_eq("published", true), 1, 20)
//!     .await?;
//!
//! // …or straight through Rainier ORM, since `Database` is an `Executor`.
//! let newest: Option<Post> = repo::query::<Post>().order_by_desc("id").first(&db).await?;
//! # Ok(()) }
//! ```
//!
//! ## The `Executor` problem, and how it is solved
//!
//! Rainier ORM's `Executor` uses `async fn` in trait and carries no `Send + Sync`
//! bound — deliberately, so the same code runs in a single-threaded Cloudflare
//! Worker over a `!Send` D1 binding. That makes it unusable as `dyn Executor`,
//! which is exactly what a container needs to store.
//!
//! [`Database`] resolves this by holding an `Arc<dyn Connection>` (Rainier's
//! `dyn`-safe mirror) and **re-implementing `Executor` on top of it**. The
//! whole ORM surface therefore works against a `Database` unchanged — no
//! wrapper per operation, and no gap between what Rainier ORM can do and what
//! Rainier exposes. Register a concrete backend with
//! [`bind_executor!`].
//!
//! [Rainier ORM]: https://github.com/safewords/rainier-framework/tree/main/crates/rainier-orm

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod connection;
pub mod criteria;
pub mod databases;
pub mod factory;
pub mod manager;
pub mod migrator;
pub mod model;
pub mod pagination;
pub mod raw;
pub mod relation;
pub mod repository;
pub mod row;
pub mod statement;
pub mod sticky;
pub mod testing;

pub use connection::{Connection, Database};
pub use criteria::{
    Assignment, Comparison, Constraint, Criteria, DatePart, JoinKind, OrGroup, Projection,
    Subquery, SubqueryDraft, SubqueryPredicate,
};
pub use databases::{
    DatabaseConfig, DatabaseCredentials, DatabaseDriver, DatabaseRole, Databases, DsnDatabase,
    PoolSettings, ServerDatabase, SqliteDatabase,
};
pub use factory::{Factory, HasFactory};
pub use manager::DatabaseManager;
pub use migrator::{Down, Migration, Migrator, Step};
pub use model::{Created, Creating, Deleted, Deleting, Model, Updated, Updating};
pub use pagination::Paginated;
pub use raw::RawQuery;
pub use relation::{
    BelongsTo, BelongsToMany, HasMany, HasOne, PivotQuery, Related, RelatedCounts, Relation,
    RelationKey,
};
pub use repository::{EntityRepository, Repository};
pub use row::{Cell, ColumnRequest, OwnedRow};
pub use statement::Prepared;
pub use sticky::{in_sticky_scope, with_sticky_scope};

// The ORM, re-exported so an application depends on one Rainier ORM version and
// gets the derive, the query builder and the migrator without naming it twice.
pub use rainier_orm;
pub use rainier_orm::{
    ddl, migrate, repo, schema, Action, Blueprint, ColumnKind, Dialect, Entity, Executor, IndexDef,
    Query, TableChanges, Upsert, UpsertAction,
};

/// What [`bind_executor!`] expands to. Not a stable API.
#[doc(hidden)]
pub mod reexport {
    pub use crate::row::{ColumnRequest, OwnedRow};
    pub use rainier_orm::sea_query::Value;
    pub use rainier_orm::{Dialect, ExecOutcome, Executor, Row, ShardRoute};
    pub use rainier_support::{BoxFuture, Error, LocalBoxFuture, Result};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_database, MemoryConnection};

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "users")]
    struct User {
        #[orm(pk, auto_increment)]
        id: u64,
        #[orm(unique)]
        email: String,
    }

    impl Model for User {}

    #[tokio::test]
    async fn the_orm_surface_is_reachable_through_the_executor_impl() {
        // Implementing `Executor` for `Database` keeps Rainier ORM's own API
        // available: `repo::` and the query builder take it with no adapter.
        // These futures are `Send` — see the assertion in
        // `the_orms_own_futures_are_send_once_upstream_scopes_its_statements`
        // below — so this path is usable from a request handler too.
        let (db, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite)
                .returning([OwnedRow::new().with("id", 1_u64).with("email", "a@b.c")]),
        );

        let found: Option<User> = repo::find_one_by(&db, "email", "a@b.c").await.unwrap();
        assert_eq!(found.unwrap().email, "a@b.c");

        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("users"), "{sql}");
        assert!(sql.contains("email"), "{sql}");
    }

    #[tokio::test]
    async fn the_query_builder_works_against_a_database() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));

        let _: Vec<User> = repo::query::<User>()
            .where_like("email", "%@example.com")
            .order_by_desc("id")
            .limit(5)
            .all(&db)
            .await
            .unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("LIKE"), "{sql}");
        assert!(sql.contains("ORDER BY"), "{sql}");
    }

    #[tokio::test]
    async fn the_repository_path_covers_the_same_ground_and_stays_send() {
        let (db, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite)
                .returning([OwnedRow::new().with("id", 1_u64).with("email", "a@b.c")]),
        );
        let users = EntityRepository::<User>::new(db);

        let found = users.first_by("email", "a@b.c".into()).await.unwrap();
        assert_eq!(found.unwrap().email, "a@b.c");
        assert!(connection.last_statement().unwrap().contains("users"));
    }

    #[test]
    fn the_orms_own_futures_are_send_once_upstream_scopes_its_statements() {
        // The property `rainier-database::statement` exists to work around.
        // While Rainier ORM holds a `sea_query` statement across its awaits
        // these futures are `!Send` and this does not compile; once it does,
        // the request path could call `repo::` directly and that module could
        // be retired.
        //
        // Kept as a live check rather than a comment so the answer is never
        // stale: it tracks whichever Rainier ORM the build resolves.
        fn assert_send<T: Send>(_: T) {}

        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        assert_send(async move {
            let found: rainier_orm::Result<Option<User>> =
                repo::find_one_by(&db, "email", "a@b.c").await;
            let listed: rainier_orm::Result<Vec<User>> =
                repo::query::<User>().where_eq("id", 1_i64).all(&db).await;
            (found.is_ok(), listed.is_ok())
        });
    }

    #[tokio::test]
    async fn schema_ddl_renders_for_the_connections_dialect() {
        let ddl = schema::create_table_ddl::<User>(Dialect::Sqlite);
        assert!(ddl.contains("users"), "{ddl}");
        assert!(ddl.contains("email"), "{ddl}");
    }
}
