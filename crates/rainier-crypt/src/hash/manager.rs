//! Choosing the algorithm — [`HashManager`], [`HashDriver`] and the drivers.
//!
//! ```ignore
//! // In a provider. `HASH_DRIVER=argon2id` (the default) or `bcrypt`.
//! app.instance(HashManager::new(config.setting(keys::HASH_DRIVER)?)?);
//!
//! // Anywhere after that — or through the `Hash` facade.
//! let stored = manager.hash(&input.password)?;
//! let ok = manager.verify(&input.password, &user.password);
//! ```
//!
//! Argon2 and bcrypt are different **algorithms**, not an algorithm and its
//! poor relation. The manager treats them as peers: each is a full
//! [`Hasher`], one of them — named by configuration or by explicit
//! declaration — is what [`hash`](Hasher::hash) writes, and **verification
//! never consults the selection at all**. A stored hash names its own
//! algorithm in its own prefix, so `verify` dispatches to whichever driver
//! recognises it — exactly the contract PHP's `password_verify` and
//! Laravel's `Hash::check` honour, and the property that makes changing the
//! driver a deploy rather than a migration.
//!
//! # Changing algorithm is the same block it always was
//!
//! ```ignore
//! if manager.verify(&input.password, &user.password) {
//!     if manager.needs_rehash(&user.password) {
//!         user.password = manager.hash(&input.password)?;
//!         users.update(&user).await?;
//!     }
//!     // … log them in
//! }
//! ```
//!
//! [`needs_rehash`](Hasher::needs_rehash) answers `true` for any row the
//! *selected* driver did not write — another driver's format, a
//! [legacy scheme's](crate::LegacyVerifier), or the selected driver's own
//! format at weaker parameters. Deploy the new selection and the population
//! converts itself as people log in.
//!
//! # Every failure costs the same
//!
//! The manager pads the branches a bare driver answers quickly: the
//! [unusable sentinel](Hasher::unusable), and a stored value in a format
//! *nothing* recognises — a corrupt row, a column filled in by hand. Both
//! spend a [`dummy_verify`](Hasher::dummy_verify) at the selected driver's
//! cost before failing, so neither an SSO-only account nor a damaged row can
//! be singled out of a timing profile.

use std::sync::Arc;

use rainier_support::{setting_enum, Error, Result};

use super::hasher::{Argon2Hasher, Hasher};
use super::legacy::{LegacySchemes, LegacyVerifier};

setting_enum! {
    /// Which algorithm [`Hasher::hash`] writes.
    ///
    /// A closed set, like every driver selection: `HASH_DRIVER=argon2` fails
    /// at boot naming the variable and the valid values, rather than silently
    /// hashing with something else. Verification is deliberately **not**
    /// governed by this — a stored hash is verified by whichever driver
    /// recognises its prefix, whatever is selected here.
    pub enum HashDriver: "hash driver" {
        /// Argon2id — the current recommendation, and the default.
        #[default]
        Argon2id = "argon2id",

        /// bcrypt — what PHP's `password_hash` writes by default.
        ///
        /// A real choice for an application that shares its users table with
        /// a PHP application still writing rows. Needs the `bcrypt` cargo
        /// feature; note the algorithm silently truncates at 72 bytes.
        Bcrypt = "bcrypt",
    }
}

/// bcrypt as a first-class driver.
///
/// Reads `$2a$`, `$2b$` and `$2y$` — the three prefixes in the wild — and
/// writes `$2b$`, which PHP's `password_verify` reads happily. Prefer
/// [`HashDriver::Argon2id`] unless a PHP application you share rows with
/// forces the issue: bcrypt truncates at 72 bytes and its cost scales worse
/// against modern hardware.
#[cfg(feature = "bcrypt")]
#[derive(Debug, Clone, Copy)]
pub struct BcryptHasher {
    cost: u32,
}

#[cfg(feature = "bcrypt")]
impl Default for BcryptHasher {
    fn default() -> Self {
        // What Laravel ships as `bcrypt.rounds` today. PHP's own default is
        // still 10; 12 is where OWASP puts the floor.
        Self { cost: 12 }
    }
}

#[cfg(feature = "bcrypt")]
impl BcryptHasher {
    /// bcrypt at cost 12.
    pub fn new() -> Self {
        Self::default()
    }

    /// bcrypt at an explicit cost.
    pub fn with_cost(cost: u32) -> Self {
        Self { cost }
    }

    /// Cost 4 — the algorithm's minimum. Never use this outside tests.
    pub fn insecure_for_tests() -> Self {
        Self { cost: 4 }
    }
}

#[cfg(feature = "bcrypt")]
impl Hasher for BcryptHasher {
    fn hash(&self, plain: &str) -> Result<String> {
        bcrypt::hash(plain, self.cost)
            .map_err(|e| Error::internal(format!("could not hash the password: {e}")))
    }

    fn verify(&self, plain: &str, hashed: &str) -> bool {
        if self.is_unusable(hashed) {
            self.dummy_verify(plain);
            return false;
        }

        bcrypt::verify(plain, hashed).unwrap_or(false)
    }

    fn needs_rehash(&self, hashed: &str) -> bool {
        // `$2y$12$…` — the cost is the second dollar-field. A row this driver
        // cannot read the cost out of is certainly not current.
        let cost = hashed
            .get(4..6)
            .and_then(|digits| digits.parse::<u32>().ok());

        cost.is_none_or(|cost| cost < self.cost)
    }

    fn recognises(&self, hashed: &str) -> bool {
        hashed.starts_with("$2a$") || hashed.starts_with("$2b$") || hashed.starts_with("$2y$")
    }
}

/// The algorithms, behind one selection.
///
/// Registers every driver this build carries — Argon2id always, bcrypt with
/// the `bcrypt` feature — and writes with the one named at construction. See
/// the [module docs](self) for the dispatch rules; the type itself is a
/// [`Hasher`], so anything that takes the port takes the manager.
#[derive(Clone)]
pub struct HashManager {
    /// Every driver, in dispatch order. The selected one also pays for the
    /// padded branches.
    drivers: Vec<(HashDriver, Arc<dyn Hasher>)>,
    selected: HashDriver,
    /// Read-only schemes with no driver — an inherited Django or Rails
    /// table. Consulted after the drivers.
    legacy: LegacySchemes,
}

impl HashManager {
    /// Every compiled driver at production parameters, writing with `selected`.
    ///
    /// Fails — at boot, naming the feature — when the selection names a
    /// driver this build does not carry, rather than hashing with something
    /// the configuration did not say.
    pub fn new(selected: HashDriver) -> Result<Self> {
        let mut drivers: Vec<(HashDriver, Arc<dyn Hasher>)> =
            vec![(HashDriver::Argon2id, Arc::new(Argon2Hasher::new()))];

        #[cfg(feature = "bcrypt")]
        drivers.push((HashDriver::Bcrypt, Arc::new(BcryptHasher::new())));

        Self::from_parts(drivers, selected)
    }

    /// Every compiled driver at its weakest parameters, for tests.
    ///
    /// Hashing at production cost is the point of a KDF, and it is also what
    /// turns a suite that creates fifty users into a slow one.
    pub fn insecure_for_tests(selected: HashDriver) -> Result<Self> {
        let mut drivers: Vec<(HashDriver, Arc<dyn Hasher>)> =
            vec![(HashDriver::Argon2id, Arc::new(Argon2Hasher::insecure_for_tests()))];

        #[cfg(feature = "bcrypt")]
        drivers.push((HashDriver::Bcrypt, Arc::new(BcryptHasher::insecure_for_tests())));

        Self::from_parts(drivers, selected)
    }

    fn from_parts(drivers: Vec<(HashDriver, Arc<dyn Hasher>)>, selected: HashDriver) -> Result<Self> {
        if !drivers.iter().any(|(name, _)| *name == selected) {
            return Err(Error::internal(format!(
                "HASH_DRIVER={selected} names a driver this build does not carry — it needs \
                 the `{selected}` cargo feature"
            )));
        }

        Ok(Self { drivers, selected, legacy: LegacySchemes::default() })
    }

    /// Replace a driver's parameters, or add a driver of your own.
    ///
    /// ```ignore
    /// HashManager::new(HashDriver::Argon2id)?
    ///     .with_driver(HashDriver::Argon2id, Arc::new(Argon2Hasher::with_params(64 * 1024, 3, 2)))
    /// ```
    #[must_use = "this returns a configured manager rather than configuring in place"]
    pub fn with_driver(mut self, name: HashDriver, hasher: Arc<dyn Hasher>) -> Self {
        self.drivers.retain(|(existing, _)| *existing != name);
        self.drivers.push((name, hasher));
        self
    }

    /// Also read a scheme that has no driver — an inherited `pbkdf2_sha256$`
    /// or `sha1$` table. Consulted after the drivers, in the order added, and
    /// never written: [`LegacyVerifier`] has no `hash`, which is the point.
    #[must_use = "this returns a configured manager rather than configuring in place"]
    pub fn with_legacy(mut self, verifier: impl LegacyVerifier) -> Self {
        self.legacy.push(Arc::new(verifier));
        self
    }

    /// Which driver [`hash`](Hasher::hash) writes with.
    pub fn selected(&self) -> HashDriver {
        self.selected
    }

    /// A specific driver, whatever is selected — `Hash::driver("bcrypt")`.
    ///
    /// For the rare call site that must write a named algorithm regardless of
    /// configuration. If that is every call site, change the selection
    /// instead.
    pub fn driver(&self, name: HashDriver) -> Option<Arc<dyn Hasher>> {
        self.drivers
            .iter()
            .find(|(existing, _)| *existing == name)
            .map(|(_, hasher)| Arc::clone(hasher))
    }

    /// Whether some driver or legacy scheme recognises `value` as a hash.
    ///
    /// `Hash::isHashed`, for the guard that stops a plaintext column being
    /// double-hashed — or a hashed one being hashed again.
    pub fn is_hashed(&self, value: &str) -> bool {
        self.drivers.iter().any(|(_, hasher)| hasher.recognises(value))
            || self.legacy.matching(value).is_some()
    }

    /// The selected driver.
    fn writer(&self) -> &Arc<dyn Hasher> {
        self.drivers
            .iter()
            .find(|(name, _)| *name == self.selected)
            .map(|(_, hasher)| hasher)
            .expect("construction verified the selected driver exists")
    }
}

impl Hasher for HashManager {
    fn hash(&self, plain: &str) -> Result<String> {
        self.writer().hash(plain)
    }

    fn verify(&self, plain: &str, hashed: &str) -> bool {
        // The sentinel first and by name: an account that authenticates some
        // other way must cost what a password account costs, or the timing
        // says which is which.
        if self.is_unusable(hashed) {
            self.writer().dummy_verify(plain);
            return false;
        }

        // The stored hash names its own algorithm; the selection is not
        // consulted. This is what lets `HASH_DRIVER` change while every
        // existing row keeps verifying.
        if let Some((_, driver)) = self.drivers.iter().find(|(_, d)| d.recognises(hashed)) {
            return driver.verify(plain, hashed);
        }

        if let Some(scheme) = self.legacy.matching(hashed) {
            return scheme.verify(plain, hashed);
        }

        // A format nothing recognises — a corrupt row, a column somebody
        // filled in by hand. Padded, because it is rare enough that answering
        // quickly would single those accounts out of a timing profile.
        self.writer().dummy_verify(plain);
        false
    }

    fn needs_rehash(&self, hashed: &str) -> bool {
        match self.drivers.iter().find(|(_, d)| d.recognises(hashed)) {
            // The selected driver wrote it; it answers for its own parameters.
            Some((name, driver)) if *name == self.selected => driver.needs_rehash(hashed),
            // Another algorithm wrote it — readable, and out of date by
            // definition, however strong its own parameters are.
            Some(_) => true,
            // A legacy scheme or nothing at all. Either way, the next
            // successful login should replace it. (An unrecognised row can
            // never *pass* `verify`, so this arm never fires for one — it is
            // here so the answer stays right if a scheme is removed later.)
            None => true,
        }
    }

    fn dummy_verify(&self, plain: &str) {
        self.writer().dummy_verify(plain);
    }
}

impl std::fmt::Debug for HashManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashManager")
            .field("selected", &self.selected)
            .field("drivers", &self.drivers.iter().map(|(name, _)| *name).collect::<Vec<_>>())
            .field("legacy", &self.legacy)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    fn manager() -> HashManager {
        HashManager::insecure_for_tests(HashDriver::Argon2id).unwrap()
    }

    #[test]
    fn the_selection_parses_the_way_a_dotenv_spells_it() {
        assert_eq!(HashDriver::parse("argon2id").unwrap(), HashDriver::Argon2id);
        assert_eq!(HashDriver::parse("bcrypt").unwrap(), HashDriver::Bcrypt);
        // A typo names the valid values rather than defaulting to something.
        assert!(HashDriver::parse("argon2").is_err());
    }

    #[test]
    fn the_selected_driver_writes() {
        let hashed = manager().hash("correct horse").unwrap();
        assert!(hashed.starts_with("$argon2id$"), "{hashed}");
    }

    #[cfg(feature = "bcrypt")]
    #[test]
    fn selecting_bcrypt_writes_bcrypt() {
        let manager = HashManager::insecure_for_tests(HashDriver::Bcrypt).unwrap();
        let hashed = manager.hash("correct horse").unwrap();

        assert!(hashed.starts_with("$2b$"), "{hashed}");
        assert!(manager.verify("correct horse", &hashed));
    }

    #[cfg(feature = "bcrypt")]
    #[test]
    fn verification_never_consults_the_selection() {
        // The `password_verify` contract: the stored hash names its own
        // algorithm, so either manager reads either row.
        let argon2 = HashManager::insecure_for_tests(HashDriver::Argon2id).unwrap();
        let bcrypt_manager = HashManager::insecure_for_tests(HashDriver::Bcrypt).unwrap();

        let argon2_row = argon2.hash("one").unwrap();
        let bcrypt_row = bcrypt_manager.hash("two").unwrap();

        assert!(argon2.verify("two", &bcrypt_row));
        assert!(bcrypt_manager.verify("one", &argon2_row));
        assert!(!argon2.verify("wrong", &bcrypt_row));
    }

    #[cfg(feature = "bcrypt")]
    #[test]
    fn a_row_another_algorithm_wrote_asks_to_be_rehashed() {
        // The migration story: change `HASH_DRIVER`, deploy, and rows convert
        // on the next successful login — in either direction.
        let argon2 = HashManager::insecure_for_tests(HashDriver::Argon2id).unwrap();
        let bcrypt_row = bcrypt::hash("x", 4).unwrap();

        assert!(argon2.needs_rehash(&bcrypt_row));
        assert!(!argon2.needs_rehash(&argon2.hash("x").unwrap()));

        let bcrypt_manager = HashManager::insecure_for_tests(HashDriver::Bcrypt).unwrap();
        assert!(bcrypt_manager.needs_rehash(&argon2.hash("x").unwrap()));
        assert!(!bcrypt_manager.needs_rehash(&bcrypt_manager.hash("x").unwrap()));
    }

    #[cfg(feature = "bcrypt")]
    #[test]
    fn a_php_spelled_bcrypt_row_verifies() {
        // PHP writes `$2y$`; the crate writes `$2b$`. A ported table is full
        // of the former.
        let manager = manager();
        let php_style = bcrypt::hash("laravel-legacy", 4).unwrap().replacen("$2b$", "$2y$", 1);

        assert!(manager.verify("laravel-legacy", &php_style));
        assert!(manager.needs_rehash(&php_style));
    }

    #[test]
    fn an_unrecognised_format_fails_closed_at_full_cost() {
        // A corrupt row is an account that cannot be logged into, and it must
        // be indistinguishable from a wrong password in time as well as in
        // words. Timed loosely on purpose — the assertion is "the same order
        // of magnitude", not a number.
        use std::time::Instant;

        let manager = HashManager::new(HashDriver::Argon2id).unwrap();
        let real = manager.hash("a-real-password").unwrap();

        let started = Instant::now();
        assert!(!manager.verify("guess", &real));
        let with_a_password = started.elapsed();

        let started = Instant::now();
        assert!(!manager.verify("guess", "plaintext-not-a-hash"));
        let unrecognised = started.elapsed();

        assert!(
            unrecognised * 10 > with_a_password,
            "unrecognised-format verify took {unrecognised:?} against {with_a_password:?} — \
             fast enough to be an oracle"
        );
    }

    #[test]
    fn the_sentinel_is_padded_and_cannot_be_guessed_into() {
        let manager = manager();
        let stored = manager.unusable();

        assert!(manager.is_unusable(&stored));
        for attempt in ["", "password", "*no-password*"] {
            assert!(!manager.verify(attempt, &stored));
        }
    }

    #[test]
    fn a_missing_driver_is_a_boot_error_naming_the_feature() {
        // Constructed by hand so the test does not depend on which features
        // this run carries.
        let err = HashManager::from_parts(
            vec![(HashDriver::Argon2id, Arc::new(Argon2Hasher::insecure_for_tests()))],
            HashDriver::Bcrypt,
        )
        .unwrap_err();

        assert!(err.message().contains("bcrypt"), "{}", err.message());
        assert!(err.message().contains("feature"), "{}", err.message());
    }

    #[test]
    fn a_named_driver_is_reachable_regardless_of_the_selection() {
        // `Hash::driver("…")` — the escape hatch for one call site that must
        // write a specific algorithm.
        let manager = manager();

        assert!(manager.driver(HashDriver::Argon2id).is_some());
        assert_eq!(manager.selected(), HashDriver::Argon2id);
    }

    #[test]
    fn is_hashed_recognises_what_the_drivers_do() {
        let manager = manager();

        assert!(manager.is_hashed(&manager.hash("x").unwrap()));
        assert!(!manager.is_hashed("hunter2"));
        assert!(!manager.is_hashed(""));
    }

    #[test]
    fn a_legacy_scheme_still_reads_through_the_manager() {
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

        let manager = manager().with_legacy(Reversed);

        assert!(manager.verify("password", "rev$drowssap"));
        assert!(manager.needs_rehash("rev$drowssap"));
        assert!(manager.is_hashed("rev$drowssap"));
    }
}
