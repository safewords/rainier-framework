//! Test fixtures — [`Factory`] and [`HasFactory`].
//!
//! ```ignore
//! #[derive(Entity, Default, Clone, Factory)]
//! struct User { id: u64, email: String, verified_at: Option<DateTime<Utc>> }
//!
//! let users = User::factory()
//!     .count(3)
//!     .sequence(|user, i| user.email = format!("user{i}@example.com"))
//!     .state(|user| user.verified_at = Some(Utc::now()))
//!     .create(&*users)
//!     .await?;
//! ```
//!
//! [`TestApp`](https://docs.rs/rainier-framework) solved booting an
//! application. This is the other half: getting rows into it without every
//! test constructing them field by field, which is why a suite's setup ends up
//! longer than its assertions.
//!
//! # What a factory is for, and what it is not
//!
//! It builds a row that is **valid and uninteresting**. Every field a test
//! does not care about gets a value that will not trip a constraint, and the
//! one or two fields the test *is* about are set with [`state`](Factory::state).
//!
//! That is the whole point: a test that spells out fifteen fields to assert on
//! one of them has buried its subject. A reader cannot tell which value
//! matters, and neither can the next person to change the schema.
//!
//! # Unique columns need a sequence
//!
//! A derived factory builds from [`Default`], and three defaults are three
//! identical rows — which a `UNIQUE` index refuses on the second. Anything
//! unique needs [`sequence`](Factory::sequence), which is handed the index:
//!
//! ```ignore
//! .sequence(|user, i| user.email = format!("user{i}@example.com"))
//! ```
//!
//! Deliberately not automatic. A factory that invented unique values would
//! have to guess which columns are unique and what shape they take, and a
//! guess that is wrong produces a row that fails to insert for a reason nobody
//! can see from the test.

use std::sync::Arc;

use rainier_support::Result;

use crate::model::Model;
use crate::repository::Repository;

/// Sets one aspect of a model being built.
type State<M> = Arc<dyn Fn(&mut M, usize) + Send + Sync>;

/// Builds models for a test.
pub struct Factory<M> {
    make: Arc<dyn Fn(usize) -> M + Send + Sync>,
    states: Vec<State<M>>,
    count: usize,
}

impl<M> Factory<M> {
    /// Build each model with `make`, which is handed the row's index.
    pub fn new(make: impl Fn(usize) -> M + Send + Sync + 'static) -> Self {
        Self { make: Arc::new(make), states: Vec::new(), count: 1 }
    }

    /// How many to build.
    #[must_use = "this returns a configured factory rather than configuring in place"]
    pub fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    /// Adjust every model this factory builds.
    ///
    /// The field the test is actually about. Applied in the order added, after
    /// the base model is built.
    #[must_use = "this returns a configured factory rather than configuring in place"]
    pub fn state(self, state: impl Fn(&mut M) + Send + Sync + 'static) -> Self {
        self.sequence(move |model, _| state(model))
    }

    /// Adjust each model, knowing which one it is.
    ///
    /// For anything that has to differ between rows — a unique email, an
    /// ordering, a timestamp that has to be distinct.
    #[must_use = "this returns a configured factory rather than configuring in place"]
    pub fn sequence(mut self, state: impl Fn(&mut M, usize) + Send + Sync + 'static) -> Self {
        self.states.push(Arc::new(state));
        self
    }

    /// Build them, without touching a database.
    ///
    /// For a test about a pure function, a serialiser or a policy — none of
    /// which need a row to exist anywhere.
    pub fn make(&self) -> Vec<M> {
        (0..self.count)
            .map(|index| {
                let mut model = (self.make)(index);
                for state in &self.states {
                    state(&mut model, index);
                }
                model
            })
            .collect()
    }

    /// Build exactly one.
    ///
    /// # Panics
    ///
    /// Never — [`count`](Self::count) is ignored and one is built.
    pub fn make_one(&self) -> M {
        let mut model = (self.make)(0);
        for state in &self.states {
            state(&mut model, 0);
        }
        model
    }
}

impl<M: Model> Factory<M> {
    /// Build them and insert them.
    ///
    /// Returns what the repository returned — so a database-assigned key is on
    /// the model a test goes on to use, rather than the zero it was built
    /// with.
    pub async fn create(&self, repository: &dyn Repository<M>) -> Result<Vec<M>> {
        let mut created = Vec::with_capacity(self.count);

        // Sequentially, and deliberately. Concurrent inserts against one
        // connection interleave in whatever order the pool feels like, which
        // makes a test that asserts on ordering fail once a fortnight.
        for model in self.make() {
            created.push(repository.create(model).await?);
        }

        Ok(created)
    }

    /// Build one and insert it.
    pub async fn create_one(&self, repository: &dyn Repository<M>) -> Result<M> {
        repository.create(self.make_one()).await
    }
}

impl<M> Clone for Factory<M> {
    fn clone(&self) -> Self {
        Self { make: Arc::clone(&self.make), states: self.states.clone(), count: self.count }
    }
}

impl<M> std::fmt::Debug for Factory<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Factory")
            .field("count", &self.count)
            .field("states", &self.states.len())
            .finish()
    }
}

/// A model with a factory.
///
/// Implemented by `#[derive(Factory)]`, which needs [`Default`] — or by hand,
/// when the default row is not a sensible starting point:
///
/// ```ignore
/// impl HasFactory for User {
///     fn factory() -> Factory<Self> {
///         Factory::new(|i| User {
///             id: 0,
///             email: format!("user{i}@example.com"),
///             verified_at: None,
///         })
///     }
/// }
/// ```
///
/// Writing it by hand is often the better answer for a model with unique
/// columns: it puts the sequence in one place instead of in every test.
pub trait HasFactory: Sized {
    /// A factory building one of these.
    fn factory() -> Factory<Self>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Default, PartialEq)]
    struct User {
        id: u64,
        email: String,
        admin: bool,
    }

    impl HasFactory for User {
        fn factory() -> Factory<Self> {
            Factory::new(|_| User::default())
        }
    }

    #[test]
    fn one_by_default() {
        let users = User::factory().make();

        assert_eq!(users.len(), 1);
        assert_eq!(users[0], User::default());
    }

    #[test]
    fn count_builds_that_many() {
        assert_eq!(User::factory().count(3).make().len(), 3);
        assert_eq!(User::factory().count(0).make().len(), 0);
    }

    #[test]
    fn state_applies_to_every_one() {
        let users = User::factory().count(3).state(|user| user.admin = true).make();

        assert!(users.iter().all(|user| user.admin));
    }

    #[test]
    fn a_sequence_knows_which_row_it_is() {
        // The answer to a UNIQUE index, and the reason `state` is not enough.
        let users = User::factory()
            .count(3)
            .sequence(|user, i| user.email = format!("user{i}@example.com"))
            .make();

        assert_eq!(users[0].email, "user0@example.com");
        assert_eq!(users[2].email, "user2@example.com");

        let emails: std::collections::HashSet<_> = users.iter().map(|user| &user.email).collect();
        assert_eq!(emails.len(), 3, "a unique column needs distinct values");
    }

    #[test]
    fn states_apply_in_the_order_they_were_added() {
        // So a later one can override an earlier one, which is how a shared
        // base factory gets specialised by one test.
        let users = User::factory()
            .state(|user| user.email = "first".into())
            .state(|user| user.email = "second".into())
            .make();

        assert_eq!(users[0].email, "second");
    }

    #[test]
    fn make_one_ignores_the_count() {
        let user = User::factory().count(5).state(|user| user.admin = true).make_one();

        assert!(user.admin);
    }

    #[test]
    fn a_factory_can_be_cloned_and_specialised() {
        // The pattern a test suite converges on: one base factory, several
        // narrower ones.
        let base = User::factory().count(2);

        let admins = base.clone().state(|user| user.admin = true).make();
        let ordinary = base.make();

        assert!(admins.iter().all(|user| user.admin));
        assert!(!ordinary.iter().any(|user| user.admin));
    }

    #[test]
    fn a_hand_written_factory_can_do_what_a_derived_one_cannot() {
        struct Post {
            slug: String,
        }

        impl HasFactory for Post {
            fn factory() -> Factory<Self> {
                Factory::new(|i| Post { slug: format!("post-{i}") })
            }
        }

        let posts = Post::factory().count(2).make();

        assert_eq!(posts[0].slug, "post-0");
        assert_eq!(posts[1].slug, "post-1");
    }
}
