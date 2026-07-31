//! Proof that unique constraints, indexes, and foreign keys derived from
//! struct attributes are actually enforced — and that relationship traversal
//! works as an explicit `find_by` query against the flat FK column.
//!
//! Runs on SQLite in-memory always (with `PRAGMA foreign_keys = ON` so FKs are
//! enforced), and on the MySQL container when `TEST_DATABASE_URL` is set.
#![cfg(feature = "sea-orm-executor")]

use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, schema, Entity, Executor, PoolConfig};

#[derive(Debug, Clone, Entity)]
#[orm(table = "authors")]
struct Author {
    #[orm(pk, auto_increment)]
    id: u64,
    #[orm(unique)]
    email: String,
    name: String,
}

#[derive(Debug, Clone, Entity)]
#[orm(table = "posts")]
#[orm(index = "author_id, slug")] // composite index
struct Post {
    #[orm(pk, auto_increment)]
    id: u64,
    // FK *id*, indexed, with cascade delete — a constraint, not a relationship.
    #[orm(index, references = "authors(id)", on_delete = "cascade")]
    author_id: u64,
    #[orm(unique)]
    slug: String,
    title: String,
}

fn author(email: &str) -> Author {
    Author { id: 0, email: email.into(), name: "n".into() }
}

fn post(author_id: u64, slug: &str) -> Post {
    Post { id: 0, author_id, slug: slug.into(), title: "t".into() }
}

async fn create_schema(exec: &SeaOrmExecutor) {
    // authors before posts — the FK depends on it.
    for ddl in schema::schema_ddl::<Author>(exec.dialect()) {
        exec.execute(&ddl, vec![]).await.expect("authors ddl");
    }
    for ddl in schema::schema_ddl::<Post>(exec.dialect()) {
        exec.execute(&ddl, vec![]).await.expect("posts ddl");
    }
}

async fn run(exec: SeaOrmExecutor) {
    create_schema(&exec).await;

    let alice = repo::insert(&exec, &author("alice@x.io")).await.expect("insert author");

    // unique email — a second author with the same email is rejected.
    assert!(
        repo::insert(&exec, &author("alice@x.io")).await.is_err(),
        "duplicate email should violate the UNIQUE constraint"
    );

    // FK enforced — a post referencing a non-existent author is rejected.
    assert!(
        repo::insert(&exec, &post(999_999, "ghost")).await.is_err(),
        "post with a dangling author_id should violate the FK"
    );

    // valid posts insert.
    repo::insert(&exec, &post(alice as u64, "hello")).await.expect("post 1");
    repo::insert(&exec, &post(alice as u64, "world")).await.expect("post 2");

    // unique slug — duplicate slug rejected.
    assert!(
        repo::insert(&exec, &post(alice as u64, "hello")).await.is_err(),
        "duplicate slug should violate the UNIQUE constraint"
    );

    // relationship traversal is an explicit query on the flat FK column.
    let alices_posts: Vec<Post> =
        repo::find_by(&exec, "author_id", alice as u64).await.expect("find_by author_id");
    assert_eq!(alices_posts.len(), 2);

    let by_slug: Option<Post> =
        repo::find_one_by(&exec, "slug", "world").await.expect("find_one_by slug");
    assert_eq!(by_slug.map(|p| p.title), Some("t".to_string()));

    // ON DELETE CASCADE — removing the author removes their posts.
    repo::delete_by_pk::<Author, _, _>(&exec, alice as u64).await.expect("delete author");
    let remaining: Vec<Post> =
        repo::find_by(&exec, "author_id", alice as u64).await.expect("find_by after cascade");
    assert!(remaining.is_empty(), "cascade should have removed the posts");
}

#[tokio::test]
async fn constraints_sqlite() {
    // serverless preset = max_connections 1, so the PRAGMA sticks for the run.
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect sqlite");
    // SQLite/D1 only *enforce* foreign keys when this is on.
    exec.execute("PRAGMA foreign_keys = ON", vec![]).await.expect("enable fk enforcement");
    run(exec).await;
}

#[tokio::test]
async fn constraints_mysql() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL constraint check");
        return;
    };
    let exec = SeaOrmExecutor::connect(&url, &PoolConfig::default()).await.expect("connect mysql");
    // Drop in FK order from a previous run.
    for t in ["posts", "authors"] {
        let _ = exec.execute(&format!("DROP TABLE IF EXISTS {t}"), vec![]).await;
    }
    run(exec).await;
}

/// Regression (no database needed): a *keyed* `Text` column — here the unique
/// `email` — must render as `VARCHAR` on MySQL, because MySQL can't index a
/// `TEXT` column without a prefix length. A non-keyed string (`name`) stays
/// `TEXT`. This is what makes `constraints_mysql`'s `CREATE TABLE` valid.
#[test]
fn keyed_text_is_varchar_on_mysql() {
    use rainier_orm::Dialect;
    let ddl = schema::create_table_ddl::<Author>(Dialect::MySql).to_lowercase();
    assert!(ddl.contains("varchar"), "keyed `email` must be VARCHAR: {ddl}");
    assert!(ddl.contains("text"), "non-keyed `name` should stay TEXT: {ddl}");
}
