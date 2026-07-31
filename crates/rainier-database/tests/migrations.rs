//! End-to-end proof of the migration contract against a real database.
//!
//! The unit tests in `migrator.rs` run against a fake connection, which records
//! SQL without parsing it. That is the right tool for asserting *which*
//! statements are issued and in what order, and the wrong tool for asserting
//! that they are valid SQL — the ledger's `BIGINT NOT NULL DEFAULT 1` and the
//! `DROP TABLE` a `create_table` step generates have to actually run somewhere.
//!
//! SQLite in memory, with a pool of one so the database survives between
//! statements.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::{Database, Down, Migrator};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Dialect, Entity, PoolConfig};

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "widgets")]
struct Widget {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
}

async fn database() -> Database {
    // A pool of one: an in-memory SQLite database exists only as long as its
    // connection, so five would be five empty databases.
    let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    Database::new(executor)
}

fn migrator() -> Migrator {
    Migrator::new().create_table::<Widget>("0001_widgets").raw(
        "0002_index_widgets_name",
        vec!["CREATE INDEX idx_widgets_name ON widgets (name)".into()],
        vec!["DROP INDEX idx_widgets_name".into()],
    )
}

/// Does the table exist? The question every one of these tests is really
/// asking, and SQLite will answer it without a schema query.
async fn widgets_exist(db: &Database) -> bool {
    db.statement("SELECT 1 FROM widgets LIMIT 1").await.is_ok()
}

#[tokio::test]
async fn migrating_creates_the_schema_and_is_idempotent() {
    let db = database().await;
    let migrator = migrator();

    let first = migrator.run(&db).await.expect("first run");
    assert_eq!(first, vec!["0001_widgets", "0002_index_widgets_name"]);
    assert!(widgets_exist(&db).await);

    // The ledger round-trips through a real table, so the second run has
    // nothing to do — including the index, whose `CREATE INDEX` has no
    // `IF NOT EXISTS` and would fail if it ran twice.
    let second = migrator.run(&db).await.expect("second run");
    assert!(second.is_empty(), "{second:?}");

    assert_eq!(
        migrator.applied(&db).await.unwrap(),
        vec!["0001_widgets", "0002_index_widgets_name"]
    );
    assert!(migrator.pending(&db).await.unwrap().is_empty());
}

#[tokio::test]
async fn rolling_back_undoes_the_batch_and_lets_it_run_again() {
    let db = database().await;
    let migrator = migrator();

    migrator.run(&db).await.expect("run");
    assert!(widgets_exist(&db).await);

    let rolled_back = migrator.rollback(&db, 1).await.expect("rollback");
    assert_eq!(
        rolled_back,
        vec!["0002_index_widgets_name", "0001_widgets"],
        "reverse order: the index goes before the table it is on"
    );
    assert!(!widgets_exist(&db).await, "the table should be gone");

    // The ledger rows went with it, so the same migrator applies cleanly again.
    assert!(migrator.applied(&db).await.unwrap().is_empty());

    let again = migrator.run(&db).await.expect("re-run");
    assert_eq!(again, vec!["0001_widgets", "0002_index_widgets_name"]);
    assert!(widgets_exist(&db).await);
}

#[tokio::test]
async fn only_the_last_batch_comes_off() {
    let db = database().await;

    // Batch 1.
    let first = Migrator::new().create_table::<Widget>("0001_widgets");
    first.run(&db).await.expect("batch 1");

    // Batch 2, a superset — only `0002` is pending, so only it is recorded
    // against the new batch.
    let second = Migrator::new().create_table::<Widget>("0001_widgets").raw(
        "0002_index_widgets_name",
        vec!["CREATE INDEX idx_widgets_name ON widgets (name)".into()],
        vec!["DROP INDEX idx_widgets_name".into()],
    );
    assert_eq!(second.run(&db).await.expect("batch 2"), vec!["0002_index_widgets_name"]);

    assert_eq!(
        second.rollback(&db, 1).await.expect("rollback"),
        vec!["0002_index_widgets_name"],
        "batch 1 is not in range"
    );
    assert!(widgets_exist(&db).await, "the table belongs to the earlier batch");
    assert_eq!(second.applied(&db).await.unwrap(), vec!["0001_widgets"]);
}

#[tokio::test]
async fn an_irreversible_step_refuses_and_changes_nothing() {
    let db = database().await;

    let migrator = Migrator::new().create_table::<Widget>("0001_widgets").raw_irreversible(
        "0002_normalise_names",
        vec!["UPDATE widgets SET name = lower(name)".into()],
        "the original casing is not recoverable",
    );

    migrator.run(&db).await.expect("run");

    let err = migrator.rollback(&db, 1).await.unwrap_err();
    assert!(err.message().contains("0002_normalise_names"), "{}", err.message());
    assert!(err.message().contains("not recoverable"), "{}", err.message());

    // Both steps are in the one batch. The refusal is up front, so the
    // reversible one must not have been undone either.
    assert!(widgets_exist(&db).await, "nothing should have been rolled back");
    assert_eq!(migrator.applied(&db).await.unwrap(), vec!["0001_widgets", "0002_normalise_names"]);
}

#[tokio::test]
async fn a_step_can_render_per_dialect() {
    let db = database().await;

    let migrator = Migrator::new().create_table::<Widget>("0001_widgets").step(
        "0002_search",
        |dialect| match dialect {
            Dialect::Sqlite => vec!["CREATE VIRTUAL TABLE widget_fts USING fts5(name)".into()],
            // No-op elsewhere, which is how a step skips a backend that does
            // not need it.
            _ => Vec::new(),
        },
        |dialect| match dialect {
            Dialect::Sqlite => Down::statements(["DROP TABLE widget_fts".to_string()]),
            _ => Down::statements([]),
        },
    );

    migrator.run(&db).await.expect("run");
    assert!(db.statement("SELECT 1 FROM widget_fts LIMIT 1").await.is_ok());

    migrator.rollback(&db, 1).await.expect("rollback");
    assert!(db.statement("SELECT 1 FROM widget_fts LIMIT 1").await.is_err());
}
