//! End-to-end proof of the relationships against a real database.
//!
//! The unit tests in `relation.rs` run against a fake connection, which records
//! SQL without executing it — the right tool for "is this one query?" and the
//! wrong one for "does the grouping put the right rows under the right
//! parent?". A `GROUP BY` and an `IN (…)` have to actually run somewhere.
//!
//! SQLite in memory, with a pool of one so the database survives between
//! statements.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::{
    BelongsTo, BelongsToMany, Criteria, Database, EntityRepository, HasMany, HasOne, Migrator,
    Model, Relation, Repository,
};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Entity, PoolConfig};

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "users")]
struct User {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
}
impl Model for User {}

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "posts")]
struct Post {
    #[orm(pk, auto_increment)]
    id: u64,
    title: String,
    user_id: u64,
}
impl Model for Post {}

/// A one-to-one: a user has one profile.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "profiles")]
struct Profile {
    #[orm(pk, auto_increment)]
    id: u64,
    #[orm(unique)]
    user_id: u64,
    bio: String,
}
impl Model for Profile {}

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "tags")]
struct Tag {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
}
impl Model for Tag {}

struct World {
    users: EntityRepository<User>,
    posts: EntityRepository<Post>,
    profiles: EntityRepository<Profile>,
    tags: EntityRepository<Tag>,
    db: Database,
}

/// Two users, three posts (2 for Ada, 1 for Grace), one profile, two tags, and
/// a pivot linking post 1 to both tags and post 2 to one.
async fn world() -> World {
    let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    let db = Database::new(executor);

    Migrator::new()
        .create_table::<User>("0001_users")
        .create_table::<Post>("0002_posts")
        .create_table::<Profile>("0003_profiles")
        .create_table::<Tag>("0004_tags")
        .raw(
            "0005_post_tag",
            vec!["CREATE TABLE post_tag (post_id BIGINT NOT NULL, tag_id BIGINT NOT NULL)".into()],
            vec!["DROP TABLE post_tag".into()],
        )
        .run(&db)
        .await
        .expect("migrate");

    let users = EntityRepository::<User>::new(db.clone());
    let posts = EntityRepository::<Post>::new(db.clone());
    let profiles = EntityRepository::<Profile>::new(db.clone());
    let tags = EntityRepository::<Tag>::new(db.clone());

    users.create(User { id: 0, name: "Ada".into() }).await.expect("ada");
    users.create(User { id: 0, name: "Grace".into() }).await.expect("grace");
    users.create(User { id: 0, name: "Nobody".into() }).await.expect("nobody");

    posts.create(Post { id: 0, title: "First".into(), user_id: 1 }).await.expect("post");
    posts.create(Post { id: 0, title: "Second".into(), user_id: 1 }).await.expect("post");
    posts.create(Post { id: 0, title: "Third".into(), user_id: 2 }).await.expect("post");

    profiles.create(Profile { id: 0, user_id: 1, bio: "Countess".into() }).await.expect("profile");

    tags.create(Tag { id: 0, name: "rust".into() }).await.expect("tag");
    tags.create(Tag { id: 0, name: "tokio".into() }).await.expect("tag");

    for (post_id, tag_id) in [(1, 1), (1, 2), (2, 1)] {
        db.statement(&format!("INSERT INTO post_tag VALUES ({post_id}, {tag_id})"))
            .await
            .expect("link");
    }

    World { users, posts, profiles, tags, db }
}

#[tokio::test]
async fn has_many_puts_the_right_children_under_the_right_parent() {
    let world = world().await;
    let users = world.users.all().await.unwrap();

    let posts = HasMany::<User, Post>::new().load(&users, &world.posts).await.unwrap();

    assert_eq!(posts.of(&users[0]).len(), 2, "Ada");
    assert_eq!(posts.of(&users[1]).len(), 1, "Grace");
    assert_eq!(posts.of(&users[2]), &[], "a user with none");
    assert_eq!(posts.of(&users[1])[0].title, "Third");
}

#[tokio::test]
async fn belongs_to_resolves_the_owner_of_each_row() {
    let world = world().await;
    let posts = world.posts.all().await.unwrap();

    let authors = BelongsTo::<Post, User>::new().load(&posts, &world.users).await.unwrap();

    assert_eq!(authors.one(&posts[0]).unwrap().name, "Ada");
    assert_eq!(authors.one(&posts[2]).unwrap().name, "Grace");
}

#[tokio::test]
async fn has_one_reads_the_single_row() {
    let world = world().await;
    let users = world.users.all().await.unwrap();

    let profiles = HasOne::<User, Profile>::new().load(&users, &world.profiles).await.unwrap();

    assert_eq!(profiles.one(&users[0]).unwrap().bio, "Countess");
    assert!(profiles.one(&users[1]).is_none(), "no profile is not an error");
}

#[tokio::test]
async fn belongs_to_many_fans_out_along_the_pivot() {
    let world = world().await;
    let posts = world.posts.all().await.unwrap();

    let tags = BelongsToMany::<Post, Tag>::new("post_tag").load(&posts, &world.tags).await.unwrap();

    assert_eq!(tags.of(&posts[0]).len(), 2, "linked to both");
    assert_eq!(tags.of(&posts[1]).len(), 1);
    assert_eq!(tags.of(&posts[2]), &[], "linked to none");

    // The same tag is under two parents, from one fetch of the tag rows.
    assert_eq!(tags.of(&posts[0])[0].name, "rust");
    assert_eq!(tags.of(&posts[1])[0].name, "rust");
}

#[tokio::test]
async fn counting_is_one_grouped_query_and_zero_for_the_childless() {
    let world = world().await;
    let users = world.users.all().await.unwrap();

    let counts = HasMany::<User, Post>::new().count(&users, &world.posts).await.unwrap();

    assert_eq!(counts.of(&users[0]), 2);
    assert_eq!(counts.of(&users[1]), 1);
    assert_eq!(counts.of(&users[2]), 0, "no group at all, reported as none");
    assert_eq!(counts.total(), 3);
}

#[tokio::test]
async fn counting_through_a_pivot_counts_links() {
    let world = world().await;
    let posts = world.posts.all().await.unwrap();

    let counts =
        BelongsToMany::<Post, Tag>::new("post_tag").count(&posts, &world.tags).await.unwrap();

    assert_eq!(counts.of(&posts[0]), 2);
    assert_eq!(counts.of(&posts[2]), 0);
}

#[tokio::test]
async fn a_constrained_relation_narrows_what_it_loads() {
    let world = world().await;
    let users = world.users.all().await.unwrap();

    let posts = HasMany::<User, Post>::new()
        .matching(Criteria::new().where_eq("title", "First"))
        .load(&users, &world.posts)
        .await
        .unwrap();

    assert_eq!(posts.of(&users[0]).len(), 1);
    assert_eq!(posts.of(&users[1]), &[], "Grace's post did not match the filter");
}

#[tokio::test]
async fn zipping_pairs_every_parent_including_the_empty_ones() {
    let world = world().await;
    let users = world.users.all().await.unwrap();

    let pairs =
        HasMany::<User, Post>::new().load(&users, &world.posts).await.unwrap().zip(users.clone());

    assert_eq!(pairs.len(), 3);
    assert_eq!(pairs[2].0.name, "Nobody");
    assert!(pairs[2].1.is_empty());
}

#[tokio::test]
async fn one_parent_is_still_the_batched_path() {
    let world = world().await;
    let ada = world.users.find(1_u64.into()).await.unwrap().unwrap();

    let posts = HasMany::<User, Post>::new().for_one(&ada, &world.posts).await.unwrap();

    assert_eq!(posts.len(), 2);
}

#[tokio::test]
async fn grouping_by_a_column_that_does_not_exist_says_so() {
    let world = world().await;

    let err =
        world.posts.count_grouped("nonsense", Criteria::new()).await.expect_err("no such column");

    assert!(err.message().contains("nonsense"), "{}", err.message());
    assert!(err.message().contains("posts"), "{}", err.message());
    drop(world.db);
}
