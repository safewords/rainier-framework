//! From a [`SocialUser`] to a logged-in user of this application —
//! [`SocialAccounts`] and [`login_with`].
//!
//! # Why an existing account is only linked by a verified email
//!
//! The tempting rule is "a sign-in whose email matches an existing account
//! logs into that account". It is an account takeover:
//!
//! 1. Ada has an account here as `ada@example.com`, with a password.
//! 2. Mallory creates an account at some provider and types
//!    `ada@example.com` as its email. Many providers let you; some never
//!    check; Entra ID lets a tenant administrator set any address at all.
//! 3. Mallory signs in here with that provider. The email matches, the rule
//!    links the provider to Ada's account, and Mallory is in.
//!
//! So [`login_with`] links to an existing account by email **only** when the
//! provider asserts it verified that address (`email_verified == Some(true)`).
//! Otherwise it refuses with a `409`, and the person signs in the way they
//! already can and links the provider from their settings — where they have
//! proven they own the account, which is the thing the email was standing in
//! for.
//!
//! The same reasoning is why linking is by `(provider, id)` from then on, and
//! never by email again: the provider id does not change hands.

use async_trait::async_trait;
use rainier_auth::{Authenticatable, StatefulGuard};
use rainier_http::Request;
use rainier_support::{Error, Result};

use crate::user::SocialUser;

/// Where the application keeps which provider identities belong to which
/// users — usually a `social_accounts` table of `(user_id, provider,
/// provider_id)`, unique on the last two.
#[async_trait]
pub trait SocialAccounts<U: Authenticatable>: Send + Sync + 'static {
    /// The user already linked to this provider identity.
    async fn find_by_provider(&self, provider: &str, provider_id: &str) -> Result<Option<U>>;

    /// The user with this email, linked or not.
    async fn find_by_email(&self, email: &str) -> Result<Option<U>>;

    /// Record that `social` belongs to `user`. Called for a new user as well
    /// as an existing one, so `create_user` only has to create.
    async fn link(&self, user: &U, social: &SocialUser) -> Result<()>;

    /// Create a user for a person seen for the first time.
    ///
    /// The password is the application's call; an SSO-only user has none, and
    /// [`Authenticatable::auth_password_hash`] returning `None` is how the
    /// password guard learns that.
    async fn create_user(&self, social: &SocialUser) -> Result<U>;
}

/// What [`login_with`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    /// The identity was already linked; that user is now logged in.
    Existing,
    /// The identity was linked to an existing account by a verified email.
    Linked,
    /// A new user was created and linked.
    Created,
}

/// Find or create the user behind `social`, and log them in on `guard`.
///
/// # Errors
///
/// A `409` when an account already holds the email and the provider did not
/// verify it — see the module docs for why that is not linked automatically.
pub async fn login_with<U: Authenticatable>(
    guard: &dyn StatefulGuard<U>,
    accounts: &dyn SocialAccounts<U>,
    request: &Request,
    social: &SocialUser,
) -> Result<(U, LoginOutcome)> {
    if social.id.is_empty() {
        return Err(Error::unauthenticated(format!(
            "{} returned nobody to sign in as",
            social.provider
        )));
    }

    let (user, outcome) =
        if let Some(user) = accounts.find_by_provider(&social.provider, &social.id).await? {
            (user, LoginOutcome::Existing)
        } else {
            let existing = match &social.email {
                Some(email) => accounts.find_by_email(email).await?,
                None => None,
            };

            match existing {
                Some(user) if social.email_verified == Some(true) => {
                    accounts.link(&user, social).await?;
                    (user, LoginOutcome::Linked)
                }
                Some(_) => {
                    return Err(Error::conflict(format!(
                        "an account already uses this email. Sign in to it, then connect {} \
                         from your account settings",
                        social.provider
                    )))
                }
                None => {
                    let user = accounts.create_user(social).await?;
                    accounts.link(&user, social).await?;
                    (user, LoginOutcome::Created)
                }
            }
        };

    guard.login(request, &user).await?;
    Ok((user, outcome))
}

/// Link `social` to an already-authenticated `user` — the "connect GitHub"
/// button on a settings page, which is where an unverified email ends up.
///
/// # Errors
///
/// A `409` when the identity already belongs to a different user.
pub async fn connect<U: Authenticatable>(
    accounts: &dyn SocialAccounts<U>,
    user: &U,
    social: &SocialUser,
) -> Result<()> {
    if let Some(owner) = accounts.find_by_provider(&social.provider, &social.id).await? {
        if owner.auth_identifier() == user.auth_identifier() {
            return Ok(());
        }
        return Err(Error::conflict(format!(
            "this {} account is already connected to another user",
            social.provider
        )));
    }
    accounts.link(user, social).await
}
