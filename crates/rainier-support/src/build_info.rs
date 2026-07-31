//! What is actually running — [`BuildInfo`] and [`build_info!`](macro@crate::build_info).
//!
//! ```ignore
//! router.get("/health/version", || async { Response::json(&build_info!()) });
//! ```
//!
//! ```json
//! { "name": "identity", "version": "2.4.1", "commit": "9f3c2ab", "profile": "release" }
//! ```
//!
//! The first question of every incident is *which build is this?* — and the
//! usual answer is somebody reading a deploy pipeline backwards. A version
//! endpoint answers it in a second, and a log line at boot answers it for the
//! machine nobody can reach any more.
//!
//! # Where the values come from
//!
//! The macro expands **in your crate**, so the name and version are your
//! package's, not Rainier's. The commit and build time come from the
//! environment at compile time, so they are present when a build sets them and
//! absent when it does not:
//!
//! | Field | Read from |
//! |---|---|
//! | `commit` | `GIT_SHA`, `GITHUB_SHA`, or `VERGEN_GIT_SHA` |
//! | `built_at` | `BUILD_TIMESTAMP` or `SOURCE_DATE_EPOCH` |
//!
//! `GITHUB_SHA` is set by GitHub Actions already, so a CI build gets its
//! commit with nothing added. Anywhere else it is one line:
//!
//! ```dockerfile
//! ARG GIT_SHA
//! ENV GIT_SHA=$GIT_SHA
//! RUN cargo build --release
//! ```
//!
//! A local `cargo run` has neither, and reports `None` rather than guessing —
//! a commit inferred from a dirty working tree is worse than no commit.

use serde::Serialize;

/// The identity of a running binary.
///
/// Build it with [`build_info!`](macro@crate::build_info), which fills it in from the crate being
/// compiled.
///
/// Every field is `&'static str`, because every value is known when the binary
/// is — so this costs nothing to hold and nothing to clone. That is also why
/// it is `Serialize` and not `Deserialize`: a service reads its *own* build
/// info, and a client reading somebody else's wants its own owned type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    /// The package name.
    pub name: &'static str,
    /// The package version.
    pub version: &'static str,
    /// The commit this was built from, if the build was told.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<&'static str>,
    /// When it was built, if the build was told.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built_at: Option<&'static str>,
    /// `debug` or `release`.
    pub profile: &'static str,
}

impl BuildInfo {
    /// The commit, shortened to the first seven characters.
    ///
    /// What everybody actually pastes into a chat message.
    pub fn short_commit(&self) -> Option<&str> {
        self.commit.map(|commit| &commit[..commit.len().min(7)])
    }

    /// Whether this is a debug build.
    ///
    /// Worth asserting at boot in production: a debug binary is several times
    /// slower, and the difference is easy to deploy by accident.
    pub fn is_debug(&self) -> bool {
        self.profile == "debug"
    }

    /// One line, for a boot log.
    ///
    /// ```text
    /// identity 2.4.1 (9f3c2ab, release)
    /// ```
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(commit) = self.short_commit() {
            parts.push(commit.to_string());
        }
        parts.push(self.profile.to_string());

        format!("{} {} ({})", self.name, self.version, parts.join(", "))
    }
}

impl std::fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

/// The [`BuildInfo`] of the crate this is written in.
///
/// ```
/// use rainier_support::build_info;
///
/// let info = build_info!();
///
/// assert_eq!(info.name, "rainier-support");
/// assert!(!info.version.is_empty());
/// ```
#[macro_export]
macro_rules! build_info {
    () => {
        $crate::build_info::BuildInfo {
            name: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            commit: option_env!("GIT_SHA")
                .or(option_env!("GITHUB_SHA"))
                .or(option_env!("VERGEN_GIT_SHA")),
            built_at: option_env!("BUILD_TIMESTAMP").or(option_env!("SOURCE_DATE_EPOCH")),
            profile: if cfg!(debug_assertions) { "debug" } else { "release" },
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> BuildInfo {
        BuildInfo {
            name: "identity",
            version: "2.4.1",
            commit: Some("9f3c2ab1d4e5f60718293a4b5c6d7e8f90123456"),
            built_at: Some("2026-07-25T09:14:00Z"),
            profile: "release",
        }
    }

    #[test]
    fn the_macro_reads_the_crate_it_is_written_in() {
        let info = build_info!();

        assert_eq!(info.name, "rainier-support");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        // Tests are a debug build.
        assert_eq!(info.profile, "debug");
        assert!(info.is_debug());
    }

    #[test]
    fn a_commit_is_shortened_to_what_people_paste() {
        assert_eq!(info().short_commit(), Some("9f3c2ab"));
    }

    #[test]
    fn a_short_commit_is_not_truncated_past_its_end() {
        // A tag or a branch name could be shorter than seven characters, and
        // slicing past the end of a string is a panic in a health endpoint.
        let mut info = info();
        info.commit = Some("ab1");

        assert_eq!(info.short_commit(), Some("ab1"));
    }

    #[test]
    fn no_commit_is_none_rather_than_unknown() {
        let mut info = info();
        info.commit = None;

        assert_eq!(info.short_commit(), None);
        assert_eq!(info.summary(), "identity 2.4.1 (release)");
    }

    #[test]
    fn the_summary_is_one_line_worth_reading() {
        assert_eq!(info().summary(), "identity 2.4.1 (9f3c2ab, release)");
        assert_eq!(info().to_string(), info().summary());
    }

    #[test]
    fn absent_fields_are_left_out_of_the_json_entirely() {
        // `"commit": null` on a version endpoint reads as "there is no
        // commit"; omitting it reads as "this build was not told", which is
        // what actually happened.
        let mut info = info();
        info.commit = None;
        info.built_at = None;

        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "name": "identity",
                "version": "2.4.1",
                "profile": "release",
            })
        );
    }

    #[test]
    fn the_whole_thing_serialises() {
        assert_eq!(
            serde_json::to_value(info()).unwrap(),
            serde_json::json!({
                "name": "identity",
                "version": "2.4.1",
                "commit": "9f3c2ab1d4e5f60718293a4b5c6d7e8f90123456",
                "built_at": "2026-07-25T09:14:00Z",
                "profile": "release",
            })
        );
    }
}
