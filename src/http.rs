//! The HTTP front: axum routing, health probes, CORS, and handing requests to [`Server`]
//! on blocking threads.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response as HttpResponse;

use crate::server::{Request, Server};

/// All of rGIS's HTTP endpoints, served by `server`.
pub fn router(server: Arc<Server>) -> axum::Router {
    axum::Router::new().fallback(handle).with_state(server)
}

async fn handle(
    State(server): State<Arc<Server>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> HttpResponse {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");
    if uri.path() == "/health" {
        return HttpResponse::new(Body::from("ok"));
    }
    if uri.path() == "/ready" {
        let ready = server.is_ready();
        let mut out = HttpResponse::new(Body::from(if ready { "ok" } else { "warming up" }));
        if !ready {
            *out.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        }
        return out;
    }
    if method == Method::OPTIONS {
        let mut out = HttpResponse::new(Body::empty());
        *out.status_mut() = StatusCode::NO_CONTENT;
        cors(out.headers_mut());
        return out;
    }
    let req = Request {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().unwrap_or("").to_string(),
        base_url: format!("{proto}://{host}{}", uri.path()),
    };
    // Rendering and data access are blocking work, like QGIS Server's request thread.
    // Server::handle stops between layers once the timeout passes; work inside one layer
    // (a large raster read, a whole-layer WFS response) can overrun it, so the client gets
    // its answer after a grace period while that thread finishes on its own.
    let limit = server
        .request_timeout()
        .map(|t| t + Duration::from_secs(30));
    let task = tokio::task::spawn_blocking(move || server.handle(&req));
    let resp = match limit {
        Some(limit) => match tokio::time::timeout(limit, task).await {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!("{method} {}: no response after {limit:?}", uri.path());
                let mut out = HttpResponse::new(Body::from("request timed out"));
                *out.status_mut() = StatusCode::GATEWAY_TIMEOUT;
                cors(out.headers_mut());
                return out;
            }
        },
        None => task.await,
    };
    match resp {
        Ok(r) => {
            let mut out = HttpResponse::new(Body::from(r.body));
            *out.status_mut() =
                StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            if let Ok(ct) = HeaderValue::from_str(&r.content_type) {
                out.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            out.headers_mut()
                .insert(header::SERVER, HeaderValue::from_static("rGIS"));
            cors(out.headers_mut());
            out
        }
        Err(e) => {
            tracing::error!("{method} {}: request task failed: {e}", uri.path());
            let mut out = HttpResponse::new(Body::from("Internal Server Error"));
            *out.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            out
        }
    }
}

/// CORS as the Caddy front of the tnt-qgis image sets it: web maps fetch() legends and
/// GetFeatureInfo cross-origin, and QGIS Server sends no CORS headers itself.
fn cors(h: &mut HeaderMap) {
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
}
