//! `INSERT … SELECT`, upsert-from-select, and the [`SubSelect`] shapes that
//! feed them — derived tables, `UNION ALL`, self-joins with `HAVING`, and
//! top-K-per-group rankings — against a real database.
//!
//! These are the statements a recommendation pipeline is made of: the whole
//! computation is one `SELECT`, and its result is written without leaving the
//! database. Each used to be a MySQL-only raw string that the SQLite suite
//! could not run at all.
#![cfg(feature = "sea-orm-executor")]

use rainier_database::expression::*;
use rainier_database::{statement, ColumnRequest, Database};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, ColumnType, Dialect, Entity, Executor, PoolConfig, Row as _};

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

/// Keyed by the pair, like every affinity table.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "pair_scores")]
struct PairScore {
    #[orm(pk)]
    a_id: u64,
    #[orm(pk)]
    b_id: u64,
    score: f64,
    reason: Option<String>,
}

async fn world() -> Database {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    for sql in rainier_orm::schema::schema_ddl::<Follow>(Dialect::Sqlite)
        .into_iter()
        .chain(rainier_orm::schema::schema_ddl::<Block>(Dialect::Sqlite))
        .chain(rainier_orm::schema::schema_ddl::<PairScore>(Dialect::Sqlite))
    {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }

    // 1 and 2 are both followed by 3 and 4; 2 alone is followed by 5.
    for (id, follower_id, followed_id) in [(1, 3, 1), (2, 3, 2), (3, 4, 1), (4, 4, 2), (5, 5, 2)] {
        repo::insert(&exec, &Follow { id, follower_id, followed_id }).await.expect("follow");
    }
    // 1 blocked 3.
    repo::insert(&exec, &Block { id: 1, blocker_id: 1, blocked_id: 3 }).await.expect("block");

    Database::new(exec)
}

async fn scores(db: &Database) -> Vec<(u64, u64, f64, Option<String>)> {
    let mut rows: Vec<_> = db
        .fetch_all::<PairScore>(statement::select_all::<PairScore>(db.dialect()))
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.a_id, p.b_id, p.score, p.reason))
        .collect();
    rows.sort_by_key(|x| (x.0, x.1));
    rows
}

const COLUMNS: [&str; 4] = ["a_id", "b_id", "score", "reason"];

#[tokio::test]
async fn a_self_join_grouped_and_filtered_by_having_is_written_in_one_statement() {
    // Pairs of profiles followed by at least two of the same people.
    let db = world().await;
    let pairs = SubSelect::from("follows")
        .alias("fa")
        .join(
            "follows",
            "fb",
            all([
                col("fb.follower_id").eq(col("fa.follower_id")),
                col("fb.followed_id").ne(col("fa.followed_id")),
            ]),
        )
        .select(col("fa.followed_id"))
        .select(col("fb.followed_id"))
        .select(cast(count_all(), CastAs::Real))
        .select(concat([val("shared by "), count_all()]))
        .group_by(col("fa.followed_id"))
        .group_by(col("fb.followed_id"))
        .having(count_all().gte(2_i64));

    let written = db
        .execute(statement::insert_select::<PairScore>(db.dialect(), &COLUMNS, &pairs).unwrap())
        .await
        .unwrap();
    assert_eq!(written.rows_affected, 2);
    assert_eq!(
        scores(&db).await,
        vec![(1, 2, 2.0, Some("shared by 2".into())), (2, 1, 2.0, Some("shared by 2".into()))]
    );
}

#[tokio::test]
async fn a_union_all_of_two_sources_is_summed_through_a_derived_table() {
    // Each follow is worth 1, each block 5, and a pair that is both sums.
    let db = world().await;
    let follows = SubSelect::from("follows")
        .select_as(col("followed_id"), "a")
        .select_as(col("follower_id"), "b")
        .select_as(val(1.0_f64), "w");
    let blocks = SubSelect::from("blocks")
        .select(col("blocker_id"))
        .select(col("blocked_id"))
        .select(val(5.0_f64));
    let summed = SubSelect::from_select(follows.union_all(blocks))
        .alias("unioned")
        .select(col("a"))
        .select(col("b"))
        .select(sum(col("w")))
        .select(val(Option::<String>::None))
        .group_by(col("a"))
        .group_by(col("b"));

    db.execute(statement::insert_select::<PairScore>(db.dialect(), &COLUMNS, &summed).unwrap())
        .await
        .unwrap();
    assert_eq!(
        scores(&db).await,
        vec![
            (1, 3, 6.0, None), // followed by 3, and blocked 3
            (1, 4, 1.0, None),
            (2, 3, 1.0, None),
            (2, 4, 1.0, None),
            (2, 5, 1.0, None),
        ]
    );
}

#[tokio::test]
async fn top_k_per_group_is_a_window_in_a_derived_table() {
    // Each profile's single highest-numbered follower.
    let db = world().await;
    let ranked = SubSelect::from("follows")
        .select_as(col("followed_id"), "a")
        .select_as(col("follower_id"), "b")
        .select_as(
            row_number().partition_by([col("followed_id")]).order_by_desc(col("follower_id")),
            "rk",
        );
    let top = SubSelect::from_select(ranked)
        .alias("ranked")
        .select(col("a"))
        .select(col("b"))
        .select(cast(col("rk"), CastAs::Real))
        .select(val(Option::<String>::None))
        .filter(col("rk").lte(1_i64));

    db.execute(statement::insert_select::<PairScore>(db.dialect(), &COLUMNS, &top).unwrap())
        .await
        .unwrap();
    assert_eq!(scores(&db).await, vec![(1, 4, 1.0, None), (2, 5, 1.0, None)]);
}

/// The two conflict assignments a pipeline that runs several channels into
/// one table needs: add the new score to the stored one, and keep whichever
/// reason came with the larger contribution. The reason goes first — see
/// [`incoming`] on MySQL's left-to-right evaluation.
fn accumulate() -> Vec<(&'static str, Expression)> {
    vec![
        (
            "reason",
            case(incoming("score").gt(col("pair_scores.score")), incoming("reason"))
                .otherwise(col("pair_scores.reason")),
        ),
        ("score", col("pair_scores.score").plus(incoming("score"))),
    ]
}

#[tokio::test]
async fn an_upsert_from_a_select_accumulates_and_keeps_the_stronger_reason() {
    let db = world().await;
    let seed = |a_id, b_id, score: f64, reason: &str| PairScore {
        a_id,
        b_id,
        score,
        reason: Some(reason.into()),
    };
    db.execute(statement::insert(db.dialect(), &seed(1, 3, 0.5, "old, weak"), None)).await.unwrap();
    db.execute(statement::insert(db.dialect(), &seed(1, 4, 9.0, "old, strong"), None))
        .await
        .unwrap();

    // Every follow, at 2.0: (1,3) and (1,4) collide, the rest are new. A join
    // with no `WHERE` of its own, which SQLite would otherwise misparse.
    let channel = SubSelect::from("follows")
        .alias("f")
        .join("blocks", "b", col("b.blocker_id").ne(col("f.followed_id")).or(col("b.id").eq(1_i64)))
        .select(col("f.followed_id"))
        .select(col("f.follower_id"))
        .select(val(2.0_f64))
        .select(val("channel"));

    let prepared = statement::upsert_select::<PairScore>(
        db.dialect(),
        &COLUMNS,
        &channel,
        &["a_id", "b_id"],
        &accumulate(),
    )
    .unwrap();
    db.execute(prepared).await.unwrap();

    assert_eq!(
        scores(&db).await,
        vec![
            (1, 3, 2.5, Some("channel".into())),      // 2.0 beat 0.5
            (1, 4, 11.0, Some("old, strong".into())), // 2.0 did not beat 9.0
            (2, 3, 2.0, Some("channel".into())),
            (2, 4, 2.0, Some("channel".into())),
            (2, 5, 2.0, Some("channel".into())),
        ]
    );
}

#[tokio::test]
async fn a_prune_to_top_k_deletes_through_a_ranked_derived_table() {
    // Keep each `a_id`'s best row; delete the rest. MySQL refuses a `DELETE`
    // whose sub-select reads the target table directly, and a derived table
    // holding a window function is always materialised, which is what makes
    // this shape portable.
    let db = world().await;
    for (a_id, b_id, score) in [(1, 3, 1.0), (1, 4, 3.0), (2, 3, 2.0), (2, 5, 1.0)] {
        let row = PairScore { a_id, b_id, score, reason: None };
        db.execute(statement::insert(db.dialect(), &row, None)).await.unwrap();
    }
    let ranked = SubSelect::from("pair_scores")
        .select_as(col("a_id"), "a_id")
        .select_as(col("b_id"), "b_id")
        .select_as(row_number().partition_by([col("a_id")]).order_by_desc(col("score")), "rk");
    let beyond_top = SubSelect::from_select(ranked).alias("ranked").filter(all([
        col("ranked.a_id").eq(col("pair_scores.a_id")),
        col("ranked.b_id").eq(col("pair_scores.b_id")),
        col("ranked.rk").gt(1_i64),
    ]));

    let deleted = db
        .execute(statement::delete_matching::<PairScore>(
            db.dialect(),
            &rainier_database::Criteria::new()
                .where_in("a_id", [1_u64, 2])
                .where_expr(exists(beyond_top)),
        ))
        .await
        .unwrap();
    assert_eq!(deleted.rows_affected, 2);
    assert_eq!(scores(&db).await, vec![(1, 4, 3.0, None), (2, 3, 2.0, None)]);
}

#[tokio::test]
async fn a_sub_select_reads_as_rows_of_its_own() {
    let db = world().await;
    let counts = SubSelect::from("follows")
        .select_as(col("followed_id"), "profile")
        .select_as(count_all(), "followers")
        .group_by(col("followed_id"))
        .order_by_desc(count_all())
        .order_by(col("followed_id"));
    let rows = db
        .fetch(
            statement::select(db.dialect(), &counts),
            vec![
                ColumnRequest::new("profile", ColumnType::BigInt),
                ColumnRequest::new("followers", ColumnType::BigInt),
            ],
        )
        .await
        .unwrap();
    let got: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| (r.get_i64("profile").unwrap().unwrap(), r.get_i64("followers").unwrap().unwrap()))
        .collect();
    assert_eq!(got, vec![(2, 3), (1, 2)]);
}

#[test]
fn the_incoming_row_is_spelled_per_dialect() {
    let source = SubSelect::from("follows")
        .select(col("followed_id"))
        .select(col("follower_id"))
        .select(val(1.0_f64))
        .select(val("x"))
        .filter(col("id").gt(0_i64));
    let render = |dialect| {
        statement::upsert_select::<PairScore>(
            dialect,
            &COLUMNS,
            &source,
            &["a_id", "b_id"],
            &accumulate(),
        )
        .unwrap()
        .sql
    };

    let mysql = render(Dialect::MySql);
    assert!(
        mysql.starts_with("INSERT INTO `pair_scores` (`a_id`, `b_id`, `score`, `reason`) SELECT"),
        "{mysql}"
    );
    assert!(
        mysql.ends_with(
            "ON DUPLICATE KEY UPDATE `reason` = (CASE WHEN (VALUES(`score`) > `pair_scores`.`score`) \
             THEN VALUES(`reason`) ELSE `pair_scores`.`reason` END), \
             `score` = `pair_scores`.`score` + VALUES(`score`)"
        ),
        "{mysql}"
    );

    let postgres = render(Dialect::Postgres);
    assert!(
        postgres.ends_with(
            r#"ON CONFLICT ("a_id", "b_id") DO UPDATE SET "reason" = (CASE WHEN ("excluded"."score" > "pair_scores"."score") THEN "excluded"."reason" ELSE "pair_scores"."reason" END), "score" = "pair_scores"."score" + "excluded"."score""#
        ),
        "{postgres}"
    );
}

#[test]
fn a_mismatched_or_unknown_column_is_refused_before_anything_renders() {
    let two = SubSelect::from("follows").select(col("followed_id")).select(col("follower_id"));
    let err = statement::insert_select::<PairScore>(Dialect::Sqlite, &COLUMNS, &two).unwrap_err();
    assert!(err.to_string().contains("names 4 columns but its SELECT produces 2"), "{err}");

    let err = statement::insert_select::<PairScore>(Dialect::Sqlite, &["a_id", "nope"], &two)
        .unwrap_err();
    assert!(err.to_string().contains("no column `nope`"), "{err}");

    let err =
        statement::upsert_select::<PairScore>(Dialect::Sqlite, &["a_id", "b_id"], &two, &[], &[])
            .unwrap_err();
    assert!(err.to_string().contains("no conflict columns"), "{err}");
}

/// An append-only series with a database-assigned key.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "view_points")]
struct ViewPoint {
    #[orm(pk, auto_increment)]
    id: u64,
    post_id: u64,
    views: u64,
}

#[tokio::test]
async fn insert_many_writes_every_row_in_one_statement_and_leaves_the_key_to_the_database() {
    let db = world().await;
    for sql in rainier_orm::schema::schema_ddl::<ViewPoint>(Dialect::Sqlite) {
        db.statement(&sql).await.expect("create table");
    }
    let rows: Vec<ViewPoint> = [(7, 10), (8, 0), (9, 42)]
        .map(|(post_id, views)| ViewPoint { id: 0, post_id, views })
        .into();

    let prepared = statement::insert_many(db.dialect(), &rows).unwrap();
    assert_eq!(prepared.sql.matches("(?, ?)").count(), 3, "{}", prepared.sql);
    assert_eq!(db.execute(prepared).await.unwrap().rows_affected, 3);

    let mut got: Vec<(u64, u64, u64)> = db
        .fetch_all::<ViewPoint>(statement::select_all::<ViewPoint>(db.dialect()))
        .await
        .unwrap()
        .into_iter()
        .map(|p| (p.id, p.post_id, p.views))
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![(1, 7, 10), (2, 8, 0), (3, 9, 42)]);

    let err = statement::insert_many::<ViewPoint>(db.dialect(), &[]).unwrap_err();
    assert!(err.to_string().contains("given no rows"), "{err}");
}
