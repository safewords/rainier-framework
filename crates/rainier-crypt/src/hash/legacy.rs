//! Verifying hashes this application did not write — [`LegacyVerifier`].
//!
//! ```ignore
//! app.instance(Argon2Hasher::new().with_legacy(BcryptVerifier));
//! ```
//!
//! Every port from PHP, Rails or Django lands on this the first day: the users
//! table already exists, it is full of `$2y$` or `pbkdf2_sha256$` hashes, and
//! nobody knows anybody's password — so the rows cannot be re-hashed until
//! their owners log in.
//!
//! The shape that works is always the same. Verify with whichever scheme wrote
//! the row, decide the row is out of date, and re-hash it with the current
//! scheme while the plaintext is briefly in hand:
//!
//! ```ignore
//! if hasher.verify(&input.password, &user.password) {
//!     if hasher.needs_rehash(&user.password) {
//!         user.password = hasher.hash(&input.password)?;
//!         users.update(&user).await?;
//!     }
//!     // … log them in
//! }
//! ```
//!
//! That block is the whole migration: deploy it, and the population converts
//! itself as people arrive. What is left over after a year is the accounts
//! nobody uses, which is its own useful signal.

use std::sync::Arc;

/// A hash scheme this application can **read** but does not write.
///
/// Deliberately not [`Hasher`](crate::Hasher): a legacy scheme has no `hash`,
/// because writing one is exactly what must not happen. Anything implementing
/// this can only ever confirm a password against a row that already exists.
pub trait LegacyVerifier: Send + Sync + 'static {
    /// A label, for diagnostics and for `needs_rehash` logging.
    fn name(&self) -> &'static str;

    /// Whether this scheme wrote `hashed`.
    ///
    /// Almost always a prefix test — `$2y$`, `pbkdf2_sha256$`, `sha1$`. It
    /// must be **cheap and total**: it runs on every login, against strings
    /// written by schemes it has never heard of.
    fn recognises(&self, hashed: &str) -> bool;

    /// Whether `plain` matches `hashed`.
    ///
    /// Only called when [`recognises`](Self::recognises) said yes. `false` for
    /// a malformed hash, for the same reason
    /// [`Hasher::verify`](crate::Hasher::verify) does it: to a caller deciding
    /// whether to let someone in, "corrupt" and "wrong" must lead to the same
    /// place.
    fn verify(&self, plain: &str, hashed: &str) -> bool;
}

/// The legacy schemes a hasher will read, in the order they are consulted.
#[derive(Clone, Default)]
pub(crate) struct LegacySchemes {
    verifiers: Vec<Arc<dyn LegacyVerifier>>,
}

impl LegacySchemes {
    pub(crate) fn push(&mut self, verifier: Arc<dyn LegacyVerifier>) {
        self.verifiers.push(verifier);
    }

    /// The first scheme that recognises `hashed`, if any.
    pub(crate) fn matching(&self, hashed: &str) -> Option<&Arc<dyn LegacyVerifier>> {
        self.verifiers.iter().find(|verifier| verifier.recognises(hashed))
    }

    pub(crate) fn names(&self) -> Vec<&'static str> {
        self.verifiers.iter().map(|verifier| verifier.name()).collect()
    }
}

impl std::fmt::Debug for LegacySchemes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// bcrypt, which is what an inherited PHP or Rails table is full of.
///
/// Recognises `$2a$`, `$2b$` and `$2y$` — the three prefixes in the wild. They
/// differ over how a pre-2011 implementation handled the null terminator and
/// over 8-bit characters, and the `bcrypt` crate reads all three.
///
/// Note bcrypt silently truncates at **72 bytes**. That is a property of the
/// algorithm, not of this implementation, and it is one of the reasons to
/// migrate off it rather than keep writing it.
#[cfg(feature = "bcrypt")]
#[derive(Debug, Clone, Copy, Default)]
pub struct BcryptVerifier;

#[cfg(feature = "bcrypt")]
impl LegacyVerifier for BcryptVerifier {
    fn name(&self) -> &'static str {
        "bcrypt"
    }

    fn recognises(&self, hashed: &str) -> bool {
        hashed.starts_with("$2a$") || hashed.starts_with("$2b$") || hashed.starts_with("$2y$")
    }

    fn verify(&self, plain: &str, hashed: &str) -> bool {
        bcrypt::verify(plain, hashed).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scheme nobody should ship, standing in for one somebody has.
    struct Reversed;

    impl LegacyVerifier for Reversed {
        fn name(&self) -> &'static str {
            "reversed"
        }

        fn recognises(&self, hashed: &str) -> bool {
            hashed.starts_with("rev$")
        }

        fn verify(&self, plain: &str, hashed: &str) -> bool {
            hashed
                .strip_prefix("rev$")
                .is_some_and(|stored| stored.chars().rev().collect::<String>() == plain)
        }
    }

    #[test]
    fn the_first_scheme_that_recognises_it_wins() {
        let mut schemes = LegacySchemes::default();
        schemes.push(Arc::new(Reversed));

        assert!(schemes.matching("rev$drowssap").is_some());
        assert!(schemes.matching("$argon2id$v=19$...").is_none());
        assert_eq!(schemes.names(), vec!["reversed"]);
    }

    #[test]
    fn an_empty_set_recognises_nothing() {
        let schemes = LegacySchemes::default();

        assert!(schemes.names().is_empty());
        assert!(schemes.matching("rev$drowssap").is_none());
    }

    #[cfg(feature = "bcrypt")]
    #[test]
    fn bcrypt_reads_all_three_prefixes_and_nothing_else() {
        let verifier = BcryptVerifier;

        for prefix in ["$2a$", "$2b$", "$2y$"] {
            assert!(verifier.recognises(&format!("{prefix}10$abcdefg")), "{prefix}");
        }

        assert!(!verifier.recognises("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA"));
        assert!(!verifier.recognises(""));
        assert!(!verifier.recognises("$2$10$old"), "the 1999 prefix is not one of the three");
    }

    #[cfg(feature = "bcrypt")]
    #[test]
    fn bcrypt_verifies_a_hash_php_would_have_written() {
        // `password_hash('correct horse', PASSWORD_BCRYPT)` at cost 4, which
        // is the lowest the algorithm allows and keeps this test fast.
        let hashed = bcrypt::hash("correct horse", 4).unwrap();
        // PHP writes `$2y$`; the crate writes `$2b$`. Same algorithm, and a
        // real table is full of the former.
        let php_style = hashed.replacen("$2b$", "$2y$", 1);

        assert!(BcryptVerifier.verify("correct horse", &php_style));
        assert!(!BcryptVerifier.verify("wrong horse", &php_style));
    }

    #[cfg(feature = "bcrypt")]
    #[test]
    fn a_malformed_bcrypt_hash_is_false_rather_than_a_panic() {
        assert!(!BcryptVerifier.verify("anything", "$2y$not-a-hash"));
        assert!(!BcryptVerifier.verify("anything", "$2y$"));
    }
}
