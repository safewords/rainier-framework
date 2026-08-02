//! ActiveRecord-style change tracking: load a row, mutate it **in place**, then
//! [`save`](Tracked::save) the diff.
//!
//! The repository layer ([`crate::repo`]) is deliberately stateless — free
//! functions over an [`Entity`] value. That's the right default, but it means a
//! "read, tweak a couple of fields, write it back" flow has to either re-send
//! every column ([`repo::update`]) or hand-list the changed
//! ones ([`Query::update`](crate::Query::update())). [`Tracked`] adds the missing
//! stateful convenience without changing the entity or the derive: it snapshots
//! a row's column values when wrapped, hands you the inner `E` through
//! `Deref`/`DerefMut` so you mutate fields exactly as you would the struct, and
//! on [`save`](Tracked::save) computes the dirty set and issues a single partial
//! `UPDATE` of **only** what changed (routed to the right shard by the primary
//! key, like every other write).
//!
//! ```ignore
//! use rainier_orm::{active::Tracked, repo};
//!
//! let mut user = Tracked::load::<_, _>(&db, user_id).await?.expect("exists");
//! user.email = "new@example.com".into();   // DerefMut → mutate the row
//! user.locale = "fr".into();
//! let changed = user.save(&db).await?;      // UPDATE users SET email=?, locale=? WHERE id=?
//! assert_eq!(changed, 1);
//! user.save(&db).await?;                     // no-op: nothing dirty since the last save
//! ```
//!
//! It tracks the columns [`Entity::update_values`] reports (every non-primary-key
//! column), so the primary key is never part of a diff — a `save` can't move a
//! row between shards, by construction.

use core::ops::{Deref, DerefMut};

use crate::{repo, Entity, Executor, Result, SingleKey};
use sea_query::Value;

/// A loaded [`Entity`] that remembers its original column values so
/// [`save`](Self::save) persists only what changed. Deref targets the inner
/// `E`, so reads and field mutations go straight through.
pub struct Tracked<E: Entity> {
    /// Snapshot of `update_values()` at load (or last save) — the diff baseline.
    baseline: Vec<(&'static str, Value)>,
    current: E,
}

impl<E: Entity> Tracked<E> {
    /// Wrap an in-hand entity; its current column values become the baseline.
    pub fn new(entity: E) -> Self {
        Self { baseline: entity.update_values(), current: entity }
    }

    /// Load `pk`'s row and wrap it for tracking. `None` when no row matches.
    ///
    /// One key value, so [`SingleKey`]. A composite-key row is loaded with
    /// [`repo::find_by_keys`] and wrapped with [`new`](Self::new) — tracking
    /// itself works for either, only this shorthand is single-key.
    pub async fn load<X, V>(exec: &X, pk: V) -> Result<Option<Self>>
    where
        E: SingleKey,
        X: Executor,
        V: Into<Value>,
    {
        Ok(repo::find_by_pk::<E, X, V>(exec, pk).await?.map(Self::new))
    }

    /// The columns whose value differs from the baseline (the dirty set), in
    /// the entity's declared column order.
    pub fn changes(&self) -> Vec<(&'static str, Value)> {
        let now = self.current.update_values();
        // `update_values()` is deterministic in column order, so the snapshot
        // and the current values line up positionally.
        now.into_iter()
            .zip(self.baseline.iter())
            .filter_map(|((col, val), (_, base))| (val != *base).then_some((col, val)))
            .collect()
    }

    /// Whether any tracked column has changed since load / last save.
    pub fn is_dirty(&self) -> bool {
        let now = self.current.update_values();
        now.iter().zip(self.baseline.iter()).any(|((_, val), (_, base))| val != base)
    }

    /// Persist the dirty columns with a single `UPDATE … WHERE pk = ?`, returning
    /// rows affected. A no-op returning `0` when nothing changed. On success the
    /// baseline is reset to the saved state, so a later `save` writes only newer
    /// changes (and an immediate re-`save` is a no-op).
    pub async fn save<X: Executor>(&mut self, exec: &X) -> Result<u64> {
        let changes = self.changes();
        if changes.is_empty() {
            return Ok(0);
        }
        // Every key column is constrained, not just the first. On a composite
        // key, filtering by one part would make this partial `UPDATE` land on
        // each sibling row sharing it — silently, since the affected count would
        // look plausible.
        //
        // The builder is created *and consumed* inside this block, so only the
        // already-rendered future survives to the await. A `Query<E>` holds
        // `Rc`, and one still in scope across the await would be captured by the
        // generated future and make `save` unusable from a spawned handler —
        // which `tests/send_futures.rs` catches at compile time.
        let update = {
            let mut query = repo::query::<E>();
            for (column, value) in E::primary_key_columns().iter().zip(self.current.pk_values()) {
                query = query.where_eq(column, value);
            }
            query.update(exec, changes)
        };

        let affected = update.await?;
        self.baseline = self.current.update_values();
        Ok(affected)
    }

    /// Borrow the inner entity (also available through `Deref`).
    pub fn get(&self) -> &E {
        &self.current
    }

    /// Unwrap, discarding tracking state.
    pub fn into_inner(self) -> E {
        self.current
    }
}

impl<E: Entity> Deref for Tracked<E> {
    type Target = E;
    fn deref(&self) -> &E {
        &self.current
    }
}

impl<E: Entity> DerefMut for Tracked<E> {
    fn deref_mut(&mut self) -> &mut E {
        &mut self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dialect, ExecOutcome, Row};
    use core::cell::RefCell;
    use sea_query::Value;

    #[derive(Debug, Clone, crate::Entity)]
    #[orm(table = "widgets")]
    struct Widget {
        #[orm(pk, auto_increment)]
        id: u64,
        name: String,
        count: i32,
    }

    /// Records every `execute` so a test can assert what `save` emitted; a flag
    /// makes it panic if touched (to prove `save` is a true no-op when clean).
    struct MockExec {
        calls: RefCell<Vec<(String, Vec<Value>)>>,
        forbid: bool,
    }

    impl MockExec {
        fn new() -> Self {
            Self { calls: RefCell::new(Vec::new()), forbid: false }
        }
        fn forbidding() -> Self {
            Self { calls: RefCell::new(Vec::new()), forbid: true }
        }
    }

    impl Executor for MockExec {
        fn dialect(&self) -> Dialect {
            Dialect::Sqlite
        }
        async fn fetch_all(&self, _sql: &str, _params: Vec<Value>) -> Result<Vec<Box<dyn Row>>> {
            Ok(Vec::new())
        }
        async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecOutcome> {
            if self.forbid {
                panic!("execute called on a clean save: {sql}");
            }
            self.calls.borrow_mut().push((sql.to_string(), params));
            Ok(ExecOutcome { rows_affected: 1, last_insert_id: 0 })
        }
    }

    #[test]
    fn clean_entity_has_no_changes() {
        let t = Tracked::new(Widget { id: 1, name: "a".into(), count: 0 });
        assert!(!t.is_dirty());
        assert!(t.changes().is_empty());
    }

    #[test]
    fn mutation_is_detected() {
        let mut t = Tracked::new(Widget { id: 1, name: "a".into(), count: 0 });
        t.name = "b".into();
        assert!(t.is_dirty());
        let changes = t.changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, "name");
        // The primary key is never in the diff.
        assert!(!t.changes().iter().any(|(c, _)| *c == "id"));
    }

    #[tokio::test]
    async fn save_when_clean_is_a_noop() {
        let exec = MockExec::forbidding();
        let mut t = Tracked::new(Widget { id: 1, name: "a".into(), count: 0 });
        let n = t.save(&exec).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn save_emits_partial_update_then_clears_dirty() {
        let exec = MockExec::new();
        let mut t = Tracked::new(Widget { id: 7, name: "a".into(), count: 0 });
        t.name = "b".into();
        t.count = 3;

        let n = t.save(&exec).await.unwrap();
        assert_eq!(n, 1);

        let calls = exec.calls.borrow();
        assert_eq!(calls.len(), 1, "one UPDATE");
        let sql = calls[0].0.to_uppercase();
        assert!(sql.starts_with("UPDATE"), "{sql}");
        assert!(sql.contains("\"NAME\""), "{sql}");
        assert!(sql.contains("\"COUNT\""), "{sql}");
        assert!(sql.contains("WHERE"), "{sql}");
        drop(calls);

        // Baseline advanced → immediately clean again.
        assert!(!t.is_dirty());
    }
}
