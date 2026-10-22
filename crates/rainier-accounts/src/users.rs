//! What the application implements — [`AccountUser`], [`AccountUsers`] and
//! [`CreatesNewUsers`].
//!
//! The scaffolding owns the *flows* — which checks run in which order, what is
//! hashed, what is throttled, what is said to a caller who guessed wrong. The
//! application owns the *rows*. These three traits are the whole of the
//! boundary between them, and they are small on purpose: nothing here knows
//! whether users live in a table, a document store or another service.

use rainier_auth::Authenticatable;
use rainier_support::Result;
use serde_json::Value;

/// A user model the account flows can work with.
///
/// [`Authenticatable`] already says how to identify one and where its
/// password hash is. The flows additionally need the address mail goes to, and
/// whether it has been proven.
pub trait AccountUser: Authenticatable {
    /// The address password reset and verification links go to.
    fn email(&self) -> &str;

    /// Whether the address has been verified.
    ///
    /// Return `true` unconditionally for an application that does not verify
    /// addresses; [`EnsureEmailIsVerified`](crate::EnsureEmailIsVerified) is
    /// then simply never mounted.
    fn has_verified_email(&self) -> bool;
}

/// Where users are found and changed.
#[async_trait::async_trait]
pub trait AccountUsers<U: AccountUser>: Send + Sync + 'static {
    /// The user with this identifier — the value
    /// [`auth_identifier`](Authenticatable::auth_identifier) returns.
    async fn find_by_id(&self, id: &str) -> Result<Option<U>>;

    /// The user with this email address.
    ///
    /// Already lowercased and trimmed by the caller. Whether the lookup is
    /// case-insensitive beyond that is the store's decision — but a store that
    /// lets `Ada@` and `ada@` be two accounts will confuse its users long
    /// before it confuses this crate.
    async fn find_by_email(&self, email: &str) -> Result<Option<U>>;

    /// Store a new password hash — after a reset or an update.
    async fn set_password_hash(&self, user: &U, hash: &str) -> Result<()>;

    /// Record that the user's address is verified.
    ///
    /// Only called by the email verification flow. The default refuses, so an
    /// application that turns that flow on without implementing this finds out
    /// at the first click rather than by a link that silently does nothing.
    async fn mark_email_verified(&self, user: &U) -> Result<()> {
        let _ = user;
        Err(rainier_support::Error::internal(
            "email verification is enabled, but `AccountUsers::mark_email_verified` is not \
             implemented",
        ))
    }
}

/// What registration hands the application.
///
/// The address has been validated, normalised and checked for a duplicate,
/// and the password has been validated, confirmed and **hashed**. The
/// plaintext password is not here, so it cannot end up in a row, a log line or
/// an event by accident.
#[derive(Debug, Clone)]
pub struct NewUser {
    /// The address, trimmed and lowercased.
    pub email: String,
    /// The password, hashed by the configured hasher.
    pub password_hash: String,
    /// Everything else that was submitted — `name`, a referral code, a
    /// time zone — minus `password` and `password_confirmation`.
    pub input: Value,
}

/// Creates a user at registration.
///
/// The application's own fields are validated here: return
/// `Error::validation(json!({ "name": ["…"] }))` and the caller sees a `422`
/// exactly as it would for the scaffold's own rules.
#[async_trait::async_trait]
pub trait CreatesNewUsers<U: AccountUser>: Send + Sync + 'static {
    /// Create and persist the user.
    async fn create(&self, new_user: NewUser) -> Result<U>;
}
