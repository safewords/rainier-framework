//! `Executor::pool_usage` on the SeaORM executor: the pool's own counts on
//! every backend, and on MySQL/MariaDB what the server says about the user.
#![cfg(feature = "sea-orm-executor")]

use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Executor, PoolConfig};

#[tokio::test]
async fn sqlite_reports_its_pool_and_no_server() {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless()).await.unwrap();
    exec.fetch_all("SELECT 1", vec![]).await.unwrap();
    let usage = exec.pool_usage().await.expect("a pool");
    assert_eq!(usage.max, PoolConfig::serverless().max_connections);
    assert!(usage.open >= 1 && usage.idle <= usage.open);
    assert_eq!((usage.server, usage.server_error), (None, None));
}

#[tokio::test]
async fn mysql_reports_the_users_connections_and_caps() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL pool-usage check");
        return;
    };
    let exec = SeaOrmExecutor::connect(&url, &PoolConfig::default()).await.unwrap();
    exec.fetch_all("SELECT 1", vec![]).await.unwrap();
    let usage = exec.pool_usage().await.expect("a pool");
    eprintln!("{usage:?}");
    let server = usage.server.unwrap_or_else(|| panic!("no answer: {:?}", usage.server_error));
    assert!(server.user >= 1, "at least the connection that asked");
    assert!(server.total >= server.user);
    assert!(server.max > 0);
}
