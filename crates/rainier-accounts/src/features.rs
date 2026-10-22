//! Which flows are switched on — [`Features`].
//!
//! Every flow is a route, and a route that is not mounted cannot be attacked.
//! So a disabled feature is not a handler that answers `403`: it is a path
//! that was never declared, and the router answers `404` for it like any other
//! path it does not know.

use std::time::Duration;

/// The flows [`Accounts`](crate::Accounts) mounts, and the knobs on them.
///
/// The defaults are what a new application wants: everything on except email
/// verification, which needs a [`UrlSigner`](rainier_crypt::UrlSigner) and a
/// way to send mail before it can work, so it is opted into rather than
/// discovered missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Features {
    /// `POST /register`.
    pub registration: bool,
    /// `POST /forgot-password` and `POST /reset-password`.
    pub reset_passwords: bool,
    /// `GET /email/verify/{id}/{hash}` and
    /// `POST /email/verification-notification`, and a verification link sent
    /// on registration.
    pub email_verification: bool,
    /// `PUT /user/password`.
    pub update_passwords: bool,
    /// `POST /user/confirm-password` — the endpoint
    /// [`ConfirmPassword`](rainier_auth::ConfirmPassword)'s `423` points at.
    pub password_confirmation: bool,
    /// TOTP two-factor authentication: a challenge after a right password,
    /// and the routes that enable, confirm and disable it. Needs the crate's
    /// `two-factor` feature and [`Accounts::two_factor_with`](crate::Accounts).
    pub two_factor: bool,
    /// Passkey sign-in and registration. Needs the crate's `passkeys`
    /// feature and [`Accounts::passkeys_with`](crate::Accounts).
    pub passkeys: bool,
    /// Wrong second-factor codes allowed per pending login before it is
    /// locked out for [`login_decay`](Self::login_decay).
    pub two_factor_attempts: u32,
    /// How long a right password waits for its second factor before the
    /// login has to start again.
    pub two_factor_timeout: Duration,
    /// How many recovery codes enabling two-factor issues.
    pub recovery_codes: usize,
    /// Failed logins allowed per email-and-address before a lockout.
    pub max_login_attempts: u32,
    /// How long the failed-login window lasts — and so how long a lockout
    /// lasts, since the window is anchored to its first failure.
    pub login_decay: Duration,
    /// The shortest password registration, reset and update accept.
    pub password_min_length: usize,
    /// How long a password reset link works.
    pub reset_token_ttl: Duration,
    /// How soon another reset link may be sent for the same account.
    pub reset_throttle: Duration,
    /// How long an email verification link works.
    pub verification_ttl: Duration,
    /// How often a verification link may be re-sent per account.
    pub verification_throttle: Duration,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            registration: true,
            reset_passwords: true,
            email_verification: false,
            update_passwords: true,
            password_confirmation: true,
            two_factor: false,
            passkeys: false,
            two_factor_attempts: 5,
            two_factor_timeout: Duration::from_secs(5 * 60),
            recovery_codes: 8,
            max_login_attempts: 5,
            login_decay: Duration::from_secs(60),
            password_min_length: 8,
            reset_token_ttl: Duration::from_secs(60 * 60),
            reset_throttle: Duration::from_secs(60),
            verification_ttl: Duration::from_secs(60 * 60),
            verification_throttle: Duration::from_secs(60),
        }
    }
}

impl Features {
    /// Every flow on, email verification included.
    pub fn all() -> Self {
        Self { email_verification: true, ..Self::default() }
    }

    /// Login and logout only — an application whose accounts are made
    /// somewhere else.
    pub fn login_only() -> Self {
        Self {
            registration: false,
            reset_passwords: false,
            email_verification: false,
            update_passwords: false,
            password_confirmation: false,
            two_factor: false,
            passkeys: false,
            ..Self::default()
        }
    }
}
