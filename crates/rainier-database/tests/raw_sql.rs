//! Raw SQL against a real database.
//!
//! The unit tests in `raw.rs` assert on the statement that would be sent. This
//! asserts that the statement is one SQLite will actually run, that the
//! bindings arrive in the right order, and that the values come back — which
//! is the part a fake connection cannot answer.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::{Database, Migrator};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Entity, PoolConfig};

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "widgets")]
struct Widget {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
    weight: i64,
}

async fn database() -> Database {
    // A pool of one: an in-memory SQLite database exists only as long as its
    // connection.
    let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::in_memory())
        .await
        .expect("connect");
    let database = Database::new(executor);

    Migrator::new().create_table::<Widget>("0001_widgets").run(&database).await.expect("migrate");

    for (name, weight) in [("anvil", 100), ("feather", 1), ("brick", 20)] {
        database
            .query("INSERT INTO widgets (name, weight) VALUES (?, ?)")
            .bind(name)
            .bind(weight)
            .execute()
            .await
            .expect("insert");
    }

    database
}

#[tokio::test]
async fn a_bound_write_runs_and_reports_what_it_did() {
    let database = database().await;

    let outcome = database
        .query("UPDATE widgets SET weight = weight + ? WHERE name = ?")
        .bind(5)
        .bind("brick")
        .execute()
        .await
        .expect("update");

    assert_eq!(outcome.rows_affected, 1);

    let weight = database
        .query("SELECT weight FROM widgets WHERE name = ?")
        .bind("brick")
        .scalar_i64("weight")
        .await
        .expect("read back");

    assert_eq!(weight, Some(25));
}

#[tokio::test]
async fn bindings_keep_their_order() {
    // Two placeholders of the same type, so a swapped pair would still run and
    // simply return the wrong rows.
    let database = database().await;

    let names = database
        .query("SELECT name FROM widgets WHERE weight > ? AND weight < ? ORDER BY name")
        .bind(5)
        .bind(50)
        .column("name")
        .await
        .expect("select");

    assert_eq!(names, vec!["brick".to_string()]);
}

#[tokio::test]
async fn a_row_decodes_into_an_entity() {
    let database = database().await;

    let heavy: Vec<Widget> = database
        .query("SELECT * FROM widgets WHERE weight >= ? ORDER BY weight DESC")
        .bind(20)
        .fetch_all()
        .await
        .expect("select");

    assert_eq!(heavy.len(), 2);
    assert_eq!(heavy[0].name, "anvil");
    assert_eq!(heavy[1].name, "brick");
}

#[tokio::test]
async fn one_row_or_none() {
    let database = database().await;

    let found: Option<Widget> = database
        .query("SELECT * FROM widgets WHERE name = ?")
        .bind("feather")
        .fetch_one()
        .await
        .expect("select");
    assert_eq!(found.map(|w| w.weight), Some(1));

    let missing: Option<Widget> = database
        .query("SELECT * FROM widgets WHERE name = ?")
        .bind("nothing here")
        .fetch_one()
        .await
        .expect("select");
    assert!(missing.is_none());
}

#[tokio::test]
async fn an_aggregate_over_no_rows_is_none_rather_than_zero() {
    // `SUM` over an empty set is NULL. Flattening that to 0 is how a total
    // silently becomes wrong.
    let database = database().await;

    let total = database
        .query("SELECT SUM(weight) AS total FROM widgets WHERE name = ?")
        .bind("no such widget")
        .scalar_i64("total")
        .await
        .expect("sum");

    assert_eq!(total, None);

    let real = database
        .query("SELECT SUM(weight) AS total FROM widgets")
        .scalar_i64("total")
        .await
        .expect("sum");

    assert_eq!(real, Some(121));
}

#[tokio::test]
async fn a_value_that_looks_like_sql_is_data_and_nothing_else() {
    // The whole reason `bind` exists. If this were interpolated the table
    // would be gone and the next assertion would fail.
    let database = database().await;

    let found = database
        .query("SELECT name FROM widgets WHERE name = ?")
        .bind("'; DROP TABLE widgets; --")
        .column("name")
        .await
        .expect("select");

    assert!(found.is_empty());

    let still_there = database
        .query("SELECT COUNT(*) AS cnt FROM widgets")
        .scalar_i64("cnt")
        .await
        .expect("count");
    assert_eq!(still_there, Some(3));
}

#[tokio::test]
async fn a_statement_with_no_bindings_still_works() {
    let database = database().await;

    let name = database
        .query("SELECT name FROM widgets ORDER BY weight DESC LIMIT 1")
        .scalar_string("name")
        .await
        .expect("select");

    assert_eq!(name.as_deref(), Some("anvil"));
}
