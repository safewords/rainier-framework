//! Cluster-safe Lua scripts, and the hash slot that decides whether one is.
//!
//! A Redis Cluster runs a script on one node, so every key it touches has to
//! live in one **hash slot** — and every key it touches has to be *declared*,
//! in `KEYS`, for the client to route it there. Get either wrong and the
//! command fails with `CROSSSLOT`, or reads and writes keys on a node that
//! does not own them.
//!
//! Neither mistake shows on a single server, which is where tests run. A
//! script that spans two slots passes every test and fails on the first
//! production request. [`RedisScript`] closes that gap: it computes each
//! declared key's slot on the client, with the same function the cluster uses
//! ([`hash_slot`]), and refuses a call whose keys disagree — on any server,
//! so the test that exercises the script is the test that catches it.
//!
//! It also sends the script by its SHA-1 (`EVALSHA`) and only falls back to
//! the source when the server has not seen it, so a script called on every
//! request is not re-sent on every request.
//!
//! What it cannot check: a key the script builds *inside* Lua instead of
//! receiving in `KEYS`. Declare every key.

use rainier_support::{Error, Result};

/// The number of hash slots in a Redis Cluster.
pub const HASH_SLOTS: u16 = 16_384;

/// The cluster hash slot of `key`: CRC16 (XMODEM) of the key, or of its hash
/// tag, modulo 16,384.
///
/// The hash tag is the part between the first `{` and the first `}` after it,
/// when that part is non-empty. Keys that share a tag share a slot, which is
/// how a group of keys is kept together on purpose: `{user:7}:cart` and
/// `{user:7}:wishlist` are always on the same node.
pub fn hash_slot(key: &str) -> u16 {
    let bytes = key.as_bytes();
    let hashed = match bytes.iter().position(|&b| b == b'{') {
        Some(open) => match bytes[open + 1..].iter().position(|&b| b == b'}') {
            Some(len) if len > 0 => &bytes[open + 1..open + 1 + len],
            _ => bytes,
        },
        None => bytes,
    };
    crc16_xmodem(hashed) % HASH_SLOTS
}

/// CRC16-CCITT (XMODEM): polynomial 0x1021, initial value 0 — the variant the
/// Redis Cluster specification names.
fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Whether every key in `keys` hashes to the same slot. No keys is one slot.
///
/// # Errors
///
/// An internal error — this is a mistake in the code, not in a request —
/// naming the first two keys that disagree and their slots, so the message
/// says which hash tag is missing.
pub fn check_one_slot<K: AsRef<str>>(keys: &[K]) -> Result<()> {
    let Some(first) = keys.first() else { return Ok(()) };
    let slot = hash_slot(first.as_ref());
    if let Some(other) = keys.iter().find(|k| hash_slot(k.as_ref()) != slot) {
        return Err(Error::internal(format!(
            "the keys `{}` (slot {slot}) and `{}` (slot {}) are in different hash slots; a \
                 cluster runs a script on one node, so give every key it touches a shared \
                 hash tag, `{{…}}`",
            first.as_ref(),
            other.as_ref(),
            hash_slot(other.as_ref())
        )));
    }
    Ok(())
}

/// A Lua script for Redis or Valkey, checked for cluster safety on every call.
///
/// Build it once — a `static` or a field — and call it with
/// [`RedisConnection::eval`](crate::RedisConnection::eval) or
/// [`SharedRedis::eval`](crate::SharedRedis::eval). See the module docs.
///
/// ```ignore
/// static TOUCH: LazyLock<RedisScript> = LazyLock::new(|| RedisScript::new(
///     "redis.call('HINCRBY', KEYS[1], ARGV[1], 1) return redis.call('EXPIRE', KEYS[1], ARGV[2])",
/// ));
/// let _: i64 = connection.eval(&TOUCH, &["{user:7}:counts"], &["views", "3600"]).await?;
/// ```
#[derive(Clone, Debug)]
pub struct RedisScript {
    pub(crate) script: redis::Script,
}

impl RedisScript {
    /// A script from its Lua source.
    pub fn new(source: &str) -> Self {
        Self { script: redis::Script::new(source) }
    }

    /// The script's SHA-1, as `EVALSHA` and `SCRIPT EXISTS` name it.
    pub fn hash(&self) -> &str {
        self.script.get_hash()
    }

    /// An invocation with `keys` and `args`, after the slot check.
    pub(crate) fn invocation<K, A>(
        &self,
        keys: &[K],
        args: &[A],
    ) -> Result<redis::ScriptInvocation<'_>>
    where
        K: AsRef<str>,
        A: redis::ToRedisArgs,
    {
        check_one_slot(keys)?;
        let mut invocation = self.script.prepare_invoke();
        for key in keys {
            invocation.key(key.as_ref());
        }
        for arg in args {
            invocation.arg(arg);
        }
        Ok(invocation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_match_the_cluster_specification() {
        // The specification's own CRC16 check value, and keys whose slots
        // `CLUSTER KEYSLOT` answers on any cluster.
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
        assert_eq!(hash_slot("foo"), 12_182);
        assert_eq!(hash_slot("bar"), 5_061);
        assert_eq!(hash_slot(""), 0);
    }

    #[test]
    fn a_hash_tag_decides_the_slot() {
        assert_eq!(hash_slot("{user1000}.following"), hash_slot("{user1000}.followers"));
        assert_eq!(hash_slot("{user1000}.following"), hash_slot("user1000"));
        // The first `{` and the first `}` after it.
        assert_eq!(hash_slot("foo{bar}{zap}"), hash_slot("bar"));
        // An empty tag is no tag: the whole key is hashed.
        assert_eq!(hash_slot("{}foo"), crc16_xmodem(b"{}foo") % HASH_SLOTS);
        // An unclosed tag is no tag.
        assert_eq!(hash_slot("{foo"), crc16_xmodem(b"{foo") % HASH_SLOTS);
    }

    #[test]
    fn keys_in_one_slot_pass_and_keys_across_two_are_refused() {
        assert!(check_one_slot::<&str>(&[]).is_ok());
        assert!(check_one_slot(&["{p:7}:a", "{p:7}:b", "{p:7}:c"]).is_ok());

        let refused = check_one_slot(&["{p:7}:a", "{p:8}:b"]).unwrap_err();
        assert_eq!(refused.kind(), rainier_support::ErrorKind::Internal);
        assert!(refused.message().contains("{p:8}:b"), "{}", refused.message());
    }

    #[test]
    fn an_invocation_is_refused_before_anything_is_sent() {
        let script = RedisScript::new("return 1");
        assert!(script.invocation(&["{a}:1", "{b}:1"], &["x"]).is_err());
        assert!(script.invocation(&["{a}:1", "{a}:2"], &["x"]).is_ok());
        assert_eq!(script.hash().len(), 40);
    }
}
