//! The ciphers that run under the PHP envelope — and nothing else.
//!
//! Bytes in, bytes out: no JSON, no base64, no hex, no key ring, no policy
//! about which failures look alike. The other half of the split is
//! [`envelope`](super::envelope), which owns the encoding and runs none of
//! this; [`PhpEncrypter`](super::PhpEncrypter) is the only place the two
//! halves meet.
//!
//! **Raw keys, deliberately.** PHP's encrypter feeds `APP_KEY` straight to
//! `openssl_encrypt`, so compatibility means doing the same. That is the
//! opposite of [`Cipher`](crate::Cipher), which HKDF-derives a per-algorithm
//! subkey for the native envelope — and it is why these functions exist
//! instead of reusing it.

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::Mac as _;
use rainier_support::{Error, Result};

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// AES-256-CBC's block and IV size.
pub(super) const CBC_IV_LEN: usize = 16;

/// AES-256-GCM's nonce size.
pub(super) const GCM_IV_LEN: usize = 12;

/// AES-256-GCM's authentication-tag size.
pub(super) const GCM_TAG_LEN: usize = 16;

/// AES-256-CBC with PKCS#7 padding.
pub(super) fn cbc_encrypt(key: &[u8], iv: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
    Ok(Aes256CbcEnc::new_from_slices(key, iv)
        .map_err(|_| Error::internal("the key or IV is the wrong length for AES-256-CBC"))?
        .encrypt_padded_vec_mut::<Pkcs7>(plain))
}

/// The inverse. **Only** call this after the MAC over the envelope has been
/// verified — CBC with an unauthenticated ciphertext is a padding oracle,
/// and this function cannot know whether its caller checked.
pub(super) fn cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    Aes256CbcDec::new_from_slices(key, iv)
        .map_err(|_| Error::internal("the key or IV is the wrong length for AES-256-CBC"))?
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| Error::internal("the ciphertext did not decrypt"))
}

/// AES-256-GCM, with the tag returned separately.
///
/// Separately because that is this primitive's contract, not because of any
/// envelope: the `aes-gcm` crate's concatenated `ciphertext || tag` is a
/// library artefact, and a caller assembling a format that keeps the two
/// apart should not have to know how long a tag is.
pub(super) fn gcm_encrypt(key: &[u8], nonce: &[u8], plain: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    use aes_gcm::aead::Aead as _;

    if nonce.len() != GCM_IV_LEN {
        return Err(Error::internal(format!(
            "an AES-GCM nonce must be {GCM_IV_LEN} bytes, got {}",
            nonce.len()
        )));
    }

    let cipher: aes_gcm::Aes256Gcm = aes_gcm::KeyInit::new_from_slice(key)
        .map_err(|_| Error::internal("the key is the wrong length for AES-256-GCM"))?;

    let mut sealed = cipher
        .encrypt(nonce.into(), plain)
        .map_err(|_| Error::internal("encryption failed"))?;

    let tag = sealed.split_off(sealed.len() - GCM_TAG_LEN);
    Ok((sealed, tag))
}

/// The inverse, verifying the tag.
pub(super) fn gcm_decrypt(
    key: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
    tag: &[u8],
) -> Result<Vec<u8>> {
    use aes_gcm::aead::Aead as _;

    if nonce.len() != GCM_IV_LEN || tag.len() != GCM_TAG_LEN {
        return Err(Error::internal("the nonce or tag is the wrong length"));
    }

    let cipher: aes_gcm::Aes256Gcm = aes_gcm::KeyInit::new_from_slice(key)
        .map_err(|_| Error::internal("the key is the wrong length for AES-256-GCM"))?;

    let mut sealed = ciphertext.to_vec();
    sealed.extend_from_slice(tag);

    cipher
        .decrypt(nonce.into(), sealed.as_ref())
        .map_err(|_| Error::internal("the ciphertext failed authentication"))
}

/// HMAC-SHA256 over `covers`, raw bytes out.
///
/// What `covers` is — the base64 forms of the IV and ciphertext, concatenated
/// — is the envelope's rule, stated there. This function just runs the MAC.
pub(super) fn mac(key: &[u8], covers: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| Error::internal("the key is the wrong length for HMAC-SHA256"))?;

    mac.update(covers);

    Ok(mac.finalize().into_bytes().to_vec())
}
