//! Sharding **policy** on top of the routing primitives in [`crate::shard`].
//!
//! [`crate::shard`] gives you the mechanism — a [`ShardedExecutor`] that routes
//! each operation to a physical database via a [`ShardLocator`]. This module
//! adds the *policy* an application needs to actually run a fleet:
//!
//! * [`IdAllocator`] — mint shard-encoded, time-ordered `u64` ids (the id
//!   carries its shard in the high bits, so any id self-routes).
//! * [`ShardRegistry`] / [`ShardRecord`] — the global `shards` table: every
//!   physical shard's name, capacity, and lifecycle [`ShardStatus`].
//! * [`Placer`] / [`Placement`] — capacity-aware placement: admit new rows to
//!   the active shard until it crosses its admit threshold, then seal it and
//!   promote a standby.
//! * [`Directory`] / [`DirectoryEntry`] — a global `key → id` map for resolving
//!   a natural key (e.g. a login email) to its shard-encoded id with no fan-out.
//! * [`refresh_shard_sizes`] — measure each shard's on-disk size (SQLite/D1) and
//!   record it so [`Placer`] can seal a full shard.
//!
//! It is all backend-agnostic and wasm-safe — generic over [`Executor`], no
//! `tokio`/`sqlx`. The control tables ([`ShardRecord`], [`DirectoryEntry`]) are
//! ordinary [`Entity`]s, so their schema is derived; [`control_tables_ddl`]
//! renders it for a dialect (the sharded surface applies it to its global DB).
//!
//! On a single, unsharded database the routing is a no-op (the default
//! [`Executor`] methods ignore the route) and the registry/placement simply go
//! unused — so the *same* application code runs sharded or not.
//!
//! [`ShardedExecutor`]: crate::ShardedExecutor
//! [`ShardLocator`]: crate::ShardLocator

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};

use crate::{
    repo, Dialect, Entity, Executor, ShardCatalog, ShardCodec, ShardId, ShardLocator, ShardRoute,
    ShardedExecutor, StringColumn,
};

// ---------------------------------------------------------------------------
// Id allocation
// ---------------------------------------------------------------------------

/// Custom epoch (2024-01-01T00:00:00Z) so the 42-bit ms timestamp lasts ~139 y.
const EPOCH_MS: i64 = 1_704_067_200_000;
/// Counter bits within a millisecond (≤1024 ids/ms before borrowing ahead).
const COUNTER_BITS: u32 = 10;

/// Mints monotonically increasing, collision-free sequences, and (with a
/// [`ShardCodec`]) shard-encoded ids.
///
/// Snowflake-style: a 42-bit millisecond timestamp (since 2024) plus a counter
/// form a monotonic [`next_seq`](Self::next_seq); [`next`](Self::next) packs
/// that into a codec's sequence bits with the shard in the top bits. A single
/// counter backs every shard — composing with the shard number keeps cross-shard
/// ids distinct, and the time seed keeps them sortable and restart-safe.
/// `Clone` shares the counter (it's an `Arc<AtomicU64>`), so cloning into app
/// state — or into each shard connector — keeps one global monotonic source.
#[derive(Clone)]
pub struct IdAllocator {
    seq: Arc<AtomicU64>,
}

impl IdAllocator {
    pub fn new() -> Self {
        Self { seq: Arc::new(AtomicU64::new(0)) }
    }

    /// A fresh monotonic, time-seeded sequence value — no shard bits. Compose it
    /// with a [`ShardCodec`] (see [`next`](Self::next)) to get a shard-encoded id,
    /// or use it directly as an unsharded id.
    pub fn next_seq(&self) -> u64 {
        let now = ((Utc::now().timestamp_millis() - EPOCH_MS).max(0) as u64) << COUNTER_BITS;
        loop {
            let prev = self.seq.load(Ordering::Acquire);
            let next = prev.wrapping_add(1).max(now);
            if self
                .seq
                .compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break next;
            }
        }
    }

    /// A fresh shard-encoded id for `shard` under `codec`: `codec`'s high bits
    /// hold the shard, the low bits a monotonic sequence.
    pub fn next(&self, codec: ShardCodec, shard: ShardId) -> u64 {
        codec.compose(shard, self.next_seq())
    }

    /// The process-global allocator — one monotonic counter shared by every
    /// caller. A clone shares the counter, so ids stay unique across call sites.
    /// Codec-free: callers apply their own [`ShardCodec`] via [`next`](Self::next),
    /// so one global counter serves any number of shard families.
    pub fn global() -> Self {
        static G: std::sync::OnceLock<IdAllocator> = std::sync::OnceLock::new();
        G.get_or_init(IdAllocator::new).clone()
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Connector configuration
// ---------------------------------------------------------------------------

/// The sharding configuration a surface hands its connector — everything the ORM
/// needs to route and mint, in one struct, so a deployment declares its policy
/// explicitly in one place rather than threading three loose values.
///
/// * `family` — the shard **family**: the naming prefix for shard databases
///   (`{family}_shard_0001`, see [`crate::shard_db_name`]) and the family this
///   connector routes within ([`Executor::shard_family`]).
/// * `codec` — the id **layout**: how the shard is packed into a shard-encoded
///   id. Drives id minting ([`mint`](Self::mint)) and decoding a row's owning
///   shard from its key ([`shard_of`](Self::shard_of)).
/// * `locator` — the routing **model** (the "key/hashing"): how a routed key
///   maps to a physical shard. The default [`ShardCodec`] reads the shard from
///   the id's high bits (encoded-id, no lookup); swap in
///   [`HashLocator`](crate::HashLocator) or [`SlotLocator`](crate::SlotLocator)
///   for a tier whose key isn't shard-encoded (e.g. a hashed directory).
///
/// Build the connector's [`ShardedExecutor`] with [`executor`](Self::executor)
/// and mint ids with [`mint`](Self::mint); the codec and locator stay
/// consistent because they come from the one settings value.
#[derive(Clone, Debug)]
pub struct ShardingSettings<L = ShardCodec> {
    pub family: String,
    pub codec: ShardCodec,
    pub locator: L,
}

impl ShardingSettings<ShardCodec> {
    /// Encoded-id routing — the common case: the shard lives in the id's high
    /// bits, so the [`ShardCodec`] is *both* the id layout and the locator.
    pub fn encoded(family: impl Into<String>, codec: ShardCodec) -> Self {
        Self { family: family.into(), codec, locator: codec }
    }
}

impl<L: ShardLocator + Clone> ShardingSettings<L> {
    /// A custom routing model (hash / slot / …): the `codec` still defines the id
    /// layout for minting, while `locator` decides where each routed key lands.
    pub fn with_locator(family: impl Into<String>, codec: ShardCodec, locator: L) -> Self {
        Self { family: family.into(), codec, locator }
    }

    /// Build the routed [`ShardedExecutor`] over `catalog` with this policy — the
    /// connector the generic services run on.
    pub fn executor<C: ShardCatalog>(&self, catalog: C) -> ShardedExecutor<C, L> {
        ShardedExecutor::new(catalog, self.locator.clone(), self.family.clone())
    }

    /// Mint a shard-encoded id on `shard`, using `allocator` for the monotonic
    /// sequence and this settings' codec for the layout.
    pub fn mint(&self, allocator: &IdAllocator, shard: ShardId) -> u64 {
        allocator.next(self.codec, shard)
    }

    /// The shard that owns `key`, decoded from a shard-encoded id's high bits —
    /// used to mint a child row on its parent's shard. (Routing an arbitrary,
    /// non-encoded key is the `locator`'s job, applied inside the executor.)
    pub fn shard_of(&self, key: u64) -> ShardId {
        self.codec.shard_of(key)
    }
}

// ---------------------------------------------------------------------------
// Shard registry
// ---------------------------------------------------------------------------

/// Lifecycle of a shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardStatus {
    /// Being created (database provisioned, schema migrating).
    Provisioning,
    /// Accepting new rows (until it fills past its admit threshold).
    Active,
    /// Empty and ready — promoted to active the moment the active shard seals.
    Standby,
    /// No longer admits new rows; existing keys keep growing into it.
    Sealed,
    /// Read-only / retired.
    Disabled,
}

impl StringColumn for ShardStatus {
    fn to_column_str(&self) -> String {
        match self {
            ShardStatus::Provisioning => "provisioning",
            ShardStatus::Active => "active",
            ShardStatus::Standby => "standby",
            ShardStatus::Sealed => "sealed",
            ShardStatus::Disabled => "disabled",
        }
        .to_string()
    }
    fn from_column_str(s: &str) -> Result<Self> {
        Ok(match s {
            "active" => ShardStatus::Active,
            "standby" => ShardStatus::Standby,
            "sealed" => ShardStatus::Sealed,
            "disabled" => ShardStatus::Disabled,
            _ => ShardStatus::Provisioning,
        })
    }
}
crate::impl_string_column!(ShardStatus);

/// One row of the global `shards` registry. Not itself sharded.
#[derive(Debug, Clone, Entity)]
#[orm(table = "shards")]
pub struct ShardRecord {
    /// Logical shard number — the high bits of every id placed here. App-assigned
    /// (it equals the shard a [`ShardCodec`] encodes), so deliberately NOT
    /// auto-increment.
    #[orm(pk)]
    pub id: u32,
    /// Deterministic physical db / binding name (see [`crate::shard_db_name`]).
    pub name: String,
    /// Backend-specific database id / binding (None on a single-DB deployment).
    pub database_id: Option<String>,
    pub status: ShardStatus,
    pub size_bytes: i64,
    pub max_bytes: i64,
    /// Seal to new admissions at this fill ratio (0.50 = 50 %).
    pub admit_threshold: f64,
    pub created_at: DateTime<Utc>,
    pub sealed_at: Option<DateTime<Utc>>,
    pub last_size_check_at: Option<DateTime<Utc>>,
}

impl ShardRecord {
    pub fn fill_ratio(&self) -> f64 {
        if self.max_bytes <= 0 {
            return 1.0;
        }
        self.size_bytes as f64 / self.max_bytes as f64
    }

    /// True when this shard is active and still under its admit threshold.
    pub fn accepts_admission(&self) -> bool {
        self.status == ShardStatus::Active && self.fill_ratio() < self.admit_threshold
    }
}

/// Registry operations over the global executor.
#[derive(Clone)]
pub struct ShardRegistry<X> {
    exec: X,
}

impl<X: Executor> ShardRegistry<X> {
    pub fn new(exec: X) -> Self {
        Self { exec }
    }

    pub async fn list(&self) -> Result<Vec<ShardRecord>> {
        repo::all(&self.exec).await
    }

    pub async fn get(&self, id: u32) -> Result<Option<ShardRecord>> {
        repo::find_by_pk(&self.exec, id).await
    }

    /// Insert a shard record (id is app-assigned, the logical shard number).
    pub async fn register(&self, record: &ShardRecord) -> Result<()> {
        repo::insert(&self.exec, record).await?;
        Ok(())
    }

    pub async fn set_status(&self, id: u32, status: ShardStatus) -> Result<()> {
        repo::query::<ShardRecord>()
            .where_eq("id", id)
            .update(&self.exec, vec![("status", status.to_column_str().into())])
            .await?;
        Ok(())
    }

    /// Record a fresh size measurement (from the size-check job).
    pub async fn set_size(&self, id: u32, size_bytes: i64) -> Result<()> {
        let now = Utc::now();
        repo::query::<ShardRecord>()
            .where_eq("id", id)
            .update(
                &self.exec,
                vec![("size_bytes", size_bytes.into()), ("last_size_check_at", now.into())],
            )
            .await?;
        Ok(())
    }

    /// Seal a shard to new admissions (existing keys keep growing into it).
    pub async fn seal(&self, id: u32) -> Result<()> {
        let now = Utc::now();
        repo::query::<ShardRecord>()
            .where_eq("id", id)
            .update(
                &self.exec,
                vec![
                    ("status", ShardStatus::Sealed.to_column_str().into()),
                    ("sealed_at", now.into()),
                ],
            )
            .await?;
        Ok(())
    }
}

/// The shard ids a [`ShardCatalog`] should wire up — every
/// non-disabled shard. (The catalog opens an executor per id by deterministic
/// name.)
pub fn live_shard_ids(records: &[ShardRecord]) -> Vec<u32> {
    records.iter().filter(|r| r.status != ShardStatus::Disabled).map(|r| r.id).collect()
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// The outcome of placing a new key: its shard and shard-encoded id.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub shard: ShardId,
    pub id: u64,
}

/// Capacity-aware placement: where a new key's data goes, and its id.
///
/// Admit new keys to the active shard until it crosses its admit threshold
/// (~50 %, leaving the existing cohort room to grow), then seal it and promote a
/// pre-provisioned standby. On a single-database deployment there is one active
/// shard (id 0) that never fills, so every key lands there.
#[derive(Clone)]
pub struct Placer<X> {
    registry: ShardRegistry<X>,
    allocator: IdAllocator,
    codec: ShardCodec,
}

impl<X: Executor> Placer<X> {
    pub fn new(registry: ShardRegistry<X>, allocator: IdAllocator, codec: ShardCodec) -> Self {
        Self { registry, allocator, codec }
    }

    fn compose(&self, shard: ShardId) -> Placement {
        Placement { shard, id: self.allocator.next(self.codec, shard) }
    }

    /// Choose a shard for a new key and mint its shard-encoded id.
    pub async fn place_new(&self) -> Result<Placement> {
        let shards = self.registry.list().await?;

        // Fast path: an active shard still under its admit threshold.
        if let Some(s) = shards.iter().find(|r| r.accepts_admission()) {
            return Ok(self.compose(s.id));
        }

        // The admitting shard(s) are full — seal them.
        for s in shards
            .iter()
            .filter(|r| r.status == ShardStatus::Active && r.fill_ratio() >= r.admit_threshold)
        {
            self.registry.seal(s.id).await?;
        }

        // Promote a pre-provisioned standby to active and place there.
        if let Some(sb) = shards.iter().find(|r| r.status == ShardStatus::Standby) {
            self.registry.set_status(sb.id, ShardStatus::Active).await?;
            return Ok(self.compose(sb.id));
        }

        bail!(
            "no shard available to admit a new key — provision a standby \
             (the provisioning controller keeps one ready)"
        )
    }
}

// ---------------------------------------------------------------------------
// Directory
// ---------------------------------------------------------------------------

/// Normalise a natural key for the directory (trim + lowercase) — suitable for
/// case-insensitive keys like email addresses.
pub fn normalize_key(key: &str) -> String {
    key.trim().to_lowercase()
}

/// One directory row: a natural `key` (e.g. a normalized email) → a
/// shard-encoded `id`. The shard lives *inside* the id, so no `shard` column is
/// needed and resolving a key is a single global lookup.
#[derive(Debug, Clone, PartialEq, Eq, Entity)]
#[orm(table = "shard_directory")]
pub struct DirectoryEntry {
    #[orm(pk)]
    pub key: String,
    /// Shard-encoded — resolving this is the only lookup a router needs.
    pub id: u64,
}

/// Port over the directory so resolution can be mocked / swapped.
#[allow(async_fn_in_trait)]
pub trait DirectoryRepository {
    async fn resolve(&self, key: &str) -> Result<Option<u64>>;
    async fn put(&self, key: &str, id: u64) -> Result<()>;
}

#[derive(Clone)]
pub struct Directory<X> {
    exec: X,
}

impl<X: Executor> Directory<X> {
    pub fn new(exec: X) -> Self {
        Self { exec }
    }
}

impl<X: Executor> DirectoryRepository for Directory<X> {
    /// Resolve a natural key to its shard-encoded id.
    async fn resolve(&self, key: &str) -> Result<Option<u64>> {
        let row: Option<DirectoryEntry> = repo::find_by_pk(&self.exec, normalize_key(key)).await?;
        Ok(row.map(|r| r.id))
    }

    /// Record a new `key → id` mapping. Idempotent on the key.
    async fn put(&self, key: &str, id: u64) -> Result<()> {
        let row = DirectoryEntry { key: normalize_key(key), id };
        repo::upsert(&self.exec, &row, &["key"], &["id"]).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Size check
// ---------------------------------------------------------------------------

/// SQLite/D1: bytes = page_count × page_size.
const SIZE_SQL: &str = "SELECT (SELECT page_count FROM pragma_page_count()) * \
                        (SELECT page_size FROM pragma_page_size()) AS bytes";

/// Measure every registered shard that has its own database and update its
/// `size_bytes`, so [`Placer`] can seal a shard once it crosses its admit
/// threshold. `exec` is the sharded fleet executor; `registry` reads/writes the
/// global `shards` table; `codec` composes a probe id that routes to each shard.
///
/// SQLite/D1-only: the size query uses SQLite pragmas. A single MySQL has no cap
/// and no size-check; its lone shard never seals.
pub async fn refresh_shard_sizes<X: Executor>(
    exec: &X,
    registry: &ShardRegistry<X>,
    codec: ShardCodec,
) -> Result<()> {
    for s in registry.list().await? {
        if s.database_id.is_none() {
            continue; // single-DB / not a real fleet shard
        }
        // Route the probe to this shard (any id in the shard targets it).
        let probe = codec.compose(s.id, 0);
        // Scope the fetched rows so the (possibly `!Send`) `Box<dyn Row>` handles
        // drop before the next await — otherwise they'd be held across
        // `set_size`, making this future `!Send` on a multi-threaded runtime.
        let bytes = {
            let rows = exec.fetch_all_routed(ShardRoute::Key(probe), SIZE_SQL, vec![]).await?;
            rows.first().and_then(|r| r.get_i64("bytes").ok().flatten()).unwrap_or(0)
        };
        registry.set_size(s.id, bytes).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Control-table DDL
// ---------------------------------------------------------------------------

/// The DDL for the sharding **control tables** — the [`ShardRecord`] registry and
/// the [`DirectoryEntry`] directory — rendered for `dialect`, in execution order.
///
/// The control tables live on the sharded surface's *global* database, so that
/// surface (e.g. a Cloudflare D1 deployment) applies these as its sharding
/// migration; an unsharded surface never creates them. The schema is derived
/// from the entities themselves, so it can't drift from what the services query.
pub fn control_tables_ddl(dialect: Dialect) -> Vec<String> {
    let mut out = crate::schema::schema_ddl::<ShardRecord>(dialect);
    out.extend(crate::schema::schema_ddl::<DirectoryEntry>(dialect));
    out
}

/// Ensure the sharding control tables exist on `global`, idempotently
/// (`CREATE TABLE IF NOT EXISTS`).
///
/// This is the **ORM's** responsibility, not the application's: a sharded
/// deployment needs the `shards` registry and the `shard_directory` to function,
/// but those tables are an implementation detail of sharding itself — so the ORM
/// creates them, and no flavor (Cloudflare or otherwise) declares them in its
/// own migrations. [`Migrator::run`](crate::migrate::Migrator::run) calls this
/// automatically whenever the executor reports it is sharded
/// ([`Executor::is_sharded`]); call it directly if you migrate without a
/// `Migrator`. `global` is the sharded connector (its unrouted writes land on
/// the global/directory database) or that database's own executor.
pub async fn ensure_control_tables<X: Executor>(global: &X) -> Result<()> {
    for ddl in control_tables_ddl(global.dialect()) {
        global.execute(&ddl, vec![]).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_shard_encoded_and_monotonic() {
        let codec = ShardCodec::DEFAULT;
        let alloc = IdAllocator::new();
        let a = alloc.next(codec, 7);
        let b = alloc.next(codec, 7);
        assert_eq!(codec.shard_of(a), 7);
        assert_eq!(codec.shard_of(b), 7);
        assert!(b > a, "ids increase");
        // Different shard → different high bits, distinct id space.
        let c = alloc.next(codec, 9);
        assert_eq!(codec.shard_of(c), 9);
        assert_ne!(codec.shard_of(a), codec.shard_of(c));
    }

    #[test]
    fn next_seq_is_monotonic() {
        let alloc = IdAllocator::new();
        let mut last = 0;
        for _ in 0..1000 {
            let n = alloc.next_seq();
            assert!(n > last, "sequence strictly increases");
            last = n;
        }
    }

    #[test]
    fn normalize_key_trims_and_lowercases() {
        assert_eq!(normalize_key("  Foo@Bar.COM "), "foo@bar.com");
    }

    #[test]
    fn admission_respects_threshold_and_status() {
        let mut r = ShardRecord {
            id: 0,
            name: "x_shard_0000".into(),
            database_id: None,
            status: ShardStatus::Active,
            size_bytes: 4,
            max_bytes: 10,
            admit_threshold: 0.5,
            created_at: Utc::now(),
            sealed_at: None,
            last_size_check_at: None,
        };
        assert!(r.accepts_admission(), "active + under threshold admits");
        r.size_bytes = 6; // 0.6 ≥ 0.5
        assert!(!r.accepts_admission(), "over threshold seals out");
        r.size_bytes = 1;
        r.status = ShardStatus::Sealed;
        assert!(!r.accepts_admission(), "sealed never admits");
    }

    #[test]
    fn settings_encoded_uses_codec_as_locator() {
        let s = ShardingSettings::encoded("users", ShardCodec::new(8));
        assert_eq!(s.family, "users");
        // The locator (the codec) routes an encoded id back to its shard.
        let id = s.codec.compose(5, 1);
        assert_eq!(s.locator.locate(id), 5);
        // mint composes on the requested shard; shard_of decodes it.
        let alloc = IdAllocator::new();
        assert_eq!(s.shard_of(s.mint(&alloc, 3)), 3);
    }

    #[test]
    fn settings_with_hash_locator_routes_by_hash() {
        use crate::HashLocator;
        let s = ShardingSettings::with_locator("dir", ShardCodec::DEFAULT, HashLocator::new(4));
        // The hash model maps every key into [0, 4) regardless of id layout…
        for k in 0..200u64 {
            assert!(s.locator.locate(k) < 4);
        }
        // …while the codec still mints under the default layout.
        let alloc = IdAllocator::new();
        assert_eq!(s.shard_of(s.mint(&alloc, 2)), 2);
    }

    #[test]
    fn control_tables_ddl_renders_both_tables() {
        let sql = control_tables_ddl(Dialect::Sqlite).join("\n");
        assert!(sql.contains("shards"), "registry table present");
        assert!(sql.contains("shard_directory"), "directory table present");
    }
}
