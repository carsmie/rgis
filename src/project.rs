//! QGIS project reading (QgsProject::read) for `.qgs` and `.qgz` files.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// `wms-group-request-mode="Opaque"` (QGIS 4.2+): the group is published as one layer;
    /// its children are hidden and cannot be requested on their own.
    pub opaque: bool,
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
    /// Render-time simplification tolerance in pixels (`simplifyDrawingHints`/`simplifyDrawingTol`).
    pub simplify_tol: Option<f64>,
    /// Layer expression variables (custom properties `variableNames` / `variableValues`).
    pub variables: Vec<(String, String)>,
    pub style_name: String,
    pub identifiable: bool,
    pub hidden_wms: Vec<String>,
    pub hidden_wfs: Vec<String>,
    /// Raster layers: the `<pipe>` (renderer, resampling, filters) and blend mode.
    #[cfg_attr(not(feature = "raster"), allow(dead_code))]
    pub raster_style: Option<crate::raster::RasterStyle>,
    /// Labeling rules (one for simple labeling), empty when labels are disabled.
    pub labeling: Vec<crate::labeling::settings::LabelingRule>,
    pub base_dir: PathBuf,
    /// The opened data provider, or why it failed and when (retried after PROVIDER_RETRY).
    provider: parking_lot::Mutex<Option<OpenedProvider>>,
}

/// When a layer's provider was opened, and the provider or why opening failed.
type OpenedProvider = (std::time::Instant, std::result::Result<Arc<Provider>, String>);

/// A failed open (database or network briefly unreachable, an egress rule not active yet)
/// is retried after this, instead of the layer staying invalid for the life of the process.
const PROVIDER_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

impl MapLayer {
    pub fn provider(&self) -> Result<Arc<Provider>> {
        // Held while opening: concurrent requests wait for one attempt instead of each
        // opening (or failing to open) the same source.
        let mut slot = self.provider.lock();
        let stale = match slot.as_ref() {
            Some((_, Ok(_))) => false,
            Some((at, Err(_))) => at.elapsed() >= PROVIDER_RETRY,
            None => true,
        };
        if stale {
            let opened = Provider::open(&self.base_dir, &self.provider_key, &self.datasource)
                .map(Arc::new)
                .map_err(|e| format!("{e:#}"));
            *slot = Some((std::time::Instant::now(), opened));
        }
        let (_, opened) = slot.as_ref().expect("set above");
        opened.clone().map_err(|e| anyhow!("layer '{}' is invalid: {e}", self.name))
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

/// A list-valued custom property: `<Option type="List" name=key><Option value=…/>` (new)
/// or `<property key=key><value>…</value></property>` (old).
fn custom_list(n: Node, key: &str) -> Vec<String> {
    let Some(cp) = child(n, "customproperties") else {
        return vec![];
    };
    for c in cp.descendants() {
        if c.has_tag_name("Option") && c.attribute("name") == Some(key) {
            return c
                .children()
                .filter(|o| o.has_tag_name("Option"))
                .filter_map(|o| o.attribute("value"))
                .map(String::from)
                .collect();
        }
        if c.has_tag_name("property") && c.attribute("key") == Some(key) {
            return c
                .children()
                .filter(|v| v.has_tag_name("value"))
                .map(|v| v.text().unwrap_or("").to_string())
                .collect();
        }
    }
    vec![]
}

fn parse_group(n: Node) -> TreeGroup {
    let mut g = TreeGroup {
        name: n.attribute("name").unwrap_or("").to_string(),
        short_name: custom_property(n, "wmsShortName").filter(|s| !s.is_empty()),
        title: custom_property(n, "wmsTitle").filter(|s| !s.is_empty()),
        opaque: n.attribute("wms-group-request-mode") == Some("Opaque"),
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
        // QGIS 3: <WMSServiceTitle type=…>; QGIS 4: <properties name="WMSServiceTitle" type=…>.
        let name = match (c.tag_name().name(), c.attribute("name")) {
            ("properties", Some(name)) => name,
            (tag, _) => tag,
        };
        let key = format!("{prefix}{name}");
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
        renderer: {
            let mut r = child(ml, "renderer-v2")
                .map(Renderer::parse)
                .unwrap_or(Renderer::Null);
            r.resolve_paths(base_dir);
            r
        },
        opacity: text(ml, "layerOpacity")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0),
        scale_range: (flag("hasScaleBasedVisibilityFlag") == Some("1"))
            .then(|| (scale("minScale"), scale("maxScale"))),
        variables: custom_list(ml, "variableNames")
            .into_iter()
            .zip(custom_list(ml, "variableValues"))
            .collect(),
        // simplifyDrawingHints is a flag set; 0 = no simplification.
        simplify_tol: flag("simplifyDrawingHints")
            .and_then(|h| h.parse::<u32>().ok())
            .filter(|h| *h != 0)
            .map(|_| scale("simplifyDrawingTol"))
            .filter(|t| *t > 0.0),
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
        raster_style: crate::raster::parse_style(ml),
        labeling: crate::labeling::settings::parse_labeling(ml),
        base_dir: base_dir.to_path_buf(),
        provider: Default::default(),
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

    /// Project-scope expression variables: QGIS's built-in `@project_*` ones plus the
    /// user variables from `<properties><Variables>`.
    pub fn variables(&self) -> Vec<(String, String)> {
        let file = self
            .path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut v = vec![
            ("project_title".to_string(), self.title.clone()),
            ("project_path".to_string(), self.path.display().to_string()),
            ("project_filename".to_string(), file.clone()),
            (
                "project_basename".to_string(),
                file.rsplit_once('.')
                    .map(|(b, _)| b.to_string())
                    .unwrap_or(file),
            ),
            (
                "project_folder".to_string(),
                self.path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
            ("project_crs".to_string(), self.crs.authid.clone()),
        ];
        v.extend(
            self.list_prop("Variables/variableNames")
                .into_iter()
                .zip(self.list_prop("Variables/variableValues")),
        );
        v
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
            && let Some(r) = Rect::parse(&configured.join(","))
        {
            return r;
        }
        let mut r = Rect::empty();
        for l in self.layers.values() {
            if let Some(e) = l.full_extent()
                && let Ok(t) = Transform::new(&l.crs(), &self.crs).rect(&e)
            {
                r.union(&t);
            }
        }
        r
    }

    /// Find a tree group by its WMS nickname (the root is named by `root_name`).
    /// Layer ids and group names inside opaque groups (not requestable on their own).
    fn hidden_by_opaque(
        &self,
    ) -> (
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    ) {
        let (mut ids, mut groups) = (Default::default(), Default::default());
        fn walk(
            g: &TreeGroup,
            inside: bool,
            ids: &mut std::collections::HashSet<String>,
            groups: &mut std::collections::HashSet<String>,
        ) {
            for c in &g.children {
                match c {
                    TreeNode::Layer { id } if inside => {
                        ids.insert(id.clone());
                    }
                    TreeNode::Layer { .. } => {}
                    TreeNode::Group(sub) => {
                        if inside {
                            groups.insert(sub.nickname().to_string());
                        }
                        walk(sub, inside || sub.opaque, ids, groups);
                    }
                }
            }
        }
        walk(&self.tree, false, &mut ids, &mut groups);
        (ids, groups)
    }

    pub fn find_group(&self, name: &str) -> Option<TreeGroup> {
        fn walk(g: &TreeGroup, name: &str) -> Option<TreeGroup> {
            for c in &g.children {
                if let TreeNode::Group(sub) = c {
                    if sub.nickname() == name {
                        return Some(sub.clone());
                    }
                    // Groups inside an opaque group cannot be addressed.
                    if sub.opaque {
                        continue;
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

    /// Resolve a WMS layer/group name into concrete layers, top of the legend first.
    pub fn resolve(&self, name: &str) -> Option<Vec<Arc<MapLayer>>> {
        // Several layers may share a WMS name; QGIS keeps them in a QMultiMap filled in
        // layer-id order, so they end up stacked with the smallest id on top.
        let (opaque_children, _) = self.hidden_by_opaque();
        let mut same: Vec<Arc<MapLayer>> = self
            .layers
            .values()
            .filter(|l| {
                self.nickname(l) == name
                    && !self.is_restricted(l)
                    && !opaque_children.contains(&l.id)
            })
            .cloned()
            .collect();
        if !same.is_empty() {
            same.sort_by(|a, b| a.id.cmp(&b.id));
            return Some(same);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_property_encodings() {
        for xml in [
            r#"<properties><WMSRestrictedLayers type="QStringList"><value>a</value></WMSRestrictedLayers><Variables><variableNames type="QStringList"><value>r</value></variableNames></Variables><WMSServiceTitle type="QString">T</WMSServiceTitle></properties>"#,
            r#"<properties name="properties"><properties name="WMSRestrictedLayers" type="QStringList"><value>a</value></properties><properties name="Variables"><properties name="variableNames" type="QStringList"><value>r</value></properties></properties><properties name="WMSServiceTitle" type="QString">T</properties></properties>"#,
        ] {
            let doc = Document::parse(xml).unwrap();
            let mut out = HashMap::new();
            parse_properties(doc.root_element(), "", &mut out);
            assert!(
                matches!(out.get("WMSRestrictedLayers"), Some(PropValue::List(v)) if v == &["a"]),
                "{xml}"
            );
            assert!(
                matches!(out.get("Variables/variableNames"), Some(PropValue::List(v)) if v == &["r"]),
                "{xml}"
            );
            assert!(
                matches!(out.get("WMSServiceTitle"), Some(PropValue::Text(t)) if t == "T"),
                "{xml}"
            );
        }
    }
}
