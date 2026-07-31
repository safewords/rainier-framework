//! Route-model binding — an action parameter that is the looked-up model
//! rather than its id.
//!
//! ```ignore
//! // routes/api.rs
//! router.get("/posts/{post}", show).where_slug("post");
//!
//! // app/http/controllers/post_controller.rs
//! pub async fn show(Bound(post): Bound<Post>) -> Result<Response> {
//!     Ok(Response::json(&post))
//! }
//! ```
//!
//! The action receives the **model**, not the id: the lookup, the 404 and the
//! `?` are all gone from the body, and what is left is the thing the action is
//! actually about.
//!
//! # Why it lives here
//!
//! It needs a request, a container and a repository at once, and
//! [`rainier-database`](rainier_database) is not allowed to know an HTTP
//! request exists — that rule is what keeps a repository usable from a queue
//! worker with no router in it. This crate is the one that already has both
//! halves, the same reason
//! [`DatabaseChannel`](crate::notifications::DatabaseChannel) is here.

use std::marker::PhantomData;
use std::sync::Arc;

use rainier_database::{Database, EntityRepository, Model, Repository};
use rainier_http::{FromRequest, Request};
use rainier_support::{str::snake, BoxedFuture, Error, Result};

/// A model resolved from a route parameter.
///
/// Looks the row up by the model's
/// [route key](rainier_database::Model::route_key_name) — the primary key
/// unless the model says otherwise, which is how `/posts/{post}` binds by slug
/// while `/users/{user}` binds by id.
///
/// # Which parameter
///
/// The one named after the model, lower-cased: `Post` reads `{post}`. A
/// convention worth keeping to — a route whose parameter is named
/// something else should say so with [`BoundAs`], not leave the reader
/// guessing which of two parameters was meant.
///
/// # Failures
///
/// - no such parameter on the route → a `500`, because that is a wiring
///   mistake rather than anything the caller did;
/// - no such row → a `404` naming the model, exactly as
///   [`find_or_fail`](rainier_database::Repository::find_or_fail) does.
///
/// # It does not authorise
///
/// Binding finds the row; it does not ask whether this caller may have it.
/// A draft post binds happily for a stranger. Follow it with a
/// [policy](rainier_auth::Gate) — or, better, with a
/// [request contract](rainier_validation::FormRequest) that authorises before
/// the action runs at all.
pub struct Bound<M: Model>(pub M);

impl<M: Model> Bound<M> {
    /// The model.
    pub fn into_inner(self) -> M {
        self.0
    }
}

impl<M: Model> std::ops::Deref for Bound<M> {
    type Target = M;

    fn deref(&self) -> &M {
        &self.0
    }
}

impl<M: Model> FromRequest for Bound<M> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move { resolve::<M>(&request, &snake(M::model_name())).await.map(Bound) })
    }
}

/// A model resolved from a **named** route parameter.
///
/// For the route with two of them, where the convention cannot say which:
///
/// ```ignore
/// router.get("/posts/{post}/comments/{comment}", show);
///
/// pub async fn show(
///     post: BoundAs<Post, PostParam>,
///     comment: BoundAs<Comment, CommentParam>,
/// ) -> Result<Response> {
///     Ok(Response::json(&comment.into_inner()))
/// }
/// ```
///
/// The parameter name is a type, because an extractor's configuration has to
/// live in its type — there is nowhere else for it to come from. Declare one
/// with [`param_key!`](crate::param_key).
pub struct BoundAs<M: Model, K: ParamKey>(pub M, PhantomData<fn() -> K>);

impl<M: Model, K: ParamKey> BoundAs<M, K> {
    /// The model.
    pub fn into_inner(self) -> M {
        self.0
    }
}

impl<M: Model, K: ParamKey> std::ops::Deref for BoundAs<M, K> {
    type Target = M;

    fn deref(&self) -> &M {
        &self.0
    }
}

impl<M: Model, K: ParamKey> FromRequest for BoundAs<M, K> {
    fn from_request(request: Arc<Request>) -> BoxedFuture<Result<Self>> {
        Box::pin(async move {
            resolve::<M>(&request, K::NAME).await.map(|model| BoundAs(model, PhantomData))
        })
    }
}

/// The name of a route parameter, as a type. See [`param_key!`](crate::param_key).
pub trait ParamKey: Send + Sync + 'static {
    /// The parameter's name.
    const NAME: &'static str;
}

/// Declare a route-parameter name for [`BoundAs`].
///
/// ```ignore
/// param_key!(pub AuthorParam = "author");
///
/// pub async fn show(author: BoundAs<User, AuthorParam>) -> Result<Response> {
///     Ok(Response::json(&author.into_inner()))
/// }
/// ```
#[macro_export]
macro_rules! param_key {
    ($(#[$meta:meta])* $vis:vis $name:ident = $param:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy)]
        $vis struct $name;

        impl $crate::binding::ParamKey for $name {
            const NAME: &'static str = $param;
        }
    };
}

/// Look one model up by the route parameter `name`.
async fn resolve<M: Model>(request: &Request, name: &str) -> Result<M> {
    let Some(value) = request.route_param(name) else {
        // Not the caller's doing: the route does not have this parameter, or
        // it is spelled differently. Saying which is the difference between a
        // one-minute fix and an afternoon.
        return Err(Error::internal(format!(
            "the route has no `{{{name}}}` parameter to bind a {} from",
            M::model_name()
        )));
    };

    let database = rainier_container::facade_application().resolve::<Database>()?;
    let repository = EntityRepository::<M>::new((*database).clone());

    repository
        .first_by(M::route_key_name(), value.into())
        .await?
        .ok_or_else(|| Error::not_found(format!("No {} matches the given key.", M::model_name())))
}

#[cfg(test)]
mod tests {
    use super::*;

    param_key!(
        /// The `author` parameter.
        pub AuthorParam = "author"
    );

    #[test]
    fn a_param_key_carries_its_name() {
        assert_eq!(AuthorParam::NAME, "author");
    }

    #[test]
    fn the_conventional_parameter_is_the_model_name_lower_cased() {
        // `Post` reads `{post}`, which is the convention a reader assumes.
        assert_eq!(snake("Post"), "post");
        assert_eq!(snake("BlogPost"), "blog_post");
    }

    #[tokio::test]
    async fn a_route_with_no_such_parameter_is_a_wiring_error_not_a_404() {
        #[derive(Debug, Clone, PartialEq, rainier_orm::Entity)]
        #[orm(table = "posts")]
        struct Post {
            #[orm(pk)]
            id: u64,
        }
        impl Model for Post {}

        let request = Request::builder().build();
        let err = resolve::<Post>(&request, "post").await.unwrap_err();

        assert_eq!(err.status(), 500, "{}", err.message());
        assert!(err.message().contains("{post}"), "{}", err.message());
        assert!(err.message().contains("Post"), "{}", err.message());
    }
}
