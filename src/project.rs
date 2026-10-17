//! QGIS project reading (QgsProject::read) for `.qgs` and `.qgz` files.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use roxmltree::{Document, Node, ParsingOptions};
use tracing::{info, warn};

use crate::crs::{Crs, Transform};
use crate::geom::Rect;
use crate::provider::Provider;
use crate::symbology::Renderer;

#[derive(Debug, Clone)]
pub enum PropValue {
    Text(String),
    List(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum TreeNode {
    Group(TreeGroup),
    Layer { id: String },
}

#[derive(Debug, Clone, Default)]
pub struct TreeGroup {
    pub name: String,
    pub short_name: Option<String>,
    pub title: Option<String>,
    pub children: Vec<TreeNode>,
}

impl TreeGroup {
    /// All layer ids below this group, in tree order (top of the legend first).
    pub fn layer_ids(&self) -> Vec<String> {
        let mut out = vec![];
        fn walk(g: &TreeGroup, out: &mut Vec<String>) {
            for c in &g.children {
                match c {
                    TreeNode::Group(sub) => walk(sub, out),
                    TreeNode::Layer { id, .. } => out.push(id.clone()),
                }
            }
        }
        walk(self, &mut out);
        out
    }

    pub fn nickname(&self) -> &str {
        self.short_name.as_deref().unwrap_or(&self.name)
    }
}

pub struct MapLayer {
    pub id: String,
    pub name: String,
    pub short_name: Option<String>,
    pub title: Option<String>,
    pub abstract_: Option<String>,
    pub kind: String,
    pub provider_key: String,
    pub datasource: String,
    pub crs: Option<Crs>,
    pub extent: Option<Rect>,
    pub wgs84_extent: Option<Rect>,
    pub geometry: String,
    pub renderer: Renderer,
    pub opacity: f64,
    pub scale_range: Option<(f64, f64)>,
    pub style_name: String,
    pub identifiable: bool,
    pub hidden_wms: Vec<String>,
    pub hidden_wfs: Vec<String>,
    base_dir: PathBuf,
    provider: OnceLock<std::result::Result<Arc<Provider>, String>>,
}

impl MapLayer {
    pub fn provider(&self) -> Result<Arc<Provider>> {
        self.provider
            .get_or_init(|| {
                Provider::open(&self.base_dir, &self.provider_key, &self.datasource)
                    .map(Arc::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .clone()
            .map_err(|e| anyhow!("layer '{}' is invalid: {e}", self.name))
    }

    pub fn crs(&self) -> Crs {
        self.crs.clone().unwrap_or_else(Crs::wgs84)
    }

    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(&self.name)
    }

    /// Layer extent in the layer CRS: `<extent>` from the project, else from the provider.
    pub fn full_extent(&self) -> Option<Rect> {
        self.extent.or_else(|| self.provider().ok()?.extent)
    }

    /// Layer extent in WGS84 (from `<wgs84extent>`, else the transformed full extent).
    pub fn wgs84(&self) -> Option<Rect> {
        self.wgs84_extent.or_else(|| {
            let e = self.full_extent()?;
            Transform::new(&self.crs(), &Crs::wgs84()).rect(&e).ok()
        })
    }

    /// QGIS scale-based visibility: visible when maxScale < scale <= minScale.
    pub fn visible_at(&self, scale: f64) -> bool {
        match self.scale_range {
            Some((min, max)) => (min <= 0.0 || scale <= min) && (max <= 0.0 || scale > max),
            None => true,
        }
    }
}

pub struct Project {
    pub path: PathBuf,
    pub last_modified: SystemTime,
    pub title: String,
    pub crs: Crs,
    pub tree: TreeGroup,
    pub layers: IndexMap<String, Arc<MapLayer>>,
    pub properties: HashMap<String, PropValue>,
}

fn text<'a>(n: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    n.children()
        .find(|c| c.has_tag_name(tag))
        .and_then(|c| c.text())
        .map(str::trim)
}

fn child<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(tag))
}

fn rect(n: Option<Node>) -> Option<Rect> {
    let n = n?;
    let v = |t| text(n, t).and_then(|s| s.parse::<f64>().ok());
    let r = Rect::new(v("xmin")?, v("ymin")?, v("xmax")?, v("ymax")?);
    (!r.is_empty()).then_some(r)
}

fn srs(n: Option<Node>) -> Option<Crs> {
    let s = child(n?, "spatialrefsys")?;
    let authid = text(s, "authid").unwrap_or("");
    let proj4 = text(s, "proj4").unwrap_or("");
    if authid.is_empty() && proj4.is_empty() {
        return None;
    }
    Crs::from_authid_or_proj4(authid, proj4)
        .map_err(|e| warn!("unsupported layer CRS: {e:#}"))
        .ok()
}

/// Custom properties come as `<Option type="Map">` (new) or `<property key value>` (old).
fn custom_property(n: Node, key: &str) -> Option<String> {
    let cp = child(n, "customproperties")?;
    cp.descendants()
        .find(|c| {
            (c.has_tag_name("Option") && c.attribute("name") == Some(key))
                || (c.has_tag_name("property") && c.attribute("key") == Some(key))
        })
        .and_then(|c| c.attribute("value"))
        .map(String::from)
}

fn parse_group(n: Node) -> TreeGroup {
    let mut g = TreeGroup {
        name: n.attribute("name").unwrap_or("").to_string(),
        short_name: custom_property(n, "wmsShortName").filter(|s| !s.is_empty()),
        title: custom_property(n, "wmsTitle").filter(|s| !s.is_empty()),
        children: vec![],
    };
    for c in n.children() {
        if c.has_tag_name("layer-tree-group") {
            g.children.push(TreeNode::Group(parse_group(c)));
        } else if c.has_tag_name("layer-tree-layer") {
            g.children.push(TreeNode::Layer {
                id: c.attribute("id").unwrap_or("").to_string(),
            });
        }
    }
    g
}

fn parse_properties(n: Node, prefix: &str, out: &mut HashMap<String, PropValue>) {
    for c in n.children().filter(|c| c.is_element()) {
        let key = format!("{prefix}{}", c.tag_name().name());
        match c.attribute("type") {
            Some("QStringList") => {
                let v = c
                    .children()
                    .filter(|v| v.has_tag_name("value"))
                    .map(|v| v.text().unwrap_or("").to_string())
                    .collect();
                out.insert(key, PropValue::List(v));
            }
            Some(_) => {
                out.insert(
                    key,
                    PropValue::Text(c.text().unwrap_or("").trim().to_string()),
                );
            }
            None => parse_properties(c, &format!("{key}/"), out),
        }
    }
}

fn hidden_fields(ml: Node, flag: &str, legacy_tag: &str) -> Vec<String> {
    let mut out: Vec<String> = child(ml, "fieldConfiguration")
        .map(|fc| {
            fc.children()
                .filter(|f| {
                    f.has_tag_name("field")
                        && f.attribute("configurationFlags")
                            .is_some_and(|fl| fl.split('|').any(|x| x == flag))
                })
                .filter_map(|f| f.attribute("name").map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if let Some(ex) = child(ml, legacy_tag) {
        out.extend(
            ex.children()
                .filter(|a| a.has_tag_name("attribute"))
                .filter_map(|a| a.text())
                .map(String::from),
        );
    }
    out
}

fn parse_layer(ml: Node, base_dir: &Path) -> MapLayer {
    let flag = |k: &str| ml.attribute(k);
    let scale = |k: &str| flag(k).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    MapLayer {
        id: text(ml, "id").unwrap_or("").to_string(),
        name: text(ml, "layername").unwrap_or("").to_string(),
        short_name: text(ml, "shortname")
            .filter(|s| !s.is_empty())
            .map(String::from),
        title: text(ml, "title")
            .filter(|s| !s.is_empty())
            .map(String::from),
        abstract_: text(ml, "abstract")
            .filter(|s| !s.is_empty())
            .map(String::from),
        kind: flag("type").unwrap_or("vector").to_string(),
        provider_key: text(ml, "provider").unwrap_or("ogr").to_string(),
        datasource: text(ml, "datasource").unwrap_or("").to_string(),
        crs: srs(child(ml, "srs")),
        extent: rect(child(ml, "extent")),
        wgs84_extent: rect(child(ml, "wgs84extent")),
        geometry: flag("geometry").unwrap_or("").to_string(),
        renderer: child(ml, "renderer-v2")
            .map(Renderer::parse)
            .unwrap_or(Renderer::Null),
        opacity: text(ml, "layerOpacity")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0),
        scale_range: (flag("hasScaleBasedVisibilityFlag") == Some("1"))
            .then(|| (scale("minScale"), scale("maxScale"))),
        style_name: child(ml, "map-layer-style-manager")
            .and_then(|s| s.attribute("current"))
            .filter(|s| !s.is_empty())
            .unwrap_or("default")
            .to_string(),
        identifiable: child(ml, "flags")
            .and_then(|f| text(f, "Identifiable"))
            .is_none_or(|v| v != "0"),
        hidden_wms: hidden_fields(ml, "HideFromWms", "excludeAttributesWMS"),
        hidden_wfs: hidden_fields(ml, "HideFromWfs", "excludeAttributesWFS"),
        base_dir: base_dir.to_path_buf(),
        provider: OnceLock::new(),
    }
}

/// Reads the project XML, unpacking `.qgz` archives like QgsProject::unzip.
fn read_xml(path: &Path) -> Result<String> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("qgz"))
    {
        let mut zip =
            zip::ZipArchive::new(std::fs::File::open(path)?).context("not a valid .qgz archive")?;
        for i in 0..zip.len() {
            let mut f = zip.by_index(i)?;
            if f.name()?.to_ascii_lowercase().ends_with(".qgs") {
                let mut s = String::new();
                f.read_to_string(&mut s)?;
                return Ok(s);
            }
        }
        bail!("no .qgs file inside {}", path.display());
    }
    Ok(std::fs::read_to_string(path)?)
}

impl Project {
    pub fn read(path: &Path) -> Result<Project> {
        let started = std::time::Instant::now();
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot open project {}", path.display()))?;
        let last_modified = std::fs::metadata(&path)?.modified()?;
        let xml = read_xml(&path)?;
        let doc = Document::parse_with_options(
            &xml,
            ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .with_context(|| format!("invalid project XML in {}", path.display()))?;
        let root = doc.root_element();
        if !root.has_tag_name("qgis") {
            bail!("{} is not a QGIS project", path.display());
        }
        let base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let mut layers = IndexMap::new();
        if let Some(pl) = child(root, "projectlayers") {
            for ml in pl.children().filter(|c| c.has_tag_name("maplayer")) {
                let l = parse_layer(ml, &base_dir);
                layers.insert(l.id.clone(), Arc::new(l));
            }
        }
        let mut properties = HashMap::new();
        if let Some(p) = child(root, "properties") {
            parse_properties(p, "", &mut properties);
        }
        let project = Project {
            title: text(root, "title")
                .filter(|t| !t.is_empty())
                .or(root.attribute("projectname"))
                .unwrap_or("")
                .to_string(),
            crs: srs(child(root, "projectCrs")).unwrap_or_else(Crs::wgs84),
            tree: child(root, "layer-tree-group")
                .map(parse_group)
                .unwrap_or_default(),
            layers,
            properties,
            path,
            last_modified,
        };
        info!(
            "read project {} (QGIS {}, {} layers) in {:?}",
            project.path.display(),
            root.attribute("version").unwrap_or("?"),
            project.layers.len(),
            started.elapsed()
        );
        // Mirror QgsStoreBadLayerInfo: validate data sources up front and report bad layers.
        for l in project.layers.values().filter(|l| l.kind == "vector") {
            if let Err(e) = l.provider() {
                warn!("bad layer: {e:#}");
            }
        }
        Ok(project)
    }

    pub fn text_prop(&self, key: &str) -> Option<&str> {
        match self.properties.get(key) {
            Some(PropValue::Text(t)) if !t.is_empty() => Some(t),
            _ => None,
        }
    }

    pub fn list_prop(&self, key: &str) -> Vec<String> {
        match self.properties.get(key) {
            Some(PropValue::List(l)) => l.clone(),
            _ => vec![],
        }
    }

    pub fn bool_prop(&self, key: &str) -> bool {
        self.text_prop(key).is_some_and(|v| v == "true" || v == "1")
    }

    pub fn int_prop(&self, key: &str) -> Option<i64> {
        self.text_prop(key)
            .and_then(|v| v.parse().ok())
            .filter(|v| *v > 0)
    }

    /// WMSRootName, else the project title (QgsWmsRenderContext::initNicknameLayers).
    pub fn root_name(&self) -> String {
        self.text_prop("WMSRootName")
            .map(String::from)
            .unwrap_or_else(|| self.title.clone())
    }

    pub fn service_title(&self) -> String {
        self.text_prop("WMSServiceTitle")
            .map(String::from)
            .unwrap_or_else(|| self.title.clone())
    }

    /// QgsWmsRenderContext::layerNickname: id if WMSUseLayerIDs, else short name, else name.
    pub fn nickname(&self, l: &MapLayer) -> String {
        if self.bool_prop("WMSUseLayerIDs") {
            return l.id.clone();
        }
        l.short_name.clone().unwrap_or_else(|| l.name.clone())
    }

    pub fn restricted(&self) -> Vec<String> {
        self.list_prop("WMSRestrictedLayers")
    }

    pub fn is_restricted(&self, l: &MapLayer) -> bool {
        self.restricted().contains(&l.name)
    }

    pub fn wfs_layers(&self) -> Vec<Arc<MapLayer>> {
        self.list_prop("WFSLayers")
            .iter()
            .filter_map(|id| self.layers.get(id).cloned())
            .collect()
    }

    pub fn wfs_precision(&self, l: &MapLayer) -> usize {
        // QgsServerProjectUtils::wfsLayerPrecision defaults to 6.
        self.text_prop(&format!("WFSLayersPrecision/{}", l.id))
            .and_then(|v| v.parse().ok())
            .unwrap_or(6)
    }

    /// Advertised CRSs (WMSCrsList), always including the project CRS, EPSG:4326 and CRS:84.
    pub fn crs_list(&self) -> Vec<String> {
        let mut v = vec!["CRS:84".to_string()];
        let mut configured = self.list_prop("WMSCrsList");
        if configured.is_empty() {
            configured = vec![
                self.crs.authid.clone(),
                "EPSG:4326".into(),
                "EPSG:3857".into(),
            ];
        }
        for c in configured {
            if !v.contains(&c) {
                v.push(c);
            }
        }
        v
    }

    /// WMSExtent (in the project CRS) or the union of all layer extents.
    pub fn extent(&self) -> Rect {
        let configured = self.list_prop("WMSExtent");
        if configured.len() == 4
            && let Some(r) = Rect::parse(&configured.join(",")) {
                return r;
            }
        let mut r = Rect::empty();
        for l in self.layers.values() {
            if let Some(e) = l.full_extent()
                && let Ok(t) = Transform::new(&l.crs(), &self.crs).rect(&e) {
                    r.union(&t);
                }
        }
        r
    }

    /// Find a tree group by its WMS nickname (the root is named by `root_name`).
    pub fn find_group(&self, name: &str) -> Option<TreeGroup> {
        fn walk(g: &TreeGroup, name: &str) -> Option<TreeGroup> {
            for c in &g.children {
                if let TreeNode::Group(sub) = c {
                    if sub.nickname() == name {
                        return Some(sub.clone());
                    }
                    if let Some(f) = walk(sub, name) {
                        return Some(f);
                    }
                }
            }
            None
        }
        if self.root_name() == name {
            return Some(self.tree.clone());
        }
        walk(&self.tree, name)
    }

    pub fn layer_by_nickname(&self, name: &str) -> Option<Arc<MapLayer>> {
        self.layers
            .values()
            .find(|l| self.nickname(l) == name && !self.is_restricted(l))
            .cloned()
    }

    /// Resolve a WMS layer/group name into concrete layers, top of the legend first.
    pub fn resolve(&self, name: &str) -> Option<Vec<Arc<MapLayer>>> {
        if let Some(l) = self.layer_by_nickname(name) {
            return Some(vec![l]);
        }
        let g = self.find_group(name)?;
        Some(
            g.layer_ids()
                .iter()
                .filter_map(|id| self.layers.get(id))
                .filter(|l| !self.is_restricted(l))
                .cloned()
                .collect(),
        )
    }
}
