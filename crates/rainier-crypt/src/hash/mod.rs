//! Password hashing — the [`Hasher`] port, the algorithms, and the
//! [`HashManager`] that selects which one writes.
//!
//! Hashing lives in this crate because it is cryptography; it is **not**
//! encryption. Nothing here can be reversed, which is the point — for data
//! that must be read back, see [`Encryption`](crate::Encryption).
//!
//! | | |
//! |---|---|
//! | [`Hasher`] | the port — hash, verify, `needs_rehash`, the unusable sentinel |
//! | [`Argon2Hasher`] | Argon2id, the default |
//! | [`BcryptHasher`] | bcrypt as a peer driver, behind the `bcrypt` feature |
//! | [`HashManager`] | the algorithms behind one selection — `HASH_DRIVER` |
//! | [`LegacyVerifier`] | a scheme that can be read but never written |
//!
//! ```ignore
//! // In a provider. `HASH_DRIVER=argon2id` (the default) or `bcrypt`.
//! app.instance(HashManager::new(config.setting(keys::HASH_DRIVER)?)?);
//! ```

pub mod hasher;
pub mod legacy;
pub mod manager;

pub use hasher::{Argon2Hasher, Hasher};
#[cfg(feature = "bcrypt")]
pub use legacy::BcryptVerifier;
pub use legacy::LegacyVerifier;
#[cfg(feature = "bcrypt")]
pub use manager::BcryptHasher;
pub use manager::{HashDriver, HashManager};
