//! WMS 1.3.0 / 1.1.1 (QGIS services/wms: qgswms.cpp, qgswmsgetmap.cpp, qgswmsrenderer.cpp, …).

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::crs::{Crs, Transform};
use crate::expression::{Expr, to_str};
use crate::geom::{self, Rect};
use crate::project::{MapLayer, Project, TreeGroup, TreeNode};
use crate::render::{self, LayerJob, MapSettings, OGC_DPI};
use crate::server::{Context, OwsError, Response, Service, ServiceRegistry, esc};
use crate::symbology::Rgba;

const DEFAULT_MAX_SIZE: u32 = 10_000;

pub struct Wms {
    version: &'static str,
}

pub fn register(r: &mut ServiceRegistry) {
    r.register(Arc::new(Wms { version: "1.3.0" }));
    r.register(Arc::new(Wms { version: "1.1.1" }));
}

impl Service for Wms {
    fn name(&self) -> &'static str {
        "WMS"
    }
    fn version(&self) -> &'static str {
        self.version
    }
    fn execute(&self, ctx: &Context) -> Result<Response, OwsError> {
        let req = ctx.params.get("REQUEST").unwrap_or("");
        let is = |n: &str| req.eq_ignore_ascii_case(n);
        if req.is_empty() {
            return Err(OwsError::new(
                "OperationNotSupported",
                "Please add or check the value of the REQUEST parameter",
                501,
            ));
        }
        if is("GetCapabilities") || is("GetProjectSettings") {
            Ok(Response::ok(
                "text/xml; charset=utf-8",
                capabilities(ctx, self.version),
            ))
        } else if is("GetMap") {
            get_map(ctx, self.version)
        } else if is("GetFeatureInfo") {
            get_feature_info(ctx, self.version)
        } else if is("GetLegendGraphic") || is("GetLegendGraphics") {
            get_legend(ctx)
        } else {
            Err(OwsError::new(
                "OperationNotSupported",
                format!("Request {req} is not supported"),
                501,
            ))
        }
    }
}

// ------------------------------------------------------------ request helpers

fn require_version(ctx: &Context) -> Result<(), OwsError> {
    if ctx.params.get("VERSION").is_none() {
        return Err(OwsError::new(
            "OperationNotSupported",
            "Please add the value of the VERSION parameter",
            501,
        ));
    }
    Ok(())
}

fn dpi(ctx: &Context) -> f64 {
    ctx.params
        .f64("DPI")
        .or_else(|| ctx.params.f64("MAP_RESOLUTION"))
        .or_else(|| {
            ctx.params.get("FORMAT_OPTIONS").and_then(|o| {
                o.split(';').find_map(|kv| {
                    kv.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("dpi"))
                        .and_then(|(_, v)| v.parse().ok())
                })
            })
        })
        .filter(|d: &f64| *d > 0.0)
        .unwrap_or(OGC_DPI)
}

fn parse_bgcolor(s: &str) -> Option<Rgba> {
    let hex = s
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .trim_start_matches('#');
    Rgba::parse(&format!("#{hex}"))
}

/// QgsRenderer::configureMapSettings: size, CRS (CRS:84, axis order), BBOX, DPI, background.
fn map_settings(
    ctx: &Context,
    version: &str,
    layers: &[Arc<MapLayer>],
) -> Result<MapSettings, OwsError> {
    let p = ctx.params;
    let dim = |k: &str| -> Result<u32, OwsError> {
        let v = p
            .get(k)
            .ok_or_else(|| OwsError::bad("MissingParameterValue", format!("{k} is mandatory")))?;
        v.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                OwsError::bad(
                    "InvalidParameterValue",
                    format!("{k} ('{v}') must be a positive integer"),
                )
            })
    };
    let (width, height) = (dim("WIDTH")?, dim("HEIGHT")?);
    // Project WMSMaxWidth/Height and the QGIS_SERVER_WMS_MAX_* variables (smallest wins,
    // as in QgsWmsRenderContext::isValidWidthHeight). Unlike QGIS, which is unlimited
    // when neither is set, we keep a default safety cap.
    let limit = |prop: &str, env: &str| -> u32 {
        let from_env = std::env::var(env)
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0);
        match (ctx.project.int_prop(prop), from_env) {
            (Some(a), Some(b)) => a.min(b) as u32,
            (Some(a), None) | (None, Some(a)) => a as u32,
            (None, None) => DEFAULT_MAX_SIZE,
        }
    };
    let (max_w, max_h) = (
        limit("WMSMaxWidth", "QGIS_SERVER_WMS_MAX_WIDTH"),
        limit("WMSMaxHeight", "QGIS_SERVER_WMS_MAX_HEIGHT"),
    );
    if width > max_w || height > max_h {
        return Err(OwsError::bad(
            "InvalidParameterValue",
            "The requested map size is too large",
        ));
    }
    let v13 = version != "1.1.1";
    let crs_param = if v13 {
        p.get("CRS").or_else(|| p.get("SRS"))
    } else {
        p.get("SRS").or_else(|| p.get("CRS"))
    };
    let crs_id = crs_param.unwrap_or("EPSG:4326");
    let crs = Crs::from_ogc(crs_id).map_err(|_| {
        if v13 {
            OwsError::bad(
                "InvalidCRS",
                format!("Could not create output CRS from '{crs_id}'"),
            )
        } else {
            OwsError::bad(
                "InvalidSRS",
                format!("Could not create output SRS from '{crs_id}'"),
            )
        }
    })?;
    let extent = match p.get("BBOX") {
        Some(b) => {
            let r = Rect::parse(b)
                .filter(|r| r.width() > 0.0 && r.height() > 0.0)
                .ok_or_else(|| {
                    OwsError::bad("InvalidParameterValue", format!("Invalid BBOX '{b}'"))
                })?;
            // WMS 1.3.0: EPSG geographic CRSs are lat/lon (QgsRenderer::configureMapSettings).
            if v13 && crs.axis_inverted() {
                r.swapped()
            } else {
                r
            }
        }
        None => {
            let mut r = Rect::empty();
            for l in layers {
                if let Some(e) = l
                    .full_extent()
                    .and_then(|e| Transform::new(&l.crs(), &crs).rect(&e).ok())
                {
                    r.union(&e);
                }
            }
            if r.is_empty() {
                return Err(OwsError::bad("MissingParameterValue", "BBOX is mandatory"));
            }
            r
        }
    };
    let transparent = p.bool("TRANSPARENT");
    let background = if transparent {
        None
    } else {
        Some(
            p.get("BGCOLOR")
                .and_then(parse_bgcolor)
                .unwrap_or(Rgba(255, 255, 255, 255)),
        )
    };
    Ok(MapSettings {
        extent,
        width,
        height,
        dpi: dpi(ctx),
        crs,
        background,
        project_variables: ctx.project.variables(),
        extent_buffer: 0.0,
        labeling: crate::labeling::engine_settings(ctx.project),
    })
}

/// QgsWmsRenderContext::searchLayersToRender: names may be layers or groups.
/// WMS lists layers bottom-to-top; groups expand keeping their legend order on top.
fn layers_from(project: &Project, names: &[String]) -> Result<Vec<Arc<MapLayer>>, OwsError> {
    let mut out = vec![];
    for n in names {
        let mut ls = project.resolve(n).ok_or_else(|| {
            OwsError::bad(
                "LayerNotDefined",
                format!("The layer '{n}' does not exist."),
            )
        })?;
        ls.reverse();
        out.extend(ls);
    }
    Ok(out)
}

/// FILTER=layer:"expr";layer2:"expr"
fn filters(ctx: &Context) -> Result<Vec<(String, Expr)>, OwsError> {
    let Some(f) = ctx.params.get("FILTER") else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for part in f.split(';').filter(|s| !s.trim().is_empty()) {
        let (layer, expr) = part.split_once(':').ok_or_else(|| {
            OwsError::bad("InvalidParameterValue", format!("Invalid FILTER '{part}'"))
        })?;
        let e = Expr::parse(expr).map_err(|e| {
            OwsError::bad(
                "InvalidParameterValue",
                format!("The filter string {expr} has been rejected: {e}"),
            )
        })?;
        out.push((layer.trim().to_string(), e));
    }
    Ok(out)
}

// ---------------------------------------------------------------------- GetMap

fn get_map(ctx: &Context, version: &str) -> Result<Response, OwsError> {
    require_version(ctx)?;
    let names = ctx.params.list("LAYERS");
    if names.is_empty() {
        return Err(OwsError::bad(
            "MissingParameterValue",
            "The LAYERS parameter is missing",
        ));
    }
    let format = ctx
        .params
        .get_or("FORMAT", "image/png")
        .to_ascii_lowercase();
    if !(format.starts_with("image/png") || format == "png") {
        return Err(OwsError::bad(
            "InvalidFormat",
            format!("Output format '{format}' is not supported (use image/png)"),
        ));
    }
    let layers = layers_from(ctx.project, &names)?;
    let mut ms = map_settings(ctx, version, &layers)?;
    let requested = (ms.width, ms.height);
    if version != "1.1.1" {
        (ms.width, ms.height) = render::square_pixel_size(&ms.extent, ms.width, ms.height);
    }
    // QgsWmsRenderContext::mapTileBuffer: only for TILED requests.
    if ctx.params.bool("TILED") {
        let tile_buffer: f64 = ctx.project.text_prop("WMSTileBuffer").and_then(|v| v.trim().parse().ok()).unwrap_or(0.0);
        ms.extent_buffer = tile_buffer * ms.extent.width() / ms.width as f64;
    }
    let opacities: Vec<f64> = ctx
        .params
        .list("OPACITIES")
        .iter()
        .map(|o| o.parse::<f64>().map(|v| v / 255.0).unwrap_or(1.0))
        .collect();
    let filters = filters(ctx)?;
    // Map per-name opacities onto the expanded layer list.
    let mut jobs = vec![];
    for (i, n) in names.iter().enumerate() {
        let mut group = ctx.project.resolve(n).unwrap_or_default();
        group.reverse();
        for l in group {
            let nick = ctx.project.nickname(&l);
            jobs.push(LayerJob {
                filter: filters
                    .iter()
                    .find(|(name, _)| *name == nick)
                    .map(|(_, e)| e.clone()),
                opacity: opacities.get(i).copied().unwrap_or(1.0),
                layer: l,
            });
        }
    }
    // Held through scaling and PNG encoding, which allocate at the requested size too.
    let _permit = render::render_permit(ms.width.max(requested.0), ms.height.max(requested.1));
    let t = std::time::Instant::now();
    let pm = render::render(&jobs, &ms)
        .map_err(|e| OwsError::internal(format!("Rendering error : '{e:#}'")))?;
    let pm = render::scale(&pm, requested.0, requested.1);
    let rendered = t.elapsed();
    let png = render::encode_png(&pm)?;
    tracing::debug!(
        "GetMap: render {rendered:?}, png encode {:?}",
        t.elapsed() - rendered
    );
    Ok(Response::ok("image/png", png))
}

// ------------------------------------------------------------- GetFeatureInfo

fn get_feature_info(ctx: &Context, version: &str) -> Result<Response, OwsError> {
    let p = ctx.params;
    let query_names = p.list("QUERY_LAYERS");
    if query_names.is_empty() {
        return Err(OwsError::bad(
            "MissingParameterValue",
            "QUERY_LAYERS parameter is missing",
        ));
    }
    let qlayers = layers_from(ctx.project, &query_names)?;
    let ms = map_settings(ctx, version, &qlayers)?;
    let (ik, jk) = if version == "1.1.1" {
        ("X", "Y")
    } else {
        ("I", "J")
    };
    let i = p
        .f64(ik)
        .or_else(|| p.f64(if ik == "I" { "X" } else { "I" }));
    let j = p
        .f64(jk)
        .or_else(|| p.f64(if jk == "J" { "Y" } else { "J" }));
    let (Some(i), Some(j)) = (i, j) else {
        return Err(OwsError::bad(
            "MissingParameterValue",
            format!("{ik}/{jk} parameters are required for GetFeatureInfo"),
        ));
    };
    let (x, y) = ms.to_map(i, j);
    let count = p
        .f64("FEATURE_COUNT")
        .map(|c| c.max(1.0) as usize)
        .unwrap_or(1);
    let with_geometry = p.bool("WITH_GEOMETRY");
    let filters = filters(ctx)?;
    let info_format = p.get_or("INFO_FORMAT", "text/plain").to_ascii_lowercase();

    struct Hit {
        layer: Arc<MapLayer>,
        features: Vec<crate::provider::Feature>,
    }
    let mut hits = vec![];
    // Report layers top-first, like QGIS.
    for l in qlayers.iter().rev() {
        if !l.identifiable {
            return Err(OwsError::bad(
                "LayerNotQueryable",
                format!("Layer '{}' is not queryable", ctx.project.nickname(l)),
            ));
        }
        // QgsRenderer::featureInfoSearchRect tolerances.
        let px_tol = |k: &str| p.f64(k).filter(|v| *v > 0.0).map(|v| v * ms.mupp());
        let tol = match l.geometry.as_str() {
            "Polygon" => px_tol("FI_POLYGON_TOLERANCE").unwrap_or(ms.extent.width() / 400.0),
            "Line" => px_tol("FI_LINE_TOLERANCE").unwrap_or(ms.extent.width() / 200.0),
            _ => px_tol("FI_POINT_TOLERANCE").unwrap_or(ms.extent.width() / 100.0),
        };
        let search = Rect::new(x - tol, y - tol, x + tol, y + tol);
        let nick = ctx.project.nickname(l);
        let filter = filters.iter().find(|(n, _)| *n == nick).map(|(_, e)| e);
        let mut fs = render::layer_features(l, &search, &ms.crs, filter)?;
        fs.retain(|f| {
            f.geometry
                .as_ref()
                .is_some_and(|g| geom::hits(g, x, y, tol))
        });
        fs.truncate(count);
        for f in &mut fs {
            f.attrs.retain(|k, _| !l.hidden_wms.contains(k));
        }
        hits.push(Hit {
            layer: l.clone(),
            features: fs,
        });
    }

    let project = ctx.project;
    if info_format.contains("json") {
        let mut features = vec![];
        for h in &hits {
            let nick = project.nickname(&h.layer);
            for f in &h.features {
                let props: serde_json::Map<String, Value> = f
                    .attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let geometry = if with_geometry {
                    f.geometry
                        .as_ref()
                        .map(|g| geom::to_geojson(g, 8))
                        .unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                features.push(json!({"type": "Feature", "id": format!("{nick}.{}", f.fid), "featureType": nick, "geometry": geometry, "properties": props}));
            }
        }
        let mut doc = json!({"type": "FeatureCollection", "features": features});
        // As observed from QGIS Server: a named CRS member is added unless the map is WGS84.
        if ms.crs != Crs::wgs84() {
            doc["crs"] = json!({"type": "name", "properties": {"name": ms.crs.urn()}});
        }
        return Ok(Response::json(&doc));
    }
    if info_format == "text/xml" || info_format == "application/vnd.ogc.gml" {
        let mut s =
            String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<GetFeatureInfoResponse>\n");
        for h in &hits {
            let _ = writeln!(s, " <Layer name=\"{}\">", esc(&project.nickname(&h.layer)));
            for f in &h.features {
                let _ = writeln!(s, "  <Feature id=\"{}\">", f.fid);
                for (k, v) in &f.attrs {
                    let _ = writeln!(
                        s,
                        "   <Attribute name=\"{}\" value=\"{}\"/>",
                        esc(k),
                        esc(&to_str(v))
                    );
                }
                if with_geometry && let Some(g) = &f.geometry {
                    let _ = writeln!(
                        s,
                        "   <Attribute name=\"geometry\" value=\"{}\" type=\"derived\"/>",
                        esc(&geom::to_geojson(g, 8).to_string())
                    );
                }
                s += "  </Feature>\n";
            }
            s += " </Layer>\n";
        }
        s += "</GetFeatureInfoResponse>\n";
        return Ok(Response::ok("text/xml; charset=utf-8", s));
    }
    if info_format == "text/html" {
        let mut s = String::from(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Information</title></head><body>\n",
        );
        for h in &hits {
            let _ = writeln!(
                s,
                "<table border=\"1\" width=\"100%\"><tr><th colspan=\"2\">Layer: {}</th></tr>",
                esc(&project.nickname(&h.layer))
            );
            for f in &h.features {
                let _ = writeln!(s, "<tr><th colspan=\"2\">Feature {}</th></tr>", f.fid);
                for (k, v) in &f.attrs {
                    let _ = writeln!(
                        s,
                        "<tr><th>{}</th><td>{}</td></tr>",
                        esc(k),
                        esc(&to_str(v))
                    );
                }
            }
            s += "</table>\n";
        }
        s += "</body></html>\n";
        return Ok(Response::ok("text/html; charset=utf-8", s));
    }
    if info_format != "text/plain" {
        return Err(OwsError::bad(
            "InvalidFormat",
            format!("Feature info format '{info_format}' is not supported"),
        ));
    }
    let mut s = String::from("GetFeatureInfo results\n\n");
    for h in &hits {
        let _ = writeln!(s, "Layer '{}'", project.nickname(&h.layer));
        for f in &h.features {
            let _ = writeln!(s, "Feature {}", f.fid);
            for (k, v) in &f.attrs {
                let _ = writeln!(s, "{k} = '{}'", to_str(v));
            }
        }
        s.push('\n');
    }
    Ok(Response::ok("text/plain; charset=utf-8", s))
}

// --------------------------------------------------------- GetLegendGraphic

fn get_legend(ctx: &Context) -> Result<Response, OwsError> {
    let p = ctx.params;
    let mut names = p.list("LAYERS");
    if names.is_empty() {
        names = p.list("LAYER");
    }
    if names.is_empty() {
        return Err(OwsError::bad(
            "MissingParameterValue",
            "LAYER is mandatory for GetLegendGraphic operation",
        ));
    }
    let dpi = dpi(ctx);
    let mm = |k: &str, d: f64| p.f64(k).unwrap_or(d) * dpi / 25.4;
    let (w, h) = (
        mm("SYMBOLWIDTH", 7.0).round().max(1.0) as u32,
        mm("SYMBOLHEIGHT", 4.0).round().max(1.0) as u32,
    );
    let mut layers = vec![];
    for n in &names {
        // Legend order follows the layer tree (top first).
        layers.extend(ctx.project.resolve(n).ok_or_else(|| {
            OwsError::bad(
                "LayerNotDefined",
                format!("The layer '{n}' does not exist."),
            )
        })?);
    }
    let rule = p.get("RULE");
    let format = p.get_or("FORMAT", "image/png").to_ascii_lowercase();

    if format.contains("json") {
        let b64 = |pm: &tiny_skia::Pixmap| -> Result<String, OwsError> {
            Ok(base64(&render::encode_png(pm)?))
        };
        let mut nodes = vec![];
        for l in &layers {
            let items = l.renderer.legend();
            let title = l.display_title().to_string();
            if items.len() == 1 && items[0].label.is_empty() {
                nodes.push(json!({"type": "layer", "title": title, "icon": b64(&render::symbol_patch(items[0].symbol, w, h, dpi))?}));
            } else {
                let mut symbols = vec![];
                for it in &items {
                    symbols.push(json!({"title": it.label, "icon": b64(&render::symbol_patch(it.symbol, w, h, dpi))?}));
                }
                nodes.push(json!({"type": "layer", "title": title, "symbols": symbols}));
            }
        }
        return Ok(Response::json(&json!({"title": "", "nodes": nodes})));
    }
    if !(format.starts_with("image/png") || format == "png") {
        return Err(OwsError::bad(
            "InvalidFormat",
            format!("Output format '{format}' is not supported"),
        ));
    }
    // PNG: symbol patches only (this server does not rasterise text).
    let mut patches = vec![];
    for l in &layers {
        for it in l.renderer.legend() {
            if rule.is_none_or(|r| r == it.label) {
                patches.push(render::symbol_patch(it.symbol, w, h, dpi));
            }
        }
    }
    if patches.is_empty() {
        // An unknown RULE is a client error; a layer with nothing to show (raster, labels
        // only) gets an empty image, not a 400: tnt-mapproxy's legend builder fetches every
        // project and treats tiny images as "no legend", errors as failures.
        if rule.is_some() {
            return Err(OwsError::bad("InvalidParameterValue", "No legend items found"));
        }
        let empty = tiny_skia::Pixmap::new(1, 1).expect("1x1");
        return Ok(Response::ok("image/png", render::encode_png(&empty)?));
    }
    let pad = if rule.is_some() { 0 } else { 2 };
    let total_h = patches.iter().map(|pm| pm.height() + pad).sum::<u32>() + pad;
    let mut out = tiny_skia::Pixmap::new(w + 2 * pad, total_h).expect("legend size");
    if !p.bool("TRANSPARENT") {
        out.fill(tiny_skia::Color::WHITE);
    }
    let mut y = pad;
    for pm in &patches {
        out.draw_pixmap(
            pad as i32,
            y as i32,
            pm.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
        y += pm.height() + pad;
    }
    Ok(Response::ok("image/png", render::encode_png(&out)?))
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                s.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

// ----------------------------------------------------------- GetCapabilities

fn bbox_xml(r: &Rect, crs_id: &str, v13: bool) -> Option<String> {
    let crs = Crs::from_ogc(crs_id).ok()?;
    if crs_id == "CRS:84" {
        return None;
    }
    let t = Transform::new(&Crs::wgs84(), &crs).rect(r).ok()?;
    let t = if v13 && crs.axis_inverted() {
        t.swapped()
    } else {
        t
    };
    let attr = if v13 { "CRS" } else { "SRS" };
    Some(format!(
        "<BoundingBox {attr}=\"{crs_id}\" minx=\"{}\" miny=\"{}\" maxx=\"{}\" maxy=\"{}\"/>",
        geom::round(t.xmin, 6),
        geom::round(t.ymin, 6),
        geom::round(t.xmax, 6),
        geom::round(t.ymax, 6)
    ))
}

fn extent_xml(s: &mut String, wgs84: &Rect, crs_list: &[String], v13: bool, indent: &str) {
    if wgs84.is_empty() {
        return;
    }
    let r = |v: f64| geom::round(v, 6);
    if v13 {
        let _ = writeln!(
            s,
            "{indent}<EX_GeographicBoundingBox><westBoundLongitude>{}</westBoundLongitude><eastBoundLongitude>{}</eastBoundLongitude><southBoundLatitude>{}</southBoundLatitude><northBoundLatitude>{}</northBoundLatitude></EX_GeographicBoundingBox>",
            r(wgs84.xmin),
            r(wgs84.xmax),
            r(wgs84.ymin),
            r(wgs84.ymax)
        );
    } else {
        let _ = writeln!(
            s,
            "{indent}<LatLonBoundingBox minx=\"{}\" miny=\"{}\" maxx=\"{}\" maxy=\"{}\"/>",
            r(wgs84.xmin),
            r(wgs84.ymin),
            r(wgs84.xmax),
            r(wgs84.ymax)
        );
    }
    for c in crs_list {
        if let Some(b) = bbox_xml(wgs84, c, v13) {
            let _ = writeln!(s, "{indent}{b}");
        }
    }
}

fn group_wgs84(project: &Project, g: &TreeGroup) -> Rect {
    let mut r = Rect::empty();
    for id in g.layer_ids() {
        if let Some(e) = project.layers.get(&id).and_then(|l| l.wgs84()) {
            r.union(&e);
        }
    }
    r
}

fn crs_xml(s: &mut String, crs_list: &[String], v13: bool, indent: &str) {
    let tag = if v13 { "CRS" } else { "SRS" };
    for c in crs_list {
        let _ = writeln!(s, "{indent}<{tag}>{}</{tag}>", esc(c));
    }
}

fn layer_xml(
    s: &mut String,
    ctx: &Context,
    l: &MapLayer,
    crs_list: &[String],
    v13: bool,
    depth: usize,
) {
    let ind = " ".repeat(depth);
    let project = ctx.project;
    let nick = project.nickname(l);
    let _ = writeln!(
        s,
        "{ind}<Layer queryable=\"{}\">",
        if l.identifiable { 1 } else { 0 }
    );
    let _ = writeln!(s, "{ind} <Name>{}</Name>", esc(&nick));
    let _ = writeln!(s, "{ind} <Title>{}</Title>", esc(l.display_title()));
    if let Some(a) = &l.abstract_ {
        let _ = writeln!(s, "{ind} <Abstract>{}</Abstract>", esc(a));
    }
    crs_xml(s, crs_list, v13, &format!("{ind} "));
    if let Some(w) = l.wgs84() {
        extent_xml(s, &w, crs_list, v13, &format!("{ind} "));
    }
    let legend = format!(
        "{}SERVICE=WMS&VERSION={}&REQUEST=GetLegendGraphic&LAYER={}&FORMAT=image/png&STYLE={}&SLD_VERSION=1.1.0",
        ctx.service_url,
        if v13 { "1.3.0" } else { "1.1.1" },
        urlencoding::encode(&nick),
        urlencoding::encode(&l.style_name)
    );
    let _ = writeln!(
        s,
        "{ind} <Style>\n{ind}  <Name>{0}</Name>\n{ind}  <Title>{0}</Title>\n{ind}  <LegendURL>\n{ind}   <Format>image/png</Format>\n{ind}   <OnlineResource xmlns:xlink=\"http://www.w3.org/1999/xlink\" xlink:type=\"simple\" xlink:href=\"{1}\"/>\n{ind}  </LegendURL>\n{ind} </Style>",
        esc(&l.style_name),
        esc(&legend)
    );
    if let Some((min, max)) = l.scale_range
        && v13
    {
        if max > 0.0 {
            let _ = writeln!(s, "{ind} <MinScaleDenominator>{max}</MinScaleDenominator>");
        }
        if min > 0.0 {
            let _ = writeln!(s, "{ind} <MaxScaleDenominator>{min}</MaxScaleDenominator>");
        }
    }
    let _ = writeln!(s, "{ind}</Layer>");
}

#[allow(clippy::too_many_arguments)]
fn group_xml(
    s: &mut String,
    ctx: &Context,
    g: &TreeGroup,
    name: &str,
    title: &str,
    crs_list: &[String],
    v13: bool,
    depth: usize,
) {
    let ind = " ".repeat(depth);
    let project = ctx.project;
    let _ = writeln!(s, "{ind}<Layer queryable=\"1\">");
    if !name.is_empty() && !(project.bool_prop("WMSSkipNameForGroup") && depth > 2) {
        let _ = writeln!(s, "{ind} <Name>{}</Name>", esc(name));
    }
    let _ = writeln!(s, "{ind} <Title>{}</Title>", esc(title));
    if depth == 2
        && let Some(a) = project.text_prop("WMSServiceAbstract")
    {
        let _ = writeln!(s, "{ind} <Abstract>{}</Abstract>", esc(a));
    }
    crs_xml(s, crs_list, v13, &format!("{ind} "));
    let wgs84 = if depth == 2 {
        // Root layer: WMSExtent (or all layers) in the project CRS.
        Transform::new(&project.crs, &Crs::wgs84())
            .rect(&project.extent())
            .unwrap_or_else(|_| group_wgs84(project, g))
    } else {
        group_wgs84(project, g)
    };
    extent_xml(s, &wgs84, crs_list, v13, &format!("{ind} "));
    // Opaque groups are published like a single layer (qgswmsgetcapabilities.cpp:796).
    let children: &[TreeNode] = if g.opaque { &[] } else { &g.children };
    for c in children {
        match c {
            TreeNode::Group(sub) => {
                if sub.layer_ids().iter().any(|id| {
                    project
                        .layers
                        .get(id)
                        .is_some_and(|l| !project.is_restricted(l))
                }) {
                    group_xml(
                        s,
                        ctx,
                        sub,
                        sub.nickname(),
                        sub.title.as_deref().unwrap_or(&sub.name),
                        crs_list,
                        v13,
                        depth + 1,
                    );
                }
            }
            TreeNode::Layer { id, .. } => {
                if let Some(l) = project.layers.get(id).filter(|l| !project.is_restricted(l)) {
                    layer_xml(s, ctx, l, crs_list, v13, depth + 1);
                }
            }
        }
    }
    let _ = writeln!(s, "{ind}</Layer>");
}

pub fn capabilities(ctx: &Context, version: &str) -> String {
    let v13 = version != "1.1.1";
    let project = ctx.project;
    let url = esc(&ctx.service_url);
    let mut s = String::new();
    if v13 {
        s += "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<WMS_Capabilities version=\"1.3.0\" xmlns=\"http://www.opengis.net/wms\" xmlns:sld=\"http://www.opengis.net/sld\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://www.opengis.net/wms http://schemas.opengis.net/wms/1.3.0/capabilities_1_3_0.xsd http://www.opengis.net/sld http://schemas.opengis.net/sld/1.1.0/sld_capabilities.xsd\">\n";
    } else {
        s += "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<WMT_MS_Capabilities version=\"1.1.1\" xmlns:xlink=\"http://www.w3.org/1999/xlink\">\n";
    }
    s += " <Service>\n  <Name>WMS</Name>\n";
    let _ = writeln!(s, "  <Title>{}</Title>", esc(&project.service_title()));
    if let Some(a) = project.text_prop("WMSServiceAbstract") {
        let _ = writeln!(s, "  <Abstract>{}</Abstract>", esc(a));
    }
    let keywords = project.list_prop("WMSKeywordList");
    if !keywords.is_empty() || v13 {
        s += "  <KeywordList>\n";
        if v13 {
            s += "   <Keyword vocabulary=\"ISO\">infoMapAccessService</Keyword>\n";
        }
        for k in keywords.iter().filter(|k| !k.is_empty()) {
            let _ = writeln!(s, "   <Keyword>{}</Keyword>", esc(k));
        }
        s += "  </KeywordList>\n";
    }
    let _ = writeln!(
        s,
        "  <OnlineResource xlink:type=\"simple\" xlink:href=\"{}\"/>",
        esc(project
            .text_prop("WMSOnlineResource")
            .unwrap_or(&ctx.service_url))
    );
    let contact = |k: &str| project.text_prop(k).map(esc);
    if let Some(person) = contact("WMSContactPerson").or(contact("WMSContactOrganization")) {
        s += "  <ContactInformation>\n   <ContactPersonPrimary>\n";
        let _ = writeln!(s, "    <ContactPerson>{person}</ContactPerson>");
        let _ = writeln!(
            s,
            "    <ContactOrganization>{}</ContactOrganization>",
            contact("WMSContactOrganization").unwrap_or_default()
        );
        s += "   </ContactPersonPrimary>\n";
        if let Some(pos) = contact("WMSContactPosition") {
            let _ = writeln!(s, "   <ContactPosition>{pos}</ContactPosition>");
        }
        if let Some(phone) = contact("WMSContactPhone") {
            let _ = writeln!(
                s,
                "   <ContactVoiceTelephone>{phone}</ContactVoiceTelephone>"
            );
        }
        if let Some(mail) = contact("WMSContactMail") {
            let _ = writeln!(
                s,
                "   <ContactElectronicMailAddress>{mail}</ContactElectronicMailAddress>"
            );
        }
        s += "  </ContactInformation>\n";
    }
    let _ = writeln!(
        s,
        "  <Fees>{}</Fees>",
        contact("WMSFees").unwrap_or_else(|| "conditions unknown".into())
    );
    let _ = writeln!(
        s,
        "  <AccessConstraints>{}</AccessConstraints>",
        contact("WMSAccessConstraints").unwrap_or_else(|| "None".into())
    );
    if v13 {
        if let Some(w) = project.int_prop("WMSMaxWidth") {
            let _ = writeln!(s, "  <MaxWidth>{w}</MaxWidth>");
        }
        if let Some(h) = project.int_prop("WMSMaxHeight") {
            let _ = writeln!(s, "  <MaxHeight>{h}</MaxHeight>");
        }
    }
    s += " </Service>\n <Capability>\n  <Request>\n";
    let get = format!(
        "<DCPType><HTTP><Get><OnlineResource xlink:type=\"simple\" xlink:href=\"{url}\"/></Get></HTTP></DCPType>"
    );
    let caps_format = if v13 {
        "text/xml"
    } else {
        "application/vnd.ogc.wms_xml"
    };
    let _ = writeln!(
        s,
        "   <GetCapabilities><Format>{caps_format}</Format>{get}</GetCapabilities>"
    );
    let _ = writeln!(s, "   <GetMap><Format>image/png</Format>{get}</GetMap>");
    let _ = writeln!(
        s,
        "   <GetFeatureInfo><Format>text/plain</Format><Format>text/html</Format><Format>text/xml</Format><Format>application/json</Format><Format>application/geo+json</Format>{get}</GetFeatureInfo>"
    );
    let lg = if v13 {
        "sld:GetLegendGraphic"
    } else {
        "GetLegendGraphic"
    };
    let _ = writeln!(
        s,
        "   <{lg}><Format>image/png</Format><Format>application/json</Format>{get}</{lg}>"
    );
    s += "  </Request>\n";
    let _ = writeln!(
        s,
        "  <Exception><Format>{}</Format></Exception>",
        if v13 {
            "XML"
        } else {
            "application/vnd.ogc.se_xml"
        }
    );
    let crs_list = project.crs_list();
    let crs_list: Vec<String> = if v13 {
        crs_list
    } else {
        crs_list.into_iter().filter(|c| c != "CRS:84").collect()
    };
    let root_title = project
        .text_prop("WMSRootTitle")
        .map(String::from)
        .unwrap_or_else(|| project.service_title());
    group_xml(
        &mut s,
        ctx,
        &project.tree,
        &project.root_name(),
        &root_title,
        &crs_list,
        v13,
        2,
    );
    s += " </Capability>\n";
    s += if v13 {
        "</WMS_Capabilities>\n"
    } else {
        "</WMT_MS_Capabilities>\n"
    };
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
