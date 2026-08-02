//! Composite primary keys against a real database.
//!
//! The rendering tests in `rainier-orm` prove the right SQL is built. They
//! cannot prove SQLite accepts it, that the generated `PRIMARY KEY (a, b)`
//! actually constrains a pair rather than a column, or that an `UPDATE` keyed on
//! two columns leaves the neighbouring rows alone. Those need statements to run.
//!
//! The table here is the ordinary reason to want a composite key — a join table
//! keyed `(team_id, user_id)` — and it is deliberately populated with rows that
//! *share* each half of the key. That is what gives the tests something to get
//! wrong: a predicate missing either part matches more than one row, so the
//! assertions on row counts detect it even without looking at the SQL.
#![cfg(feature = "sea-orm-executor")]

use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, Dialect, Entity, Executor, PoolConfig};

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "memberships")]
struct Membership {
    #[orm(pk)]
    team_id: u64,
    #[orm(pk)]
    user_id: u64,
    role: String,
}

/// A membership table holding four rows across two teams and two users, so
/// every row shares its `team_id` with another and its `user_id` with another.
/// Only the *pair* is unique.
async fn world() -> SeaOrmExecutor {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");

    // Straight from the entity's own metadata — so this also proves the
    // composite `PRIMARY KEY` clause is something SQLite will actually create.
    for sql in rainier_orm::schema::schema_ddl::<Membership>(Dialect::Sqlite) {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }

    for (team_id, user_id, role) in
        [(1, 10, "owner"), (1, 20, "member"), (2, 10, "member"), (2, 20, "member")]
    {
        repo::insert(&exec, &Membership { team_id, user_id, role: role.into() })
            .await
            .expect("insert");
    }

    exec
}

async fn all(exec: &SeaOrmExecutor) -> Vec<Membership> {
    let mut rows = repo::all::<Membership, _>(exec).await.expect("all");
    rows.sort_by_key(|m| (m.team_id, m.user_id));
    rows
}

#[tokio::test]
async fn a_composite_key_round_trips_through_find_update_and_delete() {
    let exec = world().await;

    // find
    let found: Membership = repo::find_by_keys(&exec, vec![1_u64.into(), 20_u64.into()])
        .await
        .expect("find")
        .expect("the row exists");
    assert_eq!(found, Membership { team_id: 1, user_id: 20, role: "member".into() });

    // update
    let affected =
        repo::update(&exec, &Membership { team_id: 1, user_id: 20, role: "admin".into() })
            .await
            .expect("update");
    assert_eq!(affected, 1, "exactly the one row named by the pair");

    let reloaded: Membership = repo::find_by_keys(&exec, vec![1_u64.into(), 20_u64.into()])
        .await
        .expect("find")
        .expect("still there");
    assert_eq!(reloaded.role, "admin");

    // delete
    let deleted = repo::delete_by_keys::<Membership, _>(&exec, vec![1_u64.into(), 20_u64.into()])
        .await
        .expect("delete");
    assert_eq!(deleted, 1);

    let remaining = all(&exec).await;
    assert_eq!(remaining.len(), 3);
    assert!(!remaining.iter().any(|m| m.team_id == 1 && m.user_id == 20));
}

#[tokio::test]
async fn an_update_leaves_the_rows_sharing_half_the_key_alone() {
    // The bug this whole feature has to avoid. `(1, 10)` shares `team_id` with
    // `(1, 20)` and `user_id` with `(2, 10)`. A `WHERE` that dropped either half
    // would rewrite one of those, and the row count alone would not look wrong.
    let exec = world().await;

    repo::update(&exec, &Membership { team_id: 1, user_id: 10, role: "changed".into() })
        .await
        .expect("update");

    let rows = all(&exec).await;
    assert_eq!(
        rows,
        vec![
            Membership { team_id: 1, user_id: 10, role: "changed".into() },
            Membership { team_id: 1, user_id: 20, role: "member".into() },
            Membership { team_id: 2, user_id: 10, role: "member".into() },
            Membership { team_id: 2, user_id: 20, role: "member".into() },
        ],
        "only the named pair may change"
    );
}

#[tokio::test]
async fn a_delete_removes_one_row_not_the_bucket() {
    let exec = world().await;

    let deleted = repo::delete_by_keys::<Membership, _>(&exec, vec![1_u64.into(), 10_u64.into()])
        .await
        .expect("delete");

    assert_eq!(deleted, 1, "a partial key would have removed two rows");
    assert_eq!(all(&exec).await.len(), 3);
}

#[tokio::test]
async fn a_find_is_not_satisfied_by_either_half_alone() {
    let exec = world().await;

    // `(1, 10)` and `(2, 20)` both exist; the crossed pairs are the ones that
    // tell a real pair lookup apart from a match on one column.
    assert!(repo::find_by_keys::<Membership, _>(&exec, vec![1_u64.into(), 10_u64.into()])
        .await
        .unwrap()
        .is_some());

    assert!(
        repo::find_by_keys::<Membership, _>(&exec, vec![1_u64.into(), 99_u64.into()])
            .await
            .unwrap()
            .is_none(),
        "a known team with an unknown user must not match"
    );
    assert!(
        repo::find_by_keys::<Membership, _>(&exec, vec![99_u64.into(), 10_u64.into()])
            .await
            .unwrap()
            .is_none(),
        "an unknown team with a known user must not match"
    );
}

#[tokio::test]
async fn the_database_enforces_the_pair_rather_than_either_column() {
    let exec = world().await;

    // A repeat of `team_id` alone is fine — `(1, 30)` is a new pair.
    repo::insert(&exec, &Membership { team_id: 1, user_id: 30, role: "member".into() })
        .await
        .expect("a new pair sharing a team is allowed");

    // The same pair again is not: the generated constraint covers both columns.
    let duplicate =
        repo::insert(&exec, &Membership { team_id: 1, user_id: 30, role: "other".into() }).await;
    assert!(duplicate.is_err(), "the composite PRIMARY KEY must reject a repeated pair");
}

#[tokio::test]
async fn a_partial_key_errors_without_touching_the_table() {
    let exec = world().await;

    let deleted = repo::delete_by_keys::<Membership, _>(&exec, vec![1_u64.into()]).await;
    assert!(deleted.is_err(), "one value cannot name a row in a two-column key");

    assert_eq!(all(&exec).await.len(), 4, "the table is untouched");
}
