//! Labeling, ported from QGIS: QgsPalLayerSettings registration (labeling/settings.rs, here),
//! the PAL placement engine (labeling/pal.rs) and QgsTextRenderer drawing (text.rs).
//!
//! Flow as in QgsMapRendererJob: while each vector layer is drawn its features are registered
//! with the engine; after all layers, the engine solves the placement problem once for the whole
//! map and draws the labels on top.

pub mod settings;

mod index;
mod pal;
mod rtree;
mod shape;

use std::f64::consts::PI;
use geo_types::{Coord, Geometry};
use serde_json::{json, Value};
use tiny_skia::Pixmap;

use crate::expression::{num, to_str, truthy, Attrs};
use crate::geom::{self, Rect};
use crate::project::MapLayer;
use crate::render::MapSettings;
use crate::symbology::{RenderUnits, Size, Symbol, SymbolLayer, Unit};
use crate::text::{self, HAlign, LabelDocument};

use index::BBox;
use pal::{LabelFeature, Pal, PalLayer, Quadrant};
use settings::{EngineSettings, LabelingRule, MultiLineAlign, ObstacleType, PalSettings, Placement};
use shape::Shape;

/// What is needed to draw a placed label.
struct Payload {
    doc: LabelDocument,
    color: crate::symbology::Rgba,
    opacity: f64,
    buffer: Option<text::Buffer>,
    align: MultiLineAlign,
    z_index: f64,
    render_order: usize,
    area: f64,
}

/// QgsLabelingEngine for one map render.
pub struct Engine {
    pal: Pal,
    payloads: Vec<Payload>,
    /// Visible extent grown by the extent buffer.
    extent: Rect,
    xmin: f64,
    ymax: f64,
    mupp: f64,
    units: RenderUnits,
    scale: f64,
}

/// A feature as the layer renderer passes it to labeling: attributes plus its geometry in map
/// units (simplified like the drawn geometry, not clipped).
pub struct LabelSource<'a> {
    pub fid: i64,
    pub attrs: &'a Attrs,
    pub geometry: &'a Geometry<f64>,
    pub symbols: Vec<&'a Symbol>,
}

impl Engine {
    pub fn new(ms: &MapSettings, settings: EngineSettings) -> Engine {
        let units = ms.units();
        let b = ms.extent_buffer;
        let e = &ms.extent;
        let extent = Rect::new(e.xmin - b, e.ymin - b, e.xmax + b, e.ymax + b);
        Engine {
            pal: Pal::new(settings, 10.0 * ms.dpi / 25.4 * ms.mupp()),
            payloads: Vec::new(),
            extent,
            xmin: e.xmin,
            ymax: e.ymax,
            mupp: ms.mupp(),
            units,
            scale: ms.scale(),
        }
    }

    pub fn has_layers(&self) -> bool {
        !self.pal.layers.is_empty()
    }

    fn to_px(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.xmin) / self.mupp, (self.ymax - y) / self.mupp)
    }

    fn px(&self, s: &Size) -> f64 {
        s.px(&self.units)
    }

    /// QgsRenderContext::convertToMapUnits.
    fn map_units(&self, v: f64, unit: Unit) -> f64 {
        let s = Size::new(None, None, v);
        let s = Size { unit, ..s };
        s.px(&self.units) * self.mupp
    }

    /// Register a layer's labels (QgsVectorLayerLabelProvider per labeling rule).
    pub fn register_layer(&mut self, layer: &MapLayer, features: &[LabelSource], vars: &Attrs, render_order: usize) {
        for rule in &layer.labeling {
            if let Some((mindenom, maxdenom)) = rule.scale_range {
                if (mindenom > 0.0 && self.scale < mindenom) || (maxdenom > 0.0 && self.scale > maxdenom) {
                    continue;
                }
            }
            self.register_rule(layer, rule, features, vars, render_order);
        }
    }

    fn register_rule(
        &mut self,
        layer: &MapLayer,
        rule: &LabelingRule,
        features: &[LabelSource],
        vars: &Attrs,
        render_order: usize,
    ) {
        let s = &rule.settings;
        let is_point_layer = layer.geometry == "Point";
        let has_renderer = !matches!(layer.renderer, crate::symbology::Renderer::Null);
        // QgsVectorLayerLabelProvider::init
        let obstacle_type = if is_point_layer && has_renderer { ObstacleType::PolygonWhole } else { s.obstacle_type };
        let mut pl = PalLayer::new(
            layer.id.clone(),
            s.placement,
            1.0 - s.priority as f64 / 10.0,
            obstacle_type,
            s.centroid_inside,
            s.upside_down,
            s.draw_labels,
            render_order,
        );
        let li = self.pal.layers.len();
        // QgsPalLayerSettings::isLabelVisible (scale part).
        let visible = !s.scale_visibility
            || !((s.minimum_scale > 0.0 && self.scale >= s.minimum_scale)
                || (s.maximum_scale > 0.0 && self.scale < s.maximum_scale));
        if !visible && !s.obstacle {
            return;
        }
        for f in features {
            if rule.filter.as_ref().is_some_and(|e| !e.matches(f.attrs)) {
                continue;
            }
            let mut ctx = f.attrs.clone();
            ctx.extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
            ctx.insert("$id".into(), json!(f.fid));
            // $x / $y: the point (first point of a multipoint), else the centroid.
            let xy = match f.geometry {
                Geometry::Point(p) => Some((p.x(), p.y())),
                Geometry::MultiPoint(mp) => mp.0.first().map(|p| (p.x(), p.y())),
                g => shapes(g).first().map(shape::centroid),
            };
            if let Some((x, y)) = xy {
                ctx.insert("$x".into(), json!(x));
                ctx.insert("$y".into(), json!(y));
            }
            if let Some(lf) = self.label_feature(s, layer, f, &ctx, visible, render_order) {
                pl.register(li, lf);
            }
        }
        self.pal.layers.push(pl);
    }

    fn dd(&self, s: &PalSettings, key: &str, ctx: &Attrs) -> Option<Value> {
        s.dd.get(key).map(|e| e.eval(ctx)).filter(|v| !v.is_null())
    }

    /// QgsPalLayerSettings::registerFeatureWithDetails + generateLabelFeature.
    fn label_feature(
        &mut self,
        s: &PalSettings,
        layer: &MapLayer,
        f: &LabelSource,
        ctx: &Attrs,
        visible: bool,
        render_order: usize,
    ) -> Option<LabelFeature> {
        let obstacle = self.dd(s, "IsObstacle", ctx).map(|v| truthy(&v)).unwrap_or(s.obstacle);
        if !s.draw_labels && !obstacle {
            return None;
        }
        let draw = s.draw_labels && visible && self.dd(s, "Show", ctx).is_none_or(|v| truthy(&v));
        if !draw && !obstacle {
            return None;
        }
        // evaluateTextFormat: font size in whole pixels.
        let mut size = s.format.size;
        if let Some(v) = self.dd(s, "Size", ctx).and_then(|v| num(&v)) {
            size.value = v;
        }
        if size.value <= 0.0 {
            return None;
        }
        let px = (self.px(&size) + 0.5) as i32;
        if px < 1 {
            return None;
        }
        if size.unit == Unit::MapUnit && s.font_limit_pixel_size && (s.font_min_pixel_size > px as i64 || px as i64 > s.font_max_pixel_size) {
            return None;
        }
        let font = text::font(&s.format.font, px)?;

        // evaluateLabelContent
        let mut label = s.expression.as_ref().map(|e| e.eval(ctx)).map(|v| to_str(&v)).unwrap_or_default();
        match s.format.capitalization {
            1 => label = label.to_uppercase(),
            2 => label = label.to_lowercase(),
            _ => {}
        }
        let wrap = if s.wrap_char.is_empty() { "\n" } else { s.wrap_char.as_str() };
        let lines: Vec<&str> = label.split(wrap).flat_map(|l| l.split('\n')).collect();
        let doc = LabelDocument::new(font.clone(), &lines, s.format.line_height);

        // Geometry: clipped to the (buffered) extent unless a whole-polygon centroid is wanted.
        let g = f.geometry;
        let centroid_poly = matches!(s.placement, Placement::AroundPoint | Placement::OverPoint)
            && matches!(g, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
        let do_clip = !centroid_poly || !s.centroid_whole;
        let g = if do_clip && !self.extent.contains_rect(&geom::bbox(g)) {
            geom::clip_to_rect(g, &self.extent)?
        } else {
            g.clone()
        };
        let parts = shapes(&g);
        if parts.is_empty() {
            return None;
        }

        // Point symbols as obstacles (QgsVectorLayerLabelProvider::getPointObstacleGeometry).
        let obstacle_parts = match f.geometry {
            Geometry::Point(_) | Geometry::MultiPoint(_) if !f.symbols.is_empty() => {
                Some(self.point_obstacles(f.geometry, &f.symbols, ctx))
            }
            _ => None,
        };
        let symbol_size = obstacle_parts
            .as_ref()
            .and_then(|o| {
                let mut b = Rect::empty();
                for s in o {
                    if let Shape::Polygon(r) = s {
                        for p in &r[0] {
                            b.expand(p.0, p.1);
                        }
                    }
                }
                (!b.is_empty()).then(|| (b.width(), b.height()))
            })
            .unwrap_or((0.0, 0.0));

        let (w, h) = (doc.width * self.mupp, doc.height * self.mupp);
        let quad = match self.dd(s, "OffsetQuad", ctx).and_then(|v| num(&v)) {
            Some(q) if (0.0..=8.0).contains(&q) => (q as i64, true),
            _ => (s.quad_offset, false),
        };
        let quad_offset = match quad.0 {
            0 => (-1.0, 1.0),
            1 => (0.0, 1.0),
            2 => (1.0, 1.0),
            3 => (-1.0, 0.0),
            5 => (1.0, 0.0),
            6 => (-1.0, -1.0),
            7 => (0.0, -1.0),
            8 => (1.0, -1.0),
            _ => (0.0, 0.0),
        };
        let position_offset = (self.map_units(s.x_offset, s.offset_unit), self.map_units(-s.y_offset, s.offset_unit));
        let mut angle = 0.0;
        let layer_rotation = s.angle_offset.abs() > 1e-12;
        if layer_rotation {
            angle = (360.0 - s.angle_offset) * PI / 180.0;
        }
        let mut dd_rotation = false;
        if let Some(r) = self.dd(s, "LabelRotation", ctx).and_then(|v| num(&v)) {
            dd_rotation = true;
            angle = (360.0 - r) * PI / 180.0;
        }
        let mut fixed_position = None;
        if let (Some(x), Some(y)) = (
            self.dd(s, "PositionX", ctx).and_then(|v| num(&v)),
            self.dd(s, "PositionY", ctx).and_then(|v| num(&v)),
        ) {
            if layer_rotation && !dd_rotation {
                angle = 0.0;
            }
            let (mut xd, mut yd) = (0.0, 0.0);
            match self.dd(s, "Hali", ctx).map(|v| to_str(&v).to_ascii_lowercase()).as_deref() {
                Some("center") => xd -= w / 2.0,
                Some("right") => xd -= w,
                _ => {}
            }
            match self.dd(s, "Vali", ctx).map(|v| to_str(&v).to_ascii_lowercase()).as_deref() {
                Some("top") => yd -= h,
                Some("base") => yd -= h * font.descent / (font.ascent + font.descent),
                Some(v @ ("cap" | "half")) => {
                    let line = font.ascent + font.descent;
                    let descent_ratio = font.descent / line;
                    let cap_ratio = (font.cap_height + 1.0 + font.descent) / line;
                    yd -= h * cap_ratio;
                    if v == "half" {
                        yd += h * (cap_ratio - descent_ratio) / 2.0;
                    }
                }
                _ => {}
            }
            if dd_rotation {
                let (c, sn) = (angle.cos(), angle.sin());
                (xd, yd) = (xd * c - yd * sn, xd * sn + yd * c);
            }
            fixed_position = Some((x + xd, y + yd));
        }
        let fixed_angle = (dd_rotation || (fixed_position.is_none() && angle.abs() > 1e-12)).then_some(angle);

        let mut distance = self.px(&s.dist);
        if let Some(d) = self.dd(s, "LabelDistance", ctx).and_then(|v| num(&v)) {
            distance = self.px(&Size { value: d, ..s.dist });
        }
        let outside = s.polygon_flags & 1 != 0;
        match s.placement {
            Placement::Line | Placement::Curved | Placement::PerimeterCurved => {
                distance = distance.signum() * distance.abs().max(1.0);
                if distance == 0.0 {
                    distance = 1.0;
                }
            }
            Placement::AroundPoint | Placement::OverPoint | Placement::Horizontal | Placement::Free if outside => {
                distance = distance.max(2.0);
            }
            Placement::OutsidePolygons => distance = distance.max(2.0),
            _ => {}
        }
        let dist_label = if distance.abs() > 1e-12 { self.mupp * distance } else { 0.0 };
        let priority = self
            .dd(s, "Priority", ctx)
            .and_then(|v| num(&v))
            .map(|p| 1.0 - p.clamp(0.0, 10.0) / 10.0);
        let z_index = self.dd(s, "ZIndex", ctx).and_then(|v| num(&v)).unwrap_or(s.z_index);
        let always_show = self.dd(s, "AlwaysShow", ctx).is_some_and(|v| truthy(&v));

        // Drawing payload (the document metrics and the evaluated text format).
        let mut color = s.format.color;
        if let Some(c) = self.dd(s, "Color", ctx).and_then(|v| crate::symbology::Rgba::parse(&to_str(&v))) {
            color = c;
        }
        let buffer = s.format.buffer.as_ref().map(|b| text::Buffer {
            width_px: self.px(&b.size),
            color: b.color,
            opacity: b.opacity,
            fill_interior: b.fill_interior,
            join: b.join,
        });
        let payload = self.payloads.len();
        self.payloads.push(Payload {
            doc,
            color,
            opacity: s.format.opacity,
            buffer,
            align: s.multiline_align,
            z_index,
            render_order,
            area: w * h,
        });

        let visual_margin = (0.0, (0.25 * font.ascent).max(0.0) * self.mupp, 0.0, (1.0 + font.descent) * self.mupp);
        let _ = layer;
        Some(LabelFeature {
            fid: f.fid,
            sub_part: 0,
            parts,
            size: if draw { (w, h) } else { (0.0, 0.0) },
            fixed_position,
            fixed_angle,
            has_fixed_quadrant: quad.1,
            quad_offset,
            position_offset,
            offset_from_symbol: s.offset_type == 1,
            always_show,
            dist_label,
            max_distance: self.map_units(s.max_distance.value, s.max_distance.unit),
            priority,
            overlap: s.overlap,
            allow_degraded: s.allow_degraded,
            prefer_closer: s.prefer_closer,
            position_order: s.position_order.clone(),
            visual_margin,
            symbol_size,
            obstacle,
            obstacle_factor: s.obstacle_factor,
            obstacle_parts,
            label_text: label,
            z_index,
            payload,
        })
    }

    /// Marker symbol bounds in pixels, truncated to whole pixels and mapped back to map units.
    fn point_obstacles(&self, g: &Geometry<f64>, symbols: &[&Symbol], ctx: &Attrs) -> Vec<Shape> {
        let mut out = Vec::new();
        let mut points = Vec::new();
        geom::for_each_coord(g, &mut |c: Coord<f64>| points.push(c));
        for c in points {
            let (px, py) = self.to_px(c.x, c.y);
            let mut bounds: Option<(f64, f64, f64, f64)> = None;
            for sym in symbols {
                for l in &sym.layers {
                    let Some(b) = l.resolve(ctx).and_then(|k| marker_bounds(&k, px, py, &self.units)) else { continue };
                    bounds = Some(match bounds {
                        None => b,
                        Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
                    });
                }
            }
            let Some((l, t, r, b)) = bounds else { continue };
            let corners = [(l, t), (r, t), (r, b), (l, b), (l, t)];
            let ring: Vec<(f64, f64)> = corners
                .iter()
                .map(|(x, y)| {
                    let (mx, my) = (*x as i64 as f64, *y as i64 as f64);
                    (self.xmin + mx * self.mupp, self.ymax - my * self.mupp)
                })
                .collect();
            out.push(Shape::Polygon(vec![ring]));
        }
        out
    }

    /// Solve and draw (QgsLabelingEngine::solve + drawLabels).
    pub fn draw(mut self, pm: &mut Pixmap) {
        let e = &self.extent;
        let t0 = std::time::Instant::now();
        let placed = self.pal.solve(BBox { xmin: e.xmin, ymin: e.ymin, xmax: e.xmax, ymax: e.ymax });
        let solve_time = t0.elapsed();
        let (mut t_path, mut t_buffer, mut t_text) = (std::time::Duration::ZERO, std::time::Duration::ZERO, std::time::Duration::ZERO);
        let mut placed: Vec<_> = placed.into_iter().collect();
        // QgsLabelSorter: z-index, then layer order (lower layers first), then larger labels first.
        placed.sort_by(|a, b| {
            let (pa, pb) = (&self.payloads[a.payload], &self.payloads[b.payload]);
            pa.z_index
                .partial_cmp(&pb.z_index)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(pa.render_order.cmp(&pb.render_order))
                .then(pb.area.partial_cmp(&pa.area).unwrap_or(std::cmp::Ordering::Equal))
        });
        let rects_only = std::env::var("RGIS_LABEL_RECTS").is_ok();
        for p in placed {
            let pay = &self.payloads[p.payload];
            let align = match pay.align {
                MultiLineAlign::Left => HAlign::Left,
                MultiLineAlign::Center => HAlign::Center,
                MultiLineAlign::Right => HAlign::Right,
                MultiLineAlign::Justify => HAlign::Left,
                MultiLineAlign::FollowPlacement => match p.quadrant {
                    Quadrant::AboveLeft | Quadrant::Left | Quadrant::BelowLeft => HAlign::Right,
                    Quadrant::Above | Quadrant::Over | Quadrant::Below => HAlign::Center,
                    _ => HAlign::Left,
                },
            };
            let origin = self.to_px(p.x[0], p.y[0]);
            if rects_only {
                // QGIS's DrawLabelRectOnly (debugging aid).
                let mut pb = tiny_skia::PathBuilder::new();
                for i in 0..4 {
                    let (x, y) = self.to_px(p.x[i], p.y[i]);
                    if i == 0 {
                        pb.move_to(x as f32, y as f32);
                    } else {
                        pb.line_to(x as f32, y as f32);
                    }
                }
                pb.close();
                if let Some(path) = pb.finish() {
                    let (fill, stroke) = if p.conflicts_with_obstacle {
                        ((255, 0, 0, 100), (255, 0, 0, 150))
                    } else {
                        ((0, 255, 0, 100), (0, 255, 0, 150))
                    };
                    let mut paint = tiny_skia::Paint::default();
                    paint.set_color_rgba8(fill.0, fill.1, fill.2, fill.3);
                    pm.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
                    paint.set_color_rgba8(stroke.0, stroke.1, stroke.2, stroke.3);
                    pm.stroke_path(&path, &paint, &tiny_skia::Stroke::default(), tiny_skia::Transform::identity(), None);
                }
                continue;
            }
            let t = std::time::Instant::now();
            let Some(path) = pay.doc.path(origin, p.alpha, align) else { continue };
            t_path += t.elapsed();
            let t = std::time::Instant::now();
            if let Some(b) = &pay.buffer {
                text::draw_buffer(pm, &pay.doc, &path, origin, p.alpha, align, b);
            }
            t_buffer += t.elapsed();
            let t = std::time::Instant::now();
            text::draw_text(pm, &path, pay.color, pay.opacity);
            t_text += t.elapsed();
        }
        tracing::debug!(?solve_time, ?t_path, ?t_buffer, ?t_text, "labeling stages");
    }
}

/// Simple geometries of a (multi) geometry, as PAL's Util::unmulti.
fn shapes(g: &Geometry<f64>) -> Vec<Shape> {
    let ring = |l: &geo_types::LineString<f64>| l.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>();
    match g {
        Geometry::Point(p) => vec![Shape::Point((p.x(), p.y()))],
        Geometry::MultiPoint(mp) => mp.0.iter().map(|p| Shape::Point((p.x(), p.y()))).collect(),
        Geometry::LineString(l) => vec![Shape::Line(ring(l))],
        Geometry::MultiLineString(ml) => ml.0.iter().map(|l| Shape::Line(ring(l))).collect(),
        Geometry::Polygon(p) => vec![Shape::Polygon(geom::polygon_rings(p).map(ring).collect())],
        Geometry::MultiPolygon(mp) => mp.0.iter().map(|p| Shape::Polygon(geom::polygon_rings(p).map(ring).collect())).collect(),
        Geometry::GeometryCollection(gc) => gc.0.iter().flat_map(shapes).collect(),
        _ => Vec::new(),
    }
}

/// QgsMarkerSymbolLayer::bounds in pixels (left, top, right, bottom).
fn marker_bounds(l: &SymbolLayer, px: f64, py: f64, u: &RenderUnits) -> Option<(f64, f64, f64, f64)> {
    let rect = |w: f64, h: f64, off: (f64, f64), angle: f64| {
        let (cx, cy) = (px + off.0, py + off.1);
        let pts = [(-w / 2.0, -h / 2.0), (w / 2.0, -h / 2.0), (w / 2.0, h / 2.0), (-w / 2.0, h / 2.0)];
        let (s, c) = angle.to_radians().sin_cos();
        let (mut l, mut t, mut r, mut b) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in pts {
            let (x, y) = if angle != 0.0 { (x * c - y * s, x * s + y * c) } else { (x, y) };
            l = l.min(cx + x);
            r = r.max(cx + x);
            t = t.min(cy + y);
            b = b.max(cy + y);
        }
        (l, t, r, b)
    };
    let rotated = |off: (f64, f64), angle: f64| {
        if angle == 0.0 {
            off
        } else {
            let (s, c) = angle.to_radians().sin_cos();
            (off.0 * c - off.1 * s, off.0 * s + off.1 * c)
        }
    };
    match l {
        SymbolLayer::Marker { width, height, stroke, angle, offset, .. } => {
            let (w, h) = (width.px(u), height.px(u));
            let off = rotated((offset.0.px(u), offset.1.px(u)), *angle);
            let (l, t, r, b) = rect(w, h, off, *angle);
            let pen = if stroke.visible { stroke.width.px(u) } else { 0.0 } + 1.0;
            Some((l - pen / 2.0, t - pen / 2.0, r + pen / 2.0, b + pen / 2.0))
        }
        SymbolLayer::SvgMarker { size, fixed_aspect, path, stroke_width, angle, offset, .. } => {
            let w = size.px(u);
            if (w as i64) < 1 || w > 10_000.0 {
                return None;
            }
            let aspect = if *fixed_aspect > 0.0 {
                *fixed_aspect
            } else {
                path.as_deref().and_then(crate::svg::aspect).unwrap_or(1.0)
            };
            let h = w * aspect;
            let off = rotated((offset.0.px(u), offset.1.px(u)), *angle);
            let (l, t, r, b) = rect(w, h, off, *angle);
            let sw = stroke_width.px(u) + 0.5;
            Some((l - sw / 2.0, t - sw / 2.0, r + sw / 2.0, b + sw / 2.0))
        }
        _ => None,
    }
}

/// Project-wide labeling engine settings.
pub fn engine_settings(project: &crate::project::Project) -> EngineSettings {
    EngineSettings::from_project(|k| project.text_prop(&format!("PAL/{k}")).map(String::from))
}
