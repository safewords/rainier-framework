//! defuse/php-encryption v2 ciphertexts — read and written.
//!
//! The library PHP OAuth servers (league/oauth2-server) seal their refresh
//! tokens and authorization codes with. A server taking over from one has to
//! open what it sealed, or every refresh token in the field stops working at
//! the cutover; this is that.
//!
//! # The format
//!
//! ```text
//! hex( DEF5 0200 ‖ salt[32] ‖ iv[16] ‖ AES-256-CTR(plaintext) ‖ HMAC-SHA256[32] )
//! ```
//!
//! The MAC covers everything before it — the version header, salt, IV and
//! ciphertext. Two subkeys come out of HKDF-SHA256 with the salt as HKDF salt
//! and the info strings `DefusePHP|V2|KeyForAuthentication` and
//! `DefusePHP|V2|KeyForEncryption`. Their input key material is:
//!
//! - for a **key** (`Crypto::encrypt`) — the 32 raw key bytes;
//! - for a **password** (`Crypto::encryptWithPassword`) —
//!   `PBKDF2-SHA256(SHA256(password), salt, 100 000 iterations, 32 bytes)`.
//!
//! The PHP OAuth servers use the **password** form, with the application's
//! decoded 32-byte encryption key as the password. The prehash is defuse's
//! defence against PBKDF2's long-password DoS; the 100 000 iterations mean
//! each open costs a few milliseconds, which is the format's cost, not this
//! implementation's.
//!
//! # Failures
//!
//! The MAC is checked — in constant time — **before** anything is decrypted,
//! and every failure (bad hex, short input, wrong version, wrong key,
//! tampering) is one error with one message. A format that told a caller
//! *why* it refused would be telling an attacker which byte they got right.
//!
//! The legacy v1 format (`Crypto::legacyDecrypt`, AES-128-CBC) is not read:
//! nothing written in the last decade uses it.

use aes::Aes256;
use ctr::cipher::{KeyIvInit, StreamCipher};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use rainier_support::{Error, Result};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;
type Aes256Ctr = ctr::Ctr128BE<Aes256>;

/// The v2 version header.
const VERSION: [u8; 4] = [0xDE, 0xF5, 0x02, 0x00];
const SALT_LEN: usize = 32;
const IV_LEN: usize = 16;
const MAC_LEN: usize = 32;
const MIN_LEN: usize = VERSION.len() + SALT_LEN + IV_LEN + MAC_LEN;
const PBKDF2_ITERATIONS: u32 = 100_000;
const AUTH_INFO: &[u8] = b"DefusePHP|V2|KeyForAuthentication";
const ENC_INFO: &[u8] = b"DefusePHP|V2|KeyForEncryption";

/// What the input key material is.
enum Secret<'a> {
    Key(&'a [u8; 32]),
    Password(&'a [u8]),
}

/// Encrypt with a password, as `Crypto::encryptWithPassword` — hex-encoded.
pub fn encrypt_with_password(plaintext: &[u8], password: &[u8]) -> String {
    hex(&encrypt_raw(plaintext, Secret::Password(password)))
}

/// Decrypt what `Crypto::encryptWithPassword` produced.
pub fn decrypt_with_password(ciphertext: &str, password: &[u8]) -> Result<Vec<u8>> {
    decrypt_raw(&unhex(ciphertext).ok_or_else(refused)?, Secret::Password(password))
}

/// Encrypt with a 32-byte key, as `Crypto::encrypt` — hex-encoded.
pub fn encrypt_with_key(plaintext: &[u8], key: &[u8; 32]) -> String {
    hex(&encrypt_raw(plaintext, Secret::Key(key)))
}

/// Decrypt what `Crypto::encrypt` produced.
pub fn decrypt_with_key(ciphertext: &str, key: &[u8; 32]) -> Result<Vec<u8>> {
    decrypt_raw(&unhex(ciphertext).ok_or_else(refused)?, Secret::Key(key))
}

/// [`encrypt_with_password`] without the hex — `$raw_binary = true`.
pub fn encrypt_with_password_raw(plaintext: &[u8], password: &[u8]) -> Vec<u8> {
    encrypt_raw(plaintext, Secret::Password(password))
}

/// [`decrypt_with_password`] without the hex.
pub fn decrypt_with_password_raw(ciphertext: &[u8], password: &[u8]) -> Result<Vec<u8>> {
    decrypt_raw(ciphertext, Secret::Password(password))
}

/// [`encrypt_with_key`] without the hex.
pub fn encrypt_with_key_raw(plaintext: &[u8], key: &[u8; 32]) -> Vec<u8> {
    encrypt_raw(plaintext, Secret::Key(key))
}

/// [`decrypt_with_key`] without the hex.
pub fn decrypt_with_key_raw(ciphertext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    decrypt_raw(ciphertext, Secret::Key(key))
}

fn encrypt_raw(plaintext: &[u8], secret: Secret<'_>) -> Vec<u8> {
    let mut salt = [0u8; SALT_LEN];
    let mut iv = [0u8; IV_LEN];
    fill_random(&mut salt);
    fill_random(&mut iv);
    seal(plaintext, &secret, &salt, &iv)
}

/// The deterministic half of encryption, so a test can pin output for a
/// fixed salt and IV.
fn seal(
    plaintext: &[u8],
    secret: &Secret<'_>,
    salt: &[u8; SALT_LEN],
    iv: &[u8; IV_LEN],
) -> Vec<u8> {
    let (auth_key, enc_key) = derive(secret, salt);

    let mut body = plaintext.to_vec();
    Aes256Ctr::new_from_slices(&enc_key, iv)
        .expect("a 32-byte key and a 16-byte IV are the sizes AES-256-CTR takes")
        .apply_keystream(&mut body);

    let mut out = Vec::with_capacity(MIN_LEN + body.len());
    out.extend_from_slice(&VERSION);
    out.extend_from_slice(salt);
    out.extend_from_slice(iv);
    out.extend_from_slice(&body);

    let mut mac = HmacSha256::new_from_slice(&auth_key).expect("HMAC takes a key of any length");
    mac.update(&out);
    out.extend_from_slice(&mac.finalize().into_bytes());
    out
}

fn decrypt_raw(ciphertext: &[u8], secret: Secret<'_>) -> Result<Vec<u8>> {
    if ciphertext.len() < MIN_LEN || ciphertext[..VERSION.len()] != VERSION {
        return Err(refused());
    }

    let salt: &[u8; SALT_LEN] =
        ciphertext[VERSION.len()..VERSION.len() + SALT_LEN].try_into().map_err(|_| refused())?;
    let iv = &ciphertext[VERSION.len() + SALT_LEN..VERSION.len() + SALT_LEN + IV_LEN];
    let (signed, tag) = ciphertext.split_at(ciphertext.len() - MAC_LEN);
    let body = &signed[VERSION.len() + SALT_LEN + IV_LEN..];

    let (auth_key, enc_key) = derive(&secret, salt);

    // Authenticate first, in constant time, and only then decrypt.
    let mut mac = HmacSha256::new_from_slice(&auth_key).expect("HMAC takes a key of any length");
    mac.update(signed);
    mac.verify_slice(tag).map_err(|_| refused())?;

    let mut plain = body.to_vec();
    Aes256Ctr::new_from_slices(&enc_key, iv).map_err(|_| refused())?.apply_keystream(&mut plain);
    Ok(plain)
}

/// The authentication and encryption subkeys.
fn derive(secret: &Secret<'_>, salt: &[u8; SALT_LEN]) -> ([u8; 32], [u8; 32]) {
    let prekey: [u8; 32] = match secret {
        Secret::Key(key) => **key,
        Secret::Password(password) => {
            let prehash = Sha256::digest(password);
            let mut prekey = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(&prehash, salt, PBKDF2_ITERATIONS, &mut prekey);
            prekey
        }
    };

    let hkdf = Hkdf::<Sha256>::new(Some(salt), &prekey);
    let mut auth_key = [0u8; 32];
    let mut enc_key = [0u8; 32];
    hkdf.expand(AUTH_INFO, &mut auth_key).expect("32 bytes is within HKDF-SHA256's output");
    hkdf.expand(ENC_INFO, &mut enc_key).expect("32 bytes is within HKDF-SHA256's output");
    (auth_key, enc_key)
}

/// The one error every failure is.
fn refused() -> Error {
    Error::bad_request("The ciphertext could not be opened: wrong key or modified ciphertext.")
}

fn fill_random(buffer: &mut [u8]) {
    use rand::Rng as _;
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(buffer);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Lowercase or uppercase hex; anything else is `None`.
fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trip_and_the_shape_of_what_it_writes() {
        let sealed = encrypt_with_password(b"{\"refresh_token_id\":\"abc\"}", b"app key");
        assert!(sealed.starts_with("def50200"), "the v2 header, hex: {sealed}");
        assert_eq!(sealed.len(), 2 * (MIN_LEN + 26));
        assert_eq!(
            decrypt_with_password(&sealed, b"app key").unwrap(),
            b"{\"refresh_token_id\":\"abc\"}"
        );
    }

    #[test]
    fn a_key_round_trip() {
        let key = [7u8; 32];
        let sealed = encrypt_with_key(b"hello", &key);
        assert_eq!(decrypt_with_key(&sealed, &key).unwrap(), b"hello");
        assert!(decrypt_with_key(&sealed, &[8u8; 32]).is_err());
    }

    #[test]
    fn every_failure_is_the_same_error() {
        let sealed = encrypt_with_password(b"secret", b"right");
        let message = |r: Result<Vec<u8>>| r.unwrap_err().message().to_string();

        let wrong_key = message(decrypt_with_password(&sealed, b"wrong"));
        let mut flipped = sealed.clone().into_bytes();
        let last = flipped.len() - 70; // a ciphertext byte, not the MAC
        flipped[last] = if flipped[last] == b'0' { b'1' } else { b'0' };
        let tampered =
            message(decrypt_with_password(std::str::from_utf8(&flipped).unwrap(), b"right"));
        let short = message(decrypt_with_password("def50200", b"right"));
        let not_hex = message(decrypt_with_password("zz", b"right"));
        let wrong_version =
            message(decrypt_with_password(&sealed.replacen("def502", "def501", 1), b"right"));

        for other in [&tampered, &short, &not_hex, &wrong_version] {
            assert_eq!(&wrong_key, other);
        }
    }

    #[test]
    fn an_empty_plaintext_is_legal() {
        let sealed = encrypt_with_password(b"", b"k");
        assert_eq!(decrypt_with_password(&sealed, b"k").unwrap(), b"");
    }

    /// Pinned output for a fixed salt and IV, computed independently with
    /// Python's `hashlib`/`hmac` and `cryptography` (AES-CTR) following the
    /// PHP source step by step — see the module docs for the construction.
    /// If this changes, everything ever sealed stops opening.
    #[test]
    fn a_fixed_salt_and_iv_give_the_pinned_ciphertext() {
        let salt = [0x11u8; SALT_LEN];
        let iv = [0x22u8; IV_LEN];
        let sealed = hex(&seal(b"rainier", &Secret::Password(b"password"), &salt, &iv));
        assert_eq!(sealed, PINNED_PASSWORD_VECTOR);
        let sealed = hex(&seal(b"rainier", &Secret::Key(&[0x33u8; 32]), &salt, &iv));
        assert_eq!(sealed, PINNED_KEY_VECTOR);
    }

    const PINNED_PASSWORD_VECTOR: &str = "def502001111111111111111111111111111111111111111111111111111111111111111222222222222222222222222222222226b67cf8988f93f7115b7cb9f0f8884285497e338044e93a48436447dd9471af534807fa2cbdc59";
    const PINNED_KEY_VECTOR: &str = "def50200111111111111111111111111111111111111111111111111111111111111111122222222222222222222222222222222d23e19a25235f77500ad5908134cdb3d3abc8964371ed57bf70ade53714e45e6801a2ad9ed03a8";
}
