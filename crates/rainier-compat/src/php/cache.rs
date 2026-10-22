//! Values in a PHP application's cache — [`PhpCache`].
//!
//! ```ignore
//! // The PHP side's Redis, reached through any Rainier cache store with no
//! // prefix of its own; the PHP application's prefixes are applied here.
//! let shared = PhpCache::new(redis_store, "app_cache_");
//!
//! let rate: Option<Value> = shared.get("exchange-rate:usd").await?;
//! shared.put("feature:new-dashboard", &Value::Bool(true), Some(Duration::from_secs(600))).await?;
//!
//! // `Cache::tags(['vehicle-telemetry'])->get($key)` on the other side:
//! let reading = shared.tags(&["vehicle-telemetry"]).get(&vin).await?;
//! ```
//!
//! # What is in the store
//!
//! | The PHP side stores | As |
//! |---|---|
//! | an integer or finite float | its decimal text, **unserialized** — so `INCR` works on it |
//! | anything else | `serialize($value)` |
//!
//! under the key `{connection prefix}{cache prefix}{key}`. The connection
//! prefix is the Redis client option (empty unless configured); the cache
//! prefix is the cache store's. They simply concatenate, so pass the two
//! joined as one `prefix`.
//!
//! # Tagged entries
//!
//! A tagged entry lives at `sha1(namespace) + ":" + key`, where the
//! namespace is the tags' ids joined with `|`. Two schemes exist for the ids
//! ([`TagIds`]): Redis-backed stores use the fixed id `tag:{name}:entries`
//! (and track members in a sorted set of that name), other stores keep a
//! random id at `tag:{name}:key` and change it to flush.
//!
//! **Tagged reads and forgets only.** Writing a tagged entry on a Redis
//! store also adds it to each tag's sorted set — which is how a flush finds
//! it — and a plain key-value port cannot express that. An entry written here
//! without it would survive the next flush of its tag, silently. So tagged
//! writes stay on the PHP side until it is gone; plain keys can be written
//! from either.
//!
//! # Prefer a key of your own
//!
//! This exists so the two applications can share state *during* a migration.
//! For anything both sides will keep reading for long, a key written as plain
//! JSON by both is simpler than either speaking the other's format.

use std::sync::Arc;
use std::time::Duration;

use rainier_cache::Cache;
use rainier_support::Result;
use sha1::{Digest, Sha1};

use super::serialize::{serialize, unserialize, Value};

/// A PHP application's cache, read and written in its format.
#[derive(Clone)]
pub struct PhpCache {
    store: Arc<dyn Cache>,
    prefix: String,
}

impl PhpCache {
    /// The cache in `store` — which must apply **no** prefix of its own —
    /// under `prefix`, the PHP side's connection and cache prefixes joined.
    pub fn new(store: Arc<dyn Cache>, prefix: impl Into<String>) -> Self {
        Self { store, prefix: prefix.into() }
    }

    /// The full key `key` lives at.
    pub fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    /// The store underneath.
    pub fn store(&self) -> &Arc<dyn Cache> {
        &self.store
    }

    /// The value at `key`.
    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        self.read(&self.key(key)).await
    }

    /// The stored bytes at `key`, undecoded.
    pub async fn get_raw(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.store.get(&self.key(key)).await
    }

    /// Store `value` at `key`, as the PHP side would have.
    pub async fn put(&self, key: &str, value: &Value, ttl: Option<Duration>) -> Result<()> {
        self.store.put(&self.key(key), &encode(value), ttl).await
    }

    /// Remove `key`.
    pub async fn forget(&self, key: &str) -> Result<bool> {
        self.store.forget(&self.key(key)).await
    }

    /// The entries tagged with every one of `names`, under the Redis id
    /// scheme. See [`tags_with`](Self::tags_with) for the other.
    pub fn tags(&self, names: &[&str]) -> TaggedPhpCache {
        self.tags_with(names, TagIds::Fixed)
    }

    /// The entries tagged with `names`, under an explicit id scheme.
    pub fn tags_with(&self, names: &[&str], ids: TagIds) -> TaggedPhpCache {
        TaggedPhpCache {
            cache: self.clone(),
            names: names.iter().map(|name| name.to_string()).collect(),
            ids,
        }
    }

    async fn read(&self, full_key: &str) -> Result<Option<Value>> {
        let Some(bytes) = self.store.get(full_key).await? else {
            return Ok(None);
        };
        decode(&bytes).map(Some)
    }
}

impl std::fmt::Debug for PhpCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhpCache").field("prefix", &self.prefix).finish_non_exhaustive()
    }
}

/// How a tag's id is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagIds {
    /// `tag:{name}:entries` — fixed, what a Redis-backed store uses.
    Fixed,
    /// The value stored at `tag:{name}:key` — what other stores use, which
    /// changes whenever the tag is flushed.
    Stored,
}

/// A view of the entries carrying a set of tags. Reads and forgets only —
/// see the [module documentation](self#tagged-entries).
#[derive(Debug, Clone)]
pub struct TaggedPhpCache {
    cache: PhpCache,
    names: Vec<String>,
    ids: TagIds,
}

impl TaggedPhpCache {
    /// The key (before the cache prefix) a tagged `key` is stored under.
    ///
    /// `None` under [`TagIds::Stored`] when a tag has no id yet — which means
    /// nothing has been written with it, so there is nothing to find.
    pub async fn item_key(&self, key: &str) -> Result<Option<String>> {
        let mut ids = Vec::with_capacity(self.names.len());
        for name in &self.names {
            match self.ids {
                TagIds::Fixed => ids.push(format!("tag:{name}:entries")),
                TagIds::Stored => {
                    let id = self.cache.get(&format!("tag:{name}:key")).await?;
                    match id.as_ref().and_then(scalar_text) {
                        Some(id) => ids.push(id),
                        None => return Ok(None),
                    }
                }
            }
        }
        Ok(Some(tagged_key(&ids.join("|"), key)))
    }

    /// The value tagged entry `key` holds.
    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        match self.item_key(key).await? {
            Some(item) => self.cache.get(&item).await,
            None => Ok(None),
        }
    }

    /// Remove tagged entry `key`. Its membership in the tag's set is left for
    /// the PHP side's own stale-entry sweep, which is what a forget there
    /// does too.
    pub async fn forget(&self, key: &str) -> Result<bool> {
        match self.item_key(key).await? {
            Some(item) => self.cache.forget(&item).await,
            None => Ok(false),
        }
    }
}

/// `sha1(namespace) + ":" + key`.
pub fn tagged_key(namespace: &str, key: &str) -> String {
    let digest = Sha1::digest(namespace.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{hex}:{key}")
}

/// What the PHP side would store for `value`.
pub fn encode(value: &Value) -> Vec<u8> {
    match value {
        // Numbers raw, so `INCRBY` and `INCRBYFLOAT` work on them.
        Value::Int(n) => n.to_string().into_bytes(),
        // A float that prints as an integer (`2.0` → `2`) reads back as one
        // on both sides, as it does there.
        Value::Float(f) if f.is_finite() => f.to_string().into_bytes(),
        other => serialize(other),
    }
}

/// What a stored value means.
pub fn decode(bytes: &[u8]) -> Result<Value> {
    if let Some(number) = numeric(bytes) {
        return Ok(number);
    }
    unserialize(bytes)
}

/// The value of a raw numeric entry, if this is one.
fn numeric(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    // A serialized value always starts with a type letter and a colon, so
    // nothing serialized is mistaken for a number here.
    if let Ok(n) = text.parse::<i64>() {
        return Some(Value::Int(n));
    }
    let plausible =
        text.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'));
    if plausible {
        if let Ok(f) = text.parse::<f64>() {
            return Some(Value::Float(f));
        }
    }
    None
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(_) => value.as_str().map(str::to_string),
        Value::Int(n) => Some(n.to_string()),
        Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_cache::MemoryCache;

    fn shared() -> (PhpCache, Arc<MemoryCache>) {
        let store = Arc::new(MemoryCache::new());
        (PhpCache::new(Arc::clone(&store) as _, "app_cache_"), store)
    }

    #[tokio::test]
    async fn numbers_are_stored_raw_and_everything_else_serialized() {
        let (cache, store) = shared();

        cache.put("count", &Value::Int(41), None).await.unwrap();
        cache.put("ratio", &Value::Float(0.25), None).await.unwrap();
        cache.put("name", &Value::string("Ada"), None).await.unwrap();

        assert_eq!(store.get("app_cache_count").await.unwrap().unwrap(), b"41");
        assert_eq!(store.get("app_cache_ratio").await.unwrap().unwrap(), b"0.25");
        assert_eq!(store.get("app_cache_name").await.unwrap().unwrap(), b"s:3:\"Ada\";");

        assert_eq!(cache.get("count").await.unwrap(), Some(Value::Int(41)));
        assert_eq!(cache.get("ratio").await.unwrap(), Some(Value::Float(0.25)));
        assert_eq!(cache.get("name").await.unwrap(), Some(Value::string("Ada")));
        assert_eq!(cache.get("missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_entry_the_php_side_wrote_reads() {
        let (cache, store) = shared();
        store.put("app_cache_settings", br#"a:1:{s:5:"theme";s:4:"dark";}"#, None).await.unwrap();

        let value = cache.get("settings").await.unwrap().unwrap();
        assert_eq!(value.get("theme").and_then(Value::as_str), Some("dark"));
    }

    #[tokio::test]
    async fn infinity_is_not_stored_raw_because_it_is_not_numeric_there() {
        let (cache, store) = shared();
        cache.put("big", &Value::Float(f64::INFINITY), None).await.unwrap();
        assert_eq!(store.get("app_cache_big").await.unwrap().unwrap(), b"d:INF;");
        assert_eq!(cache.get("big").await.unwrap(), Some(Value::Float(f64::INFINITY)));
    }

    #[test]
    fn a_tagged_key_is_the_sha1_of_the_joined_ids() {
        // sha1("tag:a:entries|tag:b:entries"), computed independently.
        let key = tagged_key("tag:a:entries|tag:b:entries", "item");
        assert_eq!(key.len(), 40 + 1 + 4);
        assert!(key.ends_with(":item"));

        // Known SHA-1 vector: sha1("abc").
        assert_eq!(&tagged_key("abc", "k")[..40], "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[tokio::test]
    async fn a_tagged_entry_reads_under_the_fixed_ids() {
        let (cache, store) = shared();
        let item = tagged_key("tag:vehicle-telemetry:entries", "5YJ3E1EA7KF000000");
        store.put(&format!("app_cache_{item}"), b"a:1:{s:3:\"soc\";i:80;}", None).await.unwrap();

        let tagged = cache.tags(&["vehicle-telemetry"]);
        let value = tagged.get("5YJ3E1EA7KF000000").await.unwrap().unwrap();
        assert_eq!(value.get("soc").and_then(Value::as_int), Some(80));

        assert!(tagged.forget("5YJ3E1EA7KF000000").await.unwrap());
        assert_eq!(tagged.get("5YJ3E1EA7KF000000").await.unwrap(), None);
    }

    #[tokio::test]
    async fn stored_tag_ids_are_looked_up_and_a_missing_one_finds_nothing() {
        let (cache, store) = shared();
        let tagged = cache.tags_with(&["a", "b"], TagIds::Stored);
        assert_eq!(tagged.get("k").await.unwrap(), None, "no id yet, nothing to find");

        store.put("app_cache_tag:a:key", b"s:5:\"id-a1\";", None).await.unwrap();
        store.put("app_cache_tag:b:key", b"s:5:\"id-b1\";", None).await.unwrap();
        let item = tagged_key("id-a1|id-b1", "k");
        store.put(&format!("app_cache_{item}"), b"b:1;", None).await.unwrap();

        assert_eq!(tagged.get("k").await.unwrap(), Some(Value::Bool(true)));
    }

    #[test]
    fn nothing_serialized_is_mistaken_for_a_number() {
        for serialized in [&b"i:5;"[..], b"s:1:\"5\";", b"d:1.5;", b"b:1;", b"N;"] {
            assert!(numeric(serialized).is_none(), "{}", String::from_utf8_lossy(serialized));
        }
        assert_eq!(numeric(b"-12"), Some(Value::Int(-12)));
        assert_eq!(numeric(b"1.5E+3"), Some(Value::Float(1500.0)));
        assert_eq!(numeric(b"inf"), None);
    }
}
