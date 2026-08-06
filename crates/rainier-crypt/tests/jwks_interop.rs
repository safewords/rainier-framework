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

/// The framework's own JWKS reader agrees with the hand-rolled one above.
///
/// The test above rebuilds the key from `n` and `e` by hand, which is what a
/// relying party used to have to do. `JwtKeyRing::from_jwks` exists so nobody
/// writes that twice — and this asserts the two reach the same verdict on the
/// same document, so a change to the reader cannot quietly stop matching the
/// interop check that guards the wire format.
#[test]
fn a_ring_rebuilt_from_the_published_jwks_verifies_the_token() {
    let signer = Jwt::new(JwtKeyRing::new(JwtKey::generate_rs256("k1", 2048).unwrap()))
        .issued_by("https://issuer.test")
        .for_audience("interop");

    #[derive(Serialize, serde::Deserialize)]
    struct Claims {
        iss: String,
        aud: String,
        sub: String,
        exp: i64,
    }

    let token = signer
        .sign(&Claims {
            iss: "https://issuer.test".into(),
            aud: "interop".into(),
            sub: "client:4".into(),
            exp: 9_999_999_999,
        })
        .unwrap();

    // Only the published document crosses this line — no help from the
    // signing side, which is the whole point.
    let verifier = Jwt::new(JwtKeyRing::from_jwks(&signer.jwks()).unwrap())
        .issued_by("https://issuer.test")
        .for_audience("interop");

    let claims: Claims = verifier.verify(&token).expect("the published key verifies the token");
    assert_eq!(claims.sub, "client:4");
}

#[test]
fn a_ring_rebuilt_from_a_jwks_cannot_sign() {
    // The failure this guards is a service that verifies somebody else's
    // tokens and then tries to mint one, getting an error about the key rather
    // than about what it asked for.
    let signer = Jwt::new(JwtKeyRing::new(JwtKey::generate_rs256("k1", 2048).unwrap()));
    let verifier = Jwt::new(JwtKeyRing::from_jwks(&signer.jwks()).unwrap());

    #[derive(Serialize)]
    struct Claims {
        sub: String,
    }

    let error = verifier.sign(&Claims { sub: "nope".into() }).expect_err("cannot sign");
    assert!(error.message().contains("verify but not sign"), "{}", error.message());
}

#[test]
fn an_es256_key_survives_the_same_round_trip() {
    // The EC branch decodes an affine point rather than two bignums, so it
    // fails differently from RSA and is worth its own pass.
    let signer = Jwt::new(JwtKeyRing::new(JwtKey::generate_es256("ec1").unwrap()));

    #[derive(Serialize, serde::Deserialize)]
    struct Claims {
        sub: String,
        exp: i64,
    }

    let token = signer.sign(&Claims { sub: "client:4".into(), exp: 9_999_999_999 }).unwrap();
    let verifier = Jwt::new(JwtKeyRing::from_jwks(&signer.jwks()).unwrap());

    let claims: Claims = verifier.verify(&token).expect("the published EC key verifies");
    assert_eq!(claims.sub, "client:4");
}

#[test]
fn a_key_type_the_verifier_does_not_know_is_skipped_and_not_fatal() {
    // An issuer adding a key type before its relying parties understand it
    // must not take them all down. The unknown entry is dropped; the one that
    // can verify still does.
    let signer = Jwt::new(JwtKeyRing::new(JwtKey::generate_rs256("k1", 2048).unwrap()));

    let mut document = signer.jwks();
    document["keys"].as_array_mut().unwrap().push(serde_json::json!({
        "kty": "OKP", "crv": "Ed25519", "kid": "future", "alg": "EdDSA", "x": "AAAA",
    }));

    let ring = JwtKeyRing::from_jwks(&document).expect("the usable key is still read");

    assert_eq!(ring.ids(), vec!["k1"]);
}

#[test]
fn a_document_with_nothing_usable_is_an_error() {
    // Distinguishable from a successful fetch. A silently empty ring rejects
    // every token with "names a key this service does not hold", which sends
    // whoever debugs it looking at the issuer's signing key.
    let empty = serde_json::json!({"keys": []});

    assert!(JwtKeyRing::from_jwks(&empty).is_err());
    assert!(JwtKeyRing::from_jwks(&serde_json::json!({})).is_err());
}
