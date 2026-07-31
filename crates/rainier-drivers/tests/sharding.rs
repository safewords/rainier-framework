//! The transparency proof: one generic function, using only the normal
//! `repo::` API, runs **unchanged** against a single database and against a
//! sharded executor over several databases — data routing by shard-encoded id.
//! This is the "zero code change to switch engines" property.
//!
//! Uses separate in-memory SQLite connections as stand-in shards (each
//! `sqlite::memory:` connection is its own database).
#![cfg(feature = "sea-orm-executor")]

use rainier_drivers::sql::SeaOrmExecutor;
use rainier_orm::migrate::Migrator;
use rainier_orm::{
    repo, Entity, Executor, MapCatalog, PoolConfig, ShardCatalog, ShardCodec, ShardLocator,
    ShardedExecutor,
};

// 4-bit shard (16 shards) + 60-bit per-shard sequence.
const CODEC: ShardCodec = ShardCodec::new(4);

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "accounts")]
struct Account {
    // App-assigned, shard-encoded primary key — the shard key, by proxy.
    // No family here: the family is a connector setting.
    #[orm(pk, shard_key)]
    id: u64,
    name: String,
}

/// A "service" function — it has NO idea sharding exists. It just takes an
/// executor and uses the generic repo API.
async fn create_then_fetch<X: Executor>(db: &X, id: u64, name: &str) -> Account {
    repo::insert(db, &Account { id, name: name.to_string() }).await.expect("insert");
    repo::find_by_pk::<Account, _, _>(db, id).await.expect("find").expect("present")
}

async fn fresh_sqlite() -> SeaOrmExecutor {
    SeaOrmExecutor::connect("sqlite::memory:", &PoolConfig::serverless())
        .await
        .expect("connect sqlite")
}

#[tokio::test]
async fn same_service_code_single_and_sharded() {
    // (1) Single database — the function works directly.
    let single = fresh_sqlite().await;
    Migrator::new().create_table::<Account>("0001_accounts").run(&single).await.unwrap();
    let solo = create_then_fetch(&single, CODEC.compose(0, 1), "solo").await;
    assert_eq!(solo.name, "solo");
    // A plain connector is explicitly unsharded.
    assert_eq!(single.shard_family(), None);
    assert!(!single.is_sharded());

    // (2) Sharded — the SAME function, just a different executor.
    let global = fresh_sqlite().await;
    let shard0 = fresh_sqlite().await;
    let shard1 = fresh_sqlite().await;
    // Each shard gets the schema.
    Migrator::new()
        .create_table::<Account>("0001_accounts")
        .run_on_each(&[&shard0, &shard1])
        .await
        .unwrap();

    let catalog = MapCatalog::new(global).with_shard(0, shard0).with_shard(1, shard1);
    // The FAMILY is a connector setting, declared here — not on the entity.
    let sharded = ShardedExecutor::new(catalog, CODEC, "accounts");
    assert_eq!(sharded.shard_family(), Some("accounts"));
    assert!(sharded.is_sharded());

    let id_alice = CODEC.compose(0, 1); // → shard 0
    let id_bob = CODEC.compose(1, 1); // → shard 1
    let alice = create_then_fetch(&sharded, id_alice, "alice").await;
    let bob = create_then_fetch(&sharded, id_bob, "bob").await;
    assert_eq!(alice.name, "alice");
    assert_eq!(bob.name, "bob");

    // Physical placement: alice's row is ONLY in shard 0, bob's ONLY in shard 1.
    let in0: Vec<Account> = repo::all(sharded.catalog().shard(0).unwrap()).await.unwrap();
    let in1: Vec<Account> = repo::all(sharded.catalog().shard(1).unwrap()).await.unwrap();
    assert_eq!(in0.len(), 1, "exactly one row in shard 0");
    assert_eq!(in0[0].name, "alice");
    assert_eq!(in1.len(), 1, "exactly one row in shard 1");
    assert_eq!(in1[0].name, "bob");

    // Cross-shard reads route correctly through the sharded executor.
    let a2: Option<Account> = repo::find_by_pk(&sharded, id_alice).await.unwrap();
    let b2: Option<Account> = repo::find_by_pk(&sharded, id_bob).await.unwrap();
    assert_eq!(a2.unwrap().name, "alice");
    assert_eq!(b2.unwrap().name, "bob");

    // A routing miss (unknown shard) is an error, not a silent wrong answer.
    let bad = CODEC.compose(9, 1); // shard 9 not in the catalog
    assert!(repo::find_by_pk::<Account, _, _>(&sharded, bad).await.is_err());
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let db = fresh_sqlite().await;
    let m = Migrator::new().create_table::<Account>("0001_accounts");
    assert_eq!(m.run(&db).await.unwrap(), vec!["0001_accounts"]);
    assert!(m.run(&db).await.unwrap().is_empty(), "re-run applies nothing");
}

// A directory entity sharded by a *string* key (hashed email) — the tier that
// partitions when even the routing table outgrows one database.
#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "directory")]
struct DirEntry {
    #[orm(pk, shard_key)]
    email: String,
    user_id: u64,
}

#[tokio::test]
async fn directory_routes_by_hashed_string_key() {
    use rainier_orm::SlotLocator;

    let global = fresh_sqlite().await;
    let d0 = fresh_sqlite().await;
    let d1 = fresh_sqlite().await;
    Migrator::new()
        .create_table::<DirEntry>("0001_directory")
        .run_on_each(&[&d0, &d1])
        .await
        .unwrap();

    // 8 fixed slots; even slots → shard 0, odd → shard 1 (a hand-built map).
    let map: Vec<u32> = (0..8).map(|i| i % 2).collect();
    let locator = SlotLocator::new(map);
    let catalog = MapCatalog::new(global).with_shard(0, d0).with_shard(1, d1);
    let dir = ShardedExecutor::new(catalog, locator, "users_directory");

    // Insert a batch of emails; each lands in the shard its slot maps to.
    let emails = ["a@x.io", "b@x.io", "c@x.io", "d@x.io", "e@x.io", "f@x.io"];
    for (i, e) in emails.iter().enumerate() {
        repo::insert(&dir, &DirEntry { email: e.to_string(), user_id: i as u64 }).await.unwrap();
    }

    // Every email reads back through the router (routed by hashed string).
    for (i, e) in emails.iter().enumerate() {
        let got: Option<DirEntry> = repo::find_by_pk(&dir, e.to_string()).await.unwrap();
        assert_eq!(got.unwrap().user_id, i as u64, "routed lookup for {e}");
    }

    // Placement is consistent with the slot map: a row is in exactly the shard
    // the locator picks, and the two shards partition the set.
    let in0: Vec<DirEntry> = repo::all(dir.catalog().shard(0).unwrap()).await.unwrap();
    let in1: Vec<DirEntry> = repo::all(dir.catalog().shard(1).unwrap()).await.unwrap();
    assert_eq!(in0.len() + in1.len(), emails.len());
    for row in in0.iter().chain(in1.iter()) {
        let want = dir.locator().locate(rainier_orm::stable_hash(row.email.as_bytes()));
        let here: Vec<DirEntry> = repo::all(dir.catalog().shard(want).unwrap()).await.unwrap();
        assert!(here.iter().any(|r| r.email == row.email));
    }
}

// An executor that mints shard-encoded ids — proving id assignment lives in
// the connector, transparent to the (unchanged) insert call.
struct AllocExec {
    inner: SeaOrmExecutor,
    codec: ShardCodec,
    ctr: std::sync::atomic::AtomicU64,
}

impl Executor for AllocExec {
    fn dialect(&self) -> rainier_orm::Dialect {
        self.inner.dialect()
    }
    async fn fetch_all(
        &self,
        sql: &str,
        p: Vec<rainier_orm::sea_query::Value>,
    ) -> rainier_orm::Result<Vec<Box<dyn rainier_orm::Row>>> {
        self.inner.fetch_all(sql, p).await
    }
    async fn execute(
        &self,
        sql: &str,
        p: Vec<rainier_orm::sea_query::Value>,
    ) -> rainier_orm::Result<rainier_orm::ExecOutcome> {
        self.inner.execute(sql, p).await
    }
    fn allocate_id(&self, shard_key: u64) -> Option<u64> {
        let shard = self.codec.shard_of(shard_key);
        let seq = self.ctr.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        Some(self.codec.compose(shard, seq))
    }
}

#[derive(Debug, Clone, PartialEq, Entity)]
#[orm(table = "child")]
struct Child {
    #[orm(pk, shard_key)]
    id: u64, // unset on insert — the connector mints it
    #[orm(shard_key)]
    user_id: u64,
    label: String,
}

#[tokio::test]
async fn connector_assigns_shard_encoded_pk() {
    let inner = fresh_sqlite().await;
    let exec = AllocExec { inner, codec: CODEC, ctr: std::sync::atomic::AtomicU64::new(0) };
    Migrator::new().create_table::<Child>("0001_child").run(&exec).await.unwrap();

    // The service-style call: id is 0; user_id is in shard 3. No allocation code
    // here — the connector assigns the id.
    let user_id = CODEC.compose(3, 99);
    let new_id =
        repo::insert(&exec, &Child { id: 0, user_id, label: "x".into() }).await.unwrap() as u64;

    assert_ne!(new_id, 0, "connector assigned an id");
    assert_eq!(CODEC.shard_of(new_id), 3, "id is in the user's shard");
    let got: Child = repo::find_by_pk(&exec, new_id).await.unwrap().unwrap();
    assert_eq!(got.user_id, user_id);
}

#[test]
fn codec_round_trips() {
    let c = ShardCodec::new(4);
    let id = c.compose(7, 12345);
    assert_eq!(c.shard_of(id), 7);
    assert_eq!(c.local_of(id), 12345);
}

#[test]
fn family_names_shards_deterministically() {
    use rainier_orm::{directory_db_name, shard_db_name};
    assert_eq!(shard_db_name("users", 1), "users_shard_0001");
    assert_eq!(shard_db_name("users", 42), "users_shard_0042");
    assert_eq!(directory_db_name("users"), "users_directory");
}
