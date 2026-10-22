//! Email verification links — [`EmailVerifier`] — and the
//! [`EnsureEmailIsVerified`] gate.
//!
//! A verification link is `/email/verify/{id}/{hash}?expires=…&signature=…`:
//!
//! - `id` says whose address it is,
//! - `hash` is SHA-256 of the address the link was sent **to**, so a link
//!   sent before the user changed their address does not verify the new one,
//! - the signature, from [`UrlSigner`], proves this application issued it and
//!   covers the expiry, so neither can be edited.
//!
//! Stateless: nothing is stored when a link is sent, so there is nothing to
//! expire or sweep. What that costs is that a link cannot be revoked
//! individually — rotating the application key retires all of them, and
//! changing the address retires that one.

use std::sync::Arc;

use rainier_auth::{AuthenticatedUser, Guard};
use rainier_crypt::UrlSigner;
use rainier_http::{IntoResponse, Redirect, Request, Response};
use rainier_middleware::{Middleware, Next};
use rainier_support::{Error, Result};

use crate::passwords::{digest, now};
use crate::users::AccountUser;

/// Builds and checks verification links.
pub struct EmailVerifier {
    signer: Arc<UrlSigner>,
    path: String,
    ttl_secs: i64,
}

impl EmailVerifier {
    /// Links under `path` (the mounted `/email/verify` route, prefix
    /// included), signed by `signer`, working for `ttl_secs`.
    pub fn new(signer: Arc<UrlSigner>, path: impl Into<String>, ttl_secs: i64) -> Self {
        Self { signer, path: path.into().trim_end_matches('/').to_string(), ttl_secs }
    }

    /// The signed path for `user` — relative, so the host the application is
    /// reached on does not have to match the one it signed for.
    pub fn link_for<U: AccountUser>(&self, user: &U) -> Result<String> {
        let unsigned = format!(
            "{}/{}/{}",
            self.path,
            encode(&user.auth_identifier()),
            digest(&normalise_email(user.email()))
        );
        self.signer.sign_until(&unsigned, now() + self.ttl_secs)
    }

    /// Check the link this request arrived on against `user`.
    ///
    /// # Errors
    ///
    /// `403` for a link that is not this application's, has expired, or names
    /// an address the user no longer has. The message says which, because the
    /// person holding an expired link should be told to ask for a new one.
    pub fn check<U: AccountUser>(&self, request: &Request, user: &U) -> Result<()> {
        let path_and_query = request
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| request.path().to_string());

        self.signer.verify(&path_and_query).map_err(|e| Error::unauthorized(e.message()))?;

        let hash = request.route_param("hash").unwrap_or_default();
        if hash != digest(&normalise_email(user.email())) {
            return Err(Error::unauthorized("This link was sent to a different address."));
        }
        Ok(())
    }
}

impl std::fmt::Debug for EmailVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailVerifier").field("path", &self.path).finish()
    }
}

/// Trimmed and lowercased — the one form an address is stored and compared in.
pub(crate) fn normalise_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// An identifier as a path segment.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Refuses a signed-in user whose address is not verified.
///
/// Reads the user [`Authenticate`](rainier_auth::Authenticate) attached when
/// it ran first, and asks the guard otherwise — so it can stand alone or
/// behind `auth`. A guest is a `401`; an unverified user is a `403` for a
/// JSON client and a redirect to the verification notice for a browser.
pub struct EnsureEmailIsVerified<U: AccountUser> {
    guard: Arc<dyn Guard<U>>,
    notice: Option<String>,
}

impl<U: AccountUser> EnsureEmailIsVerified<U> {
    /// Check the user `guard` resolves.
    pub fn new(guard: Arc<dyn Guard<U>>) -> Self {
        Self { guard, notice: None }
    }

    /// Send browsers here instead of answering `403`.
    #[must_use]
    pub fn redirect_to(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }
}

#[async_trait::async_trait]
impl<U: AccountUser> Middleware for EnsureEmailIsVerified<U> {
    async fn handle(&self, request: Request, next: Next) -> Response {
        let verified = match request.extension::<AuthenticatedUser<U>>() {
            Some(user) => Some(user.has_verified_email()),
            None => match self.guard.user(&request).await {
                Ok(user) => user.map(|user| user.has_verified_email()),
                Err(e) => return e.into_response(),
            },
        };

        match verified {
            None => Error::unauthenticated("Unauthenticated.").into_response(),
            Some(true) => next.run(request).await,
            Some(false) => match &self.notice {
                Some(notice) if !request.expects_json() => {
                    Redirect::to(notice.clone()).into_response()
                }
                _ => Error::unauthorized("Your email address is not verified.").into_response(),
            },
        }
    }

    fn name(&self) -> &'static str {
        "EnsureEmailIsVerified"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_is_escaped_as_a_path_segment() {
        assert_eq!(encode("42"), "42");
        assert_eq!(encode("a/b c"), "a%2Fb%20c");
    }
}
