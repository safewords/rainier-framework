//! The `{iv, value, mac, tag}` wire format — and nothing else.
//!
//! This module can encode a payload, decode one, and say **exactly which
//! bytes a MAC must cover**. It runs no cipher and computes no MAC: whether
//! the covered bytes check out is [`PhpEncrypter`](super::PhpEncrypter)'s
//! business, and which cipher opens the payload is
//! [`primitive`](super::primitive)'s. Keeping the codec free of cryptography
//! is what keeps "compatibility with PHP's format" from being welded to
//! "AES", which are different facts.
//!
//! # The format
//!
//! ```text
//! base64( json( { "iv": base64(iv), "value": base64(ciphertext),
//!                 "mac": hex(...), "tag": base64(...) } ) )
//! ```
//!
//! `mac` and `tag` tell the variants apart on the way in:
//!
//! | Variant | `iv` | `mac` | `tag` |
//! |---|---|---|---|
//! | CBC (what PHP ≤ its GCM era writes) | 16 bytes | required, hex | absent |
//! | GCM (what a GCM-configured app writes) | 12 bytes | `""` on the wire | required |
//!
//! On the way out the GCM variant writes `"mac": ""` because that is the
//! byte-for-byte shape PHP produces — its `validPayload` insists the key
//! *exists* even where the tag does the authenticating. On the way in an
//! **absent** `mac` is accepted for GCM too, because at least one earlier
//! reimplementation omitted it and its rows still have to open.
//!
//! # The one rule everybody gets wrong
//!
//! The CBC MAC covers `iv_base64 || value_base64` — the base64 **strings as
//! they appear in the JSON**, concatenated, not the raw bytes. That is a fact
//! about this *encoding*, which is why the rule lives here: the envelope
//! hands the covered bytes out, and no caller re-derives them.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};

use super::primitive::GCM_TAG_LEN;

/// The JSON inside the outer base64.
#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    iv: String,
    value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

/// A decoded payload, dispatched by variant.
pub(super) enum Opened {
    /// The `{iv, value, mac}` half.
    Cbc {
        iv: Vec<u8>,
        ciphertext: Vec<u8>,
        /// The MAC the payload presented, hex-decoded to raw bytes.
        presented_mac: Vec<u8>,
        /// The exact bytes a verifier must MAC and compare.
        mac_covers: Vec<u8>,
    },
    /// The `{iv, value, tag}` half. Any `mac` beside a tag is ignored, as
    /// PHP's own decrypt ignores it for AEAD ciphers.
    Gcm {
        iv: Vec<u8>,
        ciphertext: Vec<u8>,
        tag: Vec<u8>,
    },
}

/// One error for every malformation, so nothing distinguishes bad base64
/// from bad JSON from a missing field.
fn invalid() -> Error {
    Error::internal("could not decrypt the payload")
}

/// Decode a payload into its variant.
pub(super) fn decode(payload: &str) -> Result<Opened> {
    let decoded = B64.decode(payload).map_err(|_| invalid())?;
    let wire: Wire = serde_json::from_slice(&decoded).map_err(|_| invalid())?;

    let iv = B64.decode(&wire.iv).map_err(|_| invalid())?;
    let ciphertext = B64.decode(&wire.value).map_err(|_| invalid())?;

    if let Some(tag) = wire.tag.as_deref().filter(|tag| !tag.is_empty()) {
        return Ok(Opened::Gcm {
            iv,
            ciphertext,
            tag: B64.decode(tag).map_err(|_| invalid())?,
        });
    }

    let presented = wire.mac.as_deref().filter(|mac| !mac.is_empty()).ok_or_else(invalid)?;

    Ok(Opened::Cbc {
        presented_mac: hex_decode(presented).ok_or_else(invalid)?,
        mac_covers: mac_covers(&wire.iv, &wire.value),
        iv,
        ciphertext,
    })
}

/// A CBC payload part-way through encoding: the base64 forms exist, the MAC
/// does not yet.
///
/// Two steps rather than one function, because the MAC covers the *encoded*
/// strings — so the encoder has to hand them out, let the caller compute the
/// MAC with whatever key it holds, and take the result back. That hand-off is
/// the seam between this module and the cryptography.
pub(super) struct CbcDraft {
    iv: String,
    value: String,
}

impl CbcDraft {
    /// Encode `iv` and `ciphertext`.
    pub(super) fn new(iv: &[u8], ciphertext: &[u8]) -> Self {
        Self { iv: B64.encode(iv), value: B64.encode(ciphertext) }
    }

    /// The exact bytes the MAC must cover.
    pub(super) fn mac_covers(&self) -> Vec<u8> {
        mac_covers(&self.iv, &self.value)
    }

    /// Seal with the computed MAC (raw bytes; hex is this module's job).
    pub(super) fn seal(self, mac: &[u8]) -> Result<String> {
        let wire = Wire {
            iv: self.iv,
            value: self.value,
            mac: Some(hex_encode(mac)),
            tag: None,
        };

        Ok(B64.encode(serde_json::to_vec(&wire)?))
    }
}

/// Encode a GCM payload.
///
/// `"mac": ""` goes on the wire — the byte-for-byte shape PHP writes, and
/// the difference between a payload its `validPayload` accepts and one it
/// refuses before looking at the tag.
pub(super) fn encode_gcm(iv: &[u8], ciphertext: &[u8], tag: &[u8]) -> Result<String> {
    debug_assert_eq!(tag.len(), GCM_TAG_LEN);

    let wire = Wire {
        iv: B64.encode(iv),
        value: B64.encode(ciphertext),
        mac: Some(String::new()),
        tag: Some(B64.encode(tag)),
    };

    Ok(B64.encode(serde_json::to_vec(&wire)?))
}

/// `iv_base64 || value_base64`, the concatenation the CBC MAC covers.
fn mac_covers(iv_b64: &str, value_b64: &str) -> Vec<u8> {
    let mut covers = Vec::with_capacity(iv_b64.len() + value_b64.len());
    covers.extend_from_slice(iv_b64.as_bytes());
    covers.extend_from_slice(value_b64.as_bytes());
    covers
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }

    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mac_covers_the_encoded_strings_and_not_the_bytes() {
        // The detail every reimplementation gets wrong. Asserted at the codec,
        // because the codec is where the rule lives now.
        let draft = CbcDraft::new(&[7u8; 16], b"ciphertext");

        let expected =
            format!("{}{}", B64.encode([7u8; 16]), B64.encode(b"ciphertext")).into_bytes();
        assert_eq!(draft.mac_covers(), expected);
    }

    #[test]
    fn a_round_trip_through_the_codec_is_lossless() {
        let sealed = CbcDraft::new(&[1u8; 16], b"ct").seal(&[0xAB; 32]).unwrap();

        match decode(&sealed).unwrap() {
            Opened::Cbc { iv, ciphertext, presented_mac, .. } => {
                assert_eq!(iv, vec![1u8; 16]);
                assert_eq!(ciphertext, b"ct");
                assert_eq!(presented_mac, vec![0xAB; 32]);
            }
            Opened::Gcm { .. } => panic!("a CBC payload decoded as GCM"),
        }
    }

    #[test]
    fn a_tag_selects_the_gcm_variant_and_an_empty_mac_is_tolerated() {
        let sealed = encode_gcm(&[2u8; 12], b"ct", &[3u8; 16]).unwrap();

        // The wire carries the empty mac PHP writes…
        let json: serde_json::Value =
            serde_json::from_slice(&B64.decode(&sealed).unwrap()).unwrap();
        assert_eq!(json["mac"], "");

        // …and decodes as GCM regardless.
        assert!(matches!(decode(&sealed).unwrap(), Opened::Gcm { .. }));
    }

    #[test]
    fn a_gcm_payload_with_no_mac_key_at_all_still_decodes() {
        // At least one earlier reimplementation omitted the key entirely, and
        // its rows still have to open.
        let json = serde_json::json!({
            "iv": B64.encode([2u8; 12]),
            "value": B64.encode(b"ct"),
            "tag": B64.encode([3u8; 16]),
        });
        let sealed = B64.encode(serde_json::to_vec(&json).unwrap());

        assert!(matches!(decode(&sealed).unwrap(), Opened::Gcm { .. }));
    }

    #[test]
    fn a_cbc_payload_without_a_mac_is_refused_at_the_codec() {
        // No tag and no MAC is a payload nothing can authenticate, and it must
        // not reach a cipher.
        let json = serde_json::json!({
            "iv": B64.encode([1u8; 16]),
            "value": B64.encode(b"ct"),
        });
        let sealed = B64.encode(serde_json::to_vec(&json).unwrap());

        assert!(decode(&sealed).is_err());
    }

    #[test]
    fn nonsense_is_an_error_rather_than_a_panic() {
        for payload in ["", "not base64 !!", &B64.encode("not json"), &B64.encode("{}")] {
            assert!(decode(payload).is_err(), "{payload:?}");
        }
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex_decode(&hex_encode(&[0, 15, 16, 255])), Some(vec![0, 15, 16, 255]));
        assert_eq!(hex_decode("abc"), None, "an odd length is not hex");
        assert_eq!(hex_decode("zz"), None);
    }
}
