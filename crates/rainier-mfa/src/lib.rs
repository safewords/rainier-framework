//! # rainier-mfa
//!
//! The second factor: time-based one-time passwords with recovery codes, and
//! WebAuthn passkeys.
//!
//! | | | |
//! |---|---|---|
//! | [`totp`] | `totp` (default) | RFC 6238 codes, secrets, `otpauth://` URIs, replay refusal |
//! | [`recovery`] | `totp` | single-use recovery codes, stored hashed |
//! | [`webauthn`] | `passkeys` | a WebAuthn relying party: registration and assertion ceremonies |
//!
//! This crate is the cryptography and the ceremonies, and nothing else: no
//! routes, no tables, no session. It takes what was stored and what was
//! submitted, and answers whether they match and what to store next. The
//! account flows that put it behind a login live in `rainier-accounts`
//! (features `two-factor` and `passkeys`); an application with its own flows
//! uses this crate directly.
//!
//! ## No C, no OpenSSL
//!
//! The passkey signature algorithms — ES256, EdDSA and RS256 — come from the
//! RustCrypto crates, and the CBOR an authenticator speaks is read by a small
//! strict decoder here. So the crate builds wherever the rest of the framework
//! does, a Worker included.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod base32;
#[cfg(feature = "totp")]
pub mod recovery;
#[cfg(feature = "totp")]
pub mod totp;

#[cfg(feature = "passkeys")]
pub mod cbor;
#[cfg(feature = "passkeys")]
pub mod cose;
#[cfg(feature = "passkeys")]
pub mod webauthn;

#[cfg(feature = "totp")]
pub use totp::{Secret, Totp};
#[cfg(feature = "passkeys")]
pub use webauthn::{
    AuthenticationResponse, AuthenticationState, PasskeyCredential, PasskeyUser,
    RegistrationResponse, RegistrationState, RelyingParty, VerifiedAssertion,
};

/// `len` bytes from the operating system's random source.
#[cfg(any(feature = "totp", feature = "passkeys"))]
pub(crate) fn random_bytes(len: usize) -> Vec<u8> {
    use rand::Rng as _;

    let mut bytes = vec![0u8; len];
    rand::rand_core::UnwrapErr(rand::rngs::SysRng).fill_bytes(&mut bytes);
    bytes
}
