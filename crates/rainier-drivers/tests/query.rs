//! Exercises the fluent query builder end to end: predicates (eq/ne/like/in/
//! null/comparison), ordering + paging, a single-backend join filter, count,
//! delete, and first_or_create. Runs on SQLite always, MySQL when
//! `TEST_DATABASE_URL` is set.
#![cfg(feature = "sea-orm-executor")]

use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::sea_query::Cond;
use rainier_orm::{repo, schema, Entity, Executor, PoolConfig};

#[derive(Debug, Clone, Entity)]
#[orm(table = "authors")]
struct Author {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
    active: bool,
}

#[derive(Debug, Clone, Entity)]
#[orm(table = "posts")]
struct Post {
    #[orm(pk, auto_increment)]
    id: u64,
    author_id: u64,
    title: String,
    views: i64,
    note: Option<String>,
}

async fn setup(exec: &SeaOrmExecutor) {
    for ddl in schema::schema_ddl::<Author>(exec.dialect()) {
        exec.execute(&ddl, vec![]).await.unwrap();
    }
    for ddl in schema::schema_ddl::<Post>(exec.dialect()) {
        exec.execute(&ddl, vec![]).await.unwrap();
    }
    let alice =
        repo::insert(exec, &Author { id: 0, name: "alice".into(), active: true }).await.unwrap();
    let bob =
        repo::insert(exec, &Author { id: 0, name: "bob".into(), active: false }).await.unwrap();

    let posts = [
        (alice, "alpha", 100i64, Some("x")),
        (alice, "alphabet", 50, None),
        (alice, "beta", 10, Some("y")),
        (bob, "gamma", 999, None),
    ];
    for (author, title, views, note) in posts {
        repo::insert(
            exec,
            &Post {
                id: 0,
                author_id: author as u64,
                title: title.into(),
                views,
                note: note.map(Into::into),
            },
        )
        .await
        .unwrap();
    }
}

async fn run(exec: SeaOrmExecutor) {
    setup(&exec).await;

    // where_like + order + limit
    let like: Vec<Post> = repo::query::<Post>()
        .where_like("title", "alpha%")
        .order_by_asc("title")
        .all(&exec)
        .await
        .unwrap();
    assert_eq!(like.iter().map(|p| p.title.clone()).collect::<Vec<_>>(), ["alpha", "alphabet"]);

    // where_gt + order_by_desc
    let popular: Vec<Post> = repo::query::<Post>()
        .where_gt("views", 40i64)
        .order_by_desc("views")
        .all(&exec)
        .await
        .unwrap();
    assert_eq!(popular.iter().map(|p| p.views).collect::<Vec<_>>(), [999, 100, 50]);

    // where_in
    let in_set: Vec<Post> =
        repo::query::<Post>().where_in("title", ["alpha", "gamma"]).all(&exec).await.unwrap();
    assert_eq!(in_set.len(), 2);

    // where_null / where_not_null
    let no_note = repo::query::<Post>().where_null("note").count(&exec).await.unwrap();
    let with_note = repo::query::<Post>().where_not_null("note").count(&exec).await.unwrap();
    assert_eq!(no_note, 2);
    assert_eq!(with_note, 2);

    // where_ne
    let not_beta = repo::query::<Post>().where_ne("title", "beta").count(&exec).await.unwrap();
    assert_eq!(not_beta, 3);

    // OR group via filter()
    let alpha_or_gamma: Vec<Post> = repo::query::<Post>()
        .filter(
            Cond::any()
                .add(
                    rainier_orm::sea_query::Expr::col((
                        rainier_orm::sea_query::Alias::new("posts"),
                        rainier_orm::sea_query::Alias::new("title"),
                    ))
                    .eq("alpha"),
                )
                .add(
                    rainier_orm::sea_query::Expr::col((
                        rainier_orm::sea_query::Alias::new("posts"),
                        rainier_orm::sea_query::Alias::new("title"),
                    ))
                    .eq("gamma"),
                ),
        )
        .all(&exec)
        .await
        .unwrap();
    assert_eq!(alpha_or_gamma.len(), 2);

    // join filter: posts whose author is active (alice) — bob's gamma excluded
    let active_authored: Vec<Post> = repo::query::<Post>()
        .join("authors", "author_id", "id")
        .where_eq("authors.active", true)
        .all(&exec)
        .await
        .unwrap();
    assert_eq!(active_authored.len(), 3);
    assert!(active_authored.iter().all(|p| p.title != "gamma"));

    // first
    let first_pop: Option<Post> =
        repo::query::<Post>().order_by_desc("views").first(&exec).await.unwrap();
    assert_eq!(first_pop.map(|p| p.title), Some("gamma".to_string()));

    // first_or_create: existing → returns it (no new row)
    let before = repo::query::<Post>().count(&exec).await.unwrap();
    let existing = repo::query::<Post>()
        .where_eq("title", "alpha")
        .first_or_create(
            &exec,
            Post { id: 0, author_id: 1, title: "alpha".into(), views: 0, note: None },
        )
        .await
        .unwrap();
    assert_eq!(existing.views, 100); // the real row, not the default
    assert_eq!(repo::query::<Post>().count(&exec).await.unwrap(), before);

    // first_or_create: missing → inserts and returns the new row
    let created = repo::query::<Post>()
        .where_eq("title", "delta")
        .first_or_create(
            &exec,
            Post { id: 0, author_id: 1, title: "delta".into(), views: 7, note: None },
        )
        .await
        .unwrap();
    assert!(created.id > 0);
    assert_eq!(created.title, "delta");
    assert_eq!(repo::query::<Post>().count(&exec).await.unwrap(), before + 1);

    // delete WHERE
    let deleted = repo::query::<Post>().where_eq("title", "delta").delete(&exec).await.unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(repo::query::<Post>().count(&exec).await.unwrap(), before);
}

#[tokio::test]
async fn query_sqlite() {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless()).await.unwrap();
    run(exec).await;
}

#[tokio::test]
async fn query_mysql() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL query-builder check");
        return;
    };
    let exec = SeaOrmExecutor::connect(&url, &PoolConfig::default()).await.unwrap();
    for t in ["posts", "authors"] {
        let _ = exec.execute(&format!("DROP TABLE IF EXISTS {t}"), vec![]).await;
    }
    run(exec).await;
}
