//! General expressions and predicates against a real database.
//!
//! The rendering tests in `expression.rs` prove the SQL each dialect gets.
//! These prove it *means* the right thing: each fixture holds rows that a
//! wrong rendering would include or exclude, so a dropped parenthesis, an
//! uncorrelated sub-select or an unescaped wildcard shows up as a wrong set of
//! ids rather than as nothing at all.
//!
//! Every test is a shape an application used to write as a raw string.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::expression::*;
use rainier_database::{
    statement, Assignment, Criteria, Database, EntityRepository, Model, Repository,
};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, Dialect, Entity, Executor, PoolConfig, Row as _};

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "profiles")]
struct Profile {
    #[orm(pk)]
    id: u64,
    username: String,
    location: Option<String>,
    followers_count: i64,
    /// Three timestamps of which any may be missing — the `COALESCE` shape.
    last_event_at: Option<i64>,
    started_at: Option<i64>,
    created_at: Option<i64>,
    decommissioned_at: Option<i64>,
    last_heartbeat_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "follows")]
struct Follow {
    #[orm(pk)]
    id: u64,
    follower_id: u64,
    followed_id: u64,
}

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "blocks")]
struct Block {
    #[orm(pk)]
    id: u64,
    blocker_id: u64,
    blocked_id: u64,
}

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "renditions")]
struct Rendition {
    #[orm(pk)]
    id: u64,
    ref_count: u64,
}

impl Model for Profile {}
impl Model for Follow {}
impl Model for Rendition {}

fn profile(id: u64, username: &str, location: Option<&str>, followers: i64) -> Profile {
    Profile {
        id,
        username: username.into(),
        location: location.map(Into::into),
        followers_count: followers,
        last_event_at: None,
        started_at: None,
        created_at: None,
        decommissioned_at: None,
        last_heartbeat_at: None,
    }
}

async fn world() -> Database {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");

    for sql in rainier_orm::schema::schema_ddl::<Profile>(Dialect::Sqlite)
        .into_iter()
        .chain(rainier_orm::schema::schema_ddl::<Follow>(Dialect::Sqlite))
        .chain(rainier_orm::schema::schema_ddl::<Block>(Dialect::Sqlite))
        .chain(rainier_orm::schema::schema_ddl::<Rendition>(Dialect::Sqlite))
    {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }

    let mut profiles = vec![
        profile(1, "amy", Some("  Berlin "), 50),
        profile(2, "amy_b", Some("berlin"), 70),
        profile(3, "amyxb", Some("Paris"), 10),
        profile(4, "bob", None, 5),
        profile(5, "cleo", Some(""), 1),
    ];
    // The COALESCE shape: 1 has only created_at, 2 only started_at, 3 has a
    // recent last_event_at that must win over an old created_at.
    profiles[0].created_at = Some(100);
    profiles[1].started_at = Some(200);
    profiles[2].last_event_at = Some(900);
    profiles[2].created_at = Some(50);
    // The worker-reaping shape: 3 decommissioned long ago, 4 alive but silent,
    // 5 decommissioned recently.
    profiles[2].decommissioned_at = Some(10);
    profiles[3].last_heartbeat_at = Some(20);
    profiles[4].decommissioned_at = Some(1000);
    for p in &profiles {
        repo::insert(&exec, p).await.expect("insert profile");
    }

    // 1 and 2 are both followed by 3 and 4; 2 alone is followed by 5.
    for (id, follower_id, followed_id) in [(1, 3, 1), (2, 3, 2), (3, 4, 1), (4, 4, 2), (5, 5, 2)] {
        repo::insert(&exec, &Follow { id, follower_id, followed_id }).await.expect("follow");
    }
    // 1 blocked 3.
    repo::insert(&exec, &Block { id: 1, blocker_id: 1, blocked_id: 3 }).await.expect("block");

    for (id, ref_count) in [(1, 3_u64), (2, 1), (3, 0)] {
        repo::insert(&exec, &Rendition { id, ref_count }).await.expect("rendition");
    }

    Database::new(exec)
}

fn ids(profiles: &[Profile]) -> Vec<u64> {
    let mut ids: Vec<u64> = profiles.iter().map(|p| p.id).collect();
    ids.sort_unstable();
    ids
}

fn profiles(db: &Database) -> EntityRepository<Profile> {
    EntityRepository::new(db.clone())
}

#[tokio::test]
async fn an_or_of_ands_keeps_its_grouping() {
    // Reap: decommissioned before 100, or alive and silent since before 100.
    let db = world().await;
    let stale = any([
        all([col("decommissioned_at").is_not_null(), col("decommissioned_at").lt(100_i64)]),
        all([col("decommissioned_at").is_null(), col("last_heartbeat_at").lt(100_i64)]),
    ]);
    let found = profiles(&db).matching(Criteria::new().where_expr(stale)).await.unwrap();
    // 3 (old decommission) and 4 (silent). Not 5, decommissioned recently —
    // which an ungrouped `a AND b OR c AND d` would still get right, but an
    // `a AND (b OR c) AND d` would not.
    assert_eq!(ids(&found), vec![3, 4]);
}

#[tokio::test]
async fn a_function_on_the_left_of_a_comparison() {
    let db = world().await;
    let found = profiles(&db)
        .matching(Criteria::new().where_expr(lower(trim(col("location"))).eq("berlin")))
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![1, 2], "'  Berlin ' and 'berlin', not 'Paris'");
}

#[tokio::test]
async fn coalesce_picks_the_first_timestamp_that_exists() {
    let db = world().await;
    let started = coalesce([col("last_event_at"), col("started_at"), col("created_at")]);
    let found =
        profiles(&db).matching(Criteria::new().where_expr(started.lte(300_i64))).await.unwrap();
    // 1 (created 100) and 2 (started 200). 3's last event is 900, which must
    // win over its created_at of 50.
    assert_eq!(ids(&found), vec![1, 2]);
}

#[tokio::test]
async fn a_search_term_matches_itself_and_not_its_wildcards() {
    let db = world().await;
    let found = profiles(&db)
        .matching(Criteria::new().where_expr(col("username").contains("y_b")))
        .await
        .unwrap();
    // `amy_b` contains `y_b`. `amyxb` would match an unescaped `%y_b%`, where
    // `_` is any character.
    assert_eq!(ids(&found), vec![2]);
}

#[tokio::test]
async fn a_case_puts_the_exact_match_first() {
    let db = world().await;
    let rank = case(col("username").eq("amy"), 0_i64)
        .when(col("username").starts_with("amy"), 1_i64)
        .otherwise(2_i64);
    let found = profiles(&db)
        .matching(
            Criteria::new()
                .where_expr(col("username").contains("amy").or(col("username").eq("bob")))
                .order_by_expr(rank, false)
                .order_by_expr(col("followers_count"), true),
        )
        .await
        .unwrap();
    let order: Vec<u64> = found.iter().map(|p| p.id).collect();
    // Exact "amy" first; then the two starting with "amy", most followed first;
    // then bob.
    assert_eq!(order, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn a_self_join_with_having_finds_co_followed_pairs() {
    // Pairs of profiles followed by at least two of the same people: the
    // co-follow shape, which is `follows` joined to itself.
    let db = world().await;
    let rows = EntityRepository::<Follow>::new(db.clone())
        .aggregate(
            Criteria::new()
                .join_as(
                    "follows",
                    "other",
                    all([
                        col("other.follower_id").eq(col("follower_id")),
                        col("other.followed_id").ne(col("followed_id")),
                    ]),
                )
                .select_expr(col("followed_id"), "a")
                .select_expr(col("other.followed_id"), "b")
                .select_expr(count_all(), "shared")
                .group_by_expr(col("followed_id"))
                .group_by_expr(col("other.followed_id"))
                .having(count_all().gte(2_i64)),
        )
        .await
        .unwrap();

    let mut pairs: Vec<(i64, i64, i64)> = rows
        .iter()
        .map(|r| {
            (
                r.get_i64("a").unwrap().unwrap(),
                r.get_i64("b").unwrap().unwrap(),
                r.get_i64("shared").unwrap().unwrap(),
            )
        })
        .collect();
    pairs.sort_unstable();
    // 1 and 2 share followers 3 and 4. 5 follows only 2, so no pair has three.
    assert_eq!(pairs, vec![(1, 2, 2), (2, 1, 2)]);
}

#[tokio::test]
async fn a_left_join_anti_join_excludes_only_this_viewers_blocks() {
    // Profiles viewer 1 has not blocked: LEFT JOIN blocks with the viewer in
    // the ON, and IS NULL on the joined key.
    let db = world().await;
    let viewer = 1_i64;
    let found = profiles(&db)
        .matching(
            Criteria::new()
                .left_join_as(
                    "blocks",
                    "mine",
                    all([col("mine.blocked_id").eq(col("id")), col("mine.blocker_id").eq(viewer)]),
                )
                .where_expr(col("mine.id").is_null())
                .where_expr(col("id").ne(viewer)),
        )
        .await
        .unwrap();
    // Everyone but 1 itself and 3, whom 1 blocked.
    assert_eq!(ids(&found), vec![2, 4, 5]);
}

#[tokio::test]
async fn a_correlated_not_exists() {
    // Profiles nobody follows.
    let db = world().await;
    let found = profiles(&db)
        .matching(Criteria::new().where_expr(not_exists(
            SubSelect::from("follows").filter(col("followed_id").eq(col("profiles.id"))),
        )))
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![3, 4, 5]);
}

#[tokio::test]
async fn in_an_uncorrelated_sub_select() {
    // Profiles that follow somebody.
    let db = world().await;
    let found =
        profiles(&db)
            .matching(Criteria::new().where_expr(
                col("id").in_select(SubSelect::from("follows").select(col("follower_id"))),
            ))
            .await
            .unwrap();
    assert_eq!(ids(&found), vec![3, 4, 5]);
}

#[tokio::test]
async fn between_is_inclusive() {
    let db = world().await;
    let found = profiles(&db)
        .matching(Criteria::new().where_expr(col("followers_count").between(5_i64, 50_i64)))
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![1, 3, 4]);
}

#[tokio::test]
async fn an_expression_assignment_releases_a_reference_without_going_negative() {
    // `ref_count = GREATEST(CAST(ref_count AS SIGNED) - ?, 0)`: releasing two
    // from a count of one must leave zero, not wrap an unsigned column.
    let db = world().await;
    let release = greatest([cast(col("ref_count"), CastAs::Integer).minus(2_i64), val(0_i64)]);
    let prepared = statement::update_matching_with::<Rendition>(
        Dialect::Sqlite,
        &Criteria::new(),
        vec![("ref_count".into(), Assignment::Expression(release))],
    );
    db.execute(prepared).await.unwrap();

    let after = EntityRepository::<Rendition>::new(db.clone()).all().await.unwrap();
    let mut counts: Vec<(u64, u64)> = after.iter().map(|r| (r.id, r.ref_count)).collect();
    counts.sort_unstable();
    assert_eq!(counts, vec![(1, 1), (2, 0), (3, 0)]);
}

#[tokio::test]
async fn a_window_ranks_within_each_partition() {
    let db = world().await;
    let rows = EntityRepository::<Follow>::new(db.clone())
        .aggregate(
            Criteria::new()
                .select_expr(col("followed_id"), "followed")
                .select_expr(col("follower_id"), "follower")
                .select_expr(
                    row_number()
                        .partition_by([col("followed_id")])
                        .order_by_desc(col("follower_id"))
                        .into(),
                    "rk",
                ),
        )
        .await
        .unwrap();
    let mut ranked: Vec<(i64, i64, i64)> = rows
        .iter()
        .map(|r| {
            (
                r.get_i64("followed").unwrap().unwrap(),
                r.get_i64("follower").unwrap().unwrap(),
                r.get_i64("rk").unwrap().unwrap(),
            )
        })
        .collect();
    ranked.sort_unstable();
    // Within followed 1: follower 4 ranks 1, 3 ranks 2. Within 2: 5, 4, 3.
    assert_eq!(ranked, vec![(1, 3, 2), (1, 4, 1), (2, 3, 3), (2, 4, 2), (2, 5, 1)]);
}

#[tokio::test]
async fn the_count_uses_the_same_predicates_as_the_select() {
    let db = world().await;
    let criteria = Criteria::new().where_expr(lower(trim(col("location"))).eq("berlin"));
    assert_eq!(profiles(&db).count_matching(criteria).await.unwrap(), 2);
}

#[tokio::test]
async fn an_expression_delete_removes_only_what_it_names() {
    let db = world().await;
    let deleted = profiles(&db)
        .delete_matching(
            Criteria::new()
                .where_expr(any([col("location").is_null(), trim(col("location")).eq("")])),
        )
        .await
        .unwrap();
    assert_eq!(deleted, 2, "bob (no location) and cleo (blank)");
    assert_eq!(ids(&profiles(&db).all().await.unwrap()), vec![1, 2, 3]);
}

#[tokio::test]
async fn a_merged_scope_keeps_its_joins_and_predicates() {
    // `merge` used to drop left joins, so a scope carrying an anti-join merged
    // into nothing and the query returned rows it was meant to exclude.
    let db = world().await;
    let not_blocked_by_1 = Criteria::new()
        .left_join_as(
            "blocks",
            "mine",
            all([col("mine.blocked_id").eq(col("id")), col("mine.blocker_id").eq(1_i64)]),
        )
        .where_expr(col("mine.id").is_null());
    let found = profiles(&db)
        .matching(Criteria::new().where_expr(col("id").gt(1_i64)).merge(not_blocked_by_1))
        .await
        .unwrap();
    assert_eq!(ids(&found), vec![2, 4, 5]);
}
