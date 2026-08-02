//! # rainier_orm
//!
//! A dynamic, multi-dialect DBAL/ORM. You define a plain struct, derive
//! [`Entity`] on it, and run generic CRUD across **SQLite / Cloudflare D1 /
//! MySQL / Postgres** — no per-entity repositories written by hand.
//!
//! Three pieces compose it:
//! 1. **`#[derive(Entity)]`** turns a struct into table + column metadata, a
//!    generated row decoder ([`Entity::from_row`]), and value projections.
//!    Column SQL types are deferred to each field type's [`SqlType`] impl and
//!    decoding to its [`FromColumn`] impl, so adding a supported type is one
//!    trait impl — not macro surgery.
//! 2. **SQL generation via `sea-query`** (re-exported), which already renders
//!    MySQL / Postgres / SQLite. Dialect translation is therefore free: the
//!    same statement is built for whatever the [`Executor`] reports.
//! 3. **A pluggable [`Executor`] trait** — one impl (`sea-orm`) covers MySQL,
//!    Postgres, and SQLite; the Cloudflare path (D1 over HTTP, D1 native
//!    binding in a Worker) implements the same trait. The core is wasm-safe
//!    so the Worker can use it; native executors are feature-gated.
//!
//! Constraints (unique columns, indexes, foreign keys) are derived too; see
//! [`schema`]. Relationships are *not* generated as navigation — a foreign key
//! is a flat id field you traverse with [`repo::find_by`], because the related
//! rows may live in a different backend (a cross-backend JOIN is impossible).
//!
//! ## The executors live elsewhere
//!
//! This crate defines the [`Executor`] port and **implements none of it**. Every
//! driver — sea-orm for native MySQL / Postgres / SQLite, Cloudflare D1 over
//! HTTP, libSQL / Turso — lives in
//! [`rainier-drivers`](https://docs.rs/rainier-drivers), alongside Redis,
//! Memcached and the AWS clients.
//!
//! That is not filing for its own sake. It is what keeps this crate
//! **wasm-safe with no feature dance**: there are no optional dependencies here
//! to accidentally enable, so the core compiles for `wasm32` unconditionally and
//! a Worker takes the same `rainier-orm` a server does.
//!
//! Prefer a **native** driver wherever the runtime allows it: sea-orm is faster,
//! supports real transactions, and pools locally. The HTTP executors exist for
//! one reason — a `wasm32` runtime has no sockets, so a `fetch`-based transport
//! is the *only* way to reach a database there.
//!
//! ```ignore
//! use rainier_drivers::sql::SeaOrmExecutor;   // native: preferred
//! use rainier_drivers::sql::D1Executor;       // wasm: HTTP transport
//! ```
//!
//! ```ignore
//! use rainier_orm::{Entity, repo};
//!
//! #[derive(Entity)]
//! #[orm(table = "users")]
//! struct User {
//!     #[orm(pk, auto_increment)]
//!     id: u64,
//!     #[orm(unique)]
//!     email: String,
//!     timezone: Option<String>,
//! }
//!
//! # async fn run(db: &impl rainier_orm::Executor) -> rainier_orm::Result<()> {
//! let id = repo::insert(db, &User { id: 0, email: "a@b.c".into(), timezone: None }).await?;
//! let u: Option<User> = repo::find_by_pk(db, id).await?;
//! let by_email: Option<User> = repo::find_one_by(db, "email", "a@b.c").await?;
//! # Ok(()) }
//! ```
//!
//! Beyond CRUD: a fluent query builder ([`repo::query`] — predicates, joins,
//! `first_or_create`, partial [`update`](query::Query::update) / atomic
//! [`increment`](query::Query::increment)), [`repo::upsert`], a keyset
//! [`Cursor`](repo::Cursor), derived constraints + DDL ([`schema`]), and column
//! types past the primitives: [`Json<T>`](Json) (serde-as-text),
//! [`Base64Bytes`] (uniform binary), and string-backed enums via
//! [`impl_string_column!`].

// Lets `#[derive(Entity)]` (which emits `::rainier_orm::…` paths) be used
// *within* this crate — e.g. the migration-tracking entity in `migrate`.
extern crate self as rainier_orm;

pub use rainier_orm_macros::Entity;
// The factory derive expands to a `rainier_database` path, so it is only
// usable by something that depends on that crate — re-exported here so one
// `use` reaches both derives.
pub use rainier_orm_macros::Factory;

// Re-export sea-query so the generated code (and callers building custom
// queries) reference one version.
pub use sea_query;

pub mod active;
pub mod blueprint;
mod bytes;
mod column;
pub mod ddl;
mod dialect;
mod entity;
mod executor;
mod json;
pub mod key;
pub mod migrate;
pub mod pool;
pub mod query;
pub mod repo;
mod route;
mod row;
pub mod schema;
pub mod shard;
pub mod sharding;
pub mod string_column;

pub use active::Tracked;
pub use blueprint::{Action, Blueprint, Column, ColumnKind, IndexDef, TableChanges};
pub use bytes::Base64Bytes;
pub use column::{ColumnSpec, ColumnType, ForeignKeySpec, IndexSpec, RefAction, SqlType};
pub use json::Json;
pub use route::{stable_hash, ShardCodec, ShardRoute};
pub use shard::{
    directory_db_name, shard_db_name, HashLocator, MapCatalog, ShardCatalog, ShardId, ShardLocator,
    ShardedExecutor, SlotLocator,
};
// The shard-encoded id allocator and the connector's sharding config are the
// most-used sharding-policy types and pair with `ShardCodec`; the rest of the
// toolkit stays under `sharding::`.
pub use dialect::Dialect;
pub use entity::Entity as EntityTrait; // avoid clashing with the derive name
pub use entity::Entity;
pub use entity::SingleKey;
pub use executor::{ExecOutcome, Executor};
pub use key::{key_condition, key_route, row_key_condition};
pub use pool::PoolConfig;
pub use query::Query;
pub use row::{FromColumn, Row, ToColumn};
pub use sharding::{IdAllocator, ShardingSettings};
pub use string_column::StringColumn;

/// The crate error type. `anyhow` keeps the surface small and wasm-safe; a
/// typed error is a later tightening.
pub type Error = anyhow::Error;
pub type Result<T> = core::result::Result<T, Error>;
