//! The connection port — [`Connection`] and [`Database`].
//!
//! Rainier ORM's `Executor` trait is deliberately **not** `dyn`-safe: it uses
//! `async fn` in trait and carries no `Send + Sync` bound, so the same generic
//! code can run inside a single-threaded Cloudflare Worker over a `!Send` D1
//! binding. That is the right call for the ORM, and it means a framework
//! cannot put an `Arc<dyn Executor>` in its container.
//!
//! [`Connection`] is Rainier's `dyn`-safe mirror of that port, and [`Database`]
//! is the shared handle the container holds.
//!
//! ## Two ways to read rows
//!
//! `Connection` exposes fetching twice, and **both futures are `Send`**:
//!
//! | | Yields | For |
//! |---|---|---|
//! | [`fetch`](Connection::fetch) | [`OwnedRow`] — a `Send` snapshot | the repository layer, which decodes later |
//! | [`fetch_raw`](Connection::fetch_raw) | the driver's own `Box<dyn Row>` | [`Database`]'s `Executor` impl, and so Rainier ORM's `repo::` |
//!
//! A future's **output** need not be `Send` for the future to be `Send` — the
//! value is moved out on the final poll rather than held across a suspension —
//! so `fetch_raw` can hand back `!Send` rows from a `Send` future. What the
//! caller must not do is hold those rows across a *further* await; decoding
//! them immediately, as `Entity::from_row` does, is the whole pattern.
//!
//! `fetch` exists on top of that because the repository decodes into an entity
//! some way after the query, and an [`OwnedRow`] can be carried there safely.
//!
//! ## `repo::` on the request path
//!
//! Rainier ORM used to hold a `sea_query` statement across its awaits, which
//! made every `repo::` future `!Send` and unusable from a handler the server
//! will `tokio::spawn`. That is fixed upstream (the statement is now built and
//! dropped in a scope that ends before the await), so
//! `repo::`/[`Query`](rainier_orm::Query) work directly against a [`Database`].
//!
//! [`statement`](crate::statement) predates the fix and still backs the
//! repository. It is no longer a workaround — it is how the repository renders
//! SQL — but the constraint that forced it is gone, and the
//! `the_orms_own_futures_are_send…` test in this crate's root asserts as much.

use std::sync::Arc;

use rainier_orm::sea_query::Value;
use rainier_orm::{Dialect, ExecOutcome, Executor, Row, ShardRoute};
use rainier_support::{BoxFuture, Error, Result};

use crate::row::{ColumnRequest, OwnedRow};
use crate::statement::Prepared;

/// A `dyn`-safe database backend.
///
/// Implemented for concrete Rainier ORM executors by
/// [`bind_executor!`](crate::bind_executor).
pub trait Connection: Send + Sync + 'static {
    /// The SQL dialect this backend speaks.
    fn dialect(&self) -> Dialect;

    /// The shard family this connector routes within, or `None` for a single
    /// database.
    fn shard_family(&self) -> Option<String> {
        None
    }

    /// Mint a shard-encoded primary key for a row owned by `shard_key`.
    fn allocate_id(&self, shard_key: u64) -> Option<u64> {
        let _ = shard_key;
        None
    }

    /// Run a query, copying the named columns out of each row.
    ///
    /// `Send`, which is what makes it usable from a request handler.
    fn fetch<'a>(
        &'a self,
        route: ShardRoute,
        sql: &'a str,
        params: Vec<Value>,
        columns: Vec<ColumnRequest>,
    ) -> BoxFuture<'a, Result<Vec<OwnedRow>>>;

    /// Run a write and report its outcome. `Send`.
    fn execute<'a>(
        &'a self,
        route: ShardRoute,
        sql: &'a str,
        params: Vec<Value>,
    ) -> BoxFuture<'a, Result<Vec<ExecOutcome>>>;

    /// Run a query and hand back the driver's own rows.
    ///
    /// The future is `Send` even though `Vec<Box<dyn Row>>` is not — a future's
    /// **output** need not be `Send` for the future to be, because the value is
    /// moved out on the final poll rather than held across a suspension. That
    /// is what lets [`Database`]'s `Executor` impl, and therefore Rainier ORM'
    /// own `repo::` API, be used from a request handler.
    ///
    /// The rows themselves still cannot cross a thread, so decode them before
    /// the next `await` — which is exactly what `Entity::from_row` does.
    fn fetch_raw<'a>(
        &'a self,
        route: ShardRoute,
        sql: &'a str,
        params: Vec<Value>,
    ) -> BoxFuture<'a, Result<Vec<Box<dyn Row>>>>;
}

/// Writes the [`Connection`] impl for a concrete Rainier ORM `Executor`.
///
/// ```ignore
/// struct MyExecutor { /* … */ }
/// impl rainier_orm::Executor for MyExecutor { /* … */ }
///
/// rainier_database::bind_executor!(MyExecutor);
/// ```
///
/// Two constraints, both worth knowing before you reach for this:
///
/// 1. **The type must be concrete.** Rust cannot prove a generic `E: Executor`
///    has `Send` futures — that needs return-type notation, still unstable —
///    whereas for a named type the auto traits leak out of the `async fn`
///    bodies and the compiler works it out.
/// 2. **The type must be yours.** [`Connection`] is defined here, so the
///    orphan rule allows the impl only in this crate or in the crate defining
///    the executor. An application therefore cannot bind an executor it did
///    not write; the ones Rainier ORM ships are bound below, behind the same
///    feature flags Rainier ORM uses for them.
#[macro_export]
macro_rules! bind_executor {
    ($executor:ty) => {
        impl $crate::Connection for $executor {
            fn dialect(&self) -> $crate::reexport::Dialect {
                $crate::reexport::Executor::dialect(self)
            }

            fn shard_family(&self) -> ::std::option::Option<::std::string::String> {
                $crate::reexport::Executor::shard_family(self)
                    .map(::std::string::ToString::to_string)
            }

            fn allocate_id(&self, shard_key: u64) -> ::std::option::Option<u64> {
                $crate::reexport::Executor::allocate_id(self, shard_key)
            }

            fn fetch<'a>(
                &'a self,
                route: $crate::reexport::ShardRoute,
                sql: &'a str,
                params: ::std::vec::Vec<$crate::reexport::Value>,
                columns: ::std::vec::Vec<$crate::reexport::ColumnRequest>,
            ) -> $crate::reexport::BoxFuture<
                'a,
                $crate::reexport::Result<::std::vec::Vec<$crate::reexport::OwnedRow>>,
            > {
                ::std::boxed::Box::pin(async move {
                    // The driver rows are created *after* the await and never
                    // held across one, so this future stays `Send` even though
                    // `dyn Row` is not.
                    let rows =
                        $crate::reexport::Executor::fetch_all_routed(self, route, sql, params)
                            .await
                            .map_err($crate::reexport::Error::from)?;

                    let mut owned = ::std::vec::Vec::with_capacity(rows.len());
                    for row in &rows {
                        owned.push(
                            $crate::reexport::OwnedRow::snapshot(row.as_ref(), &columns)
                                .map_err($crate::reexport::Error::from)?,
                        );
                    }
                    ::std::result::Result::Ok(owned)
                })
            }

            fn execute<'a>(
                &'a self,
                route: $crate::reexport::ShardRoute,
                sql: &'a str,
                params: ::std::vec::Vec<$crate::reexport::Value>,
            ) -> $crate::reexport::BoxFuture<
                'a,
                $crate::reexport::Result<::std::vec::Vec<$crate::reexport::ExecOutcome>>,
            > {
                ::std::boxed::Box::pin(async move {
                    let outcome =
                        $crate::reexport::Executor::execute_routed(self, route, sql, params)
                            .await
                            .map_err($crate::reexport::Error::from)?;
                    ::std::result::Result::Ok(::std::vec![outcome])
                })
            }

            fn fetch_raw<'a>(
                &'a self,
                route: $crate::reexport::ShardRoute,
                sql: &'a str,
                params: ::std::vec::Vec<$crate::reexport::Value>,
            ) -> $crate::reexport::BoxFuture<
                'a,
                $crate::reexport::Result<
                    ::std::vec::Vec<::std::boxed::Box<dyn $crate::reexport::Row>>,
                >,
            > {
                ::std::boxed::Box::pin(async move {
                    $crate::reexport::Executor::fetch_all_routed(self, route, sql, params)
                        .await
                        .map_err($crate::reexport::Error::from)
                })
            }
        }
    };
}

// The executors Rainier ORM ships, bound here because the orphan rule puts
// them out of reach of an application crate. Feature-gated to match
// Rainier ORM's own gating, so the wasm-safe default stays wasm-safe.
#[cfg(feature = "sea-orm-executor")]
crate::bind_executor!(rainier_drivers::sql::SeaOrmExecutor);

/// A database handle: a shared [`Connection`], cheap to clone.
///
/// Lives as a container singleton and is cloned into repositories, jobs and
/// console commands freely.
#[derive(Clone)]
pub struct Database {
    connection: Arc<dyn Connection>,
}

impl Database {
    /// Wrap a concrete connection.
    pub fn new(connection: impl Connection) -> Self {
        Self { connection: Arc::new(connection) }
    }

    /// Wrap an already-shared connection.
    pub fn from_arc(connection: Arc<dyn Connection>) -> Self {
        Self { connection }
    }

    /// The underlying connection.
    pub fn connection(&self) -> &Arc<dyn Connection> {
        &self.connection
    }

    /// The dialect the backend speaks.
    pub fn dialect(&self) -> Dialect {
        self.connection.dialect()
    }

    /// Whether the backend is a sharded fleet.
    pub fn is_sharded(&self) -> bool {
        self.connection.shard_family().is_some()
    }

    /// The shard family, if any.
    pub fn shard_family(&self) -> Option<String> {
        self.connection.shard_family()
    }

    // --- the Send-safe query path ------------------------------------------

    /// Run a prepared query, reading back the named columns.
    pub async fn fetch(
        &self,
        prepared: Prepared,
        columns: Vec<ColumnRequest>,
    ) -> Result<Vec<OwnedRow>> {
        self.connection.fetch(prepared.route, &prepared.sql, prepared.params, columns).await
    }

    /// Run a prepared query and decode every row into `E`.
    pub async fn fetch_all<E: rainier_orm::Entity>(&self, prepared: Prepared) -> Result<Vec<E>> {
        let rows = self.fetch(prepared, ColumnRequest::for_entity::<E>()).await?;
        rows.iter().map(|row| E::from_row(row).map_err(Error::from)).collect()
    }

    /// Run a prepared query and decode the first row into `E`.
    pub async fn fetch_one<E: rainier_orm::Entity>(&self, prepared: Prepared) -> Result<Option<E>> {
        let rows = self.fetch(prepared, ColumnRequest::for_entity::<E>()).await?;
        match rows.first() {
            Some(row) => Ok(Some(E::from_row(row).map_err(Error::from)?)),
            None => Ok(None),
        }
    }

    /// Run a prepared `COUNT(*) AS cnt` and read the total.
    pub async fn fetch_count(&self, prepared: Prepared) -> Result<u64> {
        let columns = vec![ColumnRequest::new("cnt", rainier_orm::ColumnType::BigInt)];
        let rows = self.fetch(prepared, columns).await?;

        let count = match rows.first() {
            Some(row) => row.get_i64("cnt").map_err(Error::from)?.unwrap_or(0),
            None => 0,
        };
        // A negative count is impossible, but the column is signed.
        Ok(count.max(0) as u64)
    }

    /// Run a prepared write.
    pub async fn execute(&self, prepared: Prepared) -> Result<ExecOutcome> {
        let outcomes =
            self.connection.execute(prepared.route, &prepared.sql, prepared.params).await?;
        Ok(outcomes.into_iter().next().unwrap_or_default())
    }

    /// Run a raw statement with no bindings — for DDL.
    pub async fn statement(&self, sql: &str) -> Result<ExecOutcome> {
        let outcomes = self.connection.execute(ShardRoute::Global, sql, Vec::new()).await?;
        Ok(outcomes.into_iter().next().unwrap_or_default())
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("dialect", &self.connection.dialect())
            .field("shard_family", &self.connection.shard_family())
            .finish()
    }
}

/// Implementing `Executor` is what keeps Rainier ORM's whole surface available.
///
/// `repo::insert`, `repo::query::<E>()`, `Cursor` and `Migrator::run` are all
/// generic over `X: Executor`, so they accept a `Database` directly. Their
/// futures are `!Send` (see the module docs), so use them from a console
/// command, a migration, or a Worker — not from a request handler, which needs
/// the `Send` path above.
impl Executor for Database {
    fn dialect(&self) -> Dialect {
        self.connection.dialect()
    }

    fn shard_family(&self) -> Option<&str> {
        // The `dyn`-safe port returns an owned `String` — a borrow would tie
        // the trait object's lifetime into the signature — so there is nothing
        // to hand back a reference to. `is_sharded` still answers correctly,
        // and `Database::shard_family` returns the owned name.
        None
    }

    fn is_sharded(&self) -> bool {
        self.connection.shard_family().is_some()
    }

    fn allocate_id(&self, shard_key: u64) -> Option<u64> {
        self.connection.allocate_id(shard_key)
    }

    async fn fetch_all(
        &self,
        sql: &str,
        params: Vec<Value>,
    ) -> rainier_orm::Result<Vec<Box<dyn Row>>> {
        self.connection.fetch_raw(ShardRoute::Global, sql, params).await.map_err(into_anyhow)
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> rainier_orm::Result<ExecOutcome> {
        let outcomes =
            self.connection.execute(ShardRoute::Global, sql, params).await.map_err(into_anyhow)?;
        Ok(outcomes.into_iter().next().unwrap_or_default())
    }

    async fn fetch_all_routed(
        &self,
        route: ShardRoute,
        sql: &str,
        params: Vec<Value>,
    ) -> rainier_orm::Result<Vec<Box<dyn Row>>> {
        self.connection.fetch_raw(route, sql, params).await.map_err(into_anyhow)
    }

    async fn execute_routed(
        &self,
        route: ShardRoute,
        sql: &str,
        params: Vec<Value>,
    ) -> rainier_orm::Result<ExecOutcome> {
        let outcomes = self.connection.execute(route, sql, params).await.map_err(into_anyhow)?;
        Ok(outcomes.into_iter().next().unwrap_or_default())
    }
}

/// Convert back into the ORM's error type at the boundary.
fn into_anyhow(error: Error) -> rainier_orm::Error {
    anyhow::anyhow!("{error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statement;
    use crate::testing::{fake_database, MemoryConnection};

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
        title: String,
    }

    fn row(id: u64, title: &str) -> OwnedRow {
        OwnedRow::new().with("id", id).with("title", title)
    }

    #[test]
    fn the_send_query_path_is_actually_send() {
        // The property the whole module exists to guarantee. If this stops
        // holding, every handler that touches the database breaks with a far
        // more confusing error than this one.
        fn assert_send<T: Send>(_: T) {}

        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        assert_send(async move {
            let prepared = statement::select_all::<Post>(Dialect::Sqlite);
            let _: Vec<Post> = db.fetch_all(prepared).await.unwrap();
        });
    }

    #[test]
    fn a_database_reports_its_backend() {
        let db = Database::new(MemoryConnection::new(Dialect::Postgres));
        assert_eq!(db.dialect(), Dialect::Postgres);
        assert!(!db.is_sharded());
        assert_eq!(db.shard_family(), None);
    }

    #[tokio::test]
    async fn fetch_all_decodes_entities() {
        let (db, _) = fake_database(
            MemoryConnection::new(Dialect::Sqlite).returning([row(1, "a"), row(2, "b")]),
        );

        let posts: Vec<Post> =
            db.fetch_all(statement::select_all::<Post>(Dialect::Sqlite)).await.unwrap();

        assert_eq!(posts.len(), 2);
        assert_eq!(posts[1], Post { id: 2, title: "b".into() });
    }

    #[tokio::test]
    async fn fetch_one_returns_none_for_no_rows() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let found: Option<Post> = db
            .fetch_one(statement::select_by_pk::<Post>(Dialect::Sqlite, 1_i64.into()))
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn fetch_count_reads_the_cnt_column() {
        let (db, _) = fake_database(
            MemoryConnection::new(Dialect::Sqlite).returning([OwnedRow::new().with("cnt", 42_i64)]),
        );

        let criteria = crate::Criteria::new();
        let prepared = statement::count_matching::<Post>(Dialect::Sqlite, &criteria);
        assert_eq!(db.fetch_count(prepared).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn fetch_count_is_zero_when_nothing_comes_back() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let criteria = crate::Criteria::new();
        let prepared = statement::count_matching::<Post>(Dialect::Sqlite, &criteria);
        assert_eq!(db.fetch_count(prepared).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn statements_and_bindings_reach_the_connection() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));

        db.execute(statement::delete_by_pk::<Post>(Dialect::Sqlite, 3_i64.into())).await.unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.starts_with("DELETE FROM"), "{sql}");
        assert_eq!(connection.bindings()[0].len(), 1);
    }

    #[tokio::test]
    async fn ddl_runs_through_statement() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        db.statement("CREATE TABLE posts (id INTEGER)").await.unwrap();
        assert_eq!(connection.last_statement().as_deref(), Some("CREATE TABLE posts (id INTEGER)"));
    }

    #[tokio::test]
    async fn an_error_survives_the_round_trip() {
        let (db, _) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).failing("table is locked"));

        let err = db.statement("SELECT 1").await.unwrap_err();
        assert!(err.message().contains("table is locked"), "{}", err.message());
    }

    #[tokio::test]
    async fn the_orm_surface_is_still_reachable_through_the_executor_impl() {
        // Not `Send`, but valid outside the request path — a console command,
        // a migration, or a Worker.
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));

        let outcome = Executor::execute(&db, "DELETE FROM posts", vec![]).await.unwrap();
        assert_eq!(outcome.rows_affected, 1);
        assert_eq!(connection.last_statement().as_deref(), Some("DELETE FROM posts"));
    }

    #[test]
    fn cloning_shares_one_connection() {
        let db = Database::new(MemoryConnection::new(Dialect::MySql));
        assert!(Arc::ptr_eq(db.connection(), db.clone().connection()));
    }
}
