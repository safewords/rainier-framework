//! PKCE from the client's side — [`verifier`] and [`challenge`].
//!
//! The server checks PKCE; these make the other half, for a first-party
//! client, a test, or an example that plays the client.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// A fresh code verifier: 48 random bytes, base64url — 64 characters, inside
/// RFC 7636's 43–128.
pub fn verifier() -> String {
    use rand::Rng as _;

    let mut bytes = [0u8; 48];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    B64.encode(bytes)
}

/// The `S256` challenge for `verifier` — `BASE64URL(SHA256(verifier))`.
pub fn challenge(verifier: &str) -> String {
    B64.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_7636_appendix_b_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(verifier().len(), 64);
    }
}
