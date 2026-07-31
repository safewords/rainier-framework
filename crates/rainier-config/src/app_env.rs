//! Which deployment this is — [`AppEnv`].

use rainier_support::setting_enum;

setting_enum! {
    /// `APP_ENV`: which deployment the process is running as.
    ///
    /// ```
    /// use rainier_config::AppEnv;
    /// use rainier_support::Setting;
    ///
    /// assert!(AppEnv::parse("local").unwrap().is_developing());
    /// assert!(!AppEnv::Production.is_developing());
    /// ```
    ///
    /// Note the default. A process with no `APP_ENV` is treated as
    /// **production**, which is the conservative direction: the failure mode is
    /// a developer seeing terse error pages, not a production deployment
    /// printing stack traces to strangers.
    pub enum AppEnv: "application environment" {
        /// A real deployment serving real users.
        #[default]
        Production = "production",

        /// A production-shaped deployment that is not serving real users.
        Staging = "staging",

        /// A developer's machine.
        Local = "local",

        /// A test run.
        Testing = "testing",
    }
}

impl AppEnv {
    /// Whether this is somebody's working copy or a test run.
    ///
    /// The gate for anything that trades safety for feedback: re-reading
    /// templates on every render, verbose error pages, seeded fixtures.
    pub fn is_developing(&self) -> bool {
        matches!(self, Self::Local | Self::Testing)
    }

    /// Whether real users are on the other end.
    ///
    /// `false` for [`Staging`](Self::Staging) — which is what makes it useful:
    /// staging is production-shaped, and the one thing it must not do is mail
    /// the real customers in the copied database.
    pub fn is_serving_users(&self) -> bool {
        matches!(self, Self::Production)
    }

    /// Whether a failure may show its internals to the client.
    ///
    /// Not the same as [`is_developing`](Self::is_developing) only by accident;
    /// it is spelled separately because `APP_DEBUG` can override it, and the
    /// two questions drift apart the moment someone debugs staging.
    pub fn may_leak_details(&self) -> bool {
        self.is_developing()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    #[test]
    fn an_unset_environment_is_production() {
        // The safe direction to be wrong in.
        assert_eq!(AppEnv::default(), AppEnv::Production);
        assert!(!AppEnv::default().is_developing());
    }

    #[test]
    fn staging_is_production_shaped_but_has_no_real_users() {
        assert!(!AppEnv::Staging.is_developing());
        assert!(!AppEnv::Staging.is_serving_users());
        assert!(AppEnv::Production.is_serving_users());
    }

    #[test]
    fn only_the_development_environments_may_leak_details() {
        for env in AppEnv::ALL {
            assert_eq!(env.may_leak_details(), env.is_developing(), "{env}");
        }
    }
}
