//! PHP — the formats a PHP application writes into the state it shares with
//! you.
//!
//! | Module | What |
//! |---|---|
//! | [`serialize`] | the `serialize()` codec everything else here sits on |
//! | [`defuse`] | defuse/php-encryption ciphertexts |
//! | [`cookie`] | encrypted cookies |
//! | [`session`] | sessions and remember-me cookies |
//! | [`cache`] | cache values |
//! | [`csrf`] | the sealed anti-forgery cookie |
//! | [`queue`] | queued job payloads |
//!
//! The application key is the thread through most of it: the same
//! `APP_KEY` the PHP side runs with, handed to Rainier as a
//! [`KeyRing`](rainier_crypt::KeyRing) — current key first, previous keys
//! after, exactly as they rotate there.

pub mod cache;
pub mod cookie;
pub mod csrf;
pub mod defuse;
pub mod queue;
pub mod serialize;
pub mod session;

#[cfg(feature = "php-oauth")]
pub mod oauth;
#[cfg(feature = "php-webauthn")]
pub mod webauthn;

#[cfg(test)]
mod fixtures;
