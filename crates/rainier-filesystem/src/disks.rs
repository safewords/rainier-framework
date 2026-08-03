//! Disks as configuration — [`Disks`], [`DiskConfig`], [`S3Disk`],
//! [`CustomDisk`].
//!
//! A [`Storage`] holds a default disk and a map of named ones, and something
//! has to put them there. Doing it imperatively works until two disks live on
//! **different backends**, at which point the loop that builds them all from
//! one connector produces a disk with the right bucket name pointed at the
//! wrong service. That failure does not raise anything: the bucket resolves,
//! the prefix is empty, and a listing reports nothing — which reads exactly
//! like a bucket that is genuinely empty.
//!
//! So a disk declares **its own** settings, and is built from those alone:
//!
//! ```
//! use rainier_filesystem::{DiskConfig, Disks, S3Disk};
//!
//! let disks = Disks::new("uploads")
//!     .with("uploads", DiskConfig::local("storage/app"))
//!     .with("archive", S3Disk::new("archive-bucket").region("us-east-1"));
//!
//! assert_eq!(disks.default_name(), "uploads");
//! assert!(disks.get("archive").is_some());
//! ```
//!
//! Declaring is separate from building, which is what lets the example above
//! run anywhere: [`build`](Disks::build) on an `s3` disk needs the `s3` feature
//! and **fails without it** rather than quietly substituting a local directory.
//! That is the right behaviour and it makes `build` the wrong thing to put in a
//! doc example — this one demonstrates the shape, and the tests below build.
//!
//! ## The same thing, from the configuration tree
//!
//! [`Disks`] deserialises from the shape a `filesystems` section already has —
//! a `default` naming one of the entries in `disks`, and each entry naming its
//! own driver:
//!
//! ```
//! # use rainier_filesystem::Disks;
//! # use serde_json::json;
//! let disks: Disks = serde_json::from_value(json!({
//!     "default": "uploads",
//!     "disks": {
//!         "uploads": { "driver": "local", "root": "storage/app" },
//!         "archive": {
//!             "driver": "s3",
//!             "bucket": "archive-bucket",
//!             "region": "auto",
//!             "endpoint": "https://account.example.com",
//!             "key": "…",
//!             "secret": "…",
//!         },
//!     },
//! })).unwrap();
//!
//! assert_eq!(disks.default_name(), "uploads");
//! ```
//!
//! Nothing here is an application's business but the values: the framework
//! names no disk, no bucket and no environment variable.
//!
//! ## What a declaration refuses
//!
//! Every rejection below is a case where accepting the declaration would give a
//! working-looking disk that reads or writes somewhere other than the one
//! intended, so each is a boot failure instead:
//!
//! | Declaration | Why it is refused |
//! |---|---|
//! | no `driver` | an assumed driver is a disk pointed at whatever the default happens to be |
//! | `bucket` on a `local` disk | somebody believes these files reach object storage; they reach a directory |
//! | `key` without `secret` | falls back to the ambient chain, and reads a **different account's** bucket of the same name |
//! | `key` and `secret` with no `region` | a signed request has to name one, and a guess is a wrong one |
//! | `default` naming an undeclared disk | the fallback would be silent, and the wrong disk |
//! | a `driver` no built-in and no registration answers to | the fallback would be a disk on whichever backend the default happens to be |
//!
//! ## A driver the framework does not ship
//!
//! The `driver` field is not limited to the drivers in this crate. An
//! application registers its own with
//! [`FilesystemDriver::extend`](crate::FilesystemDriver::extend) and then
//! declares it by name like any other, carrying whatever settings that driver
//! needs:
//!
//! ```json
//! { "driver": "my-store", "endpoint": "https://example.invalid", "namespace": "uploads" }
//! ```
//!
//! Those settings arrive at the factory as a [`CustomDisk`]. They are *not*
//! checked against the built-in field list — the framework has no idea what a
//! driver it does not ship needs — so a custom driver validates its own, which
//! [`CustomDisk::settings_as`] makes one `?`.
//!
//! The name still has to resolve. An unregistered one is refused when the
//! declaration is read, and a declaration assembled in code is refused when it
//! is [built](DiskConfig::build); neither ever falls back to a driver that
//! works. See [`driver`](crate::driver) for the two messages and why they
//! differ.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rainier_support::{Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::driver::{DiskDriver, FilesystemDriver};
use crate::filesystem::Filesystem;
use crate::{LocalFilesystem, MemoryFilesystem, Storage};

/// The disks an application declares, and which of them is the default.
///
/// The `filesystems` section, as a type. Deserialises from the configuration
/// tree and builds a [`Storage`] in one call, so declaring a disk is a config
/// edit rather than a line of wiring.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disks {
    /// Which entry of `disks` [`Storage`]'s default disk is.
    #[serde(default = "conventional_default")]
    default: String,

    /// Every declared disk, by the name callers reach it with.
    ///
    /// A `BTreeMap` so a dump and a build order are stable — a `HashMap` would
    /// make an error that lists the declared disks read differently each run.
    #[serde(default)]
    disks: BTreeMap<String, DiskConfig>,
}

/// The disk name assumed when a `filesystems` section does not say.
///
/// A convention rather than a guess at the application's naming: `local` is
/// what the equivalent section elsewhere defaults to, and a `default` naming a
/// disk that is not declared fails at [`build`](Disks::build) rather than
/// falling back.
fn conventional_default() -> String {
    "local".to_string()
}

impl Disks {
    /// An empty set whose default disk will be `default`.
    ///
    /// The name has to be declared with [`with`](Self::with) before
    /// [`build`](Self::build) will succeed.
    pub fn new(default: impl Into<String>) -> Self {
        Self { default: default.into(), disks: BTreeMap::new() }
    }

    /// Declare a disk under `name`.
    pub fn with(mut self, name: impl Into<String>, disk: impl Into<DiskConfig>) -> Self {
        self.disks.insert(name.into(), disk.into());
        self
    }

    /// The name of the disk that will be the default.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// The declaration filed under `name`.
    pub fn get(&self, name: &str) -> Option<&DiskConfig> {
        self.disks.get(name)
    }

    /// Every declared name, in a stable order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.disks.keys().map(String::as_str)
    }

    /// Whether anything is declared at all.
    pub fn is_empty(&self) -> bool {
        self.disks.is_empty()
    }

    /// Build every declared disk and assemble them into a [`Storage`].
    ///
    /// Each disk is built from **its own** declaration. There is no shared
    /// connector to inherit from, which is the entire point: two disks on two
    /// services with two credential sets are two backends, and the version of
    /// this that built them from one connector produced a second disk with the
    /// right bucket name pointed at the wrong host — reporting an empty prefix
    /// rather than an error.
    ///
    /// A disk is built **once** and registered under its name *and*, if it is
    /// the default, as the default. Building it twice would give
    /// `Storage::disk("uploads")` a different backend from the default disk
    /// even though both name one declaration — invisible for `local`, and for
    /// `memory` a write through one that cannot be read through the other.
    ///
    /// The default name is checked before anything is built, so a typo fails
    /// immediately instead of after resolving credentials for disks that were
    /// never going to be used.
    pub async fn build(&self) -> Result<Storage> {
        if !self.disks.contains_key(&self.default) {
            return Err(Error::internal(format!(
                "the default disk `{}` is not declared; declared disks are {}",
                self.default,
                self.declared()
            )));
        }

        let mut built: Vec<(&str, Arc<dyn Filesystem>)> = Vec::with_capacity(self.disks.len());
        for (name, disk) in &self.disks {
            let filesystem = disk
                .build()
                .await
                .map_err(|e| Error::internal(format!("disk `{name}`: {}", e.message())))?;
            built.push((name, filesystem));
        }

        let default = built
            .iter()
            .find(|(name, _)| *name == self.default)
            .map(|(_, disk)| Arc::clone(disk))
            .expect("the default was checked against the same map");

        let mut storage = Storage::new(default);
        for (name, disk) in built {
            storage = storage.with_disk(name, disk);
        }
        Ok(storage)
    }

    /// The declared names, backtick-quoted, for an error message.
    fn declared(&self) -> String {
        if self.disks.is_empty() {
            return "none".to_string();
        }
        self.names().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
    }
}

// Deliberately no `Default`: an empty set declares no disks, so its default
// name cannot resolve and `build` fails. A constructor whose result does not
// work is worse than one that asks for the one thing it needs.

impl std::fmt::Debug for Disks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disks").field("default", &self.default).field("disks", &self.disks).finish()
    }
}

/// One disk: which driver, and the settings that driver needs.
///
/// An enum rather than a struct of optionals, so the settings a driver does not
/// have cannot be written down: there is no `bucket` on a local disk to fill in
/// and wonder why it is ignored. The wire form is still flat — `driver` beside
/// the rest — because that is what a configuration file wants to be.
#[derive(Clone)]
pub enum DiskConfig {
    /// One directory on this machine.
    Local(LocalDisk),

    /// In memory, for tests.
    Memory,

    /// A bucket on S3 or anything that speaks its API.
    S3(S3Disk),

    /// A driver the framework does not ship, registered by the application.
    ///
    /// Its settings are not this crate's business, so they are carried as
    /// written and handed to whatever
    /// [`FilesystemDriver::extend`](crate::FilesystemDriver::extend)
    /// registered. Everything the variants above enforce — a driver that must
    /// be named, a name that must resolve, a build that fails rather than
    /// substitutes — still applies.
    Custom(CustomDisk),
}

impl DiskConfig {
    /// Files under a local directory.
    ///
    /// The shorthand. [`LocalDisk`] is the same thing with somewhere to hang a
    /// URL prefix.
    pub fn local(root: impl Into<PathBuf>) -> Self {
        Self::Local(LocalDisk::new(root))
    }

    /// Files in memory.
    pub fn memory() -> Self {
        Self::Memory
    }

    /// A disk on a driver the application registered.
    pub fn custom(driver: impl Into<String>) -> Self {
        Self::Custom(CustomDisk::new(driver))
    }

    /// Which driver this declares.
    ///
    /// A [`DiskDriver`] rather than a [`FilesystemDriver`], because a
    /// declaration may name one the framework does not ship. Comparing against
    /// a built-in still reads as it did — `disk.driver() ==
    /// FilesystemDriver::S3` — and [`built_in`](DiskDriver::built_in) is the way
    /// back to the closed set for code that matches on it.
    pub fn driver(&self) -> DiskDriver {
        match self {
            Self::Local(_) => FilesystemDriver::Local.into(),
            Self::Memory => FilesystemDriver::Memory.into(),
            Self::S3(_) => FilesystemDriver::S3.into(),
            Self::Custom(disk) => DiskDriver::Custom(disk.driver().to_string()),
        }
    }

    /// Build this disk, and only this disk.
    ///
    /// Every setting it uses comes from this declaration, so two disks built
    /// from two declarations share nothing — not a connector, not a credential,
    /// not an endpoint.
    pub async fn build(&self) -> Result<Arc<dyn Filesystem>> {
        match self {
            Self::Local(disk) => Ok(Arc::new(disk.build())),
            Self::Memory => Ok(Arc::new(MemoryFilesystem::new())),
            Self::Custom(disk) => disk.build().await,

            #[cfg(feature = "s3")]
            Self::S3(disk) => Ok(Arc::new(disk.build().await?)),

            // Loud, and naming the fix. Falling back to a local directory would
            // "work": uploads would land on a container's disk, be served back
            // for the life of that container, and vanish on the next deploy.
            //
            // Note that a driver whose feature is off is refused *here* rather
            // than being handed to the registry: `s3` is the framework's name
            // whether or not the feature is compiled in, so a build without it
            // is a build error and never a lookup that some registration could
            // answer.
            #[cfg(not(feature = "s3"))]
            Self::S3(disk) => Err(Error::internal(format!(
                "this disk uses the `s3` driver for bucket `{}`, but rainier-filesystem was \
                 built without the `s3` feature",
                disk.bucket()
            ))),
        }
    }

    /// This declaration as the flat form it is written in.
    fn wire_form(&self) -> Value {
        match self {
            Self::Local(disk) => RawDisk::for_local(disk).to_value(),
            Self::Memory => RawDisk::blank(FilesystemDriver::Memory).to_value(),
            Self::S3(disk) => RawDisk::for_s3(disk).to_value(),
            Self::Custom(disk) => disk.wire_form(),
        }
    }

    /// Read a declaration out of the flat form, driver first.
    ///
    /// The driver decides everything else, so it is resolved before any other
    /// field is looked at: a built-in goes through [`RawDisk`], which knows
    /// exactly which settings it has and refuses the rest, and an application's
    /// driver keeps its settings as written because nothing here knows what they
    /// should be.
    fn from_wire_form(value: Value) -> Result<Self> {
        let Value::Object(mut fields) = value else {
            return Err(Error::internal(
                "a disk is declared as a table of settings, one of which is `driver`",
            ));
        };

        let named = fields.remove("driver").ok_or_else(|| {
            Error::internal(
                "a disk declaration needs a `driver`; an assumed driver is a disk pointed at \
                 whichever backend the default happens to be",
            )
        })?;

        let named = named.as_str().ok_or_else(|| {
            Error::internal("a disk's `driver` names a driver, so it has to be a string")
        })?;

        match DiskDriver::resolve(named)? {
            DiskDriver::BuiltIn(built_in) => {
                // Put back the *canonical* spelling rather than what was
                // written, so the checked form cannot disagree with what was
                // just resolved.
                fields.insert("driver".to_string(), Value::String(built_in.to_string()));

                let raw: RawDisk = serde_json::from_value(Value::Object(fields))
                    .map_err(|e| Error::internal(e.to_string()))?;
                Self::try_from(raw)
            }
            DiskDriver::Custom(driver) => Ok(Self::Custom(CustomDisk { driver, settings: fields })),
        }
    }
}

impl From<S3Disk> for DiskConfig {
    fn from(disk: S3Disk) -> Self {
        Self::S3(disk)
    }
}

impl From<LocalDisk> for DiskConfig {
    fn from(disk: LocalDisk) -> Self {
        Self::Local(disk)
    }
}

impl From<CustomDisk> for DiskConfig {
    fn from(disk: CustomDisk) -> Self {
        Self::Custom(disk)
    }
}

/// Written and read as the flat table a configuration file wants.
///
/// Hand-written rather than derived through the built-in wire form, which names
/// every field the drivers in this crate have — exactly what makes it able to
/// refuse a setting one of them would ignore. An application's driver has fields
/// nobody here can enumerate, so it is carried as written.
impl Serialize for DiskConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.wire_form().serialize(serializer)
    }
}

/// Read through the tree the configuration already is.
///
/// A declaration has to be looked at *twice* — once for its `driver`, and again
/// for the settings that driver turns out to have — so it is buffered into the
/// same `serde_json::Value` the configuration repository holds rather than a
/// second representation of it. That does mean a self-describing format is
/// required, which every configuration format is.
impl<'de> Deserialize<'de> for DiskConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = Value::deserialize(deserializer)?;
        Self::from_wire_form(value).map_err(|e| D::Error::custom(e.message()))
    }
}

impl std::fmt::Debug for DiskConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(disk) => std::fmt::Debug::fmt(disk, f),
            Self::Memory => f.write_str("Memory"),
            Self::S3(disk) => std::fmt::Debug::fmt(disk, f),
            Self::Custom(disk) => std::fmt::Debug::fmt(disk, f),
        }
    }
}

/// One directory on this machine.
///
/// Survives a restart and not a redeploy, which is the distinction that catches
/// people out: a container's disk is not storage.
#[derive(Clone, Debug)]
pub struct LocalDisk {
    root: PathBuf,
    url: Option<String>,
}

impl LocalDisk {
    /// Files under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), url: None }
    }

    /// The URL prefix this directory is served at.
    ///
    /// Only meaningful if something — nginx, a CDN, a route you wrote —
    /// actually serves it. Without it [`Filesystem::url`] is `None`, because a
    /// link that 404s is worse than no link.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// The root directory.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// The public URL prefix, if one was declared.
    pub fn url_prefix(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// Build the disk, as its concrete driver.
    pub fn build(&self) -> LocalFilesystem {
        let disk = LocalFilesystem::new(&self.root);
        match &self.url {
            Some(url) => disk.with_url_prefix(url),
            None => disk,
        }
    }
}

/// A bucket on S3, Cloudflare R2, MinIO, B2 or Wasabi.
///
/// Which of those it is falls out of `endpoint` and `region`; the driver is the
/// same one either way.
#[derive(Clone)]
pub struct S3Disk {
    bucket: String,
    region: Option<String>,
    endpoint: Option<String>,
    url: Option<String>,
    path_style: bool,
    credentials: S3Credentials,
}

impl S3Disk {
    /// A disk on `bucket`, authenticating with the [ambient credential
    /// chain](S3Credentials::Chain).
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: None,
            endpoint: None,
            url: None,
            path_style: false,
            credentials: S3Credentials::Chain,
        }
    }

    /// The region to sign for. `auto` for Cloudflare R2.
    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Talk to something other than AWS — an R2 account host, a MinIO server.
    ///
    /// Also turns on path-style addressing, because a fixed endpoint host has
    /// nowhere to put a bucket subdomain.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// The public URL prefix objects are reachable at — a CDN, a custom domain.
    ///
    /// Without it [`Filesystem::url`] is `None`, because a private bucket's
    /// object URL answers `403` and a link that fails is worse than no link.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Put the bucket in the path rather than the host.
    ///
    /// Only ever turns it *on*: an endpoint override turns it on by itself, and
    /// there is no arrangement where a fixed host addresses a bucket by
    /// subdomain.
    pub fn path_style(mut self) -> Self {
        self.path_style = true;
        self
    }

    /// Authenticate with an explicit key pair rather than the ambient chain.
    ///
    /// For a service that is not AWS and has no chain to discover. A
    /// [`region`](Self::region) becomes required — see [`S3Credentials`].
    pub fn credentials(
        mut self,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        self.credentials = S3Credentials::Static {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
        };
        self
    }

    /// The bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The region, if one was declared.
    pub fn region_name(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// The endpoint override, if one was declared.
    pub fn endpoint_url(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// The public URL prefix, if one was declared.
    pub fn url_prefix(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// Whether addressing is path-style.
    pub fn is_path_style(&self) -> bool {
        self.path_style || self.endpoint.is_some()
    }

    /// How this disk authenticates.
    pub fn credential_source(&self) -> &S3Credentials {
        &self.credentials
    }

    /// Whether this declaration can be built.
    ///
    /// Checked when a declaration is deserialised so a bad `filesystems`
    /// section fails while the configuration is being read, and again when the
    /// disk is built so one assembled in code fails the same way with the same
    /// message.
    fn validate(&self) -> Result<()> {
        if matches!(self.credentials, S3Credentials::Static { .. }) && self.region.is_none() {
            return Err(Error::internal(format!(
                "the bucket `{}` is declared with `key` and `secret` but no `region`; a signed \
                 request has to name one (`auto` for Cloudflare R2)",
                self.bucket
            )));
        }
        Ok(())
    }
}

#[cfg(feature = "s3")]
impl S3Disk {
    /// The connector this disk signs with.
    ///
    /// Built per disk and never shared. Sharing one is the bug this module
    /// exists to make impossible: a second disk inheriting the first's endpoint
    /// and credentials keeps its own bucket *name*, resolves against the wrong
    /// service, and reports an empty prefix rather than an error.
    pub async fn connector(&self) -> Result<rainier_drivers::AwsConnector> {
        use rainier_drivers::AwsConnector;

        self.validate()?;

        let mut connector = match &self.credentials {
            S3Credentials::Chain => match &self.region {
                Some(region) => AwsConnector::in_region(region.clone()).await,
                None => AwsConnector::from_env().await,
            },
            S3Credentials::Static { access_key_id, secret_access_key } => {
                let region =
                    self.region.clone().expect("validate rejects a static pair without one");
                AwsConnector::with_credentials(
                    access_key_id.clone(),
                    secret_access_key.clone(),
                    region,
                )
                .await
            }
        };

        if let Some(endpoint) = &self.endpoint {
            connector = connector.endpoint(endpoint);
        }
        if self.path_style {
            connector = connector.path_style(true);
        }
        Ok(connector)
    }

    /// Build the disk, as its concrete driver.
    pub async fn build(&self) -> Result<crate::S3Filesystem> {
        let disk = crate::S3Filesystem::new(&self.connector().await?, &self.bucket);
        Ok(match &self.url {
            Some(url) => disk.with_url_prefix(url),
            None => disk,
        })
    }
}

impl std::fmt::Debug for S3Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Disk")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("url", &self.url)
            .field("path_style", &self.is_path_style())
            // The key pair is deliberately absent, not redacted-in-place: see
            // `S3Credentials`, whose own `Debug` names the source and nothing
            // else.
            .field("credentials", &self.credentials)
            .finish()
    }
}

/// How an [`S3Disk`] proves who it is.
///
/// Two cases, because there are two kinds of bucket. AWS itself hands out
/// temporary credentials through a chain that has to be *discovered and
/// refreshed* — an instance role, an EKS service account, an SSO cache, a
/// profile — and a process that pins one at boot starts answering `403` a few
/// hours later. Cloudflare R2, MinIO and B2 issue a static key pair and have no
/// chain at all.
///
/// [`Chain`](Self::Chain) is the default, and is the safe one to be wrong
/// about: a disk that should have named a key pair fails to authenticate, which
/// is loud. The reverse — a disk that names a key pair for the wrong service —
/// authenticates successfully against somebody else's bucket.
#[derive(Clone, Default)]
pub enum S3Credentials {
    /// Whatever the environment provides, discovered and refreshed by the SDK:
    /// environment variables, an EKS web identity token, the SSO cache, a
    /// profile, the ECS credential endpoint, EC2 instance metadata.
    #[default]
    Chain,

    /// An explicit key pair, for a service with no chain to discover.
    Static {
        /// The access key id.
        access_key_id: String,
        /// The secret access key.
        secret_access_key: String,
    },
}

/// Names the source and never the values.
///
/// Hand-written rather than derived, and it stays that way: a derived `Debug`
/// would print the key pair into whatever logged the disk, which for a
/// configuration dump at boot means the secret is in the log of every process
/// that started.
impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Chain => f.write_str("Chain"),
            Self::Static { .. } => f.write_str("Static(<redacted>)"),
        }
    }
}

/// A disk on a driver the framework does not ship.
///
/// The declaration, exactly as it was written, minus the `driver` key that
/// selected it. What is left is that driver's own settings, and nothing in this
/// crate pretends to know what they should be — the driver validates them, and
/// [`settings_as`](Self::settings_as) is the one-line way to do it against a
/// struct with `#[serde(deny_unknown_fields)]`, which gets a custom driver the
/// same refusal of a misfiled setting the built-ins have.
///
/// ```
/// use rainier_filesystem::CustomDisk;
///
/// let disk = CustomDisk::new("my-store")
///     .with("endpoint", "https://example.invalid")
///     .with("namespace", "uploads");
///
/// assert_eq!(disk.driver(), "my-store");
/// assert_eq!(disk.string("endpoint"), Some("https://example.invalid"));
/// assert_eq!(disk.string("missing"), None);
/// ```
///
/// Constructing one does **not** check that anything is registered under the
/// name — it is a declaration, and a declaration is checked when it is
/// [built](Self::build). That is the path where "register it before boot" is the
/// diagnosis rather than "you spelled it wrong".
#[derive(Clone)]
pub struct CustomDisk {
    driver: String,
    settings: Map<String, Value>,
}

impl CustomDisk {
    /// A disk on the driver registered under `driver`, with no settings yet.
    pub fn new(driver: impl Into<String>) -> Self {
        Self { driver: driver.into(), settings: Map::new() }
    }

    /// Declare a setting.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.settings.insert(key.into(), value.into());
        self
    }

    /// The driver name this disk was declared with.
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// Every setting, as written.
    pub fn settings(&self) -> &Map<String, Value> {
        &self.settings
    }

    /// One setting, as written.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.settings.get(key)
    }

    /// One setting, if it is a string.
    ///
    /// `None` both for a setting that is absent and for one that is not a
    /// string, because a driver reading `endpoint` wants the same answer either
    /// way: it was not declared usably. Where the difference matters,
    /// [`get`](Self::get) has it.
    pub fn string(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    /// The settings, deserialised into this driver's own type.
    ///
    /// The recommended way to read them. A struct with
    /// `#[serde(deny_unknown_fields)]` gets a custom driver the property the
    /// built-ins have — a misfiled setting is refused rather than dropped —
    /// which is worth more here than anywhere, since nothing else in this crate
    /// can check them.
    ///
    /// ```
    /// # use rainier_filesystem::CustomDisk;
    /// #[derive(serde::Deserialize)]
    /// #[serde(deny_unknown_fields)]
    /// struct MyStore {
    ///     endpoint: String,
    /// }
    ///
    /// let disk = CustomDisk::new("my-store").with("endpoint", "https://example.invalid");
    /// assert_eq!(disk.settings_as::<MyStore>().unwrap().endpoint, "https://example.invalid");
    ///
    /// let typo = CustomDisk::new("my-store").with("endpiont", "https://example.invalid");
    /// assert!(typo.settings_as::<MyStore>().is_err());
    /// ```
    pub fn settings_as<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(Value::Object(self.settings.clone())).map_err(|e| {
            Error::internal(format!("the `{}` disk's settings do not fit: {e}", self.driver))
        })
    }

    /// Build this disk through whatever was registered under its driver name.
    ///
    /// # It fails rather than substituting
    ///
    /// A name nothing is registered under is an error naming the driver, listing
    /// what *is* registered, and saying that registration has to come first.
    /// There is deliberately no fallback: a disk that quietly became `local`
    /// would accept every write, serve them back for the life of the container,
    /// and lose them on the next deploy — which is indistinguishable from
    /// working until it is not.
    pub async fn build(&self) -> Result<Arc<dyn Filesystem>> {
        // A built-in name in this slot means the declaration was assembled by
        // hand with a name the framework already owns. Handing it to the
        // registry would report it unregistered, which sends the reader looking
        // for a registration that must never exist.
        if let Some(built_in) = crate::driver::built_in_matching(&self.driver) {
            return Err(Error::internal(format!(
                "this disk names `{}` as an application driver, but `{built_in}` is one the \
                 framework ships; declare it as `{built_in}` so the framework's own driver builds \
                 it",
                self.driver
            )));
        }

        let factory = crate::driver::factory_for(&self.driver).ok_or_else(|| {
            Error::internal(format!(
                "no filesystem driver is registered under `{}`; register it with \
                 `FilesystemDriver::extend` before the disk that names it is built. {}",
                self.driver,
                crate::driver::registered_summary()
            ))
        })?;

        factory(self.clone()).await
    }

    /// This declaration as the flat table it was written as.
    fn wire_form(&self) -> Value {
        let mut fields = self.settings.clone();
        fields.insert("driver".to_string(), Value::String(self.driver.clone()));
        Value::Object(fields)
    }
}

/// Names the driver and the settings it was given, and never their values.
///
/// Hand-written for the reason [`S3Credentials`]' is: a driver the framework
/// does not ship is exactly the kind to be handed a bearer token or a signing
/// key, and this crate has no idea which of its settings those are. Printing the
/// keys says enough to diagnose a declaration; printing the values puts whatever
/// is in them in the log of every process that dumped its configuration at boot.
impl std::fmt::Debug for CustomDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomDisk")
            .field("driver", &self.driver)
            .field("settings", &self.settings.keys().collect::<Vec<_>>())
            .finish()
    }
}

// --- the wire form -----------------------------------------------------------

/// A **built-in** disk as it is written down, before it is known to make sense.
///
/// The flat shape a configuration file wants, which [`DiskConfig`] is the
/// checked form of. Everything but `driver` is optional here so the *driver*
/// gets to say which settings apply, and so a misfiled one can be named in the
/// error rather than silently dropped.
///
/// It covers the drivers this crate ships and no others: naming every field they
/// have is what lets `deny_unknown_fields` and
/// [`reject_settings_it_ignores`](RawDisk::reject_settings_it_ignores) refuse
/// anything else. A [`CustomDisk`] cannot go through it — its fields are the
/// application's, and enumerating them here would mean the framework deciding
/// what a driver it does not ship is allowed to be configured with.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisk {
    /// Required: an assumed driver is a disk pointed at whichever backend the
    /// default happens to be.
    driver: FilesystemDriver,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path_style: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

impl RawDisk {
    /// Refuse settings this driver would ignore.
    ///
    /// A `bucket` on a `local` disk is not a harmless extra key — it is
    /// somebody believing these files reach object storage when they reach a
    /// directory that goes away with the container. Dropping it silently is how
    /// that belief survives to production.
    fn reject_settings_it_ignores(&self, used: &[&str]) -> Result<()> {
        let declared: [(&str, bool); 8] = [
            ("root", self.root.is_some()),
            ("bucket", self.bucket.is_some()),
            ("region", self.region.is_some()),
            ("endpoint", self.endpoint.is_some()),
            ("url", self.url.is_some()),
            ("path_style", self.path_style.is_some()),
            ("key", self.key.is_some()),
            ("secret", self.secret.is_some()),
        ];

        let ignored: Vec<String> = declared
            .iter()
            .filter(|(name, present)| *present && !used.contains(name))
            .map(|(name, _)| format!("`{name}`"))
            .collect();

        if ignored.is_empty() {
            return Ok(());
        }

        Err(Error::internal(format!(
            "the `{}` driver does not use {}; that setting would be ignored, and a disk that \
             ignores where it was told to write is one that writes somewhere else",
            self.driver,
            ignored.join(", ")
        )))
    }
}

impl TryFrom<RawDisk> for DiskConfig {
    type Error = Error;

    fn try_from(raw: RawDisk) -> Result<Self> {
        match raw.driver {
            FilesystemDriver::Local => {
                raw.reject_settings_it_ignores(&["root", "url"])?;
                let root = raw.root.ok_or_else(|| {
                    Error::internal("a `local` disk needs a `root` directory to store under")
                })?;
                Ok(Self::Local(LocalDisk { root, url: raw.url }))
            }

            FilesystemDriver::Memory => {
                raw.reject_settings_it_ignores(&[])?;
                Ok(Self::Memory)
            }

            FilesystemDriver::S3 => {
                raw.reject_settings_it_ignores(&[
                    "bucket",
                    "region",
                    "endpoint",
                    "url",
                    "path_style",
                    "key",
                    "secret",
                ])?;

                let bucket =
                    raw.bucket.ok_or_else(|| Error::internal("an `s3` disk needs a `bucket`"))?;

                let credentials = match (raw.key, raw.secret) {
                    (None, None) => S3Credentials::Chain,
                    (Some(access_key_id), Some(secret_access_key)) => {
                        S3Credentials::Static { access_key_id, secret_access_key }
                    }
                    // Half a key pair is the dangerous case, so it is the one
                    // spelled out: the missing half would fall back to the
                    // ambient chain, which for a bucket on another service
                    // means signing as *this* account against a bucket of the
                    // same name somewhere else — and reading it empty.
                    (Some(_), None) | (None, Some(_)) => {
                        return Err(Error::internal(format!(
                            "the `s3` disk for bucket `{bucket}` declares one of `key` and \
                             `secret` but not the other; with only one it would authenticate \
                             from the ambient credential chain instead, against whatever \
                             bucket of that name the chain's account can reach"
                        )))
                    }
                };

                let disk = S3Disk {
                    bucket,
                    region: raw.region,
                    endpoint: raw.endpoint,
                    url: raw.url,
                    path_style: raw.path_style.unwrap_or(false),
                    credentials,
                };
                disk.validate()?;

                Ok(Self::S3(disk))
            }
        }
    }
}

impl RawDisk {
    /// A declaration naming only its driver.
    fn blank(driver: FilesystemDriver) -> Self {
        Self {
            driver,
            root: None,
            bucket: None,
            region: None,
            endpoint: None,
            url: None,
            path_style: None,
            key: None,
            secret: None,
        }
    }

    /// The written form of a local disk.
    fn for_local(disk: &LocalDisk) -> Self {
        Self {
            root: Some(disk.root.clone()),
            url: disk.url.clone(),
            ..Self::blank(FilesystemDriver::Local)
        }
    }

    /// The written form of an object-storage disk.
    fn for_s3(disk: &S3Disk) -> Self {
        let (key, secret) = match &disk.credentials {
            S3Credentials::Chain => (None, None),
            S3Credentials::Static { access_key_id, secret_access_key } => {
                (Some(access_key_id.clone()), Some(secret_access_key.clone()))
            }
        };

        Self {
            bucket: Some(disk.bucket.clone()),
            region: disk.region.clone(),
            endpoint: disk.endpoint.clone(),
            url: disk.url.clone(),
            // Written back only when it was the disk's own doing: an endpoint
            // implies it, and re-emitting the implication as a literal would
            // make a round trip say more than the original did.
            path_style: disk.path_style.then_some(true),
            key,
            secret,
            ..Self::blank(FilesystemDriver::S3)
        }
    }

    /// This declaration as the flat table it is serialised as.
    fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("a RawDisk is a flat table of strings and booleans")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rainier_support::BoxFuture;
    use serde_json::json;

    use crate::{FilesystemExt, Metadata};

    // --- a driver the framework does not ship -------------------------------

    /// A driver registered by an "application", for the tests that need one to
    /// be **distinguishable** from a built-in.
    ///
    /// It reports its own name and keeps the settings it was declared with,
    /// which is the whole point: a factory answering with a bare
    /// [`MemoryFilesystem`] would be indistinguishable from a silent fallback to
    /// the `memory` driver, and every assertion below would pass for the bug.
    struct BespokeFilesystem {
        name: String,
        settings: Map<String, Value>,
        inner: MemoryFilesystem,
    }

    impl BespokeFilesystem {
        fn new(disk: &CustomDisk) -> Self {
            Self {
                name: disk.driver().to_string(),
                settings: disk.settings().clone(),
                inner: MemoryFilesystem::new(),
            }
        }
    }

    impl Filesystem for BespokeFilesystem {
        fn name(&self) -> &str {
            &self.name
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Bytes>>> {
            self.inner.get(path)
        }

        fn put<'a>(&'a self, path: &'a str, contents: Bytes) -> BoxFuture<'a, Result<()>> {
            self.inner.put(path, contents)
        }

        fn delete<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
            self.inner.delete(path)
        }

        fn exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<bool>> {
            self.inner.exists(path)
        }

        fn metadata<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<Metadata>>> {
            self.inner.metadata(path)
        }

        fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<Metadata>>> {
            self.inner.list(prefix)
        }

        fn directories<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<String>>> {
            self.inner.directories(prefix)
        }
    }

    /// Register [`BespokeFilesystem`] under `name`.
    ///
    /// Each test uses a name of its own: the registry is process-wide, tests run
    /// in parallel, and a name shared between two of them would make whichever
    /// registered second fail.
    fn register(name: &'static str) {
        FilesystemDriver::extend(name, |disk: CustomDisk| async move {
            Ok(Arc::new(BespokeFilesystem::new(&disk)) as Arc<dyn Filesystem>)
        })
        .expect("this test's driver name is its own");
    }

    // --- reading a declaration ---------------------------------------------

    #[test]
    fn a_section_deserialises_into_the_disks_it_declares() {
        let disks: Disks = serde_json::from_value(json!({
            "default": "uploads",
            "disks": {
                "uploads": { "driver": "local", "root": "storage/app" },
                "scratch": { "driver": "memory" },
                "archive": { "driver": "s3", "bucket": "archive-bucket", "region": "us-east-1" },
            },
        }))
        .unwrap();

        assert_eq!(disks.default_name(), "uploads");
        assert_eq!(disks.names().collect::<Vec<_>>(), vec!["archive", "scratch", "uploads"]);
        assert_eq!(disks.get("uploads").unwrap().driver(), FilesystemDriver::Local);
        assert_eq!(disks.get("scratch").unwrap().driver(), FilesystemDriver::Memory);
        assert_eq!(disks.get("archive").unwrap().driver(), FilesystemDriver::S3);
    }

    #[test]
    fn a_disk_without_a_driver_is_refused() {
        // An assumed driver is a disk pointed at whichever backend the default
        // happens to be, which is the whole failure this module is about.
        let err = serde_json::from_value::<Disks>(json!({
            "disks": { "uploads": { "root": "storage/app" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("driver"), "{err}");
    }

    #[test]
    fn a_misspelled_driver_lists_the_valid_ones() {
        let err = serde_json::from_value::<Disks>(json!({
            "disks": { "uploads": { "driver": "s4", "bucket": "b" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`local`, `memory`, `s3`"), "{err}");
    }

    #[test]
    fn a_setting_the_driver_ignores_is_refused_rather_than_dropped() {
        // Someone believes these files reach object storage. They reach a
        // directory that goes away with the container.
        let err = serde_json::from_value::<Disks>(json!({
            "disks": { "uploads": { "driver": "local", "root": "app", "bucket": "b" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("`bucket`"), "{err}");
        assert!(err.contains("does not use"), "{err}");
    }

    #[test]
    fn an_unknown_setting_is_refused_rather_than_dropped() {
        let err = serde_json::from_value::<Disks>(json!({
            "disks": { "uploads": { "driver": "s3", "bucket": "b", "buckett": "typo" } },
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("buckett"), "{err}");
    }

    #[test]
    fn a_local_disk_needs_a_root_and_an_s3_disk_needs_a_bucket() {
        let no_root = serde_json::from_value::<DiskConfig>(json!({ "driver": "local" }))
            .unwrap_err()
            .to_string();
        assert!(no_root.contains("`root`"), "{no_root}");

        let no_bucket = serde_json::from_value::<DiskConfig>(json!({ "driver": "s3" }))
            .unwrap_err()
            .to_string();
        assert!(no_bucket.contains("`bucket`"), "{no_bucket}");
    }

    #[test]
    fn a_declaration_round_trips_through_its_wire_form() {
        let original = json!({
            "driver": "s3",
            "bucket": "archive-bucket",
            "region": "auto",
            "endpoint": "https://account.example.com",
            "url": "https://cdn.example.com",
            "key": "id",
            "secret": "shh",
        });

        let disk: DiskConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&disk).unwrap(), original);
    }

    #[test]
    fn a_local_declaration_round_trips_without_inventing_settings() {
        let original = json!({ "driver": "local", "root": "storage/app" });

        let disk: DiskConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&disk).unwrap(), original);
    }

    #[tokio::test]
    async fn a_local_disk_has_a_url_only_where_something_serves_it() {
        // A link that 404s is worse than no link, so a root nobody serves has
        // no URL rather than a guessed one.
        let storage = Disks::new("private")
            .with("private", LocalDisk::new("storage/app"))
            .with("public", LocalDisk::new("public/files").url("https://example.com/files/"))
            .build()
            .await
            .unwrap();

        assert_eq!(storage.disk("private").unwrap().url("a.txt"), None);
        assert_eq!(
            storage.disk("public").unwrap().url("a.txt").as_deref(),
            Some("https://example.com/files/a.txt")
        );
    }

    // --- credentials --------------------------------------------------------

    #[test]
    fn credentials_default_to_the_ambient_chain() {
        // The safe direction: a disk that should have named a key pair fails to
        // authenticate. The reverse authenticates against somebody else's
        // bucket.
        let disk: DiskConfig =
            serde_json::from_value(json!({ "driver": "s3", "bucket": "b" })).unwrap();

        let DiskConfig::S3(s3) = disk else { panic!("declared as s3") };
        assert!(matches!(s3.credential_source(), S3Credentials::Chain));
    }

    #[test]
    fn half_a_key_pair_is_refused_rather_than_falling_back_to_the_chain() {
        for half in [
            json!({ "driver": "s3", "bucket": "b", "key": "id", "region": "auto" }),
            json!({ "driver": "s3", "bucket": "b", "secret": "shh", "region": "auto" }),
        ] {
            let err = serde_json::from_value::<DiskConfig>(half).unwrap_err().to_string();
            assert!(err.contains("ambient credential chain"), "{err}");
        }
    }

    #[test]
    fn an_explicit_key_pair_needs_a_region_to_sign_for() {
        let err = serde_json::from_value::<DiskConfig>(
            json!({ "driver": "s3", "bucket": "b", "key": "id", "secret": "shh" }),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("`region`"), "{err}");
    }

    #[test]
    fn no_debug_rendering_discloses_a_credential() {
        // The one that has to hold whatever else changes: a config dump at boot
        // must not put the secret in the log of every process that started.
        let disks = Disks::new("archive").with(
            "archive",
            S3Disk::new("archive-bucket")
                .region("auto")
                .credentials("AKIA-visible", "super-secret"),
        );

        let rendered = format!("{disks:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("AKIA-visible"), "{rendered}");
        assert!(rendered.contains("archive-bucket"), "{rendered}");
    }

    // --- building -----------------------------------------------------------

    #[tokio::test]
    async fn a_local_disk_and_an_object_disk_coexist_in_one_registry() {
        let disks: Disks = serde_json::from_value(json!({
            "default": "uploads",
            "disks": {
                "uploads": { "driver": "local", "root": "storage/app" },
                "archive": { "driver": "s3", "bucket": "archive-bucket", "region": "us-east-1" },
            },
        }))
        .unwrap();

        // Without the `s3` feature the second disk cannot be built at all, and
        // says so rather than quietly becoming a local directory.
        if cfg!(feature = "s3") {
            let storage = disks.build().await.unwrap();

            assert_eq!(storage.driver(), "local");
            assert_eq!(storage.disk("uploads").unwrap().driver(), "local");
            assert_eq!(storage.disk("archive").unwrap().driver(), "s3");
        } else {
            let err = disks.build().await.err().expect("no s3 driver to build with");
            assert!(err.message().contains("without the `s3` feature"), "{}", err.message());
        }
    }

    #[tokio::test]
    async fn an_unregistered_name_is_still_none() {
        let disks = Disks::new("uploads").with("uploads", DiskConfig::memory());
        let storage = disks.build().await.unwrap();

        assert!(storage.disk("uploads").is_some());
        assert!(storage.disk("archive").is_none());
        assert!(!storage.has_disk("archive"));
    }

    #[tokio::test]
    async fn the_default_disk_is_the_same_backend_as_its_name() {
        // Built once, registered twice. Building it twice would give
        // `disk("scratch")` a different memory store from the default, and a
        // write through one would be invisible through the other.
        use crate::FilesystemExt as _;

        let storage =
            Disks::new("scratch").with("scratch", DiskConfig::memory()).build().await.unwrap();

        storage.put_string("a.txt", "hello").await.unwrap();
        assert_eq!(
            storage.disk("scratch").unwrap().get_string("a.txt").await.unwrap().as_deref(),
            Some("hello")
        );
    }

    #[tokio::test]
    async fn two_disks_declared_separately_do_not_share_storage() {
        use crate::FilesystemExt as _;

        let storage = Disks::new("uploads")
            .with("uploads", DiskConfig::memory())
            .with("archive", DiskConfig::memory())
            .build()
            .await
            .unwrap();

        storage.disk("uploads").unwrap().put_string("a.txt", "one").await.unwrap();

        assert!(!storage.disk("archive").unwrap().exists("a.txt").await.unwrap());
    }

    #[tokio::test]
    async fn a_default_naming_an_undeclared_disk_fails_instead_of_falling_back() {
        let disks = Disks::new("uploads").with("archive", DiskConfig::memory());

        let err = disks.build().await.err().expect("the default is not declared");
        assert!(err.message().contains("`uploads`"), "{}", err.message());
        assert!(err.message().contains("`archive`"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_build_failure_names_the_disk_it_came_from() {
        // With a dozen disks declared, "needs a region" without a name is a
        // search rather than a fix.
        let disks =
            Disks::new("archive").with("archive", S3Disk::new("b").credentials("id", "shh"));

        let err = disks.build().await.err().expect("no region to sign for");
        assert!(err.message().starts_with("disk `archive`:"), "{}", err.message());
    }

    // --- two backends, two endpoints ----------------------------------------

    /// The bug this module exists for: two disks on two services, built from one
    /// connector, giving the second disk the right bucket name pointed at the
    /// wrong host. It does not raise — it reads an empty prefix, which is
    /// indistinguishable from an empty bucket.
    ///
    /// Asserted on the built disks rather than on the declarations, so
    /// reintroducing a shared connector inside [`Disks::build`] fails here.
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn two_disks_on_different_endpoints_are_two_different_backends() {
        use crate::S3Filesystem;

        let disks: Disks = serde_json::from_value(json!({
            "default": "uploads",
            "disks": {
                "uploads": {
                    "driver": "s3",
                    "bucket": "shared-name",
                    "region": "us-east-1",
                },
                "archive": {
                    "driver": "s3",
                    "bucket": "shared-name",
                    "region": "eu-west-2",
                    "endpoint": "https://archive.example.invalid",
                    "key": "other-id",
                    "secret": "other-secret",
                },
            },
        }))
        .unwrap();

        let storage = disks.build().await.unwrap();

        // The same bucket name on both, which is the point: the name is not
        // what distinguishes them.
        let uploads = storage.disk("uploads").unwrap();
        let archive = storage.disk("archive").unwrap();
        let uploads = uploads.as_driver::<S3Filesystem>().expect("built as s3");
        let archive = archive.as_driver::<S3Filesystem>().expect("built as s3");
        assert_eq!(uploads.bucket(), archive.bucket());

        // The region each client will sign for comes from its own declaration.
        // One shared connector makes these equal, and this is the assertion
        // that catches it.
        let region = |disk: &S3Filesystem| {
            disk.client().inner().config().region().map(|region| region.to_string())
        };
        assert_eq!(region(uploads).as_deref(), Some("us-east-1"));
        assert_eq!(region(archive).as_deref(), Some("eu-west-2"));

        // And so does the host it will sign against.
        let declared = |name: &str| match disks.get(name).unwrap() {
            DiskConfig::S3(disk) => disk.clone(),
            other => panic!("`{name}` declared as {:?}", other.driver()),
        };
        assert_eq!(declared("uploads").connector().await.unwrap().endpoint_url(), None);
        assert_eq!(
            declared("archive").connector().await.unwrap().endpoint_url(),
            Some("https://archive.example.invalid")
        );

        // The negative control, so the assertions above are known to be able to
        // fail. This is the shared-connector build, done by hand: one
        // connector, both buckets. The region collapses to one value — and the
        // second disk, which has the right bucket name, is now pointed at the
        // wrong service and will report its prefix empty.
        let shared = declared("uploads").connector().await.unwrap();
        let one = S3Filesystem::new(&shared, "shared-name");
        let two = S3Filesystem::new(&shared, "shared-name");
        assert_eq!(region(&one), region(&two), "sharing a connector is what this must catch");
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn an_endpoint_turns_on_path_style_without_being_asked() {
        // The usual reason R2 and MinIO "do not work", and not something a
        // declaration should have to remember.
        let disk = S3Disk::new("b").region("auto").endpoint("https://account.example.com");

        assert!(disk.is_path_style());
        assert!(disk.connector().await.unwrap().is_path_style());
    }

    // --- a driver the application registered ---------------------------------

    #[tokio::test]
    async fn a_registered_driver_is_declared_and_reached_like_any_other() {
        register("reachable-store");

        let disks: Disks = serde_json::from_value(json!({
            "default": "uploads",
            "disks": {
                "uploads": { "driver": "local", "root": "storage/app" },
                "bespoke": {
                    "driver": "reachable-store",
                    "endpoint": "https://example.invalid",
                    "namespace": "uploads",
                },
            },
        }))
        .unwrap();

        // Declared beside the built-ins, and named as itself.
        assert_eq!(disks.get("uploads").unwrap().driver(), FilesystemDriver::Local);
        assert_eq!(
            disks.get("bespoke").unwrap().driver(),
            DiskDriver::Custom("reachable-store".to_string())
        );

        let storage = disks.build().await.unwrap();
        let bespoke = storage.disk("bespoke").expect("declared under this name");

        // The disk that came back is the registered driver's, not a stand-in:
        // it names itself, which no built-in would.
        assert_eq!(bespoke.driver(), "reachable-store");

        // And it was handed its own declaration's settings.
        let built = bespoke.as_driver::<BespokeFilesystem>().expect("built by the registration");
        assert_eq!(
            built.settings.get("endpoint").and_then(Value::as_str),
            Some("https://example.invalid")
        );
        assert_eq!(built.settings.get("namespace").and_then(Value::as_str), Some("uploads"));
        // The key that selected the driver is not one of its settings.
        assert!(!built.settings.contains_key("driver"));

        // It is a working disk, not merely a resolved name.
        bespoke.put_string("a.txt", "hello").await.unwrap();
        assert_eq!(bespoke.get_string("a.txt").await.unwrap().as_deref(), Some("hello"));

        // And it is separate from the disk beside it, like any other pair.
        assert!(!storage.disk("uploads").unwrap().exists("a.txt").await.unwrap());
    }

    #[tokio::test]
    async fn a_registered_driver_can_be_the_default_disk() {
        register("default-store");

        let storage = Disks::new("bespoke")
            .with("bespoke", CustomDisk::new("default-store").with("endpoint", "https://x.invalid"))
            .build()
            .await
            .unwrap();

        assert_eq!(storage.driver(), "default-store");
        assert!(storage.as_driver::<BespokeFilesystem>().is_some());
    }

    #[test]
    fn a_custom_declaration_round_trips_through_its_wire_form() {
        register("round-trip-store");

        // Settings this crate has no field for, and of types it never uses —
        // they belong to the driver, so they survive as written.
        let original = json!({
            "driver": "round-trip-store",
            "endpoint": "https://example.invalid",
            "retries": 3,
            "verify": true,
            "headers": { "x-tenant": "one" },
        });

        let disk: DiskConfig = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&disk).unwrap(), original);

        // The shorthand constructor names the same driver as the declaration.
        assert_eq!(disk.driver(), DiskConfig::custom("round-trip-store").driver());
    }

    #[test]
    fn a_custom_driver_settles_its_own_settings_rather_than_this_crate_s() {
        // A `bucket` on a `local` disk is refused because this crate knows
        // `local` has no bucket. It knows nothing about an application's driver,
        // so the driver is the one that gets to refuse — and `settings_as` makes
        // that the same refusal.
        register("validating-store");

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Settings {
            endpoint: String,
        }

        let disk: DiskConfig = serde_json::from_value(json!({
            "driver": "validating-store",
            "endpoint": "https://example.invalid",
        }))
        .unwrap();
        let DiskConfig::Custom(custom) = disk else { panic!("declared as a custom driver") };
        assert_eq!(custom.settings_as::<Settings>().unwrap().endpoint, "https://example.invalid");

        let typo: DiskConfig = serde_json::from_value(json!({
            "driver": "validating-store",
            "endpiont": "https://example.invalid",
        }))
        .unwrap();
        let DiskConfig::Custom(typo) = typo else { panic!("declared as a custom driver") };
        let err = typo.settings_as::<Settings>().err().expect("`endpiont` is not a setting");
        assert!(err.message().contains("endpiont"), "{}", err.message());
    }

    #[test]
    fn no_debug_rendering_discloses_a_custom_driver_s_settings() {
        // The same rule the S3 key pair has, applied where this crate cannot
        // tell which setting is the secret: a driver it does not ship is exactly
        // the kind to be handed a bearer token, so the keys are printed and the
        // values never are.
        register("secretive-store");

        let disks = Disks::new("bespoke").with(
            "bespoke",
            CustomDisk::new("secretive-store")
                .with("token", "super-secret")
                .with("endpoint", "https://example.invalid"),
        );

        let rendered = format!("{disks:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("secretive-store"), "{rendered}");
        assert!(rendered.contains("token"), "{rendered}");
    }

    // --- a driver nobody registered ------------------------------------------

    #[test]
    fn an_unregistered_driver_is_refused_when_the_declaration_is_read() {
        register("store-that-is-registered");

        let err = serde_json::from_value::<Disks>(json!({
            "disks": { "uploads": { "driver": "store-that-is-not" } },
        }))
        .unwrap_err()
        .to_string();

        // Names the driver, lists the built-ins, and lists what is registered.
        assert!(err.contains("`store-that-is-not`"), "{err}");
        assert!(err.contains("`local`, `memory`, `s3`"), "{err}");
        assert!(err.contains("`store-that-is-registered`"), "{err}");
    }

    #[tokio::test]
    async fn a_declaration_built_before_its_driver_is_registered_says_so() {
        // The one somebody will hit. A declaration assembled in code is not read
        // through serde, so nothing checks the name until the disk is built —
        // and "register it first" is a different fix from "you spelled it
        // wrong", so it is a different message.
        let disks = Disks::new("bespoke").with("bespoke", CustomDisk::new("store-registered-late"));

        let err = disks.build().await.err().expect("nothing is registered under that name");

        assert!(err.message().starts_with("disk `bespoke`:"), "{}", err.message());
        assert!(err.message().contains("`store-registered-late`"), "{}", err.message());
        assert!(err.message().contains("no filesystem driver is registered"), "{}", err.message());
        assert!(
            err.message().contains("before the disk that names it is built"),
            "{}",
            err.message()
        );
        // And what it could have been, so the answer is in the message.
        assert!(err.message().contains("`local`, `memory`, `s3`"), "{}", err.message());

        // Registering it is the fix, and it is the *only* fix — the same
        // declaration builds once the driver exists.
        register("store-registered-late");
        assert_eq!(disks.build().await.unwrap().driver(), "store-registered-late");
    }

    #[tokio::test]
    async fn a_built_in_name_declared_as_a_custom_disk_is_sent_back_to_the_built_in() {
        // Reachable only by assembling a declaration in code. Reporting `s3`
        // unregistered would send the reader looking for a registration that
        // must never exist — `extend` refuses to make one.
        let err = CustomDisk::new("s3")
            .build()
            .await
            .err()
            .expect("`s3` is the framework's, not an application's");

        assert!(err.message().contains("the framework ships"), "{}", err.message());
        assert!(err.message().contains("`s3`"), "{}", err.message());
    }

    #[tokio::test]
    async fn a_built_in_whose_feature_is_off_is_an_error_and_not_a_substitution() {
        // Two halves of one property. `s3` is the framework's name whether or
        // not the feature is compiled in, so: no registration can claim it…
        let taken = FilesystemDriver::extend("s3", |_disk: CustomDisk| async move {
            Ok(Arc::new(MemoryFilesystem::new()) as Arc<dyn Filesystem>)
        })
        .err()
        .expect("`s3` is a built-in name");
        assert!(taken.message().contains("built-in"), "{}", taken.message());

        // …and a declaration naming it is the built-in driver either way, so
        // there is no lookup for a registration to answer.
        let disks: Disks = serde_json::from_value(json!({
            "default": "archive",
            "disks": { "archive": { "driver": "s3", "bucket": "b", "region": "us-east-1" } },
        }))
        .unwrap();
        assert_eq!(disks.get("archive").unwrap().driver(), FilesystemDriver::S3);

        if cfg!(feature = "s3") {
            assert_eq!(disks.build().await.unwrap().disk("archive").unwrap().driver(), "s3");
        } else {
            let err = disks.build().await.err().expect("no s3 driver to build with");
            assert!(err.message().contains("without the `s3` feature"), "{}", err.message());
        }
    }

    /// **The test that must not be made to pass by relaxing it.**
    ///
    /// Every route a driver name can travel, swept with names nothing answers
    /// to. If anyone later adds a convenience fallback — a declaration that
    /// resolves to the default driver, a build that shrugs and returns a local
    /// directory — every other test in this crate still passes, and a disk
    /// declared for object storage silently starts writing to a container's
    /// filesystem: accepted, served back for the life of that container, gone on
    /// the next deploy.
    ///
    /// So this asserts on the *absence of a working disk*, not on the wording of
    /// an error.
    #[tokio::test]
    async fn no_route_from_an_unrecognised_driver_reaches_a_working_disk() {
        // A positive control first, so the sweep below is known to be able to
        // pass rather than merely never reaching its assertions.
        register("sweep-control-store");
        assert!(Disks::new("d")
            .with("d", CustomDisk::new("sweep-control-store"))
            .build()
            .await
            .is_ok());

        for name in [
            "ceph",          // a real backend nobody registered
            "s4",            // a typo for a built-in
            "loca",          // a truncation of one
            "local-disk",    // a built-in with something appended
            "s3-compatible", // the kind of name that sounds official
            "default",       // a word somebody might expect to mean "the usual one"
            "none",          // and one that might be expected to mean "no driver"
            "",              // written as empty
            "   ",           // and as blank
        ] {
            // 1. read as a `filesystems` section.
            let section = serde_json::from_value::<Disks>(json!({
                "default": "uploads",
                "disks": { "uploads": { "driver": name } },
            }));
            assert!(section.is_err(), "the section declaring `{name}` was accepted");

            // 2. read as one declaration, in case the section is what refused.
            let declaration = serde_json::from_value::<DiskConfig>(json!({ "driver": name }));
            assert!(declaration.is_err(), "the declaration naming `{name}` was accepted");

            // 3. assembled in code, where nothing is read through serde at all
            //    and the only check left is the one `build` does.
            let built = Disks::new("uploads").with("uploads", CustomDisk::new(name)).build().await;

            if let Ok(storage) = built {
                panic!(
                    "a disk declared with the driver `{name}` — which is not built in and is \
                     registered nowhere — was built anyway, on the `{}` driver. That is the \
                     silent substitution this crate is shaped around: every write to that disk \
                     appears to succeed and goes somewhere other than where it was declared, and \
                     nothing about the call site reads as broken. An unrecognised driver has to \
                     be a failure, never a default.",
                    storage.driver()
                );
            }

            // The failure names the driver, so the fix is in the message rather
            // than in a search. (Not asserted for the blank spellings, where
            // `contains` would hold for anything.)
            if !name.trim().is_empty() {
                let message = Disks::new("uploads")
                    .with("uploads", CustomDisk::new(name))
                    .build()
                    .await
                    .err()
                    .expect("just asserted")
                    .message()
                    .to_string();
                assert!(message.contains(name), "`{name}` is missing from: {message}");
            }
        }
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn a_url_prefix_belongs_to_the_disk_that_declared_it() {
        let storage = Disks::new("uploads")
            .with("uploads", S3Disk::new("b").region("us-east-1"))
            .with("public", S3Disk::new("b").region("us-east-1").url("https://cdn.example.com"))
            .build()
            .await
            .unwrap();

        assert_eq!(storage.disk("uploads").unwrap().url("a.txt"), None);
        assert_eq!(
            storage.disk("public").unwrap().url("a.txt").as_deref(),
            Some("https://cdn.example.com/a.txt")
        );
    }
}
