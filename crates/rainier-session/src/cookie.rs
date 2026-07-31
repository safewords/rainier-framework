//! [`CookieSessionStore`] — the whole session, encrypted, in the cookie.

use rainier_crypt::Encryption;
use rainier_support::{BoxFuture, Error, Result};
use serde::{Deserialize, Serialize};

use crate::session::{generate_session_id, SessionData};
use crate::store::SessionStore;

/// A cookie's practical size limit.
///
/// Browsers guarantee 4096 bytes **per cookie including its name and
/// attributes**, so the value itself has less. 3500 leaves room for the name,
/// `Path`, `SameSite`, `Max-Age` and the rest.
const MAX_VALUE_BYTES: usize = 3500;

/// What travels in the cookie.
#[derive(Serialize, Deserialize)]
struct Envelope {
    /// The session id. Still present, so [`regenerate`](crate::Session::regenerate)
    /// means something and so a log line can correlate requests.
    id: String,
    /// The bag.
    data: SessionData,
}

/// Sessions kept entirely in the cookie, encrypted.
///
/// **No server-side state at all**: nothing to store, nothing to expire,
/// nothing to share between instances. That is the whole appeal, and every
/// limitation below follows from it.
///
/// | | |
/// |---|---|
/// | Needs a store | no |
/// | Works across instances | yes, with no shared infrastructure |
/// | Size limit | ~3.5 KB, hard |
/// | Can be revoked server-side | **no** |
/// | Cost per request | the cookie, on every request, in both directions |
///
/// ## What you give up
///
/// **Revocation.** A session you cannot see is a session you cannot end. There
/// is no "log out all my devices", no invalidating everything when a password
/// changes, and a stolen cookie works until it expires. If any of that matters,
/// use a server-side store.
///
/// **Size.** The limit is small and the failure is at write time. Putting a
/// user's cart in the session will eventually exceed it.
///
/// **Freshness.** The client holds the only copy, so a session it does not send
/// back is simply gone, and one it sends from an old tab is stale.
///
/// ## What protects it
///
/// The cookie is [encrypted](rainier_crypt), so the client cannot read it or
/// change it — tampering fails the AEAD tag and the session is discarded. Which
/// means the [key ring](rainier_crypt::KeyRing) matters twice over here:
/// rotating a key out ends every session it wrote.
///
/// ```
/// use rainier_crypt::{Encryption, Key, KeyRing};
/// use rainier_session::{CookieSessionStore, Session, SessionStore};
///
/// # fn main() -> rainier_support::Result<()> {
/// let store = CookieSessionStore::new(Encryption::from_keys(KeyRing::new(Key::generate())));
///
/// let session = Session::new();
/// session.put("user_id", 42u64)?;
///
/// // What would be sent as the cookie value…
/// let cookie = store.encode(&session.id(), &session.age_and_take())?;
///
/// // The field name rather than the value: base64 of random bytes contains a
/// // given two-character run often enough that asserting on `"42"` fails a
/// // run every few hundred for no reason at all.
/// assert!(!cookie.contains("user_id"), "the client cannot read it");
///
/// // …and what comes back.
/// let (id, data) = store.decode(&cookie)?;
/// assert_eq!(id, session.id());
/// assert_eq!(data.unwrap().values["user_id"], 42);
/// # Ok(()) }
/// ```
pub struct CookieSessionStore {
    crypt: Encryption,
    max_bytes: usize,
}

impl CookieSessionStore {
    /// A store encrypting with `crypt`.
    pub fn new(crypt: Encryption) -> Self {
        Self { crypt, max_bytes: MAX_VALUE_BYTES }
    }

    /// Allow a larger cookie than the conservative default.
    ///
    /// Only worth touching if you know what your clients and any proxy in front
    /// of them accept. A cookie over the browser's limit is silently dropped,
    /// and a session that silently does not persist is a bad afternoon.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The size limit in force.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}

impl SessionStore for CookieSessionStore {
    fn name(&self) -> &str {
        "cookie"
    }

    /// Never called: the data is in the cookie, and
    /// [`decode`](SessionStore::decode) has already produced it.
    fn read<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<Option<SessionData>>> {
        Box::pin(async { Ok(None) })
    }

    /// A no-op: there is nowhere to write to.
    fn write<'a>(&'a self, _id: &'a str, _data: &'a SessionData) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// A no-op.
    ///
    /// This is the limitation to understand before choosing this store: a
    /// session held only by the client **cannot be destroyed** server-side. The
    /// middleware still rotates the id and clears the bag on logout, which
    /// replaces the cookie in that browser — but a copy taken beforehand keeps
    /// working until it expires.
    fn destroy<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn is_client_side(&self) -> bool {
        true
    }

    fn encode(&self, id: &str, data: &SessionData) -> Result<String> {
        let envelope = Envelope { id: id.to_string(), data: data.clone() };
        let sealed = self.crypt.encrypt_json(&envelope)?;

        if sealed.len() > self.max_bytes {
            // Refused rather than truncated or sent anyway: an over-long cookie
            // is dropped by the browser without a word, and the symptom is a
            // session that mysteriously does not persist for some users.
            return Err(Error::internal(format!(
                "this session is {} bytes encrypted, over the {} the cookie driver allows. \
                 Move the large values to a server-side store, or switch the session driver.",
                sealed.len(),
                self.max_bytes
            )));
        }

        Ok(sealed)
    }

    fn decode(&self, value: &str) -> Result<(String, Option<SessionData>)> {
        let envelope: Envelope = self.crypt.decrypt_json(value)?;

        // The id came out of an authenticated payload, so a client cannot have
        // chosen it — but a payload written by an older version might carry
        // something unexpected, and a fresh id is cheaper than trusting it.
        let id = if crate::store::is_well_formed(&envelope.id) {
            envelope.id
        } else {
            generate_session_id()
        };

        Ok((id, Some(envelope.data)))
    }
}

impl std::fmt::Debug for CookieSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CookieSessionStore").field("max_bytes", &self.max_bytes).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_crypt::{Key, KeyRing};

    fn store() -> CookieSessionStore {
        CookieSessionStore::new(Encryption::from_keys(KeyRing::new(Key::generate())))
    }

    fn data(user: u64) -> SessionData {
        let mut values = serde_json::Map::new();
        values.insert("user_id".into(), user.into());
        SessionData { values, flash: vec!["status".to_string()] }
    }

    #[test]
    fn a_session_round_trips_through_a_cookie() {
        let store = store();
        let id = generate_session_id();

        let cookie = store.encode(&id, &data(42)).unwrap();
        let (recovered, recovered_data) = store.decode(&cookie).unwrap();

        assert_eq!(recovered, id);
        let recovered_data = recovered_data.expect("client-side stores return their data");
        assert_eq!(recovered_data.values["user_id"], 42);
        assert_eq!(recovered_data.flash, vec!["status".to_string()], "flash bookkeeping survives");
    }

    #[test]
    fn the_client_cannot_read_it() {
        let cookie = store().encode(&generate_session_id(), &data(42)).unwrap();

        // The key names and the flash marker, not the value: a two-character
        // value like "42" turns up in base64 ciphertext by chance often enough
        // to make that assertion flaky. Longer strings effectively do not.
        assert!(!cookie.contains("user_id"), "{cookie}");
        assert!(!cookie.contains("status"), "{cookie}");
        assert!(!cookie.contains("values"), "{cookie}");
    }

    #[test]
    fn the_client_cannot_change_it() {
        let store = store();
        let cookie = store.encode(&generate_session_id(), &data(42)).unwrap();

        // Flip a byte in the ciphertext.
        let mut bytes = cookie.into_bytes();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };

        assert!(store.decode(&String::from_utf8(bytes).unwrap()).is_err());
    }

    #[test]
    fn another_applications_key_cannot_read_it() {
        let cookie = store().encode(&generate_session_id(), &data(42)).unwrap();
        assert!(store().decode(&cookie).is_err());
    }

    #[test]
    fn rubbish_is_refused() {
        let store = store();
        for bad in ["", "nonsense", "xc20p.abc.def.ghi"] {
            assert!(store.decode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn it_is_marked_client_side() {
        assert!(store().is_client_side());
        assert_eq!(store().name(), "cookie");
    }

    #[tokio::test]
    async fn the_server_side_methods_are_inert() {
        let store = store();

        assert!(store.read("anything").await.unwrap().is_none());
        assert!(store.write("anything", &data(1)).await.is_ok());
        assert!(store.destroy("anything").await.is_ok());
        assert_eq!(store.gc().await.unwrap(), 0);
    }

    #[test]
    fn an_over_large_session_is_refused_with_both_numbers() {
        let store = store();
        let mut values = serde_json::Map::new();
        values.insert("cart".into(), serde_json::json!("x".repeat(8000)));

        let err = store
            .encode(&generate_session_id(), &SessionData { values, flash: Vec::new() })
            .unwrap_err();

        assert!(err.message().contains("3500"), "{}", err.message());
        assert!(err.message().contains("over the"), "{}", err.message());
    }

    #[test]
    fn the_limit_is_adjustable() {
        let store = store().with_max_bytes(64);
        assert_eq!(store.max_bytes(), 64);

        // 64 bytes is smaller than an encrypted empty session, so even that is
        // refused — which is the point of the guard being at write time.
        assert!(store.encode(&generate_session_id(), &SessionData::default()).is_err());
    }

    #[test]
    fn an_empty_session_fits_comfortably() {
        let cookie = store().encode(&generate_session_id(), &SessionData::default()).unwrap();
        assert!(cookie.len() < 300, "an empty session is {} bytes", cookie.len());
    }

    #[test]
    fn the_cookie_value_is_safe_to_put_in_a_header() {
        let cookie = store().encode(&generate_session_id(), &data(42)).unwrap();

        assert!(
            cookie.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
            "{cookie}"
        );
    }

    #[test]
    fn a_malformed_id_inside_a_valid_payload_is_replaced() {
        // Written by an older version, or by something that did not use
        // `generate_session_id`. The payload authenticated, so this is not an
        // attack — but a fresh id is cheaper than trusting the one inside.
        let store = store();
        let cookie = store.encode("not-a-session-id", &data(42)).unwrap();

        let (id, data) = store.decode(&cookie).unwrap();
        assert_ne!(id, "not-a-session-id");
        assert_eq!(data.unwrap().values["user_id"], 42, "the data is still trusted");
    }
}
