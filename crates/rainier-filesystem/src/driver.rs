//! Which disk to build — [`FilesystemDriver`], [`DiskDriver`], and the registry
//! an application adds its own driver to.
//!
//! Two types, because there are two questions. [`FilesystemDriver`] is the
//! closed set the *framework* ships and knows how to build; [`DiskDriver`] is
//! what a *declaration* names, which is one of those or a driver the
//! application registered. Keeping them apart is what lets a caller match on
//! [`FilesystemDriver::S3`] without having to handle an open-ended string to do
//! it.
//!
//! ## Extending the set
//!
//! An application registers a name and a factory, and a declaration naming that
//! driver resolves through it:
//!
//! ```
//! use std::sync::Arc;
//! use rainier_filesystem::{CustomDisk, Disks, Filesystem, FilesystemDriver, MemoryFilesystem};
//!
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! FilesystemDriver::extend("my-store", |disk: CustomDisk| async move {
//!     // Whatever the declaration wrote down, this driver's own settings.
//!     let _endpoint = disk.string("endpoint").unwrap_or_default();
//!     Ok(Arc::new(MemoryFilesystem::new()) as Arc<dyn Filesystem>)
//! })?;
//!
//! let disks: Disks = serde_json::from_value(serde_json::json!({
//!     "default": "bespoke",
//!     "disks": {
//!         "bespoke": { "driver": "my-store", "endpoint": "https://example.invalid" },
//!     },
//! }))
//! .unwrap();
//!
//! let storage = disks.build().await?;
//! assert!(storage.has_disk("bespoke"));
//! # Ok(()) }
//! ```
//!
//! ## Registration happens before the declaration is built
//!
//! Both halves of that are enforced, and they fail differently on purpose:
//!
//! | When | What happened | What the error says |
//! |---|---|---|
//! | a declaration is read | the name is neither built in nor registered | `` `x` is not a valid filesystem driver ``, listing the built-ins *and* everything registered |
//! | a declaration is built | it names a driver nobody registered | `` no filesystem driver is registered under `x` ``, and to register it first |
//!
//! The second is the one somebody will hit: the declaration was assembled in
//! code rather than read from configuration, so nothing checked the name until
//! the disk was built. "Register it before the disk that names it is built" is a
//! different fix from "you spelled it wrong", so it is a different message.
//!
//! What never happens is either of them resolving to something that works. An
//! unrecognised driver is a boot failure, not a disk that quietly becomes
//! `local` — see [`disks`](crate::disks) for why a disk pointed somewhere other
//! than where it was told is the failure this crate is shaped around.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, RwLock};

use rainier_support::{boxed, setting_enum, BoxedFuture, Error, Result, Setting};

use crate::disks::CustomDisk;
use crate::filesystem::Filesystem;

setting_enum! {
    /// Which [`Filesystem`](crate::Filesystem) a named disk uses.
    ///
    /// The drivers the framework ships, and only those — an application's own
    /// driver is a [`DiskDriver::Custom`], reached through
    /// [`extend`](FilesystemDriver::extend). This stays closed so that matching
    /// on a variant stays exhaustive.
    ///
    /// ```
    /// use rainier_filesystem::FilesystemDriver;
    /// use rainier_support::Setting;
    ///
    /// // R2, MinIO, B2 and Wasabi are all `s3` pointed at a different endpoint.
    /// assert_eq!(FilesystemDriver::parse("s3").unwrap(), FilesystemDriver::S3);
    /// ```
    pub enum FilesystemDriver: "filesystem driver" {
        /// One directory on this machine.
        ///
        /// The default. Survives a restart and not a redeploy, which is the
        /// distinction that catches people out on ephemeral hosting.
        #[default]
        Local = "local",

        /// In memory, for tests.
        Memory = "memory",

        /// S3, and everything that speaks its API: **Cloudflare R2**, MinIO,
        /// Backblaze B2, Wasabi. The difference is the endpoint, not the
        /// driver.
        S3 = "s3",
    }
}

impl FilesystemDriver {
    /// Whether files written by one instance are visible to the others.
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::S3)
    }

    /// Whether files survive the machine going away.
    ///
    /// `false` for [`Local`](Self::Local), which is the answer that surprises
    /// people: a container's disk is not storage.
    pub fn is_durable(&self) -> bool {
        matches!(self, Self::S3)
    }
}

/// Builds one disk from its declaration, boxed so the registry can hold any.
type BoxedFactory =
    Arc<dyn Fn(CustomDisk) -> BoxedFuture<Result<Arc<dyn Filesystem>>> + Send + Sync>;

/// Every driver an application has registered, by the name a declaration writes.
///
/// Process-wide, because the thing that has to see it is
/// [`Deserialize`](serde::Deserialize) — a `filesystems` section is read through
/// serde, and there is nowhere in that call to thread a registry through. That
/// is the same trade the framework this borrows from makes, where `extend` also
/// registers against the one application.
///
/// A `BTreeMap` so anything that lists the registered names lists them in a
/// stable order: an error message that reads differently each run is one nobody
/// can grep for.
static REGISTERED: RwLock<BTreeMap<String, BoxedFactory>> = RwLock::new(BTreeMap::new());

impl FilesystemDriver {
    /// Register a driver the framework does not ship, under the name a
    /// declaration will write in its `driver` field.
    ///
    /// The extension point, in the spirit of `Storage::extend` in the framework
    /// this borrows from. The factory is handed the [`CustomDisk`] — the
    /// declaration's own settings, minus the `driver` key that selected it — and
    /// answers with a built disk.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use rainier_filesystem::{CustomDisk, Filesystem, FilesystemDriver, MemoryFilesystem};
    ///
    /// # fn main() -> rainier_support::Result<()> {
    /// FilesystemDriver::extend("custom", |_disk: CustomDisk| async move {
    ///     Ok(Arc::new(MemoryFilesystem::new()) as Arc<dyn Filesystem>)
    /// })?;
    ///
    /// assert!(FilesystemDriver::is_registered("custom"));
    /// # Ok(()) }
    /// ```
    ///
    /// # It has to run before the declaration is built
    ///
    /// A declaration read from configuration checks its driver name as it is
    /// read, so registering after the `filesystems` section is deserialised is
    /// already too late. Register in `main`, before anything resolves storage.
    /// A declaration assembled in code is not checked until
    /// [`build`](crate::Disks::build), and fails there naming the ordering
    /// rather than the spelling.
    ///
    /// # What it refuses
    ///
    /// Both refusals are the same shape as everything else here — a silent
    /// substitution that produces a *working* disk pointed somewhere other than
    /// where it was declared:
    ///
    /// - **A name the framework already ships.** Registering over `s3` would
    ///   move every disk declared with `s3` onto the replacement, and nothing
    ///   about those declarations would change to say so.
    /// - **A name that is already registered**, including one that differs only
    ///   in case or in `_` versus `-`. Two registrations under one name means
    ///   which disk you get depends on which registration ran last, which is an
    ///   ordering nobody wrote down.
    pub fn extend<F, Fut>(name: impl Into<String>, factory: F) -> Result<()>
    where
        F: Fn(CustomDisk) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Arc<dyn Filesystem>>> + Send + 'static,
    {
        let name = name.into();
        let name = name.trim();

        if name.is_empty() {
            return Err(Error::internal(
                "a filesystem driver has to be registered under a name; an empty one cannot be \
                 written in a declaration, so nothing could ever select it",
            ));
        }

        if let Some(built_in) = built_in_matching(name) {
            return Err(Error::internal(format!(
                "`{name}` is the built-in `{built_in}` driver; registering over it would move \
                 every disk declared with `{built_in}` onto the replacement without a single \
                 declaration changing to say so"
            )));
        }

        let mut registered =
            REGISTERED.write().expect("the filesystem driver registry lock is poisoned");

        if let Some(taken) = registered.keys().find(|key| collides(key, name)) {
            return Err(Error::internal(format!(
                "a filesystem driver is already registered under `{taken}`, which `{name}` \
                 selects; replacing it would move every disk declared with it onto the second \
                 registration, and which one won would depend on the order the two happened to \
                 run in"
            )));
        }

        registered.insert(name.to_string(), Arc::new(move |disk| boxed(factory(disk))));
        Ok(())
    }

    /// Every driver an application has registered, in a stable order.
    ///
    /// For an error message or a configuration dump. The built-in drivers are
    /// [`ALL`](Setting::ALL) and are not repeated here.
    pub fn registered() -> Vec<String> {
        REGISTERED
            .read()
            .expect("the filesystem driver registry lock is poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// Whether `name` selects a driver an application registered.
    ///
    /// Answers for the name as a declaration would write it, so the same
    /// spellings [`DiskDriver::resolve`] accepts answer `true` here.
    pub fn is_registered(name: &str) -> bool {
        factory_for(name).is_some()
    }
}

/// Which driver a *declaration* names.
///
/// One of the [`FilesystemDriver`]s the framework ships, or a name an
/// application registered with [`extend`](FilesystemDriver::extend). Open where
/// [`FilesystemDriver`] is closed, and separate from it so that the closed set
/// stays matchable:
///
/// ```
/// # use rainier_filesystem::{DiskConfig, DiskDriver, FilesystemDriver};
/// let disk = DiskConfig::local("storage/app");
///
/// // Comparing against a built-in needs no unwrapping…
/// assert_eq!(disk.driver(), FilesystemDriver::Local);
///
/// // …and matching on one is still exhaustive over the closed set.
/// match disk.driver().built_in() {
///     Some(FilesystemDriver::Local) => {}
///     Some(FilesystemDriver::Memory | FilesystemDriver::S3) => unreachable!(),
///     None => unreachable!("this one is not an application's driver"),
/// }
///
/// assert!(DiskDriver::from(FilesystemDriver::S3).built_in().is_some());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DiskDriver {
    /// One the framework ships and builds itself.
    BuiltIn(FilesystemDriver),

    /// One an application registered, held as the name it registered under.
    ///
    /// The factory is looked up when the disk is built rather than kept here, so
    /// a declaration stays a plain value: cloneable, comparable, and printable
    /// without a closure in the middle of it.
    Custom(String),
}

impl DiskDriver {
    /// Resolve a written-down driver name against the built-ins and the
    /// registry.
    ///
    /// Fails, naming the value and listing every driver that would have been
    /// accepted, for anything neither set holds. It never answers with a
    /// default: a driver nobody recognises is a deployment that has to be fixed,
    /// not a disk that quietly becomes `local`.
    ///
    /// ```
    /// # use rainier_filesystem::{DiskDriver, FilesystemDriver};
    /// assert_eq!(DiskDriver::resolve("s3").unwrap(), FilesystemDriver::S3);
    /// assert!(DiskDriver::resolve("s4").is_err());
    /// ```
    ///
    /// # The exact spelling is tried before the tolerant one
    ///
    /// The same ordering, and for the same reason, as
    /// [`Setting::parse`]: `_` is accepted where the canonical spelling uses
    /// `-`, but only after an exact match has been looked for across *both*
    /// sets. Normalising first would let the rewritten form of one name select a
    /// different driver — a registered `my_store` rewritten to `my-store` and
    /// resolved to somebody else's registration is a disk on the wrong backend,
    /// which is the failure this module exists to prevent.
    ///
    /// [`extend`](FilesystemDriver::extend) also refuses to register a name that
    /// collides with an existing one under that normalisation, so there is never
    /// more than one candidate to choose between.
    pub fn resolve(raw: &str) -> Result<Self> {
        let name = raw.trim();

        if name.is_empty() {
            return Err(Error::internal(format!(
                "a disk's `driver` is empty; {}",
                expected_drivers()
            )));
        }

        // Exact, across both sets, before anything is rewritten.
        if let Some(built_in) =
            FilesystemDriver::ALL.iter().copied().find(|d| d.as_str().eq_ignore_ascii_case(name))
        {
            return Ok(Self::BuiltIn(built_in));
        }
        if let Some(registered) = registered_matching(|key| key.eq_ignore_ascii_case(name)) {
            return Ok(Self::Custom(registered));
        }

        // Then the `_`-for-`-` tolerance an environment variable wants.
        let wanted = canonical(name);
        if let Some(built_in) =
            FilesystemDriver::ALL.iter().copied().find(|d| canonical(d.as_str()) == wanted)
        {
            return Ok(Self::BuiltIn(built_in));
        }
        if let Some(registered) = registered_matching(|key| canonical(key) == wanted) {
            return Ok(Self::Custom(registered));
        }

        Err(Error::internal(format!(
            "`{name}` is not a valid filesystem driver; {}",
            expected_drivers()
        )))
    }

    /// The name, as a declaration writes it.
    pub fn as_str(&self) -> &str {
        match self {
            Self::BuiltIn(driver) => driver.as_str(),
            Self::Custom(name) => name,
        }
    }

    /// The built-in driver this is, or `None` for an application's own.
    ///
    /// The way back to the closed set, for code that wants to match on
    /// [`FilesystemDriver`] exhaustively.
    pub fn built_in(&self) -> Option<FilesystemDriver> {
        match self {
            Self::BuiltIn(driver) => Some(*driver),
            Self::Custom(_) => None,
        }
    }

    /// Whether this names a driver an application registered.
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }
}

impl From<FilesystemDriver> for DiskDriver {
    fn from(driver: FilesystemDriver) -> Self {
        Self::BuiltIn(driver)
    }
}

/// So a caller comparing a declaration against a built-in writes what it means.
///
/// `disk.driver() == FilesystemDriver::S3` rather than a match with a `None` arm
/// nobody has anything to say about. Both directions, so which side the built-in
/// is written on does not matter.
impl PartialEq<FilesystemDriver> for DiskDriver {
    fn eq(&self, other: &FilesystemDriver) -> bool {
        matches!(self, Self::BuiltIn(driver) if driver == other)
    }
}

impl PartialEq<DiskDriver> for FilesystemDriver {
    fn eq(&self, other: &DiskDriver) -> bool {
        other == self
    }
}

impl std::fmt::Display for DiskDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// --- the registry, from the inside -------------------------------------------

/// The factory registered for `name`, by the same rules
/// [`DiskDriver::resolve`] matches with.
pub(crate) fn factory_for(name: &str) -> Option<BoxedFactory> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let registered = REGISTERED.read().expect("the filesystem driver registry lock is poisoned");

    // Exact first, then the tolerant spelling — never the other way round.
    if let Some(factory) = registered
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, factory)| Arc::clone(factory))
    {
        return Some(factory);
    }

    let wanted = canonical(name);
    registered
        .iter()
        .find(|(key, _)| canonical(key) == wanted)
        .map(|(_, factory)| Arc::clone(factory))
}

/// The built-in driver `name` selects, if it selects one.
pub(crate) fn built_in_matching(name: &str) -> Option<FilesystemDriver> {
    let name = name.trim();
    FilesystemDriver::ALL
        .iter()
        .copied()
        .find(|driver| driver.as_str().eq_ignore_ascii_case(name))
        .or_else(|| {
            let wanted = canonical(name);
            FilesystemDriver::ALL
                .iter()
                .copied()
                .find(|driver| canonical(driver.as_str()) == wanted)
        })
}

/// The registered name a predicate picks out.
fn registered_matching(matches: impl Fn(&str) -> bool) -> Option<String> {
    REGISTERED
        .read()
        .expect("the filesystem driver registry lock is poisoned")
        .keys()
        .find(|key| matches(key))
        .cloned()
}

/// Whether two driver names select each other.
///
/// Case, surrounding whitespace and `_`-versus-`-` all collapse, because
/// [`DiskDriver::resolve`] accepts a name written any of those ways.
fn collides(one: &str, other: &str) -> bool {
    canonical(one) == canonical(other)
}

/// A driver name reduced to the form every spelling of it shares.
fn canonical(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

/// What a driver name could have been, for the error that says it was not.
///
/// The built-ins are listed **first and unbroken**, so the framework's own set
/// reads the same however many drivers an application has added to it.
///
/// Both spellings end on the ordering, because a name missing from this list is
/// as often a registration that has not run yet as it is a typo, and the reader
/// cannot tell which from the outside.
fn expected_drivers() -> String {
    let registered = FilesystemDriver::registered();

    if registered.is_empty() {
        return format!(
            "expected one of {}. No application driver has been registered either — one the \
             framework does not ship has to be registered with `FilesystemDriver::extend` before \
             the declaration naming it is built",
            FilesystemDriver::options()
        );
    }

    format!(
        "expected one of {}, or one registered with `FilesystemDriver::extend` before the \
         declaration naming it is built: {}",
        FilesystemDriver::options(),
        quoted(&registered)
    )
}

/// The registered drivers, for the error a declaration gets when it is built
/// and its driver is not among them.
pub(crate) fn registered_summary() -> String {
    let registered = FilesystemDriver::registered();

    if registered.is_empty() {
        return format!(
            "Nothing has been registered; the built-in drivers are {}",
            FilesystemDriver::options()
        );
    }

    format!(
        "Registered drivers are {}; the built-in drivers are {}",
        quoted(&registered),
        FilesystemDriver::options()
    )
}

/// Names, backtick-quoted and comma-separated, for a message.
fn quoted(names: &[String]) -> String {
    names.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryFilesystem;
    use rainier_support::Setting;

    /// A factory that builds something, so a registration under test is a
    /// registration that would work.
    async fn works(_disk: CustomDisk) -> Result<Arc<dyn Filesystem>> {
        Ok(Arc::new(MemoryFilesystem::new()))
    }

    #[test]
    fn only_object_storage_is_shared_and_durable() {
        assert!(FilesystemDriver::S3.is_shared());
        assert!(FilesystemDriver::S3.is_durable());

        for driver in FilesystemDriver::ALL.iter().filter(|d| **d != FilesystemDriver::S3) {
            assert!(!driver.is_shared(), "{driver} is per-machine");
            assert!(!driver.is_durable(), "{driver} does not outlive its machine");
        }
    }

    // --- resolving ----------------------------------------------------------

    #[test]
    fn a_built_in_resolves_without_anything_being_registered() {
        for driver in FilesystemDriver::ALL {
            assert_eq!(DiskDriver::resolve(driver.as_str()).unwrap(), *driver);
        }

        // And the same tolerances the closed set has always had.
        assert_eq!(DiskDriver::resolve("  S3 \n").unwrap(), FilesystemDriver::S3);
    }

    #[test]
    fn an_unknown_driver_names_itself_and_lists_the_built_ins() {
        let err = DiskDriver::resolve("s4").unwrap_err();

        assert!(err.message().contains("`s4`"), "{}", err.message());
        assert!(err.message().contains("`local`, `memory`, `s3`"), "{}", err.message());
    }

    #[test]
    fn a_registered_driver_resolves_by_name() {
        FilesystemDriver::extend("driver-resolves-by-name", works).unwrap();

        assert_eq!(
            DiskDriver::resolve("driver-resolves-by-name").unwrap(),
            DiskDriver::Custom("driver-resolves-by-name".to_string())
        );
        assert!(DiskDriver::resolve("driver-resolves-by-name").unwrap().is_custom());
        assert!(FilesystemDriver::is_registered("driver-resolves-by-name"));
    }

    #[test]
    fn an_unknown_driver_lists_what_is_registered() {
        FilesystemDriver::extend("driver-listed-in-the-error", works).unwrap();

        let err = DiskDriver::resolve("driver-that-nobody-registered").unwrap_err();

        assert!(err.message().contains("`driver-that-nobody-registered`"), "{}", err.message());
        assert!(err.message().contains("`driver-listed-in-the-error`"), "{}", err.message());
        assert!(err.message().contains("`local`, `memory`, `s3`"), "{}", err.message());
    }

    #[test]
    fn an_exact_registered_name_is_not_shadowed_by_a_normalisation() {
        // The regression `Setting::parse` was fixed for, in the shape it takes
        // once names are open: a registered `driver_underscored` must resolve as
        // itself, not be rewritten to `driver-underscored` and matched against
        // something else — or worse, not matched at all and reported invalid
        // while being listed among the valid ones.
        FilesystemDriver::extend("driver_underscored", works).unwrap();

        assert_eq!(
            DiskDriver::resolve("driver_underscored").unwrap(),
            DiskDriver::Custom("driver_underscored".to_string())
        );

        // The tolerance still applies where nothing exact matches.
        assert_eq!(
            DiskDriver::resolve("driver-underscored").unwrap(),
            DiskDriver::Custom("driver_underscored".to_string())
        );
    }

    // --- registering --------------------------------------------------------

    #[test]
    fn a_built_in_name_cannot_be_registered_over() {
        // Otherwise every disk declared `s3` moves onto the replacement, and not
        // one declaration changes to say so.
        for name in ["s3", "S3", "local", "memory"] {
            let err = FilesystemDriver::extend(name, works)
                .err()
                .expect("a built-in name is not available");

            assert!(err.message().contains("built-in"), "{}", err.message());
        }

        // And the built-ins still resolve to themselves afterwards.
        assert_eq!(DiskDriver::resolve("s3").unwrap(), FilesystemDriver::S3);
    }

    #[test]
    fn a_name_cannot_be_registered_twice() {
        FilesystemDriver::extend("driver-registered-once", works).unwrap();

        let err = FilesystemDriver::extend("driver-registered-once", works)
            .err()
            .expect("the name is taken");
        assert!(err.message().contains("already registered"), "{}", err.message());

        // Including under the spellings that select the same driver, so the
        // second registration cannot sneak in through the tolerance.
        for spelling in
            ["Driver-Registered-Once", "driver_registered_once", "  driver-registered-once  "]
        {
            assert!(FilesystemDriver::extend(spelling, works).is_err(), "{spelling}");
        }
    }

    #[test]
    fn an_empty_name_cannot_be_registered() {
        // Nothing could ever select it, so it is a registration that silently
        // does nothing.
        for blank in ["", "   ", "\t\n"] {
            assert!(FilesystemDriver::extend(blank, works).is_err(), "{blank:?}");
        }
        assert!(DiskDriver::resolve("").is_err());
    }

    // --- the typed ergonomics -----------------------------------------------

    #[test]
    fn a_built_in_compares_and_matches_without_handling_a_string() {
        let driver = DiskDriver::from(FilesystemDriver::S3);

        assert_eq!(driver, FilesystemDriver::S3);
        assert_eq!(FilesystemDriver::S3, driver);
        assert_ne!(driver, FilesystemDriver::Local);
        assert_eq!(driver.built_in(), Some(FilesystemDriver::S3));
        assert_eq!(driver.as_str(), "s3");
        assert_eq!(driver.to_string(), "s3");
        assert!(!driver.is_custom());
    }

    #[test]
    fn a_custom_driver_is_never_equal_to_a_built_in() {
        // Including one spelled like it. Equality answers what the disk *is*,
        // and a custom driver is not the framework's.
        let driver = DiskDriver::Custom("s3-alike".to_string());

        for built_in in FilesystemDriver::ALL {
            assert_ne!(driver, *built_in);
        }
        assert_eq!(driver.built_in(), None);
    }
}
