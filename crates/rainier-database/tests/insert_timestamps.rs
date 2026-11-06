//! `created_at` / `updated_at` stamped on insert, against a real database.
//!
//! Applications build rows with `..Default::default()`, which leaves both
//! `None`. Before `rainier_orm::timestamps` that wrote NULL, silently: lewd.net
//! had 21 tables of rows that did not know when they were made. These read the
//! row back, because a test of the SQL could not tell "stamped" from "bound a
//! value the database then ignored".
#![cfg(feature = "sea-orm-executor")]

use chrono::{DateTime, TimeZone, Utc};
use rainier_database::{Database, EntityRepository, Model, Repository};
use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{Dialect, Entity, Executor, PoolConfig};

#[derive(Debug, Clone, PartialEq, Default, Entity)]
#[orm(table = "uploads")]
struct Upload {
    #[orm(pk, auto_increment)]
    id: u64,
    name: String,
    /// A timestamp that is not one of the two, which must stay NULL.
    published_at: Option<DateTime<Utc>>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

impl Model for Upload {}

async fn uploads() -> EntityRepository<Upload> {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect");
    for sql in rainier_orm::schema::schema_ddl::<Upload>(Dialect::Sqlite) {
        exec.execute(&sql, Vec::new()).await.expect("create table");
    }
    EntityRepository::new(Database::new(exec))
}

#[tokio::test]
async fn a_row_built_from_default_is_stamped_when_it_is_created() {
    let before = Utc::now() - chrono::Duration::seconds(1);

    let created =
        uploads().await.create(Upload { name: "a".into(), ..Default::default() }).await.unwrap();

    let stamped = created.created_at.expect("created_at was stamped");
    assert!(stamped >= before, "{stamped} is now, not some default");
    assert_eq!(created.updated_at, Some(stamped), "both get the same instant");
    assert_eq!(created.published_at, None, "only created_at and updated_at are touched");
}

#[tokio::test]
async fn an_explicit_time_is_kept() {
    let when = Utc.with_ymd_and_hms(2026, 8, 2, 23, 0, 0).unwrap();

    let created = uploads()
        .await
        .create(Upload { name: "backfill".into(), created_at: Some(when), ..Default::default() })
        .await
        .unwrap();

    assert_eq!(created.created_at, Some(when), "a backfill keeps its own time");
    assert!(created.updated_at.is_some(), "the empty one is still stamped");
}
