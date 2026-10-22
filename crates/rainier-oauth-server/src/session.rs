//! Carrying an authorization request across the consent screen —
//! [`AuthorizationSession`].
//!
//! `GET /oauth/authorize` validates the request and renders the consent
//! screen; the user's answer arrives as a separate `POST`. Between the two the
//! request lives in the user's session, next to a random `auth_token` the
//! consent form posts back.
//!
//! The token is what makes the approval this user's decision about this
//! request: without it, a page on another site could post "approve" to the
//! endpoint from inside the user's logged-in browser, and the most recent
//! authorization they looked at would be approved for them. It is single-use
//! and compared in constant time.

use rainier_http::Request;
use rainier_session::SessionRequestExt;
use rainier_support::{Error, Result};
use subtle::ConstantTimeEq;

use crate::server::AuthorizationRequest;

const REQUEST_KEY: &str = "_oauth.authorization";
const TOKEN_KEY: &str = "_oauth.auth_token";

/// Stashes and retrieves the pending authorization in the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct AuthorizationSession;

impl AuthorizationSession {
    /// Store `authorization` in the request's session and return the
    /// `auth_token` the consent form must post back.
    pub fn stash(request: &Request, authorization: &AuthorizationRequest) -> Result<String> {
        let session = request.session().ok_or_else(|| {
            Error::internal("the authorization endpoint needs the session middleware")
        })?;
        let token = crate::model::new_identifier();
        session.put(REQUEST_KEY, authorization)?;
        session.put(TOKEN_KEY, &token)?;
        Ok(token)
    }

    /// Take the pending authorization back out, if `auth_token` matches the
    /// one stashed with it. Single-use: a second call finds nothing.
    pub fn take(request: &Request, auth_token: &str) -> Result<AuthorizationRequest> {
        let session = request.session().ok_or_else(|| {
            Error::internal("the authorization endpoint needs the session middleware")
        })?;
        let stored: Option<String> = session.pull(TOKEN_KEY);
        let authorization: Option<AuthorizationRequest> = session.pull(REQUEST_KEY);

        match (stored, authorization) {
            (Some(stored), Some(authorization))
                if !auth_token.is_empty()
                    && bool::from(stored.as_bytes().ct_eq(auth_token.as_bytes())) =>
            {
                Ok(authorization)
            }
            _ => Err(Error::bad_request("The authorization request was not found or has expired.")),
        }
    }
}
