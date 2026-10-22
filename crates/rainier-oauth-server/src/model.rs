//! What the server stores — clients, access tokens, refresh tokens, codes.
//!
//! Plain records rather than models: the [`OAuthStore`](crate::OAuthStore)
//! decides how they are persisted, and an application that keeps its OAuth
//! tables under another server's schema needs to map these onto columns it
//! did not design.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The grant names, as they appear in `grant_type` and in a client's
/// allow-list.
pub mod grant {
    /// The authorization code grant (RFC 6749 §4.1), with PKCE.
    pub const AUTHORIZATION_CODE: &str = "authorization_code";
    /// Exchanging a refresh token (RFC 6749 §6).
    pub const REFRESH_TOKEN: &str = "refresh_token";
    /// The resource owner password grant (RFC 6749 §4.3).
    pub const PASSWORD: &str = "password";
    /// The client credentials grant (RFC 6749 §4.4).
    pub const CLIENT_CREDENTIALS: &str = "client_credentials";
    /// Tokens a user issues to themselves — not an RFC grant, and never
    /// accepted at the token endpoint; see
    /// [`issue_personal_access_token`](crate::AuthorizationServer::issue_personal_access_token).
    pub const PERSONAL_ACCESS: &str = "personal_access";
}

/// A registered client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    /// Its identifier — what it sends as `client_id`.
    pub id: String,
    /// A display name, for the consent screen.
    pub name: String,
    /// The **hash** of its secret, or `None` for a public client (a mobile
    /// or browser app, which cannot keep one).
    pub secret_hash: Option<String>,
    /// Where an authorization may redirect. Matched exactly, except that
    /// loopback `http` URIs match on any port (RFC 8252 §7.3).
    pub redirect_uris: Vec<String>,
    /// The grants it may use.
    pub grant_types: Vec<String>,
    /// The scopes it may be granted, or `None` for any known scope.
    pub scopes: Option<Vec<String>>,
    /// Which user provider its users come from, when there is more than one.
    pub provider: Option<String>,
    /// The user who registered it, if any. A client with no owner is first
    /// party.
    pub owner_id: Option<String>,
    /// Whether it has been revoked. A revoked client is treated as unknown.
    pub revoked: bool,
}

impl Client {
    /// Whether it holds a secret — a confidential client in RFC 6749's terms.
    pub fn is_confidential(&self) -> bool {
        self.secret_hash.as_deref().is_some_and(|hash| !hash.is_empty())
    }

    /// Whether it may use `grant`.
    pub fn supports_grant(&self, grant: &str) -> bool {
        self.grant_types.iter().any(|allowed| allowed == grant)
    }

    /// Whether it may be granted `scope`.
    pub fn allows_scope(&self, scope: &str) -> bool {
        match &self.scopes {
            None => true,
            Some(allowed) => crate::scope::held_grants(allowed, scope),
        }
    }

    /// Whether nobody in particular owns it.
    pub fn is_first_party(&self) -> bool {
        self.owner_id.is_none()
    }
}

/// An issued access token, as stored. The token itself is a signed JWT and
/// never stored; this is what revoking it and listing it need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessTokenRecord {
    /// The token's id — the JWT's `jti`.
    pub id: String,
    /// The user it was issued to, or `None` for a client-credentials token.
    pub user_id: Option<String>,
    /// The client it was issued to.
    pub client_id: String,
    /// A name, for a personal access token.
    pub name: Option<String>,
    /// The scopes it carries.
    pub scopes: Vec<String>,
    /// Whether it has been revoked.
    pub revoked: bool,
    /// When it was issued.
    pub created_at: DateTime<Utc>,
    /// When it expires.
    pub expires_at: DateTime<Utc>,
}

/// An issued refresh token, as stored. The token itself is sealed and handed
/// to the client; this row is what makes it revocable and single-use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshTokenRecord {
    /// Its id.
    pub id: String,
    /// The access token it was issued alongside.
    pub access_token_id: String,
    /// Whether it has been revoked (or spent).
    pub revoked: bool,
    /// When it expires.
    pub expires_at: DateTime<Utc>,
}

/// An issued authorization code, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCodeRecord {
    /// Its id.
    pub id: String,
    /// The user who approved it.
    pub user_id: String,
    /// The client it was issued to.
    pub client_id: String,
    /// The scopes approved.
    pub scopes: Vec<String>,
    /// Whether it has been revoked (or spent).
    pub revoked: bool,
    /// When it expires.
    pub expires_at: DateTime<Utc>,
}

/// A new random identifier: 40 bytes from the OS, hex — 80 characters, the
/// width the common schema gives these columns.
pub fn new_identifier() -> String {
    use rand::Rng as _;

    let mut bytes = [0u8; 40];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_eighty_hex_characters_and_do_not_repeat() {
        let a = new_identifier();
        assert_eq!(a.len(), 80);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, new_identifier());
    }

    #[test]
    fn a_client_without_a_secret_is_public() {
        let mut client = Client {
            id: "c".into(),
            name: "n".into(),
            secret_hash: None,
            redirect_uris: vec![],
            grant_types: vec![grant::AUTHORIZATION_CODE.into()],
            scopes: None,
            provider: None,
            owner_id: None,
            revoked: false,
        };
        assert!(!client.is_confidential());
        client.secret_hash = Some(String::new());
        assert!(!client.is_confidential(), "an empty hash is no secret");
        client.secret_hash = Some("$argon2id$…".into());
        assert!(client.is_confidential());
        assert!(client.supports_grant("authorization_code"));
        assert!(!client.supports_grant("password"));
    }
}
