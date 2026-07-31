//! Shard routing — the per-query decision of *which physical database* a
//! generic operation targets.
//!
//! Routing is derived in the [`repo`](crate::repo)/[`query`](crate::query)
//! layer from an entity's shard metadata and passed to the executor as a
//! [`ShardRoute`]. Single-database executors ignore it (the default trait
//! methods), so the *same* service code runs against one database or many —
//! the consumer never threads a shard key, and switching a sharded backend for
//! a single one (e.g. D1 → MySQL) is a config change, not a code change.
//!
//! The routing key is a **shard-encoded id**: the physical shard number lives
//! in the high bits of a `u64` id ([`ShardCodec`]), so any id self-describes
//! its shard and routing needs no directory round-trip.

use sea_query::Value;

/// Where a single operation should run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShardRoute {
    /// A non-sharded (global) entity, or a single-database backend: no routing.
    Global,
    /// Route by this shard-key value — a shard-encoded id the executor decodes
    /// to a physical shard.
    Key(u64),
}

/// Packs a physical shard number into the high bits of a `u64` id, so any id
/// (primary key or a shard-encoded foreign key) self-describes its shard.
///
/// With `shard_bits = 12`, ids carry a 12-bit shard (up to 4096 shards) and a
/// 52-bit per-shard sequence — ample for app-generated ids. Auto-increment is
/// not used for sharded entities (it can't carry the shard); the app composes
/// the id with [`compose`](Self::compose).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardCodec {
    shard_bits: u32,
}

impl ShardCodec {
    /// A sensible default: 12 shard bits (up to 4096 shards) + 52 sequence bits.
    /// Surfaces that don't need a custom layout can share this so their id
    /// schemes agree (and stay portable between a single-DB and a fleet).
    pub const DEFAULT: ShardCodec = ShardCodec::new(12);

    pub const fn new(shard_bits: u32) -> Self {
        Self { shard_bits }
    }
    /// Bits available for the per-shard sequence.
    pub const fn seq_bits(&self) -> u32 {
        64 - self.shard_bits
    }
    /// The shard number encoded in `id`.
    pub const fn shard_of(&self, id: u64) -> u32 {
        (id >> self.seq_bits()) as u32
    }
    /// The per-shard local part of `id`.
    pub const fn local_of(&self, id: u64) -> u64 {
        id & ((1u64 << self.seq_bits()) - 1)
    }
    /// Compose a full id from a shard number and a per-shard local sequence.
    pub const fn compose(&self, shard: u32, local: u64) -> u64 {
        ((shard as u64) << self.seq_bits()) | (local & ((1u64 << self.seq_bits()) - 1))
    }
}

/// Stable 64-bit hash (FNV-1a) — deterministic across processes/builds, unlike
/// `std`'s randomized hasher, so a string shard key always routes the same way.
pub fn stable_hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Reduce a bound shard-key value to a `u64` routing key: a numeric id is taken
/// as-is (the high bits hold the shard under [`ShardCodec`]); a string (e.g. a
/// hashed-email directory key) is `stable_hash`ed. The locator then maps the
/// routing key to a physical shard.
pub(crate) fn routing_key(v: &Value) -> Option<u64> {
    use sea_query::Value as V;
    Some(match v {
        V::TinyInt(Some(n)) => *n as u64,
        V::SmallInt(Some(n)) => *n as u64,
        V::Int(Some(n)) => *n as u64,
        V::BigInt(Some(n)) => *n as u64,
        V::TinyUnsigned(Some(n)) => *n as u64,
        V::SmallUnsigned(Some(n)) => *n as u64,
        V::Unsigned(Some(n)) => *n as u64,
        V::BigUnsigned(Some(n)) => *n,
        V::String(Some(s)) => stable_hash(s.as_bytes()),
        V::Char(Some(c)) => stable_hash(c.encode_utf8(&mut [0u8; 4]).as_bytes()),
        _ => return None,
    })
}

/// The route for constraining `column` to `value` on entity `E`: `Key` when
/// `column` is one of `E`'s shard-encoded columns, else `Global`.
pub(crate) fn route_for<E: crate::Entity>(column: &str, value: &Value) -> ShardRoute {
    if E::shard_columns().contains(&column) {
        if let Some(k) = routing_key(value) {
            return ShardRoute::Key(k);
        }
    }
    ShardRoute::Global
}
