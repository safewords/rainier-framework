//! Disks as configuration — [`Disks`], [`DiskConfig`], [`S3Disk`].
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
//! # #[tokio::main] async fn main() -> rainier_support::Result<()> {
//! let disks = Disks::new("uploads")
//!     .with("uploads", DiskConfig::local("storage/app"))
//!     .with("archive", S3Disk::new("archive-bucket").region("us-east-1"));
//!
//! let storage = disks.build().await?;
//!
//! assert_eq!(storage.driver(), "local");
//! assert!(storage.disk("archive").is_some());
//! # Ok(()) }
//! ```
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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rainier_support::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::driver::FilesystemDriver;
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
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "RawDisk", into = "RawDisk")]
pub enum DiskConfig {
    /// One directory on this machine.
    Local(LocalDisk),

    /// In memory, for tests.
    Memory,

    /// A bucket on S3 or anything that speaks its API.
    S3(S3Disk),
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

    /// Which driver this declares.
    pub fn driver(&self) -> FilesystemDriver {
        match self {
            Self::Local(_) => FilesystemDriver::Local,
            Self::Memory => FilesystemDriver::Memory,
            Self::S3(_) => FilesystemDriver::S3,
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

            #[cfg(feature = "s3")]
            Self::S3(disk) => Ok(Arc::new(disk.build().await?)),

            // Loud, and naming the fix. Falling back to a local directory would
            // "work": uploads would land on a container's disk, be served back
            // for the life of that container, and vanish on the next deploy.
            #[cfg(not(feature = "s3"))]
            Self::S3(disk) => Err(Error::internal(format!(
                "this disk uses the `s3` driver for bucket `{}`, but rainier-filesystem was \
                 built without the `s3` feature",
                disk.bucket()
            ))),
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

impl std::fmt::Debug for DiskConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local(disk) => std::fmt::Debug::fmt(disk, f),
            Self::Memory => f.write_str("Memory"),
            Self::S3(disk) => std::fmt::Debug::fmt(disk, f),
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

// --- the wire form -----------------------------------------------------------

/// A disk as it is written down, before it is known to make sense.
///
/// The flat shape a configuration file wants, which [`DiskConfig`] is the
/// checked form of. Everything but `driver` is optional here so the *driver*
/// gets to say which settings apply, and so a misfiled one can be named in the
/// error rather than silently dropped.
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

impl From<DiskConfig> for RawDisk {
    fn from(disk: DiskConfig) -> Self {
        let blank = |driver| Self {
            driver,
            root: None,
            bucket: None,
            region: None,
            endpoint: None,
            url: None,
            path_style: None,
            key: None,
            secret: None,
        };

        match disk {
            DiskConfig::Local(disk) => {
                Self { root: Some(disk.root), url: disk.url, ..blank(FilesystemDriver::Local) }
            }
            DiskConfig::Memory => blank(FilesystemDriver::Memory),
            DiskConfig::S3(disk) => {
                let (key, secret) = match disk.credentials {
                    S3Credentials::Chain => (None, None),
                    S3Credentials::Static { access_key_id, secret_access_key } => {
                        (Some(access_key_id), Some(secret_access_key))
                    }
                };
                Self {
                    bucket: Some(disk.bucket),
                    region: disk.region,
                    endpoint: disk.endpoint,
                    url: disk.url,
                    // Written back only when it was the disk's own doing: an
                    // endpoint implies it, and re-emitting the implication as a
                    // literal would make a round trip say more than the
                    // original did.
                    path_style: disk.path_style.then_some(true),
                    key,
                    secret,
                    ..blank(FilesystemDriver::S3)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
