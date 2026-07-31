//! SQL executors — the database transports.
//!
//! [`rainier-orm`](rainier_orm) defines the `Executor` port and implements none
//! of it. Every driver is here, alongside Redis, Memcached and the AWS clients,
//! for the reason the [crate docs](crate) give: **a driver never names a port
//! trait's owner's semantics, and every connector lives in one place**.
//!
//! It also keeps the ORM core **wasm-safe with no feature dance**. With the
//! executors out, `rainier-orm` has no optional dependencies at all, so it
//! compiles for `wasm32` unconditionally and a Cloudflare Worker takes the same
//! crate a server does.
//!
//! ## Native first
//!
//! | Executor | Feature | For |
//! |---|---|---|
//! | [`SeaOrmExecutor`] | `sea-orm-executor` | MySQL, Postgres, SQLite — **prefer this** |
//! | [`D1Executor`] | `d1-http` | Cloudflare D1 over HTTP |
//! | [`LibSqlExecutor`] | `libsql-http` | libSQL / Turso over Hrana |
//!
//! Prefer a native driver wherever the runtime allows it: sea-orm is faster,
//! supports real transactions, and pools locally. The HTTP executors exist for
//! one reason — a `wasm32` runtime has no sockets, so a `fetch`-based transport
//! is the *only* way to reach a database there.
//!
//! That preference is enforced rather than merely advised: enabling
//! `sea-orm-executor` while targeting `wasm32` is a `compile_error!`, because
//! the alternative is a deep and unreadable `sqlx` build failure.

// The wasm contract, enforced. `sea-orm-executor` pulls in `sqlx`/`tokio`, which
// have no wasm32 target, so enabling it while targeting wasm is a configuration
// error worth rejecting up front with a message that says what to do instead.
#[cfg(all(feature = "sea-orm-executor", target_arch = "wasm32"))]
compile_error!(
    "feature `sea-orm-executor` is native-only (sqlx/tokio) and cannot target wasm32; \
     in a Worker enable `d1-http` or `libsql-http` instead"
);

#[cfg(feature = "d1-http")]
pub mod d1;
#[cfg(feature = "libsql-http")]
pub mod libsql;
#[cfg(feature = "sea-orm-executor")]
mod sea_orm_executor;

#[cfg(feature = "d1-http")]
pub use d1::{D1Executor, D1Response, D1Row, D1Transport};
#[cfg(feature = "libsql-http")]
pub use libsql::{LibSqlExecutor, LibSqlResult, LibSqlRow, LibSqlTransport};
#[cfg(feature = "sea-orm-executor")]
pub use sea_orm_executor::SeaOrmExecutor;
