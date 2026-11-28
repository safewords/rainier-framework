//! What this crate publishes on the event bus, and a declarative way to
//! subscribe to one model's changes.
//!
//! - [`QueryExecuted`] — every statement through a [`Database`](crate::Database)
//!   handle, an [`Activity`] of kind `db.query` carrying what the `db.query`
//!   span carries. Published whether or not the `telemetry` feature is on.
//! - [`ModelChanged`] — every committed write; see
//!   [`Database::dispatch_changes_to`](crate::Database::dispatch_changes_to).
//! - `Created<M>`, `Updated<M>`, `Deleted<M>` — published (fire and forget)
//!   by a repository built `with_events`; `Creating<M>`, `Updating<M>` and
//!   `Deleting<M>` are dispatched and awaited, so they can veto the write.
//!
//! [`OnModel::on_model`] subscribes to [`ModelChanged`] for one model, and
//! optionally one row and some actions:
//!
//! ```
//! # use rainier_orm::Entity;
//! # use rainier_database::{Model, ModelAction, OnModel};
//! # use rainier_events::Dispatcher;
//! # #[derive(Entity, Clone)]
//! # #[orm(table = "users")]
//! # struct User { #[orm(pk)] id: u64 }
//! # impl Model for User {}
//! # fn wire(events: &Dispatcher) {
//! events
//!     .on_model::<User>()
//!     .key(42)
//!     .actions(&[ModelAction::Updated])
//!     .run(|change, _cx| async move {
//!         println!("user 42 changed: {}", change.action);
//!         Ok(())
//!     });
//! # }
//! ```

use std::backtrace::Backtrace;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rainier_events::{code_trace, Activity, Context, Dispatcher, Fields};
use rainier_orm::Dialect;
use rainier_support::Result;

use crate::model::{Model, ModelAction, ModelChanged};
use crate::telemetry;

// --- PoolSampled -------------------------------------------------------------

/// One process's look at one connection's pool, on the framework's sampling
/// interval: kind `db.pool.sample`.
///
/// Published outside every unit of work, never on a query's path. Every
/// replica samples its own pool, so a reader adds the replicas' pools
/// together (each sample says how long it stands for, `interval`), and reads
/// the server's numbers — the same server, seen by each — as one value.
#[derive(Debug, Clone)]
pub struct PoolSampled {
    /// The connection's name (`mysql`, `primary`), or `default`.
    pub connection: String,
    /// The backend.
    pub dialect: Dialect,
    /// What the pool and the server said.
    pub usage: rainier_orm::PoolUsage,
    /// How long this sample stands for.
    pub interval: Duration,
}

impl Activity for PoolSampled {
    fn kind(&self) -> &'static str {
        "db.pool.sample"
    }

    fn fields(&self, fields: &mut Fields) {
        let server = self.usage.server.as_ref();
        fields
            .set("db.system.name", telemetry::system(self.dialect))
            .set("rainier.db.connection", self.connection.clone())
            .set("rainier.db.pool.open", u64::from(self.usage.open))
            .set("rainier.db.pool.idle", u64::from(self.usage.idle))
            .set("rainier.db.pool.max", u64::from(self.usage.max))
            .set(
                "rainier.db.interval_ms",
                u64::try_from(self.interval.as_millis()).unwrap_or(u64::MAX),
            )
            .maybe("rainier.db.server.user", server.map(|s| s.user))
            .maybe("rainier.db.server.user_limit", server.and_then(|s| s.user_limit))
            .maybe("rainier.db.server.total", server.map(|s| s.total))
            .maybe("rainier.db.server.max", server.map(|s| s.max))
            .maybe("rainier.db.server.error", self.usage.server_error.clone());
    }
}

// --- QueryExecuted -----------------------------------------------------------

/// One statement run through a [`Database`](crate::Database) handle: kind
/// `db.query`, with the fields the `db.query` span records.
///
/// The SQL is [sanitised](crate::telemetry::sanitize) — placeholders and
/// literals as `?` — and the bound values are never carried out of it.
/// The sanitising happens **when a subscriber first reads it**, on the
/// subscriber's thread: the statement runs on the request's thread and that
/// thread only copies the text. The unsanitised text is private to this type;
/// nothing reads it but [`sql`](Self::sql) and its siblings.
#[derive(Clone)]
pub struct QueryExecuted {
    /// `db.system.name`: `mysql`, `postgresql` or `sqlite`.
    pub system: &'static str,
    /// The statement as run. Private: a raw statement can carry literal
    /// values.
    statement: Arc<str>,
    /// What the statement says, worked out on first read.
    described: Arc<std::sync::OnceLock<Described>>,
    /// `db.response.returned_rows`, for a read that succeeded.
    pub returned_rows: Option<usize>,
    /// How long the statement took.
    pub duration: Duration,
    /// Whether it failed.
    pub failed: bool,
    /// Where it was run from, when it was slow enough for a subscriber to
    /// have asked ([`code_trace`]).
    pub code_trace: Option<Arc<Backtrace>>,
}

#[derive(Debug)]
struct Described {
    operation: Option<String>,
    collection: Option<String>,
    sql: String,
}

impl QueryExecuted {
    /// Describe `sql`, run on `dialect`, as it ended.
    ///
    /// Copies the statement and nothing else; a code trace is taken here, on
    /// the thread that ran it, only when it was slower than a subscriber
    /// asked about.
    pub fn new(
        dialect: Dialect,
        sql: &str,
        duration: Duration,
        returned_rows: Option<usize>,
        failed: bool,
    ) -> Self {
        let code_trace = code_trace::capture(duration, || telemetry::sanitize(sql));
        Self {
            system: telemetry::system(dialect),
            statement: Arc::from(sql),
            described: Arc::new(std::sync::OnceLock::new()),
            returned_rows,
            duration,
            failed,
            code_trace,
        }
    }

    fn described(&self) -> &Described {
        self.described.get_or_init(|| {
            let (operation, collection) = telemetry::describe(&self.statement);
            Described { operation, collection, sql: telemetry::sanitize(&self.statement) }
        })
    }

    /// `db.query.text`: the sanitised statement.
    pub fn sql(&self) -> &str {
        &self.described().sql
    }

    /// `db.operation.name`: the statement's first keyword, upper-cased.
    pub fn operation(&self) -> Option<&str> {
        self.described().operation.as_deref()
    }

    /// `db.collection.name`: the table the statement names, when it can be
    /// read.
    pub fn collection(&self) -> Option<&str> {
        self.described().collection.as_deref()
    }

    /// The span's `otel.name`: `SELECT users`, `SELECT`, or the system.
    pub fn otel_name(&self) -> String {
        match (self.operation(), self.collection()) {
            (Some(operation), Some(collection)) => format!("{operation} {collection}"),
            (Some(operation), None) => operation.to_string(),
            _ => self.system.to_string(),
        }
    }
}

impl std::fmt::Debug for QueryExecuted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryExecuted")
            .field("system", &self.system)
            .field("sql", &self.sql())
            .field("returned_rows", &self.returned_rows)
            .field("duration", &self.duration)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Activity for QueryExecuted {
    fn kind(&self) -> &'static str {
        "db.query"
    }

    fn fields(&self, fields: &mut Fields) {
        fields
            .set("otel.name", self.otel_name())
            .set("otel.kind", "client")
            .set("db.system.name", self.system)
            .set("db.operation.name", self.operation().unwrap_or_default().to_string())
            .set("db.collection.name", self.collection().unwrap_or_default().to_string())
            .set("db.query.text", self.sql().to_string())
            .maybe("db.response.returned_rows", self.returned_rows);
        if self.failed {
            // `error.type` only, as the span: a driver's message is where a
            // bound value comes back out.
            fields.set("otel.status_code", "ERROR").set("error.type", "db_error");
        }
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }

    fn failed(&self) -> bool {
        self.failed
    }

    fn code_trace(&self) -> Option<&Arc<Backtrace>> {
        self.code_trace.as_ref()
    }
}

/// Run one statement's `future`: inside its `db.query` span with the
/// `telemetry` feature, and published as a [`QueryExecuted`] when anything
/// hears it.
///
/// With nobody subscribed this asks the bus once and awaits `future` as it
/// is: no clock read, no sanitising, no allocation.
pub(crate) async fn observed<T, E, F>(
    dialect: Dialect,
    sql: &str,
    future: F,
    rows: fn(&T) -> Option<usize>,
) -> std::result::Result<T, E>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    #[cfg(feature = "telemetry")]
    let future = telemetry::traced(telemetry::span(dialect, sql), future, rows);

    if !rainier_events::activity::has_activity_subscribers::<QueryExecuted>() {
        return future.await;
    }

    let started = Instant::now();
    let result = future.await;
    let elapsed = started.elapsed();
    rainier_events::activity::publish_activity_with(|| {
        QueryExecuted::new(
            dialect,
            sql,
            elapsed,
            result.as_ref().ok().and_then(rows),
            result.is_err(),
        )
    });
    result
}

// --- on_model ----------------------------------------------------------------

/// Subscribe to one model's [`ModelChanged`] events, declaratively. See the
/// [module docs](self).
pub trait OnModel {
    /// Changes to `M`'s rows — narrow them with
    /// [`key`](ModelSubscription::key) and
    /// [`actions`](ModelSubscription::actions), then
    /// [`run`](ModelSubscription::run) or
    /// [`run_sync`](ModelSubscription::run_sync).
    fn on_model<M: Model>(&self) -> ModelSubscription<'_>;
}

impl OnModel for Dispatcher {
    fn on_model<M: Model>(&self) -> ModelSubscription<'_> {
        ModelSubscription {
            events: self,
            model_type: std::any::type_name::<M>(),
            table: M::table(),
            model_name: M::model_name(),
            keys: Vec::new(),
            actions: Vec::new(),
            name: None,
        }
    }
}

/// A subscription to one model's changes being declared:
/// [`OnModel::on_model`].
///
/// Built on [`Dispatcher::on`]`::<ModelChanged>()`, so it is a subscriber
/// like any other: it runs on a thread of its own after the write has
/// returned, never on the writer's path, and a handler's `Err` is logged and
/// reported, never returned to the writer.
#[must_use = "a subscription does nothing until `run` or `run_sync` registers it"]
pub struct ModelSubscription<'a> {
    events: &'a Dispatcher,
    model_type: &'static str,
    table: &'static str,
    model_name: &'static str,
    keys: Vec<String>,
    actions: Vec<ModelAction>,
    name: Option<String>,
}

/// What a [`ModelSubscription`] matches, moved onto the subscriber's thread.
#[derive(Debug, Clone)]
struct ModelFilter {
    model_type: &'static str,
    table: &'static str,
    keys: Vec<String>,
    actions: Vec<ModelAction>,
}

impl ModelFilter {
    fn matches(&self, change: &ModelChanged) -> bool {
        let same_model = match change.model_type {
            Some(model_type) => model_type == self.model_type,
            // A change described by table alone names no type: the table is
            // what identifies the model.
            None => change.table == self.table,
        };
        same_model
            && (self.actions.is_empty() || self.actions.contains(&change.action))
            && (self.keys.is_empty()
                || match &change.keys {
                    Some(keys) => keys.iter().any(|key| self.keys.contains(key)),
                    // A write that did not say which rows may have touched
                    // this one — the same answer `RowChange::may_touch` gives.
                    None => true,
                })
    }
}

impl ModelSubscription<'_> {
    /// Only changes to the row keyed `key` — its primary key as
    /// `ModelChanged` carries it: an integer in decimal, a string as it is.
    /// Several calls widen the set.
    ///
    /// A write that does not say which rows it touched (a bulk
    /// `update_matching`, an insert whose key the database assigns) may have
    /// touched this one, so it matches too; its `keys` is `None`.
    pub fn key(mut self, key: impl ToString) -> Self {
        self.keys.push(key.to_string());
        self
    }

    /// Only changes to any of `keys`. See [`key`](Self::key).
    pub fn keys<K: ToString>(mut self, keys: impl IntoIterator<Item = K>) -> Self {
        self.keys.extend(keys.into_iter().map(|key| key.to_string()));
        self
    }

    /// Only these actions. Every action when never called.
    pub fn actions(mut self, actions: &[ModelAction]) -> Self {
        self.actions.extend_from_slice(actions);
        self
    }

    /// Name the subscriber's thread. Defaults to `model-<Model>`.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn subscription(&self) -> rainier_events::bus::Subscription<'_, ModelChanged> {
        let filter = ModelFilter {
            model_type: self.model_type,
            table: self.table,
            keys: self.keys.clone(),
            actions: self.actions.clone(),
        };
        let name = self.name.clone().unwrap_or_else(|| format!("model-{}", self.model_name));
        self.events.on::<ModelChanged>().named(name).when(move |change| filter.matches(change))
    }

    /// Register `handler`, an async closure taking the change and its
    /// [`Context`].
    pub fn run<F, Fut>(self, handler: F)
    where
        F: Fn(Arc<ModelChanged>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.subscription().run(handler);
    }

    /// Register `handler`, a synchronous closure run on the subscriber's
    /// thread.
    pub fn run_sync<F>(self, handler: F)
    where
        F: Fn(&ModelChanged, &Context) -> Result<()> + Send + Sync + 'static,
    {
        self.subscription().run_sync(handler);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rainier_events::Value;

    use super::*;
    use crate::statement;
    use crate::testing::{fake_database, MemoryConnection};
    use crate::OwnedRow;

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "users")]
    struct User {
        #[orm(pk, auto_increment)]
        id: u64,
        email: String,
    }

    impl Model for User {}

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
    }

    impl Model for Post {}

    /// Every `QueryExecuted` published on a fresh bus, as its fields.
    fn heard_queries(events: &Dispatcher) -> Arc<Mutex<Vec<(Fields, bool, Duration)>>> {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on::<QueryExecuted>().run_sync(move |query, _cx| {
            sink.lock().unwrap().push((query.collect_fields(), query.failed, query.duration));
            Ok(())
        });
        heard
    }

    #[tokio::test]
    async fn every_statement_is_published_with_the_spans_fields_and_never_its_values() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard_queries(&events);
        let (db, _) = fake_database(
            MemoryConnection::new(Dialect::Sqlite)
                .returning([OwnedRow::new().with("id", 1_i64).with("email", "x")]),
        );

        let user = User { id: 0, email: "ada@example.com".into() };
        db.execute(statement::insert(Dialect::Sqlite, &user, None)).await.unwrap();
        let _: Vec<User> =
            db.fetch_all(statement::select_all::<User>(Dialect::Sqlite)).await.unwrap();
        db.statement("DELETE FROM sessions WHERE token = 'hunter2'").await.unwrap();
        events.settled().await;

        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 3);
        let insert = &heard[0].0;
        assert_eq!(insert.text("db.system.name"), Some("sqlite"));
        assert_eq!(insert.text("db.operation.name"), Some("INSERT"));
        assert_eq!(insert.text("db.collection.name"), Some("users"));
        assert_eq!(insert.text("otel.name"), Some("INSERT users"));
        assert_eq!(insert.text("otel.kind"), Some("client"));
        assert_eq!(heard[1].0.get("db.response.returned_rows"), Some(&Value::U64(1)));
        assert_eq!(
            heard[2].0.text("db.query.text"),
            Some("DELETE FROM sessions WHERE token = ?"),
            "a raw statement's inline value is sanitised away"
        );
        let everything = format!("{heard:?}");
        assert!(!everything.contains("ada@example.com"), "a bound value never appears");
        assert!(!everything.contains("hunter2"), "nor an inline one");
    }

    #[tokio::test]
    async fn a_failed_statement_is_published_failed_as_the_span_records_it() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let heard = heard_queries(&events);
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite).failing("disk full"));

        db.statement("DELETE FROM jobs").await.unwrap_err();
        events.settled().await;

        let heard = heard.lock().unwrap();
        assert!(heard[0].1, "failed");
        assert_eq!(heard[0].0.text("otel.status_code"), Some("ERROR"));
        assert_eq!(heard[0].0.text("error.type"), Some("db_error"));
    }

    #[tokio::test]
    async fn with_nobody_subscribed_nothing_is_built() {
        // A bus with a subscriber for something else: `observed` asks once and
        // runs the statement as it is.
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        events.on::<ModelChanged>().run_sync(|_, _| Ok(()));
        assert!(!rainier_events::activity::has_activity_subscribers::<QueryExecuted>());

        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        db.statement("DELETE FROM jobs").await.unwrap();
        assert_eq!(connection.statement_count(), 1);
    }

    #[tokio::test]
    async fn a_slow_query_carries_a_code_trace_when_one_was_asked_for() {
        let events = Arc::new(Dispatcher::new());
        let _bus = rainier_events::bus::set_default(Arc::clone(&events));
        let traced = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&traced);
        events.on::<QueryExecuted>().run_sync(move |query, _| {
            sink.lock().unwrap().push(query.code_trace.is_some());
            Ok(())
        });

        code_trace::request_code_traces(Duration::ZERO);
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        db.statement("DELETE FROM jobs").await.unwrap();
        code_trace::reset();
        events.settled().await;

        assert_eq!(*traced.lock().unwrap(), [true]);
    }

    fn change(
        model_type: Option<&'static str>,
        table: &str,
        action: ModelAction,
        keys: Option<&[&str]>,
    ) -> ModelChanged {
        ModelChanged {
            model: table.to_string(),
            model_type,
            table: table.to_string(),
            action,
            keys: keys.map(|keys| keys.iter().map(|k| k.to_string()).collect()),
        }
    }

    #[tokio::test]
    async fn on_model_hears_one_model_one_row_and_the_actions_asked_for() {
        let events = Dispatcher::new();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on_model::<User>().key(42).actions(&[ModelAction::Updated]).run_sync(
            move |change, _cx| {
                sink.lock().unwrap().push(change.clone());
                Ok(())
            },
        );

        let user = Some(std::any::type_name::<User>());
        let post = Some(std::any::type_name::<Post>());
        // Heard: user 42 updated; an update that did not name its rows.
        events.publish(change(user, "users", ModelAction::Updated, Some(&["42"])));
        events.publish(change(user, "users", ModelAction::Updated, None));
        // Not heard: another user, another action, another model.
        events.publish(change(user, "users", ModelAction::Updated, Some(&["7"])));
        events.publish(change(user, "users", ModelAction::Deleted, Some(&["42"])));
        events.publish(change(post, "posts", ModelAction::Updated, Some(&["42"])));
        // Described by table alone: the table names the model.
        events.publish(change(None, "users", ModelAction::Updated, Some(&["42", "43"])));
        events.publish(change(None, "posts", ModelAction::Updated, Some(&["42"])));
        events.settled().await;

        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 3, "{heard:#?}");
        assert_eq!(heard[0].keys.as_deref(), Some(&["42".to_string()][..]));
        assert_eq!(heard[1].keys, None);
        assert_eq!(heard[2].model_type, None);
    }

    #[tokio::test]
    async fn on_model_with_no_narrowing_hears_every_change_to_the_model_async() {
        let events = Dispatcher::new();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on_model::<Post>().run(move |change, _cx| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push(change.action);
                Ok(())
            }
        });

        let post = Some(std::any::type_name::<Post>());
        for action in ModelAction::ALL {
            events.publish(change(post, "posts", *action, Some(&["1"])));
        }
        events.publish(change(
            Some(std::any::type_name::<User>()),
            "users",
            ModelAction::Created,
            None,
        ));
        events.settled().await;

        assert_eq!(*heard.lock().unwrap(), ModelAction::ALL);
    }

    #[tokio::test]
    async fn on_model_hears_a_real_write_through_dispatch_changes_to() {
        let events = Arc::new(Dispatcher::new());
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite).with_outcome(1, 0));
        db.dispatch_changes_to(Arc::clone(&events));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        events.on_model::<User>().key(42).run_sync(move |change, _| {
            sink.lock().unwrap().push(change.action);
            Ok(())
        });

        let repo = crate::EntityRepository::<User>::new(db);
        crate::Repository::update(&repo, &User { id: 42, email: "a".into() }).await.unwrap();
        crate::Repository::update(&repo, &User { id: 7, email: "b".into() }).await.unwrap();
        events.settled().await;

        assert_eq!(*heard.lock().unwrap(), [ModelAction::Updated]);
    }
}
