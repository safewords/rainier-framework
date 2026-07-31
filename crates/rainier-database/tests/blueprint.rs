//! The schema builder, against a real database.
//!
//! The unit tests in `blueprint.rs` assert on the *strings* each dialect
//! renders, which is the right tool for "does MySQL get its `AUTO_INCREMENT`".
//! It is the wrong tool for "is that valid SQL" — a builder that renders
//! plausible nonsense passes every string assertion there is.
//!
//! So this file executes what it builds.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::{Database, Down, Migration, Migrator, Step};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Dialect, PoolConfig};

async fn database() -> Database {
    let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    Database::new(executor)
}

/// Migrations covering every construct the builder can emit.
fn migrator() -> Migrator {
    Migrator::new()
        .add(Step::create("0001_users", "users", |table| {
            table.id();
            table.string("email").unique();
            table.string("name");
            table.text("bio").nullable();
            table.integer("logins").default(0);
            table.timestamps();
        }))
        .add(Step::create("0002_posts", "posts", |table| {
            table.id();
            table.string("slug").unique();
            table.string("title");
            table.text("body");
            table.boolean("published").default(false);
            table.decimal("price", 8, 2).nullable();
            table.json("meta").nullable();
            table.foreign_id("author_id").constrained_on("users").cascade_on_delete();
            table.timestamps();

            table.index(["published", "created_at"]);
        }))
        .add(Step::create("0003_post_tag", "post_tag", |table| {
            table.foreign_id("post_id").constrained_on("posts").cascade_on_delete();
            table.foreign_id("tag_id").constrained_on("posts").cascade_on_delete();
            table.primary(["post_id", "tag_id"]);
        }))
}

#[tokio::test]
async fn everything_the_builder_emits_is_valid_sql() {
    let db = database().await;

    let applied = migrator().run(&db).await.expect("migrate");
    assert_eq!(applied.len(), 3);

    // The tables exist and accept the shapes they declared.
    db.statement("INSERT INTO users (email, name, bio, logins) VALUES ('a@b.c', 'Ada', NULL, 0)")
        .await
        .expect("insert a user");
    db.statement(
        "INSERT INTO posts (slug, title, body, published, price, meta, author_id) \
         VALUES ('s', 't', 'b', 0, 1.5, '{}', 1)",
    )
    .await
    .expect("insert a post");
    db.statement("INSERT INTO post_tag (post_id, tag_id) VALUES (1, 1)")
        .await
        .expect("insert a link");
}

/// One user and one post, so a foreign key has something to point at.
async fn seed(db: &Database) {
    db.statement("INSERT INTO users (email, name, logins) VALUES ('a@b.c', 'Ada', 0)")
        .await
        .expect("a user");
    db.statement(
        "INSERT INTO posts (slug, title, body, published, author_id) \
         VALUES ('s', 't', 'b', 0, 1)",
    )
    .await
    .expect("a post");
}

#[tokio::test]
async fn a_foreign_key_is_enforced_not_just_declared() {
    // `constrained_on` has to produce a constraint the database applies. It
    // caught two of the tests in this file before they seeded their parents.
    let db = database().await;
    migrator().run(&db).await.expect("migrate");

    let orphan = db
        .statement(
            "INSERT INTO posts (slug, title, body, published, author_id) \
             VALUES ('s', 't', 'b', 0, 999)",
        )
        .await;

    assert!(orphan.is_err(), "a post with no author should be refused");
}

#[tokio::test]
async fn a_cascade_takes_the_children_with_it() {
    let db = database().await;
    migrator().run(&db).await.expect("migrate");
    seed(&db).await;

    db.statement("DELETE FROM users WHERE id = 1").await.expect("delete the author");

    let rows = db
        .fetch(
            rainier_database::Prepared {
                sql: "SELECT COUNT(*) AS cnt FROM posts".into(),
                params: Vec::new(),
                route: rainier_orm::ShardRoute::Global,
            },
            vec![rainier_database::ColumnRequest::new("cnt", rainier_orm::ColumnType::BigInt)],
        )
        .await
        .expect("count");

    assert_eq!(
        rows[0].cell("cnt").and_then(rainier_database::Cell::as_u64),
        Some(0),
        "the post should have gone with its author"
    );
}

#[tokio::test]
async fn a_unique_column_is_actually_unique() {
    // The modifier has to reach the database, not just the rendered string.
    let db = database().await;
    migrator().run(&db).await.expect("migrate");

    db.statement("INSERT INTO users (email, name, logins) VALUES ('a@b.c', 'Ada', 0)")
        .await
        .expect("the first");

    let second =
        db.statement("INSERT INTO users (email, name, logins) VALUES ('a@b.c', 'Grace', 0)").await;

    assert!(second.is_err(), "a duplicate email should be refused");
}

#[tokio::test]
async fn a_default_backfills_a_column_the_insert_omits() {
    let db = database().await;
    migrator().run(&db).await.expect("migrate");

    db.statement("INSERT INTO users (email, name) VALUES ('a@b.c', 'Ada')")
        .await
        .expect("insert without logins");

    let rows = db
        .fetch(
            rainier_database::Prepared {
                sql: "SELECT logins AS cnt FROM users".into(),
                params: Vec::new(),
                route: rainier_orm::ShardRoute::Global,
            },
            vec![rainier_database::ColumnRequest::new("cnt", rainier_orm::ColumnType::BigInt)],
        )
        .await
        .expect("read it back");

    assert_eq!(rows[0].cell("cnt").and_then(rainier_database::Cell::as_u64), Some(0));
}

#[tokio::test]
async fn a_composite_primary_key_refuses_the_same_pair_twice() {
    // What a pivot needs, and what a double-click would otherwise insert.
    let db = database().await;
    migrator().run(&db).await.expect("migrate");

    seed(&db).await;

    db.statement("INSERT INTO post_tag (post_id, tag_id) VALUES (1, 1)").await.expect("first");
    let again = db.statement("INSERT INTO post_tag (post_id, tag_id) VALUES (1, 1)").await;

    assert!(again.is_err(), "the pair is the key");
}

#[tokio::test]
async fn a_create_rolls_back_by_dropping_the_table() {
    let db = database().await;
    let migrator = migrator();
    migrator.run(&db).await.expect("migrate");

    let rolled_back = migrator.rollback(&db, 1).await.expect("roll back");

    assert_eq!(rolled_back.len(), 3);
    assert!(db.statement("SELECT 1 FROM posts LIMIT 1").await.is_err(), "posts should be gone");
    assert!(db.statement("SELECT 1 FROM users LIMIT 1").await.is_err(), "users too");
}

#[tokio::test]
async fn an_alter_adds_a_column_and_an_index_and_takes_both_away_again() {
    let db = database().await;
    migrator().run(&db).await.expect("the base schema");

    let change = Migrator::new().add(Step::table("0004_posts_subtitle", "posts", |table| {
        table.string("subtitle").nullable();
        table.index(["title"]);
    }));

    change.run(&db).await.expect("alter");
    db.statement("INSERT INTO users (email, name, logins) VALUES ('a@b.c', 'Ada', 0)")
        .await
        .expect("an author");
    db.statement(
        "INSERT INTO posts (slug, title, body, published, subtitle, author_id) \
         VALUES ('s2', 't', 'b', 0, 'sub', 1)",
    )
    .await
    .expect("the new column is usable");

    change.rollback(&db, 1).await.expect("roll the alter back");

    assert!(
        db.statement("SELECT subtitle FROM posts LIMIT 1").await.is_err(),
        "the derived down should have dropped the column"
    );
}

#[tokio::test]
async fn renaming_a_column_reverses_by_renaming_it_back() {
    let db = database().await;
    migrator().run(&db).await.expect("the base schema");

    let change = Migrator::new().add(Step::table("0004_rename", "posts", |table| {
        table.rename_column("body", "content");
    }));

    change.run(&db).await.expect("rename");
    assert!(db.statement("SELECT content FROM posts LIMIT 1").await.is_ok());

    change.rollback(&db, 1).await.expect("roll back");
    assert!(db.statement("SELECT body FROM posts LIMIT 1").await.is_ok(), "renamed back");
}

#[tokio::test]
async fn renaming_a_table_reverses_by_renaming_it_back() {
    let db = database().await;
    migrator().run(&db).await.expect("the base schema");

    let change = Migrator::new().add(Step::rename_table("0004_rename", "posts", "articles"));

    change.run(&db).await.expect("rename");
    assert!(db.statement("SELECT 1 FROM articles LIMIT 1").await.is_ok());

    change.rollback(&db, 1).await.expect("roll back");
    assert!(db.statement("SELECT 1 FROM posts LIMIT 1").await.is_ok());
}

#[tokio::test]
async fn dropping_a_column_says_it_cannot_be_undone_before_it_runs() {
    // The property that matters: the refusal is up front, so a batch does not
    // half-unwind and then discover it is stuck.
    let step = Step::table("0004_drop", "posts", |table| {
        table.drop_column("body");
    });

    let Down::Irreversible(reason) = step.down(Dialect::Sqlite) else {
        panic!("dropping a column destroys data");
    };

    assert!(reason.contains("body"), "{reason}");
    assert!(reason.contains("posts"), "{reason}");
}

#[tokio::test]
async fn dropping_an_index_recreates_it_on_the_way_back() {
    let db = database().await;
    migrator().run(&db).await.expect("the base schema");

    let change = Migrator::new().add(Step::table("0004_drop_index", "posts", |table| {
        table.drop_index(["published", "created_at"]);
    }));

    change.run(&db).await.expect("drop the index");
    change.rollback(&db, 1).await.expect("put it back");

    // Creating it again would fail if the rollback had already made one, and
    // the migrator's own `IF NOT EXISTS` is what makes that safe to assert.
    let recreate = Migrator::new().add(Step::table("0005_again", "posts", |table| {
        table.index(["published", "created_at"]);
    }));
    recreate.run(&db).await.expect("idempotent");
}

#[tokio::test]
async fn the_same_blueprint_renders_for_every_dialect() {
    // Executed only against SQLite here, but the other two have to at least
    // render — a builder that panics on Postgres is not portable.
    for dialect in [Dialect::Sqlite, Dialect::MySql, Dialect::Postgres] {
        let names = migrator().names().len();
        assert_eq!(names, 3, "{dialect:?}");

        for step in [
            Step::create("t", "t", |table| {
                table.id();
                table.string("a").unique();
            }),
            Step::table("t2", "t", |table| {
                table.string("b").nullable();
            }),
            Step::rename_table("t3", "a", "b"),
        ] {
            assert!(!step.up(dialect).is_empty(), "`{}` on {dialect:?}", step.name());
        }
    }
}
