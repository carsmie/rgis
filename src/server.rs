//! The request pipeline, mirroring QgsServer::handleRequest:
//! parse parameters → resolve the project path → load it through the config cache →
//! dispatch to an OGC API (no SERVICE) or to a registered OWS service module.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::params::Params;
use crate::project::Project;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    /// Scheme + host + path, used to build OnlineResource URLs.
    pub base_url: String,
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status: 200,
            content_type: content_type.to_string(),
            body: body.into(),
        }
    }
    pub fn json(v: &serde_json::Value) -> Response {
        Response::ok(
            "application/json",
            serde_json::to_vec(v).unwrap_or_default(),
        )
    }
}

/// An OGC service exception (QgsServiceException / QgsBadRequestException).
#[derive(Debug)]
pub struct OwsError {
    pub code: String,
    pub message: String,
    pub status: u16,
}

impl OwsError {
    pub fn new(code: &str, message: impl Into<String>, status: u16) -> OwsError {
        OwsError {
            code: code.to_string(),
            message: message.into(),
            status,
        }
    }
    pub fn bad(code: &str, message: impl Into<String>) -> OwsError {
        OwsError::new(code, message, 400)
    }
    pub fn internal(e: impl std::fmt::Display) -> OwsError {
        OwsError::new("InternalServerError", e.to_string(), 500)
    }
    pub fn to_response(&self, version: &str) -> Response {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ServiceExceptionReport version=\"{version}\" xmlns=\"http://www.opengis.net/ogc\">\n <ServiceException code=\"{}\">{}</ServiceException>\n</ServiceExceptionReport>\n",
            esc(&self.code),
            esc(&self.message)
        );
        Response {
            status: self.status,
            content_type: "text/xml; charset=utf-8".into(),
            body: body.into_bytes(),
        }
    }
}

impl From<anyhow::Error> for OwsError {
    fn from(e: anyhow::Error) -> Self {
        OwsError::internal(format!("{e:#}"))
    }
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

/// What a service module gets for one request.
pub struct Context<'a> {
    pub params: &'a Params,
    pub project: &'a Project,
    /// Base URL for links in capabilities, ending in '?' or '&'.
    pub service_url: String,
}

/// An OWS service module (QgsService); WMS and WFS register themselves per version.
pub trait Service: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;
    fn execute(&self, ctx: &Context) -> Result<Response, OwsError>;
}

/// QgsServiceRegistry: services keyed by (NAME, version) with a default version per name.
#[derive(Default)]
pub struct ServiceRegistry {
    services: HashMap<(String, String), Arc<dyn Service>>,
    defaults: HashMap<String, String>,
}

impl ServiceRegistry {
    pub fn register(&mut self, s: Arc<dyn Service>) {
        let (name, version) = (s.name().to_string(), s.version().to_string());
        info!("Adding service {name} {version}");
        let newer = self
            .defaults
            .get(&name)
            .is_none_or(|d| version_key(&version) > version_key(d));
        if newer {
            self.defaults.insert(name.clone(), version.clone());
        }
        self.services.insert((name, version), s);
    }

    pub fn get(&self, name: &str, version: Option<&str>) -> Option<Arc<dyn Service>> {
        let default = self.defaults.get(name)?;
        let v = version.unwrap_or(default);
        match self.services.get(&(name.to_string(), v.to_string())) {
            Some(s) => Some(s.clone()),
            None => {
                warn!("Service {name} {v} not found, returning default");
                self.services
                    .get(&(name.to_string(), default.clone()))
                    .cloned()
            }
        }
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.defaults.keys().cloned().collect();
        v.sort();
        v
    }
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// A /vsigs/ project in the config cache: (last checked, ETag, project).
type RemoteProject = (Instant, Option<String>, Arc<Project>);

/// QgsConfigCache with the 'filesystem' strategy, done by comparing modification times.
#[derive(Default)]
pub struct ConfigCache {
    projects: Mutex<HashMap<PathBuf, Arc<Project>>>,
    /// Projects on GCS (`MAP=/vsigs/…`): fetched to a local copy and revalidated by ETag
    /// after RGIS_REMOTE_PROJECT_TTL seconds (default 300): (checked at, ETag, project).
    remote: Mutex<HashMap<String, RemoteProject>>,
}

impl ConfigCache {
    pub fn project(&self, path: &Path) -> anyhow::Result<Arc<Project>> {
        if let Some(obj) = path.to_str().and_then(|p| p.strip_prefix("/vsigs/")) {
            return self.remote_project(obj);
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("cannot open project {}: {e}", path.display()))?;
        let mtime = std::fs::metadata(&canonical)?.modified()?;
        if let Some(p) = self.projects.lock().get(&canonical) {
            if p.last_modified >= mtime {
                return Ok(p.clone());
            }
            info!("project {} changed on disk, reloading", canonical.display());
        }
        let p = Arc::new(Project::read(&canonical)?);
        self.projects.lock().insert(canonical, p.clone());
        Ok(p)
    }
}

impl ConfigCache {
    fn remote_project(&self, obj: &str) -> anyhow::Result<Arc<Project>> {
        let ttl = std::env::var("RGIS_REMOTE_PROJECT_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300u64);
        let cached = self.remote.lock().get(obj).cloned();
        if let Some((at, _, p)) = &cached
            && at.elapsed().as_secs() < ttl
        {
            return Ok(p.clone());
        }
        // An unchanged project keeps its layers, and with them their open files and cached
        // blocks; a reload would start every layer cold again.
        let etag = cached.as_ref().and_then(|c| c.1.as_deref());
        let (bytes, etag) = match crate::provider::fetch_vsigs_if_changed(obj, etag) {
            Ok(Some(fetched)) => fetched,
            Ok(None) => {
                let (_, etag, p) = cached.expect("304 only with an ETag");
                self.remote
                    .lock()
                    .insert(obj.to_string(), (Instant::now(), etag, p.clone()));
                return Ok(p);
            }
            // GCS unreachable: keep serving the project we have.
            Err(e) => match cached {
                Some((_, etag, p)) => {
                    warn!("revalidating /vsigs/{obj} failed, keeping the cached project: {e:#}");
                    self.remote
                        .lock()
                        .insert(obj.to_string(), (Instant::now(), etag, p.clone()));
                    return Ok(p);
                }
                None => return Err(e),
            },
        };
        // Project::read works on files (.qgs or .qgz); keep the object's file name.
        let local = std::env::temp_dir()
            .join("rgis-projects")
            .join(obj.replace('/', "__"));
        std::fs::create_dir_all(local.parent().expect("has parent"))?;
        // Write under a unique name and rename into place: concurrent reloads of the same
        // object would otherwise read each other's half-written file.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = local.with_extension(format!("{}.{n}.part", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &local)?;
        let p = Arc::new(Project::read(&local)?);
        info!("loaded project /vsigs/{obj}");
        self.remote
            .lock()
            .insert(obj.to_string(), (Instant::now(), etag, p.clone()));
        Ok(p)
    }
}

pub type ApiHandler = fn(&Request, &Params, Option<&Project>) -> Result<Response, OwsError>;

thread_local! {
    /// When the request on this thread has to give up (Settings::request_timeout).
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// The current request's deadline, if it has one.
pub(crate) fn deadline() -> Option<Instant> {
    DEADLINE.get()
}

/// Fail once the current request is past its deadline. Called between units of work
/// (layers, the label solver), so a runaway request frees its thread instead of only
/// having its response dropped.
pub(crate) fn check_deadline() -> anyhow::Result<()> {
    match DEADLINE.get() {
        Some(d) if Instant::now() >= d => anyhow::bail!("request timed out"),
        _ => Ok(()),
    }
}

#[derive(Default)]
pub struct Settings {
    /// QGIS_PROJECT_FILE: a fixed project that overrides MAP.
    pub fixed_project: Option<PathBuf>,
    /// Used when no MAP is given (qgis_mapserver -p).
    pub default_project: Option<PathBuf>,
    /// When set, MAP must point inside this directory.
    pub projects_dir: Option<PathBuf>,
    /// Requests still working after this long stop at the next layer and answer 503.
    pub request_timeout: Option<Duration>,
}

pub struct Server {
    settings: Settings,
    /// False while RGIS_WARM_PROJECTS are loading (GET /ready answers 503).
    ready: std::sync::atomic::AtomicBool,
    cache: ConfigCache,
    registry: ServiceRegistry,
    apis: Vec<(&'static str, ApiHandler)>,
}

impl Server {
    pub fn new(settings: Settings) -> Server {
        let mut registry = ServiceRegistry::default();
        // QgsServiceNativeLoader loads these from shared libraries; here they are compiled in.
        crate::services::wms::register(&mut registry);
        crate::services::wfs::register(&mut registry);
        Server {
            settings,
            ready: std::sync::atomic::AtomicBool::new(true),
            cache: ConfigCache::default(),
            registry,
            apis: vec![("/wfs3", crate::services::oapif::handle)],
        }
    }

    pub fn request_timeout(&self) -> Option<std::time::Duration> {
        self.settings.request_timeout
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Load projects and open their vector data before taking traffic, so a new pod
    /// doesn't serve its first requests cold (tnt-qgis `warmProjects`). Names are project
    /// names under --projects-dir; `*` lists a local projects directory.
    pub fn warm(&self, names: &[String]) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(dir) = &self.settings.projects_dir else {
            warn!("RGIS_WARM_PROJECTS needs --projects-dir");
            return;
        };
        self.ready.store(false, Relaxed);
        let started = Instant::now();
        let dir_s = dir.to_string_lossy().to_string();
        let sep = if dir_s.ends_with('/') { "" } else { "/" };
        let mut paths: Vec<PathBuf> = vec![];
        for n in names {
            if n == "*" {
                match std::fs::read_dir(dir) {
                    Ok(rd) => paths.extend(
                        rd.filter_map(|e| e.ok().map(|e| e.path()))
                            .filter(|p| p.extension().is_some_and(|x| x == "qgs" || x == "qgz")),
                    ),
                    Err(e) => warn!("cannot list {dir_s} for RGIS_WARM_PROJECTS=*: {e}"),
                }
            } else {
                paths.push(PathBuf::from(format!(
                    "{dir_s}{sep}{}.qgs",
                    n.trim_end_matches(".qgs")
                )));
            }
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let (projects, layers) = (
            std::sync::atomic::AtomicUsize::new(0),
            std::sync::atomic::AtomicUsize::new(0),
        );
        // A few at a time: opening a remote file is one small range request each.
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    while let Some(path) = paths.get(next.fetch_add(1, Relaxed)) {
                        match self.cache.project(path) {
                            Ok(p) => {
                                projects.fetch_add(1, Relaxed);
                                for l in p.layers.values().filter(|l| l.kind == "vector") {
                                    match l.provider() {
                                        Ok(_) => _ = layers.fetch_add(1, Relaxed),
                                        Err(e) => debug!(
                                            "warm-up: layer {} of {}: {e:#}",
                                            l.name,
                                            path.display()
                                        ),
                                    }
                                }
                            }
                            Err(e) => warn!("warm-up: {}: {e:#}", path.display()),
                        }
                    }
                });
            }
        });
        info!(
            "warm-up: {} of {} project(s), {} layer(s) opened in {:?}",
            projects.into_inner(),
            paths.len(),
            layers.into_inner(),
            started.elapsed()
        );
        self.ready.store(true, Relaxed);
    }

    /// QgsServer::configPath, plus the tnt-qgis URL scheme: with a projects directory,
    /// `/qgis/QLR/<name>` serves `<projects-dir><name>.qgs` (its Caddy front rewrites that
    /// path to MAP), so clients only need a base URL.
    fn config_path(&self, req: &Request, params: &Params) -> Result<Option<PathBuf>, OwsError> {
        if let Some(p) = &self.settings.fixed_project {
            return Ok(Some(p.clone()));
        }
        if let (None, Some(dir), Some(name)) = (
            params.get("MAP"),
            &self.settings.projects_dir,
            req.path.strip_prefix("/qgis/QLR/"),
        ) {
            let name = name.trim_end_matches('/').trim_end_matches(".qgs");
            if name.is_empty() || name.contains('/') || name.contains("..") {
                return Err(OwsError::new(
                    "ProjectFileError",
                    format!("no project for path '{}'", req.path),
                    404,
                ));
            }
            let dir = dir.to_string_lossy();
            let sep = if dir.ends_with('/') { "" } else { "/" };
            return Ok(Some(PathBuf::from(format!("{dir}{sep}{name}.qgs"))));
        }
        if let Some(map) = params.get("MAP") {
            let p = PathBuf::from(map);
            if let Some(dir) = &self.settings.projects_dir {
                // A /vsigs/ projects "directory" is a prefix of the object key.
                let dir_str = dir.to_string_lossy();
                if !map.starts_with("/vsigs/") && !p.exists() {
                    return Err(OwsError::new(
                        "ProjectFileError",
                        format!("project '{map}' not found"),
                        404,
                    ));
                }
                let ok = if map.starts_with("/vsigs/") || dir_str.starts_with("/vsigs/") {
                    dir_str.starts_with("/vsigs/")
                        && map.starts_with(dir_str.as_ref())
                        && !map.contains("..")
                } else {
                    p.canonicalize()
                        .ok()
                        .zip(dir.canonicalize().ok())
                        .is_some_and(|(p, d)| p.starts_with(d))
                };
                if !ok {
                    return Err(OwsError::new(
                        "Forbidden",
                        format!("MAP '{map}' is outside the projects directory"),
                        403,
                    ));
                }
            }
            return Ok(Some(p));
        }
        Ok(self.settings.default_project.clone())
    }

    pub fn handle(&self, req: &Request) -> Response {
        let started = Instant::now();
        DEADLINE.set(self.settings.request_timeout.map(|t| started + t));
        let params = Params::from_query(&req.query);
        let service = params.service();
        let resp = self.dispatch(req, &params, &service);
        let timed_out = check_deadline().is_err();
        DEADLINE.set(None);
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                warn!("{} {}: {} ({})", req.method, req.path, e.message, e.code);
                let version = if service == "WFS" { "1.2.0" } else { "1.3.0" };
                if timed_out {
                    OwsError::new(
                        "RequestTimeout",
                        format!("The request took longer than {:?}", started.elapsed()),
                        503,
                    )
                    .to_response(version)
                } else if e.status >= 500 {
                    // Internal errors carry file paths, data source URLs and library
                    // messages; those are logged above, not sent to the client.
                    OwsError::new(
                        &e.code,
                        "Internal server error, see the server log",
                        e.status,
                    )
                    .to_response(version)
                } else {
                    e.to_response(version)
                }
            }
        };
        // One logfmt-style line per request (Loki: `| logfmt | unwrap ms` for percentiles).
        info!(
            method = %req.method,
            path = %req.path,
            service = %service,
            request = %params.get("REQUEST").unwrap_or(""),
            status = resp.status,
            r#type = %resp.content_type.split(';').next().unwrap_or(""),
            bytes = resp.body.len(),
            ms = (started.elapsed().as_secs_f64() * 1e4).round() / 10.0,
            query = %req.query,
            "request"
        );
        resp
    }

    fn dispatch(
        &self,
        req: &Request,
        params: &Params,
        service: &str,
    ) -> Result<Response, OwsError> {
        let project = match self.config_path(req, params)? {
            Some(path) => Some(self.cache.project(&path).map_err(|e| {
                // The cause (missing file, permissions, XML error) would tell clients
                // about files on the server; it goes to the log only.
                warn!("project {}: {e:#}", path.display());
                OwsError::bad(
                    "ProjectFileError",
                    match params.get("MAP") {
                        Some(map) => format!("cannot load project '{map}'"),
                        None => "cannot load the project".to_string(),
                    },
                )
            })?),
            None => None,
        };
        if service.is_empty() {
            // No SERVICE: try an OGC API (QgsServiceRegistry::apiForRequest).
            if let Some((_, api)) = self.apis.iter().find(|(root, _)| {
                req.path == *root
                    || req.path.starts_with(&format!("{root}/"))
                    || req.path.starts_with(&format!("{root}."))
            }) {
                return api(req, params, project.as_deref());
            }
            return Err(OwsError::bad(
                "Service configuration error",
                format!(
                    "Service unknown or unsupported. Current supported services (case-sensitive): {}, or use a WFS3 (OGC API Features) endpoint",
                    self.registry.names().join(" ")
                ),
            ));
        }
        let project = project.ok_or_else(|| {
            OwsError::bad("Server", "Project file error. For OWS services: please provide a SERVICE and a MAP parameter pointing to a valid QGIS project file")
        })?;
        let module = self.registry.get(service, params.get("VERSION")).ok_or_else(|| {
            OwsError::bad(
                "Service configuration error",
                format!("Service unknown or unsupported. Current supported services (case-sensitive): {}", self.registry.names().join(" ")),
            )
        })?;
        let mut service_url = project
            .text_prop(if service == "WFS" { "WFSUrl" } else { "WMSUrl" })
            .map(String::from)
            .unwrap_or_else(|| req.base_url.clone());
        service_url.push(if service_url.contains('?') { '&' } else { '?' });
        if let (Some(map), None) = (params.get("MAP"), &self.settings.fixed_project) {
            service_url += &format!("MAP={}&", urlencoding::encode(map));
        }
        let ctx = Context {
            params,
            project: &project,
            service_url,
        };
        module.execute(&ctx)
    }
}
