use opentelemetry::sdk::propagation::TraceContextPropagator;
use opentelemetry::sdk::trace::Sampler;
use opentelemetry::sdk::Resource;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn, Level};
use tracing_subscriber::filter::{FilterFn, LevelFilter};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::subscribe::CollectExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Subscribe;

pub struct Spew(Mutex<SpewInner>);

struct SpewInner {
    // The number of messages suppressed since the last log.
    suppressed: usize,
    last_logged: Option<Instant>,
}

impl Spew {
    pub const fn new() -> Self {
        Self(Mutex::new(SpewInner {
            suppressed: 0,
            last_logged: None,
        }))
    }

    /// If it's time to log again, returns Some with the number of suppressed
    /// messages. Otherwise, returns None.
    pub fn ok(&self) -> Option<usize> {
        let now = Instant::now();
        let mut locked = self.0.lock().unwrap();
        let elapsed = locked
            .last_logged
            .map(|last_logged| now.saturating_duration_since(last_logged))
            .unwrap_or(Duration::MAX);
        if elapsed >= Duration::from_secs(30) {
            let were_suppressed = locked.suppressed;
            locked.suppressed = 0;
            locked.last_logged = Some(now);
            Some(were_suppressed)
        } else {
            locked.suppressed += 1;
            None
        }
    }
}

static EXPORT_SPEW: Spew = Spew::new();

pub fn configure(service_name: &str) {
    let log_level = std::env::var("LOGLEVEL")
        .map(|s| match Level::from_str(&s) {
            Ok(level) => level,
            Err(e) => panic!("failed to parse LOGLEVEL: {e}"),
        })
        .unwrap_or(Level::INFO);

    // Quiet down some libs.
    let filter = FilterFn::new(|metadata| {
        if let Some(module) = metadata.module_path() {
            if module.starts_with("h2::")
                || module.starts_with("hyper::")
                || module.starts_with("reqwest::")
                || module.starts_with("tokio_util::")
                || module.starts_with("tonic::")
                || module.starts_with("tower::")
                || module.starts_with("want::")
            {
                return false;
            }
        }
        true
    });

    let terminal = tracing_subscriber::fmt::Subscriber::new()
        .with_file(true)
        .with_line_number(true)
        .with_span_events(FmtSpan::ACTIVE)
        .with_target(false);

    // By default, opentelemetry spews pretty often to stderr when it can't
    // find a server to submit traces to. This quiets down the errors and sends
    // them to the logger.
    opentelemetry::global::set_error_handler(|e| {
        use opentelemetry::global::Error;
        use opentelemetry::trace::TraceError;
        match e {
            Error::Trace(TraceError::ExportFailed(_))
            | Error::Trace(TraceError::ExportTimedOut(_)) => {
                // These errors are unlikely to cause infinite cycles with logging.
                if let Some(suppressed) = EXPORT_SPEW.ok() {
                    warn!(
                        error = %e,
                        suppressed,
                        "opentelemetry error",
                    );
                }
            }

            _ => {
                // This goes to stderr so that it's not an infinite cycle with logging.
                eprintln!("opentelemetry error: {e}");
            }
        }
    })
    .unwrap();

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint("http://localhost:4317"),
        )
        .with_trace_config(
            opentelemetry::sdk::trace::config()
                .with_sampler(Sampler::TraceIdRatioBased(0.1))
                .with_resource(Resource::new(vec![KeyValue::new(
                    "service.name",
                    service_name.to_owned(),
                )])),
        )
        .install_batch(opentelemetry::runtime::Tokio)
        .expect("TODO");

    let telemetry = tracing_opentelemetry::subscriber().with_tracer(tracer);
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    tracing_subscriber::registry()
        .with(filter)
        .with(terminal.with_filter(LevelFilter::from_level(log_level)))
        .with(telemetry)
        .init();

    info!(
        max_level = %log_level,
        "initialized logging to terminal and telemetry to OTLP/Jaeger. you can set verbosity with env var LOGLEVEL."
    );
}

pub fn flush() {
    opentelemetry::global::shutdown_tracer_provider()
}
