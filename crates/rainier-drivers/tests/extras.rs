//! upsert, partial UPDATE via the builder, and a string-backed enum column —
//! the generic features beyond plain CRUD. SQLite always,
//! MySQL when `TEST_DATABASE_URL` is set.
#![cfg(feature = "sea-orm-executor")]

use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::{
    impl_string_column, repo, schema, Base64Bytes, Entity, Error, Executor, Json, PoolConfig,
    Result, StringColumn,
};

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Pending,
    Active,
    Banned,
}

impl StringColumn for Status {
    fn to_column_str(&self) -> String {
        match self {
            Status::Pending => "pending",
            Status::Active => "active",
            Status::Banned => "banned",
        }
        .to_string()
    }
    fn from_column_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Status::Pending),
            "active" => Ok(Status::Active),
            "banned" => Ok(Status::Banned),
            other => Err(Error::msg(format!("unknown Status `{other}`"))),
        }
    }
}
impl_string_column!(Status);

#[derive(Debug, Clone, Entity)]
#[orm(table = "accounts")]
struct Account {
    #[orm(pk, auto_increment)]
    id: u64,
    #[orm(unique)]
    email: String,
    status: Status,        // enum stored as text
    label: Option<Status>, // nullable enum
    logins: i64,
    handle: Option<Base64Bytes>, // binary as uniform base64 text
    tags: Json<Vec<String>>,     // serde value as JSON text
}

// A keyed table without a surrogate id — pk is the natural key, for upsert.
#[derive(Debug, Clone, Entity)]
#[orm(table = "suppressions")]
struct Suppression {
    #[orm(pk)]
    email: String,
    reason: String,
}

async fn setup(exec: &SeaOrmExecutor) {
    for ddl in schema::schema_ddl::<Account>(exec.dialect()) {
        exec.execute(&ddl, vec![]).await.unwrap();
    }
    for ddl in schema::schema_ddl::<Suppression>(exec.dialect()) {
        exec.execute(&ddl, vec![]).await.unwrap();
    }
}

async fn run(exec: SeaOrmExecutor) {
    setup(&exec).await;

    // string enum round-trips (incl. nullable).
    let id = repo::insert(
        &exec,
        &Account {
            id: 0,
            email: "a@x.io".into(),
            status: Status::Pending,
            label: None,
            logins: 0,
            handle: Some(Base64Bytes(vec![1, 2, 255, 0, 16])),
            tags: Json(vec!["a".to_string(), "b".to_string()]),
        },
    )
    .await
    .unwrap();
    let got: Account = repo::find_by_pk(&exec, id).await.unwrap().unwrap();
    assert_eq!(got.status, Status::Pending);
    assert_eq!(got.label, None);
    // binary round-trips as base64 text on every dialect.
    assert_eq!(got.handle, Some(Base64Bytes(vec![1, 2, 255, 0, 16])));
    // Json<T> round-trips through a TEXT column.
    assert_eq!(got.tags.into_inner(), vec!["a".to_string(), "b".to_string()]);

    // atomic increment: SET logins = logins + 3.
    let n =
        repo::query::<Account>().where_eq("id", id).increment(&exec, "logins", 3).await.unwrap();
    assert_eq!(n, 1);
    let got: Account = repo::find_by_pk(&exec, id).await.unwrap().unwrap();
    assert_eq!(got.logins, 3);

    // partial UPDATE via the builder — bump logins, set status, without
    // rewriting the whole row.
    let n = repo::query::<Account>()
        .where_eq("id", id)
        .update(
            &exec,
            vec![("status", Status::Active.to_column_str().into()), ("logins", 5i64.into())],
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
    let got: Account = repo::find_by_pk(&exec, id).await.unwrap().unwrap();
    assert_eq!(got.status, Status::Active);
    assert_eq!(got.logins, 5);

    // upsert on a natural key: insert, then conflict → update reason.
    let sup = Suppression { email: "spam@x.io".into(), reason: "bounce".into() };
    repo::upsert(&exec, &sup, &["email"], &["reason"]).await.unwrap();
    let sup2 = Suppression { email: "spam@x.io".into(), reason: "complaint".into() };
    repo::upsert(&exec, &sup2, &["email"], &["reason"]).await.unwrap();
    let rows: Vec<Suppression> = repo::all(&exec).await.unwrap();
    assert_eq!(rows.len(), 1, "upsert must not duplicate the key");
    assert_eq!(rows[0].reason, "complaint");

    // insert-or-ignore: empty update set leaves the existing row alone.
    let sup3 = Suppression { email: "spam@x.io".into(), reason: "ignored".into() };
    repo::upsert(&exec, &sup3, &["email"], &[]).await.unwrap();
    let again: Option<Suppression> = repo::find_by_pk(&exec, "spam@x.io").await.unwrap();
    assert_eq!(again.unwrap().reason, "complaint");
}

#[tokio::test]
async fn extras_sqlite() {
    let exec = SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless()).await.unwrap();
    run(exec).await;
}

#[tokio::test]
async fn extras_mysql() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL unset — skipping MySQL extras check");
        return;
    };
    let exec = SeaOrmExecutor::connect(&url, &PoolConfig::default()).await.unwrap();
    for t in ["accounts", "suppressions"] {
        let _ = exec.execute(&format!("DROP TABLE IF EXISTS {t}"), vec![]).await;
    }
    run(exec).await;
}
