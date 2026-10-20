//! rGIS: a QGIS-Server-like OGC map server in Rust that serves QGIS
//! project files (.qgs/.qgz) over WMS, WFS and OGC API Features.

mod crs;
mod expression;
mod geom;
mod labeling;
mod params;
mod project;
mod provider;
mod raster;
mod render;
mod server;
mod services;
mod svg;
mod symbology;
mod text;

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response as HttpResponse;

use crate::project::{Project, TreeGroup, TreeNode};
use crate::server::{Request, Server, Settings};

const USAGE: &str = "\
rGIS – serve QGIS projects over WMS / WFS / OGC API Features

USAGE:
    rgis [OPTIONS] [ADDRESS]          (default address 127.0.0.1:8000)
    rgis --inspect <PROJECT>          print how a project is understood

OPTIONS:
    -p, --project <FILE>       default project when no MAP parameter is given
        --projects-dir <DIR>   only allow MAP to point inside this directory
                               (or under a /vsigs/bucket/prefix/ on GCS)
    -h, --help                 show this help

ENVIRONMENT:
    QGIS_PROJECT_FILE          fixed project, overrides MAP (as in QGIS Server)
    RUST_LOG                   log filter, e.g. info or rgis=debug
    GS_ACCESS_KEY_ID, GS_SECRET_ACCESS_KEY
                               HMAC keys for /vsigs/ data and projects (as GDAL)
    GDAL_HTTP_HEADERS          extra headers for /vsigs/ without HMAC keys,
                               e.g. \"Authorization: Bearer <token>\"
    CPL_GS_ENDPOINT            GCS endpoint (default https://storage.googleapis.com/)
    RGIS_REMOTE_PROJECT_TTL    seconds a /vsigs/ project is cached (default 300)

    GET /health answers 200 (liveness/readiness probes).
";

fn print_tree(p: &Project, g: &TreeGroup, depth: usize) {
    for c in &g.children {
        let ind = "  ".repeat(depth);
        match c {
            TreeNode::Group(sub) => {
                println!("{ind}[group] {} (WMS name '{}')", sub.name, sub.nickname());
                print_tree(p, sub, depth + 1);
            }
            TreeNode::Layer { id, .. } => match p.layers.get(id) {
                Some(l) => {
                    let provider = match l.provider() {
                        Ok(pr) => format!("{} ({} fields)", pr.description, pr.fields.len()),
                        Err(e) => format!("INVALID: {e:#}"),
                    };
                    println!(
                        "{ind}[layer] {} (WMS name '{}', {}, {})",
                        l.name,
                        p.nickname(l),
                        l.geometry,
                        l.crs().authid
                    );
                    println!("{ind}        source:   {}", l.datasource);
                    println!("{ind}        provider: {provider}");
                    println!(
                        "{ind}        renderer: {} legend item(s)",
                        l.renderer.legend().len()
                    );
                }
                None => println!("{ind}[layer] {id} (missing from <projectlayers>)"),
            },
        }
    }
}

fn inspect(path: &str) -> anyhow::Result<()> {
    let p = Project::read(std::path::Path::new(path))?;
    println!("Project:   {}", p.path.display());
    println!("Title:     {}", p.title);
    println!("CRS:       {}", p.crs.authid);
    println!("WMS root:  {}", p.root_name());
    println!("CRS list:  {}", p.crs_list().join(", "));
    println!(
        "WFS layers: {}",
        p.wfs_layers()
            .iter()
            .map(|l| p.nickname(l))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Layer tree:");
    print_tree(&p, &p.tree, 1);
    Ok(())
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
    let resp = tokio::task::spawn_blocking(move || server.handle(&req)).await;
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
            let mut out = HttpResponse::new(Body::from(format!("Internal Server Error: {e}")));
            *out.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            out
        }
    }
}

/// CORS as the Caddy front of the tnt-qgis image sets it: web maps fetch() legends and
/// GetFeatureInfo cross-origin, and QGIS Server sends no CORS headers itself.
fn cors(h: &mut HeaderMap) {
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let mut addr = "127.0.0.1:8000".to_string();
    let mut default_project = None;
    let mut projects_dir = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--inspect" => {
                let p = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--inspect needs a project file"))?;
                return tokio::task::spawn_blocking(move || inspect(&p)).await?;
            }
            "-p" | "--project" => {
                default_project = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("-p needs a file"))?,
                ))
            }
            "--projects-dir" => {
                projects_dir =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        anyhow::anyhow!("--projects-dir needs a directory")
                    })?))
            }
            other if !other.starts_with('-') => addr = other.to_string(),
            other => anyhow::bail!("unknown option {other}\n\n{USAGE}"),
        }
    }
    let settings = Settings {
        fixed_project: std::env::var_os("QGIS_PROJECT_FILE")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty()),
        default_project,
        projects_dir,
    };
    let server = Arc::new(Server::new(settings));
    let app = axum::Router::new().fallback(handle).with_state(server);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("rGIS listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
