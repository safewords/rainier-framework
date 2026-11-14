//! The database store against a real server: MySQL, MariaDB or Postgres.
//!
//! The unit tests run the conformance suite on SQLite, which hides what a
//! server does differently — `SUM` result types on MariaDB, reserved words
//! (`type`, `key`) on MySQL, index length limits, boolean decoding. This runs
//! the same suite against every server `LOOKOUT_TEST_DATABASE_URLS` names
//! (comma-separated; CI sets a MariaDB and a Postgres), or `TEST_DATABASE_URL`,
//! and skips when neither is set, like `rainier-database`'s own server checks.
//!
//! ```sh
//! docker run -d --rm -p 3307:3306 -e MARIADB_ROOT_PASSWORD=pw -e MARIADB_DATABASE=lookout mariadb:11
//! LOOKOUT_TEST_DATABASE_URLS=mysql://root:pw@127.0.0.1:3307/lookout cargo test -p rainier-lookout --features sea-orm-executor --test database_server
//! ```
//!
//! The tables are dropped first, so point it at a database you can throw away.
//! Needs the `sea-orm-executor` feature, which the CI runs that have servers
//! turn on.

#![cfg(feature = "sea-orm-executor")]

use rainier_database::Database;
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_lookout::store::conformance;
use rainier_lookout::DatabaseStore;
use rainier_orm::PoolConfig;

const TABLES: &[&str] =
    &["lookout_entries", "lookout_batch_tags", "lookout_aggregates", "lookout_batches"];

fn servers() -> Vec<String> {
    let urls = std::env::var("LOOKOUT_TEST_DATABASE_URLS")
        .or_else(|_| std::env::var("TEST_DATABASE_URL"))
        .unwrap_or_default();
    urls.split(',').map(str::trim).filter(|u| !u.is_empty()).map(String::from).collect()
}

async fn fresh(url: &str) -> Database {
    let executor = SeaOrmExecutor::connect(url, &PoolConfig::default()).await.expect("connect");
    let db = Database::new(executor);
    for table in TABLES {
        db.statement(&format!("DROP TABLE IF EXISTS {table}")).await.expect("drop");
    }
    db.statement("DROP TABLE IF EXISTS rainier_migrations").await.expect("drop the ledger");
    db
}

// One test, so the servers' cases do not race each other on shared tables.
#[tokio::test]
async fn the_database_store_conforms_on_a_real_server() {
    let servers = servers();
    if servers.is_empty() {
        eprintln!("no LOOKOUT_TEST_DATABASE_URLS / TEST_DATABASE_URL — skipping");
        return;
    }
    for url in servers {
        let scheme = url.split("://").next().unwrap_or("?").to_string();
        let db = fresh(&url).await;

        assert_eq!(
            DatabaseStore::migrations().run(&db).await.expect("migrate").len(),
            4,
            "[{scheme}]"
        );
        assert!(
            DatabaseStore::migrations().run(&db).await.expect("migrate again").is_empty(),
            "[{scheme}]"
        );

        let store = DatabaseStore::new(db);
        conformance::run(&store).await;
        // Every insert and delete chunked, as on D1.
        conformance::run(&store.with_max_bind_params(100)).await;
        eprintln!("[{scheme}] conforms");
    }
}
