//! The [`Model`] contract and its lifecycle **hooks**.
//!
//! A model is a Rainier ORM [`Entity`] with the extra facts the framework needs:
//! a display name for error messages, and the column route-model binding looks
//! up by. Declaring one is a single line, because everything else is derived:
//!
//! ```
//! use rainier_orm::Entity;
//! use rainier_database::Model;
//!
//! #[derive(Entity, Clone)]
//! #[orm(table = "posts")]
//! struct Post {
//!     #[orm(pk, auto_increment)]
//!     id: u64,
//!     #[orm(unique)]
//!     slug: String,
//!     title: String,
//! }
//!
//! impl Model for Post {
//!     // Bind `/posts/{post}` by slug rather than by id.
//!     fn route_key_name() -> &'static str { "slug" }
//! }
//! ```
//!
//! ## Lifecycle hooks
//!
//! Every repository write dispatches events through
//! [`rainier_events`](rainier_events::Dispatcher), which is how the model
//! lifecycle hooks — creating, created, saved, deleted — are spelled here. Each is a
//! distinct type, so a listener registers for exactly the moment it cares
//! about rather than matching on a discriminant:
//!
//! ```
//! # use rainier_orm::Entity;
//! # use rainier_database::{Created, Model};
//! # use rainier_events::Dispatcher;
//! # use std::sync::Arc;
//! # #[derive(Entity, Clone)]
//! # struct Post { #[orm(pk)] id: u64, title: String }
//! # impl Model for Post {}
//! # fn wire(events: &Dispatcher) {
//! events.listen(|event: Arc<Created<Post>>| async move {
//!     println!("created post {}", event.model.title);
//!     Ok(())
//! });
//! # }
//! ```
//!
//! ### What a hook can and cannot do
//!
//! A `-ing` hook (`Creating`, `Updating`, `Deleting`) runs **before** the write
//! and can **veto** it by returning `Err` — the repository propagates the error
//! and never touches the database. It cannot *modify* the model: listeners
//! receive a shared `Arc`, and letting several listeners mutate one row in an
//! unspecified order would make the outcome depend on registration order.
//! Derive values in the model's own constructor, or in the repository, where
//! there is exactly one place to look.
//!
//! A `-ed` hook (`Created`, `Updated`, `Deleted`) runs **after** a successful
//! write. Returning `Err` from one does not roll anything back; it only
//! surfaces to the caller.

use std::any::type_name;

use rainier_orm::{Entity, SingleKey};
use rainier_support::str::class_basename;

/// An entity the framework manages.
///
/// `Clone` is required because lifecycle hooks receive the model by value:
/// a repository hands a copy to the event bus and keeps the original for the
/// write. The clone is skipped entirely when nothing is listening.
///
/// [`SingleKey`] is required because this layer is single-key *throughout*, in
/// its types rather than by convention: [`Repository::find`] and
/// [`Repository::delete`] take one [`Value`], [`Deleting`]/[`Deleted`] carry one
/// key, and [`route_key_name`](Self::route_key_name) names the one column a URL
/// segment binds to. A composite-key entity has no honest answer for any of
/// them, so it is refused here — where the error names the model — rather than
/// at each of those call sites, or worse, in a `WHERE` built from the first key
/// column alone.
///
/// Composite-key tables are still fully usable through Rainier ORM itself:
/// [`repo::find_by_keys`](rainier_orm::repo::find_by_keys),
/// [`repo::update`](rainier_orm::repo::update),
/// [`repo::delete_by_keys`](rainier_orm::repo::delete_by_keys) and
/// [`repo::query`](rainier_orm::repo::query), plus this crate's
/// [`Criteria`](crate::Criteria)-driven statements.
///
/// [`Value`]: rainier_orm::sea_query::Value
/// [`Repository::find`]: crate::Repository::find
/// [`Repository::delete`]: crate::Repository::delete
pub trait Model: Entity + SingleKey + Clone + Send + Sync + 'static {
    /// The model's name, for error messages — `"Post"`.
    fn model_name() -> &'static str {
        class_basename(type_name::<Self>())
    }

    /// The column route-model binding looks a member up by.
    ///
    /// Defaults to the primary key; override to bind by slug or UUID.
    fn route_key_name() -> &'static str {
        Self::primary_key()
    }
}

/// Fired before a row is inserted. Returning `Err` cancels the insert.
#[derive(Debug, Clone)]
pub struct Creating<M> {
    /// The model about to be inserted.
    pub model: M,
}

/// Fired after a row is inserted.
#[derive(Debug, Clone)]
pub struct Created<M> {
    /// The inserted model, with its assigned key where the backend reported
    /// one.
    pub model: M,
    /// The key the database assigned, or `0` for an app-assigned key.
    pub id: i64,
}

/// Fired before a row is updated. Returning `Err` cancels the update.
#[derive(Debug, Clone)]
pub struct Updating<M> {
    /// The model about to be written.
    pub model: M,
}

/// Fired after a row is updated.
#[derive(Debug, Clone)]
pub struct Updated<M> {
    /// The model as written.
    pub model: M,
    /// How many rows the update touched.
    pub rows_affected: u64,
}

/// Fired before a row is deleted. Returning `Err` cancels the delete.
#[derive(Debug, Clone)]
pub struct Deleting<M> {
    /// The primary key about to be deleted.
    pub key: String,
    /// The model type, for listeners registered generically.
    pub model: std::marker::PhantomData<M>,
}

/// Fired after a row is deleted.
#[derive(Debug, Clone)]
pub struct Deleted<M> {
    /// The primary key that was deleted.
    pub key: String,
    /// How many rows the delete removed.
    pub rows_affected: u64,
    /// The model type, for listeners registered generically.
    pub model: std::marker::PhantomData<M>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "posts")]
    struct Post {
        #[orm(pk, auto_increment)]
        id: u64,
        #[orm(unique)]
        slug: String,
        title: String,
    }

    impl Model for Post {}

    #[derive(rainier_orm::Entity, Clone, Debug)]
    #[orm(table = "pages")]
    struct Page {
        #[orm(pk, auto_increment)]
        id: u64,
        #[orm(unique)]
        slug: String,
    }

    impl Model for Page {
        fn route_key_name() -> &'static str {
            "slug"
        }
    }

    #[test]
    fn the_model_name_is_the_type_name_without_its_module_path() {
        assert_eq!(Post::model_name(), "Post");
    }

    #[test]
    fn the_route_key_defaults_to_the_primary_key() {
        assert_eq!(Post::route_key_name(), "id");
        assert_eq!(Post::route_key_name(), Post::primary_key());
    }

    #[test]
    fn the_route_key_can_be_overridden() {
        assert_eq!(Page::route_key_name(), "slug");
        assert_eq!(Page::primary_key(), "id", "the primary key is unchanged");
    }

    #[test]
    fn the_entity_metadata_still_comes_from_the_derive() {
        assert_eq!(Post::table(), "posts");
        assert_eq!(Post::columns().len(), 3);
    }

    #[test]
    fn hook_events_carry_the_model() {
        let post = Post { id: 1, slug: "hi".into(), title: "Hi".into() };
        let creating = Creating { model: post.clone() };
        assert_eq!(creating.model.title, "Hi");

        let created = Created { model: post, id: 7 };
        assert_eq!(created.id, 7);
    }
}
