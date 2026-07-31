//! Which disk to build — [`FilesystemDriver`].

use rainier_support::setting_enum;

setting_enum! {
    /// Which [`Filesystem`](crate::Filesystem) a named disk uses.
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

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn only_object_storage_is_shared_and_durable() {
        assert!(FilesystemDriver::S3.is_shared());
        assert!(FilesystemDriver::S3.is_durable());

        for driver in FilesystemDriver::ALL.iter().filter(|d| **d != FilesystemDriver::S3) {
            assert!(!driver.is_shared(), "{driver} is per-machine");
            assert!(!driver.is_durable(), "{driver} does not outlive its machine");
        }
    }
}
