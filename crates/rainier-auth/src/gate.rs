//! Authorization — the [`Gate`] and its abilities.
//!
//! Authentication asks *who are you*; authorization asks *may you do this*.
//! The split matters: a guard is about identity and a gate is about policy,
//! and mixing them produces controllers that check both by hand and get one
//! of them wrong.
//!
//! ```
//! # use rainier_auth::{Authenticatable, Gate};
//! # #[derive(Clone)] struct User { id: u64, admin: bool }
//! # impl Authenticatable for User {
//! #     fn auth_identifier(&self) -> String { self.id.to_string() }
//! #     fn auth_password_hash(&self) -> Option<&str> { None }
//! # }
//! # #[derive(Clone)] struct Post { author_id: u64 }
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let gate = Gate::<User>::new()
//!     .define("posts.update", |user: &User, post: Option<&Post>| {
//!         user.admin || post.is_some_and(|p| p.author_id == user.id)
//!     });
//!
//! let author = User { id: 1, admin: false };
//! let post = Post { author_id: 1 };
//!
//! assert!(gate.allows("posts.update", &author, Some(&post)));
//! gate.authorize("posts.update", &author, Some(&post))?;
//! # Ok(()) }
//! ```

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use rainier_support::{Error, Result};

/// Anything a gate can authorize.
///
/// Not [`Authenticatable`](crate::Authenticatable) — deliberately. A gate
/// answers *may this actor do this*, and in a service with machine callers the
/// actor is often not a person:
///
/// | The actor | Where it comes from |
/// |---|---|
/// | a user | a session or a bearer token |
/// | an API client | the client-credentials grant, where there is no user at all |
/// | a cloud principal | an assumed IAM role, an STS identity |
/// | a service account | a Kubernetes token, a signed internal request |
///
/// Requiring an actor to be authenticatable made two of those unrepresentable:
/// an API client has no password hash and no session, and inventing one so it
/// could be authorized is the kind of shape that ends in a machine identity
/// accidentally being able to log in.
///
/// There is nothing to implement. This is a blanket alias over the bounds a
/// gate genuinely needs — `Send + Sync + 'static` — so every existing
/// `Gate<User>` keeps working untouched and `Gate<ApiClient>` starts working
/// without `ApiClient` having to pretend to be a person.
pub trait Actor: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> Actor for T {}

/// A check registered against an ability name.
type Ability<A> = Arc<dyn Fn(&A, Option<&dyn Any>) -> bool + Send + Sync>;

/// A check that runs before every ability and may short-circuit it.
type BeforeCheck<A> = Arc<dyn Fn(&A, &str) -> Option<bool> + Send + Sync>;

/// A registry of authorization abilities.
///
/// Checks are **synchronous**. An authorization decision that needs a database
/// round-trip is a sign the subject should have been loaded already — the
/// controller has the record in hand by the time it authorizes, and re-fetching
/// it inside the check would double every query.
pub struct Gate<A: Actor> {
    abilities: HashMap<String, Ability<A>>,
    /// Consulted before any ability; a `Some` short-circuits the decision.
    before: Vec<BeforeCheck<A>>,
}

impl<A: Actor> Default for Gate<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Actor> Gate<A> {
    /// A gate with no abilities. Everything is denied until defined.
    pub fn new() -> Self {
        Self { abilities: HashMap::new(), before: Vec::new() }
    }

    /// Define an ability taking the actor and an optional subject.
    pub fn define<S, F>(mut self, ability: impl Into<String>, check: F) -> Self
    where
        S: 'static,
        F: Fn(&A, Option<&S>) -> bool + Send + Sync + 'static,
    {
        let erased: Ability<A> = Arc::new(move |user, subject| {
            match subject {
                // A subject of the wrong type is a programming error, not a
                // permission: deny, and let the caller notice.
                Some(subject) => match subject.downcast_ref::<S>() {
                    Some(typed) => check(user, Some(typed)),
                    None => false,
                },
                None => check(user, None),
            }
        });
        self.abilities.insert(ability.into(), erased);
        self
    }

    /// Define an ability that ignores its subject.
    pub fn define_simple<F>(self, ability: impl Into<String>, check: F) -> Self
    where
        F: Fn(&A) -> bool + Send + Sync + 'static,
    {
        self.define::<(), _>(ability, move |user, _| check(user))
    }

    /// Register a check that runs before every ability.
    ///
    /// Returning `Some(true)` grants regardless of the ability — the "an admin
    /// may do anything" rule, in one place instead of at the top of every
    /// check. `None` defers.
    pub fn before<F>(mut self, check: F) -> Self
    where
        F: Fn(&A, &str) -> Option<bool> + Send + Sync + 'static,
    {
        self.before.push(Arc::new(check));
        self
    }

    /// Whether `actor` may perform `ability` on `subject`.
    ///
    /// An **undefined** ability is denied. Defaulting to "allow" would mean a
    /// typo in an ability name silently opens a hole.
    pub fn allows<S: 'static>(&self, ability: &str, actor: &A, subject: Option<&S>) -> bool {
        for before in &self.before {
            if let Some(decision) = before(actor, ability) {
                return decision;
            }
        }

        match self.abilities.get(ability) {
            Some(check) => check(actor, subject.map(|s| s as &dyn Any)),
            None => false,
        }
    }

    /// The inverse of [`allows`](Self::allows).
    pub fn denies<S: 'static>(&self, ability: &str, actor: &A, subject: Option<&S>) -> bool {
        !self.allows(ability, actor, subject)
    }

    /// Whether `actor` may perform `ability`, with no subject.
    pub fn allows_any(&self, ability: &str, actor: &A) -> bool {
        self.allows::<()>(ability, actor, None)
    }

    /// Grant, or fail with a `403`.
    pub fn authorize<S: 'static>(
        &self,
        ability: &str,
        actor: &A,
        subject: Option<&S>,
    ) -> Result<()> {
        if self.allows(ability, actor, subject) {
            return Ok(());
        }
        Err(Error::unauthorized("This action is unauthorized."))
    }

    /// Whether `ability` has been defined.
    pub fn has(&self, ability: &str) -> bool {
        self.abilities.contains_key(ability)
    }

    /// Every defined ability, sorted.
    pub fn abilities(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.abilities.keys().map(String::as_str).collect();
        names.sort();
        names
    }
}

impl<A: Actor> std::fmt::Debug for Gate<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("abilities", &self.abilities())
            .field("before", &self.before.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct User {
        id: u64,
        admin: bool,
        banned: bool,
    }

    #[derive(Debug)]
    struct Post {
        author_id: u64,
    }

    #[derive(Debug)]
    struct Comment;

    fn user(id: u64) -> User {
        User { id, admin: false, banned: false }
    }

    fn admin() -> User {
        User { id: 99, admin: true, banned: false }
    }

    fn gate() -> Gate<User> {
        Gate::new()
            .define("posts.update", |user: &User, post: Option<&Post>| {
                post.is_some_and(|post| post.author_id == user.id)
            })
            .define_simple("posts.create", |user: &User| !user.banned)
    }

    #[test]
    fn an_ability_decides_from_the_user_and_subject() {
        let gate = gate();
        let post = Post { author_id: 1 };

        assert!(gate.allows("posts.update", &user(1), Some(&post)));
        assert!(gate.denies("posts.update", &user(2), Some(&post)));
    }

    #[test]
    fn an_undefined_ability_is_denied() {
        // A typo in an ability name must fail closed, not open.
        let gate = gate();
        assert!(gate.denies::<Post>("posts.publish", &admin(), None));
        assert!(!gate.has("posts.publish"));
    }

    #[test]
    fn an_ability_with_no_subject() {
        let gate = gate();
        assert!(gate.allows_any("posts.create", &user(1)));

        let banned = User { banned: true, ..user(1) };
        assert!(!gate.allows_any("posts.create", &banned));
    }

    #[test]
    fn a_subject_of_the_wrong_type_is_denied_rather_than_ignored() {
        let gate = gate();
        // `posts.update` expects a `Post`; passing a `Comment` is a bug, and
        // silently treating it as "no subject" would grant or deny by accident.
        assert!(gate.denies("posts.update", &user(1), Some(&Comment)));
    }

    #[test]
    fn a_before_check_short_circuits_everything() {
        let gate = gate().before(|user: &User, _ability: &str| user.admin.then_some(true));

        let someone_elses = Post { author_id: 1 };
        assert!(gate.allows("posts.update", &admin(), Some(&someone_elses)));
        assert!(gate.allows_any("posts.anything.undefined", &admin()));
    }

    #[test]
    fn a_before_check_can_also_deny_outright() {
        let gate = gate().before(|user: &User, _: &str| user.banned.then_some(false));

        let banned = User { banned: true, ..user(1) };
        let own_post = Post { author_id: 1 };
        assert!(gate.denies("posts.update", &banned, Some(&own_post)));
    }

    #[test]
    fn a_before_check_returning_none_defers_to_the_ability() {
        let gate = gate().before(|_: &User, _: &str| None);
        let post = Post { author_id: 1 };

        assert!(gate.allows("posts.update", &user(1), Some(&post)));
        assert!(gate.denies("posts.update", &user(2), Some(&post)));
    }

    #[test]
    fn authorize_turns_a_denial_into_a_403() {
        let gate = gate();
        let post = Post { author_id: 1 };

        assert!(gate.authorize("posts.update", &user(1), Some(&post)).is_ok());

        let err = gate.authorize("posts.update", &user(2), Some(&post)).unwrap_err();
        assert_eq!(err.status(), 403);
        assert_eq!(err.message(), "This action is unauthorized.");
    }

    #[test]
    fn defined_abilities_can_be_listed() {
        assert_eq!(gate().abilities(), vec!["posts.create", "posts.update"]);
        assert!(gate().has("posts.update"));
    }

    /// A machine caller: no password, no session, no person behind it.
    #[derive(Debug)]
    struct ApiClient {
        id: String,
        scopes: Vec<&'static str>,
    }

    /// An assumed cloud role, which is not even an account this service owns.
    #[derive(Debug)]
    struct StsPrincipal {
        arn: String,
    }

    #[test]
    fn a_gate_authorizes_an_api_client() {
        // The client-credentials grant: there is no user at all, and inventing
        // one so the gate would accept it is how a machine identity ends up
        // able to log in.
        let gate = Gate::<ApiClient>::new().define_simple("posts.read", |client: &ApiClient| {
            client.scopes.contains(&"posts:read")
        });

        let reader = ApiClient { id: "svc-reports".into(), scopes: vec!["posts:read"] };
        let writer = ApiClient { id: "svc-import".into(), scopes: vec!["posts:write"] };

        assert!(gate.allows_any("posts.read", &reader));
        assert!(gate.denies::<()>("posts.read", &writer, None));
        assert_eq!(reader.id, "svc-reports");
    }

    #[test]
    fn a_gate_authorizes_a_cloud_principal() {
        let gate = Gate::<StsPrincipal>::new().define_simple("admin.enter", |principal| {
            principal.arn.starts_with("arn:aws:sts::123456789012:assumed-role/Deployer/")
        });

        let deployer =
            StsPrincipal { arn: "arn:aws:sts::123456789012:assumed-role/Deployer/ci".into() };
        let stranger =
            StsPrincipal { arn: "arn:aws:sts::999999999999:assumed-role/Other/x".into() };

        assert!(gate.allows_any("admin.enter", &deployer));
        assert!(!gate.allows_any("admin.enter", &stranger));
    }

    #[test]
    fn an_undefined_ability_is_denied_for_a_machine_too() {
        // The property that matters most, restated for the actor type nobody
        // was thinking about when it was written.
        let gate = Gate::<ApiClient>::new();
        let client = ApiClient { id: "svc".into(), scopes: vec!["everything"] };

        assert!(!gate.allows_any("posts.read", &client));
    }
}
