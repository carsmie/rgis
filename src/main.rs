//! The `rgis` command line: parse arguments, set up logging and serve [`rgis::http::router`].

use std::path::PathBuf;
use std::sync::Arc;

use rgis::{Server, Settings};

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
    RGIS_REMOTE_PROJECT_TTL    seconds before a /vsigs/ project is revalidated by ETag
                               (default 300; unchanged projects keep their warm caches)
    RGIS_WARM_PROJECTS         projects to load at startup (names under --projects-dir,
                               comma-separated; * = all in a local directory)
    RGIS_RENDER_BUDGET_MPX     megapixels rendered at once (default 16)
    RGIS_HTTP_CACHE_MB         remote-file block cache size (default 256)
    PGUSER, PGPASSWORD         credentials for postgres layers (override the datasource's)

    GET /health answers 200 (liveness); GET /ready answers 503 until the warm-up is done.
";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        // Plain text in containers (no ANSI colours in Loki), coloured on a terminal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
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
                return tokio::task::spawn_blocking(move || rgis::inspect(&p)).await?;
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
    let warm: Vec<String> = std::env::var("RGIS_WARM_PROJECTS")
        .unwrap_or_default()
        .split([',', ' ', '\n'])
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    if !warm.is_empty() {
        let s = server.clone();
        tokio::task::spawn_blocking(move || s.warm(&warm));
    }
    let app = rgis::http::router(server);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("rGIS listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
