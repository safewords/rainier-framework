//! `Database::transaction` against real databases: SQLite always, and MariaDB
//! or MySQL when `TEST_DATABASE_URL` names one.
#![cfg(feature = "sea-orm-executor")]

use std::sync::{Arc, Mutex};

use rainier_database::{Database, EntityRepository, Model, Repository, RowChange};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Dialect, Entity, Executor, PoolConfig};
use rainier_support::Error;

#[derive(Debug, Clone, PartialEq, Default, Entity)]
#[orm(table = "ledger_entries")]
struct Entry {
    #[orm(pk)]
    id: i64,
    amount: i64,
}

impl Model for Entry {}

async fn sqlite() -> Database {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::in_memory())
        .await
        .expect("connect");
    for sql in rainier_orm::schema::schema_ddl::<Entry>(Dialect::Sqlite) {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }
    Database::new(exec)
}

fn entries(db: &Database) -> EntityRepository<Entry> {
    EntityRepository::new(db.clone())
}

/// Every change subscribers hear, in order.
fn heard(db: &Database) -> Arc<Mutex<Vec<RowChange>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    db.on_change(move |change| {
        sink.lock().unwrap().push(change);
        async {}
    });
    heard
}

#[tokio::test]
async fn a_transaction_that_returns_ok_commits_everything_it_wrote() {
    let db = sqlite().await;
    db.transaction(|tx| async move {
        entries(&tx).create(Entry { id: 1, amount: 10 }).await?;
        entries(&tx).create(Entry { id: 2, amount: -10 }).await?;
        Ok(())
    })
    .await
    .unwrap();

    assert_eq!(entries(&db).all().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_transaction_that_returns_an_error_leaves_nothing_behind() {
    let db = sqlite().await;
    let result: Result<(), Error> = db
        .transaction(|tx| async move {
            entries(&tx).create(Entry { id: 1, amount: 10 }).await?;
            Err(Error::internal("the second half failed"))
        })
        .await;

    assert_eq!(result.unwrap_err().message(), "the second half failed");
    assert!(entries(&db).all().await.unwrap().is_empty(), "the first write was undone");
}

#[tokio::test]
async fn a_transaction_that_panics_is_rolled_back() {
    let db = sqlite().await;
    let inside = db.clone();
    let panicked = tokio::spawn(async move {
        inside
            .transaction(|tx| async move {
                entries(&tx).create(Entry { id: 1, amount: 10 }).await?;
                panic!("the handler blew up");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await
    })
    .await;

    assert!(panicked.is_err());
    assert!(entries(&db).all().await.unwrap().is_empty());
}

#[tokio::test]
async fn reads_inside_see_the_transactions_own_writes() {
    let db = sqlite().await;
    let seen = db
        .transaction(|tx| async move {
            entries(&tx).create(Entry { id: 1, amount: 10 }).await?;
            Ok(entries(&tx).all().await?.len())
        })
        .await
        .unwrap();
    assert_eq!(seen, 1);
}

#[tokio::test]
async fn subscribers_hear_a_committed_write_after_the_commit_and_a_rolled_back_one_never() {
    let db = sqlite().await;
    let heard = heard(&db);

    let during = Arc::clone(&heard);
    db.transaction(|tx| async move {
        entries(&tx).create(Entry { id: 1, amount: 10 }).await?;
        assert!(during.lock().unwrap().is_empty(), "nothing is announced before the commit");
        assert!(tx.in_transaction());
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(heard.lock().unwrap().len(), 1, "announced once committed");
    assert_eq!(heard.lock().unwrap()[0].table, "ledger_entries");

    let _: Result<(), Error> = db
        .transaction(|tx| async move {
            entries(&tx).create(Entry { id: 2, amount: 5 }).await?;
            Err(Error::internal("undo"))
        })
        .await;
    assert_eq!(heard.lock().unwrap().len(), 1, "a rolled-back write is never announced");
}

#[tokio::test]
async fn a_nested_transaction_joins_the_outer_one() {
    let db = sqlite().await;
    let result: Result<(), Error> = db
        .transaction(|tx| async move {
            tx.transaction(|inner| async move {
                entries(&inner).create(Entry { id: 1, amount: 10 }).await?;
                Ok(())
            })
            .await?;
            Err(Error::internal("the outer one fails after the inner one finished"))
        })
        .await;

    assert!(result.is_err());
    assert!(entries(&db).all().await.unwrap().is_empty(), "the inner write went with the outer");
}

#[tokio::test]
async fn a_conflict_is_run_again_and_an_ordinary_error_is_not() {
    let db = sqlite().await;
    let runs = Arc::new(Mutex::new(0));

    let counted = Arc::clone(&runs);
    db.transaction_retrying(3, |tx| {
        let counted = Arc::clone(&counted);
        async move {
            let run = {
                let mut runs = counted.lock().unwrap();
                *runs += 1;
                *runs
            };
            entries(&tx).create(Entry { id: 1, amount: 10 }).await?;
            if run == 1 {
                return Err(Error::internal(
                    "Deadlock found when trying to get lock; try restarting transaction",
                ));
            }
            Ok(())
        }
    })
    .await
    .unwrap();
    assert_eq!(*runs.lock().unwrap(), 2);
    assert_eq!(entries(&db).all().await.unwrap().len(), 1, "the first run was rolled back");

    *runs.lock().unwrap() = 0;
    let counted = Arc::clone(&runs);
    let failed: Result<(), Error> = db
        .transaction_retrying(3, |_tx| {
            let counted = Arc::clone(&counted);
            async move {
                *counted.lock().unwrap() += 1;
                Err(Error::internal("a validation failure"))
            }
        })
        .await;
    assert!(failed.is_err());
    assert_eq!(*runs.lock().unwrap(), 1, "only a conflict is retried");
}

#[tokio::test]
async fn a_backend_without_transactions_refuses_before_the_work_runs() {
    let db = Database::new(rainier_database::testing::MemoryConnection::new(Dialect::Sqlite));
    let ran = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&ran);
    let refused: Result<(), Error> = db
        .transaction(|_tx| async move {
            *flag.lock().unwrap() = true;
            Ok(())
        })
        .await;
    assert!(refused.is_err());
    assert!(!*ran.lock().unwrap());
}

/// The same on MariaDB or MySQL, where a transaction holds a pooled
/// connection while other connections keep serving.
#[tokio::test]
async fn on_a_real_server_other_connections_do_not_see_uncommitted_writes() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("no TEST_DATABASE_URL — skipping");
        return;
    };
    if !url.starts_with("mysql://") && !url.starts_with("mariadb://") {
        eprintln!("TEST_DATABASE_URL is not MariaDB or MySQL — skipping");
        return;
    }
    let pool = PoolConfig { max_connections: 4, ..PoolConfig::default() };
    let db = Database::new(SeaOrmExecutor::connect(&url, &pool).await.expect("connect"));
    db.statement("DROP TABLE IF EXISTS ledger_entries").await.unwrap();
    for sql in rainier_orm::schema::schema_ddl::<Entry>(Dialect::MySql) {
        db.statement(&sql).await.unwrap();
    }

    let outside = db.clone();
    db.transaction(|tx| async move {
        entries(&tx).create(Entry { id: 1, amount: 10 }).await?;
        assert!(entries(&outside).all().await?.is_empty(), "not visible before the commit");
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(entries(&db).all().await.unwrap().len(), 1);

    let _: Result<(), Error> = db
        .transaction(|tx| async move {
            entries(&tx).create(Entry { id: 2, amount: 10 }).await?;
            Err(Error::internal("undo"))
        })
        .await;
    assert_eq!(entries(&db).all().await.unwrap().len(), 1, "rolled back on the server");
    db.statement("DROP TABLE ledger_entries").await.unwrap();
}
