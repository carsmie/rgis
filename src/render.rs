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
use crate::expression::Expr;
use crate::geom::{self, Rect};
use crate::project::MapLayer;
use crate::provider::Feature;
use crate::symbology::{RenderUnits, Rgba, Stroke, Symbol, SymbolLayer};

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
fn features_for_drawing(
    layer: &MapLayer,
    ms: &MapSettings,
    filter: Option<&Expr>,
) -> Result<Vec<Feature>> {
    let provider = layer.provider()?;
    let layer_crs = layer.crs();
    let to_layer = Transform::new(&ms.crs, &layer_crs);
    let to_map = Transform::new(&layer_crs, &ms.crs);
    let mut out = provider.features(to_layer.rect(&ms.extent).ok())?;
    out.retain(|f| filter.is_none_or(|e| e.matches(&f.attrs)));

    // The pixel tolerance expressed in layer units, measured at the centre of the view.
    let e = &ms.extent;
    let (cx, cy) = ((e.xmin + e.xmax) / 2.0, (e.ymin + e.ymax) / 2.0);
    let tol = layer.simplify_tol.and_then(|px| {
        let d = px * ms.mupp();
        let a = to_layer.point(cx, cy).ok()?;
        let b = to_layer.point(cx + d, cy).ok()?;
        let c = to_layer.point(cx, cy + d).ok()?;
        let dist =
            |p: (f64, f64), q: (f64, f64)| ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt();
        Some(dist(a, b).min(dist(a, c)))
    });
    // Clip with a margin so the artificial edges along the clip boundary stay invisible.
    let m = 64.0 * ms.mupp();
    let clip = Rect::new(e.xmin - m, e.ymin - m, e.xmax + m, e.ymax + m);
    for f in &mut out {
        f.geometry = f.geometry.take().and_then(|g| {
            let g = match tol {
                Some(t) => geom::simplify(&g, t),
                None => g,
            };
            let g = if to_map.is_identity() {
                g
            } else {
                geom::transform(&g, &to_map)
            };
            geom::clip_to_rect(&g, &clip)
        });
    }
    out.retain(|f| f.geometry.is_some());
    Ok(out)
}

fn ignore_rendering_errors() -> bool {
    std::env::var("QGIS_SERVER_IGNORE_RENDERING_ERRORS")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true"))
}

/// Render layers bottom-to-top into a new pixmap.
pub fn render(jobs: &[LayerJob], ms: &MapSettings) -> Result<Pixmap> {
    let mut pm =
        Pixmap::new(ms.width, ms.height).ok_or_else(|| anyhow::anyhow!("invalid image size"))?;
    if let Some(bg) = ms.background {
        pm.fill(tiny_skia::Color::from_rgba8(bg.0, bg.1, bg.2, bg.3));
    }
    let scale = ms.scale();
    for job in jobs {
        let l = &job.layer;
        if !l.visible_at(scale) {
            debug!("layer {} not visible at 1:{scale:.0}", l.name);
            continue;
        }
        if l.kind != "vector" {
            warn!(
                "layer {} is a {} layer; only vector layers are rendered",
                l.name, l.kind
            );
            continue;
        }
        let started = std::time::Instant::now();
        let features = match features_for_drawing(l, ms, job.filter.as_ref()) {
            Ok(f) => f,
            // QgsServerSettings::ignoreRenderingErrors
            Err(e) if ignore_rendering_errors() => {
                warn!("Rendering error: {e:#} (ignored)");
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
            draw_features(&mut pm, l, &features, ms, scale);
        }
        debug!(
            "rendered {} ({} features) in {:?}",
            l.name,
            features.len(),
            started.elapsed()
        );
    }
    Ok(pm)
}

fn draw_features(
    pm: &mut Pixmap,
    l: &MapLayer,
    features: &[Feature],
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
    for f in features {
        let Some(g) = &f.geometry else { continue };
        for sym in l.renderer.symbols_for(&f.attrs, scale) {
            draw_symbol(pm, sym, g, &to_px, &units);
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
) {
    for layer in &sym.layers {
        match layer {
            SymbolLayer::SimpleFill {
                color,
                filled,
                hatched,
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
                    // Hatch patterns are approximated by a translucent solid fill.
                    let op = sym.opacity * if *hatched { 0.35 } else { 1.0 };
                    pm.fill_path(
                        &path,
                        &paint(*color, op),
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
            SymbolLayer::SimpleMarker {
                shape,
                size,
                color,
                stroke,
            } => {
                let r = (size.px(u) / 2.0) as f32;
                // Zero-size markers are a common way to make label-only layers.
                if r <= 0.0 {
                    continue;
                }
                for c in points(g) {
                    let (x, y) = to_px(&c);
                    draw_marker(pm, shape, x, y, r, *color, stroke, sym.opacity, u);
                }
            }
        }
    }
}

fn regular(pb: &mut PathBuilder, x: f32, y: f32, r: f32, n: usize, rot: f32, inner: Option<f32>) {
    let steps = if inner.is_some() { n * 2 } else { n };
    for i in 0..steps {
        let a = rot + i as f32 * std::f32::consts::TAU / steps as f32;
        let rr = if i % 2 == 1 { inner.unwrap_or(r) } else { r };
        let (px, py) = (x + rr * a.sin(), y - rr * a.cos());
        if i == 0 {
            pb.move_to(px, py)
        } else {
            pb.line_to(px, py)
        }
    }
    pb.close();
}

#[allow(clippy::too_many_arguments)]
fn draw_marker(
    pm: &mut Pixmap,
    shape: &str,
    x: f32,
    y: f32,
    r: f32,
    color: Rgba,
    stroke: &Stroke,
    opacity: f64,
    u: &RenderUnits,
) {
    let mut pb = PathBuilder::new();
    let mut stroke_only = false;
    match shape {
        "square" | "rectangle" => regular(
            &mut pb,
            x,
            y,
            r * std::f32::consts::SQRT_2,
            4,
            std::f32::consts::FRAC_PI_4,
            None,
        ),
        "diamond" => regular(&mut pb, x, y, r, 4, 0.0, None),
        "triangle" | "equilateral_triangle" => regular(&mut pb, x, y, r, 3, 0.0, None),
        "pentagon" => regular(&mut pb, x, y, r, 5, 0.0, None),
        "hexagon" => regular(&mut pb, x, y, r, 6, 0.0, None),
        "octagon" => regular(&mut pb, x, y, r, 8, std::f32::consts::PI / 8.0, None),
        "star" => regular(&mut pb, x, y, r, 5, 0.0, Some(r * 0.4)),
        "cross" | "cross_fill" => {
            stroke_only = true;
            pb.move_to(x - r, y);
            pb.line_to(x + r, y);
            pb.move_to(x, y - r);
            pb.line_to(x, y + r);
        }
        "cross2" | "x" => {
            stroke_only = true;
            let d = r * std::f32::consts::FRAC_1_SQRT_2;
            pb.move_to(x - d, y - d);
            pb.line_to(x + d, y + d);
            pb.move_to(x - d, y + d);
            pb.line_to(x + d, y - d);
        }
        "line" => {
            stroke_only = true;
            pb.move_to(x, y - r);
            pb.line_to(x, y + r);
        }
        _ => pb.push_circle(x, y, r),
    }
    let Some(path) = pb.finish() else { return };
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
    pm.fill_path(
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
    draw_symbol(&mut pm, sym, &g, &|c| (c.x as f32, c.y as f32), &u);
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

pub fn encode_png(pm: &Pixmap) -> Result<Vec<u8>> {
    pm.encode_png().map_err(|e| {
        warn!("png encoding failed: {e}");
        anyhow::anyhow!("png encoding failed: {e}")
    })
}
