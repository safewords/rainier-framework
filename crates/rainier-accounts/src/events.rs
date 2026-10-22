//! What the flows announce on the event bus.
//!
//! Dispatched with
//! [`dispatch_quietly`](rainier_events::Dispatcher::dispatch_quietly): a
//! welcome mail that fails to send must not turn a registration that has
//! already been written into a `500`, and an audit listener that throws must
//! not stop the user being logged in. Each listener's failure is logged and the
//! request carries on.
//!
//! The user-carrying events hold an `Arc<U>`, so a listener names the
//! application's own type: `events.listen(|e: Arc<Registered<User>>| …)`.

use std::sync::Arc;
use std::time::Duration;

/// A user registered — the welcome mail, the analytics row, the CRM sync.
#[derive(Debug)]
pub struct Registered<U> {
    /// Who.
    pub user: Arc<U>,
}

/// A user logged in with a password.
#[derive(Debug)]
pub struct Login<U> {
    /// Who.
    pub user: Arc<U>,
    /// The guard the session belongs to.
    pub guard: String,
}

/// A login was refused — wrong password, or no such account.
///
/// Carries the address that was tried rather than a user, because "no such
/// account" has no user, and the two cases must look the same from outside.
#[derive(Debug, Clone)]
pub struct Failed {
    /// The address that was tried, normalised.
    pub email: String,
    /// Where the attempt came from, when the server knows.
    pub ip: Option<std::net::IpAddr>,
}

/// Too many failed logins — the throttle has locked this address out.
#[derive(Debug, Clone)]
pub struct Lockout {
    /// The address that was tried, normalised.
    pub email: String,
    /// Where the attempts came from.
    pub ip: Option<std::net::IpAddr>,
    /// How long until another attempt is allowed.
    pub retry_after: Duration,
}

/// A user logged out.
#[derive(Debug)]
pub struct Logout<U> {
    /// Who.
    pub user: Arc<U>,
}

/// A password was reset through a reset link.
///
/// The listener for "tell them their password changed, in case it was not
/// them" — the mail that turns a silent takeover into a noticed one.
#[derive(Debug)]
pub struct PasswordReset<U> {
    /// Whose.
    pub user: Arc<U>,
}

/// A signed-in user changed their password.
#[derive(Debug)]
pub struct PasswordUpdated<U> {
    /// Whose.
    pub user: Arc<U>,
}

/// An email address was verified.
#[derive(Debug)]
pub struct Verified<U> {
    /// Whose.
    pub user: Arc<U>,
}

/// A right password was given and a second factor was asked for.
#[cfg(feature = "two-factor")]
#[derive(Debug, Clone)]
pub struct TwoFactorChallenged {
    /// The user's identifier — the user is not logged in yet.
    pub user_id: String,
}

/// A second-factor code was refused.
#[cfg(feature = "two-factor")]
#[derive(Debug)]
pub struct TwoFactorFailed<U> {
    /// Whose login it was — their password was right.
    pub user: Arc<U>,
}

/// A recovery code was spent. Worth a security email: either the user lost
/// their phone, or somebody else has their codes.
#[cfg(feature = "two-factor")]
#[derive(Debug, Clone)]
pub struct RecoveryCodeUsed {
    /// Whose.
    pub user_id: String,
    /// How many remain — at zero, they should make more.
    pub remaining: usize,
}

/// Two-factor authentication was turned on (enrolment confirmed).
#[cfg(feature = "two-factor")]
#[derive(Debug)]
pub struct TwoFactorEnabled<U> {
    /// Who.
    pub user: Arc<U>,
}

/// Two-factor authentication was turned off.
#[cfg(feature = "two-factor")]
#[derive(Debug)]
pub struct TwoFactorDisabled<U> {
    /// Who.
    pub user: Arc<U>,
}

/// A passkey was registered.
#[cfg(feature = "passkeys")]
#[derive(Debug)]
pub struct PasskeyRegistered<U> {
    /// Whose.
    pub user: Arc<U>,
    /// The name it was given.
    pub name: String,
}

/// A passkey was used to sign in.
#[cfg(feature = "passkeys")]
#[derive(Debug, Clone)]
pub struct PasskeyUsed {
    /// Whose.
    pub user_id: String,
    /// The credential, as unpadded base64url.
    pub credential_id: String,
}

/// A passkey was removed.
#[cfg(feature = "passkeys")]
#[derive(Debug)]
pub struct PasskeyDeleted<U> {
    /// Whose.
    pub user: Arc<U>,
    /// The credential, as unpadded base64url.
    pub credential_id: String,
}
