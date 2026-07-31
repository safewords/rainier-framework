//! Factories against a real database.
//!
//! The unit tests assert what a factory builds. This asserts the half that
//! only shows up against a schema: that the rows insert, that a
//! database-assigned key comes back on the model a test goes on to use, and
//! that a unique column really does need a sequence — which is the mistake
//! everybody makes once.
#![cfg(feature = "sea-orm-executor")]

use std::sync::Arc;

use rainier_database::{
    Database, EntityRepository, Factory, HasFactory, Migrator, Model, Repository,
};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Entity, PoolConfig};

#[derive(Debug, Clone, PartialEq, Default, Entity, rainier_orm::Factory)]
#[orm(table = "users")]
struct User {
    #[orm(pk, auto_increment)]
    id: u64,
    #[orm(unique)]
    email: String,
    admin: bool,
}

impl Model for User {}

async fn users() -> Arc<EntityRepository<User>> {
    let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::in_memory())
        .await
        .expect("connect");
    let database = Database::new(executor);

    Migrator::new().create_table::<User>("0001_users").run(&database).await.expect("migrate");

    Arc::new(EntityRepository::<User>::new(database))
}

/// The shape a real suite settles on: the sequence lives with the factory
/// rather than in every test.
fn user_factory() -> Factory<User> {
    User::factory().sequence(|user, i| user.email = format!("user{i}@example.com"))
}

#[tokio::test]
async fn a_factory_inserts_rows() {
    let users = users().await;

    let created = user_factory().count(3).create(&*users).await.expect("create");

    assert_eq!(created.len(), 3);
    assert_eq!(users.count().await.unwrap(), 3);
}

#[tokio::test]
async fn the_database_assigned_key_comes_back() {
    // Otherwise a test holds a model with `id: 0` and every later assertion
    // about it is about a row that does not exist.
    let users = users().await;

    let created = user_factory().count(2).create(&*users).await.expect("create");

    assert!(created.iter().all(|user| user.id > 0), "{created:?}");
    assert_ne!(created[0].id, created[1].id);
}

#[tokio::test]
async fn a_state_reaches_the_stored_row() {
    let users = users().await;

    user_factory().state(|user| user.admin = true).create_one(&*users).await.expect("create");

    let stored = users.all().await.unwrap();
    assert!(stored[0].admin);
}

#[tokio::test]
async fn without_a_sequence_a_unique_column_collides() {
    // The mistake everybody makes once, pinned so the documentation about it
    // stays true. Three defaults are three identical emails.
    let users = users().await;

    let failed = User::factory().count(2).create(&*users).await;

    assert!(failed.is_err(), "a UNIQUE index should have refused the second row");
}

#[tokio::test]
async fn make_touches_no_database_at_all() {
    // For a test about a serialiser or a policy, which needs a model and not a
    // row — and should not pay for a schema to get one.
    let built = user_factory().count(2).make();

    assert_eq!(built.len(), 2);
    assert_eq!(built[0].id, 0, "nothing assigned a key, because nothing inserted it");
    assert_eq!(built[1].email, "user1@example.com");
}

#[tokio::test]
async fn the_derived_factory_builds_from_default() {
    let user = User::factory().make_one();

    assert_eq!(user, User::default());
}
