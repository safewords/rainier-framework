//! # rainier-auth
//!
//! Authentication and authorization: [`Guard`]s that say *who* is making a
//! request, and a [`Gate`] that says *whether they may*.
//!
//! | | |
//! |---|---|
//! | [`Authenticatable`] | what the framework needs to know about a user model |
//! | [`UserProvider`] / [`RepositoryUserProvider`] | how users are found and their credentials verified |
//! | [`Hasher`] / [`HashManager`] | password hashing — `rainier-hashing`'s, re-exported |
//! | [`Guard`] / [`TokenGuard`] / [`SessionGuard`] | how a request is tied to a user |
//! | [`AuthManager`] | named guards — `auth:api` |
//! | [`Authenticate`] | the middleware that enforces it |
//! | [`Gate`] | abilities and policies |
//!
//! ```ignore
//! let provider = Arc::new(RepositoryUserProvider::new(users, hasher));
//! let auth = Arc::new(
//!     AuthManager::<User>::new("web")
//!         .register(Arc::new(SessionGuard::new("web", provider.clone(), sessions)))
//!         .register(Arc::new(TokenGuard::new("api", provider))),
//! );
//!
//! middleware.alias_factory("auth", move |args| {
//!     Ok(Arc::new(Authenticate::from_args(auth.clone(), args)) as Arc<_>)
//! });
//!
//! router.get("/me", show).middleware(["auth:api"]);
//! ```
//!
//! ## One user type per manager
//!
//! [`AuthManager`] is generic over the user model rather than yielding
//! `dyn Authenticatable`. An application has one user model and wants it back
//! with its own type; erasing it would only force a downcast at every call
//! site. An application with genuinely distinct user kinds — customers and
//! staff, in different tables — registers one manager each, which is clearer
//! than one registry returning different shapes.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod abilities;
pub mod challenge;
pub mod confirm;
pub mod gate;
pub mod guard;
pub mod middleware;
pub mod session;
pub mod user;

// Hashing is cryptography, so it lives in `rainier-crypt` — the guards
// *consume* the `Hasher` port, they do not own it. Re-exported here because a
// user provider is constructed with a hasher, and making callers add a second
// dependency for that would be ceremony. `rainier_crypt::hash` (or
// `rainier_framework::crypt::hash`) is the canonical path.
pub use rainier_crypt::hash as hashing;
pub use rainier_crypt::hash::legacy;

pub use abilities::Abilities;
pub use challenge::Challenges;
pub use confirm::{confirm_password, ConfirmPassword};
pub use gate::{Actor, Gate};
pub use guard::{AuthManager, Guard, GuardExt, SessionGuard, StatefulGuard, TokenGuard};
#[cfg(feature = "bcrypt")]
pub use rainier_crypt::hash::{BcryptHasher, BcryptVerifier};
pub use rainier_crypt::hash::{Argon2Hasher, HashDriver, HashManager, Hasher, LegacyVerifier};
pub use middleware::{
    AbilitiesRequestExt, Authenticate, AuthenticatedUser, RedirectIfAuthenticated, RequireAbility,
    TokenAbilities,
};
pub use session::{generate_session_id, MemorySessionStore, Session, SessionStore, AUTH_KEY};
pub use user::{Authenticatable, Credentials, RepositoryUserProvider, UserProvider};

// Re-exported so implementors get the attribute macro without adding the
// dependency themselves.
pub use async_trait::async_trait;
