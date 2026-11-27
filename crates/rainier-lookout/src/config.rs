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

/// What may be captured beyond the defaults. Everything that could carry a
/// value is off: the defaults carry what the framework's telemetry spans
/// carry, which deliberately leave out values. Code traces carry only
/// function names and source locations, and are on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    /// Request headers (redacted by name).
    pub request_headers: bool,
    /// The request body — only for batches with an exception.
    pub request_body_on_exception: bool,
    /// The request body, for every kept batch. Parsed (JSON or a form) and
    /// redacted on the writer, after the request has ended; up to
    /// [`max_body_bytes`](Self::max_body_bytes).
    #[serde(default)]
    pub request_body: bool,
    /// The response body, for every kept batch: JSON and text, redacted like
    /// the request's; a streamed or binary body is noted by its size and type,
    /// not kept.
    #[serde(default)]
    pub response_body: bool,
    /// The most of a body kept, in bytes. What is beyond it is cut before it
    /// is ever parsed.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Routes whose bodies are never kept, whatever the two switches above
    /// say: globs over the route's name, else its pattern
    /// (`api.chat.*`, `/v1/identity-documents/*`). For the endpoints that
    /// carry what must not leave them — private messages, identity documents
    /// — where redacting by field name is not enough.
    #[serde(default)]
    pub bodies_except: Vec<String>,
    /// Cache keys.
    pub cache_keys: bool,
    /// Mail subjects and recipients.
    pub mail: bool,
    /// The authenticated user's id on the batch. Needed for user scoping and
    /// the "top users" view.
    pub user_id: bool,
    /// A request answered with a client error (a 4xx — not found, invalid
    /// input, unauthenticated) as an exception entry. Off, the request is
    /// still recorded with its status, but it is not a failure: it adds no
    /// exception and does not flag the batch. A client error inside a job or
    /// a command is always recorded — there it is the application's fault.
    #[serde(default)]
    pub client_errors: bool,
    /// Where in the application a slow query, cache call or outgoing request
    /// came from: the application's own frames of the stack, as `code.trace`.
    /// Lookout asks publishers for a stack over the lowest slow threshold of
    /// these watchers, and the publisher captures it on the thread that did
    /// the work when it finishes — once per family per unit of work, so fast
    /// work and repeats cost nothing. Lookout keeps it for an entry over its
    /// own threshold and symbolises it on the writer. Function names need a
    /// binary that is not stripped; files and lines need line tables
    /// (`debug = "line-tables-only"`). On by default.
    #[serde(default = "yes")]
    pub code_traces: bool,
    /// Who made the request, beyond the id: a name, an address, an avatar,
    /// whatever the application describes with
    /// [`Lookout::describe_user`](crate::Lookout::describe_user). Off, a
    /// description given is dropped.
    #[serde(default)]
    pub user: bool,
    /// What made the request: the client's address, browser, operating
    /// system and device (from `User-Agent` and client hints), the page it
    /// came from (`Origin`, `Referer`) and its language. Read from headers the
    /// request carries anyway; the user agent is parsed on the writer.
    #[serde(default)]
    pub client: bool,
    /// How much of the client's address [`client`](Self::client) keeps.
    #[serde(default)]
    pub client_ip: ClientIp,
}

/// How much of a client's address is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientIp {
    /// The network, not the host: `203.0.113.0/24`, `2001:db8:1::/48`. Enough
    /// to tell one ISP or office from another, not to find a person.
    #[default]
    Partial,
    /// The whole address.
    Full,
    /// None of it.
    None,
}

/// Request tracing: recording, on this service's side, which call from
/// another service each request answered and which calls it made — by the
/// span ids request tagging puts on the wire — and serving that for
/// something outside, an operations console, to put a trace across services
/// back together. Lookouts never talk to one another. See `docs/lookout.md`,
/// "Traces across services".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestTracing {
    /// Off unless turned on (`LOOKOUT_REQUEST_TRACING`).
    pub enabled: bool,
    /// The fraction of traces kept whole, `0.0..=1.0`
    /// (`LOOKOUT_TRACE_SAMPLE_RATE`). Decided from the trace id, so every
    /// service with the same rate keeps the same traces without asking one
    /// another. Outside the sample, each service keeps its part by its own
    /// rule — failed and slow work, as ever.
    pub trace_sample_rate: f64,
    /// The name this service goes by in a trace (`LOOKOUT_SERVICE`, else
    /// `APP_NAME`).
    pub service: Option<String>,
}

impl Default for RequestTracing {
    fn default() -> Self {
        Self { enabled: false, trace_sample_rate: 0.01, service: None }
    }
}

impl RequestTracing {
    /// Whether the trace `trace_id` is in the sample every service agrees on:
    /// its first sixteen hex digits, as a fraction of the largest, below the
    /// rate. A trace id that is not hex is never sampled.
    pub fn samples(&self, trace_id: &str) -> bool {
        if self.trace_sample_rate <= 0.0 {
            return false;
        }
        if self.trace_sample_rate >= 1.0 {
            return true;
        }
        let Some(head) = trace_id.get(..16) else { return false };
        let Ok(value) = u64::from_str_radix(head, 16) else { return false };
        (value as f64 / u64::MAX as f64) < self.trace_sample_rate
    }
}

/// Real user monitoring: how requests looked from the browser that made
/// them. See `docs/lookout.md`, "Real user monitoring".
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Rum {
    /// Whether to serve the browser script, accept its reports and mark
    /// responses so a report can name the request it is about.
    #[serde(default)]
    pub enabled: bool,
    /// The origins of the pages allowed to report — the frontends, which on
    /// a split frontend are not this application's own
    /// (`https://example.com`). Also the origins the browser is allowed to
    /// show detailed timing to (`Timing-Allow-Origin`). Empty means none: a
    /// report needs an origin listed here.
    #[serde(default)]
    pub origins: Vec<String>,
    /// The fraction of requests a browser reports, `0.0..=1.0`. A request
    /// slower than [`slow_ms`](Self::slow_ms) in the browser is reported
    /// whatever this says.
    #[serde(default = "default_rum_sample")]
    pub sample_rate: f64,
    /// Report every request that took at least this long in the browser.
    #[serde(default = "default_rum_slow_ms")]
    pub slow_ms: u64,
    /// The key reports are signed with, so one can only describe a request
    /// this application answered. Every replica needs the same one; the
    /// framework derives it from `APP_KEY` when this is unset.
    #[serde(skip)]
    pub secret: Vec<u8>,
    /// Session replay: off unless turned on, and only with RUM on.
    #[serde(default)]
    pub replay: Replay,
    /// How long a request from a listed frontend that would otherwise be
    /// dropped is held, so that a browser report about it can pair with it
    /// and keep it. `0` holds nothing. See `docs/lookout.md`, "Pairing".
    #[serde(default = "default_rum_hold_seconds")]
    pub hold_seconds: u64,
    /// The request header a proxy in front puts the visitor's country in —
    /// `cf-ipcountry` behind Cloudflare — for the RUM rollups' `country`.
    /// `None`, the default, files every report under `unknown`. Only a
    /// two-character code is taken; anything else is `unknown` too.
    #[serde(default)]
    pub country_header: Option<String>,
}

fn default_rum_hold_seconds() -> u64 {
    120
}

/// Session replay: what the page looked like in the seconds before a request
/// failed, was slow, or the page threw — recorded masked, kept only then.
/// See `docs/lookout.md`, "Session replay".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replay {
    /// Whether pages may record at all.
    #[serde(default)]
    pub enabled: bool,
    /// Pages never recorded, whatever else is said: globs over the page's
    /// path (`/chats*`, `/settings/*`).
    #[serde(default)]
    pub except: Vec<String>,
    /// Whether a page records before it says the visitor agreed to it.
    #[serde(default)]
    pub consent: ReplayConsent,
    /// The most one recording may weigh, compressed, in bytes.
    #[serde(default = "default_replay_max_bytes")]
    pub max_bytes: usize,
    /// When a consenting page uploads what it recorded.
    #[serde(default)]
    pub mode: ReplayMode,
}

/// When a recording page uploads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    /// Only the last ~30 seconds, and only when an API request fails or is
    /// slow, or the page throws.
    #[default]
    Errors,
    /// The whole page view, in ~15-second pieces, so a session can be played
    /// from start to end. For visitors who agreed to it — the consent rule
    /// still applies — and a store with room for it.
    Always,
}

impl Default for Replay {
    fn default() -> Self {
        Self {
            enabled: false,
            except: Vec::new(),
            consent: ReplayConsent::Required,
            max_bytes: default_replay_max_bytes(),
            mode: ReplayMode::Errors,
        }
    }
}

fn default_replay_max_bytes() -> usize {
    256 * 1024
}

/// When a page may start recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayConsent {
    /// Only once the page calls `LookoutRum.allowReplay()` — from a consent
    /// prompt, a setting, whatever the site asks the visitor with.
    #[default]
    Required,
    /// From the start. For an internal tool, or a site whose terms already
    /// cover it — a decision the site makes, not a default.
    Implied,
}

impl std::fmt::Debug for Rum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rum")
            .field("enabled", &self.enabled)
            .field("origins", &self.origins)
            .field("sample_rate", &self.sample_rate)
            .field("slow_ms", &self.slow_ms)
            .field("secret", &if self.secret.is_empty() { "unset" } else { "set" })
            .field("country_header", &self.country_header)
            .finish()
    }
}

fn default_rum_sample() -> f64 {
    0.1
}

fn default_rum_slow_ms() -> u64 {
    1000
}

fn yes() -> bool {
    true
}

fn default_max_body_bytes() -> usize {
    64 * 1024
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            request_headers: false,
            request_body_on_exception: false,
            request_body: false,
            response_body: false,
            max_body_bytes: default_max_body_bytes(),
            bodies_except: Vec::new(),
            cache_keys: false,
            mail: false,
            user_id: false,
            client_errors: false,
            code_traces: true,
            user: false,
            client: false,
            client_ip: ClientIp::Partial,
        }
    }
}

/// Names that are always redacted, plus the application's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redaction {
    /// Header names, case-insensitive.
    pub headers: BTreeSet<String>,
    /// Body and payload field names, case-insensitive.
    pub fields: BTreeSet<String>,
    /// Field names are also redacted when they **contain** one of these,
    /// case-insensitive: `password` covers `new_password` and
    /// `password_confirmation` without listing each.
    #[serde(default = "default_field_patterns")]
    pub field_patterns: BTreeSet<String>,
    /// Star out any other string that looks like a secret — long, one word,
    /// from a token's alphabet, and random enough. See
    /// [`redact::mask_secret`](crate::redact::mask_secret).
    #[serde(default = "yes")]
    pub mask_secrets: bool,
    /// Longest value kept, in bytes; longer ones are truncated.
    pub max_value_len: usize,
}

/// Substrings that make a field name sensitive wherever they appear in it.
fn default_field_patterns() -> BTreeSet<String> {
    [
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "apikey",
        "private_key",
        "credential",
        "verifier",
        "signature",
        "authorization",
        "session_ticket",
        "recovery_code",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
            "cvv",
            "ssn",
            "pin",
            "otp",
            "code",
            "handoff",
        ];
        Self {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            fields: fields.iter().map(|s| s.to_string()).collect(),
            field_patterns: default_field_patterns(),
            mask_secrets: true,
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
    /// Whether "Live" streams new batches to the browser as they are stored
    /// (server-sent events at `{path}/api/stream`), rather than the page
    /// re-asking every few seconds. On by default; `LOOKOUT_STREAM=false`
    /// falls back to polling, for a proxy that buffers responses.
    #[serde(default = "yes")]
    pub stream: bool,
    /// The application's MCP server, for the dashboard's MCP page — where it
    /// is and how to connect. `None` when there is none; set by whoever
    /// mounts both, never from the environment.
    #[serde(default)]
    pub mcp: Option<McpLink>,
}

/// The application's MCP server, as the dashboard's MCP page describes it.
///
/// Nothing secret: whether tokens exist, never what they are.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpLink {
    /// Where it answers, from the site's root: `/_mcp`.
    pub path: String,
    /// The name it reports, and a sensible name for the client to give it.
    pub name: String,
    /// Whether anyone may connect here without a token: the local
    /// environment, with the server open locally.
    pub open: bool,
    /// Whether any bearer token is configured.
    pub tokens: bool,
    /// The tools it offers, by name.
    pub tools: Vec<String>,
    /// The prompts it offers, by name.
    pub prompts: Vec<String>,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/_lookout".to_string(),
            guard: None,
            ability: "viewLookout".to_string(),
            open_locally: true,
            stream: true,
            mcp: None,
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
    /// How many threads Lookout's bus subscriber records on. Each unit of
    /// work's events go to one of them, so they record in parallel; the
    /// request thread's cost does not change with it. Default: a quarter of
    /// the machine's cores, between one and four.
    pub threads: usize,
}

impl Default for Ingest {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        Self {
            channel_capacity: 10_000,
            flush_interval_ms: 1000,
            flush_batch_size: 500,
            threads: (cores / 4).clamp(1, 4),
        }
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
    /// When the dashboard calls a change an insight, and how far it forecasts.
    #[serde(default)]
    pub insights: crate::trends::InsightsConfig,
    /// Real user monitoring: off unless turned on.
    #[serde(default)]
    pub rum: Rum,
    /// Keeping traces that cross services whole: off unless turned on.
    #[serde(default)]
    pub request_tracing: RequestTracing,
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
            .field("insights", &self.insights)
            .field("rum", &self.rum)
            .field("request_tracing", &self.request_tracing)
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
            insights: crate::trends::InsightsConfig::default(),
            rum: Rum {
                hold_seconds: default_rum_hold_seconds(),
                sample_rate: default_rum_sample(),
                slow_ms: default_rum_slow_ms(),
                ..Rum::default()
            },
            request_tracing: RequestTracing::default(),
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

    /// Turn on request tracing: keep traces that cross services whole, and
    /// answer for them on the trace API. See [`RequestTracing`].
    pub fn request_tracing(mut self, request_tracing: RequestTracing) -> Self {
        self.request_tracing = request_tracing;
        self
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
    /// | `LOOKOUT_CAPTURE_REQUEST_HEADERS`, `…_REQUEST_BODY`, `…_RESPONSE_BODY`, `…_REQUEST_BODY_ON_EXCEPTION`, `…_CACHE_KEYS`, `…_MAIL`, `…_USER_ID`, `…_CLIENT_ERRORS`, `…_CODE_TRACES` | [`Capture`] | booleans |
    /// | `LOOKOUT_CAPTURE_MAX_BODY_BYTES` | [`Capture::max_body_bytes`] | a positive integer |
    /// | `LOOKOUT_CAPTURE_BODIES_EXCEPT` | [`Capture::bodies_except`] | comma-separated globs |
    /// | `LOOKOUT_MASK_SECRETS` | [`Redaction::mask_secrets`] | a boolean |
    /// | `LOOKOUT_STREAM` | [`Dashboard::stream`] | a boolean |
    /// | `LOOKOUT_CAPTURE_USER` | [`Capture::user`] | a boolean |
    /// | `LOOKOUT_CAPTURE_CLIENT` | [`Capture::client`] | a boolean |
    /// | `LOOKOUT_CAPTURE_CLIENT_IP` | [`Capture::client_ip`] | `partial`, `full` or `none` |
    /// | `LOOKOUT_RUM` | [`Rum::enabled`] | a boolean |
    /// | `LOOKOUT_RUM_ORIGINS` | [`Rum::origins`] | comma-separated origins |
    /// | `LOOKOUT_RUM_SAMPLE_RATE` | [`Rum::sample_rate`] | `0.0` to `1.0` |
    /// | `LOOKOUT_RUM_SLOW_MS` | [`Rum::slow_ms`] | an integer |
    /// | `LOOKOUT_RUM_SECRET` | [`Rum::secret`] (default: from `APP_KEY`) | a string |
    /// | `LOOKOUT_RUM_HOLD_SECONDS` | [`Rum::hold_seconds`] | an integer |
    /// | `LOOKOUT_RUM_COUNTRY_HEADER` | [`Rum::country_header`] | a header name (`cf-ipcountry`) |
    /// | `LOOKOUT_RUM_REPLAY` | [`Replay::enabled`] | a boolean |
    /// | `LOOKOUT_RUM_REPLAY_EXCEPT` | [`Replay::except`] | comma-separated globs over page paths |
    /// | `LOOKOUT_RUM_REPLAY_CONSENT` | [`Replay::consent`] | `required` or `implied` |
    /// | `LOOKOUT_RUM_REPLAY_MODE` | [`Replay::mode`] | `errors` or `always` |
    /// | `LOOKOUT_RUM_REPLAY_MAX_BYTES` | [`Replay::max_bytes`] | a positive integer |
    /// | `LOOKOUT_INSIGHTS_ENABLED` | [`InsightsConfig::enabled`](crate::trends::InsightsConfig) | a boolean |
    /// | `LOOKOUT_INSIGHTS_WINDOW_MINUTES` | the window compared with the one before | a positive integer |
    /// | `LOOKOUT_INSIGHTS_MIN_COUNT` | how many a key needs before it is reported | a positive integer |
    /// | `LOOKOUT_INSIGHTS_CHANGE_PERCENT` | the smallest change reported | a positive integer |
    /// | `LOOKOUT_INSIGHTS_FORECAST_BUCKETS` | how far a forecast reaches; `0` draws none | an integer |
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

        let flags: [(&str, &mut bool); 15] = [
            ("LOOKOUT_RUM_REPLAY", &mut config.rum.replay.enabled),
            ("LOOKOUT_CAPTURE_USER", &mut config.capture.user),
            ("LOOKOUT_CAPTURE_CLIENT", &mut config.capture.client),
            ("LOOKOUT_RUM", &mut config.rum.enabled),
            ("LOOKOUT_CAPTURE_REQUEST_HEADERS", &mut config.capture.request_headers),
            ("LOOKOUT_CAPTURE_REQUEST_BODY", &mut config.capture.request_body),
            ("LOOKOUT_CAPTURE_RESPONSE_BODY", &mut config.capture.response_body),
            ("LOOKOUT_MASK_SECRETS", &mut config.redaction.mask_secrets),
            ("LOOKOUT_STREAM", &mut config.dashboard.stream),
            (
                "LOOKOUT_CAPTURE_REQUEST_BODY_ON_EXCEPTION",
                &mut config.capture.request_body_on_exception,
            ),
            ("LOOKOUT_CAPTURE_CACHE_KEYS", &mut config.capture.cache_keys),
            ("LOOKOUT_CAPTURE_MAIL", &mut config.capture.mail),
            ("LOOKOUT_CAPTURE_USER_ID", &mut config.capture.user_id),
            ("LOOKOUT_CAPTURE_CLIENT_ERRORS", &mut config.capture.client_errors),
            ("LOOKOUT_CAPTURE_CODE_TRACES", &mut config.capture.code_traces),
        ];
        for (name, slot) in flags {
            if let Some(v) = get(name) {
                *slot = parse_bool(name, &v)?;
            }
        }
        if let Some(v) = get("LOOKOUT_CAPTURE_BODIES_EXCEPT") {
            config.capture.bodies_except =
                v.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect();
        }
        if let Some(v) = get("LOOKOUT_CAPTURE_CLIENT_IP") {
            config.capture.client_ip = match v.to_ascii_lowercase().as_str() {
                "partial" => ClientIp::Partial,
                "full" => ClientIp::Full,
                "none" => ClientIp::None,
                _ => {
                    return Err(ConfigError::new(
                        "LOOKOUT_CAPTURE_CLIENT_IP",
                        v,
                        "partial, full or none",
                    ))
                }
            };
        }
        if let Some(v) = get("LOOKOUT_RUM_ORIGINS") {
            config.rum.origins = v
                .split(',')
                .map(|o| o.trim().trim_end_matches('/'))
                .filter(|o| !o.is_empty())
                .map(String::from)
                .collect();
        }
        if let Some(v) = get("LOOKOUT_RUM_SAMPLE_RATE") {
            config.rum.sample_rate = parse_rate("LOOKOUT_RUM_SAMPLE_RATE", &v)?;
        }
        if let Some(v) = get("LOOKOUT_RUM_SLOW_MS") {
            config.rum.slow_ms = parse_u64("LOOKOUT_RUM_SLOW_MS", &v)?;
        }
        if let Some(v) = get("LOOKOUT_RUM_COUNTRY_HEADER") {
            let name = v.trim().to_ascii_lowercase();
            if name.is_empty()
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(ConfigError::new(
                    "LOOKOUT_RUM_COUNTRY_HEADER",
                    v,
                    "a header name, like cf-ipcountry",
                ));
            }
            config.rum.country_header = Some(name);
        }
        if let Some(v) = get("LOOKOUT_RUM_REPLAY_EXCEPT") {
            config.rum.replay.except =
                v.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect();
        }
        if let Some(v) = get("LOOKOUT_RUM_REPLAY_CONSENT") {
            config.rum.replay.consent = match v.to_ascii_lowercase().as_str() {
                "required" => ReplayConsent::Required,
                "implied" => ReplayConsent::Implied,
                _ => {
                    return Err(ConfigError::new(
                        "LOOKOUT_RUM_REPLAY_CONSENT",
                        v,
                        "required or implied",
                    ))
                }
            };
        }
        if let Some(v) = get("LOOKOUT_RUM_REPLAY_MODE") {
            config.rum.replay.mode = match v.to_ascii_lowercase().as_str() {
                "errors" => ReplayMode::Errors,
                "always" => ReplayMode::Always,
                _ => {
                    return Err(ConfigError::new("LOOKOUT_RUM_REPLAY_MODE", v, "errors or always"))
                }
            };
        }
        if let Some(v) = get("LOOKOUT_RUM_REPLAY_MAX_BYTES") {
            config.rum.replay.max_bytes =
                parse_positive("LOOKOUT_RUM_REPLAY_MAX_BYTES", &v)? as usize;
        }
        if let Some(v) = get("LOOKOUT_RUM_HOLD_SECONDS") {
            config.rum.hold_seconds = parse_u64("LOOKOUT_RUM_HOLD_SECONDS", &v)?;
        }
        if let Some(v) = get("LOOKOUT_REQUEST_TRACING") {
            config.request_tracing.enabled = parse_bool("LOOKOUT_REQUEST_TRACING", &v)?;
        }
        if let Some(v) = get("LOOKOUT_TRACE_SAMPLE_RATE") {
            config.request_tracing.trace_sample_rate = parse_rate("LOOKOUT_TRACE_SAMPLE_RATE", &v)?;
        }
        config.request_tracing.service = get("LOOKOUT_SERVICE").or_else(|| get("APP_NAME"));
        if let Some(v) = get("LOOKOUT_RUM_SECRET") {
            config.rum.secret = v.into_bytes();
        }
        if let Some(v) = get("LOOKOUT_CAPTURE_MAX_BODY_BYTES") {
            config.capture.max_body_bytes =
                parse_positive("LOOKOUT_CAPTURE_MAX_BODY_BYTES", &v)? as usize;
        }

        if let Some(v) = get("LOOKOUT_INSIGHTS_ENABLED") {
            config.insights.enabled = parse_bool("LOOKOUT_INSIGHTS_ENABLED", &v)?;
        }
        if let Some(v) = get("LOOKOUT_INSIGHTS_WINDOW_MINUTES") {
            let minutes = parse_positive("LOOKOUT_INSIGHTS_WINDOW_MINUTES", &v)?;
            if minutes > 7 * 24 * 60 {
                return Err(ConfigError::new(
                    "LOOKOUT_INSIGHTS_WINDOW_MINUTES",
                    v,
                    "at most 10080 (a week)",
                ));
            }
            config.insights.window_minutes = minutes;
        }
        if let Some(v) = get("LOOKOUT_INSIGHTS_MIN_COUNT") {
            config.insights.min_count = parse_positive("LOOKOUT_INSIGHTS_MIN_COUNT", &v)?;
        }
        if let Some(v) = get("LOOKOUT_INSIGHTS_CHANGE_PERCENT") {
            config.insights.change_percent = parse_positive("LOOKOUT_INSIGHTS_CHANGE_PERCENT", &v)?;
        }
        if let Some(v) = get("LOOKOUT_INSIGHTS_FORECAST_BUCKETS") {
            let n = parse_u64("LOOKOUT_INSIGHTS_FORECAST_BUCKETS", &v)?;
            if n > 100 {
                return Err(ConfigError::new(
                    "LOOKOUT_INSIGHTS_FORECAST_BUCKETS",
                    v,
                    "an integer from 0 to 100",
                ));
            }
            config.insights.forecast_buckets = n as usize;
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
        if self.ingest.threads == 0 {
            return Err(ConfigError::new("ingest.threads", "0", "a positive integer"));
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
