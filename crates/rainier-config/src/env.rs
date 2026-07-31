//! `.env` loading — [`Env`].
//!
//! Two deliberate differences from a plain "read the file into the process
//! environment" helper:
//!
//! 1. **Real environment variables win.** A value already present in the
//!    process environment is never overwritten by the file, so a container
//!    orchestrator or CI secret always beats a checked-out `.env`. This is what
//!    `phpdotenv` (and twelve-factor deployment generally) expects.
//! 2. **The process environment is not mutated** unless you ask for it with
//!    [`Env::export_to_process`]. `std::env::set_var` is unsound in a
//!    multi-threaded program — another thread reading the environment
//!    concurrently is a data race — and a framework that spawns a runtime has
//!    no way to promise it is single-threaded at that moment. Reading through
//!    `Env` sidesteps the problem entirely.

use std::collections::HashMap;
use std::path::Path;

use rainier_support::{Error, Result};

/// Variables parsed from a `.env` file, layered under the real process
/// environment.
#[derive(Debug, Clone, Default)]
pub struct Env {
    vars: HashMap<String, String>,
    /// When set, [`get`](Env::get) never consults the process environment.
    isolated: bool,
}

impl Env {
    /// An empty set — every lookup falls through to the process environment.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a `.env` file.
    ///
    /// Fails if the file exists but cannot be read; see
    /// [`load_or_default`](Self::load_or_default) for the usual bootstrap case
    /// where a missing file is fine.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            Error::internal(format!("could not read {}: {e}", path.as_ref().display()))
        })?;
        Ok(Self::parse(&contents))
    }

    /// Parse a `.env` file, or return an empty set if it is not there.
    /// Production deployments usually have no `.env` at all.
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        Self::load(path).unwrap_or_default()
    }

    /// Parse `.env` syntax from a string.
    ///
    /// Supports `KEY=value`, `export KEY=value`, `#` comments (whole-line and
    /// trailing), single and double quoting, backslash escapes inside double
    /// quotes, and `${OTHER}` interpolation from variables defined earlier in
    /// the file or present in the process environment.
    pub fn parse(contents: &str) -> Self {
        let mut vars = HashMap::new();

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
            let Some((key, raw)) = line.split_once('=') else {
                continue; // not an assignment; ignore rather than fail the boot
            };

            let key = key.trim();
            if key.is_empty() {
                continue;
            }

            let value = interpolate(&unquote(raw.trim()), &vars);
            vars.insert(key.to_string(), value);
        }

        Self { vars, isolated: false }
    }

    /// Add or replace a variable in the file layer.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    /// The value of `key`: the process environment first, then the file.
    pub fn get(&self, key: &str) -> Option<String> {
        if self.isolated {
            return self.vars.get(key).cloned();
        }

        match std::env::var(key) {
            Ok(value) => Some(value),
            Err(_) => self.vars.get(key).cloned(),
        }
    }

    /// Read only what this `Env` holds, ignoring the process environment.
    ///
    /// The process normally wins — a real variable should beat a `.env` file —
    /// and that is exactly what stops a test stating its own premise:
    ///
    /// ```
    /// # use rainier_config::Env;
    /// // Whatever the shell exported, this says `memory`.
    /// let env = Env::parse("CACHE_DRIVER=memory").isolated();
    /// assert_eq!(env.get("CACHE_DRIVER").as_deref(), Some("memory"));
    /// ```
    ///
    /// The alternative — scrubbing the process environment around the
    /// assertion — is a data race against every other thread, which is why
    /// `std::env::remove_var` is `unsafe`. This needs no unsafety and no
    /// serialisation: it simply does not look.
    #[must_use = "this returns an isolated Env rather than isolating in place"]
    pub fn isolated(mut self) -> Self {
        self.isolated = true;
        self
    }

    /// An `Env` from pairs, reading nothing else.
    ///
    /// ```
    /// # use rainier_config::Env;
    /// let env = Env::from_map([("APP_ENV", "testing"), ("CACHE_DRIVER", "memory")]);
    /// assert_eq!(env.get("APP_ENV").as_deref(), Some("testing"));
    /// assert_eq!(env.get("PATH"), None, "the process is not consulted");
    /// ```
    pub fn from_map<K, V, I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut env = Self::default();
        for (key, value) in pairs {
            env.set(key, value);
        }
        env.isolated()
    }

    /// Whether this `Env` ignores the process environment.
    pub fn is_isolated(&self) -> bool {
        self.isolated
    }

    /// The value of `key`, or `default`.
    pub fn string(&self, key: &str, default: impl Into<String>) -> String {
        self.get(key).unwrap_or_else(|| default.into())
    }

    /// The value of `key`, or an error naming it. For settings with no sane
    /// default, where booting without one should fail loudly.
    pub fn require(&self, key: &str) -> Result<String> {
        self.get(key).ok_or_else(|| {
            Error::internal(format!("the `{key}` environment variable is required but not set"))
        })
    }

    /// `key` parsed as a bool. `true`/`1`/`yes`/`on` are true, `false`/`0`/
    /// `no`/`off`/empty are false; anything else falls back to `default`.
    pub fn bool(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" | "" => false,
                _ => default,
            },
            None => default,
        }
    }

    /// `key` parsed as an integer, or `default` if unset or unparsable.
    pub fn int(&self, key: &str, default: i64) -> i64 {
        self.get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
    }

    /// `key` parsed as a floating-point number, or `default`.
    ///
    /// For a ratio or a multiplier — a sample rate, a backoff factor. Anything
    /// counted wants [`int`](Self::int), because a count that arrives as
    /// `3.0000000000000004` is a bug waiting for somewhere to happen.
    pub fn float(&self, key: &str, default: f64) -> f64 {
        self.get(key)
            .and_then(|v| v.trim().parse::<f64>().ok())
            // Neither is a number anybody meant to configure, and both poison
            // every comparison downstream.
            .filter(|value| value.is_finite())
            .unwrap_or(default)
    }

    /// `key` parsed as a [closed-set setting](rainier_support::Setting).
    ///
    /// Unset gives the setting's own `Default`. Set to something outside the
    /// set is an **error**, naming the variable and listing what was expected:
    ///
    /// ```
    /// # use rainier_config::Env;
    /// # use rainier_support::setting_enum;
    /// setting_enum! {
    ///     pub enum CacheDriver: "cache driver" {
    ///         #[default]
    ///         Memory = "memory",
    ///         Redis = "redis",
    ///     }
    /// }
    ///
    /// let env = Env::parse("CACHE_DRIVER=redys");
    /// let err = env.setting::<CacheDriver>("CACHE_DRIVER").unwrap_err();
    ///
    /// assert!(err.message().contains("CACHE_DRIVER"), "{}", err.message());
    /// assert!(err.message().contains("`memory`, `redis`"), "{}", err.message());
    /// ```
    ///
    /// Note that this does not follow [`bool`](Self::bool) and
    /// [`int`](Self::int) in falling back. Those parse a value whose whole
    /// range is obvious from the type, where a bad one is nearly always a
    /// missing quote. A driver name selects *code*, and substituting different
    /// code than the deployment asked for is not a recovery — it is the bug,
    /// arriving later and somewhere else.
    pub fn setting<T: rainier_support::Setting + Default>(&self, key: &str) -> Result<T> {
        match self.get(key) {
            Some(raw) => {
                T::parse(&raw).map_err(|e| Error::internal(format!("`{key}`: {}", e.message())))
            }
            None => Ok(T::default()),
        }
    }

    /// `key` parsed as a setting, with `default` in place of the setting's own.
    ///
    /// For the case where one application wants a different default from the
    /// one the enum declares — a test harness pinning `sync`, say. Still an
    /// error on an unrecognised value.
    pub fn setting_or<T: rainier_support::Setting>(&self, key: &str, default: T) -> Result<T> {
        match self.get(key) {
            Some(raw) => {
                T::parse(&raw).map_err(|e| Error::internal(format!("`{key}`: {}", e.message())))
            }
            None => Ok(default),
        }
    }

    /// Copy every parsed variable into the process environment, skipping any
    /// that is already set.
    ///
    /// # Safety
    ///
    /// Only sound while no other thread is reading or writing the environment.
    /// Call it at the very top of `main`, before starting a runtime — or, far
    /// better, do not call it and read through [`get`](Self::get) instead.
    /// It exists for third-party libraries that read `std::env` directly.
    pub unsafe fn export_to_process(&self) {
        for (key, value) in &self.vars {
            if std::env::var(key).is_err() {
                // SAFETY: delegated to this function's own contract.
                unsafe { std::env::set_var(key, value) };
            }
        }
    }

    /// Every variable from the file layer. Does not include the process
    /// environment.
    pub fn file_vars(&self) -> &HashMap<String, String> {
        &self.vars
    }
}

/// Strip matching surrounding quotes, honouring backslash escapes inside
/// double quotes, and drop a trailing `#` comment from an unquoted value.
fn unquote(raw: &str) -> String {
    let bytes = raw.as_bytes();

    if bytes.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        // Single quotes are literal — no escapes, no interpolation stripping.
        return raw[1..raw.len() - 1].to_string();
    }

    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        }
        return out;
    }

    // Unquoted: a `#` starts a trailing comment.
    match raw.split_once(" #") {
        Some((value, _)) => value.trim_end().to_string(),
        None => raw.trim_end().to_string(),
    }
}

/// Expand `${NAME}` references against already-parsed variables, then the
/// process environment. Unknown names expand to empty, as in a shell.
fn interpolate(value: &str, known: &HashMap<String, String>) -> String {
    if !value.contains("${") {
        return value.to_string();
    }

    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                let resolved = known
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
                    .unwrap_or_default();
                out.push_str(&resolved);
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated `${` — emit it literally rather than eating the
                // rest of the value.
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_assignments() {
        let env = Env::parse("APP_NAME=Rainier\nAPP_DEBUG=true\n");
        assert_eq!(env.get("APP_NAME").as_deref(), Some("Rainier"));
        assert!(env.bool("APP_DEBUG", false));
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let env = Env::parse("# a comment\n\nKEY=value\n   # indented comment\n");
        assert_eq!(env.file_vars().len(), 1);
        assert_eq!(env.get("KEY").as_deref(), Some("value"));
    }

    #[test]
    fn honours_the_export_prefix() {
        let env = Env::parse("export DB_HOST=localhost");
        assert_eq!(env.get("DB_HOST").as_deref(), Some("localhost"));
    }

    #[test]
    fn strips_quotes_and_handles_escapes() {
        let env = Env::parse("SINGLE='raw $value'\nDOUBLE=\"line\\nbreak\"\nBARE=plain value\n");
        assert_eq!(env.get("SINGLE").as_deref(), Some("raw $value"));
        assert_eq!(env.get("DOUBLE").as_deref(), Some("line\nbreak"));
        assert_eq!(env.get("BARE").as_deref(), Some("plain value"));
    }

    #[test]
    fn drops_trailing_comments_on_unquoted_values() {
        let env = Env::parse("PORT=8080 # the http port");
        assert_eq!(env.get("PORT").as_deref(), Some("8080"));
        assert_eq!(env.int("PORT", 0), 8080);
    }

    #[test]
    fn a_hash_inside_quotes_is_not_a_comment() {
        let env = Env::parse("KEY=\"a # b\"");
        assert_eq!(env.get("KEY").as_deref(), Some("a # b"));
    }

    #[test]
    fn interpolates_earlier_variables() {
        let env = Env::parse("HOST=db.internal\nURL=mysql://${HOST}:3306/app\n");
        assert_eq!(env.get("URL").as_deref(), Some("mysql://db.internal:3306/app"));
    }

    #[test]
    fn unknown_interpolations_become_empty() {
        let env = Env::parse("URL=http://${NOPE_NOT_SET_ANYWHERE}/x");
        assert_eq!(env.get("URL").as_deref(), Some("http:///x"));
    }

    #[test]
    fn an_unterminated_interpolation_stays_literal() {
        let env = Env::parse("KEY=${broken");
        assert_eq!(env.get("KEY").as_deref(), Some("${broken"));
    }

    #[test]
    fn the_process_environment_wins_over_the_file() {
        // Chosen to be unlikely to collide with a real variable.
        let key = "RAINIER_TEST_ENV_PRECEDENCE";
        let mut env = Env::new();
        env.set(key, "from-file");
        assert_eq!(env.get(key).as_deref(), Some("from-file"));

        // SAFETY: single-threaded test, and the variable is unique to it.
        unsafe { std::env::set_var(key, "from-process") };
        assert_eq!(env.get(key).as_deref(), Some("from-process"));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn typed_readers_fall_back_to_defaults() {
        let env = Env::parse("N=notanumber");
        assert_eq!(env.int("N", 7), 7);
        assert_eq!(env.int("MISSING", 7), 7);
        assert!(env.bool("MISSING", true));
        assert_eq!(env.string("MISSING", "fallback"), "fallback");
    }

    #[test]
    fn require_names_the_missing_key() {
        let env = Env::new();
        let err = env.require("RAINIER_TEST_DEFINITELY_MISSING").unwrap_err();
        assert!(err.message().contains("RAINIER_TEST_DEFINITELY_MISSING"));
    }

    #[test]
    fn a_missing_file_is_not_fatal_for_load_or_default() {
        let env = Env::load_or_default("./definitely-not-here.env");
        assert!(env.file_vars().is_empty());
    }

    #[test]
    fn a_float_parses_and_falls_back() {
        let env = Env::parse(
            "RATIO=0.1
NONSENSE=banana",
        );

        assert_eq!(env.float("RATIO", 1.0), 0.1);
        assert_eq!(env.float("NONSENSE", 1.0), 1.0, "unparsable falls back");
        assert_eq!(env.float("ABSENT", 0.5), 0.5);
    }

    #[test]
    fn a_float_that_is_not_a_number_falls_back_rather_than_poisoning_a_comparison() {
        // `NaN < 1.0` and `NaN > 1.0` are both false, so a sample ratio of NaN
        // would make every downstream branch take its else arm.
        let env = Env::parse(
            "RATIO=NaN
HUGE=inf",
        );

        assert_eq!(env.float("RATIO", 1.0), 1.0);
        assert_eq!(env.float("HUGE", 1.0), 1.0);
    }

    #[test]
    fn the_process_wins_over_the_map_unless_isolated() {
        // The production rule: a real variable beats a `.env` file. `PATH` is
        // set in every environment this will ever run in.
        let env = Env::parse("PATH=from-the-file");
        assert_ne!(env.get("PATH").as_deref(), Some("from-the-file"));

        // And the testing rule: state your own premise.
        let isolated = Env::parse("PATH=from-the-file").isolated();
        assert_eq!(isolated.get("PATH").as_deref(), Some("from-the-file"));
    }

    #[test]
    fn an_isolated_env_sees_nothing_it_was_not_given() {
        let env = Env::from_map([("APP_ENV", "testing")]);

        assert_eq!(env.get("APP_ENV").as_deref(), Some("testing"));
        assert_eq!(env.get("PATH"), None);
        assert!(env.is_isolated());
    }

    #[test]
    fn isolation_survives_the_typed_readers() {
        // `bool`, `int`, `float` and `setting` all go through `get`, so one
        // flag covers the whole surface.
        let env = Env::from_map([("FLAG", "true"), ("COUNT", "7"), ("RATIO", "0.5")]);

        assert!(env.bool("FLAG", false));
        assert_eq!(env.int("COUNT", 0), 7);
        assert_eq!(env.float("RATIO", 0.0), 0.5);
        assert_eq!(env.string("ABSENT", "fallback"), "fallback");
    }
}
