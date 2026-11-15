//! `concurrently!` and `concurrent::all` against a real server: independent
//! statements really do run at the same time, each on its own pooled
//! connection.
//!
//! The unit tests in `concurrent.rs` prove the combinators with sleeps. What
//! they cannot prove is the part that matters — that a `Database` hands each
//! query its own connection rather than queueing them on one — so this sends
//! `DO SLEEP(0.2)` four times, one after the other and then at once.
//!
//! Skipped unless `TEST_DATABASE_URL` names a MariaDB or MySQL server:
//!
//! ```text
//! docker run -d --rm -p 3307:3306 -e MARIADB_ROOT_PASSWORD=pw -e MARIADB_DATABASE=rainier mariadb:11.4
//! TEST_DATABASE_URL=mysql://root:pw@127.0.0.1:3307/rainier \
//!     cargo test -p rainier-database --features sea-orm-executor --test concurrent_server
//! ```
#![cfg(feature = "sea-orm-executor")]

use std::time::{Duration, Instant};

use rainier_database::{concurrent, concurrently, Database};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::PoolConfig;

const NAP: &str = "DO SLEEP(0.2)";

#[tokio::test]
async fn independent_statements_run_at_once_on_a_real_server() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("no TEST_DATABASE_URL — skipping");
        return;
    };
    if !url.starts_with("mysql://") && !url.starts_with("mariadb://") {
        eprintln!("TEST_DATABASE_URL is not MariaDB or MySQL — skipping");
        return;
    }
    let pool = PoolConfig { max_connections: 10, ..PoolConfig::default() };
    let db = Database::new(SeaOrmExecutor::connect(&url, &pool).await.expect("connect"));
    // Open the connections before timing anything.
    concurrent::all_limited(4, (0..4).map(|_| db.statement("DO 1"))).await.expect("warm");

    let started = Instant::now();
    for _ in 0..4 {
        db.statement(NAP).await.expect("nap");
    }
    let one_by_one = started.elapsed();

    let started = Instant::now();
    concurrently!(db.statement(NAP), db.statement(NAP), db.statement(NAP), db.statement(NAP))
        .expect("naps");
    let at_once = started.elapsed();

    let started = Instant::now();
    concurrent::all((0..8).map(|_| db.statement(NAP))).await.expect("bounded naps");
    let bounded = started.elapsed();

    eprintln!("one by one {one_by_one:?}, concurrently {at_once:?}, all(8) {bounded:?}");
    assert!(one_by_one >= Duration::from_millis(790), "four naps in a row: {one_by_one:?}");
    assert!(at_once < Duration::from_millis(400), "four naps at once: {at_once:?}");
    // Eight, four at a time: two rounds.
    assert!(
        bounded >= Duration::from_millis(390) && bounded < Duration::from_millis(700),
        "eight naps, four at a time: {bounded:?}"
    );
}
