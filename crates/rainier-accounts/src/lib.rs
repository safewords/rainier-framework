//! # rainier-accounts
//!
//! Authentication scaffolding: registration, login, logout, password reset,
//! email verification, password confirmation and password change — as routes
//! an application mounts, over ports it implements.
//!
//! ```ignore
//! let accounts = Arc::new(
//!     Accounts::new(guard, users, hasher, cache)            // log into, find in, hash with, throttle in
//!         .creates_users_with(Arc::new(CreateUser))          // your columns, your rules
//!         .mails_links_with(MailLinks::new(mailer, "Acme"))  // reset + verification email
//!         .signs_urls_with(signer)                           // verification links
//!         .features(Features::all())
//!         .app_url("https://acme.test")
//!         .events(events)
//!         .middleware(web()),                                // starts the session
//! );
//! accounts.routes(&mut router)?;
//! ```
//!
//! ## Why routes you mount, rather than code generated into the application
//!
//! Generated controllers are copied once and then drift: a fix to the reset
//! flow reaches nobody who already ran the generator. The parts of these flows
//! that are easy to get wrong — the timing of a failed login against an
//! unknown address, a reset token stored in plaintext, a throttle that tells
//! you which addresses exist, an open redirect in "back" — are the same in
//! every application, so they live here and are fixed once.
//!
//! The parts that *differ* — the user table, the extra registration fields,
//! how mail goes out — are traits: [`AccountUsers`], [`CreatesNewUsers`],
//! [`SendsPasswordResetLinks`], [`SendsVerificationLinks`]. And each flow is a
//! public method on [`Accounts`], so a route the defaults do not fit can be
//! mounted at another path, or wrapped.
//!
//! ## What it holds the line on
//!
//! | | |
//! |---|---|
//! | **No account enumeration** through login or reset | an unknown address costs a dummy hash verification; `forgot-password` answers identically and swallows its own failures |
//! | **Login throttling** | per address *and* client IP, in the shared cache, `429` with an accurate `Retry-After` |
//! | **Session fixation** | logging in and changing the password both rotate the session id |
//! | **Reset tokens** | 256-bit, stored as SHA-256, single use, one live per account, expiring, throttled — [`PasswordBroker`] |
//! | **Verification links** | signed, expiring, bound to the address they were sent to — [`EmailVerifier`] |
//! | **Hash upgrades** | a hash under old parameters is rehashed at the next successful login |
//!
//! ## It needs a session
//!
//! Login is a session. The routes must sit behind
//! [`StartSession`](rainier_session::StartSession) — pass it to
//! [`Accounts::middleware`], or declare the routes inside a group that has it.
//! The guard is the application's
//! [`SessionGuard`](rainier_auth::SessionGuard) (or any other
//! [`StatefulGuard`](rainier_auth::StatefulGuard)). CSRF protection for
//! browser form posts is the application's group middleware, as for any other
//! form.
//!
//! ## Responses
//!
//! A request that [expects JSON](rainier_http::Request::expects_json) gets
//! JSON and a status; a browser form post gets a `303` to the configured page.
//! Failures are the framework's ordinary errors — `422` with field messages,
//! `401`, `403`, `429` — and render the way every other error in the
//! application does.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod accounts;
pub mod events;
pub mod features;
pub mod links;
#[cfg(feature = "passkeys")]
pub mod passkeys;
pub mod passwords;
#[cfg(feature = "two-factor")]
pub mod two_factor;
pub mod users;
pub mod verification;

#[cfg(test)]
mod tests;

pub use accounts::{Accounts, RESET_LINK_SENT};
pub use features::Features;
pub use links::{
    MailLinks, ResetPasswordMail, SendsPasswordResetLinks, SendsVerificationLinks, VerifyEmailMail,
};
#[cfg(feature = "passkeys")]
pub use passkeys::{PasskeyRepository, StoredPasskey};
pub use passwords::PasswordBroker;
#[cfg(feature = "two-factor")]
pub use two_factor::{TwoFactorRecord, TwoFactorUsers};
pub use users::{AccountUser, AccountUsers, CreatesNewUsers, NewUser};
pub use verification::{EmailVerifier, EnsureEmailIsVerified};

// Re-exported so implementors get the attribute macro without adding the
// dependency themselves.
pub use async_trait::async_trait;
