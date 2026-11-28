//! The private link between Longmire and Lookout.
//!
//! Longmire reads many Lookouts; nothing else should be able to, and nothing
//! on the way should learn what they said. Both ends hold a **link key** — an
//! id and a secret, shared by the fleet, more than one at a time while one is
//! rotated in — and:
//!
//! 1. **Longmire signs every request** ([`sign`]): HMAC-SHA256 over the
//!    method, the path and query, a timestamp and a fresh nonce, in the
//!    [`HEADER`] header.
//! 2. **Lookout recognises it** ([`verify`]): the key id is one it holds, the
//!    signature matches in constant time, the timestamp is within
//!    [`MAX_SKEW`] of its own clock, and the nonce has not been seen before
//!    ([`ReplayGuard`]) — a request captured on the wire cannot be sent again.
//!    Anything else is not a link request at all: Lookout answers it as it
//!    answers a path that does not exist.
//! 3. **Lookout seals the answer** ([`seal`]): AES-256-GCM under a key
//!    derived for sealing, the request's nonce authenticated alongside, so the
//!    answer cannot be read on the way, altered, or passed off as the answer
//!    to another request.
//! 4. **Longmire opens it** ([`open`]) with the same key and nonce.
//!
//! The secret itself is never on the wire, and the signing and sealing keys
//! are derived from it separately (HKDF-SHA256), so neither use can be turned
//! against the other.
//!
//! ```
//! use rainier_lookout_link::{open, seal, sign, verify, LinkKey, ReplayGuard};
//!
//! let key = LinkKey::new("2026-11", b"a secret the fleet shares, 32 bytes or more".to_vec());
//! let now = 1_800_000_000;
//!
//! // Longmire
//! let signed = sign(&key, "GET", "/_lookout/api/traces?kept=1", now);
//!
//! // Lookout
//! let guard = ReplayGuard::default();
//! let seen = verify(&signed.header, std::slice::from_ref(&key), "GET", "/_lookout/api/traces?kept=1", now, &guard)
//!     .expect("recognised");
//! let sealed = seal(&key, &seen.nonce, br#"{"data":[]}"#).unwrap();
//! assert!(verify(&signed.header, std::slice::from_ref(&key), "GET", "/_lookout/api/traces?kept=1", now, &guard).is_none(), "once");
//!
//! // Longmire
//! assert_eq!(open(&key, &signed.nonce, &sealed).unwrap(), br#"{"data":[]}"#);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::sync::Mutex;

use aes_gcm::aead::{Aead as _, Payload};
use aes_gcm::Aes256Gcm;
use base64::Engine as _;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use rainier_support::{Error, Result};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// The request header a signed request carries.
pub const HEADER: &str = "x-lookout-link";

/// The content type of a sealed answer.
pub const SEALED: &str = "application/vnd.rainier.lookout-link+sealed";

/// How far a request's timestamp may be from the receiver's clock, either
/// way, in seconds.
pub const MAX_SKEW: u64 = 60;

/// The protocol's version, in the header and in everything signed or sealed.
const VERSION: &str = "v1";

/// The shortest secret accepted: 32 bytes.
pub const MIN_SECRET: usize = 32;

type HmacSha256 = Hmac<Sha256>;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A link key: an id both ends name it by, and the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkKey {
    id: String,
    sign: [u8; 32],
    seal: [u8; 32],
}

impl std::fmt::Debug for LinkKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkKey").field("id", &self.id).finish_non_exhaustive()
    }
}

impl LinkKey {
    /// A key from its id and secret. The secret is not kept: only the two
    /// keys derived from it, one to sign and one to seal.
    pub fn new(id: impl Into<String>, secret: Vec<u8>) -> Self {
        let hkdf = Hkdf::<Sha256>::new(Some(b"rainier-lookout-link"), &secret);
        let mut sign = [0u8; 32];
        let mut seal = [0u8; 32];
        hkdf.expand(b"v1 sign", &mut sign).expect("32 bytes is a valid HKDF length");
        hkdf.expand(b"v1 seal", &mut seal).expect("32 bytes is a valid HKDF length");
        Self { id: id.into(), sign, seal }
    }

    /// `id:secret`, the secret in base64 (standard or URL-safe, padded or
    /// not) — as an environment variable holds it. The id is letters, digits,
    /// `-`, `_` and `.`; the secret at least [`MIN_SECRET`] bytes.
    pub fn parse(text: &str) -> Result<Self> {
        let bad =
            |why: &str| Error::bad_request(format!("A link key is `id:base64-secret`: {why}."));
        let (id, secret) = text.trim().split_once(':').ok_or_else(|| bad("no `:`"))?;
        let id = id.trim();
        if id.is_empty()
            || id.len() > 64
            || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(bad("the id is letters, digits, `-`, `_` and `.`"));
        }
        let secret = secret.trim().trim_end_matches('=');
        let bytes = B64
            .decode(secret.replace('+', "-").replace('/', "_"))
            .map_err(|_| bad("the secret is not base64"))?;
        if bytes.len() < MIN_SECRET {
            return Err(bad("the secret is shorter than 32 bytes"));
        }
        Ok(Self::new(id, bytes))
    }

    /// Several keys, comma-separated — the ones a Lookout accepts while one is
    /// rotated in.
    pub fn parse_list(text: &str) -> Result<Vec<Self>> {
        text.split(',').filter(|k| !k.trim().is_empty()).map(Self::parse).collect()
    }

    /// The key's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    fn mac(
        &self,
        method: &str,
        path_and_query: &str,
        ts: u64,
        nonce: &str,
        body: Option<&str>,
    ) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(&self.sign).expect("HMAC takes any key length");
        let method = method.to_ascii_uppercase();
        mac.update(
            format!("{VERSION}\n{}\n{method}\n{path_and_query}\n{ts}\n{nonce}", self.id).as_bytes(),
        );
        // A body is signed by its digest, on a line of its own. A request
        // without one signs exactly what it always did, so Lookouts that
        // predate bodies still recognise every `GET`.
        if let Some(digest) = body {
            mac.update(format!("\n{digest}").as_bytes());
        }
        mac.finalize().into_bytes().into()
    }
}

/// A signed request: the header to send, and the nonce to open the answer
/// with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The value of [`HEADER`].
    pub header: String,
    /// This request's nonce.
    pub nonce: String,
}

/// Sign a request: `method` and `path_and_query` exactly as they will be
/// sent (`/_lookout/api/traces?kept=1`), at `now` (seconds since the epoch).
/// A request with a body is signed with [`sign_request`].
pub fn sign(key: &LinkKey, method: &str, path_and_query: &str, now: u64) -> Signed {
    sign_request(key, method, path_and_query, &[], now)
}

/// Sign a request and its `body`: as [`sign`], and when the body is not
/// empty, its SHA-256 too (the header's `body=`), so it cannot be changed on
/// the way. An empty body signs exactly as [`sign`] does.
pub fn sign_request(
    key: &LinkKey,
    method: &str,
    path_and_query: &str,
    body: &[u8],
    now: u64,
) -> Signed {
    let nonce = B64.encode(random::<18>());
    let digest = (!body.is_empty()).then(|| digest(body));
    let mac = key.mac(method, path_and_query, now, &nonce, digest.as_deref());
    let mut header = format!("{VERSION} key={} ts={now} nonce={nonce}", key.id);
    if let Some(digest) = &digest {
        header.push_str(&format!(" body={digest}"));
    }
    header.push_str(&format!(" sig={}", B64.encode(mac)));
    Signed { header, nonce }
}

/// A body's SHA-256, as the header carries it.
fn digest(body: &[u8]) -> String {
    use sha2::Digest as _;
    B64.encode(Sha256::digest(body))
}

/// A request [`verify`] recognised: the key it was signed with, and its
/// nonce, to seal the answer under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The key's id.
    pub key_id: String,
    /// The request's nonce.
    pub nonce: String,
}

/// Recognise a signed request: `Some` only when its key is one of `keys`, its
/// signature matches, its timestamp is within [`MAX_SKEW`] of `now`, and
/// `guard` has not seen its nonce — which it then remembers. Every failure is
/// the same `None`: a caller has nothing to tell the sender. A request with
/// a body is recognised with [`verify_request`]; this one recognises only a
/// request signed without one.
pub fn verify(
    header: &str,
    keys: &[LinkKey],
    method: &str,
    path_and_query: &str,
    now: u64,
    guard: &ReplayGuard,
) -> Option<Verified> {
    verify_request(header, keys, method, path_and_query, &[], now, guard)
}

/// Recognise a signed request and its `body`: as [`verify`], and the body
/// must be the one signed — a request whose header names no body must have
/// none, and one that names a body must carry exactly it.
pub fn verify_request(
    header: &str,
    keys: &[LinkKey],
    method: &str,
    path_and_query: &str,
    body: &[u8],
    now: u64,
    guard: &ReplayGuard,
) -> Option<Verified> {
    let mut parts = header.split_whitespace();
    if parts.next()? != VERSION {
        return None;
    }
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for part in parts {
        let (name, value) = part.split_once('=')?;
        fields.insert(name, value);
    }
    let (key_id, ts, nonce, sig) = (
        *fields.get("key")?,
        fields.get("ts")?.parse::<u64>().ok()?,
        *fields.get("nonce")?,
        *fields.get("sig")?,
    );
    if nonce.len() < 16 || nonce.len() > 64 || now.abs_diff(ts) > MAX_SKEW {
        return None;
    }
    let signed_body = fields.get("body").copied();
    let actual_body = (!body.is_empty()).then(|| digest(body));
    match (signed_body, actual_body.as_deref()) {
        (None, None) => {}
        (Some(signed), Some(actual)) if bool::from(signed.as_bytes().ct_eq(actual.as_bytes())) => {}
        _ => return None,
    }
    let key = keys.iter().find(|k| k.id == key_id)?;
    let presented = B64.decode(sig).ok()?;
    let expected = key.mac(method, path_and_query, ts, nonce, signed_body);
    if !bool::from(presented.as_slice().ct_eq(&expected)) {
        return None;
    }
    // Only a request that is otherwise genuine spends its nonce: a forged
    // one must not be able to block the real one.
    guard
        .first_use(key_id, nonce, now)
        .then(|| Verified { key_id: key_id.into(), nonce: nonce.into() })
}

/// Seal an answer to the request with `nonce`: AES-256-GCM, a random IV, the
/// nonce authenticated alongside. `iv || ciphertext`.
pub fn seal(key: &LinkKey, nonce: &str, body: &[u8]) -> Result<Vec<u8>> {
    let cipher =
        Aes256Gcm::new_from_slice(&key.seal).map_err(|_| Error::internal("link seal key"))?;
    let iv = random::<12>();
    let aad = format!("{VERSION}\n{}\n{nonce}", key.id);
    let sealed = cipher
        .encrypt((&iv).into(), Payload { msg: body, aad: aad.as_bytes() })
        .map_err(|_| Error::internal("could not seal the answer"))?;
    Ok([iv.as_slice(), &sealed].concat())
}

/// Open an answer sealed for the request with `nonce`. Fails — one error for
/// every reason — when it was sealed under another key or for another
/// request, or altered on the way.
pub fn open(key: &LinkKey, nonce: &str, sealed: &[u8]) -> Result<Vec<u8>> {
    let failed = || Error::unauthenticated("The answer could not be opened.");
    if sealed.len() < 12 + 16 {
        return Err(failed());
    }
    let (iv, body) = sealed.split_at(12);
    let iv: [u8; 12] = iv.try_into().map_err(|_| failed())?;
    let cipher = Aes256Gcm::new_from_slice(&key.seal).map_err(|_| failed())?;
    let aad = format!("{VERSION}\n{}\n{nonce}", key.id);
    cipher.decrypt((&iv).into(), Payload { msg: body, aad: aad.as_bytes() }).map_err(|_| failed())
}

/// The nonces seen in the last two [`MAX_SKEW`]s, so a request is honoured
/// once. Bounded: when it holds [`ReplayGuard::CAPACITY`] it refuses new
/// requests until old ones age out, rather than forget one early.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    seen: Mutex<HashMap<String, u64>>,
}

impl ReplayGuard {
    /// The most nonces held at once.
    pub const CAPACITY: usize = 100_000;

    fn first_use(&self, key_id: &str, nonce: &str, now: u64) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= Self::CAPACITY / 2 {
            seen.retain(|_, at| now.abs_diff(*at) <= 2 * MAX_SKEW);
        }
        if seen.len() >= Self::CAPACITY {
            return false;
        }
        seen.insert(format!("{key_id}\n{nonce}"), now).is_none()
    }
}

fn random<const N: usize>() -> [u8; N] {
    use rand::Rng as _;
    let mut bytes = [0u8; N];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const PATH: &str = "/_lookout/api/traces/4bf92f3577b34da6a3ce929d0e0e4736";

    fn key(id: &str) -> LinkKey {
        LinkKey::new(id, format!("{id} a secret the fleet shares, long enough").into_bytes())
    }

    #[test]
    fn a_signed_request_is_recognised_once() {
        let k = key("a");
        let guard = ReplayGuard::default();
        let signed = sign(&k, "get", PATH, NOW);
        let seen =
            verify(&signed.header, std::slice::from_ref(&k), "GET", PATH, NOW + 5, &guard).unwrap();
        assert_eq!(seen, Verified { key_id: "a".into(), nonce: signed.nonce });
        assert!(verify(&signed.header, &[k], "GET", PATH, NOW + 5, &guard).is_none(), "replayed");
    }

    #[test]
    fn anything_else_is_not_recognised() {
        let k = key("a");
        let guard = ReplayGuard::default();
        let signed = sign(&k, "GET", PATH, NOW);
        let v = |header: &str, keys: &[LinkKey], method: &str, path: &str, now: u64| {
            verify(header, keys, method, path, now, &ReplayGuard::default())
        };
        assert!(v(&signed.header, &[key("b")], "GET", PATH, NOW).is_none(), "another key");
        assert!(
            v(&signed.header, std::slice::from_ref(&k), "POST", PATH, NOW).is_none(),
            "another method"
        );
        assert!(
            v(&signed.header, std::slice::from_ref(&k), "GET", "/_lookout/api/batches", NOW)
                .is_none(),
            "another path"
        );
        assert!(
            v(&signed.header, std::slice::from_ref(&k), "GET", PATH, NOW + MAX_SKEW + 1).is_none(),
            "too late"
        );
        assert!(v(&signed.header.replace("v1", "v2"), std::slice::from_ref(&k), "GET", PATH, NOW)
            .is_none());
        let forged = signed.header.rsplit_once("sig=").unwrap().0.to_string() + "sig=AAAA";
        assert!(v(&forged, std::slice::from_ref(&k), "GET", PATH, NOW).is_none(), "forged");
        // A forged request does not spend the real one's nonce.
        assert!(verify(&forged, std::slice::from_ref(&k), "GET", PATH, NOW, &guard).is_none());
        assert!(verify(&signed.header, &[k], "GET", PATH, NOW, &guard).is_some());
    }

    #[test]
    fn a_body_is_signed_and_cannot_be_changed_on_the_way() {
        let k = key("a");
        let body = br#"{"action":"resolve"}"#;
        let signed = sign_request(&k, "POST", "/_lookout/api/cases/c1", body, NOW);
        assert!(signed.header.contains(" body="));
        let v = |header: &str, body: &[u8]| {
            verify_request(
                header,
                std::slice::from_ref(&k),
                "POST",
                "/_lookout/api/cases/c1",
                body,
                NOW,
                &ReplayGuard::default(),
            )
        };
        assert!(v(&signed.header, body).is_some());
        assert!(v(&signed.header, br#"{"action":"ignore"}"#).is_none(), "another body");
        assert!(v(&signed.header, b"").is_none(), "the body taken off");
        let unsigned = sign(&k, "POST", "/_lookout/api/cases/c1", NOW);
        assert!(v(&unsigned.header, body).is_none(), "a body added to a request signed without");
        let moved = signed.header.replace(" body=", " body=A");
        assert!(v(&moved, body).is_none(), "a digest altered");
    }

    #[test]
    fn a_request_without_a_body_signs_as_it_always_did() {
        let k = key("a");
        let signed = sign_request(&k, "GET", PATH, b"", NOW);
        assert!(!signed.header.contains("body="));
        assert!(verify(&signed.header, &[k], "GET", PATH, NOW, &ReplayGuard::default()).is_some());
    }

    #[test]
    fn a_rotated_in_key_is_accepted_beside_the_old_one() {
        let (old, new) = (key("2026-10"), key("2026-11"));
        let signed = sign(&new, "GET", PATH, NOW);
        assert!(verify(&signed.header, &[old, new], "GET", PATH, NOW, &ReplayGuard::default())
            .is_some());
    }

    #[test]
    fn a_sealed_answer_opens_only_for_its_own_request_and_key() {
        let k = key("a");
        let sealed = seal(&k, "nonce-one-nonce-one", b"{\"data\":1}").unwrap();
        assert_eq!(open(&k, "nonce-one-nonce-one", &sealed).unwrap(), b"{\"data\":1}");
        assert!(open(&k, "nonce-two-nonce-two", &sealed).is_err(), "another request");
        assert!(open(&key("b"), "nonce-one-nonce-one", &sealed).is_err(), "another key");
        let mut altered = sealed.clone();
        *altered.last_mut().unwrap() ^= 1;
        assert!(open(&k, "nonce-one-nonce-one", &altered).is_err(), "altered");
        assert!(!sealed.windows(4).any(|w| w == b"data"), "not readable on the way");
    }

    #[test]
    fn keys_parse_from_configuration_and_never_print_their_secret() {
        let secret = B64.encode([7u8; 32]);
        let k = LinkKey::parse(&format!("2026-11:{secret}")).unwrap();
        assert_eq!(k.id(), "2026-11");
        assert!(!format!("{k:?}").contains("7"), "{k:?}");
        let std_b64 = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert_eq!(LinkKey::parse(&format!("2026-11:{std_b64}")).unwrap(), k, "either alphabet");
        assert_eq!(LinkKey::parse_list(&format!("a:{secret}, b:{secret}")).unwrap().len(), 2);
        assert!(LinkKey::parse("no-colon").is_err());
        assert!(LinkKey::parse(&format!("a:{}", B64.encode([1u8; 8]))).is_err(), "too short");
        assert!(LinkKey::parse(&format!("a b:{secret}")).is_err());
    }
}
