//! Relationships — has-one, has-many, belongs-to and belongs-to-many, as
//! **values you load** rather than properties that query themselves.
//!
//! ```ignore
//! impl Post {
//!     /// The author.
//!     pub fn author() -> BelongsTo<Post, User> {
//!         BelongsTo::new()
//!     }
//! }
//!
//! let posts = post_repo.all().await?;
//! let authors = Post::author().load(&posts, &*user_repo).await?;   // ONE query
//!
//! for post in &posts {
//!     println!("{} by {}", post.title, authors.one(post).unwrap().name);
//! }
//! ```
//!
//! # Why loading is explicit
//!
//! A lazy-loading ORM gives you `$post->author`, and the price is that the same
//! expression is one query inside a loop and none after an eager load. You
//! cannot tell which by reading it. PHP can do that because `__get` can run a
//! query; Rust has no such hook, and forging one — a `RefCell`, a handle to the
//! connection on every model, a blocking call inside `Deref` — would buy the
//! syntax by making every model carry a database.
//!
//! So Rainier keeps the two operations apart. **Declaring** a relationship is a
//! value; **loading** it is a call that takes the whole slice of parents and
//! issues one query for all of them. The N+1 problem is not mitigated here, it
//! is unrepresentable: there is no per-model load to put in a loop.
//!
//! # What one query means across backends
//!
//! Loading is a `WHERE key IN (…)` against the *other side's own repository*,
//! never a `JOIN`. That is what lets the two sides live in different databases,
//! or on different shards, and it is the same strategy eager loading uses
//! everywhere. [`Criteria::join`](crate::Criteria) is still there when both sides are
//! genuinely in one place and you want the database to do the work.

use std::collections::HashMap;
use std::marker::PhantomData;

use rainier_orm::sea_query::Value;
use rainier_support::{str, Result};

use crate::criteria::Criteria;
use crate::model::Model;
use crate::repository::Repository;

/// A related-key lookup value.
///
/// Keys are normalised to text so that a `u64` primary key and the `u64`
/// foreign key pointing at it agree, whichever integer width each side's driver
/// hands back. Two *different* columns are never compared, so the normalisation
/// cannot conflate unrelated rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelationKey(String);

impl RelationKey {
    /// The key for a column value.
    pub fn new(value: &Value) -> Self {
        Self(normalise(value))
    }

    /// The key for a value read back out of a row.
    pub fn from_cell(cell: &crate::row::Cell) -> Self {
        match cell.to_value() {
            Some(value) => Self::new(&value),
            None => Self(null()),
        }
    }

    /// The key as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RelationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Normalise a value to its lookup key.
///
/// Every integer width collapses to one decimal rendering, so `Int(7)` from one
/// driver and `BigUnsigned(7)` from another are the same key.
fn normalise(value: &Value) -> String {
    macro_rules! int {
        ($v:expr) => {
            $v.map(|n| (n as i128).to_string()).unwrap_or_else(null)
        };
    }

    match value {
        Value::TinyInt(v) => int!(*v),
        Value::SmallInt(v) => int!(*v),
        Value::Int(v) => int!(*v),
        Value::BigInt(v) => int!(*v),
        Value::TinyUnsigned(v) => int!(*v),
        Value::SmallUnsigned(v) => int!(*v),
        Value::Unsigned(v) => int!(*v),
        Value::BigUnsigned(v) => int!(*v),
        Value::Bool(v) => v.map(|b| b.to_string()).unwrap_or_else(null),
        Value::String(v) => v.as_ref().map(|s| s.to_string()).unwrap_or_else(null),
        Value::Char(v) => v.map(|c| c.to_string()).unwrap_or_else(null),
        Value::Bytes(v) => v.as_ref().map(|b| hex(b)).unwrap_or_else(null),
        // Floats, dates and anything a driver adds later. Debug is stable
        // enough for a lookup key, and a float key is a mistake anyway.
        other => format!("{other:?}"),
    }
}

fn null() -> String {
    "\0null".to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Rows loaded for a relationship, grouped by the near side's key.
///
/// Returned by every relation's `load`. Look a parent's related rows up by
/// handing it the parent — the container knows which column to read.
#[derive(Debug, Clone)]
pub struct Related<C> {
    /// The column on the **near** side whose value keys this map: a parent's
    /// `id` for `has_many`, a child's `author_id` for `belongs_to`.
    near_key: String,
    groups: HashMap<RelationKey, Vec<C>>,
}

impl<C> Related<C> {
    /// An empty result — what loading no parents gives you.
    pub fn empty(near_key: impl Into<String>) -> Self {
        Self { near_key: near_key.into(), groups: HashMap::new() }
    }

    /// Every related row for `near`, in the order the query returned them.
    pub fn of<N: Model>(&self, near: &N) -> &[C] {
        self.key_of(near).and_then(|key| self.groups.get(&key)).map_or(&[], Vec::as_slice)
    }

    /// The first related row for `near` — what a `has_one` or `belongs_to`
    /// wants.
    pub fn one<N: Model>(&self, near: &N) -> Option<&C> {
        self.of(near).first()
    }

    /// How many related rows `near` has **among those loaded**.
    ///
    /// Not the same as [`Relation::count`], which asks the database and is not
    /// limited by any `limit` on the relation.
    pub fn count_of<N: Model>(&self, near: &N) -> usize {
        self.of(near).len()
    }

    /// Whether anything was loaded at all.
    pub fn is_empty(&self) -> bool {
        self.groups.values().all(Vec::is_empty)
    }

    /// How many rows were loaded, across every parent.
    pub fn len(&self) -> usize {
        self.groups.values().map(Vec::len).sum()
    }

    /// The column the map is keyed by.
    pub fn near_key(&self) -> &str {
        &self.near_key
    }

    /// Pair each of `near` with its related rows, consuming both.
    ///
    /// For handing a controller something to serialise: a `Vec<(Post, Vec<Comment>)>`
    /// rather than two collections and a lookup.
    pub fn zip<N: Model>(mut self, near: Vec<N>) -> Vec<(N, Vec<C>)> {
        near.into_iter()
            .map(|model| {
                let related = model
                    .value_of(&self.near_key)
                    .and_then(|value| self.groups.remove(&RelationKey::new(&value)))
                    .unwrap_or_default();
                (model, related)
            })
            .collect()
    }

    fn key_of<N: Model>(&self, near: &N) -> Option<RelationKey> {
        near.value_of(&self.near_key).map(|value| RelationKey::new(&value))
    }

    /// Group rows by the value of `far_key`, to be looked up by `near_key`.
    fn group_by(near_key: &str, far_key: &str, rows: Vec<C>) -> Self
    where
        C: Model,
    {
        let mut groups: HashMap<RelationKey, Vec<C>> = HashMap::new();
        for row in rows {
            let Some(value) = row.value_of(far_key) else { continue };
            groups.entry(RelationKey::new(&value)).or_default().push(row);
        }
        Self { near_key: near_key.to_string(), groups }
    }
}

/// Counts loaded for a relationship, without loading the rows.
#[derive(Debug, Clone, Default)]
pub struct RelatedCounts {
    near_key: String,
    counts: HashMap<RelationKey, u64>,
}

impl RelatedCounts {
    /// How many related rows `near` has. `0` when it has none: a parent with
    /// no children produces no group, not a zero row.
    pub fn of<N: Model>(&self, near: &N) -> u64 {
        near.value_of(&self.near_key)
            .and_then(|value| self.counts.get(&RelationKey::new(&value)).copied())
            .unwrap_or(0)
    }

    /// The total across every parent.
    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }
}

/// What every relationship can do.
///
/// Object-safe on purpose: a function can take `&dyn Relation<Post, Related = Comment>`
/// and not care which kind it was given.
#[async_trait::async_trait]
pub trait Relation<P: Model>: Send + Sync {
    /// The model on the other side.
    type Related: Model;

    /// The column on `P` that identifies it in this relationship.
    fn near_key(&self) -> &str;

    /// The column on the other side that this relationship matches against.
    fn far_key(&self) -> &str;

    /// Load the related rows for every parent, in **one** query.
    async fn load(
        &self,
        parents: &[P],
        related: &dyn Repository<Self::Related>,
    ) -> Result<Related<Self::Related>>;

    /// Count the related rows for every parent, in one query, without
    /// transferring them.
    async fn count(
        &self,
        parents: &[P],
        related: &dyn Repository<Self::Related>,
    ) -> Result<RelatedCounts>;

    /// Load for a single parent.
    ///
    /// The one-off case — a show page for one post. Inside a loop this is the
    /// N+1 that [`load`](Self::load) exists to make impossible, so reach for it
    /// only when there genuinely is one parent.
    async fn for_one(
        &self,
        parent: &P,
        related: &dyn Repository<Self::Related>,
    ) -> Result<Vec<Self::Related>>
    where
        Self: Sized,
    {
        let loaded = self.load(std::slice::from_ref(parent), related).await?;
        Ok(loaded.of(parent).to_vec())
    }
}

/// The keys of `parents`, deduplicated, in first-seen order.
fn keys_of<P: Model>(parents: &[P], column: &str) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::with_capacity(parents.len());

    for parent in parents {
        let Some(value) = parent.value_of(column) else { continue };
        if seen.insert(RelationKey::new(&value)) {
            keys.push(value);
        }
    }
    keys
}

/// The conventional foreign key pointing at `M`: `User` → `user_id`.
///
/// The classic convention, and every relationship lets a caller override it.
fn conventional_foreign_key<M: Model>() -> String {
    format!("{}_{}", str::snake(M::model_name()), M::primary_key())
}

// ---------------------------------------------------------------------------
// has_many
// ---------------------------------------------------------------------------

/// One parent, many children.
///
/// ```ignore
/// // A user has many posts, found by `posts.user_id`.
/// HasMany::<User, Post>::new()
/// ```
pub struct HasMany<P, C> {
    foreign_key: String,
    local_key: String,
    criteria: Criteria,
    _sides: PhantomData<fn() -> (P, C)>,
}

impl<P: Model, C: Model> HasMany<P, C> {
    /// With the conventional keys: `posts.user_id` matched against `users.id`.
    pub fn new() -> Self {
        Self {
            foreign_key: conventional_foreign_key::<P>(),
            local_key: P::primary_key().to_string(),
            criteria: Criteria::new(),
            _sides: PhantomData,
        }
    }

    /// The column on the child pointing back at the parent.
    pub fn foreign_key(mut self, column: impl Into<String>) -> Self {
        self.foreign_key = column.into();
        self
    }

    /// The column on the parent the child points at. Defaults to its key.
    pub fn local_key(mut self, column: impl Into<String>) -> Self {
        self.local_key = column.into();
        self
    }

    /// Narrow, order or limit the children — a constrained eager load.
    ///
    /// A `limit` here limits the **whole** query, not each parent's share: one
    /// query cannot take the newest three per parent without a window function,
    /// and pretending otherwise would silently drop rows.
    pub fn matching(mut self, criteria: Criteria) -> Self {
        self.criteria = criteria;
        self
    }
}

impl<P: Model, C: Model> Default for HasMany<P, C> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl<P: Model, C: Model> Relation<P> for HasMany<P, C> {
    type Related = C;

    fn near_key(&self) -> &str {
        &self.local_key
    }

    fn far_key(&self) -> &str {
        &self.foreign_key
    }

    async fn load(&self, parents: &[P], related: &dyn Repository<C>) -> Result<Related<C>> {
        let keys = keys_of(parents, &self.local_key);
        if keys.is_empty() {
            return Ok(Related::empty(&self.local_key));
        }

        let rows = related.matching(self.scoped(keys)).await?;
        Ok(Related::group_by(&self.local_key, &self.foreign_key, rows))
    }

    async fn count(&self, parents: &[P], related: &dyn Repository<C>) -> Result<RelatedCounts> {
        let keys = keys_of(parents, &self.local_key);
        if keys.is_empty() {
            return Ok(RelatedCounts::default());
        }

        let counts = related.count_grouped(&self.foreign_key, self.scoped(keys)).await?;
        Ok(RelatedCounts { near_key: self.local_key.clone(), counts: counts.into_iter().collect() })
    }
}

impl<P: Model, C: Model> HasMany<P, C> {
    fn scoped(&self, keys: Vec<Value>) -> Criteria {
        self.criteria.clone().where_in(&self.foreign_key, keys)
    }
}

// ---------------------------------------------------------------------------
// has_one
// ---------------------------------------------------------------------------

/// One parent, one child.
///
/// The same query as [`HasMany`]; the difference is that you read it with
/// [`Related::one`]. A second matching row is not an error here, because the
/// database is where uniqueness belongs: put a unique index on the foreign key
/// and the question cannot arise.
pub struct HasOne<P, C>(HasMany<P, C>);

impl<P: Model, C: Model> HasOne<P, C> {
    /// With the conventional keys.
    pub fn new() -> Self {
        Self(HasMany::new())
    }

    /// The column on the child pointing back at the parent.
    pub fn foreign_key(mut self, column: impl Into<String>) -> Self {
        self.0 = self.0.foreign_key(column);
        self
    }

    /// The column on the parent the child points at.
    pub fn local_key(mut self, column: impl Into<String>) -> Self {
        self.0 = self.0.local_key(column);
        self
    }

    /// Narrow or order the children.
    pub fn matching(mut self, criteria: Criteria) -> Self {
        self.0 = self.0.matching(criteria);
        self
    }
}

impl<P: Model, C: Model> Default for HasOne<P, C> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl<P: Model, C: Model> Relation<P> for HasOne<P, C> {
    type Related = C;

    fn near_key(&self) -> &str {
        self.0.near_key()
    }

    fn far_key(&self) -> &str {
        self.0.far_key()
    }

    async fn load(&self, parents: &[P], related: &dyn Repository<C>) -> Result<Related<C>> {
        self.0.load(parents, related).await
    }

    async fn count(&self, parents: &[P], related: &dyn Repository<C>) -> Result<RelatedCounts> {
        self.0.count(parents, related).await
    }
}

// ---------------------------------------------------------------------------
// belongs_to
// ---------------------------------------------------------------------------

/// The inverse — the child pointing back at its parent.
///
/// ```ignore
/// // A post belongs to a user, through `posts.user_id`.
/// BelongsTo::<Post, User>::new()
/// ```
///
/// The near side is the **child** here, so [`Related::one`] takes the post and
/// gives you its author.
pub struct BelongsTo<C, P> {
    foreign_key: String,
    owner_key: String,
    criteria: Criteria,
    _sides: PhantomData<fn() -> (C, P)>,
}

impl<C: Model, P: Model> BelongsTo<C, P> {
    /// With the conventional keys: `posts.user_id` matched against `users.id`.
    pub fn new() -> Self {
        Self {
            foreign_key: conventional_foreign_key::<P>(),
            owner_key: P::primary_key().to_string(),
            criteria: Criteria::new(),
            _sides: PhantomData,
        }
    }

    /// The column on this model that points at the owner.
    pub fn foreign_key(mut self, column: impl Into<String>) -> Self {
        self.foreign_key = column.into();
        self
    }

    /// The column on the owner being pointed at. Defaults to its key.
    pub fn owner_key(mut self, column: impl Into<String>) -> Self {
        self.owner_key = column.into();
        self
    }

    /// Narrow the owners — rare, but a soft-delete scope belongs here.
    pub fn matching(mut self, criteria: Criteria) -> Self {
        self.criteria = criteria;
        self
    }
}

impl<C: Model, P: Model> Default for BelongsTo<C, P> {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl<C: Model, P: Model> Relation<C> for BelongsTo<C, P> {
    type Related = P;

    fn near_key(&self) -> &str {
        &self.foreign_key
    }

    fn far_key(&self) -> &str {
        &self.owner_key
    }

    async fn load(&self, children: &[C], related: &dyn Repository<P>) -> Result<Related<P>> {
        let keys = keys_of(children, &self.foreign_key);
        if keys.is_empty() {
            return Ok(Related::empty(&self.foreign_key));
        }

        let criteria = self.criteria.clone().where_in(&self.owner_key, keys);
        let rows = related.matching(criteria).await?;

        Ok(Related::group_by(&self.foreign_key, &self.owner_key, rows))
    }

    async fn count(&self, children: &[C], related: &dyn Repository<P>) -> Result<RelatedCounts> {
        let keys = keys_of(children, &self.foreign_key);
        if keys.is_empty() {
            return Ok(RelatedCounts::default());
        }

        let criteria = self.criteria.clone().where_in(&self.owner_key, keys);
        let counts = related.count_grouped(&self.owner_key, criteria).await?;

        Ok(RelatedCounts {
            near_key: self.foreign_key.clone(),
            counts: counts.into_iter().collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// belongs_to_many
// ---------------------------------------------------------------------------

/// Many to many, through a pivot table.
///
/// ```ignore
/// // posts ↔ tags, through `post_tag(post_id, tag_id)`.
/// BelongsToMany::<Post, Tag>::new("post_tag")
/// ```
///
/// **Two** queries, not one: the pivot rows, then the related rows. There is no
/// third for the parents, and no per-parent query — the count does not grow
/// with the number of parents, which is the property that matters.
///
/// The pivot needs no model and no `Entity`. It is read as two columns, which
/// is all a pivot is; a pivot carrying its own data (`role`, `created_at`) is a
/// model in its own right, and two `has_many`s through it say so more clearly
/// than a pivot with attributes.
pub struct BelongsToMany<P, C> {
    pivot: String,
    parent_pivot_key: String,
    related_pivot_key: String,
    parent_key: String,
    related_key: String,
    criteria: Criteria,
    _sides: PhantomData<fn() -> (P, C)>,
}

impl<P: Model, C: Model> BelongsToMany<P, C> {
    /// Through `pivot`, with the conventional column names —
    /// `post_id` and `tag_id`.
    pub fn new(pivot: impl Into<String>) -> Self {
        Self {
            pivot: pivot.into(),
            parent_pivot_key: conventional_foreign_key::<P>(),
            related_pivot_key: conventional_foreign_key::<C>(),
            parent_key: P::primary_key().to_string(),
            related_key: C::primary_key().to_string(),
            criteria: Criteria::new(),
            _sides: PhantomData,
        }
    }

    /// The pivot's conventional name: the two tables' singulars, in
    /// alphabetical order — `post_tag`.
    pub fn conventional_pivot() -> String {
        let mut sides =
            [str::snake(P::model_name()), str::snake(C::model_name())].map(|s| str::singular(&s));
        sides.sort();
        sides.join("_")
    }

    /// The pivot column pointing at the parent.
    pub fn parent_pivot_key(mut self, column: impl Into<String>) -> Self {
        self.parent_pivot_key = column.into();
        self
    }

    /// The pivot column pointing at the related model.
    pub fn related_pivot_key(mut self, column: impl Into<String>) -> Self {
        self.related_pivot_key = column.into();
        self
    }

    /// The parent column the pivot points at. Defaults to its key.
    pub fn parent_key(mut self, column: impl Into<String>) -> Self {
        self.parent_key = column.into();
        self
    }

    /// The related column the pivot points at. Defaults to its key.
    pub fn related_key(mut self, column: impl Into<String>) -> Self {
        self.related_key = column.into();
        self
    }

    /// Narrow or order the related rows.
    pub fn matching(mut self, criteria: Criteria) -> Self {
        self.criteria = criteria;
        self
    }

    /// The pivot table's name.
    pub fn pivot(&self) -> &str {
        &self.pivot
    }
}

#[async_trait::async_trait]
impl<P: Model, C: Model> Relation<P> for BelongsToMany<P, C> {
    type Related = C;

    fn near_key(&self) -> &str {
        &self.parent_key
    }

    fn far_key(&self) -> &str {
        &self.related_key
    }

    async fn load(&self, parents: &[P], related: &dyn Repository<C>) -> Result<Related<C>> {
        let keys = keys_of(parents, &self.parent_key);
        if keys.is_empty() {
            return Ok(Related::empty(&self.parent_key));
        }

        let links = related.pivot_links(self.links_for(keys)).await?;
        if links.is_empty() {
            return Ok(Related::empty(&self.parent_key));
        }

        // The related rows, once, however many parents point at each.
        let wanted: Vec<Value> = dedup(links.iter().map(|(_, related)| related.clone()));
        let criteria = self.criteria.clone().where_in(&self.related_key, wanted);
        let rows = related.matching(criteria).await?;

        // Index them by their own key, then fan out along the pivot: a row
        // linked from three parents appears under all three.
        let by_key: HashMap<RelationKey, C> = rows
            .into_iter()
            .filter_map(|row| {
                row.value_of(&self.related_key).map(|key| (RelationKey::new(&key), row))
            })
            .collect();

        let mut groups: HashMap<RelationKey, Vec<C>> = HashMap::new();
        for (parent, related) in &links {
            if let Some(row) = by_key.get(&RelationKey::new(related)) {
                groups.entry(RelationKey::new(parent)).or_default().push(row.clone());
            }
        }

        Ok(Related { near_key: self.parent_key.clone(), groups })
    }

    async fn count(&self, parents: &[P], related: &dyn Repository<C>) -> Result<RelatedCounts> {
        let keys = keys_of(parents, &self.parent_key);
        if keys.is_empty() {
            return Ok(RelatedCounts::default());
        }

        // Counted from the pivot, so no related rows cross the wire. It counts
        // links rather than rows, which differs only if the pivot has
        // duplicates or a `matching` filter would have excluded some.
        let links = related.pivot_links(self.links_for(keys)).await?;

        let mut counts: HashMap<RelationKey, u64> = HashMap::new();
        for (parent, _) in &links {
            *counts.entry(RelationKey::new(parent)).or_insert(0) += 1;
        }

        Ok(RelatedCounts { near_key: self.parent_key.clone(), counts })
    }
}

impl<P: Model, C: Model> BelongsToMany<P, C> {
    fn links_for(&self, keys: Vec<Value>) -> PivotQuery {
        PivotQuery {
            table: self.pivot.clone(),
            parent_column: self.parent_pivot_key.clone(),
            related_column: self.related_pivot_key.clone(),
            parent_keys: keys,
        }
    }
}

/// A read of a pivot table: the links from these parents.
///
/// A pivot is two columns, so it needs no entity. This is the shape a
/// repository is asked for them in.
#[derive(Debug, Clone)]
pub struct PivotQuery {
    /// The pivot table.
    pub table: String,
    /// Its column pointing at the parent.
    pub parent_column: String,
    /// Its column pointing at the related model.
    pub related_column: String,
    /// The parent keys to fetch links for.
    pub parent_keys: Vec<Value>,
}

fn dedup(values: impl Iterator<Item = Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for value in values {
        if seen.insert(RelationKey::new(&value)) {
            unique.push(value);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_database, MemoryConnection};
    use crate::{Dialect, EntityRepository};
    use rainier_orm::Entity;

    #[derive(Debug, Clone, PartialEq, Entity)]
    #[orm(table = "users")]
    struct User {
        #[orm(pk, auto_increment)]
        id: u64,
        name: String,
    }

    impl Model for User {}

    #[derive(Debug, Clone, PartialEq, Entity)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
        title: String,
        user_id: u64,
    }

    impl Model for Post {}

    #[derive(Debug, Clone, PartialEq, Entity)]
    #[orm(table = "tags")]
    struct Tag {
        #[orm(pk, auto_increment)]
        id: u64,
        name: String,
    }

    impl Model for Tag {}

    fn user(id: u64) -> User {
        User { id, name: format!("user-{id}") }
    }

    fn post(id: u64, user_id: u64) -> Post {
        Post { id, title: format!("post-{id}"), user_id }
    }

    #[test]
    fn the_conventional_keys_come_from_the_model_names() {
        let has_many = HasMany::<User, Post>::new();
        assert_eq!(has_many.far_key(), "user_id");
        assert_eq!(has_many.near_key(), "id");

        let belongs_to = BelongsTo::<Post, User>::new();
        assert_eq!(belongs_to.near_key(), "user_id", "the column on the post");
        assert_eq!(belongs_to.far_key(), "id", "the column on the user");

        assert_eq!(BelongsToMany::<Post, Tag>::conventional_pivot(), "post_tag");
    }

    #[test]
    fn keys_are_normalised_across_integer_widths() {
        // The parent's `id: u64` and the child's `user_id: u64` can come back
        // from different drivers as different widths.
        assert_eq!(
            RelationKey::new(&Value::Int(Some(7))),
            RelationKey::new(&Value::BigUnsigned(Some(7)))
        );
        assert_ne!(RelationKey::new(&Value::Int(Some(7))), RelationKey::new(&Value::Int(Some(8))));

        // A NULL foreign key must not join to a NULL primary key.
        let null_key = RelationKey::new(&Value::BigUnsigned(None));
        assert_ne!(null_key, RelationKey::new(&Value::BigUnsigned(Some(0))));
    }

    #[test]
    fn related_rows_are_grouped_by_the_foreign_key() {
        let related = Related::group_by("id", "user_id", vec![post(1, 7), post(2, 7), post(3, 8)]);

        assert_eq!(related.of(&user(7)).len(), 2);
        assert_eq!(related.of(&user(8)).len(), 1);
        assert_eq!(related.of(&user(9)), &[], "a parent with no children");
        assert_eq!(related.one(&user(8)).unwrap().id, 3);
        assert_eq!(related.len(), 3);
    }

    #[test]
    fn zipping_pairs_each_parent_with_its_own() {
        let related = Related::group_by("id", "user_id", vec![post(1, 7), post(2, 8), post(3, 7)]);
        let pairs = related.zip(vec![user(7), user(8), user(9)]);

        assert_eq!(pairs[0].1.len(), 2);
        assert_eq!(pairs[1].1.len(), 1);
        assert!(pairs[2].1.is_empty(), "still present, with nothing");
    }

    #[test]
    fn duplicate_parent_keys_are_asked_for_once() {
        // Ten posts by one author must not put that author in the `IN` clause
        // ten times.
        let posts = vec![post(1, 7), post(2, 7), post(3, 8)];
        assert_eq!(keys_of(&posts, "user_id").len(), 2);
    }

    #[tokio::test]
    async fn loading_many_parents_is_one_query() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let posts = EntityRepository::<Post>::new(db);

        let users: Vec<User> = (1..=50).map(user).collect();
        HasMany::<User, Post>::new().load(&users, &posts).await.unwrap();

        assert_eq!(connection.statement_count(), 1, "fifty parents, one query");
        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("IN"), "{sql}");
    }

    #[tokio::test]
    async fn loading_no_parents_asks_nothing() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let posts = EntityRepository::<Post>::new(db);

        let loaded = HasMany::<User, Post>::new().load(&[], &posts).await.unwrap();

        assert_eq!(connection.statement_count(), 0, "an empty IN () is not a query worth running");
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn a_constrained_load_keeps_its_filters() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let posts = EntityRepository::<Post>::new(db);

        HasMany::<User, Post>::new()
            .matching(Criteria::new().order_by_desc("id"))
            .load(&[user(1)], &posts)
            .await
            .unwrap();

        let sql = connection.last_statement().unwrap();
        assert!(sql.contains("ORDER BY"), "{sql}");
    }

    #[tokio::test]
    async fn belongs_to_many_reads_the_pivot_then_the_rows() {
        let (db, connection) = fake_database(MemoryConnection::new(Dialect::Sqlite));
        let tags = EntityRepository::<Tag>::new(db);

        BelongsToMany::<Post, Tag>::new("post_tag")
            .load(&[post(1, 7), post(2, 7)], &tags)
            .await
            .unwrap();

        // The pivot came back empty from the fake, so there is nothing to
        // fetch — the second query is skipped rather than asking for `IN ()`.
        assert_eq!(connection.statement_count(), 1);
        assert!(connection.last_statement().unwrap().contains("post_tag"));
    }
}
