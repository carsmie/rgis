//! Prometheus metrics. Served on their own port (RGIS_METRICS_ADDR), never the service port,
//! which is behind a public ingress. Label values are fixed sets or come from the project
//! (layer names): client input never becomes a label, or anyone could mint unlimited series.

use std::sync::LazyLock;
use std::time::Duration;

use prometheus::{
    Histogram, HistogramVec, IntCounter, IntCounterVec, IntGauge, histogram_opts,
    register_histogram, register_histogram_vec, register_int_counter, register_int_counter_vec,
    register_int_gauge,
};

const REQUEST_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

static REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rgis_requests_total",
        "OWS requests by service, request and HTTP status",
        &["service", "request", "status"]
    )
    .expect("static metric")
});
static REQUEST_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        histogram_opts!(
            "rgis_request_duration_seconds",
            "OWS request duration",
            REQUEST_BUCKETS.to_vec()
        ),
        &["service", "request"]
    )
    .expect("static metric")
});
static RESPONSE_BYTES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rgis_response_bytes_total",
        "Response body bytes",
        &["service", "request"]
    )
    .expect("static metric")
});
static IN_FLIGHT: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!("rgis_requests_in_flight", "OWS requests being handled")
        .expect("static metric")
});
static OVERRUNS: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "rgis_request_overruns_total",
        "Clients answered 504 while their render thread kept running"
    )
    .expect("static metric")
});

// Counters, not a histogram: a topo deployment has hundreds of layers, and buckets × layers
// is real series load. Sum and count give the average, which is what finds the slow layer.
static LAYER_SECONDS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rgis_layer_render_microseconds_total",
        "Time reading and drawing each layer",
        &["layer"]
    )
    .expect("static metric")
});
static LAYER_RENDERS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rgis_layer_renders_total",
        "Renders of each layer",
        &["layer"]
    )
    .expect("static metric")
});
static LAYER_FEATURES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rgis_layer_features_total",
        "Features drawn per vector layer",
        &["layer"]
    )
    .expect("static metric")
});
static GATE_WAIT: LazyLock<Histogram> = LazyLock::new(|| {
    register_histogram!(histogram_opts!(
        "rgis_render_gate_wait_seconds",
        "Time waiting for RGIS_RENDER_BUDGET_MPX",
        vec![0.001, 0.01, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0]
    ))
    .expect("static metric")
});

static REMOTE_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rgis_remote_requests_total",
        "HTTP attempts on remote files: ok, retry (failed, will retry), error (gave up)",
        &["result"]
    )
    .expect("static metric")
});
/// Bytes read from remote files: with GCS this is the egress bill.
static REMOTE_BYTES: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!("rgis_remote_bytes_total", "Bytes read from remote files")
        .expect("static metric")
});
static BLOCKS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rgis_block_cache_requests_total",
        "Remote-file block cache lookups",
        &["result"]
    )
    .expect("static metric")
});
// Resolved once: the block read path is hot enough to skip the label lookup.
static BLOCK_HIT: LazyLock<IntCounter> = LazyLock::new(|| BLOCKS.with_label_values(&["hit"]));
static BLOCK_MISS: LazyLock<IntCounter> = LazyLock::new(|| BLOCKS.with_label_values(&["miss"]));
static BLOCK_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!("rgis_block_cache_bytes", "Bytes held in the block cache")
        .expect("static metric")
});

/// SERVICE as a label: client input, so anything unknown collapses to `other`.
fn service_label(service: &str) -> &'static str {
    match service {
        "WMS" => "WMS",
        "WFS" => "WFS",
        "" => "none",
        _ => "other",
    }
}

/// REQUEST as a label: client input, so anything unknown collapses to `other`.
fn request_label(request: &str) -> &'static str {
    const KNOWN: [&str; 7] = [
        "GetCapabilities",
        "GetMap",
        "GetFeatureInfo",
        "GetLegendGraphic",
        "GetStyles",
        "GetFeature",
        "DescribeFeatureType",
    ];
    if request.is_empty() {
        return "none";
    }
    KNOWN
        .into_iter()
        .find(|k| k.eq_ignore_ascii_case(request))
        .unwrap_or("other")
}

/// Counts a request as in flight until dropped, panics included.
pub struct InFlight(());

pub fn in_flight() -> InFlight {
    IN_FLIGHT.inc();
    InFlight(())
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.dec();
    }
}

pub fn request(service: &str, request: &str, status: u16, took: Duration, bytes: usize) {
    let (service, request) = (service_label(service), request_label(request));
    REQUESTS
        .with_label_values(&[service, request, status.to_string().as_str()])
        .inc();
    REQUEST_SECONDS
        .with_label_values(&[service, request])
        .observe(took.as_secs_f64());
    RESPONSE_BYTES
        .with_label_values(&[service, request])
        .inc_by(bytes as u64);
}

pub fn overrun() {
    OVERRUNS.inc();
}

pub fn layer_rendered(layer: &str, features: usize, took: Duration) {
    LAYER_SECONDS
        .with_label_values(&[layer])
        .inc_by(took.as_micros() as u64);
    LAYER_RENDERS.with_label_values(&[layer]).inc();
    if features > 0 {
        LAYER_FEATURES
            .with_label_values(&[layer])
            .inc_by(features as u64);
    }
}

pub fn gate_waited(took: Duration) {
    GATE_WAIT.observe(took.as_secs_f64());
}

/// `result`: ok, retry or error.
pub fn remote_request(result: &'static str) {
    REMOTE_REQUESTS.with_label_values(&[result]).inc();
}

pub fn remote_bytes(n: usize) {
    REMOTE_BYTES.inc_by(n as u64);
}

pub fn block_lookup(hit: bool) {
    if hit { &BLOCK_HIT } else { &BLOCK_MISS }.inc();
}

pub fn block_cache_size(bytes: usize) {
    BLOCK_BYTES.set(bytes as i64);
}

/// The metrics endpoint: `GET /metrics` in Prometheus text format.
pub fn router() -> axum::Router {
    axum::Router::new().route(
        "/metrics",
        axum::routing::get(|| async {
            use axum::response::IntoResponse;
            match prometheus::TextEncoder::new().encode_to_string(&prometheus::gather()) {
                Ok(text) => ([("content-type", prometheus::TEXT_FORMAT)], text).into_response(),
                Err(e) => {
                    tracing::warn!("metrics encoding failed: {e}");
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_input_never_becomes_a_label() {
        assert_eq!(request_label("getmap"), "GetMap");
        assert_eq!(request_label("GETMAP"), "GetMap");
        assert_eq!(request_label("x'; DROP"), "other");
        assert_eq!(request_label(""), "none");
        assert_eq!(service_label("WMS"), "WMS");
        assert_eq!(service_label("<script>"), "other");
        assert_eq!(service_label(""), "none");
    }
}
