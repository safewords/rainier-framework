//! The cargo feature set a deployment actually needs — [`compute`].
//!
//! # Why a library, and why here
//!
//! Cargo cannot enable features from code. They are resolved **before**
//! anything compiles, they are additive-only, and a build script cannot add
//! one — so "the compiler notices `MAIL_DRIVER=smtp` and turns `mail-smtp`
//! on" is not a thing cargo can do. Nor does dead-code elimination size the
//! binary on its own: a well-built application's driver `match`es are
//! deliberately exhaustive, so every compiled driver is *referenced* and the
//! linker keeps it. Features are the sizing mechanism, and something has to
//! compute them.
//!
//! The knowledge that computation needs — which selection wants which
//! framework feature — is knowledge about **Rainier**, not about any one
//! application. It lives here so it cannot drift: the tests pin the mapping
//! against the real driver enums, and a driver added to the framework breaks
//! this crate's build until the table learns it, in the same commit.
//!
//! # What it reads
//!
//! Two honest sources:
//!
//! 1. **The deployment's environment**, under the conventional variable names
//!    (`CACHE_DRIVER`, `QUEUE_DRIVER`, `MAIL_DRIVER`, `HASH_DRIVER`,
//!    `STORAGE_DRIVER`, `KAFKA_TLS`) — every runtime driver selection.
//! 2. **The source tree**, for the compile-time choices no variable selects:
//!    code either reaches for `Jwt` or the `Http` facade's real transport, or
//!    it does not.
//!
//! # What it answers, and what it cannot
//!
//! The report says what a **build must carry** for those selections to be
//! constructible. Whether your bootstrap *wires* an arm for a given driver is
//! your `match` statement's business — the boot error that names a missing
//! feature, and the compiler pointing at a non-exhaustive `match`, are the
//! authorities there.
//!
//! Install the [`cargo-rainier`] subcommand and run `cargo rainier
//! features` anywhere — or consume this library from an `xtask` if your
//! workspace prefers no globally-installed tools.
//!
//! [`cargo-rainier`]: https://github.com/safewords/rainier-framework/tree/main/crates/cargo-rainier

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

use std::collections::BTreeSet;
use std::path::Path;

/// What was computed, and why.
#[derive(Debug, Default)]
pub struct Report {
    /// The framework features the selections need, sorted.
    pub features: BTreeSet<String>,
    /// One line per feature: which selection asked for it.
    pub reasons: Vec<String>,
    /// Selections that need something `rainier-framework` does not forward —
    /// a per-crate feature you would have to enable yourself.
    pub unforwarded: Vec<String>,
}

impl Report {
    /// The `cargo build` invocation this report implies.
    pub fn build_command(&self) -> String {
        if self.features.is_empty() {
            "cargo build --release --no-default-features".to_string()
        } else {
            let list: Vec<&str> = self.features.iter().map(String::as_str).collect();
            format!(
                "cargo build --release --no-default-features --features \"{}\"",
                list.join(",")
            )
        }
    }

    /// The `--features` value, comma-separated, empty when nothing is needed.
    pub fn feature_list(&self) -> String {
        let list: Vec<&str> = self.features.iter().map(String::as_str).collect();
        list.join(",")
    }
}

/// Compute the feature set for `env` and `sources`.
///
/// `env` is ordered `KEY=VALUE` pairs — later entries win, like a shell.
/// `sources` is the application's Rust source, concatenated
/// ([`read_sources`] does the walking).
pub fn compute(env: &[(String, String)], sources: &str) -> Report {
    let mut report = Report::default();

    let get = |name: &str| -> Option<&str> {
        env.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    };

    fn want(report: &mut Report, feature: &str, why: String) {
        if report.features.insert(feature.to_string()) {
            report.reasons.push(format!("{feature:<14} {why}"));
        }
    }

    // --- runtime selections, from the environment ---------------------------
    //
    // Each table below is pinned against the real driver enum in the tests,
    // so a variant the framework grows and this table has not learned breaks
    // the build here rather than shipping a list that quietly omits it.

    match get("CACHE_DRIVER") {
        Some("redis") => want(&mut report, "redis", "CACHE_DRIVER=redis".into()),
        Some("redis-cluster") => {
            want(&mut report, "redis-cluster", "CACHE_DRIVER=redis-cluster".into())
        }
        Some("memcached") => want(&mut report, "memcached", "CACHE_DRIVER=memcached".into()),
        Some("kv") => want(&mut report, "cloudflare-kv", "CACHE_DRIVER=kv".into()),
        Some("dynamodb") => report.unforwarded.push(
            "CACHE_DRIVER=dynamodb: rainier-framework forwards no feature for it — enable \
             `rainier-cache/dynamodb` yourself"
                .into(),
        ),
        _ => {}
    }

    match get("QUEUE_DRIVER") {
        Some("redis") => want(&mut report, "redis", "QUEUE_DRIVER=redis".into()),
        Some("sqs") => want(&mut report, "sqs", "QUEUE_DRIVER=sqs".into()),
        Some("kafka") => {
            want(&mut report, "kafka", "QUEUE_DRIVER=kafka".into());
            if get("KAFKA_TLS").is_some_and(|tls| tls == "true" || tls == "1") {
                want(&mut report, "kafka-tls", "KAFKA_TLS=true".into());
            }
        }
        _ => {}
    }

    match get("MAIL_DRIVER") {
        Some(sender @ ("smtp" | "ses" | "postmark" | "mailgun" | "sendgrid" | "resend")) => {
            let feature = format!("mail-{sender}");
            want(&mut report, &feature, format!("MAIL_DRIVER={sender}"));
        }
        _ => {}
    }

    if get("HASH_DRIVER") == Some("bcrypt") {
        want(&mut report, "bcrypt", "HASH_DRIVER=bcrypt".into());
    }

    if get("STORAGE_DRIVER") == Some("s3") {
        want(&mut report, "s3", "STORAGE_DRIVER=s3".into());
    }

    // --- compile-time choices, from the source -------------------------------
    //
    // Substring matches are crude and cheap, and a false positive costs one
    // feature rather than a broken build.

    if sources.contains("BcryptVerifier") || sources.contains("BcryptHasher") {
        want(&mut report, "bcrypt", "src/ names a bcrypt type".into());
    }

    if sources.contains("crypt::Jwt")
        || sources.contains("JwtKeyRing")
        || sources.contains("JwtKey::")
    {
        want(&mut report, "jwt", "src/ names the JWT surface".into());
    }

    if sources.contains("Http::") || sources.contains("ReqwestTransport") {
        want(&mut report, "http-client", "src/ uses the Http facade".into());
    }

    report
}

/// The environment file a sizing run should read — strictly.
///
/// An explicit path must exist; with none given, `.env` must. There is **no
/// fallback to `.env.example`**: sizing a build from the example's defaults
/// would produce a binary shaped like the documentation rather than the
/// deployment, silently. Previewing against the example is one flag away —
/// `--env .env.example` — and saying so is this error's job.
pub fn resolve_env(explicit: Option<std::path::PathBuf>) -> Result<std::path::PathBuf, String> {
    match explicit {
        Some(path) => {
            if path.exists() {
                Ok(path)
            } else {
                Err(format!("{} does not exist", path.display()))
            }
        }
        None => {
            let dot_env = std::path::PathBuf::from(".env");
            if dot_env.exists() {
                Ok(dot_env)
            } else {
                Err("sizing a build needs the deployment's environment: pass --env <file> or \
                     create .env (preview against the defaults with --env .env.example)"
                    .to_string())
            }
        }
    }
}

/// Parse `KEY=VALUE` lines — later lines win, comments and blanks skipped,
/// single and double quotes stripped.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches('"').trim_matches('\'');
            Some((key.trim().to_string(), value.to_string()))
        })
        .collect()
}

/// Every `.rs` under `root`, concatenated.
///
/// Comments are not stripped — a commented-out `Http::` costs one feature,
/// which is the cheap direction to be wrong in.
pub fn read_sources(root: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rainier_support::Setting;

    fn env(text: &str) -> Vec<(String, String)> {
        parse_env(text)
    }

    fn computed(text: &str) -> Report {
        compute(&env(text), "")
    }

    // --- behaviour -----------------------------------------------------------

    #[test]
    fn a_fresh_clone_needs_nothing() {
        let report = computed("CACHE_DRIVER=memory\nQUEUE_DRIVER=sync\nMAIL_DRIVER=log");

        assert!(report.features.is_empty(), "{:?}", report.features);
        assert!(report.unforwarded.is_empty());
        assert_eq!(report.build_command(), "cargo build --release --no-default-features");
    }

    #[test]
    fn every_selection_maps_and_the_reasons_say_why() {
        let report = computed(
            "CACHE_DRIVER=redis\nQUEUE_DRIVER=sqs\nMAIL_DRIVER=smtp\nHASH_DRIVER=bcrypt\nSTORAGE_DRIVER=s3",
        );

        let expected: BTreeSet<String> =
            ["redis", "sqs", "mail-smtp", "bcrypt", "s3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(report.features, expected);
        assert_eq!(report.reasons.len(), report.features.len());
    }

    #[test]
    fn kafka_brings_tls_only_when_the_cluster_asks() {
        assert!(!computed("QUEUE_DRIVER=kafka").features.contains("kafka-tls"));
        assert!(computed("QUEUE_DRIVER=kafka\nKAFKA_TLS=true").features.contains("kafka-tls"));
    }

    #[test]
    fn source_usage_is_a_reason_too() {
        let report = compute(&[], "let jwt = crypt::Jwt::new(ring);\nHttp::get(url)");

        assert!(report.features.contains("jwt"));
        assert!(report.features.contains("http-client"));
    }

    #[test]
    fn an_unforwarded_selection_is_named_rather_than_silently_dropped() {
        let report = computed("CACHE_DRIVER=dynamodb");

        assert!(!report.unforwarded.is_empty());
        assert!(report.features.is_empty());
    }

    #[test]
    fn later_lines_win_like_a_shell() {
        assert!(computed("MAIL_DRIVER=smtp\nMAIL_DRIVER=log").features.is_empty());
    }

    // --- the drift guards ----------------------------------------------------
    //
    // The whole reason this crate lives in the framework: every variant of
    // every driver enum must be *accounted for* here. A new driver fails one
    // of these until the mapping learns it, in the same commit that adds it.

    #[test]
    fn every_cache_driver_is_accounted_for() {
        use rainier_cache::CacheDriver;

        for driver in CacheDriver::ALL {
            let report = computed(&format!("CACHE_DRIVER={}", driver.as_str()));
            let accounted = !report.features.is_empty()
                || !report.unforwarded.is_empty()
                || matches!(driver, CacheDriver::Memory);

            assert!(accounted, "CACHE_DRIVER={} maps to nothing — teach `compute`", driver);
        }
    }

    #[test]
    fn every_queue_driver_is_accounted_for() {
        use rainier_queue::QueueDriver;

        for driver in QueueDriver::ALL {
            let report = computed(&format!("QUEUE_DRIVER={}", driver.as_str()));
            let needs_nothing = matches!(
                driver,
                QueueDriver::Sync | QueueDriver::Memory | QueueDriver::Database
            );

            assert!(
                needs_nothing || !report.features.is_empty() || !report.unforwarded.is_empty(),
                "QUEUE_DRIVER={} maps to nothing — teach `compute`",
                driver
            );
        }
    }

    #[test]
    fn every_mail_driver_is_accounted_for() {
        use rainier_mail::MailDriver;

        for driver in MailDriver::ALL {
            let report = computed(&format!("MAIL_DRIVER={}", driver.as_str()));
            // The three that deliver nothing need nothing.
            let needs_nothing =
                matches!(driver, MailDriver::Log | MailDriver::File | MailDriver::Memory);

            assert!(
                needs_nothing || !report.features.is_empty(),
                "MAIL_DRIVER={} maps to nothing — teach `compute`",
                driver
            );
        }
    }

    #[test]
    fn every_filesystem_driver_is_accounted_for() {
        use rainier_filesystem::FilesystemDriver;

        for driver in FilesystemDriver::ALL {
            let report = computed(&format!("STORAGE_DRIVER={}", driver.as_str()));
            let needs_nothing =
                matches!(driver, FilesystemDriver::Local | FilesystemDriver::Memory);

            assert!(
                needs_nothing || !report.features.is_empty(),
                "STORAGE_DRIVER={} maps to nothing — teach `compute`",
                driver
            );
        }
    }

    #[test]
    fn every_hash_driver_is_accounted_for() {
        use rainier_crypt::HashDriver;

        for driver in HashDriver::ALL {
            let report = computed(&format!("HASH_DRIVER={}", driver.as_str()));
            let needs_nothing = matches!(driver, HashDriver::Argon2id);

            assert!(
                needs_nothing || !report.features.is_empty(),
                "HASH_DRIVER={} maps to nothing — teach `compute`",
                driver
            );
        }
    }

    // --- parsing -------------------------------------------------------------

    #[test]
    fn a_missing_environment_is_an_error_and_the_error_teaches_the_preview() {
        // No fallback to `.env.example`: sizing from the example's defaults
        // would shape the binary like the documentation, silently.
        let err = resolve_env(Some("no-such-file.env".into())).unwrap_err();
        assert!(err.contains("no-such-file.env"), "{err}");

        // `None` in a directory with no `.env` — this test runs in the crate
        // root, which has none.
        let err = resolve_env(None).unwrap_err();
        assert!(err.contains(".env.example"), "the fix should be in the error: {err}");
    }

    #[test]
    fn quotes_and_comments_are_not_values() {
        let parsed = env("# comment\nMAIL_DRIVER=\"smtp\"\n\nEMPTY=");

        assert!(parsed.contains(&("MAIL_DRIVER".to_string(), "smtp".to_string())));
        assert!(parsed.contains(&("EMPTY".to_string(), String::new())));
    }
}
