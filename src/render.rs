//! Map rendering (QgsMapSettings + QgsMapRendererJob equivalent) on tiny-skia.

use std::sync::Arc;

use anyhow::Result;
use geo_types::{Coord, Geometry, LineString};
use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, StrokeDash,
    Transform as SkTransform,
};
use tracing::{debug, warn};

use crate::crs::{Crs, Transform};
use crate::expression::{Attrs, Expr};
use crate::geom::{self, Rect};
use crate::project::MapLayer;
use crate::provider::Feature;
use crate::symbology::{Effect, RenderUnits, Rgba, Size, Stroke, Symbol, SymbolLayer};
use serde_json::json;

/// OGC reference pixel size (0.28 mm) as used by QgsWmsRenderContext::dotsPerMm.
pub const OGC_DPI: f64 = 25.4 / 0.28;

#[derive(Debug, Clone)]
pub struct MapSettings {
    /// Extent in `crs`, always x/east first.
    pub extent: Rect,
    pub width: u32,
    pub height: u32,
    pub dpi: f64,
    pub crs: Crs,
    pub background: Option<Rgba>,
    /// Project-scope expression variables (name without `@`, value).
    pub project_variables: Vec<(String, String)>,
    /// QgsMapSettings::extentBuffer (map units): layers and labels are processed this far
    /// beyond the visible extent (WMS `TILED=true` with the project's WMSTileBuffer).
    pub extent_buffer: f64,
    /// The project's labeling engine settings.
    pub labeling: crate::labeling::settings::EngineSettings,
}

impl MapSettings {
    pub fn mupp(&self) -> f64 {
        self.extent.width() / self.width as f64
    }

    /// Scale denominator (QgsScaleCalculator): ground width / paper width.
    pub fn scale(&self) -> f64 {
        let lat = (self.extent.ymin + self.extent.ymax) / 2.0;
        let ground = self.extent.width() * self.crs.meters_per_unit(lat);
        ground / (self.width as f64 / self.dpi * 0.0254)
    }

    pub fn units(&self) -> RenderUnits {
        let lat = (self.extent.ymin + self.extent.ymax) / 2.0;
        RenderUnits {
            dpi: self.dpi,
            mupp: self.mupp(),
            meters_per_unit: self.crs.meters_per_unit(lat),
        }
    }

    /// Pixel → map coordinates.
    pub fn to_map(&self, i: f64, j: f64) -> (f64, f64) {
        (
            self.extent.xmin + i * self.mupp(),
            self.extent.ymax - j * self.extent.height() / self.height as f64,
        )
    }
}

pub struct LayerJob {
    pub layer: Arc<MapLayer>,
    pub filter: Option<Expr>,
    pub opacity: f64,
}

/// Fetch the features of `layer` intersecting `extent` (given in `crs`), reprojected to `crs`.
pub fn layer_features(
    layer: &MapLayer,
    extent: &Rect,
    crs: &Crs,
    filter: Option<&Expr>,
) -> Result<Vec<Feature>> {
    let provider = layer.provider()?;
    let to_layer = Transform::new(crs, &layer.crs());
    // If the extent cannot be expressed in the layer CRS, fall back to reading everything.
    let query = to_layer.rect(extent).ok();
    let to_map = Transform::new(&layer.crs(), crs);
    let mut out = provider.features(query)?;
    out.retain(|f| filter.is_none_or(|e| e.matches(&f.attrs)));
    if !to_map.is_identity() {
        for f in &mut out {
            f.geometry = f.geometry.as_ref().map(|g| geom::transform(g, &to_map));
        }
    }
    Ok(out)
}

/// Features prepared for drawing, as QgsVectorLayerRenderer does: simplified in the layer
/// CRS with the layer's pixel tolerance, reprojected, then clipped to the (buffered) view.
///
/// With `keep_full`, the reprojected but unclipped geometry is kept too (for labeling).
type Prepared = (Feature, Option<Coord<f64>>, Option<Geometry<f64>>);

fn features_for_drawing(
    layer: &MapLayer,
    ms: &MapSettings,
    filter: Option<&Expr>,
    keep_full: bool,
) -> Result<Vec<Prepared>> {
    let provider = layer.provider()?;
    let layer_crs = layer.crs();
    let to_layer = Transform::new(&ms.crs, &layer_crs);
    let to_map = Transform::new(&layer_crs, &ms.crs);
    // QgsMapRendererJob renders the extent grown by the extent buffer.
    let b = ms.extent_buffer;
    let render_extent = Rect::new(
        ms.extent.xmin - b,
        ms.extent.ymin - b,
        ms.extent.xmax + b,
        ms.extent.ymax + b,
    );
    // Unlabelled layers: drop features no rule/class gives a symbol while reading, before
    // their geometry is decoded (lag24's airport rules keep a few dozen of ~250 000 N50
    // points in a 200 km view).
    let scale = ms.scale();
    let symbolled = !keep_full && !matches!(layer.renderer, crate::symbology::Renderer::Single(_));
    let keep = |a: &Attrs| {
        filter.is_none_or(|e| e.matches(a)) && (!symbolled || !layer.renderer.symbols_for(a, scale).is_empty())
    };
    let out = provider.features_where(to_layer.rect(&render_extent).ok(), Some(&keep))?;

    // The pixel tolerance in layer units, as QgsVectorLayerRenderer computes it: scale by the
    // diagonal ratio of a small square (0.0008983° or 100 units) at the centre of the extent
    // in the layer CRS, transformed to the map CRS.
    let e = &ms.extent;
    let tol = layer.simplify_tol.and_then(|threshold| {
        let mut tol = threshold * ms.mupp();
        if !to_map.is_identity() {
            let le = to_layer.rect(e).ok()?;
            let (cx, cy) = ((le.xmin + le.xmax) / 2.0, (le.ymin + le.ymax) / 2.0);
            let size = if layer_crs.geographic {
                0.0008983
            } else {
                100.0
            };
            let a = to_map.point(cx, cy).ok()?;
            let b = to_map.point(cx + size, cy + size).ok()?;
            let target = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            if target != 0.0 {
                tol *= size * std::f64::consts::SQRT_2 / target;
            }
        }
        Some(tol)
    });
    // Clip with a margin so the artificial edges along the clip boundary stay invisible.
    let m = 64.0 * ms.mupp() + b;
    let clip = Rect::new(e.xmin - m, e.ymin - m, e.xmax + m, e.ymax + m);
    let mut prepared = Vec::with_capacity(out.len());
    for mut f in out {
        let Some(g) = f.geometry.take() else { continue };
        let g = match tol {
            Some(t) => geom::simplify(&g, t),
            None => g,
        };
        // QgsSymbol::renderFeature: texture origin = (xmin, ymin) of the feature geometry as
        // delivered by the (simplifying) feature iterator, before clipping.
        let b = geom::bbox(&g);
        let origin = (!matches!(g, Geometry::Point(_) | Geometry::MultiPoint(_)) && !b.is_empty())
            .then(|| to_map.point(b.xmin, b.ymin).ok())
            .flatten()
            .map(|(x, y)| Coord { x, y });
        let (g, map_bbox) = if to_map.is_identity() {
            (g, b)
        } else {
            let g = geom::transform(&g, &to_map);
            let b = geom::bbox(&g);
            (g, b)
        };
        let full = keep_full.then(|| g.clone());
        if let Some(g) = geom::clip_owned(g, &map_bbox, &clip) {
            f.geometry = Some(g);
            prepared.push((f, origin, full));
        } else if let Some(full) = full {
            // Not drawn, but its label may still reach into the view.
            f.geometry = None;
            prepared.push((f, origin, Some(full)));
        }
    }
    Ok(prepared)
}

#[cfg(feature = "raster")]
fn render_raster(pm: &mut Pixmap, l: &MapLayer, ms: &MapSettings) -> Result<()> {
    if l.provider_key != "gdal" {
        warn!(
            "raster provider '{}' of layer {} is not supported",
            l.provider_key, l.name
        );
        return Ok(());
    }
    let Some(style) = &l.raster_style else {
        return Ok(());
    };
    let src = crate::raster::RasterSource::open(&l.datasource, &l.base_dir)?;
    crate::raster::render(pm, &src, style, &l.crs(), ms)
}

#[cfg(not(feature = "raster"))]
fn render_raster(_pm: &mut Pixmap, l: &MapLayer, _ms: &MapSettings) -> Result<()> {
    warn!(
        "layer {} is a raster layer; rGIS was built without the `raster` feature",
        l.name
    );
    Ok(())
}

fn ignore_rendering_errors() -> bool {
    std::env::var("QGIS_SERVER_IGNORE_RENDERING_ERRORS")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true"))
}

/// fill_path that skips zero-area paths (horizontal/vertical lines, collapsed slivers):
/// tiny-skia refuses them with a warning per call (thousands per hour), QGIS paints
/// nothing for them either.
pub trait FillNonEmpty {
    fn fill_nonempty(
        &mut self,
        path: &tiny_skia::Path,
        paint: &tiny_skia::Paint,
        rule: tiny_skia::FillRule,
        t: tiny_skia::Transform,
        mask: Option<&tiny_skia::Mask>,
    );
}

impl FillNonEmpty for Pixmap {
    fn fill_nonempty(
        &mut self,
        path: &tiny_skia::Path,
        paint: &tiny_skia::Paint,
        rule: tiny_skia::FillRule,
        t: tiny_skia::Transform,
        mask: Option<&tiny_skia::Mask>,
    ) {
        let b = path.bounds();
        if b.width() > 0.0 && b.height() > 0.0 {
            self.fill_path(path, paint, rule, t, mask);
        }
    }
}

/// Bounds the pixels being rendered at once. Each request's working set (output, per-layer
/// scratch pixmaps, PNG buffer) scales with its pixel count, and spawn_blocking alone would
/// run hundreds of renders in parallel (QGIS Server is bounded by its worker count instead).
/// RGIS_RENDER_BUDGET_MPX sets the budget in megapixels (default 16); a request larger than
/// the budget waits for the whole budget.
pub struct RenderGate {
    budget: u64,
    used: parking_lot::Mutex<u64>,
    freed: parking_lot::Condvar,
}

static GATE: std::sync::LazyLock<RenderGate> = std::sync::LazyLock::new(|| RenderGate {
    budget: std::env::var("RGIS_RENDER_BUDGET_MPX")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .map_or(16_000_000, |v| (v * 1e6) as u64),
    used: Default::default(),
    freed: Default::default(),
});

pub struct RenderPermit(u64);

impl Drop for RenderPermit {
    fn drop(&mut self) {
        *GATE.used.lock() -= self.0;
        GATE.freed.notify_all();
    }
}

/// Wait until `width`×`height` pixels fit in the render budget.
pub fn render_permit(width: u32, height: u32) -> RenderPermit {
    let need = (width as u64 * height as u64).clamp(1, GATE.budget);
    let mut used = GATE.used.lock();
    let started = std::time::Instant::now();
    while *used + need > GATE.budget {
        GATE.freed.wait(&mut used);
    }
    *used += need;
    let waited = started.elapsed();
    if waited.as_millis() > 100 {
        debug!("render gate: waited {waited:?} for {need} px");
    }
    RenderPermit(need)
}

/// An ignored rendering error, logged once per distinct message (an unsupported layer
/// fails identically on every request), then only at debug level.
fn warn_ignored(e: &anyhow::Error) {
    static SEEN: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    let msg = format!("{e:#}");
    if SEEN.lock().insert(msg.clone()) {
        warn!("Rendering error: {msg} (ignored; repeats logged at debug level)");
    } else {
        debug!("Rendering error: {msg} (ignored)");
    }
}

/// Render layers bottom-to-top into a new pixmap.
pub fn render(jobs: &[LayerJob], ms: &MapSettings) -> Result<Pixmap> {
    let mut pm =
        Pixmap::new(ms.width, ms.height).ok_or_else(|| anyhow::anyhow!("invalid image size"))?;
    if let Some(bg) = ms.background {
        pm.fill(tiny_skia::Color::from_rgba8(bg.0, bg.1, bg.2, bg.3));
    }
    let scale = ms.scale();
    let mut labels = jobs
        .iter()
        .any(|j| !j.layer.labeling.is_empty())
        .then(|| crate::labeling::Engine::new(ms, ms.labeling.clone()));
    // Vector layers whose features are read, decided up front so their (I/O-bound) reads
    // can run ahead of drawing: cold remote FlatGeobufs cost a few index round trips each,
    // which add up layer by layer otherwise (lag06 z11 cold: 4 layers, 3.9 s → ~1.5 s).
    let reads: Vec<Option<bool>> = jobs
        .iter()
        .map(|job| {
            let l = &job.layer;
            if !l.visible_at(scale) || l.kind != "vector" {
                return None;
            }
            let labeled = labels
                .as_ref()
                .is_some_and(|e| l.labeling.iter().any(|r| e.rule_active(r)));
            (labeled || l.renderer.active_at(scale)).then_some(labeled)
        })
        .collect();
    let order: Vec<usize> = (0..jobs.len()).filter(|&i| reads[i].is_some()).collect();
    let ahead = Lookahead::new(order.len());
    std::thread::scope(|s| {
        // Loaders take layers in drawing order, at most LOOKAHEAD ahead of the one being
        // drawn, so memory stays a few layers' features, not the whole map's.
        for _ in 0..LOOKAHEAD.min(order.len().saturating_sub(1)) {
            s.spawn(|| {
                while let Some(k) = ahead.next_job() {
                    let job = &jobs[order[k]];
                    let labeled = reads[order[k]].unwrap_or(false);
                    ahead.put(k, features_for_drawing(&job.layer, ms, job.filter.as_ref(), labeled));
                }
            });
        }
        // Cancel on every exit (errors, panics) so waiting loaders end and the scope joins.
        struct Cancel<'a>(&'a Lookahead);
        impl Drop for Cancel<'_> {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel = Cancel(&ahead);
        render_layers(&mut pm, jobs, ms, scale, &mut labels, &reads, &order, &ahead)
    })?;
    if let Some(engine) = labels.filter(|e| e.has_layers()) {
        let started = std::time::Instant::now();
        engine.draw(&mut pm);
        debug!("labeling took {:?}", started.elapsed());
    }
    Ok(pm)
}

const LOOKAHEAD: usize = 4;

/// Feature reads handed from loader threads to the drawing loop, in drawing order.
struct Lookahead {
    state: parking_lot::Mutex<LookaheadState>,
    ready: parking_lot::Condvar,
}

struct LookaheadState {
    slots: Vec<Option<Result<Vec<Prepared>>>>,
    next: usize,
    drawn: usize,
    cancelled: bool,
}

impl Lookahead {
    fn new(n: usize) -> Self {
        Lookahead {
            state: parking_lot::Mutex::new(LookaheadState {
                slots: (0..n).map(|_| None).collect(),
                next: 0,
                drawn: 0,
                cancelled: false,
            }),
            ready: parking_lot::Condvar::new(),
        }
    }

    /// The next read for a loader, waiting while it would run too far ahead.
    fn next_job(&self) -> Option<usize> {
        let mut st = self.state.lock();
        loop {
            if st.cancelled || st.next >= st.slots.len() {
                return None;
            }
            if st.next < st.drawn + LOOKAHEAD + 1 {
                st.next += 1;
                return Some(st.next - 1);
            }
            self.ready.wait(&mut st);
        }
    }

    fn put(&self, k: usize, r: Result<Vec<Prepared>>) {
        self.state.lock().slots[k] = Some(r);
        self.ready.notify_all();
    }

    /// Read `k` (in order): taken from a loader, or read here when none has claimed it.
    fn take(&self, k: usize, read: impl FnOnce() -> Result<Vec<Prepared>>) -> Result<Vec<Prepared>> {
        let mut st = self.state.lock();
        st.drawn = k;
        self.ready.notify_all();
        if st.next <= k {
            st.next = k + 1;
            drop(st);
            return read();
        }
        loop {
            if let Some(r) = st.slots[k].take() {
                return r;
            }
            self.ready.wait(&mut st);
        }
    }

    fn cancel(&self) {
        self.state.lock().cancelled = true;
        self.ready.notify_all();
    }
}

#[allow(clippy::too_many_arguments)]
fn render_layers(
    pm: &mut Pixmap,
    jobs: &[LayerJob],
    ms: &MapSettings,
    scale: f64,
    labels: &mut Option<crate::labeling::Engine>,
    reads: &[Option<bool>],
    read_order: &[usize],
    ahead: &Lookahead,
) -> Result<()> {
    let mut k = 0;
    for (order, job) in jobs.iter().enumerate() {
        let l = &job.layer;
        if !l.visible_at(scale) {
            debug!("layer {} not visible at 1:{scale:.0}", l.name);
            continue;
        }
        if l.kind == "raster" {
            if let Err(e) = render_raster(pm, l, ms) {
                if ignore_rendering_errors() {
                    warn_ignored(&e);
                } else {
                    return Err(e);
                }
            }
            continue;
        }
        if l.kind != "vector" {
            warn!("layer {} is a {} layer; not rendered", l.name, l.kind);
            continue;
        }
        let started = std::time::Instant::now();
        // Nothing to draw or label at this scale: skip before reading any data (a layer
        // without a scale limit whose rules are all out of scale otherwise loads every
        // feature in the extent — 40 s and >1 GB for national N50 points at 1:2.4M).
        let Some(labeled) = reads[order] else {
            debug!("layer {} has no active rule at 1:{scale:.0}", l.name);
            continue;
        };
        debug_assert_eq!(order, read_order[k]);
        let read = ahead.take(k, || features_for_drawing(l, ms, job.filter.as_ref(), labeled));
        k += 1;
        let features = match read {
            Ok(f) => f,
            // QgsServerSettings::ignoreRenderingErrors
            Err(e) if ignore_rendering_errors() => {
                warn_ignored(&e);
                continue;
            }
            Err(e) => return Err(e),
        };
        let opacity = (l.opacity * job.opacity).clamp(0.0, 1.0);
        if opacity < 1.0 {
            let mut tmp = Pixmap::new(ms.width, ms.height).expect("same size as output");
            draw_features(&mut tmp, l, &features, ms, scale);
            pm.draw_pixmap(
                0,
                0,
                tmp.as_ref(),
                &PixmapPaint {
                    opacity: opacity as f32,
                    ..Default::default()
                },
                SkTransform::identity(),
                None,
            );
        } else {
            draw_features(pm, l, &features, ms, scale);
        }
        debug!(
            "rendered {} ({} features) in {:?}",
            l.name,
            features.len(),
            started.elapsed()
        );
        if let (true, Some(engine)) = (labeled, labels.as_mut()) {
            // Labels are registered for rendered features (QgsVectorLayerRenderer::drawRenderer).
            let sources: Vec<crate::labeling::LabelSource> = features
                .iter()
                .filter_map(|(f, _, full)| {
                    let symbols = l.renderer.symbols_for(&f.attrs, scale);
                    let rendered = !symbols.is_empty()
                        || matches!(l.renderer, crate::symbology::Renderer::Null);
                    Some(crate::labeling::LabelSource {
                        fid: f.fid,
                        attrs: &f.attrs,
                        geometry: full.as_ref().filter(|_| rendered)?,
                        symbols,
                    })
                })
                .collect();
            engine.register_layer(l, &sources, &layer_vars(l, ms, scale), order);
        }
    }
    Ok(())
}

/// Expression context scopes as QGIS stacks them: project < map settings < layer.
fn layer_vars(l: &MapLayer, ms: &MapSettings, scale: f64) -> Attrs {
    let mut vars = Attrs::default();
    for (k, v) in &ms.project_variables {
        vars.insert(format!("@{k}"), json!(v));
    }
    vars.insert("@map_id".into(), json!("canvas"));
    vars.insert("@map_scale".into(), json!(scale));
    vars.insert("@map_rotation".into(), json!(0));
    vars.insert("@map_crs".into(), json!(ms.crs.authid));
    vars.insert(
        "@map_units".into(),
        json!(if ms.crs.geographic {
            "degrees"
        } else {
            "meters"
        }),
    );
    vars.insert("@layer_name".into(), json!(l.name));
    vars.insert("@layer_id".into(), json!(l.id));
    for (k, v) in &l.variables {
        vars.insert(format!("@{k}"), json!(v));
    }
    vars
}

fn draw_features(
    pm: &mut Pixmap,
    l: &MapLayer,
    features: &[Prepared],
    ms: &MapSettings,
    scale: f64,
) {
    let units = ms.units();
    let sx = 1.0 / ms.mupp();
    let sy = ms.height as f64 / ms.extent.height();
    let to_px = |c: &Coord<f64>| -> (f32, f32) {
        let x = ((c.x - ms.extent.xmin) * sx).clamp(-1e6, 1e6);
        let y = ((ms.extent.ymax - c.y) * sy).clamp(-1e6, 1e6);
        (x as f32, y as f32)
    };
    let vars = layer_vars(l, ms, scale);
    let empty = Attrs::default();
    for (f, origin, _) in features {
        let Some(g) = &f.geometry else { continue };
        let origin = origin.as_ref().map(to_px);
        let symbols = l.renderer.symbols_for(&f.attrs, scale);
        // Only symbols with data-defined properties need the merged context.
        let ctx = if symbols.iter().any(|s| s.data_defined) {
            let mut c = f.attrs.clone();
            c.extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
            c.insert("$id".into(), json!(f.fid));
            Some(c)
        } else {
            None
        };
        for sym in symbols {
            draw_symbol(
                pm,
                sym,
                g,
                &to_px,
                &units,
                ctx.as_ref().unwrap_or(&empty),
                origin,
            );
        }
    }
}

/// Append rings/lines to a path in pixel space, dropping sub-pixel vertex steps
/// (QGIS simplifies at render time with a 1 px tolerance too).
fn add_line(
    pb: &mut PathBuilder,
    l: &LineString<f64>,
    to_px: &dyn Fn(&Coord<f64>) -> (f32, f32),
    close: bool,
) {
    // Besides the QGIS-style simplification (geom::simplify), dropping sub-half-pixel steps
    // here matches QGIS output better (measured with tools/parity.py: 0.54% vs 0.63% on
    // Natural Earth countries), presumably as Qt's path rasterisation does similar culling.
    let mut last: Option<(f32, f32)> = None;
    let n = l.0.len();
    for (i, c) in l.0.iter().enumerate() {
        let p = to_px(c);
        match last {
            None => pb.move_to(p.0, p.1),
            Some(q) if i + 1 < n && (p.0 - q.0).abs() < 0.5 && (p.1 - q.1).abs() < 0.5 => continue,
            Some(_) => pb.line_to(p.0, p.1),
        }
        last = Some(p);
    }
    if close && last.is_some() {
        pb.close();
    }
}

fn polygons(g: &Geometry<f64>) -> Vec<&geo_types::Polygon<f64>> {
    match g {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(m) => m.0.iter().collect(),
        Geometry::GeometryCollection(gc) => gc.0.iter().flat_map(polygons).collect(),
        _ => vec![],
    }
}

fn lines(g: &Geometry<f64>) -> Vec<&LineString<f64>> {
    match g {
        Geometry::LineString(l) => vec![l],
        Geometry::MultiLineString(m) => m.0.iter().collect(),
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) => polygons(g)
            .into_iter()
            .flat_map(geom::polygon_rings)
            .collect(),
        Geometry::GeometryCollection(gc) => gc.0.iter().flat_map(lines).collect(),
        _ => vec![],
    }
}

fn points(g: &Geometry<f64>) -> Vec<Coord<f64>> {
    match g {
        Geometry::Point(p) => vec![p.0],
        Geometry::MultiPoint(m) => m.0.iter().map(|p| p.0).collect(),
        Geometry::GeometryCollection(gc) => gc.0.iter().flat_map(points).collect(),
        other => {
            let b = geom::bbox(other);
            if b.is_empty() {
                vec![]
            } else {
                vec![Coord {
                    x: (b.xmin + b.xmax) / 2.0,
                    y: (b.ymin + b.ymax) / 2.0,
                }]
            }
        }
    }
}

fn pattern_tile(bits: &[u8; 8], c: Rgba, opacity: f64) -> Pixmap {
    let mut tile = Pixmap::new(8, 8).expect("8x8");
    let a = (c.3 as f64 * opacity).round().clamp(0.0, 255.0) as u8;
    let on = tiny_skia::ColorU8::from_rgba(c.0, c.1, c.2, a).premultiply();
    for (y, row) in bits.iter().enumerate() {
        for x in 0..8 {
            if row >> x & 1 == 1 {
                tile.pixels_mut()[y * 8 + x] = on;
            }
        }
    }
    tile
}

fn paint(c: Rgba, opacity: f64) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(
        c.0,
        c.1,
        c.2,
        (c.3 as f64 * opacity).round().clamp(0.0, 255.0) as u8,
    );
    p.anti_alias = true;
    p
}

fn sk_stroke(s: &Stroke, u: &RenderUnits) -> tiny_skia::Stroke {
    let width = s.width.px(u) as f32;
    let mut st = tiny_skia::Stroke {
        width,
        line_join: match s.join.as_str() {
            "miter" => LineJoin::Miter,
            "round" => LineJoin::Round,
            _ => LineJoin::Bevel,
        },
        line_cap: match s.cap.as_str() {
            "flat" => LineCap::Butt,
            "round" => LineCap::Round,
            _ => LineCap::Square,
        },
        ..Default::default()
    };
    let pattern: Vec<f32> = match &s.custom_dash {
        Some((d, unit)) => d
            .iter()
            .map(|v| {
                crate::symbology::Size {
                    value: *v,
                    unit: *unit,
                }
                .px(u) as f32
            })
            .collect(),
        None => s
            .dash
            .iter()
            .map(|v| (*v as f32) * width.max(1.0))
            .collect(),
    };
    if pattern.len() >= 2 && pattern.len().is_multiple_of(2) {
        st.dash = StrokeDash::new(pattern, 0.0);
    }
    st
}

pub fn draw_symbol(
    pm: &mut Pixmap,
    sym: &Symbol,
    g: &Geometry<f64>,
    to_px: &dyn Fn(&Coord<f64>) -> (f32, f32),
    u: &RenderUnits,
    ctx: &Attrs,
    // Pixel position of the feature's (xmin, ymin): image fills in feature mode anchor here.
    origin: Option<(f32, f32)>,
) {
    for layer in &sym.layers {
        // Data-defined overrides for this feature (or the layer as is).
        let Some(kind) = layer.resolve(ctx) else {
            continue;
        };
        if !layer.effects.is_empty() {
            draw_with_effects(pm, sym, kind.into_owned(), &layer.effects, g, to_px, u, ctx, origin);
            continue;
        }
        match kind.as_ref() {
            SymbolLayer::SimpleFill {
                color,
                filled,
                pattern,
                stroke,
            } => {
                let polys = polygons(g);
                if polys.is_empty() {
                    continue;
                }
                let mut pb = PathBuilder::new();
                for p in &polys {
                    for r in geom::polygon_rings(p) {
                        add_line(&mut pb, r, to_px, true);
                    }
                }
                let Some(path) = pb.finish() else { continue };
                if *filled {
                    let tile;
                    let mut fill = paint(*color, sym.opacity);
                    if let Some(bits) = pattern {
                        // Qt pattern brush: the 8×8 bitmap repeats from the device origin,
                        // modulated by the anti-aliased polygon coverage.
                        tile = pattern_tile(bits, *color, sym.opacity);
                        fill.shader = tiny_skia::Pattern::new(
                            tile.as_ref(),
                            tiny_skia::SpreadMode::Repeat,
                            tiny_skia::FilterQuality::Nearest,
                            1.0,
                            SkTransform::identity(),
                        );
                    }
                    pm.fill_nonempty(
                        &path,
                        &fill,
                        FillRule::EvenOdd,
                        SkTransform::identity(),
                        None,
                    );
                }
                if stroke.visible {
                    pm.stroke_path(
                        &path,
                        &paint(stroke.color, sym.opacity),
                        &sk_stroke(stroke, u),
                        SkTransform::identity(),
                        None,
                    );
                }
            }
            SymbolLayer::ShapeburstFill { color1, color2, max_distance, ignore_rings, offset, .. } => {
                let off = (offset.0.px(u) as f32, offset.1.px(u) as f32);
                for p in polygons(g) {
                    let rings: Vec<Vec<(f32, f32)>> = geom::polygon_rings(p)
                        .map(|r| r.0.iter().map(to_px).collect())
                        .collect();
                    let max_px = max_distance.as_ref().map(|d| d.px(u).round() as i32);
                    draw_shapeburst(pm, &rings, *color1, *color2, max_px, *ignore_rings, off, sym.opacity);
                }
            }
            SymbolLayer::LinePatternFill {
                angle,
                distance,
                offset,
                feature_ref,
                image_brush,
                line,
            } => {
                let polys = polygons(g);
                if polys.is_empty() {
                    continue;
                }
                let mut pb = PathBuilder::new();
                for p in &polys {
                    for r in geom::polygon_rings(p) {
                        add_line(&mut pb, r, to_px, true);
                    }
                }
                let Some(path) = pb.finish() else { continue };
                let tile = if *image_brush {
                    line_pattern_tile(*angle, distance.px(u), offset.px(u), line, u, ctx)
                } else {
                    None
                };
                match tile {
                    Some(tile) => {
                        // Feature mode anchors the texture at the feature's (xmin, ymin),
                        // snapped to whole pixels (verified pixel-identical against QGIS).
                        let anchor = match (feature_ref, origin) {
                            (true, Some((x, y))) => {
                                SkTransform::from_translate(x.round(), y.round())
                            }
                            _ => SkTransform::identity(),
                        };
                        let fill = Paint {
                            anti_alias: true,
                            shader: tiny_skia::Pattern::new(
                                tile.as_ref(),
                                tiny_skia::SpreadMode::Repeat,
                                tiny_skia::FilterQuality::Nearest,
                                sym.opacity as f32,
                                anchor,
                            ),
                            ..Default::default()
                        };
                        pm.fill_nonempty(
                            &path,
                            &fill,
                            FillRule::EvenOdd,
                            SkTransform::identity(),
                            None,
                        );
                    }
                    None => draw_pattern_lines(
                        pm,
                        &path,
                        *angle,
                        distance.px(u),
                        offset.px(u),
                        line,
                        u,
                        ctx,
                    ),
                }
            }
            SymbolLayer::SimpleLine { stroke, .. } => {
                if !stroke.visible {
                    continue;
                }
                let mut pb = PathBuilder::new();
                for l in lines(g) {
                    add_line(&mut pb, l, to_px, false);
                }
                if let Some(path) = pb.finish() {
                    pm.stroke_path(
                        &path,
                        &paint(stroke.color, sym.opacity),
                        &sk_stroke(stroke, u),
                        SkTransform::identity(),
                        None,
                    );
                }
            }
            SymbolLayer::TemplatedLine {
                on_interval,
                on_first,
                on_last,
                every_part,
                offset,
                interval,
                offset_along,
                average_over,
                rotate,
                hash,
                sub,
            } => {
                // QgsMarkerLineSymbolLayer::renderPolyline: the sub-symbol at the parent's opacity.
                let mut sub = (**sub).clone();
                sub.opacity *= sym.opacity;
                let iv = interval.px(u);
                let along = offset_along.px(u);
                let avg = average_over.px(u) / 2.0;
                let parts = lines(g);
                let n_parts = parts.len();
                for (part, l) in parts.into_iter().enumerate() {
                    let pts: Vec<(f64, f64)> = l
                        .0
                        .iter()
                        .map(|c| {
                            let (x, y) = to_px(c);
                            (x as f64, y as f64)
                        })
                        .collect();
                    if pts.is_empty() {
                        continue;
                    }
                    // QgsSymbolLayerUtils::offsetLine: symbols follow the offset curve.
                    let off = offset.px(u);
                    let pts = if off != 0.0 { geom::offset_polyline(&pts, off, 2.0) } else { pts };
                    // QgsTemplatedLineSymbolLayerBase::renderPolyline order: interval, first, last.
                    let mut placed = vec![];
                    if *on_interval {
                        placed.extend(interval_points(&pts, iv, along, avg));
                    }
                    if *on_first && (*every_part || part == 0) {
                        placed.push((pts[0], end_angle(&pts, true)));
                    }
                    // Not on every part: QGIS defers it to the end of the feature (last part).
                    if *on_last && (*every_part || part + 1 == n_parts) {
                        placed.push((pts[pts.len() - 1], end_angle(&pts, false)));
                    }
                    for ((x, y), line_angle) in placed {
                        let a = if *rotate { line_angle } else { 0.0 };
                        match hash {
                            // QgsHashedLineSymbolLayer::renderSymbol (QgsPointXY::project)
                            Some((len, hash_angle)) => {
                                let w = len.px(u) / 2.0;
                                let b = (180.0 - (a + hash_angle)).to_radians();
                                let (dx, dy) = (w * b.sin(), w * b.cos());
                                draw_line_px(pm, &sub, (x + dx, y + dy), (x - dx, y - dy), u, ctx);
                            }
                            None => draw_point_symbol(pm, &sub, (x as f32, y as f32), a, u, ctx),
                        }
                    }
                }
            }
            SymbolLayer::SvgMarker { .. } | SymbolLayer::Marker { .. } | SymbolLayer::FontMarker { .. } => {
                for c in points(g) {
                    draw_marker_layer(pm, kind.as_ref(), sym.opacity, to_px(&c), 0.0, u);
                }
            }
        }
    }
}

/// A marker symbol at one pixel position, every layer turned by `line_angle` degrees
/// clockwise (QgsMarkerSymbol::setLineAngle: the layer angle plus the line angle).
fn draw_point_symbol(pm: &mut Pixmap, sym: &Symbol, at: (f32, f32), line_angle: f64, u: &RenderUnits, ctx: &Attrs) {
    for layer in &sym.layers {
        let Some(kind) = layer.resolve(ctx) else { continue };
        if !draw_marker_layer(pm, kind.as_ref(), sym.opacity, at, line_angle, u) {
            // Other layer types in a marker sub-symbol: as a point symbol, unrotated.
            let one = Symbol {
                kind: sym.kind.clone(),
                opacity: sym.opacity,
                data_defined: false,
                layers: vec![crate::symbology::Layer { kind: kind.into_owned(), dd: Default::default(), effects: vec![] }],
            };
            let g: Geometry<f64> = geo_types::Point::new(at.0 as f64, at.1 as f64).into();
            draw_symbol(pm, &one, &g, &|c| (c.x as f32, c.y as f32), u, ctx, None);
        }
    }
}

/// QgsMarkerSymbolLayer::markerOffset + _rotatedOffset: where a `w`×`h` marker's centre
/// goes relative to its point — the layer offset plus half the size towards the anchor
/// point, turned with the marker's total angle.
fn marker_shift(offset: (f32, f32), anchor: (u8, u8), w: f64, h: f64, angle: f64) -> (f32, f32) {
    let (mut x, mut y) = (offset.0 as f64, offset.1 as f64);
    match anchor.0 {
        0 => x += w / 2.0,
        2 => x -= w / 2.0,
        _ => {}
    }
    match anchor.1 {
        0 => y += h / 2.0,
        2 => y -= h / 2.0,
        _ => {}
    }
    if angle != 0.0 {
        let (s, c) = angle.to_radians().sin_cos();
        (x, y) = (x * c - y * s, x * s + y * c);
    }
    (x as f32, y as f32)
}

/// Draw one SVG or simple marker layer at a pixel position; false for other layer types.
fn draw_marker_layer(pm: &mut Pixmap, kind: &SymbolLayer, opacity: f64, (x, y): (f32, f32), line_angle: f64, u: &RenderUnits) -> bool {
    match kind {
        SymbolLayer::SvgMarker { path, size, fixed_aspect, fill, stroke_color, stroke_width, angle, offset, anchor, .. } => {
            let total = angle + line_angle;
            let w = size.px(u);
            let h = if anchor.1 == 1 {
                w // unused
            } else if *fixed_aspect > 0.0 {
                w * fixed_aspect
            } else {
                w * path.as_deref().and_then(crate::svg::aspect).unwrap_or(1.0)
            };
            let marker = crate::svg::Marker {
                path: path.as_deref(),
                width: w,
                fixed_aspect: *fixed_aspect,
                fill: *fill,
                stroke: *stroke_color,
                stroke_width: stroke_width.px(u),
                angle: total,
                opacity,
            };
            let (ox, oy) = marker_shift((offset.0.px(u) as f32, offset.1.px(u) as f32), *anchor, w, h, total);
            crate::svg::draw(pm, &marker, x + ox, y + oy);
            true
        }
        SymbolLayer::Marker { shape, width, height, color, stroke, angle, offset, anchor, .. } => {
            let (w, h) = (width.px(u), height.px(u));
            // Zero-size markers are a common way to make label-only layers.
            if w <= 0.0 || h <= 0.0 {
                return true;
            }
            let total = angle + line_angle;
            let (ox, oy) = marker_shift((offset.0.px(u) as f32, offset.1.px(u) as f32), *anchor, w, h, total);
            let place = SkTransform::from_scale((w / 2.0) as f32, (h / 2.0) as f32)
                .post_rotate(total as f32)
                .post_translate(x + ox, y + oy);
            draw_marker(pm, shape, place, *color, stroke, opacity, u);
            true
        }
        SymbolLayer::FontMarker { font, style, chr, size, color, stroke, angle, offset, anchor } => {
            // QgsFontMarkerSymbolLayer::startRender / renderPoint: the font at the rounded pixel
            // size (at least 2), the character centred on half its advance and half the
            // ascent (QPainterPath::addText(-w/2, ascent/2)), then rotated and placed.
            let px = size.px(u);
            if px.abs() < 1e-9 {
                return true;
            }
            let req = crate::text::FontRequest { family: font.clone(), weight: 400, italic: false, style: style.clone() };
            let Some(f) = crate::text::font(&req, (px.round() as i32).max(2)) else {
                return true;
            };
            let total = angle + line_angle;
            let (ox, oy) = marker_shift((offset.0.px(u) as f32, offset.1.px(u) as f32), *anchor, px, px, total);
            let ts = SkTransform::from_translate(-(f.horizontal_advance(chr) / 2.0) as f32, (f.ascent / 2.0) as f32)
                .post_rotate(total as f32)
                .post_translate(x + ox, y + oy);
            let mut pb = PathBuilder::new();
            f.append_run(&mut pb, &f.shape(chr), ts);
            let Some(path) = pb.finish() else {
                return true;
            };
            pm.fill_nonempty(&path, &paint(*color, opacity), FillRule::Winding, SkTransform::identity(), None);
            if stroke.visible && stroke.width.px(u) > 0.0 {
                pm.stroke_path(&path, &paint(stroke.color, opacity), &sk_stroke(stroke, u), SkTransform::identity(), None);
            }
            true
        }
        _ => false,
    }
}

/// QgsEffectStack::draw for one feature's symbol layer: the layer alone in a scratch
/// image reaching past the canvas (so blurs and shadows see the off-canvas shape, as the
/// QPicture-based effects do), then each effect composited in stack order.
#[allow(clippy::too_many_arguments)]
fn draw_with_effects(
    pm: &mut Pixmap,
    sym: &Symbol,
    kind: SymbolLayer,
    effects: &[Effect],
    g: &Geometry<f64>,
    to_px: &dyn Fn(&Coord<f64>) -> (f32, f32),
    u: &RenderUnits,
    ctx: &Attrs,
    origin: Option<(f32, f32)>,
) {
    // QgsShadowEffect/QgsGlowEffect::boundingRect: offset or spread + 2 × blur + 10.
    let blur_px = |b: &Size| b.px(u).round().max(0.0) as i32;
    let reach = effects
        .iter()
        .map(|e| match e {
            Effect::Shadow { blur, offset, .. } => offset.px(u).abs().ceil() as i32 + 2 * blur_px(blur) + 10,
            Effect::OuterGlow { spread, blur, .. } => spread.px(u).ceil() as i32 + 2 * blur_px(blur) + 10,
            Effect::DrawSource { .. } => 0,
        })
        .max()
        .unwrap_or(0);
    // The geometry's pixel extent, plus room for markers drawn around their point.
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    geom::for_each_coord(g, &mut |c| {
        let (x, y) = to_px(&c);
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    });
    if x0 > x1 {
        return;
    }
    let pad = reach + 64;
    let l = (x0.floor() as i32 - pad).max(-reach);
    let t = (y0.floor() as i32 - pad).max(-reach);
    let r = (x1.ceil() as i32 + pad).min(pm.width() as i32 + reach);
    let b = (y1.ceil() as i32 + pad).min(pm.height() as i32 + reach);
    if r <= l || b <= t {
        return;
    }
    let (w, h) = ((r - l) as usize, (b - t) as usize);
    let Some(mut src) = Pixmap::new(w as u32, h as u32) else { return };
    let one = Symbol {
        kind: sym.kind.clone(),
        opacity: sym.opacity,
        data_defined: false,
        layers: vec![crate::symbology::Layer { kind, dd: Default::default(), effects: vec![] }],
    };
    let (lf, tf) = (l as f32, t as f32);
    let shifted = |c: &Coord<f64>| {
        let (x, y) = to_px(c);
        (x - lf, y - tf)
    };
    draw_symbol(&mut src, &one, g, &shifted, u, ctx, origin.map(|(x, y)| (x - lf, y - tf)));
    let src_a: Vec<u8> = src.pixels().iter().map(|p| p.alpha()).collect();
    if src_a.iter().all(|&a| a == 0) {
        return;
    }
    // QgsImageOperation::multiplyOpacity below 1: DestinationIn with QColor(0, 0, 0, 255 × f).
    let opacity_u8 = |f: f64| if f >= 1.0 { 255 } else { (255.0 * f) as i32 };
    for e in effects {
        let out: Vec<[u8; 4]> = match e {
            Effect::DrawSource { opacity } => {
                let paint = PixmapPaint { opacity: *opacity as f32, ..Default::default() };
                pm.draw_pixmap(l, t, src.as_ref(), &paint, SkTransform::identity(), None);
                continue;
            }
            Effect::Shadow { inner, color, blur, offset, angle, opacity } => {
                // invertPixels(InvertRgba) for the inner shadow, then overlayColor (opaque).
                let mut im: Vec<[u8; 4]> = src_a
                    .iter()
                    .map(|&a| premul(*color, if *inner { 255 - a } else { a }))
                    .collect();
                stack_blur(&mut im, w, h, blur_px(blur));
                scale_alpha(&mut im, opacity_u8(*opacity));
                let d = offset.px(u);
                let a = (angle + 90.0).to_radians();
                let (dx, dy) = ((-d * a.cos()).round() as i32, (-d * a.sin()).round() as i32);
                let mut moved = vec![[0u8; 4]; w * h];
                for y in 0..h as i32 {
                    let sy = y - dy;
                    if sy < 0 || sy >= h as i32 {
                        continue;
                    }
                    for x in 0..w as i32 {
                        let sx = x - dx;
                        if sx >= 0 && sx < w as i32 {
                            moved[y as usize * w + x as usize] = im[sy as usize * w + sx as usize];
                        }
                    }
                }
                if *inner {
                    // Restricted to the source's coverage (DestinationIn). Its blend mode
                    // (Source in lag06) composites as source-over: measured against QGIS.
                    for (p, &a) in moved.iter_mut().zip(&src_a) {
                        *p = p.map(|v| mul255(v, a));
                    }
                }
                moved
            }
            Effect::OuterGlow { color, spread, blur, opacity } => {
                // QgsImageOperation::distanceTransform with shadeExterior: opaque pixels at
                // 1 − alpha, the rest at infinity; shaded colour → transparent over `spread`.
                let mut dt: Vec<f64> = src_a.iter().map(|&a| if a > 0 { 1.0 - a as f64 / 255.0 } else { 1e20 }).collect();
                distance_transform_2d(&mut dt, w, h);
                let s = spread.px(u);
                let mut im: Vec<[u8; 4]> = dt
                    .iter()
                    .map(|&sq| {
                        if s.abs() < 1e-8 {
                            return premul(*color, color.3);
                        }
                        if sq > s * s {
                            return [0; 4];
                        }
                        let dist = sq.sqrt();
                        // QgsGradientColorRamp(colour, transparent colour): alpha interpolated.
                        let mut a = lerp8(color.3, 0, dist / s) as f64;
                        if dist > s - 1.0 {
                            a = (a * (s - dist)).trunc(); // fade the last pixel (QColor::setAlpha)
                        }
                        premul(*color, a.clamp(0.0, 255.0) as u8)
                    })
                    .collect();
                stack_blur(&mut im, w, h, blur_px(blur));
                scale_alpha(&mut im, opacity_u8(*opacity));
                im
            }
        };
        let Some(mut layer) = Pixmap::new(w as u32, h as u32) else { return };
        for (p, v) in layer.pixels_mut().iter_mut().zip(out) {
            let a = v[3];
            *p = tiny_skia::PremultipliedColorU8::from_rgba(v[0].min(a), v[1].min(a), v[2].min(a), a)
                .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
        }
        pm.draw_pixmap(l, t, layer.as_ref(), &PixmapPaint::default(), SkTransform::identity(), None);
    }
}

/// x × a / 255, rounded (Qt's qt_div_255 of a premultiplying multiply).
fn mul255(x: u8, a: u8) -> u8 {
    let t = x as u32 * a as u32 + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// A colour's RGB at alpha `a`, premultiplied (RGBA).
fn premul(c: Rgba, a: u8) -> [u8; 4] {
    [mul255(c.0, a), mul255(c.1, a), mul255(c.2, a), a]
}

/// multiplyOpacity's DestinationIn fill: every premultiplied channel × a / 255.
fn scale_alpha(im: &mut [[u8; 4]], a: i32) {
    if a < 255 {
        let a = a.max(0) as u8;
        for p in im.iter_mut() {
            *p = p.map(|v| mul255(v, a));
        }
    }
}

/// QgsImageOperation::stackBlur (Qt's exponential blur from qpixmapfilter.cpp) on
/// premultiplied pixels: down, right, up, left.
fn stack_blur(im: &mut [[u8; 4]], w: usize, h: usize, radius: i32) {
    const TAB: [i32; 17] = [14, 10, 8, 6, 5, 5, 4, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2];
    let alpha = if radius < 1 { 16 } else if radius > 17 { 1 } else { TAB[radius as usize - 1] };
    let line = |im: &mut [[u8; 4]], idx: &mut dyn Iterator<Item = usize>| {
        let Some(first) = idx.next() else { return };
        let mut acc = im[first].map(|v| (v as i32) << 4);
        for i in idx {
            for c in 0..4 {
                acc[c] += (((im[i][c] as i32) << 4) - acc[c]) * alpha / 16;
                im[i][c] = (acc[c] >> 4) as u8;
            }
        }
    };
    for x in 0..w {
        line(im, &mut (0..h).map(|y| y * w + x));
    }
    for y in 0..h {
        line(im, &mut (0..w).map(|x| y * w + x));
    }
    for x in 0..w {
        line(im, &mut (0..h).rev().map(|y| y * w + x));
    }
    for y in 0..h {
        line(im, &mut (0..w).rev().map(|x| y * w + x));
    }
}

/// QgsShapeburstFillSymbolLayer::renderPolygon (no blur): a squared-distance transform
/// (Felzenszwalb & Huttenlocher, as QGIS) of the polygon drawn white with a black hairline
/// on black, shaded with the two-colour gradient and clipped by the polygon's coverage.
#[allow(clippy::too_many_arguments)]
fn draw_shapeburst(
    pm: &mut Pixmap,
    rings: &[Vec<(f32, f32)>],
    c1: Rgba,
    c2: Rgba,
    max_px: Option<i32>,
    ignore_rings: bool,
    offset: (f32, f32),
    opacity: f64,
) {
    const INF: f64 = 1e20;
    let Some(outer) = rings.first().filter(|r| r.len() >= 3) else { return };
    let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in outer {
        (l, t, r, b) = (l.min(x), t.min(y), r.max(x), b.max(y));
    }
    let side = 4 + 2 * 4; // 4 + (blur + 2) * 4 with no blur
    let (w, h) = ((r - l).round() as i32 + 2 * side, (b - t).round() as i32 + 2 * side);
    if w <= 0 || h <= 0 || (w as i64) * (h as i64) > 40_000_000 {
        return;
    }
    let shift = |x: f32, y: f32| (x - l + side as f32, y - t + side as f32);
    let path_of = |rs: &[Vec<(f32, f32)>]| {
        let mut pb = PathBuilder::new();
        for ring in rs {
            for (i, &(x, y)) in ring.iter().enumerate() {
                let (x, y) = shift(x, y);
                if i == 0 {
                    pb.move_to(x, y);
                } else {
                    pb.line_to(x, y);
                }
            }
            pb.close();
        }
        pb.finish()
    };
    let Some(path) = path_of(rings) else { return };
    let Some(mut alpha) = Pixmap::new(w as u32, h as u32) else { return };
    let white = paint(Rgba(255, 255, 255, 255), 1.0);
    let black = paint(Rgba(0, 0, 0, 255), 1.0);
    // _renderPolygon with a white brush and a cosmetic (1 px) black pen.
    let hairline = tiny_skia::Stroke { width: 1.0, ..Default::default() };
    alpha.fill_nonempty(&path, &white, FillRule::EvenOdd, SkTransform::identity(), None);
    alpha.stroke_path(&path, &black, &hairline, SkTransform::identity(), None);
    let mut fill = Pixmap::new(w as u32, h as u32).expect("same size");
    fill.fill(tiny_skia::Color::BLACK);
    if ignore_rings {
        if let Some(outer_path) = path_of(&rings[..1]) {
            fill.fill_nonempty(&outer_path, &white, FillRule::EvenOdd, SkTransform::identity(), None);
            fill.stroke_path(&outer_path, &black, &hairline, SkTransform::identity(), None);
        }
    } else {
        fill.draw_pixmap(0, 0, alpha.as_ref(), &PixmapPaint::default(), SkTransform::identity(), None);
    }
    // Black (red == 0) pixels are the boundary/outside: distance 0, the rest to be computed.
    let (wu, hu) = (w as usize, h as usize);
    let mut dt: Vec<f64> = fill.pixels().iter().map(|p| if p.red() == 0 { 0.0 } else { INF }).collect();
    distance_transform_2d(&mut dt, wu, hu);
    let max_dist = match max_px {
        Some(m) if m != 0 => m as f64,
        Some(_) => 0.0,
        None => dt.iter().cloned().fold(0.0, f64::max).sqrt(),
    };
    let alpha_px = alpha.pixels().to_vec();
    for (i, px) in fill.pixels_mut().iter_mut().enumerate() {
        let sq = dt[i];
        let v = if max_dist > 0.0 { if sq > 0.0 { (sq.sqrt() / max_dist).min(1.0) } else { 0.0 } } else { 1.0 };
        let c = tiny_skia::ColorU8::from_rgba(
            lerp8(c1.0, c2.0, v),
            lerp8(c1.1, c2.1, v),
            lerp8(c1.2, c2.2, v),
            lerp8(c1.3, c2.3, v),
        )
        .premultiply();
        // CompositionMode_DestinationIn with the polygon coverage (and the opacity).
        let a = alpha_px[i].alpha() as f64 / 255.0 * opacity;
        let m = |v: u8| (v as f64 * a).round() as u8;
        *px = tiny_skia::PremultipliedColorU8::from_rgba(m(c.red()), m(c.green()), m(c.blue()), m(c.alpha())).unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
    }
    pm.draw_pixmap(
        (l + offset.0 - side as f32).round() as i32,
        (t + offset.1 - side as f32).round() as i32,
        fill.as_ref(),
        &PixmapPaint::default(),
        SkTransform::identity(),
        None,
    );
}

/// QgsGradientColorRamp::color for two colours: a channel interpolated as floats, stored
/// in QColor's 16-bit channels (qRound), read back as 8 bits (qt_div_257).
fn lerp8(a: u8, b: u8, v: f64) -> u8 {
    let f = a as f64 / 255.0 + v * (b as f64 / 255.0 - a as f64 / 255.0);
    let x = (f * 65535.0).round() as u32 + 128;
    ((x - (x >> 8)) >> 8).min(255) as u8
}

/// QgsShapeburstFillSymbolLayer::distanceTransform1d (squared Euclidean distance).
fn distance_transform_1d(f: &[f64], v: &mut [usize], z: &mut [f64], d: &mut [f64]) {
    let n = f.len();
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    for q in 1..n {
        let qf = q as f64;
        let mut s;
        loop {
            let vk = v[k] as f64;
            s = ((f[q] + qf * qf) - (f[v[k]] + vk * vk)) / (2.0 * qf - 2.0 * vk);
            if s <= z[k] && k > 0 {
                k -= 1;
            } else {
                break;
            }
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f64::INFINITY;
    }
    k = 0;
    for (q, dq) in d.iter_mut().enumerate().take(n) {
        while z[k + 1] < q as f64 {
            k += 1;
        }
        let diff = q as f64 - v[k] as f64;
        *dq = diff * diff + f[v[k]];
    }
}

/// QgsShapeburstFillSymbolLayer::distanceTransform2d: columns, then rows.
fn distance_transform_2d(im: &mut [f64], width: usize, height: usize) {
    let m = width.max(height);
    let (mut f, mut v, mut z, mut d) = (vec![0.0; m], vec![0usize; m], vec![0.0; m + 1], vec![0.0; m]);
    for x in 0..width {
        for y in 0..height {
            f[y] = im[x + y * width];
        }
        distance_transform_1d(&f[..height], &mut v, &mut z, &mut d[..height]);
        for y in 0..height {
            im[x + y * width] = d[y];
        }
    }
    for y in 0..height {
        f[..width].copy_from_slice(&im[y * width..(y + 1) * width]);
        distance_transform_1d(&f[..width], &mut v, &mut z, &mut d[..width]);
        im[y * width..(y + 1) * width].copy_from_slice(&d[..width]);
    }
}

/// QGIS's private `Line` (line_p.h): direction, angle and step of one segment.
struct Seg {
    vertical: bool,
    increasing: bool,
    tangent: f64,
    length: f64,
}

impl Seg {
    fn new(p1: (f64, f64), p2: (f64, f64)) -> Seg {
        let mut s = Seg { vertical: false, increasing: false, tangent: 0.0, length: 0.0 };
        if p1 == p2 {
            return s;
        }
        // qgsDoubleNear: 4 * DBL_EPSILON
        if (p1.0 - p2.0).abs() <= 4.0 * f64::EPSILON {
            s.vertical = true;
            s.increasing = p2.1 > p1.1;
        } else {
            s.tangent = (p2.1 - p1.1) / (p2.0 - p1.0);
            s.increasing = p2.0 > p1.0;
        }
        s.length = ((p2.0 - p1.0).powi(2) + (p2.1 - p1.1).powi(2)).sqrt();
        s
    }

    /// Radians.
    fn angle(&self) -> f64 {
        let a = if self.vertical { std::f64::consts::FRAC_PI_2 } else { self.tangent.atan() };
        if self.increasing { a } else { a + std::f64::consts::PI }
    }

    fn diff_for_interval(&self, interval: f64) -> (f64, f64) {
        if self.vertical {
            return (0.0, if self.increasing { interval } else { -interval });
        }
        let alpha = self.tangent.atan();
        let (dx, dy) = (alpha.cos() * interval, alpha.sin() * interval);
        if self.increasing { (dx, dy) } else { (-dx, -dy) }
    }
}

/// QgsTemplatedLineSymbolLayerBase::markerAngle at the first / last vertex (not a ring):
/// the angle of the first / last non-degenerate segment, in degrees.
fn end_angle(pts: &[(f64, f64)], first: bool) -> f64 {
    let seg = if first {
        pts.iter().skip(1).find(|p| **p != pts[0]).map(|p| Seg::new(pts[0], *p))
    } else {
        let last = pts[pts.len() - 1];
        pts.iter().rev().skip(1).find(|p| **p != last).map(|p| Seg::new(*p, last))
    };
    seg.map_or(0.0, |s| s.angle().to_degrees())
}

/// QgsTemplatedLineSymbolLayerBase::collectOffsetPoints.
fn collect_offset_points(
    p: &[(f64, f64)],
    dest: &mut Vec<(f64, f64)>,
    interval: f64,
    initial_offset: f64,
    initial_lag: f64,
    required: usize,
) {
    if p.is_empty() {
        return;
    }
    let closed = p.first() == p.last();
    let mut points: Vec<(f64, f64)> = p.to_vec();
    let mut length_left = initial_offset;
    let mut lag_left = if initial_lag > 0.0 { -initial_lag } else { 1.0 };
    if lag_left < 0.0 && closed {
        // Track back around the ring: pseudo vertices before the first one.
        let mut last = *points.last().expect("non-empty");
        let mut pseudo = vec![];
        for i in (1..points.len().saturating_sub(1)).rev() {
            if lag_left >= 0.0 {
                break;
            }
            let pt = points[i];
            if last == pt {
                continue;
            }
            lag_left += Seg::new(last, pt).length;
            last = pt;
            pseudo.push(pt);
        }
        pseudo.reverse();
        pseudo.extend_from_slice(p);
        points = pseudo;
    } else {
        while lag_left < 0.0 {
            dest.push(points[0]);
            lag_left += interval;
        }
    }
    if initial_lag > 0.0 {
        length_left += interval - lag_left;
    }
    let mut last = points[0];
    let mut i = 1;
    while i < points.len() {
        let pt = points[i];
        if last == pt {
            if closed && i == points.len() - 1 && required > 0 && dest.len() < required {
                last = points[0];
                i = 1;
                continue;
            }
            i += 1;
            continue;
        }
        let l = Seg::new(last, pt);
        let diff = l.diff_for_interval(interval);
        let mut c = 1.0 - length_left / interval;
        length_left += l.length;
        while length_left > interval || (length_left - interval).abs() <= 0.000000001 {
            last = (last.0 + c * diff.0, last.1 + c * diff.1);
            length_left -= interval;
            dest.push(last);
            c = 1.0;
            if required > 0 && dest.len() >= required {
                break;
            }
        }
        last = pt;
        if required > 0 && dest.len() >= required {
            break;
        }
        if closed && i == points.len() - 1 && required > 0 && dest.len() < required {
            last = points[0];
            i = 1;
            continue;
        }
        i += 1;
    }
    if !closed && required > 0 {
        while dest.len() < required {
            dest.push(*points.last().expect("non-empty"));
        }
    }
}

/// QgsTemplatedLineSymbolLayerBase::renderPolylineInterval: symbol positions along a pixel
/// polyline with the line angle (degrees, clockwise) at each.
fn interval_points(points: &[(f64, f64)], interval: f64, offset_along: f64, average_over: f64) -> Vec<((f64, f64), f64)> {
    let mut out = vec![];
    if points.is_empty() || interval < 1e-5 {
        return out;
    }
    let mut along = offset_along;
    if along.abs() > 4.0 * f64::EPSILON && points.first() == points.last() {
        let total: f64 = points.windows(2).map(|w| Seg::new(w[0], w[1]).length).sum();
        along = if along > 0.0 { along % total } else { total - (-along % total) };
    }
    let mut length_left = interval - along;
    if average_over > 0.0 && average_over.abs() > 4.0 * f64::EPSILON {
        let mut sym = vec![];
        collect_offset_points(points, &mut sym, interval, length_left, 0.0, 0);
        if sym.is_empty() {
            return out;
        }
        if sym.len() > 1 && sym.first() == sym.last() {
            sym.pop();
        }
        let (mut start, mut end) = (vec![], vec![]);
        if average_over <= along {
            collect_offset_points(points, &mut start, interval, length_left + average_over, 0.0, sym.len());
        } else {
            collect_offset_points(points, &mut start, interval, 0.0, average_over - along, sym.len());
        }
        collect_offset_points(points, &mut end, interval, length_left - average_over, 0.0, sym.len());
        for (i, pt) in sym.iter().enumerate() {
            let (Some(a), Some(b)) = (start.get(i), end.get(i)) else { break };
            out.push((*pt, Seg::new(*a, *b).angle().to_degrees()));
        }
    } else {
        let mut last = points[0];
        for &pt in &points[1..] {
            if last == pt {
                continue;
            }
            let l = Seg::new(last, pt);
            let diff = l.diff_for_interval(interval);
            let mut c = 1.0 - length_left / interval;
            length_left += l.length;
            let angle = l.angle().to_degrees();
            while length_left > interval {
                last = (last.0 + c * diff.0, last.1 + c * diff.1);
                c = 1.0;
                out.push((last, angle));
                length_left -= interval;
            }
            last = pt;
        }
    }
    out
}

/// Half the widest stroke of a line symbol in pixels (QgsSymbolLayer::estimateMaxBleed).
fn line_bleed(line: &Symbol, u: &RenderUnits) -> f64 {
    line.layers
        .iter()
        .map(|l| match &l.kind {
            SymbolLayer::SimpleLine { stroke } => stroke.width.px(u) / 2.0,
            _ => 0.0,
        })
        .fold(0.0, f64::max)
}

fn draw_line_px(
    pm: &mut Pixmap,
    line: &Symbol,
    a: (f64, f64),
    b: (f64, f64),
    u: &RenderUnits,
    ctx: &Attrs,
) {
    let g: Geometry<f64> = LineString::from(vec![a, b]).into();
    draw_symbol(pm, line, &g, &|c| (c.x as f32, c.y as f32), u, ctx, None);
}

fn point_on_line(a: (f64, f64), b: (f64, f64), d: f64) -> (f64, f64) {
    let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    if len == 0.0 {
        return a;
    }
    (a.0 + (b.0 - a.0) * d / len, a.1 + (b.1 - a.1) * d / len)
}

/// The repeating tile of QgsLinePatternFillSymbolLayer::applyPattern, step by step:
/// integer tile sizes (which slightly change the effective angle), a buffer of whole tiles
/// to cover the line bleed, two or three lines drawn anti-aliased, then the buffer cut off.
/// Returns None where QGIS falls back to drawing individual lines.
fn line_pattern_tile(
    line_angle: f64,
    dist_px: f64,
    offset_px: f64,
    line: &Symbol,
    u: &RenderUnits,
    ctx: &Attrs,
) -> Option<Pixmap> {
    let near = |a: f64, b: f64| (a - b).abs() < 1e-8;
    let mut dist = dist_px;
    let mut offset = offset_px % dist;
    if offset > dist / 2.0 {
        offset -= dist;
    }
    let bleed = line_bleed(line, u);
    let mut angle = line_angle % 360.0;
    if angle < 0.0 {
        angle += 360.0;
    }
    let (mut width, mut height): (i64, i64);
    if near(angle, 0.0) || near(angle, 360.0) || near(angle, 180.0) {
        height = dist as i64;
        width = height;
    } else if near(angle, 90.0) || near(angle, 270.0) {
        width = dist as i64;
        height = width;
    } else {
        height = (dist / angle.to_radians().cos()) as i64;
        width = (dist / angle.to_radians().sin()) as i64;
        angle = (height as f64).atan2(width as f64).to_degrees();
        if angle < 0.0 {
            angle += 360.0;
        }
        height = height.abs();
        width = width.abs();
        dist = (height as f64 * angle.to_radians().cos()).abs();
        let offset_height = (offset / angle.to_radians().cos()).round() as i64;
        offset = offset_height as f64 * angle.to_radians().cos();
    }
    if width == 0 || height == 0 {
        return None;
    }
    // QGIS computes the buffer from the width twice (sic).
    let buffer_multi = ((bleed / width as f64).ceil() as i64).max(1);
    let (xbuf, ybuf) = (width * buffer_multi, height * buffer_multi);
    let (inner_w, inner_h) = (width, height);
    let (w, h) = (width + 2 * xbuf, height + 2 * ybuf);
    if w > 2000 || h > 2000 {
        return None;
    }
    let (wf, hf) = (w as f64, h as f64);
    let o = (0., 0.);
    let r = |d: f64| d.to_radians();
    let diagonal = !(near(angle, 0.0)
        || near(angle, 360.0)
        || near(angle, 180.0)
        || near(angle, 90.0)
        || near(angle, 270.0));
    let (mut p1, mut p2, mut p3, mut p4, mut p5, mut p6) =
        if near(angle, 0.0) || near(angle, 360.0) || near(angle, 180.0) {
            let (yb, yi) = (ybuf as f64, (ybuf + inner_h) as f64);
            ((0., yb), (wf, yb), (0., yi), (wf, yi), o, o)
        } else if near(angle, 90.0) || near(angle, 270.0) {
            let (xb, xi) = (xbuf as f64, (xbuf + inner_w) as f64);
            ((xb, hf), (xb, 0.), (xi, hf), (xi, 0.), o, o)
        } else if angle < 90.0 || (angle > 180.0 && angle < 270.0) {
            let (dx, dy) = (dist * r(90.0 - angle).cos(), dist * r(90.0 - angle).sin());
            if angle < 90.0 {
                (
                    (0., hf),
                    (wf, 0.),
                    (-dx, hf - dy),
                    (wf - dx, -dy),
                    (dx, hf + dy),
                    (wf + dx, dy),
                )
            } else {
                (
                    (wf, 0.),
                    (0., hf),
                    (wf - dx, -dy),
                    (-dx, hf - dy),
                    (wf + dx, dy),
                    (dx, hf + dy),
                )
            }
        } else {
            let (dy, dx) = (dist * r(180.0 - angle).cos(), dist * r(180.0 - angle).sin());
            if angle < 180.0 {
                (
                    (0., 0.),
                    (wf, hf),
                    (-dx, dy),
                    (wf - dx, hf + dy),
                    (dx, -dy),
                    (wf + dx, hf - dy),
                )
            } else {
                (
                    (wf, hf),
                    (0., 0.),
                    (wf - dx, hf + dy),
                    (-dx, dy),
                    (wf + dx, hf - dy),
                    (dx, -dy),
                )
            }
        };
    if offset_px != 0.0 {
        p3 = point_on_line(p1, p3, dist + offset);
        p4 = point_on_line(p2, p4, dist + offset);
        p5 = point_on_line(p1, p5, dist - offset);
        p6 = point_on_line(p2, p6, dist - offset);
        p1 = point_on_line(p1, p3, offset);
        p2 = point_on_line(p2, p4, offset);
    }
    let mut img = Pixmap::new(w as u32, h as u32)?;
    draw_line_px(&mut img, line, p1, p2, u, ctx);
    draw_line_px(&mut img, line, p3, p4, u, ctx);
    if diagonal {
        draw_line_px(&mut img, line, p5, p6, u, ctx);
    }
    // Cut off the buffer.
    let mut tile = Pixmap::new(inner_w as u32, inner_h as u32)?;
    tile.draw_pixmap(
        -(xbuf as i32),
        -(ybuf as i32),
        img.as_ref(),
        &PixmapPaint::default(),
        SkTransform::identity(),
        None,
    );
    Some(tile)
}

/// Vector line-pattern rendering (QgsLinePatternFillSymbolLayer::renderPolygon with
/// lines): parallel lines across the feature, clipped to it with a coverage mask.
#[allow(clippy::too_many_arguments)]
fn draw_pattern_lines(
    pm: &mut Pixmap,
    path: &tiny_skia::Path,
    angle: f64,
    dist_px: f64,
    offset_px: f64,
    line: &Symbol,
    u: &RenderUnits,
    ctx: &Attrs,
) {
    let dist = dist_px.max(0.025);
    let mut offset = offset_px % dist;
    if offset > dist / 2.0 {
        offset -= dist;
    }
    let Some(mut mask) = tiny_skia::Mask::new(pm.width(), pm.height()) else {
        return;
    };
    let b = path.bounds();
    if b.width() <= 0.0 || b.height() <= 0.0 {
        return;
    }
    mask.fill_path(path, FillRule::EvenOdd, true, SkTransform::identity());
    let mut layer = Pixmap::new(pm.width(), pm.height()).expect("same size");
    let (cx, cy) = (
        ((b.left() + b.right()) / 2.0) as f64,
        ((b.top() + b.bottom()) / 2.0) as f64,
    );
    // Rotate the feature's corners into line space around its centre.
    let (s, c) = angle.to_radians().sin_cos();
    let rot = |x: f64, y: f64| {
        (
            c * (x - cx) - s * (y - cy) + cx,
            s * (x - cx) + c * (y - cy) + cy,
        )
    };
    let unrot = |x: f64, y: f64| {
        (
            c * (x - cx) + s * (y - cy) + cx,
            -s * (x - cx) + c * (y - cy) + cy,
        )
    };
    let corners = [
        (b.left() as f64, b.top() as f64),
        (b.right() as f64, b.top() as f64),
        (b.right() as f64, b.bottom() as f64),
        (b.left() as f64, b.bottom() as f64),
    ];
    let t: Vec<(f64, f64)> = corners.iter().map(|(x, y)| rot(*x, *y)).collect();
    let buffer = line_bleed(line, u) * 2.0;
    let left = t.iter().map(|p| p.0).fold(f64::INFINITY, f64::min) - buffer;
    let right = t.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max) + buffer;
    let top = t.iter().map(|p| p.1).fold(f64::INFINITY, f64::min) - buffer;
    let bottom = t.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max) + buffer;
    let mut y = top;
    while y <= bottom {
        draw_line_px(
            &mut layer,
            line,
            unrot(left, y - offset),
            unrot(right, y - offset),
            u,
            ctx,
        );
        y += dist;
    }
    pm.draw_pixmap(
        0,
        0,
        layer.as_ref(),
        &PixmapPaint::default(),
        SkTransform::identity(),
        Some(&mask),
    );
}

/// Marker outlines in QGIS's unit coordinates (y down), from
/// QgsSimpleMarkerSymbolLayerBase::shapeToPolygon. `None` = circle; the bool marks
/// open shapes that are only stroked.
/// Marker outline parts (each a list of unit-space points) and whether they are open.
type MarkerShape = (Vec<Vec<(f32, f32)>>, bool);

fn marker_shape(shape: &str) -> Option<MarkerShape> {
    const S: f32 = 0.866_025_4; // sin(60°)
    let closed = |p: &[(f32, f32)]| Some((vec![p.to_vec()], false));
    match shape {
        "square" | "rectangle" => closed(&[(-1., -1.), (1., -1.), (1., 1.), (-1., 1.)]),
        "diamond" => closed(&[(-1., 0.), (0., 1.), (1., 0.), (0., -1.)]),
        "triangle" => closed(&[(-1., 1.), (1., 1.), (0., -1.)]),
        "equilateral_triangle" => closed(&[(-S, 0.5), (S, 0.5), (0., -1.)]),
        "pentagon" => closed(&[
            (-0.9511, -0.3090),
            (-0.5878, 0.8090),
            (0.5878, 0.8090),
            (0.9511, -0.3090),
            (0., -1.),
        ]),
        "hexagon" => closed(&[
            (-S, -0.5),
            (-S, 0.5),
            (0., 1.),
            (S, 0.5),
            (S, -0.5),
            (0., -1.),
        ]),
        "octagon" => {
            const O: f32 = 0.414_213_6;
            closed(&[
                (-O, -1.),
                (O, -1.),
                (1., -O),
                (1., O),
                (O, 1.),
                (-O, 1.),
                (-1., O),
                (-1., -O),
            ])
        }
        "star" => {
            let inner = 72f32.to_radians().cos() / 36f32.to_radians().cos();
            let pts: Vec<(f32, f32)> = (0..10)
                .map(|i| {
                    let a = (i as f32 * 36.0).to_radians();
                    let r = if i % 2 == 0 { 1.0 } else { inner };
                    (r * a.sin(), -r * a.cos())
                })
                .collect();
            closed(&pts)
        }
        "cross" | "cross_fill" => Some((
            vec![vec![(-1., 0.), (1., 0.)], vec![(0., -1.), (0., 1.)]],
            true,
        )),
        "cross2" | "x" => Some((
            vec![vec![(-1., -1.), (1., 1.)], vec![(-1., 1.), (1., -1.)]],
            true,
        )),
        "line" => Some((vec![vec![(0., -1.), (0., 1.)]], true)),
        _ => None,
    }
}

fn draw_marker(
    pm: &mut Pixmap,
    shape: &str,
    place: SkTransform,
    color: Rgba,
    stroke: &Stroke,
    opacity: f64,
    u: &RenderUnits,
) {
    let mut pb = PathBuilder::new();
    let mut stroke_only = false;
    match marker_shape(shape) {
        Some((parts, open)) => {
            stroke_only = open;
            for part in parts {
                for (i, (x, y)) in part.iter().enumerate() {
                    if i == 0 {
                        pb.move_to(*x, *y)
                    } else {
                        pb.line_to(*x, *y)
                    }
                }
                if !open {
                    pb.close();
                }
            }
        }
        None => pb.push_circle(0.0, 0.0, 1.0),
    }
    // Transform the outline (not the pen), so stroke widths stay in pixels.
    let Some(path) = pb.finish().and_then(|p| p.transform(place)) else {
        return;
    };
    if stroke_only {
        let mut st = sk_stroke(stroke, u);
        st.width = st.width.max(1.0);
        pm.stroke_path(
            &path,
            &paint(stroke.color, opacity),
            &st,
            SkTransform::identity(),
            None,
        );
        return;
    }
    pm.fill_nonempty(
        &path,
        &paint(color, opacity),
        FillRule::Winding,
        SkTransform::identity(),
        None,
    );
    if stroke.visible {
        pm.stroke_path(
            &path,
            &paint(stroke.color, opacity),
            &sk_stroke(stroke, u),
            SkTransform::identity(),
            None,
        );
    }
}

/// A legend patch for a symbol (QgsSymbolLegendNode icon), at `dpi`.
pub fn symbol_patch(sym: &Symbol, w: u32, h: u32, dpi: f64) -> Pixmap {
    let mut pm = Pixmap::new(w, h).expect("non-zero patch");
    let u = RenderUnits {
        dpi,
        mupp: 1.0,
        meters_per_unit: 1.0,
    };
    let (wf, hf) = (w as f64, h as f64);
    let g: Geometry<f64> = match sym.kind.as_str() {
        "line" => LineString::from(vec![(1.0, hf / 2.0), (wf - 1.0, hf / 2.0)]).into(),
        "marker" => geo_types::Point::new(wf / 2.0, hf / 2.0).into(),
        _ => geo_types::Polygon::new(
            LineString::from(vec![
                (1.0, 1.0),
                (wf - 1.0, 1.0),
                (wf - 1.0, hf - 1.0),
                (1.0, hf - 1.0),
                (1.0, 1.0),
            ]),
            vec![],
        )
        .into(),
    };
    draw_symbol(
        &mut pm,
        sym,
        &g,
        &|c| (c.x as f32, c.y as f32),
        &u,
        &Attrs::default(),
        Some((1.0, h as f32 - 1.0)),
    );
    pm
}

/// QgsWmsRenderContext::mapSize: when the BBOX aspect ratio differs from WIDTH/HEIGHT,
/// render with square pixels at the averaged cell size (as MapServer does)…
pub fn square_pixel_size(extent: &Rect, width: u32, height: u32) -> (u32, u32) {
    let map_ratio = extent.width() / extent.height();
    let image_ratio = width as f64 / height as f64;
    if (map_ratio - image_ratio).abs() <= 0.0001 * map_ratio.abs().max(image_ratio.abs()) {
        return (width, height);
    }
    let cell = (extent.width() / width as f64) * 0.5 + (extent.height() / height as f64) * 0.5;
    (
        ((extent.width() / cell) as u32).max(1),
        ((extent.height() / cell) as u32).max(1),
    )
}

/// …then stretch the image to the requested size (QgsRenderer::scaleImage, bilinear).
pub fn scale(pm: &Pixmap, width: u32, height: u32) -> Pixmap {
    if (pm.width(), pm.height()) == (width, height) {
        return pm.clone();
    }
    let mut out = Pixmap::new(width, height).expect("requested size was validated");
    let paint = PixmapPaint {
        quality: tiny_skia::FilterQuality::Bilinear,
        ..Default::default()
    };
    let t = SkTransform::from_scale(
        width as f32 / pm.width() as f32,
        height as f32 / pm.height() as f32,
    );
    out.draw_pixmap(0, 0, pm.as_ref(), &paint, t, None);
    out
}

/// RGBA PNG of a premultiplied pixmap, un-premultiplied like Qt's ARGB32_Premultiplied ->
/// ARGB32 conversion: round to nearest, but exact halves round down (Qt's SSE4 path
/// multiplies by an approximate 255/alpha that lands just below).
pub fn encode_png(pm: &Pixmap) -> Result<Vec<u8>> {
    let mut rgba = Vec::with_capacity(pm.pixels().len() * 4);
    for p in pm.pixels() {
        let a = p.alpha() as u32;
        match a {
            0 => rgba.extend([0; 4]),
            255 => rgba.extend([p.red(), p.green(), p.blue(), 255]),
            _ => {
                for c in [p.red(), p.green(), p.blue()] {
                    let twice = c as u32 * 510; // 2 * c * 255
                    let v = if twice.is_multiple_of(a) && (twice / a) % 2 == 1 {
                        twice / a / 2
                    } else {
                        (twice + a) / (2 * a)
                    };
                    rgba.push(v.min(255) as u8);
                }
                rgba.push(a as u8);
            }
        }
    }
    let mut out = vec![];
    let fail = |e: png::EncodingError| {
        warn!("png encoding failed: {e}");
        anyhow::anyhow!("png encoding failed: {e}")
    };
    let mut enc = png::Encoder::new(&mut out, pm.width(), pm.height());
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    // fdeflate: ~zlib level 1 speed (the QGIS fork serves PNG level 1); Balanced spent ~14%
    // of a dense lag21 render compressing.
    enc.set_compression(png::Compression::Fast);
    let mut w = enc.write_header().map_err(fail)?;
    w.write_image_data(&rgba).map_err(fail)?;
    w.finish().map_err(fail)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn interval_placement_follows_qgis() {
        // renderPolylineInterval without averaging: first marker after the offset, then
        // every interval; nothing in the last partial interval.
        let line = [(0.0, 0.0), (100.0, 0.0)];
        let p = interval_points(&line, 20.0, 10.0, 0.0);
        let xs: Vec<f64> = p.iter().map(|((x, _), _)| *x).collect();
        assert_eq!(xs.len(), 5);
        for (x, want) in xs.iter().zip([10.0, 30.0, 50.0, 70.0, 90.0]) {
            assert!(near(*x, want), "{xs:?}");
        }
        assert!(p.iter().all(|(_, a)| near(*a, 0.0)));
        // Averaged angles on a straight line are the line's angle (pixel y down: 90° = down).
        let down = [(0.0, 0.0), (0.0, 100.0)];
        let p = interval_points(&down, 20.0, 10.0, 7.5);
        assert_eq!(p.len(), 5);
        assert!(p.iter().all(|(_, a)| near(*a, 90.0)), "{p:?}");
        // The interval carries over a vertex: 15 px left on the first segment.
        let bent = [(0.0, 0.0), (25.0, 0.0), (25.0, 40.0)];
        let p = interval_points(&bent, 20.0, 10.0, 0.0);
        let pts: Vec<(f64, f64)> = p.iter().map(|(pt, _)| *pt).collect();
        assert_eq!(pts.len(), 3, "{pts:?}");
        assert!(near(pts[0].0, 10.0) && near(pts[1].1, 5.0) && near(pts[2].1, 25.0), "{pts:?}");
    }

    #[test]
    fn end_angles_and_anchor_shift() {
        let left = [(10.0, 0.0), (10.0, 0.0), (0.0, 0.0)];
        assert!(near(end_angle(&left, true), 180.0));
        assert!(near(end_angle(&left, false), 180.0));
        // Top anchor: centre moves half the height down, then turns with the marker
        // (QgsMarkerSymbolLayer::markerOffset + _rotatedOffset).
        let (x, y) = marker_shift((0.0, 0.0), (1, 0), 10.0, 20.0, 0.0);
        assert!(near(x as f64, 0.0) && near(y as f64, 10.0));
        let (x, y) = marker_shift((0.0, 0.0), (1, 0), 10.0, 20.0, 90.0);
        assert!((x + 10.0).abs() < 1e-5 && y.abs() < 1e-5, "{x} {y}");
        let (x, y) = marker_shift((2.0, 0.0), (0, 2), 10.0, 20.0, 0.0);
        assert!(near(x as f64, 7.0) && near(y as f64, -10.0));
    }
}
