//! Metrics, tracing and the OpenAPI document, built from
//! [configuration](crate::keys).
//!
//! The three share a shape: **off unless configured on**, each reading one
//! section of `config/`. Nothing here decides for the application; it reads
//! what the application decided and builds it.
//!
//! ```ignore
//! // config/metrics.rs
//! config.set(keys::METRICS_ENABLED, env.bool("METRICS_ENABLED", false))?;
//! config.set(keys::METRICS_PATH, env.string("METRICS_PATH", "/metrics"))?;
//! ```
//!
//! # Why off by default
//!
//! Each costs something a request pays for — a lock and a timer, a span, a
//! document rendered at boot — and more importantly each **exposes
//! something**. A metrics endpoint tells a reader your traffic shape and every
//! route you serve; an OpenAPI document is a map of your API. Both are things
//! to turn on deliberately, on a path you have thought about.

use std::sync::Arc;

use rainier_config::Config;
use rainier_metrics::Metrics;
use rainier_openapi::{OpenApi, Rendered};
use rainier_routing::CompiledRouter;
use rainier_support::Result;
use rainier_telemetry::{LogFormat, Trace};

use crate::keys;

/// What the configuration says about metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSettings {
    /// Whether to record anything at all.
    pub enabled: bool,
    /// Where the scrape endpoint lives.
    pub path: String,
}

impl MetricsSettings {
    /// Read them from `config`.
    pub fn from_config(config: &Config) -> Self {
        Self {
            enabled: config.get(keys::METRICS_ENABLED).unwrap_or(false),
            path: config.get(keys::METRICS_PATH).unwrap_or_else(|| "/metrics".to_string()),
        }
    }

    /// A registry, or `None` when metrics are off.
    ///
    /// `None` rather than a registry nobody scrapes, so
    /// [`RecordMetrics`](rainier_metrics::RecordMetrics) is not installed
    /// either — a timer on every request that nothing reads is pure cost.
    pub fn registry(&self) -> Option<Arc<Metrics>> {
        self.enabled.then(|| Arc::new(Metrics::new()))
    }
}

/// What the configuration says about tracing.
#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySettings {
    /// Whether to join and propagate trace context.
    pub enabled: bool,
    /// The OTLP collector, if spans should be exported.
    pub endpoint: Option<String>,
    /// What this service calls itself.
    pub service_name: String,
    /// What fraction of locally started traces to record.
    pub sample_ratio: f64,
    /// The shape of a log line.
    pub log_format: LogFormat,
}

impl TelemetrySettings {
    /// Read them from `config`.
    pub fn from_config(config: &Config) -> Self {
        Self {
            enabled: config.get(keys::TELEMETRY_ENABLED).unwrap_or(false),
            endpoint: config.get(keys::TELEMETRY_ENDPOINT),
            service_name: config
                .get(keys::TELEMETRY_SERVICE_NAME)
                .or_else(|| config.get(keys::APP_NAME))
                .unwrap_or_else(|| "rainier".to_string()),
            sample_ratio: config.get(keys::TELEMETRY_SAMPLE_RATIO).unwrap_or(1.0),
            log_format: config.setting(keys::LOG_FORMAT).unwrap_or_default(),
        }
    }

    /// The middleware, or `None` when tracing is off.
    pub fn middleware(&self) -> Option<Trace> {
        self.enabled.then(|| Trace::new().sampling(self.sample_ratio > 0.0))
    }

    /// Whether spans should be exported, as opposed to only propagated.
    ///
    /// Propagating without exporting is a perfectly good configuration — the
    /// trace id reaches every log line, which is most of the value — so an
    /// endpoint is what distinguishes the two.
    pub fn exports(&self) -> bool {
        self.enabled && self.endpoint.is_some()
    }
}

impl TelemetrySettings {
    /// Install a global subscriber in this format.
    ///
    /// Returns whether it installed one. `false` means a subscriber was
    /// already set — by the application, or by an earlier boot in the same
    /// test binary — and that one is left alone, because replacing somebody
    /// else's logging configuration is worse than not installing yours.
    ///
    /// `filter` is an `EnvFilter` directive, usually `RUST_LOG`. One that does
    /// not parse falls back to `info` rather than refusing to log at all.
    pub fn install_logging(&self, environment: &str, filter: &str) -> bool {
        use tracing_subscriber::{fmt, EnvFilter};

        let filter =
            EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
        let builder = fmt().with_env_filter(filter);

        match self.log_format.resolve(environment) {
            // `flatten_event` puts the message and its fields at the top
            // level, where every aggregator's default parser looks for them;
            // nested under `fields` they are one query deeper forever.
            LogFormat::Json => {
                builder.json().flatten_event(true).with_current_span(true).try_init().is_ok()
            }
            LogFormat::Compact => builder.compact().try_init().is_ok(),
            // `Auto` cannot reach here — `resolve` has answered it.
            LogFormat::Pretty | LogFormat::Auto => builder.try_init().is_ok(),
        }
    }
}

/// What to log when `RUST_LOG` is unset or unparseable.
const DEFAULT_LOG_FILTER: &str = "info";

/// What the configuration says about the OpenAPI document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenApiSettings {
    /// Whether to serve it.
    pub enabled: bool,
    /// Where.
    pub path: String,
    /// The document's title.
    pub title: String,
    /// The API's version.
    pub version: String,
    /// The base URL to advertise, if any.
    pub server: Option<String>,
}

impl OpenApiSettings {
    /// Read them from `config`.
    pub fn from_config(config: &Config) -> Self {
        Self {
            enabled: config.get(keys::OPENAPI_ENABLED).unwrap_or(false),
            path: config.get(keys::OPENAPI_PATH).unwrap_or_else(|| "/openapi.json".to_string()),
            title: config
                .get(keys::OPENAPI_TITLE)
                .or_else(|| config.get(keys::APP_NAME))
                .unwrap_or_else(|| "API".to_string()),
            version: config.get(keys::OPENAPI_VERSION).unwrap_or_else(|| "1.0.0".to_string()),
            server: config.get(keys::OPENAPI_SERVER),
        }
    }

    /// Apply the configured title, version and server to `document`.
    ///
    /// The application declares the endpoints; the configuration decides what
    /// the document calls itself, because a title and a version are deployment
    /// facts rather than code.
    pub fn apply(&self, document: OpenApi) -> OpenApi {
        let document = document.titled(&self.title, &self.version);

        match &self.server {
            Some(url) => document.server(url),
            None => document,
        }
    }

    /// Render it against `router`, or `None` when it is off.
    pub fn render(&self, document: OpenApi, router: &CompiledRouter) -> Option<Arc<Rendered>> {
        self.enabled.then(|| Arc::new(Rendered::new(&self.apply(document), router)))
    }
}

/// `GET /metrics` — the scrape endpoint, reading the bound registry.
///
/// The crate-level [`rainier_metrics::endpoint`] takes the registry off the
/// request; this one resolves it from the container, which is what an
/// application wants because that is where boot put it.
pub async fn metrics_endpoint(_request: rainier_routing::Req) -> rainier_http::Response {
    let Ok(metrics) = rainier_container::facade_application().resolve::<Metrics>() else {
        return rainier_http::Response::new(rainier_http::StatusCode::NOT_FOUND);
    };

    rainier_http::Response::ok(metrics.render())
        .with_header("content-type", "text/plain; version=0.0.4; charset=utf-8")
}

/// `GET /openapi.json` — the document, reading what boot rendered.
///
/// A `404` when nothing is bound, so an application with the feature off looks
/// like one that has no such endpoint rather than one serving an empty
/// document a client would try to use.
pub async fn openapi_endpoint(_request: rainier_routing::Req) -> rainier_http::Response {
    let Ok(document) = rainier_container::facade_application().resolve::<Rendered>() else {
        return rainier_http::Response::new(rainier_http::StatusCode::NOT_FOUND);
    };

    rainier_http::Response::ok(document.json().to_string())
        .with_header("content-type", "application/json; charset=utf-8")
}

/// Read all three, for a boot that wants to report what it turned on.
pub fn settings(config: &Config) -> Result<(MetricsSettings, TelemetrySettings, OpenApiSettings)> {
    Ok((
        MetricsSettings::from_config(config),
        TelemetrySettings::from_config(config),
        OpenApiSettings::from_config(config),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new()
    }

    #[test]
    fn everything_is_off_when_nothing_is_configured() {
        // The default an application gets by not having thought about it.
        let config = config();

        assert!(!MetricsSettings::from_config(&config).enabled);
        assert!(!TelemetrySettings::from_config(&config).enabled);
        assert!(!OpenApiSettings::from_config(&config).enabled);
    }

    #[test]
    fn metrics_off_means_no_registry_and_so_no_timer() {
        let settings = MetricsSettings::from_config(&config());
        assert!(settings.registry().is_none());

        let config = config();
        config.set(keys::METRICS_ENABLED, true).unwrap();
        assert!(MetricsSettings::from_config(&config).registry().is_some());
    }

    #[test]
    fn the_paths_have_defaults_worth_having() {
        assert_eq!(MetricsSettings::from_config(&config()).path, "/metrics");
        assert_eq!(OpenApiSettings::from_config(&config()).path, "/openapi.json");
    }

    #[test]
    fn a_configured_path_wins() {
        let config = config();
        config.set(keys::METRICS_PATH, "/internal/metrics".to_string()).unwrap();

        assert_eq!(MetricsSettings::from_config(&config).path, "/internal/metrics");
    }

    #[test]
    fn tracing_can_propagate_without_exporting() {
        // The configuration worth having by default: trace ids on every log
        // line, and no collector to run.
        let config = config();
        config.set(keys::TELEMETRY_ENABLED, true).unwrap();

        let settings = TelemetrySettings::from_config(&config);

        assert!(settings.enabled);
        assert!(!settings.exports(), "no endpoint, so nothing is sent");
        assert!(settings.middleware().is_some());
    }

    #[test]
    fn an_endpoint_turns_exporting_on() {
        let config = config();
        config.set(keys::TELEMETRY_ENABLED, true).unwrap();
        config.set(keys::TELEMETRY_ENDPOINT, "http://localhost:4317".to_string()).unwrap();

        assert!(TelemetrySettings::from_config(&config).exports());
    }

    #[test]
    fn an_endpoint_alone_does_nothing_while_telemetry_is_off() {
        // Otherwise setting a collector's address in an env file would turn
        // tracing on for a deployment that never asked.
        let config = config();
        config.set(keys::TELEMETRY_ENDPOINT, "http://localhost:4317".to_string()).unwrap();

        let settings = TelemetrySettings::from_config(&config);
        assert!(!settings.exports());
        assert!(settings.middleware().is_none());
    }

    #[test]
    fn the_service_name_falls_back_to_the_application_name() {
        let config = config();
        config.set(keys::APP_NAME, "Rainier Sample".to_string()).unwrap();

        assert_eq!(TelemetrySettings::from_config(&config).service_name, "Rainier Sample");

        config.set(keys::TELEMETRY_SERVICE_NAME, "api".to_string()).unwrap();
        assert_eq!(TelemetrySettings::from_config(&config).service_name, "api");
    }

    #[test]
    fn the_document_title_falls_back_to_the_application_name() {
        let config = config();
        config.set(keys::APP_NAME, "Rainier Sample".to_string()).unwrap();

        assert_eq!(OpenApiSettings::from_config(&config).title, "Rainier Sample");
    }

    #[test]
    fn the_log_format_defaults_to_auto_and_follows_the_environment() {
        let config = Config::new();
        assert_eq!(TelemetrySettings::from_config(&config).log_format, LogFormat::Auto);

        // Which is what makes it useful: nothing to set, and production still
        // gets machine-readable lines.
        assert_eq!(LogFormat::Auto.resolve("production"), LogFormat::Json);
    }

    #[test]
    fn the_log_format_is_read_from_configuration() {
        let config = Config::new();
        config.set(keys::LOG_FORMAT.path(), "compact").unwrap();

        assert_eq!(TelemetrySettings::from_config(&config).log_format, LogFormat::Compact);
    }

    #[test]
    fn a_log_format_nobody_can_spell_falls_back_rather_than_silencing_the_logs() {
        // `LOG_FORMAT=jsonn` in the environment is refused at boot, where the
        // error can be read. A tree built by hand cannot be, so this reader
        // falls back — logging is the thing that would report the problem, and
        // must not be the thing that breaks over it.
        let config = Config::new();
        config.set(keys::LOG_FORMAT.path(), "jsonn").unwrap();

        assert_eq!(TelemetrySettings::from_config(&config).log_format, LogFormat::Auto);
    }
}
