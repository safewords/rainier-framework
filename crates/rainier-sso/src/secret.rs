//! The random values a sign-in round trip carries — `state`, the PKCE
//! verifier, the OIDC `nonce` — and the PKCE challenge derived from one.
//!
//! All three come from the OS CSPRNG. Each is a bearer secret for the few
//! seconds it lives: whoever can predict a `state` can complete somebody
//! else's sign-in into their own session (login CSRF), and whoever can predict
//! a verifier can redeem an intercepted code.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// 256 bits of randomness, base64url without padding — 43 characters.
///
/// 43 is also the minimum length RFC 7636 allows for a PKCE verifier, so one
/// generator serves all three values.
pub fn random_token() -> String {
    use rand::Rng as _;

    let mut bytes = [0u8; 32];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    B64URL.encode(bytes)
}

/// The `S256` code challenge for `verifier` — `BASE64URL(SHA256(verifier))`.
///
/// `S256` only. RFC 7636 allows `plain`, which sends the verifier itself
/// through the browser and so protects against nothing PKCE exists for.
pub fn code_challenge(verifier: &str) -> String {
    B64URL.encode(Sha256::digest(verifier.as_bytes()))
}

/// Whether two secrets are equal, in time independent of where they differ.
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_enough_for_pkce_and_do_not_repeat() {
        let a = random_token();
        let b = random_token();

        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn the_challenge_matches_the_rfc_7636_example() {
        // Appendix B of RFC 7636.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn comparison_is_exact() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
    }
}
