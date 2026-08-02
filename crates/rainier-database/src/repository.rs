//! Repositories — the [`Repository`] contract and its generic implementation.
//!
//! Rainier ORM already collapses "a repository per engine per table" to zero
//! hand-written repositories. This layer adds the two things a *service* wants
//! on top of generic CRUD:
//!
//! 1. **A contract to depend on.** A controller takes
//!    `Arc<dyn Repository<Post>>`, so it can be handed the real thing in
//!    production and a fake in a test, with neither knowing about the other.
//! 2. **Model lifecycle hooks.** Writes dispatch `Creating`/`Created`/… through
//!    the event bus — where audit logs, cache busting and search indexing hang
//!    off without the repository knowing they exist.
//!
//! [`EntityRepository`] implements the contract for *any* [`Model`], so
//! declaring a repository is still no code:
//!
//! ```ignore
//! let posts = EntityRepository::<Post>::new(db.clone()).with_events(events.clone());
//! let page = posts.paginate(1, 20).await?;
//! ```

use std::marker::PhantomData;
use std::sync::Arc;

use rainier_events::Dispatcher;
use rainier_orm::sea_query::Value;
use rainier_orm::Entity;
use rainier_support::{Error, Result};

use crate::connection::Database;
use crate::criteria::{Criteria, Projection};
use crate::model::{Created, Creating, Deleted, Deleting, Model, Updated, Updating};
use crate::pagination::Paginated;
use crate::relation::{PivotQuery, RelationKey};
use crate::row::Cell;
use crate::row::{ColumnRequest, OwnedRow};
use crate::statement;

/// Read and write access to one model's rows.
///
/// Object-safe: keys and values arrive as [`Value`] rather than through
/// generic parameters, so `Arc<dyn Repository<Post>>` is a usable dependency.
/// The ergonomic generic wrappers are provided methods bounded on
/// `Self: Sized`, which keeps them out of the vtable.
#[async_trait::async_trait]
pub trait Repository<M: Model>: Send + Sync + 'static {
    /// Every row. Prefer [`paginate`](Self::paginate) for anything unbounded.
    async fn all(&self) -> Result<Vec<M>>;

    /// The row with this primary key.
    async fn find(&self, key: Value) -> Result<Option<M>>;

    /// Every row where `column` equals `value` — how a relationship is
    /// traversed, since the ORM keeps foreign keys as flat columns.
    async fn find_by(&self, column: &str, value: Value) -> Result<Vec<M>>;

    /// The first row where `column` equals `value`.
    async fn first_by(&self, column: &str, value: Value) -> Result<Option<M>>;

    /// Every row matching `criteria`.
    async fn matching(&self, criteria: Criteria) -> Result<Vec<M>>;

    /// The first row matching `criteria`.
    async fn first_matching(&self, criteria: Criteria) -> Result<Option<M>>;

    /// How many rows match `criteria`, ignoring its paging.
    async fn count_matching(&self, criteria: Criteria) -> Result<u64>;

    /// Run an aggregate query and return its rows as selected.
    ///
    /// For a `criteria` carrying [`select`](Criteria::select) projections and
    /// usually [`group_by`](Criteria::group_by). The result is **not** decoded
    /// into the entity — the columns are whatever was projected, read by the
    /// alias each was given.
    ///
    /// This exists so an application never has to drop to raw SQL for a report.
    /// Raw SQL in a handler is a query nothing type-checks, and it silently
    /// commits to one dialect: `MONTH(x)` is MySQL's spelling and simply does
    /// not exist in SQLite, so a query written that way works in production and
    /// fails in the test suite.
    async fn aggregate(&self, criteria: Criteria) -> Result<Vec<OwnedRow>>;

    /// One page of rows matching `criteria`.
    async fn paginate_matching(
        &self,
        criteria: Criteria,
        page: u64,
        per_page: u64,
    ) -> Result<Paginated<M>>;

    /// Insert a row, returning it with any database-assigned key.
    async fn create(&self, model: M) -> Result<M>;

    /// Update every non-key column of a row, matched by its primary key.
    async fn update(&self, model: &M) -> Result<u64>;

    /// Insert, or update on a conflict with `conflict_columns`. An empty
    /// `update_columns` makes it insert-or-ignore.
    async fn upsert(
        &self,
        model: &M,
        conflict_columns: &[&str],
        update_columns: &[&str],
    ) -> Result<u64>;

    /// Delete the row with this primary key.
    async fn delete(&self, key: Value) -> Result<u64>;

    /// Delete every row matching `criteria`. Returns rows affected.
    async fn delete_matching(&self, criteria: Criteria) -> Result<u64>;

    /// `SELECT column, COUNT(*) GROUP BY column` — how many rows share each
    /// value of `column`.
    ///
    /// What [`Relation::count`](crate::relation::Relation::count) is: one query
    /// for every parent's child count, rather than one per parent.
    ///
    /// The default loads the rows and tallies them, which is correct for any
    /// implementation and wrong only in what it transfers.
    /// [`EntityRepository`] overrides it with a real `GROUP BY`.
    async fn count_grouped(
        &self,
        column: &str,
        criteria: Criteria,
    ) -> Result<Vec<(RelationKey, u64)>> {
        let mut counts: std::collections::HashMap<RelationKey, u64> =
            std::collections::HashMap::new();

        for model in self.matching(criteria).await? {
            let Some(value) = model.value_of(column) else { continue };
            *counts.entry(RelationKey::new(&value)).or_insert(0) += 1;
        }
        Ok(counts.into_iter().collect())
    }

    /// Read a pivot table's `(parent, related)` links.
    ///
    /// A pivot is two columns and no model, so it cannot go through the typed
    /// path. It lives on this trait rather than on `Database` so that a fake
    /// repository can answer it — a many-to-many is not testable otherwise.
    ///
    /// The default returns nothing, which is what a repository with no
    /// database behind it honestly has.
    async fn pivot_links(&self, query: PivotQuery) -> Result<Vec<(Value, Value)>> {
        let _ = query;
        Ok(Vec::new())
    }

    // --- provided conveniences (not part of the vtable) --------------------

    /// [`find`](Self::find) with a key of any convertible type.
    async fn find_key(&self, key: impl Into<Value> + Send) -> Result<Option<M>>
    where
        Self: Sized,
    {
        self.find(key.into()).await
    }

    /// [`find`](Self::find), failing with a `404` naming the model when there
    /// is no such row — what a controller almost always wants.
    async fn find_or_fail(&self, key: impl Into<Value> + Send) -> Result<M>
    where
        Self: Sized,
    {
        self.find(key.into()).await?.ok_or_else(|| {
            Error::not_found(format!("No {} matches the given key.", M::model_name()))
        })
    }

    /// The row whose [route key](Model::route_key_name) equals `value` —
    /// route-model binding's lookup.
    async fn find_by_route_key(&self, value: impl Into<Value> + Send) -> Result<Option<M>>
    where
        Self: Sized,
    {
        self.first_by(M::route_key_name(), value.into()).await
    }

    /// Every row where `column` equals a value of any convertible type.
    async fn where_eq(&self, column: &str, value: impl Into<Value> + Send) -> Result<Vec<M>>
    where
        Self: Sized,
    {
        self.find_by(column, value.into()).await
    }

    /// How many rows there are in total.
    async fn count(&self) -> Result<u64>
    where
        Self: Sized,
    {
        self.count_matching(Criteria::new()).await
    }

    /// One page of every row.
    async fn paginate(&self, page: u64, per_page: u64) -> Result<Paginated<M>>
    where
        Self: Sized,
    {
        self.paginate_matching(Criteria::new(), page, per_page).await
    }

    /// Whether any row matches `criteria`.
    async fn exists(&self, criteria: Criteria) -> Result<bool>
    where
        Self: Sized,
    {
        Ok(self.count_matching(criteria).await? > 0)
    }
}

/// The generic repository: one implementation serving every [`Model`].
pub struct EntityRepository<M> {
    db: Database,
    events: Option<Arc<Dispatcher>>,
    _model: PhantomData<fn() -> M>,
}

impl<M> Clone for EntityRepository<M> {
    fn clone(&self) -> Self {
        Self { db: self.db.clone(), events: self.events.clone(), _model: PhantomData }
    }
}

/// What a repository can do knowing only that its type is an [`Entity`].
///
/// Bounded on `Entity` rather than [`Model`] — which is `Entity + SingleKey` —
/// so that an entity keyed on more than one column can still be read from.
/// [`Repository`]'s own methods stay `Model`-bound because most of them name a
/// row by its key, and "the key" is one value there.
///
/// [`aggregate`](Self::aggregate) is the method that does not: it projects
/// columns and returns rows, and never identifies a row by anything. Leaving it
/// on the `Model`-bound trait made it unreachable for composite-key entities,
/// whose only remaining route to a `SUM` was raw SQL — or loading every row and
/// adding them up in process, which is the same query with the table scan moved
/// somewhere it cannot be indexed.
impl<E: Entity> EntityRepository<E> {
    /// A repository over `db`, with no lifecycle hooks.
    pub fn new(db: Database) -> Self {
        Self { db, events: None, _model: PhantomData }
    }

    /// Dispatch lifecycle hooks through `events`.
    pub fn with_events(mut self, events: Arc<Dispatcher>) -> Self {
        self.events = Some(events);
        self
    }

    /// The underlying database handle — the escape hatch for queries this
    /// contract does not cover. Reach for it rather than growing the trait.
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Run an aggregate query and return the projected rows.
    ///
    /// The same query [`Repository::aggregate`] runs — that one delegates here,
    /// so a model and a composite-key entity cannot drift apart.
    pub async fn aggregate_rows(&self, criteria: Criteria) -> Result<Vec<OwnedRow>> {
        let requests: Vec<ColumnRequest> = criteria
            .projections()
            .iter()
            .map(|(projection, name)| {
                ColumnRequest::new(name.clone(), column_type_of::<E>(projection))
            })
            .collect();

        self.db
            .fetch(statement::select_aggregate::<E>(self.db.dialect(), &criteria), requests)
            .await
    }
}

impl<M: Model> EntityRepository<M> {
    /// Dispatch a lifecycle hook.
    ///
    /// The event is only *built* when something is listening, so the model
    /// clone a hook needs costs nothing in an application that uses none.
    async fn fire<E>(&self, build: impl FnOnce() -> E) -> Result<()>
    where
        E: Send + Sync + 'static,
    {
        let Some(events) = &self.events else { return Ok(()) };
        if !events.has_listeners::<E>() {
            return Ok(());
        }
        events.dispatch(build()).await
    }
}

#[async_trait::async_trait]
impl<M: Model> Repository<M> for EntityRepository<M> {
    async fn all(&self) -> Result<Vec<M>> {
        self.db.fetch_all(statement::select_all::<M>(self.db.dialect())).await
    }

    async fn find(&self, key: Value) -> Result<Option<M>> {
        self.db.fetch_one(statement::select_by_pk::<M>(self.db.dialect(), key)).await
    }

    async fn find_by(&self, column: &str, value: Value) -> Result<Vec<M>> {
        let prepared = statement::select_by_column::<M>(self.db.dialect(), column, value, None);
        self.db.fetch_all(prepared).await
    }

    async fn first_by(&self, column: &str, value: Value) -> Result<Option<M>> {
        let prepared = statement::select_by_column::<M>(self.db.dialect(), column, value, Some(1));
        self.db.fetch_one(prepared).await
    }

    async fn matching(&self, criteria: Criteria) -> Result<Vec<M>> {
        self.db.fetch_all(statement::select_matching::<M>(self.db.dialect(), &criteria)).await
    }

    async fn aggregate(&self, criteria: Criteria) -> Result<Vec<OwnedRow>> {
        // Delegated rather than duplicated: the same query has to mean the same
        // thing whether the type behind it has one key column or several.
        self.aggregate_rows(criteria).await
    }

    async fn first_matching(&self, criteria: Criteria) -> Result<Option<M>> {
        let criteria = criteria.limit(1);
        self.db.fetch_one(statement::select_matching::<M>(self.db.dialect(), &criteria)).await
    }

    async fn count_matching(&self, criteria: Criteria) -> Result<u64> {
        self.db.fetch_count(statement::count_matching::<M>(self.db.dialect(), &criteria)).await
    }

    async fn count_grouped(
        &self,
        column: &str,
        criteria: Criteria,
    ) -> Result<Vec<(RelationKey, u64)>> {
        let Some(spec) = M::columns().iter().find(|spec| spec.name == column) else {
            return Err(Error::internal(format!(
                "`{}` has no column `{column}` to group by",
                M::table()
            )));
        };

        let prepared = statement::count_grouped::<M>(self.db.dialect(), column, &criteria);
        let columns = vec![
            crate::row::ColumnRequest::new(column, spec.ty),
            crate::row::ColumnRequest::new("cnt", rainier_orm::ColumnType::BigInt),
        ];

        let rows = self.db.fetch(prepared, columns).await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let key = row.cell(column).map(RelationKey::from_cell)?;
                let count = row.cell("cnt").and_then(Cell::as_u64).unwrap_or(0);
                Some((key, count))
            })
            .collect())
    }

    async fn pivot_links(&self, query: PivotQuery) -> Result<Vec<(Value, Value)>> {
        if query.parent_keys.is_empty() {
            return Ok(Vec::new());
        }

        let prepared = statement::select_pivot(self.db.dialect(), &query);
        // A pivot's columns are whatever the keys they mirror are, and this
        // layer cannot see the far entity — so both are read as the near
        // side's own key type, which is what a foreign key must be.
        let key_type = M::columns()
            .iter()
            .find(|spec| spec.pk)
            .map(|spec| spec.ty)
            .unwrap_or(rainier_orm::ColumnType::BigInt);

        let columns = vec![
            crate::row::ColumnRequest::new(&query.parent_column, key_type),
            crate::row::ColumnRequest::new(&query.related_column, key_type),
        ];

        let rows = self.db.fetch(prepared, columns).await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let parent = row.cell(&query.parent_column).and_then(Cell::to_value)?;
                let related = row.cell(&query.related_column).and_then(Cell::to_value)?;
                Some((parent, related))
            })
            .collect())
    }

    async fn paginate_matching(
        &self,
        criteria: Criteria,
        page: u64,
        per_page: u64,
    ) -> Result<Paginated<M>> {
        let page = page.max(1);
        let per_page = per_page.max(1);

        let total = self.count_matching(criteria.clone()).await?;
        if total == 0 {
            // No point selecting rows we already know are not there.
            return Ok(Paginated::empty(page, per_page));
        }

        let window = criteria.limit(per_page).offset((page - 1) * per_page);
        let data = self.matching(window).await?;
        Ok(Paginated::new(data, total, page, per_page))
    }

    async fn create(&self, model: M) -> Result<M> {
        self.fire(|| Creating { model: model.clone() }).await?;

        // A sharded entity whose primary key is itself the shard key has no
        // auto-increment to fall back on, so the connector mints the id.
        let assigned = statement::shard_key_for_insert::<M>(&model)
            .and_then(|shard_key| self.db.connection().allocate_id(shard_key));

        let prepared = statement::insert::<M>(self.db.dialect(), &model, assigned);
        let outcome = self.db.execute(prepared).await?;

        let id = assigned.map(|id| id as i64).unwrap_or(outcome.last_insert_id);

        // Re-read only when the *database* assigned the key, so the caller
        // sees the row as it actually is — defaults, triggers and generated
        // columns included — rather than the struct it passed in with a stale
        // key.
        //
        // The auto-increment check is load-bearing: a model with an
        // application-assigned key (a string id, a shard-encoded id) still
        // gets a `last_insert_id` from some drivers, and following it would
        // fetch an unrelated row and hand it back as the one just created.
        let stored = if assigned.is_none() && id > 0 && has_auto_increment_key::<M>() {
            self.find(id.into()).await?.unwrap_or(model)
        } else {
            model
        };

        self.fire(|| Created { model: stored.clone(), id }).await?;
        Ok(stored)
    }

    async fn update(&self, model: &M) -> Result<u64> {
        self.fire(|| Updating { model: model.clone() }).await?;

        let prepared = statement::update::<M>(self.db.dialect(), model);
        let rows_affected = self.db.execute(prepared).await?.rows_affected;

        self.fire(|| Updated { model: model.clone(), rows_affected }).await?;
        Ok(rows_affected)
    }

    async fn upsert(
        &self,
        model: &M,
        conflict_columns: &[&str],
        update_columns: &[&str],
    ) -> Result<u64> {
        let prepared =
            statement::upsert::<M>(self.db.dialect(), model, conflict_columns, update_columns);
        Ok(self.db.execute(prepared).await?.rows_affected)
    }

    async fn delete(&self, key: Value) -> Result<u64> {
        let described = describe(&key);
        self.fire(|| Deleting::<M> { key: described.clone(), model: PhantomData }).await?;

        let prepared = statement::delete_by_pk::<M>(self.db.dialect(), key);
        let rows_affected = self.db.execute(prepared).await?.rows_affected;

        self.fire(|| Deleted::<M> { key: described, rows_affected, model: PhantomData }).await?;
        Ok(rows_affected)
    }

    async fn delete_matching(&self, criteria: Criteria) -> Result<u64> {
        let prepared = statement::delete_matching::<M>(self.db.dialect(), &criteria);
        Ok(self.db.execute(prepared).await?.rows_affected)
    }
}

/// Whether `M`'s primary key is assigned by the database.
fn has_auto_increment_key<M: Model>() -> bool {
    M::columns().iter().any(|column| column.name == M::primary_key() && column.auto_increment)
}

/// A readable rendering of a primary key, for the delete hooks.
fn describe(key: &Value) -> String {
    match key {
        Value::String(Some(text)) => text.to_string(),
        Value::BigInt(Some(n)) => n.to_string(),
        Value::Int(Some(n)) => n.to_string(),
        Value::BigUnsigned(Some(n)) => n.to_string(),
        Value::Unsigned(Some(n)) => n.to_string(),
        other => format!("{other:?}"),
    }
}

impl<M> std::fmt::Debug for EntityRepository<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntityRepository")
            .field("model", &std::any::type_name::<M>())
            .field("hooks", &self.events.is_some())
            .finish()
    }
}

/// What a projection's column comes back as.
///
/// # Why this consults the entity instead of guessing
///
/// `SUM` and `MIN`/`MAX` return the *summed column's own type*, so a fixed
/// answer is wrong for half of them. Reading `BigInt` off a `SUM` over a
/// `double` column — a money total, say — decodes a float as an integer and
/// silently mangles the value, which is worse than failing.
///
/// So the column's declared type comes from the entity, which already knows it.
/// Only the projections whose type is genuinely fixed regardless of input are
/// hardcoded: a count is integral, and a date part is an integer by
/// construction (the SQLite branch casts for exactly that reason).
fn column_type_of<E: rainier_orm::Entity>(projection: &Projection) -> rainier_orm::ColumnType {
    use rainier_orm::ColumnType;

    /// The declared type of one of the entity's own columns.
    fn declared<E: rainier_orm::Entity>(name: &str) -> Option<ColumnType> {
        // A qualified `table.column` refers to a joined table this entity knows
        // nothing about, so there is nothing to look up.
        if name.contains('.') {
            return None;
        }
        E::columns().iter().find(|c| c.name == name).map(|c| c.ty)
    }

    match projection {
        // Fixed regardless of what they read.
        Projection::CountAll | Projection::Count(_) | Projection::CountWhenIn(..) => {
            ColumnType::BigInt
        }
        Projection::DatePart(..) => ColumnType::Int,
        // A calendar date, which every dialect renders as text here.
        Projection::DateOf(_) => ColumnType::Text,

        // `AVG` is fractional even over integers.
        Projection::Avg(_) => ColumnType::Double,

        // These carry the column's own type through. Unknown — a joined table,
        // or a column this entity does not declare — falls back to `Text`,
        // which every driver can produce and the caller can parse, rather than
        // to a numeric type that would misread a string.
        Projection::Sum(c) => declared::<E>(c).unwrap_or(ColumnType::BigInt),
        Projection::Column(c) | Projection::Min(c) | Projection::Max(c) => {
            declared::<E>(c).unwrap_or(ColumnType::Text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row::OwnedRow;
    use crate::testing::{fake_database, MemoryConnection};
    use rainier_orm::{Dialect, ShardRoute};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
        title: String,
        published: bool,
    }

    impl Model for Post {}

    #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
    #[orm(table = "tokens")]
    struct Token {
        #[orm(pk, shard_key)]
        id: u64,
        #[orm(shard_key)]
        user_id: u64,
        hash: String,
    }

    impl Model for Token {}

    fn row(id: u64, title: &str) -> OwnedRow {
        OwnedRow::new().with("id", id).with("title", title).with("published", true)
    }

    fn count(n: i64) -> OwnedRow {
        OwnedRow::new().with("cnt", n)
    }

    fn repository(connection: MemoryConnection) -> (EntityRepository<Post>, Arc<MemoryConnection>) {
        let (db, handle) = fake_database(connection);
        (EntityRepository::<Post>::new(db), handle)
    }

    #[test]
    fn repository_calls_produce_send_futures() {
        // The property that lets a handler await a repository at all.
        fn assert_send<T: Send>(_: T) {}

        let (posts, _) = repository(MemoryConnection::new(Dialect::Sqlite));
        assert_send(async move {
            let _ = posts.all().await;
        });
    }

    #[tokio::test]
    async fn all_decodes_every_row() {
        let (posts, _) = repository(
            MemoryConnection::new(Dialect::Sqlite).returning([row(1, "a"), row(2, "b")]),
        );

        let found = posts.all().await.unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], Post { id: 1, title: "a".into(), published: true });
    }

    #[tokio::test]
    async fn find_returns_none_when_nothing_matches() {
        let (posts, _) = repository(MemoryConnection::new(Dialect::Sqlite));
        assert!(posts.find(1_i64.into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn find_or_fail_reports_a_404_naming_the_model() {
        let (posts, _) = repository(MemoryConnection::new(Dialect::Sqlite));

        let err = posts.find_or_fail(1_i64).await.unwrap_err();
        assert_eq!(err.status(), 404);
        assert!(err.message().contains("Post"), "{}", err.message());
    }

    #[tokio::test]
    async fn find_or_fail_returns_the_row_when_it_exists() {
        let (posts, _) =
            repository(MemoryConnection::new(Dialect::Sqlite).returning([row(1, "found")]));
        assert_eq!(posts.find_or_fail(1_i64).await.unwrap().title, "found");
    }

    #[tokio::test]
    async fn first_by_limits_to_one_row() {
        let (posts, connection) =
            repository(MemoryConnection::new(Dialect::Sqlite).returning([row(1, "a")]));

        posts.first_by("title", "a".into()).await.unwrap();
        assert!(connection.last_statement().unwrap().contains("LIMIT"));
    }

    #[tokio::test]
    async fn criteria_reach_the_generated_sql() {
        let (posts, connection) = repository(MemoryConnection::new(Dialect::Sqlite));

        posts
            .matching(Criteria::new().where_eq("published", true).order_by_desc("id").limit(5))
            .await
            .unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("published"), "{sql}");
        assert!(sql.contains("ORDER BY"), "{sql}");
        assert!(sql.contains("LIMIT"), "{sql}");
    }

    #[tokio::test]
    async fn counting_ignores_the_criterias_paging() {
        // Regression guard: a `LIMIT 5` in the criteria must not make the
        // count come back as 5.
        let (posts, connection) =
            repository(MemoryConnection::new(Dialect::Sqlite).returning([count(42)]));

        let total = posts
            .count_matching(Criteria::new().where_eq("published", true).limit(5).offset(10))
            .await
            .unwrap();

        assert_eq!(total, 42);
        let sql = connection.last_statement().unwrap();
        assert!(!sql.contains("LIMIT"), "the count must not be limited: {sql}");
        assert!(sql.contains("published"), "but it must keep the filters: {sql}");
    }

    #[tokio::test]
    async fn paginate_reports_the_window() {
        let connection = MemoryConnection::new(Dialect::Sqlite)
            .returning([count(25)])
            .returning([row(11, "k"), row(12, "l")]);
        let (posts, _) = repository(connection);

        let page = posts.paginate(2, 10).await.unwrap();
        assert_eq!(page.total, 25);
        assert_eq!(page.current_page, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page.last_page(), 3);
        assert_eq!(page.from(), Some(11));
    }

    #[tokio::test]
    async fn paginating_no_results_skips_the_row_query() {
        let (posts, connection) =
            repository(MemoryConnection::new(Dialect::Sqlite).returning([count(0)]));

        let page = posts.paginate(1, 10).await.unwrap();
        assert!(page.is_empty());
        assert_eq!(connection.statement_count(), 1);
    }

    #[tokio::test]
    async fn exists_is_a_count() {
        let (posts, _) = repository(MemoryConnection::new(Dialect::Sqlite).returning([count(3)]));
        assert!(posts.exists(Criteria::new()).await.unwrap());

        let (posts, _) = repository(MemoryConnection::new(Dialect::Sqlite).returning([count(0)]));
        assert!(!posts.exists(Criteria::new()).await.unwrap());
    }

    #[tokio::test]
    async fn create_reads_the_row_back_when_the_database_assigned_a_key() {
        let connection =
            MemoryConnection::new(Dialect::Sqlite).with_outcome(1, 7).returning([row(7, "stored")]);
        let (posts, _) = repository(connection);

        let created =
            posts.create(Post { id: 0, title: "sent".into(), published: false }).await.unwrap();

        assert_eq!(created.id, 7);
        assert_eq!(created.title, "stored", "the stored row wins over the struct we sent");
    }

    #[tokio::test]
    async fn create_does_not_re_read_a_model_with_an_application_assigned_key() {
        // Regression guard: some drivers report a `last_insert_id` even for a
        // string key. Following it would fetch an unrelated row.
        #[derive(rainier_orm::Entity, Clone, Debug, PartialEq)]
        #[orm(table = "sessions")]
        struct Session {
            #[orm(pk)]
            id: String,
            user_id: u64,
        }
        impl Model for Session {}

        let (db, connection) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).with_outcome(1, 99));
        let sessions = EntityRepository::<Session>::new(db);

        let created = sessions.create(Session { id: "abc".into(), user_id: 1 }).await.unwrap();

        assert_eq!(created.id, "abc");
        assert_eq!(connection.statement_count(), 1, "the insert only — no re-read");
        assert!(connection.last_statement().unwrap().starts_with("INSERT INTO"));
    }

    #[tokio::test]
    async fn a_sharded_insert_asks_the_connector_for_an_id_and_routes_to_it() {
        let (db, connection) =
            fake_database(MemoryConnection::new(Dialect::Sqlite).sharded("users").allocating(4242));
        let tokens = EntityRepository::<Token>::new(db);

        let created = tokens.create(Token { id: 0, user_id: 42, hash: "h".into() }).await.unwrap();

        // No re-read: the id was app-assigned, so the struct is already right.
        assert_eq!(created.id, 0);
        assert_eq!(connection.last_route(), Some(ShardRoute::Key(4242)));
        assert_eq!(connection.statement_count(), 1);
    }

    #[tokio::test]
    async fn a_database_failure_surfaces() {
        let (posts, _) = repository(MemoryConnection::new(Dialect::Sqlite).failing("disk full"));
        let err = posts.all().await.unwrap_err();
        assert!(err.message().contains("disk full"), "{}", err.message());
    }

    // --- lifecycle hooks ---------------------------------------------------

    #[allow(clippy::type_complexity)]
    fn with_hooks() -> (EntityRepository<Post>, Arc<Dispatcher>, Arc<MemoryConnection>) {
        let (db, connection) = fake_database(
            MemoryConnection::new(Dialect::Sqlite).with_outcome(1, 7).returning([row(7, "stored")]),
        );
        let events = Arc::new(Dispatcher::new());
        (EntityRepository::<Post>::new(db).with_events(Arc::clone(&events)), events, connection)
    }

    #[tokio::test]
    async fn create_fires_creating_then_created() {
        let (posts, events, _) = with_hooks();
        let log = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&log);
        events.listen(move |event: Arc<Creating<Post>>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push(format!("creating:{}", event.model.title));
                Ok(())
            }
        });

        let sink = Arc::clone(&log);
        events.listen(move |event: Arc<Created<Post>>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push(format!("created:{}", event.id));
                Ok(())
            }
        });

        posts.create(Post { id: 0, title: "sent".into(), published: false }).await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["creating:sent", "created:7"]);
    }

    #[tokio::test]
    async fn a_creating_listener_can_veto_the_insert() {
        let (posts, events, connection) = with_hooks();
        events.listen(|_: Arc<Creating<Post>>| async { Err(Error::unauthorized("read only")) });

        let err = posts
            .create(Post { id: 0, title: "blocked".into(), published: false })
            .await
            .unwrap_err();

        assert_eq!(err.status(), 403);
        assert_eq!(
            connection.statement_count(),
            0,
            "a vetoed insert must never reach the database"
        );
    }

    #[tokio::test]
    async fn update_and_delete_fire_their_hooks() {
        let (posts, events, _) = with_hooks();
        let seen = Arc::new(AtomicUsize::new(0));

        let counter = Arc::clone(&seen);
        events.listen(move |_: Arc<Updating<Post>>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        let counter = Arc::clone(&seen);
        events.listen(move |_: Arc<Updated<Post>>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(10, Ordering::SeqCst);
                Ok(())
            }
        });
        let counter = Arc::clone(&seen);
        events.listen(move |_: Arc<Deleting<Post>>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(100, Ordering::SeqCst);
                Ok(())
            }
        });
        let counter = Arc::clone(&seen);
        events.listen(move |event: Arc<Deleted<Post>>| {
            let counter = Arc::clone(&counter);
            async move {
                assert_eq!(event.key, "1");
                counter.fetch_add(1000, Ordering::SeqCst);
                Ok(())
            }
        });

        posts.update(&Post { id: 1, title: "x".into(), published: true }).await.unwrap();
        posts.delete(1_i64.into()).await.unwrap();

        assert_eq!(seen.load(Ordering::SeqCst), 1111);
    }

    #[tokio::test]
    async fn hooks_are_skipped_entirely_when_nothing_listens() {
        let (posts, _, connection) = with_hooks();
        // No listeners registered: `create` must still work, and must not have
        // paid for a model clone it never used.
        posts.create(Post { id: 0, title: "x".into(), published: false }).await.unwrap();
        assert!(connection.statement_count() >= 1);
    }

    #[tokio::test]
    async fn the_contract_is_object_safe() {
        let (db, _) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let boxed: Arc<dyn Repository<Post>> = Arc::new(EntityRepository::<Post>::new(db));
        assert!(boxed.all().await.unwrap().is_empty());
    }
}
