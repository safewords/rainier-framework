//! Password hashing — the [`Hasher`] port and its Argon2 implementation.

use argon2::{Algorithm, Argon2, Params, Version};
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rainier_support::{Error, Result};

use super::legacy::{LegacySchemes, LegacyVerifier};
use rand::rngs::OsRng;

/// Hashes and verifies passwords.
///
/// A port rather than a concrete choice, because the right algorithm changes
/// over time and an application that has to migrate should be able to run two
/// hashers side by side while it does.
pub trait Hasher: Send + Sync + 'static {
    /// Hash `plain`, returning an encoded hash that carries its own parameters
    /// and salt.
    fn hash(&self, plain: &str) -> Result<String>;

    /// Whether `plain` matches `hashed`.
    ///
    /// Returns `false` rather than an error for a malformed hash: to a caller
    /// deciding whether to let someone in, "this stored hash is corrupt" and
    /// "the password is wrong" must lead to the same outcome.
    fn verify(&self, plain: &str, hashed: &str) -> bool;

    /// Whether `hashed` was produced with weaker parameters than this hasher
    /// uses now, and should be re-hashed on the next successful login.
    fn needs_rehash(&self, hashed: &str) -> bool {
        let _ = hashed;
        false
    }

    /// Spend the work of a verify, and learn nothing.
    ///
    /// For the branch where **there is no user**. A login that looks up an
    /// email, finds nothing and returns immediately answers in a millisecond;
    /// one that finds a user and verifies the password answers in fifty. That
    /// difference is a working account-enumeration oracle — a script can walk
    /// a list of emails and learn which ones are registered, without ever
    /// guessing a password. It reads as "invalid credentials" every time,
    /// which is why it survives review.
    ///
    /// ```ignore
    /// let Some(user) = users.by_email(&email).await? else {
    ///     hasher.dummy_verify(&password);
    ///     return Err(Error::unauthenticated("Invalid credentials."));
    /// };
    /// ```
    ///
    /// The default does one hash at this hasher's own cost, which is the same
    /// KDF work a verify does.
    fn dummy_verify(&self, plain: &str) {
        let _ = self.hash(plain);
    }

    /// A stored value that no password can ever match.
    ///
    /// For an account that authenticates some other way — SSO, a magic link,
    /// an API key — or one that has been suspended. The alternatives are both
    /// worse: an empty string is a hash somebody's empty password might match
    /// depending on the algorithm, and `NULL` makes every read site decide
    /// what a missing hash means.
    ///
    /// [`verify`](Self::verify) always returns `false` for it, and takes the
    /// same time doing so as a real check — the account exists, and how it
    /// authenticates is not something a login form should leak.
    fn unusable(&self) -> String {
        UNUSABLE.to_string()
    }

    /// Whether this stored value is [`unusable`](Self::unusable).
    fn is_unusable(&self, hashed: &str) -> bool {
        hashed == UNUSABLE || hashed.is_empty()
    }

    /// Whether this hasher's own algorithm wrote `hashed`.
    ///
    /// Almost always a prefix test — `$argon2`, `$2y$` — and it must be cheap
    /// and total, because [`HashManager`](crate::HashManager) calls it on
    /// every login to decide which driver a stored hash belongs to. A driver
    /// that leaves the default cannot be dispatched to, so anything meant to
    /// live in the manager implements it.
    fn recognises(&self, hashed: &str) -> bool {
        let _ = hashed;
        false
    }
}

/// The stored value for "this account has no password".
///
/// Not a valid PHC string, so no parser will accept it and no algorithm will
/// produce it. Django's `!` marker, spelled so it is obvious in a database
/// dump.
const UNUSABLE: &str = "*no-password*";

/// Argon2id, the current recommendation for password storage.
///
/// It can also **read** schemes it will never write — see
/// [`with_legacy`](Self::with_legacy).
#[derive(Debug, Clone)]
pub struct Argon2Hasher {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    /// Schemes this hasher can verify but not produce. Empty by default.
    legacy: LegacySchemes,
}

impl Default for Argon2Hasher {
    fn default() -> Self {
        // OWASP's baseline: 19 MiB, 2 iterations, 1 lane. Comfortably above
        // the point where GPU cracking stops being cheap, and still fast
        // enough to run on every login.
        Self {
            memory_kib: 19 * 1024,
            iterations: 2,
            parallelism: 1,
            legacy: LegacySchemes::default(),
        }
    }
}

impl Argon2Hasher {
    /// Argon2id with the default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Argon2id with explicit cost parameters.
    pub fn with_params(memory_kib: u32, iterations: u32, parallelism: u32) -> Self {
        Self { memory_kib, iterations, parallelism, legacy: LegacySchemes::default() }
    }

    /// Also read hashes written by another scheme.
    ///
    /// ```ignore
    /// Argon2Hasher::new().with_legacy(BcryptVerifier)
    /// ```
    ///
    /// [`verify`](Hasher::verify) dispatches on the stored hash's own prefix,
    /// so a table half-converted from bcrypt logs everybody in.
    /// [`needs_rehash`](Hasher::needs_rehash) answers `true` for anything a
    /// legacy scheme recognises, which is what converts the row on the way
    /// past.
    ///
    /// Nothing here ever *writes* a legacy hash: [`LegacyVerifier`] has no
    /// `hash` method, so "support bcrypt" cannot quietly become "keep
    /// producing bcrypt".
    ///
    /// Call it more than once for more than one scheme; they are consulted in
    /// the order added.
    #[must_use = "this returns a configured hasher rather than configuring in place"]
    pub fn with_legacy(mut self, verifier: impl LegacyVerifier) -> Self {
        self.legacy.push(std::sync::Arc::new(verifier));
        self
    }

    /// The legacy schemes this hasher will read, in order.
    pub fn legacy_schemes(&self) -> Vec<&'static str> {
        self.legacy.names()
    }

    /// Deliberately weak parameters, for tests.
    ///
    /// Hashing at production cost turns a test suite that logs in a few dozen
    /// times into a slow one. Never use this outside tests — the whole point
    /// of the real parameters is that they are expensive.
    pub fn insecure_for_tests() -> Self {
        Self { memory_kib: 8, iterations: 1, parallelism: 1, legacy: LegacySchemes::default() }
    }

    fn argon2(&self) -> Result<Argon2<'static>> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, None)
            .map_err(|e| Error::internal(format!("invalid Argon2 parameters: {e}")))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

impl Hasher for Argon2Hasher {
    fn hash(&self, plain: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = self
            .argon2()?
            .hash_password(plain.as_bytes(), &salt)
            .map_err(|e| Error::internal(format!("could not hash the password: {e}")))?;
        Ok(hash.to_string())
    }

    fn verify(&self, plain: &str, hashed: &str) -> bool {
        // An account with no password still exists, and a login form must not
        // be able to tell that apart from a wrong password by how fast the
        // answer comes back.
        if self.is_unusable(hashed) {
            self.dummy_verify(plain);
            return false;
        }

        // A scheme this application inherited rather than wrote. Checked
        // before parsing, because a bcrypt string is not a PHC string and
        // `PasswordHash::new` would simply refuse it.
        if let Some(legacy) = self.legacy.matching(hashed) {
            return legacy.verify(plain, hashed);
        }

        // The parsed hash carries its own parameters, so a password hashed
        // with older settings still verifies — see `needs_rehash`.
        let Ok(parsed) = PasswordHash::new(hashed) else {
            return false;
        };
        Argon2::default().verify_password(plain.as_bytes(), &parsed).is_ok()
    }

    fn needs_rehash(&self, hashed: &str) -> bool {
        // A legacy scheme is by definition not the current one, however
        // strong its own parameters are.
        if self.legacy.matching(hashed).is_some() {
            return true;
        }

        let Ok(parsed) = PasswordHash::new(hashed) else {
            // Unparseable: it certainly is not in the current format.
            return true;
        };
        let Ok(params) = Params::try_from(&parsed) else {
            return true;
        };
        params.m_cost() < self.memory_kib
            || params.t_cost() < self.iterations
            || params.p_cost() < self.parallelism
    }

    fn recognises(&self, hashed: &str) -> bool {
        // `$argon2id$`, `$argon2i$`, `$argon2d$` — this driver reads all
        // three, whatever variant wrote the row.
        hashed.starts_with("$argon2")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hasher() -> Argon2Hasher {
        Argon2Hasher::insecure_for_tests()
    }

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let hasher = hasher();
        let hashed = hasher.hash("correct horse").unwrap();
        assert!(hasher.verify("correct horse", &hashed));
    }

    #[test]
    fn a_wrong_password_does_not_verify() {
        let hasher = hasher();
        let hashed = hasher.hash("correct horse").unwrap();
        assert!(!hasher.verify("wrong horse", &hashed));
        assert!(!hasher.verify("", &hashed));
        assert!(!hasher.verify("Correct Horse", &hashed), "hashing is case-sensitive");
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        // A per-hash salt is what stops identical passwords being visibly
        // identical in the database.
        let hasher = hasher();
        assert_ne!(hasher.hash("same").unwrap(), hasher.hash("same").unwrap());
    }

    #[test]
    fn a_hash_carries_its_algorithm_and_parameters() {
        let hashed = hasher().hash("x").unwrap();
        assert!(hashed.starts_with("$argon2id$"), "{hashed}");
    }

    #[test]
    fn a_corrupt_hash_fails_closed_rather_than_erroring() {
        // A caller deciding whether to let someone in must not be able to
        // confuse "corrupt hash" with "correct password".
        let hasher = hasher();
        assert!(!hasher.verify("anything", "not-a-hash"));
        assert!(!hasher.verify("anything", ""));
        assert!(!hasher.verify("anything", "$argon2id$broken"));
    }

    #[test]
    fn a_hash_from_weaker_parameters_is_flagged_for_rehashing() {
        let weak = Argon2Hasher::with_params(8, 1, 1);
        let strong = Argon2Hasher::with_params(64, 3, 1);

        let old = weak.hash("password").unwrap();
        assert!(strong.needs_rehash(&old), "stronger settings should ask for a rehash");
        assert!(!weak.needs_rehash(&old), "the same settings should not");

        // And it must still verify in the meantime, or every user would be
        // locked out the moment the parameters changed.
        assert!(strong.verify("password", &old));
    }

    #[test]
    fn an_unparseable_hash_needs_rehashing() {
        assert!(hasher().needs_rehash("plaintext-from-some-legacy-system"));
    }

    #[test]
    fn unicode_passwords_round_trip() {
        let hasher = hasher();
        let hashed = hasher.hash("pässwörd-🔒").unwrap();
        assert!(hasher.verify("pässwörd-🔒", &hashed));
        assert!(!hasher.verify("passwörd-🔒", &hashed));
    }

    #[test]
    fn an_unusable_hash_matches_nothing() {
        let hasher = hasher();
        let unusable = hasher.unusable();

        assert!(!hasher.verify("", &unusable));
        assert!(!hasher.verify("password", &unusable));
        assert!(!hasher.verify(&unusable, &unusable), "not even itself");
        assert!(hasher.is_unusable(&unusable));
    }

    #[test]
    fn an_empty_stored_hash_is_unusable_rather_than_a_match() {
        // A column defaulted to '' is the shape this arrives in, and treating
        // it as anything but unusable is a way in.
        let hasher = hasher();

        assert!(hasher.is_unusable(""));
        assert!(!hasher.verify("", ""));
    }

    #[test]
    fn a_real_hash_is_not_unusable() {
        let hasher = hasher();
        let hashed = hasher.hash("correct horse").unwrap();

        assert!(!hasher.is_unusable(&hashed));
        assert!(hasher.verify("correct horse", &hashed));
    }

    #[test]
    fn dummy_verify_costs_about_what_a_real_verify_costs() {
        // The point is the *timing*, so this measures it. Deliberately loose:
        // CI machines are noisy and a factor of five either way still closes
        // the enumeration oracle, which is the thing that matters.
        let hasher = Argon2Hasher::with_params(1024, 3, 1);
        let hashed = hasher.hash("correct horse").unwrap();

        let started = std::time::Instant::now();
        assert!(!hasher.verify("wrong horse", &hashed));
        let real = started.elapsed();

        let started = std::time::Instant::now();
        hasher.dummy_verify("wrong horse");
        let dummy = started.elapsed();

        assert!(
            dummy.as_secs_f64() > real.as_secs_f64() / 5.0,
            "a dummy verify at {dummy:?} against a real one at {real:?} still leaks"
        );
    }

    /// A scheme standing in for one an application inherited.
    struct Rot13;

    impl super::super::legacy::LegacyVerifier for Rot13 {
        fn name(&self) -> &'static str {
            "rot13"
        }

        fn recognises(&self, hashed: &str) -> bool {
            hashed.starts_with("rot13$")
        }

        fn verify(&self, plain: &str, hashed: &str) -> bool {
            let rotated: String = plain
                .chars()
                .map(|c| match c {
                    'a'..='z' => (((c as u8 - b'a' + 13) % 26) + b'a') as char,
                    'A'..='Z' => (((c as u8 - b'A' + 13) % 26) + b'A') as char,
                    other => other,
                })
                .collect();

            hashed.strip_prefix("rot13$") == Some(rotated.as_str())
        }
    }

    #[test]
    fn a_legacy_hash_verifies_and_the_current_scheme_still_does() {
        let hasher = hasher().with_legacy(Rot13);

        // The inherited row.
        assert!(hasher.verify("secret", "rot13$frperg"));
        assert!(!hasher.verify("wrong", "rot13$frperg"));

        // And one this hasher wrote itself.
        let current = hasher.hash("secret").unwrap();
        assert!(hasher.verify("secret", &current));
    }

    #[test]
    fn a_legacy_hash_always_needs_rehashing() {
        // However strong the old scheme was, it is not the current one — and
        // this is what converts the row on the next successful login.
        let hasher = hasher().with_legacy(Rot13);

        assert!(hasher.needs_rehash("rot13$frperg"));
        assert!(!hasher.needs_rehash(&hasher.hash("secret").unwrap()));
    }

    #[test]
    fn the_migration_loop_converts_a_row_once() {
        let hasher = hasher().with_legacy(Rot13);
        let mut stored = "rot13$frperg".to_string();

        // The block every port writes.
        for _ in 0..3 {
            assert!(hasher.verify("secret", &stored));
            if hasher.needs_rehash(&stored) {
                stored = hasher.hash("secret").unwrap();
            }
        }

        assert!(stored.starts_with("$argon2id$"), "{stored}");
        assert!(!hasher.needs_rehash(&stored), "it converted twice");
    }

    #[test]
    fn schemes_are_consulted_in_the_order_they_were_added() {
        let hasher = hasher().with_legacy(Rot13);
        assert_eq!(hasher.legacy_schemes(), vec!["rot13"]);

        let hasher = hasher.with_legacy(Rot13);
        assert_eq!(hasher.legacy_schemes(), vec!["rot13", "rot13"]);
    }

    #[test]
    fn a_legacy_scheme_does_not_make_an_unusable_hash_usable() {
        // `unusable()` is checked first, and no legacy scheme is asked about
        // it — otherwise a permissive `recognises` could reopen an account
        // that was deliberately closed.
        let hasher = hasher().with_legacy(Rot13);
        let unusable = hasher.unusable();

        assert!(!hasher.verify("", &unusable));
        assert!(!hasher.verify("rot13$", &unusable));
    }

    #[test]
    fn an_unrecognised_hash_is_still_refused() {
        let hasher = hasher().with_legacy(Rot13);

        assert!(!hasher.verify("secret", "$2y$10$something-bcrypt-shaped"));
        assert!(!hasher.verify("secret", "nonsense"));
    }
}
