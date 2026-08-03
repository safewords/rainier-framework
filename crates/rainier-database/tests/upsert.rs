//! Insert-or-update against a real database.
//!
//! The rendering tests in `rainier-orm` and in the statement layer prove the
//! right SQL is built for each dialect. They cannot prove SQLite *accepts* it,
//! that `ON CONFLICT ("a", "b")` actually resolves to the composite primary key
//! the derive created, or — the one that matters — that an increment
//! **accumulates** rather than overwriting.
//!
//! That last distinction is invisible in every signal except the stored number.
//! A plain assignment renders, runs, reports a row affected, and leaves a
//! counter holding the last caller's value instead of the running total; the
//! only way to catch it is to write the same key twice and check the arithmetic.
//! So the tests here are about arithmetic, not about SQL text.
#![cfg(feature = "sea-orm-executor")]

use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{repo, Dialect, Executor, PoolConfig, Upsert};

/// A counter keyed on a pair — the shape an insert-or-increment exists for, and
/// the reason the conflict target has to accept more than one column.
#[derive(Debug, Clone, PartialEq, rainier_orm::Entity)]
#[orm(table = "tallies")]
struct Tally {
    #[orm(pk)]
    bucket: String,
    #[orm(pk)]
    slot: i64,
    total: i64,
}

/// A single-key row carrying both a value to overwrite and one to accumulate,
/// for the ignore and mixed-action cases.
#[derive(Debug, Clone, PartialEq, rainier_orm::Entity)]
#[orm(table = "labels")]
struct Label {
    #[orm(pk)]
    name: String,
    note: String,
    total: i64,
}

/// A row whose primary key the database assigns, carrying a natural key of its
/// own. The two keys are the whole point: `id` is the one a caller reaches for
/// as a conflict target and the one the `INSERT` never supplies, and `path` is
/// the one that actually arbitrates the collision.
#[derive(Debug, Clone, PartialEq, rainier_orm::Entity)]
#[orm(table = "hits")]
struct Hit {
    #[orm(pk, auto_increment)]
    id: i64,
    #[orm(unique)]
    path: String,
    views: i64,
}

async fn world() -> SeaOrmExecutor {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");

    // From the entities' own metadata, so this also proves the composite
    // `PRIMARY KEY (bucket, slot)` is something a conflict target can name.
    for sql in rainier_orm::schema::schema_ddl::<Tally>(Dialect::Sqlite)
        .into_iter()
        .chain(rainier_orm::schema::schema_ddl::<Label>(Dialect::Sqlite))
        .chain(rainier_orm::schema::schema_ddl::<Hit>(Dialect::Sqlite))
    {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }

    exec
}

async fn tally(exec: &SeaOrmExecutor, bucket: &str, slot: i64) -> Option<Tally> {
    repo::find_by_keys::<Tally, _>(exec, vec![bucket.into(), slot.into()]).await.expect("find")
}

#[tokio::test]
async fn an_increment_accumulates_rather_than_overwriting() {
    // *The* test. Two writes of the same key with 5 and 7: an increment stores
    // 12, an assignment stores 7, and both are a perfectly ordinary-looking
    // successful upsert.
    let exec = world().await;
    let plan = Upsert::on(["bucket", "slot"]).increment(["total"]);

    for amount in [5, 7] {
        repo::upsert_with(&exec, &Tally { bucket: "a".into(), slot: 1, total: amount }, &plan)
            .await
            .expect("upsert");
    }

    assert_eq!(
        tally(&exec, "a", 1).await.expect("the row exists").total,
        12,
        "5 + 7; seeing 7 means the increment became an assignment and the first write was lost"
    );
}

#[tokio::test]
async fn a_replace_would_have_stored_the_last_value_instead() {
    // The contrast that gives the test above its meaning: same statements, same
    // key, same values, one word different in the plan — and the stored number
    // is the one the counter case must never produce.
    let exec = world().await;
    let plan = Upsert::on(["bucket", "slot"]).replace(["total"]);

    for amount in [5, 7] {
        repo::upsert_with(&exec, &Tally { bucket: "a".into(), slot: 1, total: amount }, &plan)
            .await
            .expect("upsert");
    }

    assert_eq!(tally(&exec, "a", 1).await.expect("the row exists").total, 7);
}

#[tokio::test]
async fn an_increment_survives_many_writes_without_reading_the_row() {
    // Ten sequential writes of one each. The point is not the loop but that no
    // step ever loads `total` into the process — the read-then-write shape this
    // replaces would be correct here too, and wrong the moment two of these ran
    // at once.
    let exec = world().await;
    let plan = Upsert::on(["bucket", "slot"]).increment(["total"]);

    for _ in 0..10 {
        repo::upsert_with(&exec, &Tally { bucket: "b".into(), slot: 2, total: 1 }, &plan)
            .await
            .expect("upsert");
    }

    assert_eq!(tally(&exec, "b", 2).await.expect("the row exists").total, 10);
}

#[tokio::test]
async fn the_conflict_target_resolves_to_the_pair_not_either_half() {
    // Rows sharing each half of the key. If the target collapsed to one column,
    // these would collide with each other and the totals would merge.
    let exec = world().await;
    let plan = Upsert::on(["bucket", "slot"]).increment(["total"]);

    for (bucket, slot) in [("a", 1), ("a", 2), ("c", 1)] {
        for _ in 0..3 {
            repo::upsert_with(&exec, &Tally { bucket: bucket.into(), slot, total: 1 }, &plan)
                .await
                .expect("upsert");
        }
    }

    for (bucket, slot) in [("a", 1), ("a", 2), ("c", 1)] {
        assert_eq!(
            tally(&exec, bucket, slot).await.expect("the row exists").total,
            3,
            "({bucket}, {slot}) must count only its own writes"
        );
    }
    assert_eq!(repo::all::<Tally, _>(&exec).await.expect("all").len(), 3, "three distinct pairs");
}

#[tokio::test]
async fn an_increment_inserts_the_row_when_there_is_nothing_to_add_to() {
    // The "insert" half. A statement that only ever updated would silently
    // count nothing at all until some other code path created the row.
    let exec = world().await;

    repo::upsert_with(
        &exec,
        &Tally { bucket: "new".into(), slot: 9, total: 4 },
        &Upsert::on(["bucket", "slot"]).increment(["total"]),
    )
    .await
    .expect("upsert");

    assert_eq!(tally(&exec, "new", 9).await.expect("the row was created").total, 4);
}

#[tokio::test]
async fn a_plan_with_no_actions_keeps_the_stored_row() {
    let exec = world().await;
    let ignore = Upsert::on(["name"]);

    repo::upsert_with(&exec, &Label { name: "x".into(), note: "first".into(), total: 0 }, &ignore)
        .await
        .expect("insert");
    repo::upsert_with(&exec, &Label { name: "x".into(), note: "second".into(), total: 0 }, &ignore)
        .await
        .expect("ignored");

    let rows = repo::all::<Label, _>(&exec).await.expect("all");
    assert_eq!(rows.len(), 1, "insert-or-ignore must not duplicate the key");
    assert_eq!(rows[0].note, "first", "the stored row wins");
}

#[tokio::test]
async fn a_plan_with_no_conflict_columns_is_refused_without_touching_the_table() {
    // SQLite rejects `ON CONFLICT DO UPDATE` with no target, and MySQL would
    // have accepted it — so the check has to happen before the statement is
    // sent, not be left to whichever database is behind this.
    let exec = world().await;
    repo::upsert_with(
        &exec,
        &Label { name: "x".into(), note: "first".into(), total: 0 },
        &Upsert::on(["name"]).replace(["note"]),
    )
    .await
    .expect("seed");

    let refused = repo::upsert_with(
        &exec,
        &Label { name: "x".into(), note: "second".into(), total: 0 },
        &Upsert::on(Vec::<String>::new()).replace(["note"]),
    )
    .await;

    assert!(refused.is_err(), "an inferred conflict target is a MySQL-only statement");
    let rows = repo::all::<Label, _>(&exec).await.expect("all");
    assert_eq!(rows[0].note, "first", "the table is untouched");
}

#[tokio::test]
async fn conflicting_on_an_auto_increment_key_is_refused_rather_than_appending_rows() {
    // The failure that reports success. `id` is auto-increment, so the insert
    // omits it and there is nothing for the stored row to collide with: the
    // statement takes the insert branch every time, returns one row affected,
    // and leaves a table full of duplicates under a key meant to be unique.
    //
    // Two calls, so the assertion is on the row count rather than on the error
    // alone — one row means the second call really was stopped, not merely
    // reported.
    let exec = world().await;
    let plan = Upsert::on(["id"]).increment(["views"]);

    repo::insert(&exec, &Hit { id: 0, path: "/a".into(), views: 1 }).await.expect("seed");

    for _ in 0..2 {
        let refused = repo::upsert_with(&exec, &Hit { id: 0, path: "/a".into(), views: 1 }, &plan)
            .await
            .err()
            .expect("a plan that can only ever insert must not reach the database");

        assert!(refused.to_string().contains("id"), "{refused}");
    }

    let rows = repo::all::<Hit, _>(&exec).await.expect("all");
    assert_eq!(rows.len(), 1, "the table is untouched; each call would otherwise append a row");
    assert_eq!(rows[0].views, 1, "and nothing was counted");
}

#[tokio::test]
async fn the_natural_key_is_what_the_same_counter_conflicts_on() {
    // The working form of the test above, against the same table: conflict on
    // the column the insert actually supplies and that carries a `UNIQUE`, and
    // the counter accumulates on one row exactly as intended. Without this the
    // refusal above could be satisfied by a check that rejects everything.
    let exec = world().await;
    let plan = Upsert::on(["path"]).increment(["views"]);

    for _ in 0..3 {
        repo::upsert_with(&exec, &Hit { id: 0, path: "/a".into(), views: 1 }, &plan)
            .await
            .expect("upsert");
    }

    let rows = repo::all::<Hit, _>(&exec).await.expect("all");
    assert_eq!(rows.len(), 1, "one row per path");
    assert_eq!(rows[0].views, 3);
}

#[tokio::test]
async fn replacing_and_incrementing_in_one_plan_do_each_their_own_thing() {
    // Both actions in one statement, because a counter row usually carries
    // something to overwrite too — and a plan that applied one rule to every
    // column could not express that without a second write.
    let exec = world().await;
    let plan = Upsert::on(["name"]).replace(["note"]).increment(["total"]);

    for (note, amount) in [("first", 5), ("second", 7)] {
        repo::upsert_with(
            &exec,
            &Label { name: "mixed".into(), note: note.into(), total: amount },
            &plan,
        )
        .await
        .expect("upsert");
    }

    let rows = repo::all::<Label, _>(&exec).await.expect("all");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].note, "second", "replaced with the incoming value");
    assert_eq!(rows[0].total, 12, "accumulated, in the same statement");
}
