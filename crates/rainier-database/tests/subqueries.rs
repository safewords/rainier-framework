//! Correlated subqueries against a real database.
//!
//! The rendering tests in the statement layer prove the right SQL is built for
//! each dialect. They cannot prove SQLite accepts it, and — the part that
//! matters — they cannot tell a correlated predicate from an uncorrelated one.
//! Both render, both run, and the uncorrelated one returns *every* row instead
//! of erroring. Only running the statement against rows that differ shows it.
//!
//! So the fixture is built to make that difference visible. Every parent here
//! has a different number of children (two, one and **none**), and the rows the
//! predicates are supposed to exclude are real rows in the same tables rather
//! than absences. A dropped correlation then shows up as a row count, not as a
//! syntax error.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::{
    statement, Assignment, Comparison, Criteria, Database, EntityRepository, Model, Repository,
    Subquery,
};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, Dialect, Entity, Executor, PoolConfig};

/// The outer table, carrying a denormalised counter to recompute.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "parents")]
struct Parent {
    #[orm(pk)]
    id: u64,
    name: String,
    children_count: i64,
}

/// The inner table. `approved` exists so the subquery has a predicate of its
/// own whose value has to stay bound and correctly ordered.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "children")]
struct Child {
    #[orm(pk)]
    id: u64,
    parent_id: u64,
    approved: bool,
}

/// A membership table, for the "exactly two members, and I am one of them"
/// shape: two `EXISTS` and a correlated `COUNT` in the same `WHERE`.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "memberships")]
struct Membership {
    #[orm(pk)]
    id: u64,
    room_id: u64,
    member_id: u64,
}

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "rooms")]
struct Room {
    #[orm(pk)]
    id: u64,
    topic: String,
}

impl Model for Parent {}
impl Model for Room {}

/// Three parents with two, one and zero children, and a counter that starts
/// deliberately wrong on all three — including a non-zero one on the parent with
/// no children at all, which is the value a naive recompute leaves in place.
async fn world() -> Database {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");

    for sql in rainier_orm::schema::schema_ddl::<Parent>(Dialect::Sqlite)
        .into_iter()
        .chain(rainier_orm::schema::schema_ddl::<Child>(Dialect::Sqlite))
        .chain(rainier_orm::schema::schema_ddl::<Membership>(Dialect::Sqlite))
        .chain(rainier_orm::schema::schema_ddl::<Room>(Dialect::Sqlite))
    {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }

    for (id, name, children_count) in [(1, "two", 999_i64), (2, "one", 999), (3, "none", 999)] {
        repo::insert(&exec, &Parent { id, name: name.into(), children_count })
            .await
            .expect("insert parent");
    }

    // Parent 1: two children, one of them unapproved. Parent 2: one, approved.
    // Parent 3: none. Each parent is named for its child count, and the
    // unapproved child means "has any child" and "has an approved child" are
    // different questions — so a subquery that drops its own predicate still
    // answers, just wrongly.
    for (id, parent_id, approved) in [(10, 1, true), (11, 1, false), (20, 2, true)] {
        repo::insert(&exec, &Child { id, parent_id, approved }).await.expect("insert child");
    }

    for (id, topic) in [(1, "pair"), (2, "trio"), (3, "solo"), (4, "other pair")] {
        repo::insert(&exec, &Room { id, topic: topic.into() }).await.expect("insert room");
    }
    // Room 1: members 100 and 200. Room 2: 100, 200 and 300 — the room that a
    // pair of `EXISTS` alone would wrongly match. Room 3: 100 only. Room 4: 200
    // and 300, a pair that does not include member 100.
    for (id, room_id, member_id) in [
        (1, 1, 100),
        (2, 1, 200),
        (3, 2, 100),
        (4, 2, 200),
        (5, 2, 300),
        (6, 3, 100),
        (7, 4, 200),
        (8, 4, 300),
    ] {
        repo::insert(&exec, &Membership { id, room_id, member_id })
            .await
            .expect("insert membership");
    }

    Database::new(exec)
}

fn parents(db: &Database) -> EntityRepository<Parent> {
    EntityRepository::new(db.clone())
}

async fn names(db: &Database, criteria: Criteria) -> Vec<String> {
    let mut found: Vec<String> =
        parents(db).matching(criteria).await.expect("query").into_iter().map(|p| p.name).collect();
    found.sort();
    found
}

async fn counts(db: &Database) -> Vec<(String, i64)> {
    let mut rows: Vec<(String, i64)> = parents(db)
        .all()
        .await
        .expect("all")
        .into_iter()
        .map(|p| (p.name, p.children_count))
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn an_exists_returns_only_the_rows_with_a_matching_related_row() {
    let db = world().await;

    let with_children =
        Criteria::new().where_exists(Subquery::count("children").correlate("parent_id", "id"));

    assert_eq!(
        names(&db, with_children).await,
        vec!["one".to_string(), "two".to_string()],
        "an uncorrelated EXISTS would return all three, because `children` is non-empty"
    );
}

#[tokio::test]
async fn an_exists_applies_its_own_predicate_as_well_as_the_correlation() {
    let db = world().await;

    // Parent 1 has an unapproved child too, so this only differs from the
    // previous test if the bound `approved` actually reaches the inner query.
    let unapproved = Criteria::new().where_exists(
        Subquery::count("children").correlate("parent_id", "id").where_eq("approved", false),
    );

    assert_eq!(names(&db, unapproved).await, vec!["two".to_string()]);
}

#[tokio::test]
async fn a_not_exists_returns_exactly_the_complement() {
    let db = world().await;

    let childless =
        Criteria::new().where_not_exists(Subquery::count("children").correlate("parent_id", "id"));

    assert_eq!(names(&db, childless).await, vec!["none".to_string()]);
}

#[tokio::test]
async fn a_correlated_scalar_compares_the_count_for_each_row_separately() {
    let db = world().await;
    let children = || Subquery::count("children").correlate("parent_id", "id");

    for (comparison, n, expected) in [
        (Comparison::Eq, 2_i64, vec!["two"]),
        (Comparison::Eq, 0, vec!["none"]),
        (Comparison::Gt, 1, vec!["two"]),
        (Comparison::Lte, 1, vec!["none", "one"]),
    ] {
        assert_eq!(
            names(&db, Criteria::new().where_subquery(children(), comparison, n)).await,
            expected,
            "{comparison:?} {n}"
        );
    }
}

#[tokio::test]
async fn several_correlated_predicates_combine_in_one_where() {
    // The shape a chat lookup needs: the two members I am asking about are both
    // in the room, *and* the room holds nobody else. Either `EXISTS` alone
    // matches the three-member room as well, and the count is what rules it out
    // — so this fails unless all three predicates survive together and each
    // binds its own value.
    let db = world().await;

    let members = |id: i64| {
        Subquery::count("memberships").correlate("room_id", "id").where_eq("member_id", id)
    };

    let pair_of_100_and_200 =
        Criteria::new().where_exists(members(100)).where_exists(members(200)).where_subquery(
            Subquery::count("memberships").correlate("room_id", "id"),
            Comparison::Eq,
            2_i64,
        );

    let found: Vec<String> = EntityRepository::<Room>::new(db)
        .matching(pair_of_100_and_200)
        .await
        .expect("query")
        .into_iter()
        .map(|r| r.topic)
        .collect();

    assert_eq!(found, vec!["pair".to_string()], "the trio and the other pair must not match");
}

#[tokio::test]
async fn an_update_assigning_a_subquery_writes_a_count_to_every_row_including_zero() {
    // The one a naive implementation gets wrong. A per-row loop driven by a
    // `GROUP BY` sees no group for a parent with no children, so it leaves the
    // stale 999 in place; the correlated scalar writes `0` because a `COUNT`
    // over no rows *is* zero.
    let db = world().await;

    let prepared = statement::update_matching_with::<Parent>(
        db.dialect(),
        &Criteria::new(),
        vec![(
            "children_count".to_string(),
            Assignment::Subquery(Subquery::count("children").correlate("parent_id", "id")),
        )],
    );
    let affected = db.execute(prepared).await.expect("recount").rows_affected;

    assert_eq!(affected, 3, "every row, in one statement");
    assert_eq!(
        counts(&db).await,
        vec![("none".to_string(), 0), ("one".to_string(), 1), ("two".to_string(), 2)],
    );
}

#[tokio::test]
async fn an_assigned_subquerys_own_predicate_is_bound_and_applied() {
    // Same statement with a filter inside the subquery: parent 1 drops from two
    // to two approved (its third child is not), and the value has to arrive as a
    // bound parameter ahead of nothing else, so this also pins the SET-before-
    // WHERE ordering against a live driver rather than against rendered text.
    let db = world().await;

    let prepared = statement::update_matching_with::<Parent>(
        db.dialect(),
        &Criteria::new().where_ne("name", "none"),
        vec![(
            "children_count".to_string(),
            Assignment::Subquery(
                Subquery::count("children")
                    .correlate("parent_id", "id")
                    .where_eq("approved", false),
            ),
        )],
    );
    let affected = db.execute(prepared).await.expect("recount").rows_affected;

    assert_eq!(affected, 2, "the outer filter excluded one row");
    assert_eq!(
        counts(&db).await,
        vec![
            ("none".to_string(), 999), // untouched by the outer filter
            ("one".to_string(), 0),    // its only child is approved
            ("two".to_string(), 1),    // exactly the unapproved one
        ],
    );
}

#[tokio::test]
async fn a_hostile_value_inside_a_subquery_stays_a_value() {
    // End to end, through a driver that would happily execute a second
    // statement if one had been concatenated in.
    let db = world().await;

    let injected = Criteria::new().where_not_exists(
        Subquery::count("children")
            .correlate("parent_id", "id")
            .where_eq("approved", "') OR 1=1; DROP TABLE parents; --"),
    );

    // Every parent matches, because no child's `approved` equals that string —
    // which is the point: it was compared, not executed.
    assert_eq!(names(&db, injected).await.len(), 3);
    assert_eq!(counts(&db).await.len(), 3, "the table is still there");
}
