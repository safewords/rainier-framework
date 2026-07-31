//! A token verifies against nothing but the published JWKS.
//!
//! The unit tests in `jwt` sign and verify with the same object, which proves
//! self-consistency and nothing about interoperability. This does what a
//! relying party does: takes the JWKS document, rebuilds the public key from
//! the `n` and `e` in it, and checks the signature — with no help from the
//! signing side at all.
//!
//! It caught nothing when it was written. It exists for the change that
//! quietly reorders a big-endian byte string, or emits base64 instead of
//! base64url, and leaves every Rainier-to-Rainier test passing while every
//! real relying party rejects the token.
//!
//! The same round trip was also checked once by hand against Python's
//! `cryptography`, which is a genuinely separate implementation. This is the
//! Rust version of it, so it runs on every commit.
#![cfg(feature = "jwt")]

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine as _;
use rainier_crypt::jwt::{Jwt, JwtKey, JwtKeyRing};
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::signature::Verifier;
use rsa::{BigUint, RsaPublicKey};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;

#[derive(Serialize)]
struct Claims {
    sub: &'static str,
    exp: i64,
}

/// Rebuild an RSA public key from a JWKS entry, the way a relying party does.
fn public_key_from_jwk(jwk: &Value) -> RsaPublicKey {
    let decode = |field: &str| {
        B64URL
            .decode(jwk[field].as_str().unwrap_or_else(|| panic!("`{field}` should be a string")))
            .unwrap_or_else(|_| panic!("`{field}` should be base64url"))
    };

    RsaPublicKey::new(BigUint::from_bytes_be(&decode("n")), BigUint::from_bytes_be(&decode("e")))
        .expect("the modulus and exponent should form a key")
}

#[test]
fn a_relying_party_can_verify_using_only_the_jwks() {
    let ring = JwtKeyRing::new(JwtKey::generate_rs256("interop", 2048).expect("generate"));
    let jwks = ring.jwks();
    let jwt = Jwt::new(ring);

    let token = jwt.sign(&Claims { sub: "user-42", exp: 4_102_444_800 }).expect("sign");

    // Everything from here on uses the document and the token, and nothing
    // else — no `DecodingKey`, no access to the private half.
    let (header, payload, signature) = {
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT is three segments");
        (parts[0], parts[1], parts[2])
    };

    let jwk = &jwks["keys"][0];
    assert_eq!(jwk["kid"], "interop");

    let verifying = VerifyingKey::<Sha256>::new(public_key_from_jwk(jwk));
    let signature = Signature::try_from(B64URL.decode(signature).expect("base64url").as_slice())
        .expect("a signature");

    verifying
        .verify(format!("{header}.{payload}").as_bytes(), &signature)
        .expect("the published key should verify the token it signed");
}

#[test]
fn the_header_names_a_key_the_jwks_lists() {
    // How a relying party picks the right key. A `kid` in the token that the
    // document does not list is a rotation somebody published wrong.
    let ring = JwtKeyRing::new(JwtKey::generate_es256("current").expect("generate"))
        .with_previous(JwtKey::generate_es256("previous").expect("generate"));

    let jwks = ring.jwks();
    let jwt = Jwt::new(ring);
    let token = jwt.sign(&Claims { sub: "user-42", exp: 4_102_444_800 }).expect("sign");

    let kid = Jwt::kid_of(&token).expect("a kid");
    let listed: Vec<&str> =
        jwks["keys"].as_array().unwrap().iter().map(|key| key["kid"].as_str().unwrap()).collect();

    assert!(listed.contains(&kid.as_str()), "{kid} is not in {listed:?}");
    assert_eq!(listed, vec!["current", "previous"]);
}

#[test]
fn a_jwks_carries_no_private_material() {
    // The document is served to the internet. A private exponent in it is the
    // whole key.
    let ring = JwtKeyRing::new(JwtKey::generate_rs256("k", 2048).expect("generate"))
        .with_previous(JwtKey::generate_es256("e").expect("generate"));

    let published = serde_json::to_string(&ring.jwks()).expect("serialise");

    // The RSA private components, and the EC one.
    for field in ["\"d\"", "\"p\"", "\"q\"", "\"dp\"", "\"dq\"", "\"qi\""] {
        assert!(!published.contains(field), "the JWKS contains {field}");
    }
    assert!(!published.contains("PRIVATE"));
}
