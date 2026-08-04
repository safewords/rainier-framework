//! Soft-delete scoping against a real database — the rows, not the SQL.
//!
//! `tests/soft_deletes.rs` proves the right statement is built. It cannot prove
//! the database agrees, and more to the point it cannot tell a predicate that
//! filters from one that parses. Only running it over rows that differ shows
//! that.
//!
//! So the fixture keeps live and tombstoned rows **in the same tables**, and
//! every assertion is over names rather than counts: a scope that matched
//! nothing and a scope that matched the right thing both produce plausible
//! counts, and only the names say which.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::{
    Criteria, Database, EntityRepository, HasMany, Model, Relation, Repository,
};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, Dialect, Entity, Executor, PoolConfig};

/// The parent, soft-deleting.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "documents")]
struct Document {
    #[orm(pk)]
    id: u64,
    title: String,
    #[orm(soft_delete)]
    deleted_at: Option<String>,
}

impl Model for Document {}

/// The child, also soft-deleting, so a relation load has tombstoned rows to
/// exclude on its own account.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "comments")]
struct Comment {
    #[orm(pk)]
    id: u64,
    document_id: u64,
    body: String,
    #[orm(soft_delete)]
    deleted_at: Option<String>,
}

impl Model for Comment {}

/// A stamp. The value never matters — only whether it is there.
const WHEN: &str = "2026-08-03T00:00:00Z";

/// Two live documents and one tombstoned, each with a live and a tombstoned
/// comment.
///
/// Every table holds both kinds, so "the scope did nothing" and "the scope
/// worked" produce different names rather than merely different totals.
async fn world() -> Database {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");

    for sql in rainier_orm::schema::schema_ddl::<Document>(Dialect::Sqlite)
        .into_iter()
        .chain(rainier_orm::schema::schema_ddl::<Comment>(Dialect::Sqlite))
    {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }

    for (id, title, deleted_at) in
        [(1, "live one", None), (2, "live two", None), (3, "trashed", Some(WHEN.to_string()))]
    {
        repo::insert(&exec, &Document { id, title: title.into(), deleted_at })
            .await
            .expect("insert document");
    }

    for (id, document_id, body, deleted_at) in [
        (10, 1, "live comment", None),
        (11, 1, "trashed comment", Some(WHEN.to_string())),
        (20, 2, "other live comment", None),
    ] {
        repo::insert(&exec, &Comment { id, document_id, body: body.into(), deleted_at })
            .await
            .expect("insert comment");
    }

    Database::new(exec)
}

fn documents(db: &Database) -> EntityRepository<Document> {
    EntityRepository::new(db.clone())
}

async fn titles(db: &Database, criteria: Criteria) -> Vec<String> {
    let mut found: Vec<String> = documents(db)
        .matching(criteria)
        .await
        .expect("query")
        .into_iter()
        .map(|d| d.title)
        .collect();
    found.sort();
    found
}

#[tokio::test]
async fn matching_does_not_return_a_tombstoned_row() {
    let db = world().await;

    assert_eq!(titles(&db, Criteria::new()).await, ["live one", "live two"]);
}

#[tokio::test]
async fn a_filter_of_its_own_does_not_displace_the_scope() {
    let db = world().await;

    // The predicate that would match the tombstoned row if the scope were gone.
    assert_eq!(titles(&db, Criteria::new().where_like("title", "%a%")).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_count_matches_what_the_page_contains() {
    let db = world().await;

    let total = documents(&db).count_matching(Criteria::new()).await.expect("count");
    let rows = documents(&db).matching(Criteria::new()).await.expect("rows");

    assert_eq!(total, 2, "the tombstoned row is not counted");
    assert_eq!(total as usize, rows.len(), "a page cannot disagree with its own total");
}

#[tokio::test]
async fn a_paginator_agrees_with_itself() {
    let db = world().await;

    let page = documents(&db).paginate_matching(Criteria::new(), 1, 10).await.expect("paginate");

    assert_eq!(page.total, 2);
    assert_eq!(page.data.len(), 2);
}

#[tokio::test]
async fn exists_does_not_see_a_tombstoned_row() {
    let db = world().await;

    let found =
        documents(&db).exists(Criteria::new().where_eq("title", "trashed")).await.expect("exists");

    assert!(!found, "a tombstoned row must not satisfy an existence check");
}

#[tokio::test]
async fn with_trashed_returns_both_kinds() {
    let db = world().await;

    assert_eq!(
        titles(&db, Criteria::new().with_trashed()).await,
        ["live one", "live two", "trashed"]
    );
}

#[tokio::test]
async fn only_trashed_returns_the_trash_and_nothing_else() {
    let db = world().await;

    // The assertion that could not have passed before the scope was applied: an
    // inert `only_trashed` returned all three.
    assert_eq!(titles(&db, Criteria::new().only_trashed()).await, ["trashed"]);
}

#[tokio::test]
async fn a_relation_load_excludes_tombstoned_children() {
    let db = world().await;

    let parents = documents(&db).matching(Criteria::new()).await.expect("parents");
    let comments: EntityRepository<Comment> = EntityRepository::new(db.clone());
    let related = HasMany::<Document, Comment>::new()
        .foreign_key("document_id")
        .load(&parents, &comments)
        .await
        .expect("load");

    let first = parents.iter().find(|d| d.title == "live one").expect("parent");
    let bodies: Vec<&str> = related.of(first).iter().map(|c| c.body.as_str()).collect();

    assert_eq!(bodies, ["live comment"], "the tombstoned child must not load");
}

#[tokio::test]
async fn a_relation_count_matches_what_the_load_returns() {
    let db = world().await;

    let parents = documents(&db).matching(Criteria::new()).await.expect("parents");
    let comments: EntityRepository<Comment> = EntityRepository::new(db.clone());
    let relation = HasMany::<Document, Comment>::new().foreign_key("document_id");

    let counts = relation.count(&parents, &comments).await.expect("count");
    let loaded = relation.load(&parents, &comments).await.expect("load");
    let first = parents.iter().find(|d| d.title == "live one").expect("parent");

    assert_eq!(counts.of(first), 1, "counting must exclude the tombstoned child too");
    assert_eq!(counts.of(first) as usize, loaded.of(first).len());
}

#[tokio::test]
async fn a_purge_can_still_remove_tombstoned_rows() {
    let db = world().await;

    // The write path stays unscoped, which is what makes a purge possible. A
    // scoped `DELETE` would match nothing here, forever.
    let removed = documents(&db)
        .delete_matching(Criteria::new().where_not_null("deleted_at"))
        .await
        .expect("purge");

    assert_eq!(removed, 1);
    assert_eq!(titles(&db, Criteria::new().with_trashed()).await, ["live one", "live two"]);
}

#[tokio::test]
async fn a_bulk_restore_reaches_the_rows_it_is_restoring() {
    let db = world().await;

    let restored = documents(&db)
        .update_column(
            Criteria::new().where_not_null("deleted_at"),
            "deleted_at",
            Option::<String>::None,
        )
        .await
        .expect("restore");

    assert_eq!(restored, 1);
    assert_eq!(titles(&db, Criteria::new()).await, ["live one", "live two", "trashed"]);
}
