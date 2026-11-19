//! `ModelChanged` on the event bus: every committed write through a handle
//! forwarded with `Database::dispatch_changes_to`, from a repository with no
//! `with_events` or from none, heard by one listener for every model.
#![cfg(feature = "sea-orm-executor")]

use std::any::Any;
use std::sync::{Arc, Mutex};

use rainier_database::{
    Criteria, Database, EntityRepository, ModelAction, ModelChanged, Repository,
};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_events::Dispatcher;
use rainier_orm::{Dialect, Executor, PoolConfig};
use rainier_support::{Error, Result};

#[derive(Debug, Clone, PartialEq, rainier_orm::Entity)]
#[orm(table = "articles")]
struct Article {
    #[orm(pk)]
    id: i64,
    title: String,
}

impl rainier_database::Model for Article {}

async fn database() -> Database {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    for sql in rainier_orm::schema::schema_ddl::<Article>(Dialect::Sqlite) {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }
    Database::new(exec)
}

fn article(id: i64) -> Article {
    Article { id, title: format!("t{id}") }
}

/// A database forwarding to a dispatcher, and every `ModelChanged` it heard.
async fn world() -> (Database, Arc<Dispatcher>, Arc<Mutex<Vec<ModelChanged>>>) {
    let db = database().await;
    let events = Arc::new(Dispatcher::new());
    db.dispatch_changes_to(Arc::clone(&events));
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    events.listen(move |change: Arc<ModelChanged>| {
        sink.lock().unwrap().push((*change).clone());
        async { Ok(()) }
    });
    (db, events, heard)
}

#[tokio::test]
async fn a_write_through_a_plain_repository_is_on_the_bus() {
    let (db, _events, heard) = world().await;
    // No `with_events`: the typed `Created<Article>` never fires.
    let articles = EntityRepository::<Article>::new(db);

    articles.create(article(7)).await.unwrap();
    articles
        .update_matching(
            Criteria::new().where_eq("title", "t7"),
            vec![("title".into(), "renamed".into())],
        )
        .await
        .unwrap();
    articles.delete(7_i64.into()).await.unwrap();

    let heard = heard.lock().unwrap().clone();
    let actions: Vec<ModelAction> = heard.iter().map(|c| c.action).collect();
    assert_eq!(actions, [ModelAction::Created, ModelAction::Updated, ModelAction::Deleted]);
    assert!(heard.iter().all(|c| c.model == "Article" && c.table == "articles"));
    assert!(heard[0].model_type.is_some_and(|t| t.ends_with("::Article")));
    assert_eq!(heard[0].keys, Some(vec!["7".to_string()]));
    assert_eq!(heard[1].keys, None, "a bulk update does not say which rows");
}

#[tokio::test]
async fn a_rolled_back_write_is_never_announced() {
    let (db, _events, heard) = world().await;

    let undone: Result<()> = db
        .transaction(|tx| async move {
            EntityRepository::<Article>::new(tx).create(article(1)).await?;
            Err(Error::internal("roll it back"))
        })
        .await;
    assert!(undone.is_err());
    assert!(heard.lock().unwrap().is_empty());

    db.transaction(|tx| async move {
        EntityRepository::<Article>::new(tx).create(article(2)).await.map(|_| ())
    })
    .await
    .unwrap();
    assert_eq!(heard.lock().unwrap().len(), 1, "announced on commit");
}

#[tokio::test]
async fn a_wildcard_listener_hears_model_changes_too() {
    let db = database().await;
    let events = Arc::new(Dispatcher::new());
    db.dispatch_changes_to(Arc::clone(&events));
    let names = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&names);
    events.listen_any(move |name: &'static str, _: Arc<dyn Any + Send + Sync>| {
        sink.lock().unwrap().push(name);
        async { Ok(()) }
    });

    EntityRepository::<Article>::new(db).create(article(3)).await.unwrap();

    assert_eq!(*names.lock().unwrap(), ["rainier_database::model::ModelChanged"]);
}

#[tokio::test]
async fn a_failing_listener_does_not_fail_the_write() {
    let db = database().await;
    let events = Arc::new(Dispatcher::new());
    db.dispatch_changes_to(Arc::clone(&events));
    events.listen(|_: Arc<ModelChanged>| async { Err(Error::internal("listener broke")) });

    let created = EntityRepository::<Article>::new(db.clone()).create(article(4)).await;
    assert!(created.is_ok(), "the write committed; a listener cannot undo it");
    assert!(EntityRepository::<Article>::new(db).find(4_i64.into()).await.unwrap().is_some());
}
