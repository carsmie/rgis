//! Request timeouts and what error responses reveal to clients.

use std::path::PathBuf;
use std::time::Duration;

use rgis::{Request, Response, Server, Settings};

fn demo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("demo/earth_local.qgs")
}

fn get(server: &Server, query: &str) -> Response {
    server.handle(&Request {
        method: "GET".into(),
        path: "/".into(),
        query: query.into(),
        base_url: "http://localhost/".into(),
    })
}

const GETMAP: &str = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:4326\
                      &BBOX=-90,-180,90,180&WIDTH=200&HEIGHT=100&FORMAT=image/png";

#[test]
fn requests_past_their_deadline_stop_with_503() {
    let s = Server::new(Settings {
        default_project: Some(demo()),
        request_timeout: Some(Duration::ZERO),
        ..Default::default()
    });
    let r = get(&s, GETMAP);
    assert_eq!(r.status, 503);
    assert!(
        String::from_utf8(r.body)
            .unwrap()
            .contains("RequestTimeout")
    );

    let s = Server::new(Settings {
        default_project: Some(demo()),
        request_timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    });
    assert_eq!(get(&s, GETMAP).status, 200);
}

#[test]
fn project_errors_do_not_reveal_the_cause() {
    let s = Server::new(Settings::default());
    let body = |map: &str| {
        let r = get(
            &s,
            &format!("MAP={map}&SERVICE=WMS&REQUEST=GetCapabilities"),
        );
        assert_eq!(r.status, 400, "{map}");
        String::from_utf8(r.body).unwrap().replace(map, "MAP")
    };
    // A missing file, an existing non-project file and a directory answer alike.
    let missing = body("/nonexistent/x.qgs");
    assert!(missing.contains("cannot load project 'MAP'"), "{missing}");
    assert!(!missing.contains("os error"), "{missing}");
    assert_eq!(body("/etc/hosts"), missing);
    assert_eq!(body("/tmp"), missing);
}
