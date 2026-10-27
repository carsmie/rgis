//! rGIS against responses captured from QGIS Server (`evidence/qgis-reference/`) for the
//! same project: `demo/earth_local*.qgs` is `earth.qgs` with its FlatGeobuf data local.
//! Tools in `tools/` compare against a live QGIS Server; these run anywhere, including CI.

use std::path::PathBuf;

use rgis::{Request, Response, Server, Settings};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn server() -> Server {
    Server::new(Settings {
        fixed_project: None,
        default_project: Some(root().join("demo/earth_local.qgs")),
        projects_dir: None,
    })
}

fn get(server: &Server, query: &str) -> Response {
    server.handle(&Request {
        method: "GET".into(),
        path: "/".into(),
        query: query.into(),
        base_url: "http://localhost/".into(),
    })
}

fn wfs_map() -> String {
    format!("MAP={}", root().join("demo/earth_local_wfs.qgs").display())
}

fn reference(name: &str) -> Vec<u8> {
    std::fs::read(root().join("evidence/qgis-reference").join(name)).unwrap()
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap()
}

fn over_white(png: &[u8]) -> tiny_skia::Pixmap {
    let img = tiny_skia::Pixmap::decode_png(png).unwrap();
    let mut out = tiny_skia::Pixmap::new(img.width(), img.height()).unwrap();
    out.fill(tiny_skia::Color::WHITE);
    out.draw_pixmap(
        0,
        0,
        img.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
    out
}

/// Share of pixels where any channel differs by more than 64/255 once both images are
/// composited over white, as `examples/compare_png.rs` and `tools/parity.py` count them.
fn differing(a: &[u8], b: &[u8]) -> f64 {
    let (a, b) = (over_white(a), over_white(b));
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    let n = a
        .data()
        .chunks(4)
        .zip(b.data().chunks(4))
        .filter(|(p, q)| p.iter().zip(*q).any(|(x, y)| x.abs_diff(*y) > 64))
        .count();
    n as f64 / (a.width() * a.height()) as f64
}

#[test]
fn getmap_world_epsg4326_matches_qgis() {
    // WMS 1.3.0 with EPSG:4326: BBOX in lat/lon order.
    let r = get(
        &server(),
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:4326\
         &BBOX=-90,-180,90,180&WIDTH=800&HEIGHT=400&FORMAT=image/png",
    );
    assert_eq!((r.status, r.content_type.as_str()), (200, "image/png"));
    let d = differing(&r.body, &reference("getmap_world_4326.png"));
    assert!(d < 0.006, "{:.2} % of pixels differ", d * 100.0);
}

#[test]
fn getmap_europe_epsg3857_matches_qgis() {
    let r = get(
        &server(),
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=Earth&CRS=EPSG:3857\
         &BBOX=-1500000,4000000,4500000,11000000&WIDTH=600&HEIGHT=700&FORMAT=image/png",
    );
    assert_eq!(r.status, 200);
    let d = differing(&r.body, &reference("getmap_europe_3857.png"));
    assert!(d < 0.003, "{:.2} % of pixels differ", d * 100.0);
}

#[test]
fn getfeatureinfo_is_byte_identical() {
    let r = get(
        &server(),
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=Earth\
         &QUERY_LAYERS=ne_10m_admin_0_countries&CRS=EPSG:4326&BBOX=50,0,70,20\
         &WIDTH=200&HEIGHT=200&I=100&J=100&INFO_FORMAT=application/json",
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        String::from_utf8(r.body).unwrap(),
        String::from_utf8(reference("getfeatureinfo.json")).unwrap()
    );
}

#[test]
fn wfs_getfeature_matches_qgis() {
    let s = server();
    // Only WFSLayers are published; MAXFEATURES and GeoJSON rounding as QGIS.
    let r = get(
        &s,
        &format!(
            "{}&SERVICE=WFS&VERSION=1.1.0&REQUEST=GetFeature&TYPENAME=ne_10m_lakes\
             &MAXFEATURES=2&OUTPUTFORMAT=application/json",
            wfs_map()
        ),
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        json(&r.body),
        json(&reference("wfs_getfeature_published.json"))
    );
    // KVP BBOX in lon/lat even for EPSG:4326; PROPERTYNAME without geometry → null.
    let r = get(
        &s,
        &format!(
            "{}&SERVICE=WFS&VERSION=1.1.0&REQUEST=GetFeature&TYPENAME=ne_10m_admin_0_countries\
             &BBOX=5,58,10,62&PROPERTYNAME=NAME&OUTPUTFORMAT=application/json",
            wfs_map()
        ),
    );
    assert_eq!(r.status, 200);
    assert_eq!(json(&r.body), json(&reference("wfs_getfeature_bbox.json")));
}

#[test]
fn unknown_layer_is_the_same_exception() {
    let r = get(
        &server(),
        "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=nope&CRS=EPSG:4326\
         &BBOX=0,0,1,1&WIDTH=10&HEIGHT=10",
    );
    assert_eq!(r.status, 400);
    assert_eq!(r.body, reference("error_layer_not_defined.xml"));
}
