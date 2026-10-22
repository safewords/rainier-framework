//! How refresh tokens and authorization codes are sealed — [`TokenSealer`].
//!
//! Both are handed to the client as opaque strings that only this server can
//! open: the payload inside says which rows they correspond to, so a forged
//! one is refused before the database is asked anything. *How* they are sealed
//! is a port because it is the one part of the format a client actually holds
//! for months — a server that takes over from another has to open what that
//! one sealed, or every refresh token in the field stops working at once.
//!
//! The native sealer is the framework's own [`Encrypter`] — an AEAD with a key
//! id, so `APP_KEY` rotation does not strand anything.
//!
//! # The payloads
//!
//! [`RefreshTokenPayload`] and [`AuthCodePayload`] are the JSON the most
//! widely deployed PHP server writes inside its sealed tokens, field for
//! field, so the same payload types serve both the native sealer and a
//! compatibility one.

use std::sync::Arc;

use rainier_crypt::Encrypter;
use rainier_support::Result;
use serde::{Deserialize, Deserializer, Serialize};

/// Seals and opens the opaque tokens the server hands out.
pub trait TokenSealer: Send + Sync + 'static {
    /// Seal `plain` into a string safe to put in a URL or a JSON body.
    fn seal(&self, plain: &[u8]) -> Result<String>;

    /// Open what [`seal`](Self::seal) produced. Any failure — truncation,
    /// tampering, the wrong key — is one indistinguishable error.
    fn open(&self, sealed: &str) -> Result<Vec<u8>>;
}

/// The native sealer: the framework [`Encrypter`].
pub struct EncrypterSealer {
    encrypter: Arc<dyn Encrypter>,
}

impl EncrypterSealer {
    /// Seal with `encrypter`.
    pub fn new(encrypter: Arc<dyn Encrypter>) -> Self {
        Self { encrypter }
    }
}

impl TokenSealer for EncrypterSealer {
    fn seal(&self, plain: &[u8]) -> Result<String> {
        self.encrypter.encrypt_bytes(plain)
    }

    fn open(&self, sealed: &str) -> Result<Vec<u8>> {
        self.encrypter.decrypt_bytes(sealed)
    }
}

impl std::fmt::Debug for EncrypterSealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncrypterSealer").finish_non_exhaustive()
    }
}

/// What a refresh token says inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshTokenPayload {
    /// The client it belongs to.
    pub client_id: String,
    /// Its row.
    pub refresh_token_id: String,
    /// The access token it was issued with.
    pub access_token_id: String,
    /// The scopes that access token carried.
    pub scopes: Vec<String>,
    /// The user, or `None` for a client token. Older writers stored an
    /// integer; it reads as its decimal string.
    #[serde(deserialize_with = "string_or_number")]
    pub user_id: Option<String>,
    /// When it expires, in Unix seconds.
    pub expire_time: i64,
}

/// What an authorization code says inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCodePayload {
    /// The client it was issued to.
    pub client_id: String,
    /// The redirect URI the authorization request named, if it named one.
    pub redirect_uri: Option<String>,
    /// Its row.
    pub auth_code_id: String,
    /// The scopes approved.
    pub scopes: Vec<String>,
    /// The user who approved it.
    #[serde(deserialize_with = "string_or_number")]
    pub user_id: Option<String>,
    /// When it expires, in Unix seconds.
    pub expire_time: i64,
    /// The PKCE challenge, if the request carried one.
    pub code_challenge: Option<String>,
    /// `S256` or `plain`.
    pub code_challenge_method: Option<String>,
}

fn string_or_number<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Text(String),
        Number(i64),
    }
    Ok(Option::<Id>::deserialize(deserializer)?.map(|id| match id {
        Id::Text(text) => text,
        Id::Number(number) => number.to_string(),
    }))
}

/// Seal a payload as JSON.
pub(crate) fn seal_json<T: Serialize>(sealer: &dyn TokenSealer, value: &T) -> Result<String> {
    sealer.seal(&serde_json::to_vec(value)?)
}

/// Open and parse a payload. `None` for anything that does not open or does
/// not parse — the caller turns that into the right OAuth error.
pub(crate) fn open_json<T: for<'de> Deserialize<'de>>(
    sealer: &dyn TokenSealer,
    sealed: &str,
) -> Option<T> {
    let plain = sealer.open(sealed).ok()?;
    serde_json::from_slice(&plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_crypt::{AeadEncrypter, Key, KeyRing};

    fn sealer() -> EncrypterSealer {
        EncrypterSealer::new(Arc::new(AeadEncrypter::new(KeyRing::new(Key::generate()))))
    }

    #[test]
    fn a_payload_round_trips_and_a_tampered_one_does_not_open() {
        let sealer = sealer();
        let payload = RefreshTokenPayload {
            client_id: "c".into(),
            refresh_token_id: "r".into(),
            access_token_id: "a".into(),
            scopes: vec!["account".into()],
            user_id: Some("7".into()),
            expire_time: 100,
        };
        let sealed = seal_json(&sealer, &payload).unwrap();
        assert_eq!(open_json::<RefreshTokenPayload>(&sealer, &sealed), Some(payload));

        let mut tampered = sealed.clone();
        tampered.push('A');
        assert_eq!(open_json::<RefreshTokenPayload>(&sealer, &tampered), None);
    }

    #[test]
    fn an_integer_user_id_from_an_older_writer_reads_as_a_string() {
        let json = r#"{"client_id":"c","refresh_token_id":"r","access_token_id":"a",
            "scopes":[],"user_id":42,"expire_time":1}"#;
        let payload: RefreshTokenPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.user_id.as_deref(), Some("42"));

        let null = r#"{"client_id":"c","refresh_token_id":"r","access_token_id":"a",
            "scopes":[],"user_id":null,"expire_time":1}"#;
        assert_eq!(serde_json::from_str::<RefreshTokenPayload>(null).unwrap().user_id, None);
    }
}
