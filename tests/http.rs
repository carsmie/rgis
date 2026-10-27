//! The HTTP front end over a real socket: probes, CORS, and that bad input is answered
//! with an error instead of taking the process down.

use std::path::PathBuf;
use std::sync::Arc;

use rgis::{Server, Settings};

/// Serve `demo/earth_local.qgs` on an ephemeral port; returns the base URL.
async fn start() -> String {
    let server = Arc::new(Server::new(Settings {
        default_project: Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("demo/earth_local.qgs"),
        ),
        ..Default::default()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, rgis::http::router(server)).into_future());
    format!("http://{addr}/")
}

#[tokio::test]
async fn health_ready_and_cors() {
    let base = start().await;
    let c = reqwest::Client::new();
    for p in ["health", "ready"] {
        let r = c.get(format!("{base}{p}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{p}");
    }
    let r = c
        .request(reqwest::Method::OPTIONS, &base)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert_eq!(r.headers()["access-control-allow-origin"], "*");

    let r = c
        .get(format!("{base}?SERVICE=WMS&REQUEST=GetCapabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["server"], "rGIS");
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
    assert!(r.text().await.unwrap().contains("<Name>Earth</Name>"));
}

#[tokio::test]
async fn hostile_filters_get_an_error_not_a_crash() {
    let base = start().await;
    let c = reqwest::Client::new();
    let wms = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=ne_10m_admin_0_countries\
               &CRS=EPSG:4326&BBOX=-90,-180,90,180&WIDTH=64&HEIGHT=32&FORMAT=image/png";
    let nested = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
    let r = c
        .get(format!(
            "{base}?{wms}&FILTER=ne_10m_admin_0_countries:{}",
            urlencoding::encode(&nested)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("nested more than"));

    // Exponential for a backtracking LIKE; must finish promptly.
    let like = format!("\"NAME\" LIKE '{}zz'", "%".repeat(40));
    let started = std::time::Instant::now();
    let r = c
        .get(format!(
            "{base}?{wms}&FILTER=ne_10m_admin_0_countries:{}",
            urlencoding::encode(&like)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(started.elapsed().as_secs() < 10);

    let r = c.get(format!("{base}health")).send().await.unwrap();
    assert_eq!(r.status(), 200);
}
