//! Partial updates against a real database — what `update_matching` protects
//! that `update` cannot.
//!
//! The unit tests in `repository.rs` run against a fake connection and prove the
//! right SQL is built: only the named column in the `SET`, the criteria in the
//! `WHERE`, no statement at all for an empty column list. They cannot prove the
//! thing the method exists for, because it is not visible in SQL text.
//!
//! `update(&model)` writes every non-key column from a struct the caller read at
//! some earlier moment. If anything changed a *different* column in between, that
//! change is written back to its old value. Nothing errors. One row is reported
//! affected, which is also what a correct write reports. The only evidence is the
//! stored data, later.
//!
//! So these tests interleave two writers over one row and read the column
//! afterwards — the same argument `upsert.rs` makes for increments, and for the
//! same reason: the distinction is invisible in every signal except the stored
//! value.
#![cfg(feature = "sea-orm-executor")]

use chrono::{DateTime, TimeZone, Utc};
use rainier_database::{Criteria, Database, EntityRepository, Migrator, Model, Repository};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Entity, PoolConfig};

/// A notification row with two independent timestamps: one a reader stamps when
/// the person looks at it, one a mailer stamps when the digest goes out. The
/// pair is the whole scenario — two writers, two columns, one row.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "notices")]
struct Notice {
    #[orm(pk, auto_increment)]
    id: u64,
    profile_id: u64,
    seen_at: Option<DateTime<Utc>>,
    emailed_at: Option<DateTime<Utc>>,
}
impl Model for Notice {}

/// A soft-deleting row, for the scoping half.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "documents")]
struct Document {
    #[orm(pk, auto_increment)]
    id: u64,
    title: String,
    #[orm(soft_delete)]
    deleted_at: Option<DateTime<Utc>>,
}
impl Model for Document {}

fn at(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 3, hour, 0, 0).unwrap()
}

struct World {
    notices: EntityRepository<Notice>,
    documents: EntityRepository<Document>,
}

/// Three unread, un-emailed notices for profile 1 and one for profile 2.
async fn world() -> World {
    let executor = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    let db = Database::new(executor);

    Migrator::new()
        .create_table::<Notice>("0001_notices")
        .create_table::<Document>("0002_documents")
        .run(&db)
        .await
        .expect("migrate");

    let notices = EntityRepository::<Notice>::new(db.clone());
    let documents = EntityRepository::<Document>::new(db);

    for profile_id in [1, 1, 1, 2] {
        notices
            .create(Notice { id: 0, profile_id, seen_at: None, emailed_at: None })
            .await
            .expect("notice");
    }

    World { notices, documents }
}

/// The failure this method exists to remove, demonstrated rather than described.
///
/// A mailer reads the row, a reader marks it seen, the mailer stamps its own
/// column through `update` — and the reader's write is gone. Every signal along
/// the way says the write succeeded.
#[tokio::test]
async fn update_clobbers_a_column_another_writer_changed() {
    let world = world().await;

    // The mailer's copy, read before the reader touches anything.
    let mut stale = world.notices.find_or_fail(1_u64).await.expect("read");
    assert_eq!(stale.seen_at, None);

    // Meanwhile: the person opens the notification.
    world
        .notices
        .update_column(Criteria::new().where_eq("id", 1_u64), "seen_at", at(12))
        .await
        .expect("reader stamps seen_at");

    // The mailer finishes and saves its copy.
    stale.emailed_at = Some(at(13));
    let affected = world.notices.update(&stale).await.expect("mailer saves");

    assert_eq!(affected, 1, "the write reports success, which is the problem");

    let stored = world.notices.find_or_fail(1_u64).await.expect("read back");
    assert_eq!(stored.emailed_at, Some(at(13)), "the column it meant to write is written");
    assert_eq!(
        stored.seen_at, None,
        "and the column it did not mean to write is back to what the stale copy held — \
         the reader's stamp is silently gone"
    );
}

/// The same interleaving through the partial write. This is the assertion the
/// test above exists to contrast with.
#[tokio::test]
async fn update_matching_leaves_every_column_it_was_not_given_alone() {
    let world = world().await;

    let stale = world.notices.find_or_fail(1_u64).await.expect("read");
    assert_eq!(stale.seen_at, None);

    world
        .notices
        .update_column(Criteria::new().where_eq("id", 1_u64), "seen_at", at(12))
        .await
        .expect("reader stamps seen_at");

    // The mailer names its column instead of saving its copy of the row.
    let affected = world
        .notices
        .update_column(Criteria::new().where_eq("id", 1_u64), "emailed_at", at(13))
        .await
        .expect("mailer stamps emailed_at");
    assert_eq!(affected, 1);

    let stored = world.notices.find_or_fail(1_u64).await.expect("read back");
    assert_eq!(stored.emailed_at, Some(at(13)));
    assert_eq!(stored.seen_at, Some(at(12)), "the concurrent write survives");
}

/// The guarded stamp: the motivating case, and the reason the `WHERE` has to
/// hold more than a primary key.
///
/// A job that emails a batch and stamps it must stamp *exactly* the rows it
/// mailed, and running twice must not stamp — or mail — anything a second time.
#[tokio::test]
async fn a_guarded_stamp_is_idempotent_across_runs() {
    let world = world().await;

    let unstamped = || Criteria::new().where_eq("profile_id", 1_u64).where_null("emailed_at");

    let first = world
        .notices
        .update_column(unstamped(), "emailed_at", at(9))
        .await
        .expect("first run stamps");
    assert_eq!(first, 3, "every un-emailed notice for this profile");

    let second = world
        .notices
        .update_column(unstamped(), "emailed_at", at(10))
        .await
        .expect("second run finds nothing");
    assert_eq!(second, 0, "a redelivered job stamps nothing, so it mails nothing");

    // And the first run's timestamp is the one that stuck.
    let stamped = world.notices.find_or_fail(1_u64).await.expect("read back");
    assert_eq!(stamped.emailed_at, Some(at(9)));

    // The other profile was never in scope.
    let untouched = world.notices.find_or_fail(4_u64).await.expect("read back");
    assert_eq!(untouched.emailed_at, None);
}

/// Rows affected is the count a caller can act on — how many were actually
/// claimed, as distinct from how many were asked for.
#[tokio::test]
async fn rows_affected_counts_the_rows_the_guard_let_through() {
    let world = world().await;

    world
        .notices
        .update_column(Criteria::new().where_eq("id", 2_u64), "emailed_at", at(8))
        .await
        .expect("one already stamped");

    let claimed = world
        .notices
        .update_column(
            Criteria::new().where_eq("profile_id", 1_u64).where_null("emailed_at"),
            "emailed_at",
            at(9),
        )
        .await
        .expect("stamp the rest");

    assert_eq!(claimed, 2, "three notices, one already stamped");
}

/// Several columns in one statement, which is the general form the single-column
/// helper is sugar over.
#[tokio::test]
async fn more_than_one_column_can_be_written_at_once() {
    let world = world().await;

    world
        .notices
        .update_matching(
            Criteria::new().where_eq("id", 1_u64),
            vec![("seen_at".into(), at(12).into()), ("emailed_at".into(), at(13).into())],
        )
        .await
        .expect("write both");

    let stored = world.notices.find_or_fail(1_u64).await.expect("read back");
    assert_eq!(stored.seen_at, Some(at(12)));
    assert_eq!(stored.emailed_at, Some(at(13)));
    assert_eq!(stored.profile_id, 1, "and nothing else moved");
}

/// An empty column list writes nothing and is not an error — a caller building
/// the list from a diff of changed fields can legitimately end up with none.
#[tokio::test]
async fn an_empty_column_list_changes_nothing() {
    let world = world().await;

    let affected = world
        .notices
        .update_matching(Criteria::new().where_eq("profile_id", 1_u64), vec![])
        .await
        .expect("not an error");

    assert_eq!(affected, 0);
    assert_eq!(world.notices.find_or_fail(1_u64).await.expect("read").emailed_at, None);
}

// --- soft deletes ----------------------------------------------------------

/// A write is not scoped, so a tombstoned row is reachable — which is what makes
/// this the way to restore one.
///
/// The contrast in the same test is the point: the row is invisible to a read
/// and writable by an update, and both halves are deliberate. See
/// `rainier_orm::trash`, which sets the policy out — a scoped write leaves a
/// purge unable to purge and a restore unable to restore, silently and forever.
#[tokio::test]
async fn a_tombstoned_row_is_hidden_from_reads_and_still_writable() {
    let world = world().await;

    world
        .documents
        .create(Document { id: 0, title: "draft".into(), deleted_at: None })
        .await
        .expect("document");

    // Tombstone it — itself a partial write, over a set rather than a row.
    let trashed = world
        .documents
        .update_column(Criteria::new().where_eq("title", "draft"), "deleted_at", at(1))
        .await
        .expect("trash");
    assert_eq!(trashed, 1);

    assert!(world.documents.all().await.expect("read").is_empty(), "the read is scoped");

    // The restore. Under a scoped write this would match nothing and report
    // success, and the document would be gone for good.
    let restored = world
        .documents
        .update_column(
            Criteria::new().where_eq("title", "draft"),
            "deleted_at",
            None::<DateTime<Utc>>,
        )
        .await
        .expect("restore");
    assert_eq!(restored, 1, "the write reached the tombstoned row");

    let back = world.documents.all().await.expect("read");
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].title, "draft");
    assert_eq!(back[0].deleted_at, None);
}

/// The contract is object-safe with the new method on it, so a handler can still
/// depend on `Arc<dyn Repository<T>>` and a fake can still stand in.
#[tokio::test]
async fn the_contract_stays_object_safe() {
    use std::sync::Arc;

    let world = world().await;
    let notices: Arc<dyn Repository<Notice>> = Arc::new(world.notices);

    let affected = notices
        .update_matching(
            Criteria::new().where_eq("id", 1_u64),
            vec![("emailed_at".into(), at(9).into())],
        )
        .await
        .expect("through the vtable");

    assert_eq!(affected, 1);
}
