//! The sharding executor — turns N physical databases into one logical
//! [`Executor`] that routes each operation to the right shard.
//!
//! This is the *only* sharding-aware executor; it's a drop-in `Executor`, so
//! the `repo`/`query` layer and the services above are byte-identical whether
//! they run against a single database or a [`ShardedExecutor`]. The repo layer
//! decodes a shard-encoded id into a [`ShardRoute`]; the
//! executor decodes the shard number from it ([`ShardCodec`]) and dispatches.
//! Swap a `ShardedExecutor<D1…>` for a `SeaOrmExecutor` (MySQL) and the rest of
//! the program does not change.

use crate::{Dialect, Error, ExecOutcome, Executor, Result, Row, ShardCodec, ShardRoute};
use sea_query::Value;

/// A physical shard number (the high bits of a shard-encoded id).
pub type ShardId = u32;

/// Zero-padding width for shard numbers in [`shard_db_name`] (4 → `0001`).
pub const SHARD_NAME_WIDTH: usize = 4;

/// The deterministic physical database / binding name for a shard:
/// `"{family}_shard_{n:04}"`.
///
/// The shard **family** is a naming prefix for a series of shards, so a logical
/// shard number maps to a concrete D1 database name with no registry lookup —
/// e.g. `shard_db_name("users", 1)` → `"users_shard_0001"`. A catalog
/// (or the provisioner) uses this to find every shard's connection from the
/// family + number alone.
pub fn shard_db_name(family: &str, shard: ShardId) -> String {
    format!("{family}_shard_{shard:0w$}", w = SHARD_NAME_WIDTH)
}

/// The directory / global database name for a family: `"{family}_directory"`
/// (the non-sharded tier — routing entries, reference data).
pub fn directory_db_name(family: &str) -> String {
    format!("{family}_directory")
}

/// Maps a routed shard-key value to a physical shard — the routing **policy**,
/// kept pluggable so the same machinery serves different tiers:
///
/// - [`ShardCodec`] (encoded-id): the shard is the high bits of the id. No
///   lookup, but a fixed id layout — best for the *data* tier (a user's rows).
/// - [`HashLocator`]: `hash(key) % n`. Even spread without an id layout — useful
///   when a *directory* table itself outgrows one database and must partition
///   (the directory caps at tens of millions of rows per database).
/// - a consumer impl can do anything (e.g. consult a capacity-aware registry).
///
/// `locate` is synchronous: in this design the directory→id resolution happens
/// as an ordinary global query in the service, and the id it returns is already
/// shard-encoded — so routing never needs an async lookup.
pub trait ShardLocator {
    fn locate(&self, key: u64) -> ShardId;
}

impl ShardLocator for ShardCodec {
    fn locate(&self, key: u64) -> ShardId {
        self.shard_of(key)
    }
}

/// Routes `hash(key) % shards` — an even spread over a fixed shard count, for
/// when there's no shard-encoded id to decode (e.g. partitioning a directory by
/// a hashed key). Resharding still needs a rebalance plan; this is not a
/// consistent hash.
#[derive(Clone, Copy, Debug)]
pub struct HashLocator {
    pub shards: u32,
}

impl HashLocator {
    pub const fn new(shards: u32) -> Self {
        Self { shards }
    }
}

impl ShardLocator for HashLocator {
    fn locate(&self, key: u64) -> ShardId {
        (mix64(key) % self.shards.max(1) as u64) as u32
    }
}

/// splitmix64 finalizer — scrambles sequential keys before a modulo.
fn mix64(key: u64) -> u64 {
    let mut x = key;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Fixed hash **slots** with a slot → shard map — the rebalanceable directory
/// strategy (Redis-Cluster style). A key's slot (`mix(key) % slots`) is
/// permanent; moving load is a matter of editing the small `map` (which slot
/// goes to which physical shard), so no key is ever rehashed. The map is
/// `slots` entries long; several slots typically point at the same shard.
#[derive(Clone, Debug)]
pub struct SlotLocator {
    slots: u32,
    map: Vec<ShardId>,
}

impl SlotLocator {
    /// `map[i]` is the shard for slot `i`; its length is the slot count.
    pub fn new(map: Vec<ShardId>) -> Self {
        Self { slots: map.len().max(1) as u32, map }
    }
    /// All slots on one shard to start; split later by editing the map.
    pub fn single(slots: u32, shard: ShardId) -> Self {
        Self::new(vec![shard; slots.max(1) as usize])
    }
    pub fn slot_of(&self, key: u64) -> u32 {
        (mix64(key) % self.slots as u64) as u32
    }
    pub fn map_mut(&mut self) -> &mut Vec<ShardId> {
        &mut self.map
    }
}

impl ShardLocator for SlotLocator {
    fn locate(&self, key: u64) -> ShardId {
        self.map[self.slot_of(key) as usize]
    }
}

/// Provides the physical executors a [`ShardedExecutor`] routes among: one
/// **global** (directory / reference) database plus the shards.
pub trait ShardCatalog {
    type Exec: Executor;
    /// The global database — holds non-sharded entities and the directory.
    fn global(&self) -> &Self::Exec;
    /// The executor for shard `id`, if present.
    fn shard(&self, id: ShardId) -> Option<&Self::Exec>;
    /// All shard ids (for migrations / fan-out).
    fn shard_ids(&self) -> Vec<ShardId>;
}

/// An [`Executor`] that routes by shard. `Global` routes (and the unrouted
/// methods) go to [`ShardCatalog::global`]; `Key(id)` routes decode the shard
/// from `id` and go to that shard.
#[derive(Clone)]
pub struct ShardedExecutor<C, L = ShardCodec> {
    catalog: C,
    locator: L,
    dialect: Dialect,
    family: String,
}

impl<C: ShardCatalog, L: ShardLocator> ShardedExecutor<C, L> {
    /// Build from a catalog, a routing [`ShardLocator`] (e.g. a [`ShardCodec`]),
    /// and the **shard family name** this connector serves (e.g. `"users"`).
    /// The family is reported by [`Executor::shard_family`] so callers can do
    /// sharded reasoning, and it should match the
    /// [`shard_columns`](crate::Entity::shard_columns) of the entities
    /// routed through it. All databases must speak the same dialect (D1/SQLite);
    /// it's read from the global executor.
    pub fn new(catalog: C, locator: L, family: impl Into<String>) -> Self {
        let dialect = catalog.global().dialect();
        Self { catalog, locator, dialect, family: family.into() }
    }

    pub fn catalog(&self) -> &C {
        &self.catalog
    }

    pub fn locator(&self) -> &L {
        &self.locator
    }

    /// This family's deterministic database name for `shard` — e.g. with family
    /// `"users"`, shard 1 → `"users_shard_0001"`. See [`shard_db_name`].
    pub fn shard_name(&self, shard: ShardId) -> String {
        shard_db_name(&self.family, shard)
    }

    fn pick(&self, route: ShardRoute) -> Result<&C::Exec> {
        match route {
            ShardRoute::Global => Ok(self.catalog.global()),
            ShardRoute::Key(id) => {
                let s = self.locator.locate(id);
                self.catalog
                    .shard(s)
                    .ok_or_else(|| Error::msg(format!("no executor for shard {s} (id {id})")))
            }
        }
    }
}

impl<C: ShardCatalog, L: ShardLocator> Executor for ShardedExecutor<C, L> {
    fn dialect(&self) -> Dialect {
        self.dialect
    }

    fn shard_family(&self) -> Option<&str> {
        Some(&self.family)
    }

    // Unrouted calls (ad-hoc SQL with no entity context) go to the global DB.
    async fn fetch_all(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
        self.catalog.global().fetch_all(sql, params).await
    }
    async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
        self.catalog.global().execute(sql, params).await
    }

    // Routed calls dispatch to the shard the repo layer picked.
    async fn fetch_all_routed(
        &self,
        route: ShardRoute,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Vec<Box<dyn Row>>> {
        self.pick(route)?.fetch_all(sql, params).await
    }
    async fn execute_routed(
        &self,
        route: ShardRoute,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<ExecOutcome> {
        self.pick(route)?.execute(sql, params).await
    }
}

/// A simple in-memory [`ShardCatalog`]: a global executor + `(id, executor)`
/// pairs. Convenient for wiring and tests; a consumer can implement the trait
/// directly to source executors however it likes (e.g. one per D1 binding).
#[derive(Clone)]
pub struct MapCatalog<X> {
    global: X,
    shards: Vec<(ShardId, X)>,
}

impl<X: Executor> MapCatalog<X> {
    pub fn new(global: X) -> Self {
        Self { global, shards: Vec::new() }
    }
    pub fn with_shard(mut self, id: ShardId, exec: X) -> Self {
        self.shards.push((id, exec));
        self
    }
}

impl<X: Executor> ShardCatalog for MapCatalog<X> {
    type Exec = X;
    fn global(&self) -> &X {
        &self.global
    }
    fn shard(&self, id: ShardId) -> Option<&X> {
        self.shards.iter().find(|(i, _)| *i == id).map(|(_, e)| e)
    }
    fn shard_ids(&self) -> Vec<ShardId> {
        self.shards.iter().map(|(i, _)| *i).collect()
    }
}
