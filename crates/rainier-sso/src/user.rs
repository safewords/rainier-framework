//! What a sign-in produces — [`SocialUser`] and its [`AccessToken`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The tokens an authorization server issued.
///
/// `Debug` prints none of them. A token in a log line is a token anybody with
/// log access can use, and `{:?}` on a struct is the easiest way to put one
/// there by accident.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessToken {
    /// The bearer token for the provider's API.
    pub access_token: String,
    /// The refresh token, when the provider issued one — usually only when
    /// asked for (`offline_access`, `access_type=offline`).
    pub refresh_token: Option<String>,
    /// When the access token stops working, if the provider said.
    pub expires_at: Option<DateTime<Utc>>,
    /// The scopes actually granted — which may be fewer than were asked for.
    /// A user can untick a box on the consent screen.
    pub scopes: Vec<String>,
    /// The OpenID Connect ID token, as issued. Already verified by the time a
    /// [`SocialUser`] holds it.
    pub id_token: Option<String>,
}

impl AccessToken {
    /// A token with only the access token known — for
    /// [`Provider::user_from_token`](crate::Provider::user_from_token).
    pub fn bearer(access_token: impl Into<String>) -> Self {
        Self { access_token: access_token.into(), ..Self::default() }
    }

    /// Whether the access token has expired, by the provider's own account.
    /// A token with no stated expiry never reads as expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|at| at <= Utc::now())
    }

    /// Whether `scope` was granted.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|granted| granted == scope)
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[redacted]"))
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .field("id_token", &self.id_token.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// The person a provider vouched for, in one shape whichever provider it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocialUser {
    /// Which provider — the name it was registered under (`"github"`).
    pub provider: String,
    /// The provider's stable identifier for this person.
    ///
    /// **This**, with [`provider`](Self::provider), is what an account is
    /// linked by. Never the email: an email address is reassigned, changed and
    /// shared, and a provider id is not.
    pub id: String,
    /// Their email address, if the provider shared one.
    pub email: Option<String>,
    /// Whether the provider asserts it verified [`email`](Self::email).
    ///
    /// `Some(true)` only when the provider said so in as many words. `None`
    /// means it did not say — which is not the same as `Some(false)`, and is
    /// treated the same as it by [`login_with`](crate::login_with), because
    /// an unverified email is anybody's.
    pub email_verified: Option<bool>,
    /// Their display name.
    pub name: Option<String>,
    /// Their handle, where the provider has one (`login`, `username`,
    /// `preferred_username`).
    pub nickname: Option<String>,
    /// A URL for their picture.
    pub avatar: Option<String>,
    /// The tokens the sign-in produced.
    pub token: AccessToken,
    /// Everything the provider returned about them, unmapped — for the field
    /// this struct does not carry (Entra's `tid`, GitHub's `company`).
    pub raw: Value,
}

impl SocialUser {
    /// The email, but only if the provider verified it.
    pub fn verified_email(&self) -> Option<&str> {
        match self.email_verified {
            Some(true) => self.email.as_deref(),
            _ => None,
        }
    }
}

/// What a [`ProfileSource`](crate::ProfileSource) knows about a person — a
/// [`SocialUser`] before the provider name and the token are attached.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
    /// See [`SocialUser::id`].
    pub id: String,
    /// See [`SocialUser::email`].
    pub email: Option<String>,
    /// See [`SocialUser::email_verified`].
    pub email_verified: Option<bool>,
    /// See [`SocialUser::name`].
    pub name: Option<String>,
    /// See [`SocialUser::nickname`].
    pub nickname: Option<String>,
    /// See [`SocialUser::avatar`].
    pub avatar: Option<String>,
    /// See [`SocialUser::raw`].
    pub raw: Value,
}

impl Profile {
    /// Attach the provider and the token.
    pub fn into_user(self, provider: impl Into<String>, token: AccessToken) -> SocialUser {
        SocialUser {
            provider: provider.into(),
            id: self.id,
            email: self.email,
            email_verified: self.email_verified,
            name: self.name,
            nickname: self.nickname,
            avatar: self.avatar,
            token,
            raw: self.raw,
        }
    }
}

/// A string field of a JSON object, `None` when absent, `null` or empty.
pub(crate) fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A boolean field, accepting the `"true"`/`"false"` strings some issuers
/// (Cognito, older ADFS) put where a boolean belongs.
pub(crate) fn flag(value: &Value, key: &str) -> Option<bool> {
    match value.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_output_carries_no_token() {
        let token = AccessToken {
            access_token: "gho_secret".into(),
            refresh_token: Some("ghr_secret".into()),
            id_token: Some("eyJ.secret".into()),
            ..AccessToken::default()
        };

        let printed = format!("{token:?}");
        assert!(!printed.contains("secret"), "{printed}");
    }

    #[test]
    fn only_a_verified_email_is_offered_as_one() {
        let mut user =
            Profile { id: "1".into(), email: Some("a@b.co".into()), ..Profile::default() }
                .into_user("github", AccessToken::bearer("t"));

        assert_eq!(user.verified_email(), None);
        user.email_verified = Some(false);
        assert_eq!(user.verified_email(), None);
        user.email_verified = Some(true);
        assert_eq!(user.verified_email(), Some("a@b.co"));
    }

    #[test]
    fn fields_are_read_leniently() {
        let raw = json!({ "id": 42, "empty": "", "yes": "TRUE", "no": false });
        assert_eq!(text(&raw, "id").as_deref(), Some("42"));
        assert_eq!(text(&raw, "empty"), None);
        assert_eq!(flag(&raw, "yes"), Some(true));
        assert_eq!(flag(&raw, "no"), Some(false));
        assert_eq!(flag(&raw, "missing"), None);
    }

    #[test]
    fn expiry_is_by_the_providers_clock() {
        let mut token = AccessToken::bearer("t");
        assert!(!token.is_expired());
        token.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(token.is_expired());
    }
}
