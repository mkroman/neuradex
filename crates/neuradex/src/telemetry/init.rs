//! OpenTelemetry traces, logs, and resource metadata.
//!
//! The service always creates OpenTelemetry spans and log records, so trace and span IDs are
//! available for correlation. OTLP export is enabled by the standard `OTEL_*` environment
//! variables and is off when no endpoint is configured, which keeps local runs quiet.
//!
//! Resource attributes come from the standard `OTEL_SERVICE_NAME` and
//! `OTEL_RESOURCE_ATTRIBUTES` variables, plus the runtime resource detectors. `service.name`
//! defaults to `neuradex` and `service.version` to this binary's package version when the
//! environment does not set them.

use std::env;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{
    LogExporter, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_LOGS_ENDPOINT,
    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT, Protocol, SpanExporter, WithExportConfig,
};
use opentelemetry_resource_detectors::{
    ContainerResourceDetector, HostResourceDetector, K8sResourceDetector, OsResourceDetector,
    ProcessResourceDetector, ServiceInstanceIdResourceDetector,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::resource::{EnvResourceDetector, TelemetryResourceDetector};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_semantic_conventions::SCHEMA_URL;
use opentelemetry_semantic_conventions::resource::{SERVICE_NAME, SERVICE_VERSION};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::Registry;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// The OTel crates keep the resource/exporter environment names private or export them from
// `opentelemetry_otlp`; only the names below are not available from a dependency.
/// The standard variable that disables the OpenTelemetry SDK.
const OTEL_SDK_DISABLED: &str = "OTEL_SDK_DISABLED";
/// The standard variable that sets `service.name`.
const OTEL_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
/// The standard variable that carries additional resource attributes.
const OTEL_RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";
/// The standard variable selecting the trace exporter.
const OTEL_TRACES_EXPORTER: &str = "OTEL_TRACES_EXPORTER";
/// The standard variable selecting the log exporter.
const OTEL_LOGS_EXPORTER: &str = "OTEL_LOGS_EXPORTER";

/// The OpenTelemetry providers that must outlive the subscriber and be shut down explicitly.
pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl Telemetry {
    /// Flushes buffered spans and log records and stops the export pipelines.
    pub fn shutdown(&self) {
        if let Some(provider) = &self.tracer_provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(
                "error.type" = "otel_shutdown",
                "exception.message" = %error,
                "could not shut down the OpenTelemetry tracer provider"
            );
        }

        if let Some(provider) = &self.logger_provider
            && let Err(error) = provider.shutdown()
        {
            tracing::warn!(
                "error.type" = "otel_shutdown",
                "exception.message" = %error,
                "could not shut down the OpenTelemetry logger provider"
            );
        }
    }
}

/// Initializes the tracing subscriber and the OpenTelemetry pipelines.
///
/// `json` selects the local formatter; OpenTelemetry export is configured independently by
/// the standard `OTEL_*` environment variables.
///
/// # Errors
///
/// Returns an error if the subscriber is already installed or an OTLP exporter cannot be
/// built from the configured environment.
pub fn init(json: bool) -> Result<Telemetry, Box<dyn std::error::Error>> {
    let (tracer_provider, logger_provider) = if sdk_disabled() {
        (None, None)
    } else {
        let resource = resource();

        (
            Some(tracer_provider(resource.clone())?),
            Some(logger_provider(resource)?),
        )
    };

    // Events are exported as OTel log records by the appender. Sending them again as
    // OTel span events would duplicate the same information in traces and logs.
    let otel_layer = tracer_provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("neuradex"))
            .with_filter(filter_fn(spans_only))
    });
    let log_layer = logger_provider
        .as_ref()
        .map(OpenTelemetryTracingBridge::new);

    Registry::default()
        .with(env_filter())
        .with(fmt_layer(json))
        .with(otel_layer)
        .with(log_layer)
        .try_init()?;

    Ok(Telemetry {
        tracer_provider,
        logger_provider,
    })
}

/// Builds the local formatter layer, JSON or plain text.
fn fmt_layer<S>(json: bool) -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber
        + for<'span> tracing_subscriber::registry::LookupSpan<'span>
        + Send
        + Sync
        + 'static,
{
    if json {
        fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_span_events(FmtSpan::CLOSE)
            .boxed()
    } else {
        fmt::layer().with_span_events(FmtSpan::CLOSE).boxed()
    }
}

/// Keeps spans in the trace pipeline; events are handled by the OTel log appender.
fn spans_only(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.is_span()
}

/// Builds the log filter from `RUST_LOG`, defaulting to `info`.
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Builds the tracer provider, adding an OTLP exporter when one is configured.
fn tracer_provider(resource: Resource) -> Result<SdkTracerProvider, Box<dyn std::error::Error>> {
    let builder = SdkTracerProvider::builder().with_resource(resource);

    if !signal_export_enabled(OTEL_TRACES_EXPORTER, OTEL_EXPORTER_OTLP_TRACES_ENDPOINT) {
        return Ok(builder.build());
    }

    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()?;

    Ok(builder.with_batch_exporter(exporter).build())
}

/// Builds the logger provider, adding an OTLP exporter when one is configured.
fn logger_provider(resource: Resource) -> Result<SdkLoggerProvider, Box<dyn std::error::Error>> {
    let builder = SdkLoggerProvider::builder().with_resource(resource);

    if !signal_export_enabled(OTEL_LOGS_EXPORTER, OTEL_EXPORTER_OTLP_LOGS_ENDPOINT) {
        return Ok(builder.build());
    }

    let exporter = LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()?;

    Ok(builder.with_batch_exporter(exporter).build())
}

/// Builds the resource from runtime detectors, the environment, and service defaults.
fn resource() -> Resource {
    let mut builder = Resource::builder_empty()
        .with_detectors(&[
            Box::new(OsResourceDetector),
            Box::new(ProcessResourceDetector),
            Box::new(HostResourceDetector::default()),
            Box::new(K8sResourceDetector),
            Box::new(ContainerResourceDetector),
            Box::new(ServiceInstanceIdResourceDetector),
        ])
        // Environment values take precedence over detected runtime values.
        .with_detector(Box::new(EnvResourceDetector::new()))
        .with_detector(Box::new(TelemetryResourceDetector))
        .with_schema_url([], SCHEMA_URL);

    if !resource_attribute_is_set(SERVICE_NAME) {
        builder = builder.with_attribute(KeyValue::new(SERVICE_NAME, "neuradex"));
    }

    if !resource_attribute_is_set(SERVICE_VERSION) {
        builder = builder.with_attribute(KeyValue::new(SERVICE_VERSION, env!("CARGO_PKG_VERSION")));
    }

    builder.build()
}

/// Returns whether the SDK is disabled by `OTEL_SDK_DISABLED`.
fn sdk_disabled() -> bool {
    env_value(OTEL_SDK_DISABLED).is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// Returns whether a signal should be exported over OTLP.
///
/// Export needs an endpoint; the generic endpoint covers both signals unless the signal's
/// exporter is explicitly set to `none`.
fn signal_export_enabled(exporter_env: &str, endpoint_env: &str) -> bool {
    if env_value(exporter_env).is_some_and(|value| value.eq_ignore_ascii_case("none")) {
        return false;
    }

    env_is_set(OTEL_EXPORTER_OTLP_ENDPOINT) || env_is_set(endpoint_env)
}

/// Returns whether an environment variable is set to a non-empty, trimmed value.
fn env_is_set(name: &str) -> bool {
    env_value(name).is_some()
}

/// Returns the trimmed, non-empty value of an environment variable.
fn env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Returns whether `name` is present in the resource environment.
fn resource_attribute_is_set(name: &str) -> bool {
    if name == SERVICE_NAME && env_is_set(OTEL_SERVICE_NAME) {
        return true;
    }

    env::var(OTEL_RESOURCE_ATTRIBUTES)
        .is_ok_and(|attributes| contains_resource_attribute(&attributes, name))
}

/// Returns whether a comma-separated `OTEL_RESOURCE_ATTRIBUTES` value contains `name`.
fn contains_resource_attribute(attributes: &str, name: &str) -> bool {
    attributes.split(',').any(|entry| {
        entry
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_resource_attribute_keys() {
        let attributes = "service.version=1.2.3, deployment.environment.name=production";

        assert!(contains_resource_attribute(attributes, SERVICE_VERSION));
        assert!(contains_resource_attribute(
            attributes,
            opentelemetry_semantic_conventions::resource::DEPLOYMENT_ENVIRONMENT_NAME
        ));
        assert!(!contains_resource_attribute(attributes, SERVICE_NAME));
        assert!(!contains_resource_attribute("not-a-pair", SERVICE_VERSION));
    }

    #[test]
    fn resource_has_a_schema_url() {
        assert_eq!(resource().schema_url(), Some(SCHEMA_URL));
    }
}
