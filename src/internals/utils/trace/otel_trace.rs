use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource, runtime,
    trace::{BatchConfig, RandomIdGenerator, Sampler},
};
use opentelemetry_semantic_conventions::resource::{SERVICE_NAME, SERVICE_VERSION};
use std::time::Duration;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

/// Initialize tracing with OpenTelemetry integration
/// This enables trace correlation in logs
pub fn init_tracing_with_otel(service_name: String, run_id: String) -> anyhow::Result<()> {
    let resource = Resource::new(vec![
        opentelemetry::KeyValue::new(SERVICE_NAME, service_name.clone()),
        opentelemetry::KeyValue::new("RUN_ID", run_id),
        opentelemetry::KeyValue::new(
            SERVICE_VERSION,
            std::env::var("SERVICE_VERSION").unwrap_or_else(|_| "1.0.0".to_string()),
        ),
    ]);

    // Configure OTLP exporter
    let otlp_exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(
            std::env::var("OTLP_ENDPOINT").unwrap_or_else(|_| "http://localhost:4317".to_string()),
        )
        .with_timeout(Duration::from_secs(3));

    // Build tracer provider
    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(otlp_exporter)
        .with_trace_config(
            opentelemetry_sdk::trace::Config::default()
                .with_sampler(Sampler::AlwaysOn)
                .with_id_generator(RandomIdGenerator::default())
                .with_resource(resource),
        )
        .with_batch_config(BatchConfig::default())
        .install_batch(runtime::Tokio)?;

    let tracer = tracer_provider.tracer(service_name.clone());

    // Create OpenTelemetry layer for tracing
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // Create JSON formatting layer
    let json_layer = fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .flatten_event(true)
        .with_span_events(FmtSpan::CLOSE);

    // Environment filter
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Initialize subscriber with all layers
    tracing_subscriber::registry()
        .with(env_filter)
        .with(otel_layer)
        .with(json_layer)
        .init();

    Ok(())
}

/// Shutdown OpenTelemetry, flushing pending spans
pub fn shutdown_otel() {
    opentelemetry::global::shutdown_tracer_provider();
}
