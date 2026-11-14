//! Lookout's configuration: what to watch, where, how much, and who may look.
//!
//! Built in code with the builder methods, or read from the environment with
//! [`LookoutConfig::from_env`]. The environment sets the scalars; scope and
//! per-watcher rules are code, because they are about the application's own
//! routes and guards.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::entry::{Batch, EntryType};

/// Where batches are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    /// In this process only. Development and tests; per pod on Kubernetes.
    Memory,
    /// Lookout's own tables, on the named connection (the default when
    /// `None`).
    Database {
        /// The database connection; `None` for the application's default.
        connection: Option<String>,
    },
    /// A ring buffer in the application's cache, on the named store.
    Cache {
        /// The cache store; `None` for the application's default.
        store: Option<String>,
        /// How many batches to keep.
        capacity: usize,
    },
}

/// A set of glob patterns (`api.*`, `/assets/*`) to include and exclude.
///
/// Empty `include` means "everything"; `exclude` always wins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patterns {
    /// Match any of these. Empty matches everything.
    pub include: Vec<String>,
    /// Never match these.
    pub exclude: Vec<String>,
}

impl Patterns {
    /// Whether `value` passes. `*` matches any run of characters.
    pub fn allows(&self, value: &str) -> bool {
        if self.exclude.iter().any(|p| glob(p, value)) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|p| glob(p, value))
    }

    /// Whether nothing is configured.
    pub fn is_unrestricted(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

/// `*` matches any run of characters (including none); everything else is
/// literal.
pub fn glob(pattern: &str, value: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut rest = value;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(at) = rest.find(part) {
            rest = &rest[at + part.len()..];
        } else {
            return false;
        }
    }
    true
}

/// Which units of work Lookout records at all.
///
/// Evaluated in two stages. At the start of a request the **route** (name,
/// pattern, path) and middleware **group** decide whether to buffer anything
/// — the cheap rejection. When the batch closes, the **guard** and **user**,
/// which are only known once authentication has run, decide whether to keep
/// it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Route names (`api.*`). A request whose route has no name is matched
    /// against its pattern instead.
    pub routes: Patterns,
    /// Raw request paths (`/assets/*`) — for requests that matched no route.
    pub paths: Patterns,
    /// Middleware groups (`web`, `api`).
    pub groups: Patterns,
    /// Authentication guards (`session`, `token`). Unauthenticated requests
    /// pass unless `include` is non-empty.
    pub guards: Patterns,
    /// Only these users, when non-empty.
    pub users: BTreeSet<String>,
    /// Batches carrying any of these tags are always kept, whatever the
    /// sampling — "watch user 42 in production".
    pub monitored_tags: BTreeSet<String>,
    /// Record queued jobs.
    pub jobs: bool,
    /// Record console commands.
    pub commands: bool,
    /// Record scheduled tasks.
    pub scheduled_tasks: bool,
}

/// Per-watcher settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatcherConfig {
    /// Whether this watcher records anything.
    pub enabled: bool,
    /// The fraction of entries kept, `0.0..=1.0`. Exceptions ignore it.
    pub sample_rate: f64,
    /// Entries slower than this are tagged `slow` and get a derived
    /// [`EntryType::Slow`] entry.
    pub slow_ms: Option<u64>,
    /// Per-pattern overrides of `slow_ms`, keyed by a glob over the entry's
    /// family key (route name, job name, …).
    pub slow_overrides: BTreeMap<String, u64>,
    /// Entries whose summary matches any of these globs are dropped.
    pub ignore: Vec<String>,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_rate: 1.0,
            slow_ms: None,
            slow_overrides: BTreeMap::new(),
            ignore: Vec::new(),
        }
    }
}

/// N+1 detection: a run of at least `min_repeats` queries with the same
/// sanitised SQL in one batch, taking at least `min_total_ms` together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NPlusOne {
    /// Whether to detect at all.
    pub enabled: bool,
    /// How many repeats make a run.
    pub min_repeats: usize,
    /// How long the run must take in total.
    pub min_total_ms: u64,
}

impl Default for NPlusOne {
    fn default() -> Self {
        Self { enabled: true, min_repeats: 5, min_total_ms: 50 }
    }
}

/// What may be captured beyond the defaults. Everything is off: the defaults
/// carry what the framework's telemetry spans carry, which deliberately leave
/// out values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    /// Request headers (redacted by name).
    pub request_headers: bool,
    /// The request body — only for batches with an exception.
    pub request_body_on_exception: bool,
    /// Cache keys.
    pub cache_keys: bool,
    /// Mail subjects and recipients.
    pub mail: bool,
    /// The authenticated user's id on the batch. Needed for user scoping and
    /// the "top users" view.
    pub user_id: bool,
}

/// Names that are always redacted, plus the application's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redaction {
    /// Header names, case-insensitive.
    pub headers: BTreeSet<String>,
    /// Body and payload field names, case-insensitive.
    pub fields: BTreeSet<String>,
    /// Longest value kept, in bytes; longer ones are truncated.
    pub max_value_len: usize,
}

impl Default for Redaction {
    fn default() -> Self {
        let headers = [
            "authorization",
            "cookie",
            "set-cookie",
            "proxy-authorization",
            "x-xsrf-token",
            "x-csrf-token",
            "x-api-key",
        ];
        let fields = [
            "password",
            "password_confirmation",
            "_token",
            "token",
            "secret",
            "api_key",
            "card_number",
            "cvc",
        ];
        Self {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            fields: fields.iter().map(|s| s.to_string()).collect(),
            max_value_len: 2048,
        }
    }
}

/// How the dashboard is served and who may see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dashboard {
    /// Whether to mount it at all.
    pub enabled: bool,
    /// Where: `/_lookout` by default.
    pub path: String,
    /// The guard that authenticates viewers. `None` for the application's
    /// default guard.
    pub guard: Option<String>,
    /// The ability a viewer must have. Outside the local environment the
    /// dashboard answers 404 to anyone without it.
    pub ability: String,
    /// Whether the local environment skips the check.
    pub open_locally: bool,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/_lookout".to_string(),
            guard: None,
            ability: "viewLookout".to_string(),
            open_locally: true,
        }
    }
}

/// How long things are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// Raw batches, in hours.
    pub batches_hours: u64,
    /// Aggregates, in days.
    pub aggregates_days: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { batches_hours: 24, aggregates_days: 30 }
    }
}

/// How recorded batches reach the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ingest {
    /// How many closed batches may wait for the writer. When full, new ones
    /// are dropped and counted — a request is never held up.
    pub channel_capacity: usize,
    /// The writer flushes at least this often.
    pub flush_interval_ms: u64,
    /// ...or once this many batches are waiting.
    pub flush_batch_size: usize,
}

impl Default for Ingest {
    fn default() -> Self {
        Self { channel_capacity: 10_000, flush_interval_ms: 1000, flush_batch_size: 500 }
    }
}

/// A rule run when a batch closes: keep it or drop it.
pub type KeepRule = std::sync::Arc<dyn Fn(&Batch) -> bool + Send + Sync>;

/// Everything about Lookout.
#[derive(Clone, Serialize, Deserialize)]
pub struct LookoutConfig {
    /// The master switch. Off, Lookout records nothing and costs nothing.
    pub enabled: bool,
    /// Where batches go.
    pub store: StoreKind,
    /// The fraction of requests recorded, `0.0..=1.0`.
    pub request_sample_rate: f64,
    /// The fraction of jobs recorded.
    pub job_sample_rate: f64,
    /// The fraction of commands and scheduled tasks recorded.
    pub command_sample_rate: f64,
    /// Which work is recorded.
    pub scope: Scope,
    /// Per-watcher settings. A type missing here uses [`WatcherConfig::default`].
    pub watchers: BTreeMap<EntryType, WatcherConfig>,
    /// N+1 detection.
    pub n_plus_one: NPlusOne,
    /// The lowest log level recorded: `error`, `warn`, `info`, `debug`.
    pub log_level: String,
    /// Opt-in capture.
    pub capture: Capture,
    /// Redaction.
    pub redaction: Redaction,
    /// The dashboard.
    pub dashboard: Dashboard,
    /// Retention.
    pub retention: Retention,
    /// Ingestion.
    pub ingest: Ingest,
    /// Keep only batches that pass. `None` keeps a batch when the
    /// environment is local, or when it has an exception, something slow, a
    /// failed job, or a monitored tag.
    #[serde(skip)]
    pub keep: Option<KeepRule>,
    /// Whether this is the local environment (`APP_ENV=local`). The default
    /// keep rule keeps every batch there; elsewhere only the interesting ones.
    #[serde(default)]
    pub local: bool,
}

impl std::fmt::Debug for LookoutConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LookoutConfig")
            .field("enabled", &self.enabled)
            .field("store", &self.store)
            .field("request_sample_rate", &self.request_sample_rate)
            .field("job_sample_rate", &self.job_sample_rate)
            .field("command_sample_rate", &self.command_sample_rate)
            .field("scope", &self.scope)
            .field("watchers", &self.watchers)
            .field("n_plus_one", &self.n_plus_one)
            .field("log_level", &self.log_level)
            .field("capture", &self.capture)
            .field("redaction", &self.redaction)
            .field("dashboard", &self.dashboard)
            .field("retention", &self.retention)
            .field("ingest", &self.ingest)
            .field("keep", &self.keep.is_some())
            .field("local", &self.local)
            .finish()
    }
}

impl Default for LookoutConfig {
    fn default() -> Self {
        let mut watchers = BTreeMap::new();
        watchers.insert(
            EntryType::Query,
            WatcherConfig { slow_ms: Some(100), ..WatcherConfig::default() },
        );
        watchers.insert(
            EntryType::Request,
            WatcherConfig { slow_ms: Some(1000), ..WatcherConfig::default() },
        );
        watchers.insert(
            EntryType::Job,
            WatcherConfig { slow_ms: Some(1000), ..WatcherConfig::default() },
        );
        watchers.insert(
            EntryType::HttpClient,
            WatcherConfig { slow_ms: Some(1000), ..WatcherConfig::default() },
        );
        Self {
            enabled: false,
            store: StoreKind::Memory,
            request_sample_rate: 1.0,
            job_sample_rate: 1.0,
            command_sample_rate: 1.0,
            scope: Scope { jobs: true, commands: true, scheduled_tasks: true, ..Scope::default() },
            watchers,
            n_plus_one: NPlusOne::default(),
            log_level: "error".to_string(),
            capture: Capture::default(),
            redaction: Redaction::default(),
            dashboard: Dashboard::default(),
            retention: Retention::default(),
            ingest: Ingest::default(),
            keep: None,
            local: false,
        }
    }
}

impl LookoutConfig {
    /// The settings for one watcher.
    pub fn watcher(&self, entry_type: EntryType) -> WatcherConfig {
        self.watchers.get(&entry_type).cloned().unwrap_or_default()
    }

    /// Turn Lookout on.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Where batches go.
    pub fn store(mut self, store: StoreKind) -> Self {
        self.store = store;
        self
    }

    /// Set the scope.
    pub fn scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }

    /// Adjust one watcher.
    pub fn watch(
        mut self,
        entry_type: EntryType,
        configure: impl FnOnce(WatcherConfig) -> WatcherConfig,
    ) -> Self {
        let current = self.watcher(entry_type);
        self.watchers.insert(entry_type, configure(current));
        self
    }

    /// Turn one watcher off.
    pub fn disable(self, entry_type: EntryType) -> Self {
        self.watch(entry_type, |w| WatcherConfig { enabled: false, ..w })
    }

    /// Keep only batches that pass `rule`.
    pub fn keep_batch_if(mut self, rule: impl Fn(&Batch) -> bool + Send + Sync + 'static) -> Self {
        self.keep = Some(std::sync::Arc::new(rule));
        self
    }
}

/// A configuration value Lookout cannot use.
///
/// Names the variable (or field), what it was given and what it accepts, so
/// the message alone is enough to fix the deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The environment variable or field at fault: `LOOKOUT_STORE`.
    pub variable: String,
    /// What it was set to.
    pub value: String,
    /// What it accepts.
    pub expected: String,
}

impl ConfigError {
    fn new(variable: &str, value: impl Into<String>, expected: &str) -> Self {
        Self { variable: variable.to_string(), value: value.into(), expected: expected.to_string() }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is {:?}; expected {}", self.variable, self.value, self.expected)
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for rainier_support::Error {
    fn from(error: ConfigError) -> Self {
        rainier_support::Error::internal(error.to_string())
    }
}

/// The log levels `log_level` accepts, most severe first. `off` records none.
pub const LOG_LEVELS: &[&str] = &["off", "error", "warn", "info", "debug", "trace"];

/// How many batches the cache store keeps when `LOOKOUT_CAPACITY` is unset.
pub const DEFAULT_CACHE_CAPACITY: usize = 1000;

impl LookoutConfig {
    /// Read the scalars from the process environment, over the defaults.
    ///
    /// | Variable | Sets | Accepts |
    /// |---|---|---|
    /// | `LOOKOUT_ENABLED` | [`enabled`](Self::enabled) | a boolean |
    /// | `LOOKOUT_STORE` | [`store`](Self::store) | `memory`, `database`, `cache` |
    /// | `LOOKOUT_CONNECTION` | the database store's connection | a name |
    /// | `LOOKOUT_CACHE_STORE` | the cache store's store | a name |
    /// | `LOOKOUT_CAPACITY` | the cache store's capacity | a positive integer |
    /// | `LOOKOUT_PATH` | [`Dashboard::path`] | a path starting `/` |
    /// | `LOOKOUT_GUARD` | [`Dashboard::guard`] | a guard name |
    /// | `LOOKOUT_REQUEST_SAMPLE_RATE` | [`request_sample_rate`](Self::request_sample_rate) | `0.0..=1.0` |
    /// | `LOOKOUT_JOB_SAMPLE_RATE` | [`job_sample_rate`](Self::job_sample_rate) | `0.0..=1.0` |
    /// | `LOOKOUT_COMMAND_SAMPLE_RATE` | [`command_sample_rate`](Self::command_sample_rate) | `0.0..=1.0` |
    /// | `LOOKOUT_SLOW_QUERY_MS` | the query watcher's `slow_ms` | an integer |
    /// | `LOOKOUT_SLOW_REQUEST_MS` | the request watcher's `slow_ms` | an integer |
    /// | `LOOKOUT_LOG_LEVEL` | [`log_level`](Self::log_level) | one of [`LOG_LEVELS`] |
    /// | `LOOKOUT_RETENTION_HOURS` | [`Retention::batches_hours`] | a positive integer |
    /// | `LOOKOUT_AGGREGATE_RETENTION_DAYS` | [`Retention::aggregates_days`] | a positive integer |
    /// | `LOOKOUT_CAPTURE_REQUEST_HEADERS`, `…_REQUEST_BODY_ON_EXCEPTION`, `…_CACHE_KEYS`, `…_MAIL`, `…_USER_ID` | [`Capture`] | booleans |
    /// | `APP_ENV` | [`local`](Self::local), when `local` | any |
    ///
    /// Booleans accept `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`. An
    /// empty variable counts as unset. Anything else is an error naming the
    /// variable — a typo in a deployment should fail the boot, not silently
    /// record nothing.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`from_env`](Self::from_env), reading variables through `lookup` —
    /// for tests, and for an application that keeps its environment elsewhere.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| lookup(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let mut config = Self::default();

        if let Some(v) = get("LOOKOUT_ENABLED") {
            config.enabled = parse_bool("LOOKOUT_ENABLED", &v)?;
        }

        let capacity = match get("LOOKOUT_CAPACITY") {
            Some(v) => {
                let n = parse_u64("LOOKOUT_CAPACITY", &v)?;
                if n == 0 {
                    return Err(ConfigError::new("LOOKOUT_CAPACITY", v, "a positive integer"));
                }
                usize::try_from(n)
                    .map_err(|_| ConfigError::new("LOOKOUT_CAPACITY", v, "a smaller integer"))?
            }
            None => DEFAULT_CACHE_CAPACITY,
        };
        if let Some(v) = get("LOOKOUT_STORE") {
            config.store = match v.to_ascii_lowercase().as_str() {
                "memory" => StoreKind::Memory,
                "database" => StoreKind::Database { connection: get("LOOKOUT_CONNECTION") },
                "cache" => StoreKind::Cache { store: get("LOOKOUT_CACHE_STORE"), capacity },
                _ => return Err(ConfigError::new("LOOKOUT_STORE", v, "memory, database or cache")),
            };
        }

        if let Some(v) = get("LOOKOUT_PATH") {
            if !v.starts_with('/') || v.len() < 2 {
                return Err(ConfigError::new("LOOKOUT_PATH", v, "a path such as /_lookout"));
            }
            config.dashboard.path = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = get("LOOKOUT_GUARD") {
            config.dashboard.guard = Some(v);
        }

        if let Some(v) = get("LOOKOUT_REQUEST_SAMPLE_RATE") {
            config.request_sample_rate = parse_rate("LOOKOUT_REQUEST_SAMPLE_RATE", &v)?;
        }
        if let Some(v) = get("LOOKOUT_JOB_SAMPLE_RATE") {
            config.job_sample_rate = parse_rate("LOOKOUT_JOB_SAMPLE_RATE", &v)?;
        }
        if let Some(v) = get("LOOKOUT_COMMAND_SAMPLE_RATE") {
            config.command_sample_rate = parse_rate("LOOKOUT_COMMAND_SAMPLE_RATE", &v)?;
        }

        if let Some(v) = get("LOOKOUT_SLOW_QUERY_MS") {
            let ms = parse_u64("LOOKOUT_SLOW_QUERY_MS", &v)?;
            config = config.watch(EntryType::Query, |w| WatcherConfig { slow_ms: Some(ms), ..w });
        }
        if let Some(v) = get("LOOKOUT_SLOW_REQUEST_MS") {
            let ms = parse_u64("LOOKOUT_SLOW_REQUEST_MS", &v)?;
            config = config.watch(EntryType::Request, |w| WatcherConfig { slow_ms: Some(ms), ..w });
        }

        if let Some(v) = get("LOOKOUT_LOG_LEVEL") {
            let level = v.to_ascii_lowercase();
            if !LOG_LEVELS.contains(&level.as_str()) {
                return Err(ConfigError::new(
                    "LOOKOUT_LOG_LEVEL",
                    v,
                    "off, error, warn, info, debug or trace",
                ));
            }
            config.log_level = level;
        }

        if let Some(v) = get("LOOKOUT_RETENTION_HOURS") {
            config.retention.batches_hours = parse_positive("LOOKOUT_RETENTION_HOURS", &v)?;
        }
        if let Some(v) = get("LOOKOUT_AGGREGATE_RETENTION_DAYS") {
            config.retention.aggregates_days =
                parse_positive("LOOKOUT_AGGREGATE_RETENTION_DAYS", &v)?;
        }

        let flags: [(&str, &mut bool); 5] = [
            ("LOOKOUT_CAPTURE_REQUEST_HEADERS", &mut config.capture.request_headers),
            (
                "LOOKOUT_CAPTURE_REQUEST_BODY_ON_EXCEPTION",
                &mut config.capture.request_body_on_exception,
            ),
            ("LOOKOUT_CAPTURE_CACHE_KEYS", &mut config.capture.cache_keys),
            ("LOOKOUT_CAPTURE_MAIL", &mut config.capture.mail),
            ("LOOKOUT_CAPTURE_USER_ID", &mut config.capture.user_id),
        ];
        for (name, slot) in flags {
            if let Some(v) = get(name) {
                *slot = parse_bool(name, &v)?;
            }
        }

        if let Some(v) = get("APP_ENV") {
            config.local = v.eq_ignore_ascii_case("local");
        }

        config.validate()?;
        Ok(config)
    }

    /// Check the values a builder can get wrong: sample rates outside
    /// `0.0..=1.0`, an unknown log level, an empty channel.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let rates = [
            ("request_sample_rate", self.request_sample_rate),
            ("job_sample_rate", self.job_sample_rate),
            ("command_sample_rate", self.command_sample_rate),
        ];
        for (name, rate) in rates {
            if !(0.0..=1.0).contains(&rate) {
                return Err(ConfigError::new(name, rate.to_string(), "a number from 0.0 to 1.0"));
            }
        }
        for (entry_type, watcher) in &self.watchers {
            if !(0.0..=1.0).contains(&watcher.sample_rate) {
                return Err(ConfigError::new(
                    &format!("watchers.{entry_type}.sample_rate"),
                    watcher.sample_rate.to_string(),
                    "a number from 0.0 to 1.0",
                ));
            }
        }
        if !LOG_LEVELS.contains(&self.log_level.to_ascii_lowercase().as_str()) {
            return Err(ConfigError::new(
                "log_level",
                self.log_level.clone(),
                "off, error, warn, info, debug or trace",
            ));
        }
        if self.ingest.channel_capacity == 0 {
            return Err(ConfigError::new("ingest.channel_capacity", "0", "a positive integer"));
        }
        if self.ingest.flush_batch_size == 0 {
            return Err(ConfigError::new("ingest.flush_batch_size", "0", "a positive integer"));
        }
        if let StoreKind::Cache { capacity: 0, .. } = self.store {
            return Err(ConfigError::new("store.capacity", "0", "a positive integer"));
        }
        Ok(())
    }

    /// Mark this the local environment, where the default keep rule keeps
    /// every batch.
    pub fn local(mut self, local: bool) -> Self {
        self.local = local;
        self
    }
}

fn parse_bool(name: &str, value: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::new(name, value, "true or false")),
    }
}

fn parse_u64(name: &str, value: &str) -> Result<u64, ConfigError> {
    value.parse().map_err(|_| ConfigError::new(name, value, "a whole number"))
}

fn parse_positive(name: &str, value: &str) -> Result<u64, ConfigError> {
    match parse_u64(name, value)? {
        0 => Err(ConfigError::new(name, value, "a positive integer")),
        n => Ok(n),
    }
}

fn parse_rate(name: &str, value: &str) -> Result<f64, ConfigError> {
    match value.parse::<f64>() {
        Ok(rate) if (0.0..=1.0).contains(&rate) => Ok(rate),
        _ => Err(ConfigError::new(name, value, "a number from 0.0 to 1.0")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("api.*", "api.posts.show"));
        assert!(!glob("api.*", "web.home"));
        assert!(glob("/assets/*", "/assets/app.js"));
        assert!(glob("*.show", "posts.show"));
        assert!(glob("a*c*e", "abcde"));
        assert!(glob("health", "health"));
        assert!(!glob("health", "healthz"));
    }

    #[test]
    fn exclude_wins_and_empty_include_matches_everything() {
        let p = Patterns { include: vec![], exclude: vec!["health".into()] };
        assert!(p.allows("posts.index"));
        assert!(!p.allows("health"));
        let p = Patterns { include: vec!["api.*".into()], exclude: vec!["api.internal.*".into()] };
        assert!(p.allows("api.posts"));
        assert!(!p.allows("api.internal.x"));
        assert!(!p.allows("web.home"));
    }

    fn lookup(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn from_env_reads_every_scalar() {
        let config = LookoutConfig::from_lookup(lookup(&[
            ("LOOKOUT_ENABLED", "true"),
            ("LOOKOUT_STORE", "cache"),
            ("LOOKOUT_CACHE_STORE", "redis"),
            ("LOOKOUT_CAPACITY", "250"),
            ("LOOKOUT_PATH", "/ops/lookout/"),
            ("LOOKOUT_GUARD", "admin"),
            ("LOOKOUT_REQUEST_SAMPLE_RATE", "0.25"),
            ("LOOKOUT_JOB_SAMPLE_RATE", "0.5"),
            ("LOOKOUT_COMMAND_SAMPLE_RATE", "0"),
            ("LOOKOUT_SLOW_QUERY_MS", "25"),
            ("LOOKOUT_SLOW_REQUEST_MS", "400"),
            ("LOOKOUT_LOG_LEVEL", "WARN"),
            ("LOOKOUT_RETENTION_HOURS", "48"),
            ("LOOKOUT_AGGREGATE_RETENTION_DAYS", "7"),
            ("LOOKOUT_CAPTURE_CACHE_KEYS", "yes"),
            ("LOOKOUT_CAPTURE_USER_ID", "1"),
            ("APP_ENV", "local"),
        ]))
        .expect("valid");
        assert!(config.enabled);
        assert_eq!(config.store, StoreKind::Cache { store: Some("redis".into()), capacity: 250 });
        assert_eq!(config.dashboard.path, "/ops/lookout");
        assert_eq!(config.dashboard.guard.as_deref(), Some("admin"));
        assert_eq!(config.request_sample_rate, 0.25);
        assert_eq!(config.job_sample_rate, 0.5);
        assert_eq!(config.command_sample_rate, 0.0);
        assert_eq!(config.watcher(EntryType::Query).slow_ms, Some(25));
        assert_eq!(config.watcher(EntryType::Request).slow_ms, Some(400));
        assert_eq!(config.log_level, "warn");
        assert_eq!(config.retention, Retention { batches_hours: 48, aggregates_days: 7 });
        assert!(config.capture.cache_keys && config.capture.user_id);
        assert!(!config.capture.mail);
        assert!(config.local);
    }

    #[test]
    fn from_env_defaults_when_nothing_is_set() {
        let config = LookoutConfig::from_lookup(|_| None).expect("valid");
        assert!(!config.enabled);
        assert_eq!(config.store, StoreKind::Memory);
        assert!(!config.local);
    }

    #[test]
    fn from_env_rejects_bad_values_by_name() {
        let cases: &[(&'static str, &'static str)] = &[
            ("LOOKOUT_ENABLED", "maybe"),
            ("LOOKOUT_STORE", "disk"),
            ("LOOKOUT_CAPACITY", "0"),
            ("LOOKOUT_PATH", "lookout"),
            ("LOOKOUT_REQUEST_SAMPLE_RATE", "1.5"),
            ("LOOKOUT_JOB_SAMPLE_RATE", "half"),
            ("LOOKOUT_SLOW_QUERY_MS", "-3"),
            ("LOOKOUT_LOG_LEVEL", "loud"),
            ("LOOKOUT_RETENTION_HOURS", "0"),
            ("LOOKOUT_CAPTURE_MAIL", "sure"),
        ];
        for &(name, value) in cases {
            let vars: &'static [(&'static str, &'static str)] =
                Box::leak(vec![(name, value)].into_boxed_slice());
            let error = LookoutConfig::from_lookup(lookup(vars)).expect_err(name);
            assert_eq!(error.variable, name);
            assert!(error.to_string().contains(name), "{error}");
        }
    }

    #[test]
    fn validate_catches_builder_mistakes() {
        let mut config = LookoutConfig { request_sample_rate: 2.0, ..LookoutConfig::default() };
        assert_eq!(config.validate().expect_err("rate").variable, "request_sample_rate");
        config.request_sample_rate = 1.0;
        config = config.watch(EntryType::Cache, |w| WatcherConfig { sample_rate: -1.0, ..w });
        assert!(config.validate().is_err());
    }
}
