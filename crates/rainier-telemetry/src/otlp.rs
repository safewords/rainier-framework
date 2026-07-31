//! The OTLP exporter — spans to a collector.
//!
//! Behind the `otlp` feature, because it is a large dependency tree and an
//! application that only wants [trace headers propagated](crate::Trace) should
//! not pay for it.

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use opentelemetry_sdk::Resource;

use rainier_support::{Error, Result};

/// Where spans go, and what this service calls itself.
#[derive(Debug, Clone)]
pub struct Otlp {
    endpoint: String,
    service_name: String,
    service_version: Option<String>,
    environment: Option<String>,
    timeout: Duration,
    sample_ratio: f64,
}

impl Otlp {
    /// Export to `endpoint`, as `service_name`.
    ///
    /// The endpoint is the collector's OTLP gRPC address —
    /// `http://localhost:4317` for a local collector, or whatever your vendor
    /// gave you.
    pub fn new(endpoint: impl Into<String>, service_name: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            service_name: service_name.into(),
            service_version: None,
            environment: None,
            timeout: Duration::from_secs(3),
            sample_ratio: 1.0,
        }
    }

    /// The version of this service, so a trace can be attributed to a deploy.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.service_version = Some(version.into());
        self
    }

    /// `production`, `staging` — whatever separates one deployment from
    /// another in your collector.
    pub fn environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }

    /// How long to wait for the collector before giving up on a batch.
    ///
    /// Short on purpose. A collector that is down must not become this
    /// service's latency, and a dropped span is a smaller problem than a
    /// dropped request.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// What fraction of traces to record, from `0.0` to `1.0`.
    ///
    /// Applies only to traces this service **starts**: one that arrives with a
    /// sampling decision keeps it, because a trace sampled in half its services
    /// is a trace with holes in it.
    pub fn sample_ratio(mut self, ratio: f64) -> Self {
        self.sample_ratio = ratio.clamp(0.0, 1.0);
        self
    }

    /// Build the tracer provider.
    ///
    /// Batched rather than one span per request: a span per network round trip
    /// would add the collector's latency to every request, which is the
    /// classic way tracing gets switched off in production.
    pub fn build(&self) -> Result<TracerProvider> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&self.endpoint)
            .with_timeout(self.timeout)
            .build()
            .map_err(|e| Error::internal(format!("could not build the OTLP exporter: {e}")))?;

        let mut attributes = vec![KeyValue::new("service.name", self.service_name.clone())];
        if let Some(version) = &self.service_version {
            attributes.push(KeyValue::new("service.version", version.clone()));
        }
        if let Some(environment) = &self.environment {
            attributes.push(KeyValue::new("deployment.environment", environment.clone()));
        }

        let sampler = if self.sample_ratio >= 1.0 {
            opentelemetry_sdk::trace::Sampler::AlwaysOn
        } else if self.sample_ratio <= 0.0 {
            opentelemetry_sdk::trace::Sampler::AlwaysOff
        } else {
            // `ParentBased` so an upstream decision wins, and the ratio only
            // decides for traces that start here.
            opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
                opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(self.sample_ratio),
            ))
        };

        Ok(TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_sampler(sampler)
            .with_resource(Resource::new(attributes))
            .build())
    }

    /// A `tracing` layer that sends spans to the collector.
    ///
    /// Add it to the subscriber the application already installs, so one
    /// `tracing::info_span!` becomes both a log line and a span.
    ///
    /// ```ignore
    /// use tracing_subscriber::prelude::*;
    ///
    /// tracing_subscriber::registry()
    ///     .with(tracing_subscriber::fmt::layer())
    ///     .with(otlp.layer()?)
    ///     .init();
    /// ```
    pub fn layer<S>(
        &self,
    ) -> Result<tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>>
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        let provider = self.build()?;
        let tracer = provider.tracer(self.service_name.clone());

        // Registered globally so `shutdown` can flush it, and so anything else
        // in the process that asks for a tracer gets this one.
        opentelemetry::global::set_tracer_provider(provider);

        Ok(tracing_opentelemetry::layer().with_tracer(tracer))
    }
}

/// Flush whatever has not been sent.
///
/// Call it before the process exits. A batch exporter holds spans for a few
/// seconds by design, and without this the last few seconds of a shutdown —
/// which is often the part you wanted to see — never leave.
pub fn shutdown() {
    opentelemetry::global::shutdown_tracer_provider();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ratio_outside_the_range_is_clamped_rather_than_believed() {
        assert_eq!(Otlp::new("http://localhost:4317", "test").sample_ratio(5.0).sample_ratio, 1.0);
        assert_eq!(Otlp::new("http://localhost:4317", "test").sample_ratio(-1.0).sample_ratio, 0.0);
    }

    #[test]
    fn the_defaults_are_the_safe_ones() {
        let otlp = Otlp::new("http://localhost:4317", "test");

        assert_eq!(otlp.sample_ratio, 1.0, "sample everything until told otherwise");
        assert_eq!(otlp.timeout, Duration::from_secs(3), "a collector must not become latency");
    }

    #[test]
    fn the_resource_attributes_are_optional() {
        let otlp =
            Otlp::new("http://localhost:4317", "test").version("1.2.3").environment("production");

        assert_eq!(otlp.service_version.as_deref(), Some("1.2.3"));
        assert_eq!(otlp.environment.as_deref(), Some("production"));
    }
}
