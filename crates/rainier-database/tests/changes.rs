//! Row-change notification: every builder and `repo` write tells
//! `Database::on_change` subscribers which rows it changed — the property a
//! cache of rows depends on, since a write it does not hear about is a stale
//! read nobody notices.
#![cfg(feature = "sea-orm-executor")]

use std::sync::{Arc, Mutex};

use rainier_database::criteria::Assignment;
use rainier_database::{
    statement, ChangeKind, ChangedKeys, Criteria, Database, EntityRepository, Repository, RowChange,
};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, Dialect, Executor, PoolConfig, Upsert};

#[derive(Debug, Clone, PartialEq, rainier_orm::Entity, serde::Serialize, serde::Deserialize)]
#[orm(table = "articles")]
struct Article {
    #[orm(pk)]
    id: i64,
    title: String,
    views: i64,
}

impl rainier_database::Model for Article {}

async fn world() -> (Database, Arc<Mutex<Vec<RowChange>>>) {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    for sql in rainier_orm::schema::schema_ddl::<Article>(Dialect::Sqlite) {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }
    let db = Database::new(exec);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    db.on_change(move |change: RowChange| {
        let sink = Arc::clone(&sink);
        async move { sink.lock().unwrap().push(change) }
    });
    (db, seen)
}

fn article(id: i64) -> Article {
    Article { id, title: format!("t{id}"), views: 0 }
}

fn take(seen: &Arc<Mutex<Vec<RowChange>>>) -> Vec<RowChange> {
    std::mem::take(&mut *seen.lock().unwrap())
}

fn keys(change: &RowChange) -> Option<Vec<String>> {
    match &change.keys {
        ChangedKeys::Keys(keys) => Some(keys.clone()),
        ChangedKeys::Unknown => None,
    }
}

#[tokio::test]
async fn a_repository_write_reports_its_row_before_it_returns() {
    let (db, seen) = world().await;
    let articles = EntityRepository::<Article>::new(db.clone());

    articles.create(article(7)).await.expect("create");
    // Read straight after the write: the subscriber has already run.
    let created = take(&seen);
    assert_eq!(created.len(), 1);
    assert_eq!((created[0].kind, keys(&created[0])), (ChangeKind::Insert, Some(vec!["7".into()])));

    let mut row = article(7);
    row.title = "renamed".into();
    articles.update(&row).await.expect("update");
    let updated = take(&seen);
    assert_eq!((updated[0].kind, keys(&updated[0])), (ChangeKind::Update, Some(vec!["7".into()])));
    assert_eq!(updated[0].table, "articles");

    articles.delete(7_i64.into()).await.expect("delete");
    let deleted = take(&seen);
    assert_eq!((deleted[0].kind, keys(&deleted[0])), (ChangeKind::Delete, Some(vec!["7".into()])));
}

#[tokio::test]
async fn a_bulk_update_names_its_keys_when_the_criteria_pin_them_and_says_unknown_otherwise() {
    let (db, seen) = world().await;
    let articles = EntityRepository::<Article>::new(db.clone());
    for id in [1, 2, 3] {
        articles.create(article(id)).await.expect("create");
    }
    take(&seen);

    articles
        .update_matching(
            Criteria::new().where_in("id", vec![1_i64, 2]).where_eq("title", "t1"),
            vec![("title".into(), "x".into())],
        )
        .await
        .expect("pinned");
    assert_eq!(keys(&take(&seen)[0]), Some(vec!["1".into(), "2".into()]));

    articles
        .update_matching(
            Criteria::new().where_eq("title", "t3"),
            vec![("views".into(), 1_i64.into())],
        )
        .await
        .expect("unpinned");
    let unpinned = take(&seen);
    assert_eq!((unpinned[0].kind, keys(&unpinned[0])), (ChangeKind::Update, None));

    // Two pins on the key: only what both allow.
    articles
        .update_matching(
            Criteria::new().where_in("id", vec![1_i64, 2]).where_eq("id", 2_i64),
            vec![("views".into(), 5_i64.into())],
        )
        .await
        .expect("narrowed");
    assert_eq!(keys(&take(&seen)[0]), Some(vec!["2".into()]));

    articles.delete_matching(Criteria::new().where_eq("id", 3_i64)).await.expect("delete");
    let deleted = take(&seen);
    assert_eq!((deleted[0].kind, keys(&deleted[0])), (ChangeKind::Delete, Some(vec!["3".into()])));
}

#[tokio::test]
async fn a_counter_increment_built_as_a_statement_is_reported() {
    let (db, seen) = world().await;
    EntityRepository::<Article>::new(db.clone()).create(article(9)).await.expect("create");
    take(&seen);

    db.execute(statement::update_matching_with::<Article>(
        db.dialect(),
        &Criteria::new().where_in("id", vec![9_i64]),
        vec![("views".to_string(), Assignment::Increment(1))],
    ))
    .await
    .expect("increment");
    let changes = take(&seen);
    assert_eq!((changes[0].kind, keys(&changes[0])), (ChangeKind::Update, Some(vec!["9".into()])));
}

#[tokio::test]
async fn orm_writes_report_through_the_executor() {
    let (db, seen) = world().await;

    repo::upsert_with(&db, &article(4), &Upsert::on(["id"]).increment(["views"]))
        .await
        .expect("upsert");
    assert_eq!(keys(&take(&seen)[0]), Some(vec!["4".into()]));

    repo::upsert_many_with(&db, &[article(5), article(6)], &Upsert::on(["id"]).replace(["title"]))
        .await
        .expect("upsert many");
    assert_eq!(keys(&take(&seen)[0]), Some(vec!["5".into(), "6".into()]));

    repo::query::<Article>().where_eq("id", 4_i64).increment(&db, "views", 2).await.expect("inc");
    let incremented = take(&seen);
    assert_eq!(
        (incremented[0].kind, keys(&incremented[0])),
        (ChangeKind::Update, Some(vec!["4".into()]))
    );

    repo::query::<Article>().where_gt("views", 100_i64).delete(&db).await.expect("delete");
    assert_eq!(keys(&take(&seen)[0]), None, "no key pinned: unknown");
}

#[tokio::test]
async fn a_read_reports_nothing_and_a_derived_handle_shares_the_subscribers() {
    let (db, seen) = world().await;
    let articles = EntityRepository::<Article>::new(db.clone());
    articles.create(article(1)).await.expect("create");
    take(&seen);

    articles.find(1_i64.into()).await.expect("read");
    assert!(take(&seen).is_empty());

    // A write through the writer handle reaches the same subscribers.
    EntityRepository::<Article>::new(db.writer()).update(&article(1)).await.expect("update");
    assert_eq!(take(&seen).len(), 1);
}
