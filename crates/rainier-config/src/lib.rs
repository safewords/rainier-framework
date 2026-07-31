//! # rainier-config
//!
//! Configuration for the framework: a dotted-path [`Config`] repository,
//! [`Key`] for naming a place in it without a magic string, and [`Env`] for
//! reading `.env` files without mutating the process environment.
//!
//! The split matters. `Env` is the *source* — raw strings from the deployment
//! — and `Config` is the *shape* — a typed tree the rest of the framework
//! reads. Components depend on `Config` alone, so a deployment can populate it
//! from a `.env`, a JSON file, a secrets manager, or a literal in code without
//! anything downstream noticing.
//!
//! ```
//! use rainier_config::{Config, Env};
//! use serde_json::json;
//!
//! let env = Env::parse("APP_NAME=Rainier\nDB_PORT=3307");
//!
//! let config = Config::new();
//! config.set("app.name", env.string("APP_NAME", "Rainier")).unwrap();
//! config.set("database.connections.mysql", json!({
//!     "host": env.string("DB_HOST", "127.0.0.1"),
//!     "port": env.int("DB_PORT", 3306),
//! })).unwrap();
//!
//! assert_eq!(config.int("database.connections.mysql.port"), Some(3307));
//! ```
//!
//! ## Two kinds of string, both worth removing
//!
//! The snippet above has magic strings on both sides of every call, and the
//! compiler checks neither. Misspell a **path** and the write lands where
//! nothing reads; misspell a **value** — `CACHE_DRIVER=redys` — and the
//! application boots on a driver nobody chose.
//!
//! | | Stringly | Typed |
//! |---|---|---|
//! | the path | `"cache.driver"` | [`Key<CacheDriver>`](Key), via [`config_keys!`] |
//! | the value | `"redis"` | a `CacheDriver` enum, via [`setting_enum!`](rainier_support::setting_enum) |
//!
//! ```
//! use rainier_config::{config_keys, Config};
//! use rainier_support::setting_enum;
//!
//! setting_enum! {
//!     /// Where cached values live.
//!     pub enum CacheDriver: "cache driver" {
//!         #[default]
//!         Memory = "memory",
//!         Redis = "redis",
//!     }
//! }
//!
//! config_keys! {
//!     /// Which cache store to build.
//!     pub CACHE_DRIVER: CacheDriver = "cache.driver";
//! }
//!
//! let config = Config::new();
//! config.set(CACHE_DRIVER, CacheDriver::Redis).unwrap();
//!
//! // Exhaustive: adding a driver makes every match on it a compile error,
//! // which is exactly the list of places that need to learn about it.
//! match config.setting(CACHE_DRIVER).unwrap() {
//!     CacheDriver::Memory => { /* … */ }
//!     CacheDriver::Redis => { /* … */ }
//! }
//! ```
//!
//! Both are opt-in. A `&str` path still works everywhere, because a path built
//! at runtime cannot be a `Key` — see [the key module](key) for where the line
//! falls.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod app_env;
pub mod env;
pub mod key;
pub mod repository;

pub use app_env::AppEnv;
pub use env::Env;
pub use key::{ConfigKey, ConfigPath, Key};
pub use repository::Config;

// So `use rainier_config::*` brings the trait's methods into scope alongside
// the types, and a downstream crate declaring a setting needs one import.
pub use rainier_support::{setting_enum, Setting};
