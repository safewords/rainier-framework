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
//!
//! ## One handle, more than one endpoint
//!
//! A [`Database`] is usually one connection, and everything above describes
//! that case unchanged. When its declaration splits reads from writes it holds
//! several, and the method that was called decides which one a statement
//! reaches: [`fetch`](Database::fetch) and its decoders read, and
//! [`execute`](Database::execute) and [`statement`](Database::statement)
//! write. Nothing inspects the SQL to decide.
//!
//! That last sentence is the one to remember when writing SQL by hand. A
//! `DELETE … RETURNING` or a `WITH … INSERT` handed to a *fetch* is a write
//! sent to a replica, where at best it is refused and at worst it lands on a
//! server nothing else reads. [`writer`](Database::writer) is the handle for
//! it: same connection, reads included, pinned to the endpoint that accepts
//! writes.
//!
//! Reading your own writes is [`sticky`]'s subject, not this
//! module's.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};

use rainier_orm::sea_query::Value;
use rainier_orm::{Dialect, ExecOutcome, Executor, Row, ShardRoute};
use rainier_support::{BoxFuture, Error, Result};

use crate::row::{ColumnRequest, OwnedRow};
use crate::statement::Prepared;
use crate::sticky;

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
    /// The endpoint writes go to, and the only endpoint of an ordinary
    /// connection. Held out of [`Split`] as well as in it so that a handle with
    /// no split costs one `Arc` and no allocation, which is nearly all of them.
    connection: Arc<dyn Connection>,

    /// The other endpoints, when the declaration named any. `None` is the
    /// unsplit connection, and every path through this type is the one it was
    /// before splitting existed.
    split: Option<Arc<Split>>,

    /// This handle sends its reads to the writer — see [`Database::writer`].
    to_the_writer: bool,
}

impl Database {
    /// Wrap a concrete connection.
    pub fn new(connection: impl Connection) -> Self {
        Self::from_arc(Arc::new(connection))
    }

    /// Wrap an already-shared connection.
    pub fn from_arc(connection: Arc<dyn Connection>) -> Self {
        Self { connection, split: None, to_the_writer: false }
    }

    /// A handle over separate write and read endpoints.
    ///
    /// `writes` is where [`execute`](Self::execute) and
    /// [`statement`](Self::statement) go; `reads` is where
    /// [`fetch`](Self::fetch) goes, and an empty `reads` means reads go to the
    /// writer like everything else. `sticky` asks for
    /// [read-your-own-writes](crate::sticky) within a scope.
    ///
    /// Built from a declaration by
    /// [`DatabaseConfig::build`](crate::DatabaseConfig::build), and reachable
    /// here for a backend a configuration tree cannot describe — the same door
    /// [`DatabaseManager::with_connection`](crate::DatabaseManager::with_connection)
    /// exists for.
    ///
    /// A single write endpoint with no read endpoints collapses to the ordinary
    /// shape, because that is what it is: one connection, with nothing to route
    /// between and nothing to be stale against.
    ///
    /// # Errors
    ///
    /// When `writes` is empty. There would be nowhere for a write to go, and
    /// answering reads from a replica while silently dropping writes is the
    /// failure this crate's configuration layer refuses at every turn.
    pub fn with_endpoints(
        writes: Vec<Arc<dyn Connection>>,
        reads: Vec<Arc<dyn Connection>>,
        sticky: bool,
    ) -> Result<Self> {
        let Some(first) = writes.first().cloned() else {
            return Err(Error::internal(
                "a database handle needs at least one endpoint to write to; one with only read \
                 endpoints would answer every query and persist nothing",
            ));
        };

        if reads.is_empty() && writes.len() == 1 {
            return Ok(Self::from_arc(first));
        }

        Ok(Self {
            connection: first,
            split: Some(Arc::new(Split {
                id: sticky::next_connection_id(),
                sticky,
                writes,
                reads,
                next_write: AtomicUsize::new(0),
                next_read: AtomicUsize::new(0),
                unscoped: Once::new(),
            })),
            to_the_writer: false,
        })
    }

    /// The same handle, with its **reads** sent to the write endpoint too.
    ///
    /// For SQL this type cannot classify. Routing is by the method that was
    /// called, so a statement that writes and returns rows —
    /// `DELETE … RETURNING`, `WITH … INSERT`, an advisory lock, `SELECT …
    /// FOR UPDATE` — reaches a replica if it is fetched, and a replica is
    /// either read-only or a server whose copy of that change nothing else
    /// will ever see.
    ///
    /// Free: no connection is opened and nothing is cloned but the handle.
    ///
    /// Does nothing on a connection that was never split, which is the right
    /// answer rather than a no-op worth warning about — the writer is the only
    /// endpoint there is.
    #[must_use = "this returns a new handle rather than changing this one"]
    pub fn writer(&self) -> Self {
        Self { to_the_writer: true, ..self.clone() }
    }

    /// The underlying connection.
    ///
    /// The write endpoint on a split connection: the one every statement can
    /// run against, and the one an ordinary handle has always returned.
    pub fn connection(&self) -> &Arc<dyn Connection> {
        &self.connection
    }

    /// Whether this connection reads and writes through separate endpoints.
    pub fn is_split(&self) -> bool {
        self.split.is_some()
    }

    /// Whether a write pins this connection's reads for the rest of the
    /// [scope](crate::sticky).
    pub fn is_sticky(&self) -> bool {
        self.split.as_ref().is_some_and(|split| split.sticky)
    }

    /// The endpoint a write goes to.
    ///
    /// Not `dialect`'s or `allocate_id`'s business: those ask the connection a
    /// question rather than running a statement, and routing them through here
    /// would let a dialect lookup take a scope's write pin.
    fn write_connection(&self) -> &Arc<dyn Connection> {
        match &self.split {
            None => &self.connection,
            Some(split) => split.write_connection(),
        }
    }

    /// The endpoint a read goes to.
    fn read_connection(&self) -> &Arc<dyn Connection> {
        match &self.split {
            None => &self.connection,
            Some(_) if self.to_the_writer => self.write_connection(),
            Some(split) => split.read_connection(),
        }
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
        self.read_connection().fetch(prepared.route, &prepared.sql, prepared.params, columns).await
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
            self.write_connection().execute(prepared.route, &prepared.sql, prepared.params).await?;
        Ok(outcomes.into_iter().next().unwrap_or_default())
    }

    /// Run a raw statement with no bindings — for DDL.
    pub async fn statement(&self, sql: &str) -> Result<ExecOutcome> {
        let outcomes = self.write_connection().execute(ShardRoute::Global, sql, Vec::new()).await?;
        Ok(outcomes.into_iter().next().unwrap_or_default())
    }
}

/// The endpoints of a connection that separates reads from writes.
///
/// Every one of them is a pool that was opened at boot, so choosing between
/// them is arithmetic rather than a connection attempt.
struct Split {
    /// This connection's identity to a [scope](crate::sticky).
    id: usize,

    /// Whether a write pins this connection's reads for the rest of a scope.
    sticky: bool,

    /// Where writes go. Never empty; `writes[0]` is [`Database::connection`].
    writes: Vec<Arc<dyn Connection>>,

    /// Where reads go. Empty means they go to the writer.
    reads: Vec<Arc<dyn Connection>>,

    next_write: AtomicUsize,
    next_read: AtomicUsize,

    /// Guards the one warning a sticky connection outside a scope emits.
    unscoped: Once,
}

impl Split {
    fn write_connection(&self) -> &Arc<dyn Connection> {
        if !self.sticky {
            return &self.writes[self.turn(&self.next_write, self.writes.len())];
        }

        let chosen =
            sticky::write_endpoint(self.id, || self.turn(&self.next_write, self.writes.len()))
                .unwrap_or_else(|| self.turn(&self.next_write, self.writes.len()));
        &self.writes[chosen]
    }

    fn read_connection(&self) -> &Arc<dyn Connection> {
        if self.reads.is_empty() {
            return self.write_connection();
        }
        if !self.sticky {
            return &self.reads[self.turn(&self.next_read, self.reads.len())];
        }

        match sticky::read_endpoint(self.id, || self.turn(&self.next_read, self.reads.len())) {
            Some(sticky::Read::Replica(replica)) => &self.reads[replica],
            Some(sticky::Read::Writer) => self.write_connection(),
            // No scope, so nothing can be pinned — and an unpinned read from a
            // connection that asked for read-your-own-writes is the stale row
            // `sticky` was declared to rule out. The writer is the answer that
            // is never wrong; see `sticky` for why it is preferred to the one
            // that is merely faster.
            None => {
                self.warn_about_the_missing_scope();
                self.write_connection()
            }
        }
    }

    /// The next endpoint of a role, round the ring.
    ///
    /// Round-robin rather than the random pick Laravel makes, because the
    /// endpoints were opened at boot and are therefore known: a counter spreads
    /// queries evenly, where a random choice is even only in expectation and
    /// will happily send a burst of six at one replica.
    ///
    /// `Relaxed` because nothing is ordered against this. Two threads racing
    /// may take the same turn or skip one, and the cost of either is that a
    /// replica serves one query more than its neighbour.
    fn turn(&self, counter: &AtomicUsize, len: usize) -> usize {
        counter.fetch_add(1, Ordering::Relaxed) % len
    }

    /// Say once, per connection per process, that the read hosts are idle and
    /// why.
    ///
    /// Once rather than per query: a console command or a worker that never
    /// enters a scope would otherwise write this line for every row it reads,
    /// and a warning that repeats is a warning that gets filtered.
    fn warn_about_the_missing_scope(&self) {
        self.unscoped.call_once(|| {
            tracing::warn!(
                "this connection declares `sticky`, and a read reached it outside a sticky \
                 scope — so it was served by the write endpoint rather than by a read host. \
                 A read that no scope is tracking cannot be known to be reading its own \
                 write, and a replica would answer it with whatever it has replicated so \
                 far. Wrap the unit of work in `rainier_database::with_sticky_scope`, or \
                 drop `sticky` if these reads tolerate replication lag"
            );
        });
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("Database");
        out.field("dialect", &self.connection.dialect())
            .field("shard_family", &self.connection.shard_family());

        // Named only when there is something to say, so the dump of an
        // ordinary connection reads exactly as it did.
        if let Some(split) = &self.split {
            out.field("write_endpoints", &split.writes.len())
                .field("read_endpoints", &split.reads.len())
                .field("sticky", &split.sticky);
        }
        out.finish()
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
        self.read_connection().fetch_raw(ShardRoute::Global, sql, params).await.map_err(into_anyhow)
    }

    async fn execute(&self, sql: &str, params: Vec<Value>) -> rainier_orm::Result<ExecOutcome> {
        let outcomes = self
            .write_connection()
            .execute(ShardRoute::Global, sql, params)
            .await
            .map_err(into_anyhow)?;
        Ok(outcomes.into_iter().next().unwrap_or_default())
    }

    async fn fetch_all_routed(
        &self,
        route: ShardRoute,
        sql: &str,
        params: Vec<Value>,
    ) -> rainier_orm::Result<Vec<Box<dyn Row>>> {
        self.read_connection().fetch_raw(route, sql, params).await.map_err(into_anyhow)
    }

    async fn execute_routed(
        &self,
        route: ShardRoute,
        sql: &str,
        params: Vec<Value>,
    ) -> rainier_orm::Result<ExecOutcome> {
        let outcomes =
            self.write_connection().execute(route, sql, params).await.map_err(into_anyhow)?;
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
    use crate::sticky::with_sticky_scope;
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

    // --- reads here, writes there -------------------------------------------

    /// `n` endpoints, plus the handles to assert what reached each of them.
    fn endpoints(n: usize) -> (Vec<Arc<dyn Connection>>, Vec<Arc<MemoryConnection>>) {
        let handles: Vec<Arc<MemoryConnection>> =
            (0..n).map(|_| Arc::new(MemoryConnection::new(Dialect::Sqlite))).collect();
        let connections =
            handles.iter().map(|handle| Arc::clone(handle) as Arc<dyn Connection>).collect();
        (connections, handles)
    }

    fn select() -> Prepared {
        statement::select_all::<Post>(Dialect::Sqlite)
    }

    fn delete() -> Prepared {
        statement::delete_by_pk::<Post>(Dialect::Sqlite, 1_i64.into())
    }

    #[tokio::test]
    async fn a_connection_with_no_split_is_the_connection_it_always_was() {
        // The property every existing deployment depends on: declaring nothing
        // leaves every query on one endpoint, through the same code path.
        let (connections, handles) = endpoints(1);
        let db = Database::with_endpoints(connections, Vec::new(), false).expect("one endpoint");

        assert!(!db.is_split());
        assert!(!db.is_sticky());

        let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
        db.execute(delete()).await.unwrap();
        db.statement("CREATE TABLE posts (id INTEGER)").await.unwrap();

        assert_eq!(handles[0].statement_count(), 3);
    }

    #[tokio::test]
    async fn reads_go_to_a_read_endpoint_and_writes_to_a_write_endpoint() {
        let (writes, writers) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, false).expect("built");

        assert!(db.is_split());

        let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
        assert_eq!(readers[0].statement_count(), 1);
        assert_eq!(writers[0].statement_count(), 0);

        db.execute(delete()).await.unwrap();
        db.statement("CREATE TABLE posts (id INTEGER)").await.unwrap();
        assert_eq!(writers[0].statement_count(), 2, "a write reached a replica");
        assert_eq!(readers[0].statement_count(), 1);
    }

    #[tokio::test]
    async fn every_read_host_takes_its_turn() {
        // Declaring three replicas and using one is a configuration that looks
        // right in the file and does nothing in the fleet.
        let (writes, _) = endpoints(1);
        let (reads, readers) = endpoints(3);
        let db = Database::with_endpoints(writes, reads, false).expect("built");

        for _ in 0..6 {
            let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
        }

        for (at, reader) in readers.iter().enumerate() {
            assert_eq!(reader.statement_count(), 2, "replica {at} was skipped");
        }
    }

    #[tokio::test]
    async fn every_write_host_takes_its_turn() {
        let (writes, writers) = endpoints(2);
        let db = Database::with_endpoints(writes, Vec::new(), false).expect("built");

        for _ in 0..4 {
            db.execute(delete()).await.unwrap();
        }

        for (at, writer) in writers.iter().enumerate() {
            assert_eq!(writer.statement_count(), 2, "primary {at} was skipped");
        }
    }

    #[tokio::test]
    async fn a_write_sends_the_reads_after_it_in_the_same_scope_to_the_writer() {
        // The whole point of `sticky`: the row the write just made is on the
        // writer, and the replica would answer without it and without an error.
        let (writes, writers) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, true).expect("built");
        assert!(db.is_sticky());

        with_sticky_scope(async {
            let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
            assert_eq!(
                readers[0].statement_count(),
                1,
                "a read before any write is a replica read"
            );

            db.execute(delete()).await.unwrap();

            let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
            let _: Vec<Post> = db.fetch_all(select()).await.unwrap();

            // One write plus two reads, all on the endpoint that has the row.
            assert_eq!(writers[0].statement_count(), 3);
            assert_eq!(readers[0].statement_count(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn a_write_in_one_scope_leaves_another_scopes_reads_on_the_replicas() {
        let (writes, writers) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, true).expect("built");

        let writing = db.clone();
        with_sticky_scope(async move {
            writing.execute(delete()).await.unwrap();
            let _: Vec<Post> = writing.fetch_all(select()).await.unwrap();
        })
        .await;

        // A second unit of work through the *same handle*. A flag on the
        // connection would still be set here.
        with_sticky_scope(async {
            let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
        })
        .await;

        assert_eq!(writers[0].statement_count(), 2, "the write and its own read");
        assert_eq!(readers[0].statement_count(), 1, "the next scope was pinned by the last one");
    }

    #[tokio::test]
    async fn a_sticky_connection_outside_a_scope_reads_from_the_writer() {
        // Documented, and the safe direction: nothing is tracking this read, so
        // nothing can say it is not reading its own write.
        let (writes, writers) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, true).expect("built");

        let _: Vec<Post> = db.fetch_all(select()).await.unwrap();

        assert_eq!(writers[0].statement_count(), 1);
        assert_eq!(readers[0].statement_count(), 0);
    }

    #[tokio::test]
    async fn a_connection_that_did_not_ask_for_sticky_reads_the_replica_either_way() {
        // The other half of the same rule: a declaration that did not ask for
        // read-your-own-writes gets the split it did ask for, scope or no
        // scope.
        let (writes, writers) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, false).expect("built");

        db.execute(delete()).await.unwrap();
        let _: Vec<Post> = db.fetch_all(select()).await.unwrap();

        assert_eq!(writers[0].statement_count(), 1);
        assert_eq!(readers[0].statement_count(), 1);
    }

    #[tokio::test]
    async fn the_writer_handle_sends_a_fetch_to_the_write_endpoint() {
        // For SQL this type cannot classify — `DELETE … RETURNING` and its
        // relatives, which are writes that come back as rows.
        let (writes, writers) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, false).expect("built");

        let _: Vec<Post> = db.writer().fetch_all(select()).await.unwrap();

        assert_eq!(writers[0].statement_count(), 1);
        assert_eq!(readers[0].statement_count(), 0);

        // …and the handle it came from is unchanged.
        let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
        assert_eq!(readers[0].statement_count(), 1);
    }

    #[tokio::test]
    async fn the_orm_surface_splits_the_same_way_the_prepared_one_does() {
        // `Database` is an `Executor`, so `repo::` renders and runs its own
        // SQL. If that path did not split, half the framework's queries would
        // ignore the declaration.
        let (writes, writers) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, false).expect("built");

        Executor::fetch_all(&db, "SELECT 1", vec![]).await.unwrap();
        assert_eq!(readers[0].statement_count(), 1);
        assert_eq!(writers[0].statement_count(), 0);

        Executor::execute(&db, "DELETE FROM posts", vec![]).await.unwrap();
        assert_eq!(writers[0].statement_count(), 1);
    }

    #[tokio::test]
    async fn asking_a_split_connection_a_question_does_not_pin_it() {
        // `dialect` and `allocate_id` are questions rather than statements. If
        // they went through the routing they would take a scope's write pin,
        // and a repository that reads the dialect before every query would pin
        // every scope on its first read.
        let (writes, _) = endpoints(1);
        let (reads, readers) = endpoints(1);
        let db = Database::with_endpoints(writes, reads, true).expect("built");

        with_sticky_scope(async {
            assert_eq!(db.dialect(), Dialect::Sqlite);
            assert!(!db.is_sharded());

            let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
            assert_eq!(readers[0].statement_count(), 1, "a dialect lookup pinned the connection");
        })
        .await;
    }

    #[test]
    fn a_handle_with_nowhere_to_write_is_refused() {
        let (reads, _) = endpoints(2);
        let err = Database::with_endpoints(Vec::new(), reads, false).unwrap_err();
        assert!(err.message().contains("write"), "{}", err.message());
    }

    #[test]
    fn one_endpoint_and_no_replicas_is_not_a_split() {
        let (writes, _) = endpoints(1);
        let db = Database::with_endpoints(writes, Vec::new(), true).expect("built");

        // Not even with `sticky` asked for: there is one endpoint, so there is
        // nothing to be stale against and nothing to pin.
        assert!(!db.is_split());
        assert!(!db.is_sticky());
    }

    #[tokio::test]
    async fn a_role_with_no_replicas_sends_its_reads_to_the_writers() {
        // `write` declared and `read` left out: two write hosts, and reads that
        // have nowhere else to go.
        let (writes, writers) = endpoints(2);
        let db = Database::with_endpoints(writes, Vec::new(), false).expect("built");

        assert!(db.is_split());
        let _: Vec<Post> = db.fetch_all(select()).await.unwrap();
        let _: Vec<Post> = db.fetch_all(select()).await.unwrap();

        assert_eq!(writers[0].statement_count() + writers[1].statement_count(), 2);
    }
}
