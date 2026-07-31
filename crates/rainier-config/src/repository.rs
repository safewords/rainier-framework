//! The configuration repository — [`Config`].
//!
//! A dotted path — `database.connections.mysql.host` — reads a nested
//! configuration tree. The tree is a `serde_json::Value`, with the values
//! deserialised into whatever Rust type the caller asks for:
//!
//! ```
//! # use rainier_config::Config;
//! let config = Config::new();
//! config.set("database.connections.mysql.port", 3306).unwrap();
//!
//! assert_eq!(config.get::<u16>("database.connections.mysql.port"), Some(3306));
//! assert_eq!(config.get_or("database.connections.mysql.host", "127.0.0.1".to_string()), "127.0.0.1");
//! ```
//!
//! Every method here takes a [`ConfigKey`], so the same call reads better with
//! a [typed key](crate::key) — the type comes from the key rather than a
//! turbofish, and a wrong one stops compiling:
//!
//! ```
//! # use rainier_config::{config_keys, Config};
//! config_keys! {
//!     pub MYSQL_PORT: u16 = "database.connections.mysql.port";
//! }
//!
//! let config = Config::new();
//! config.set(MYSQL_PORT, 3306).unwrap();
//! assert_eq!(config.get(MYSQL_PORT), Some(3306));
//! ```
//!
//! Config is read constantly and written almost never, so the whole tree sits
//! behind one `RwLock` rather than something finer-grained.

use std::sync::RwLock;

use rainier_support::{Error, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::key::{ConfigKey, ConfigPath};

/// A dotted-path configuration tree.
#[derive(Debug)]
pub struct Config {
    root: RwLock<Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    /// An empty configuration tree.
    pub fn new() -> Self {
        Self { root: RwLock::new(Value::Object(Map::new())) }
    }

    /// Build from an existing JSON object.
    ///
    /// Anything that is not an object becomes the empty tree — a scalar has no
    /// dotted paths to read, so accepting it would only defer the confusion.
    pub fn from_value(value: Value) -> Self {
        let root = if value.is_object() { value } else { Value::Object(Map::new()) };
        Self { root: RwLock::new(root) }
    }

    /// Build from any serialisable value, typically a config struct.
    pub fn from_serializable(value: impl Serialize) -> Result<Self> {
        Ok(Self::from_value(serde_json::to_value(value)?))
    }

    /// Read `key` (a dotted path) and deserialise it into `T`.
    ///
    /// `None` if the path is absent *or* the value does not deserialise into
    /// `T`. Use [`require`](Self::require) when the difference matters.
    ///
    /// With a [`Key<T>`](crate::Key) the type is inferred; with a `&str` it
    /// needs a turbofish or an annotated binding.
    pub fn get<T: DeserializeOwned>(&self, key: impl ConfigKey<T>) -> Option<T> {
        let root = self.root.read().expect("config lock poisoned");
        let value = lookup(&root, key.path())?;
        serde_json::from_value(value.clone()).ok()
    }

    /// Read `key`, or return `default` if it is missing or the wrong shape.
    pub fn get_or<T: DeserializeOwned>(&self, key: impl ConfigKey<T>, default: T) -> T {
        self.get(key).unwrap_or(default)
    }

    /// Read `key`, failing with a message that names the path and says whether
    /// it was missing or mistyped.
    pub fn require<T: DeserializeOwned>(&self, key: impl ConfigKey<T>) -> Result<T> {
        let key = key.path();
        let root = self.root.read().expect("config lock poisoned");
        let value = lookup(&root, key)
            .ok_or_else(|| Error::internal(format!("missing configuration key `{key}`")))?;
        serde_json::from_value(value.clone()).map_err(|e| {
            Error::internal(format!(
                "configuration key `{key}` has the wrong type: {e} (found {})",
                type_of(value)
            ))
        })
    }

    /// The raw JSON at `key`, if present.
    pub fn value(&self, key: impl ConfigPath) -> Option<Value> {
        let root = self.root.read().expect("config lock poisoned");
        lookup(&root, key.path()).cloned()
    }

    /// Whether anything is set at `key`.
    pub fn has(&self, key: impl ConfigPath) -> bool {
        let root = self.root.read().expect("config lock poisoned");
        lookup(&root, key.path()).is_some()
    }

    /// Write `value` at `key`, creating intermediate objects as needed.
    ///
    /// An intermediate segment that currently holds a scalar is replaced by an
    /// object — setting `a.b` after `a = 1` makes `a` an object, because there
    /// is no other way to honour the write.
    pub fn set<T: Serialize>(&self, key: impl ConfigKey<T>, value: T) -> Result<()> {
        let key = key.path();
        let value = serde_json::to_value(value)?;
        let mut root = self.root.write().expect("config lock poisoned");

        let mut cursor = &mut *root;
        let segments: Vec<&str> = key.split('.').collect();
        let Some((last, parents)) = segments.split_last() else {
            return Err(Error::internal("configuration key must not be empty"));
        };

        for segment in parents {
            if !cursor.is_object() {
                *cursor = Value::Object(Map::new());
            }
            cursor = cursor
                .as_object_mut()
                .expect("just ensured object")
                .entry(segment.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
        }

        if !cursor.is_object() {
            *cursor = Value::Object(Map::new());
        }
        cursor.as_object_mut().expect("just ensured object").insert(last.to_string(), value);
        Ok(())
    }

    /// Set `key` only if nothing is there yet. Returns whether it wrote.
    pub fn set_default<T: Serialize>(&self, key: impl ConfigKey<T>, value: T) -> Result<bool> {
        if self.has(key.path()) {
            return Ok(false);
        }
        self.set(key, value)?;
        Ok(true)
    }

    /// Deep-merge an object into the tree at `key`.
    ///
    /// Objects merge recursively; every other kind of value (including arrays)
    /// is replaced wholesale. That is the behaviour a package's default config
    /// wants: the app overrides individual keys without having to restate the
    /// whole block, but an array it sets is the array, not an append.
    ///
    /// Takes a [`ConfigPath`] rather than a typed key, because a merge writes a
    /// *fragment* — `{ "connections": { "mysql": { "port": 3307 } } }` is not a
    /// whole `DatabaseConfig`, and demanding that it were would defeat the
    /// point.
    pub fn merge(&self, key: impl ConfigPath, value: impl Serialize) -> Result<()> {
        let key = key.path();
        let incoming = serde_json::to_value(value)?;
        let existing = self.value(key);

        let merged = match existing {
            Some(existing) => deep_merge(existing, incoming),
            None => incoming,
        };
        self.set(key, merged)
    }

    /// Remove `key`. Returns the value it held, if any.
    pub fn forget(&self, key: impl ConfigPath) -> Option<Value> {
        let key = key.path();
        let mut root = self.root.write().expect("config lock poisoned");
        let segments: Vec<&str> = key.split('.').collect();
        let (last, parents) = segments.split_last()?;

        let mut cursor = &mut *root;
        for segment in parents {
            cursor = cursor.as_object_mut()?.get_mut(*segment)?;
        }
        cursor.as_object_mut()?.remove(*last)
    }

    /// A snapshot of the whole tree.
    pub fn all(&self) -> Value {
        self.root.read().expect("config lock poisoned").clone()
    }

    // --- typed conveniences ------------------------------------------------
    //
    // For the untyped path: `config.string("app.name")` rather than
    // `config.get::<String>("app.name")`. A typed key names its own type, so
    // these accept only one that agrees — `config.int(SERVER_PORT)` where
    // `SERVER_PORT: Key<u16>` does not compile, and `config.get(SERVER_PORT)`
    // is what you wanted.

    /// `key` as a string.
    pub fn string(&self, key: impl ConfigKey<String>) -> Option<String> {
        self.get(key)
    }

    /// `key` as an integer.
    pub fn int(&self, key: impl ConfigKey<i64>) -> Option<i64> {
        self.get(key)
    }

    /// `key` as a bool.
    pub fn bool(&self, key: impl ConfigKey<bool>) -> Option<bool> {
        self.get(key)
    }

    /// `key` as a float.
    pub fn float(&self, key: impl ConfigKey<f64>) -> Option<f64> {
        self.get(key)
    }

    /// `key` as a [closed-set setting](rainier_support::Setting), failing with
    /// a message that lists the valid values.
    ///
    /// The difference from `get` is the error. `get` answers `None` for both
    /// "unset" and "set to nonsense", and a driver set to nonsense should not
    /// quietly become the default:
    ///
    /// ```
    /// # use rainier_config::Config;
    /// # use rainier_support::setting_enum;
    /// setting_enum! {
    ///     pub enum CacheDriver: "cache driver" {
    ///         #[default]
    ///         Memory = "memory",
    ///         Redis = "redis",
    ///     }
    /// }
    ///
    /// let config = Config::new();
    /// config.set("cache.driver", "redys").unwrap();
    ///
    /// let err = config.setting::<CacheDriver>("cache.driver").unwrap_err();
    /// assert!(err.message().contains("expected one of `memory`, `redis`"));
    /// ```
    ///
    /// An absent key gives the setting's own `Default`, because *unset* is the
    /// case a default is for.
    pub fn setting<T>(&self, key: impl ConfigKey<T>) -> Result<T>
    where
        T: rainier_support::Setting + Default + DeserializeOwned,
    {
        let key = key.path();
        let Some(value) = self.value(key) else {
            return Ok(T::default());
        };

        match &value {
            Value::String(raw) => {
                T::parse(raw).map_err(|e| Error::internal(format!("`{key}`: {}", e.message())))
            }
            other => Err(Error::internal(format!(
                "configuration key `{key}` should be a string naming a {}, but it is {}",
                T::SETTING,
                type_of(other)
            ))),
        }
    }
}

/// Walk a dotted path. Array indices are supported numerically, so
/// `servers.0.host` reads the first element.
fn lookup<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    let mut cursor = root;
    for segment in key.split('.') {
        cursor = match cursor {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// Recursively merge `incoming` over `base`, object keys only.
fn deep_merge(base: Value, incoming: Value) -> Value {
    match (base, incoming) {
        (Value::Object(mut base), Value::Object(incoming)) => {
            for (key, value) in incoming {
                let merged = match base.remove(&key) {
                    Some(existing) => deep_merge(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, incoming) => incoming,
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Config {
        Config::from_value(json!({
            "app": { "name": "Rainier", "debug": true },
            "database": {
                "default": "mysql",
                "connections": { "mysql": { "host": "127.0.0.1", "port": 3306 } }
            },
            "servers": [{ "host": "a" }, { "host": "b" }]
        }))
    }

    #[test]
    fn reads_dotted_paths() {
        let config = sample();
        assert_eq!(config.string("app.name").as_deref(), Some("Rainier"));
        assert_eq!(config.int("database.connections.mysql.port"), Some(3306));
        assert_eq!(config.bool("app.debug"), Some(true));
    }

    #[test]
    fn reads_through_array_indices() {
        let config = sample();
        assert_eq!(config.string("servers.1.host").as_deref(), Some("b"));
        assert!(config.value("servers.9.host").is_none());
    }

    #[test]
    fn missing_paths_read_as_none() {
        let config = sample();
        assert!(config.value("app.nope").is_none());
        assert!(config.value("app.name.deeper").is_none());
        assert!(!config.has("nothing.here"));
    }

    #[test]
    fn get_or_supplies_the_default() {
        let config = sample();
        assert_eq!(config.get_or("app.locale", "en".to_string()), "en");
        assert_eq!(config.get_or("app.name", "fallback".to_string()), "Rainier");
    }

    #[test]
    fn set_creates_intermediate_objects() {
        let config = Config::new();
        config.set("mail.mailers.smtp.host", "localhost").unwrap();
        assert_eq!(config.string("mail.mailers.smtp.host").as_deref(), Some("localhost"));
    }

    #[test]
    fn set_replaces_a_scalar_standing_where_an_object_is_needed() {
        let config = Config::new();
        config.set("a", 1).unwrap();
        config.set("a.b", 2).unwrap();
        assert_eq!(config.int("a.b"), Some(2));
    }

    #[test]
    fn set_default_does_not_clobber() {
        let config = sample();
        assert!(!config.set_default("app.name", "Other").unwrap());
        assert_eq!(config.string("app.name").as_deref(), Some("Rainier"));
        assert!(config.set_default("app.locale", "en").unwrap());
        assert_eq!(config.string("app.locale").as_deref(), Some("en"));
    }

    #[test]
    fn merge_is_deep_for_objects_and_replacing_for_everything_else() {
        let config = sample();
        config.merge("database", json!({ "connections": { "mysql": { "port": 3307 } } })).unwrap();

        // The sibling key survived the merge.
        assert_eq!(config.string("database.connections.mysql.host").as_deref(), Some("127.0.0.1"));
        assert_eq!(config.int("database.connections.mysql.port"), Some(3307));
        assert_eq!(config.string("database.default").as_deref(), Some("mysql"));

        // Arrays replace rather than append.
        config.merge("servers", json!([{ "host": "z" }])).unwrap();
        assert_eq!(config.value("servers").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn require_distinguishes_missing_from_mistyped() {
        let config = sample();
        let missing = config.require::<String>("app.locale").unwrap_err();
        assert!(missing.message().contains("missing configuration key"), "{}", missing.message());

        let mistyped = config.require::<u16>("app.name").unwrap_err();
        assert!(mistyped.message().contains("wrong type"), "{}", mistyped.message());
        assert!(mistyped.message().contains("a string"), "{}", mistyped.message());
    }

    #[test]
    fn forget_removes_and_returns() {
        let config = sample();
        assert_eq!(config.forget("app.debug"), Some(json!(true)));
        assert!(!config.has("app.debug"));
        assert!(config.forget("app.debug").is_none());
    }

    #[test]
    fn deserialises_into_a_struct() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Mysql {
            host: String,
            port: u16,
        }

        let config = sample();
        let mysql: Mysql = config.require("database.connections.mysql").unwrap();
        assert_eq!(mysql, Mysql { host: "127.0.0.1".into(), port: 3306 });
    }

    #[test]
    fn a_non_object_root_becomes_empty() {
        let config = Config::from_value(json!("scalar"));
        assert_eq!(config.all(), json!({}));
    }

    // --- typed keys and settings -------------------------------------------

    rainier_support::setting_enum! {
        pub enum Driver: "cache driver" {
            #[default]
            Memory = "memory",
            Redis = "redis",
        }
    }

    crate::config_keys! {
        pub CACHE_DRIVER: Driver = "cache.driver";
        pub POSTS_PER_PAGE: u64 = "posts.per_page";
    }

    #[test]
    fn a_typed_key_needs_no_turbofish_in_either_direction() {
        let config = Config::new();

        config.set(POSTS_PER_PAGE, 15).unwrap();
        config.set(CACHE_DRIVER, Driver::Redis).unwrap();

        assert_eq!(config.get(POSTS_PER_PAGE), Some(15_u64));
        assert_eq!(config.get(CACHE_DRIVER), Some(Driver::Redis));
    }

    #[test]
    fn a_setting_writes_its_wire_spelling_so_a_dump_is_readable() {
        let config = Config::new();
        config.set(CACHE_DRIVER, Driver::Redis).unwrap();

        // Not `{"driver": "Redis"}`, and not an integer discriminant — the same
        // text a `.env` would hold.
        assert_eq!(config.all(), json!({ "cache": { "driver": "redis" } }));
    }

    #[test]
    fn a_typed_key_and_a_string_path_reach_the_same_place() {
        let config = Config::new();
        config.set(POSTS_PER_PAGE, 15).unwrap();

        assert_eq!(config.int("posts.per_page"), Some(15));
        assert!(config.has(POSTS_PER_PAGE));
        assert_eq!(config.forget(POSTS_PER_PAGE), Some(json!(15)));
    }

    #[test]
    fn an_unset_setting_reads_as_its_default() {
        // Nobody chose, so the default applies. This is the one case where
        // falling back is right.
        let config = Config::new();
        assert_eq!(config.setting(CACHE_DRIVER).unwrap(), Driver::Memory);
    }

    #[test]
    fn a_misspelled_setting_is_an_error_naming_the_key_and_the_options() {
        let config = Config::new();
        config.set("cache.driver", "redys").unwrap();

        let err = config.setting(CACHE_DRIVER).unwrap_err();
        assert!(err.message().contains("cache.driver"), "{}", err.message());
        assert!(err.message().contains("`memory`, `redis`"), "{}", err.message());
    }

    #[test]
    fn a_setting_that_is_not_a_string_says_so_rather_than_listing_options() {
        // `cache: { driver: { host: … } }` is a shape mistake, not a spelling
        // one, and "expected one of memory, redis" would send the reader
        // looking for a typo that is not there.
        let config = Config::new();
        config.set("cache.driver", json!({ "host": "localhost" })).unwrap();

        let err = config.setting(CACHE_DRIVER).unwrap_err();
        assert!(err.message().contains("should be a string"), "{}", err.message());
        assert!(err.message().contains("an object"), "{}", err.message());
    }

    #[test]
    fn get_stays_lenient_where_setting_is_strict() {
        // `get` answers None for anything it cannot read, which is why the
        // driver path uses `setting` instead: the two failures need different
        // handling and `get` cannot tell them apart.
        let config = Config::new();
        config.set("cache.driver", "redys").unwrap();

        assert_eq!(config.get(CACHE_DRIVER), None);
        assert!(config.setting(CACHE_DRIVER).is_err());
    }

    #[test]
    fn a_setting_survives_a_round_trip_through_the_tree() {
        let config = Config::new();
        for driver in [Driver::Memory, Driver::Redis] {
            config.set(CACHE_DRIVER, driver).unwrap();
            assert_eq!(config.setting(CACHE_DRIVER).unwrap(), driver);
        }
    }
}
