//! Labeling, ported from QGIS: QgsPalLayerSettings registration (labeling/settings.rs, here),
//! the PAL placement engine (labeling/pal.rs) and QgsTextRenderer drawing (text.rs).
//!
//! Flow as in QgsMapRendererJob: while each vector layer is drawn its features are registered
//! with the engine; after all layers, the engine solves the placement problem once for the whole
//! map and draws the labels on top.

pub mod settings;

mod index;
mod line;
mod pal;
mod rtree;
mod shape;
mod stdsort;

use crate::render::FillNonEmpty;
use geo_types::{Coord, Geometry};
use serde_json::{Value, json};
use std::f64::consts::PI;
use tiny_skia::Pixmap;

use crate::expression::{Attrs, num, to_str, truthy};
use crate::geom::{self, Rect};
use crate::project::MapLayer;
use crate::render::MapSettings;
use crate::symbology::{RenderUnits, Size, Symbol, SymbolLayer, Unit};
use crate::text::{self, HAlign, LabelDocument};

use index::BBox;
use pal::{LabelFeature, LineLabel, Pal, PalLayer, Quadrant};
use settings::{
    EngineSettings, LabelingRule, MultiLineAlign, ObstacleType, PalSettings, Placement, TextAnchor,
};
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
    /// Curved labels: the graphemes each part draws, and the letter spacing (pixels).
    graphemes: Option<(Vec<String>, f64)>,
    background: Option<settings::BackgroundSettings>,
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

    /// Whether a labeling rule registers anything at this scale: its scale range, then the
    /// settings' scale visibility (hidden labels still register as obstacles).
    pub fn rule_active(&self, rule: &LabelingRule) -> bool {
        if let Some((mindenom, maxdenom)) = rule.scale_range
            && ((mindenom > 0.0 && self.scale < mindenom)
                || (maxdenom > 0.0 && self.scale > maxdenom))
        {
            return false;
        }
        let s = &rule.settings;
        let visible = !s.scale_visibility
            || !((s.minimum_scale > 0.0 && self.scale >= s.minimum_scale)
                || (s.maximum_scale > 0.0 && self.scale < s.maximum_scale));
        visible || s.obstacle
    }

    /// Register a layer's labels (QgsVectorLayerLabelProvider per labeling rule).
    pub fn register_layer(
        &mut self,
        layer: &MapLayer,
        features: &[LabelSource],
        vars: &Attrs,
        render_order: usize,
    ) {
        for rule in &layer.labeling {
            if let Some((mindenom, maxdenom)) = rule.scale_range
                && ((mindenom > 0.0 && self.scale < mindenom)
                    || (maxdenom > 0.0 && self.scale > maxdenom))
            {
                continue;
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
        let obstacle_type = if is_point_layer && has_renderer {
            ObstacleType::PolygonWhole
        } else {
            s.obstacle_type
        };
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
        pl.merge_lines = s.merge_lines;
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
        let obstacle = self
            .dd(s, "IsObstacle", ctx)
            .map(|v| truthy(&v))
            .unwrap_or(s.obstacle);
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
        if size.unit == Unit::MapUnit
            && s.font_limit_pixel_size
            && (s.font_min_pixel_size > px as i64 || px as i64 > s.font_max_pixel_size)
        {
            return None;
        }
        let font = text::font(&s.format.font, px)?;

        // evaluateLabelContent
        let mut label = s
            .expression
            .as_ref()
            .map(|e| e.eval(ctx))
            .map(|v| to_str(&v))
            .unwrap_or_default();
        match s.format.capitalization {
            1 => label = label.to_uppercase(),
            2 => label = label.to_lowercase(),
            _ => {}
        }
        let wrap = if s.wrap_char.is_empty() {
            "\n"
        } else {
            s.wrap_char.as_str()
        };
        let lines: Vec<&str> = label.split(wrap).flat_map(|l| l.split('\n')).collect();
        // QgsPalLayerSettings::dataDefinedValEval FontLetterSpacing: in the font size unit,
        // converted to painter units (QFont::AbsoluteSpacing).
        let letter_spacing = self
            .dd(s, "FontLetterSpacing", ctx)
            .and_then(|v| num(&v))
            .unwrap_or(s.format.letter_spacing);
        let letter_spacing = self.px(&Size {
            value: letter_spacing,
            ..s.format.size
        });
        let doc = LabelDocument::new(font.clone(), &lines, s.format.line_height, letter_spacing);
        let curved = matches!(s.placement, Placement::Curved | Placement::PerimeterCurved);
        // QgsTextLabelFeature::calculateTextMetrics: each grapheme's own advance (letter
        // spacing included, no kerning between graphemes) and the font's height and descent.
        let graphemes: Vec<String> = if curved {
            lines.iter().flat_map(|l| split_graphemes(l)).collect()
        } else {
            Vec::new()
        };
        let metrics = curved.then(|| line::TextMetrics {
            widths: graphemes
                .iter()
                .map(|g| font.shape_spaced(g, letter_spacing).width * self.mupp)
                .collect(),
            heights: vec![(font.ascent + font.descent) * self.mupp; graphemes.len()],
            descents: vec![font.descent * self.mupp; graphemes.len()],
            graphemes: graphemes.clone(),
        });

        // Geometry: clipped to the (buffered) extent unless a whole-polygon centroid is wanted.
        let g = f.geometry;
        let centroid_poly = matches!(s.placement, Placement::AroundPoint | Placement::OverPoint)
            && matches!(g, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
        let is_line = matches!(g, Geometry::LineString(_) | Geometry::MultiLineString(_));
        // UseEntireLine: the whole (unclipped) line, so anchors follow the full feature.
        let do_clip =
            (!centroid_poly || !s.centroid_whole) && !(is_line && s.line_anchor_entire_line);
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
        let position_offset = (
            self.map_units(s.x_offset, s.offset_unit),
            self.map_units(-s.y_offset, s.offset_unit),
        );
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
            match self
                .dd(s, "Hali", ctx)
                .map(|v| to_str(&v).to_ascii_lowercase())
                .as_deref()
            {
                Some("center") => xd -= w / 2.0,
                Some("right") => xd -= w,
                _ => {}
            }
            match self
                .dd(s, "Vali", ctx)
                .map(|v| to_str(&v).to_ascii_lowercase())
                .as_deref()
            {
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
        let fixed_angle =
            (dd_rotation || (fixed_position.is_none() && angle.abs() > 1e-12)).then_some(angle);

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
            Placement::AroundPoint
            | Placement::OverPoint
            | Placement::Horizontal
            | Placement::Free
                if outside =>
            {
                distance = distance.max(2.0);
            }
            Placement::OutsidePolygons => distance = distance.max(2.0),
            _ => {}
        }
        let dist_label = if distance.abs() > 1e-12 {
            self.mupp * distance
        } else {
            0.0
        };
        let priority = self
            .dd(s, "Priority", ctx)
            .and_then(|v| num(&v))
            .map(|p| 1.0 - p.clamp(0.0, 10.0) / 10.0);
        let z_index = self
            .dd(s, "ZIndex", ctx)
            .and_then(|v| num(&v))
            .unwrap_or(s.z_index);
        let always_show = self.dd(s, "AlwaysShow", ctx).is_some_and(|v| truthy(&v));

        // Drawing payload (the document metrics and the evaluated text format).
        let mut color = s.format.color;
        if let Some(c) = self
            .dd(s, "Color", ctx)
            .and_then(|v| crate::symbology::Rgba::parse(&to_str(&v)))
        {
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
            graphemes: curved.then_some((graphemes, letter_spacing)),
            background: s.format.background.clone(),
        });

        let visual_margin = (
            0.0,
            (0.25 * font.ascent).max(0.0) * self.mupp,
            0.0,
            (1.0 + font.descent) * self.mupp,
        );
        let _ = layer;
        // generateLabelFeature: curved angle limits (degrees in, radians out), line anchor and
        // overrun (smoothed over a fixed 1 mm).
        let (angle_in, angle_out) = if curved {
            (
                s.max_curved_char_angle_in,
                -s.max_curved_char_angle_out.abs(),
            )
        } else {
            (20.0, -20.0)
        };
        let text_point = self
            .dd(s, "LineAnchorTextPoint", ctx)
            .and_then(|v| TextAnchor::from_dd(&to_str(&v)))
            .unwrap_or(s.line_anchor_text_point);
        let line = matches!(
            f.geometry,
            Geometry::LineString(_) | Geometry::MultiLineString(_)
        )
        .then(|| LineLabel {
            flags: s.line_flags,
            anchor_percent: s.line_anchor_percent,
            anchor_strict: s.line_anchor_strict,
            text_point: text_point.resolve(s.line_anchor_percent),
            overrun: if s.overrun_distance.value != 0.0 {
                self.map_units(s.overrun_distance.value, s.overrun_distance.unit)
            } else {
                0.0
            },
            overrun_smooth: self.map_units(1.0, Unit::Mm),
            max_angle_inside: angle_in.clamp(20.0, 60.0) * PI / 180.0,
            max_angle_outside: angle_out.clamp(-95.0, -20.0) * PI / 180.0,
            metrics,
            ignore_whitespace: s.ignore_whitespace_collisions,
        });
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
            line,
            repeat_distance: if s.repeat_distance.value != 0.0 {
                self.map_units(s.repeat_distance.value, s.repeat_distance.unit)
            } else {
                0.0
            },
            no_repeat_distance: s
                .min_distance_to_duplicates
                .filter(|d| d.value > 0.0)
                .map(|d| self.map_units(d.value, d.unit))
                .unwrap_or(0.0),
            min_size: if s.merge_lines
                && s.min_feature_size > 0.0
                && matches!(
                    f.geometry,
                    Geometry::LineString(_) | Geometry::MultiLineString(_)
                ) {
                self.map_units(s.min_feature_size, Unit::Mm)
            } else {
                0.0
            },
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
                    let Some(b) = l
                        .resolve(ctx)
                        .and_then(|k| marker_bounds(&k, px, py, &self.units))
                    else {
                        continue;
                    };
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

    /// QgsTextRenderer::drawBackground for a placed label part (shape backgrounds): the
    /// label rectangle grown by the buffer size (or a fixed size), centred on the label and
    /// rotated with it, filled with the background's fill symbol.
    fn draw_background(
        &self,
        pm: &mut Pixmap,
        bg: &settings::BackgroundSettings,
        xs: &[f64; 4],
        ys: &[f64; 4],
        alpha: f64,
    ) {
        let c: Vec<(f64, f64)> = (0..4).map(|i| self.to_px(xs[i], ys[i])).collect();
        let (lw, lh) = (
            (c[1].0 - c[0].0).hypot(c[1].1 - c[0].1),
            (c[3].0 - c[0].0).hypot(c[3].1 - c[0].1),
        );
        let center = ((c[0].0 + c[2].0) / 2.0, (c[0].1 + c[2].1) / 2.0);
        let (sx, sy) = (self.px(&bg.size.0), self.px(&bg.size.1));
        let (mut w, mut h) = (lw, lh);
        if bg.size_type == 1 {
            (w, h) = (sx, sy);
        } else if bg.size_type == 0 {
            match bg.shape {
                1 => {
                    w = w.max(h);
                    h = w;
                }
                3 => {
                    h = w.hypot(h);
                    w = h;
                }
                2 => {
                    h *= std::f64::consts::SQRT_2;
                    w *= std::f64::consts::SQRT_2;
                }
                _ => {}
            }
            w += 2.0 * sx;
            h += 2.0 * sy;
        }
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        // Painter rotation in degrees (y down): synced with the label, plus/or a fixed one.
        let (rot, rot_offset) = match bg.rotation_type {
            2 => (0.0, bg.rotation),
            1 => (-alpha.to_degrees(), bg.rotation),
            _ => (-alpha.to_degrees(), 0.0),
        };
        let (xoff, yoff) = (self.px(&bg.offset.0), self.px(&bg.offset.1));
        let (s0, c0) = rot.to_radians().sin_cos();
        let (s1, c1) = rot_offset.to_radians().sin_cos();
        let local = |x: f64, y: f64| {
            let (x, y) = (x * c1 - y * s1 + xoff, x * s1 + y * c1 + yoff);
            (center.0 + x * c0 - y * s0, center.1 + x * s0 + y * c0)
        };
        let ring: Vec<geo_types::Coord<f64>> = match bg.shape {
            2 | 3 => (0..=72)
                .map(|i| {
                    let t = i as f64 * PI / 36.0;
                    let (x, y) = local(w / 2.0 * t.cos(), h / 2.0 * t.sin());
                    geo_types::Coord { x, y }
                })
                .collect(),
            _ => [
                (-1.0, -1.0),
                (1.0, -1.0),
                (1.0, 1.0),
                (-1.0, 1.0),
                (-1.0, -1.0),
            ]
            .iter()
            .map(|(a, b)| {
                let (x, y) = local(a * w / 2.0, b * h / 2.0);
                geo_types::Coord { x, y }
            })
            .collect(),
        };
        let poly = Geometry::Polygon(geo_types::Polygon::new(geo_types::LineString(ring), vec![]));
        let mut sym = bg.fill.clone();
        sym.opacity *= bg.opacity;
        let attrs = Attrs::default();
        crate::render::draw_symbol(
            pm,
            &sym,
            &poly,
            &|c| (c.x as f32, c.y as f32),
            &self.units,
            &attrs,
            None,
        );
    }

    /// Solve and draw (QgsLabelingEngine::solve + drawLabels).
    pub fn draw(mut self, pm: &mut Pixmap) {
        let e = &self.extent;
        let t0 = std::time::Instant::now();
        let placed = self.pal.solve(BBox {
            xmin: e.xmin,
            ymin: e.ymin,
            xmax: e.xmax,
            ymax: e.ymax,
        });
        let solve_time = t0.elapsed();
        let (mut t_path, mut t_buffer, mut t_text) = (
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        );
        let mut placed: Vec<_> = placed.into_iter().collect();
        // QgsLabelSorter: z-index, then layer order (lower layers first), then larger labels first.
        placed.sort_by(|a, b| {
            let (pa, pb) = (&self.payloads[a.payload], &self.payloads[b.payload]);
            pa.z_index
                .partial_cmp(&pb.z_index)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(pa.render_order.cmp(&pb.render_order))
                .then(
                    pb.area
                        .partial_cmp(&pa.area)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        let rects_only = std::env::var("RGIS_LABEL_RECTS").is_ok();
        for p in placed {
            let pay = &self.payloads[p.payload];
            if tracing::enabled!(target: "rgis::labels", tracing::Level::DEBUG) {
                // One line per placed label (bbox over all parts), to diff against QGIS's
                // QgsLabelingResults.
                let (mut b0, mut b1) = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
                let quads = std::iter::once((p.x, p.y)).chain(p.parts.iter().map(|q| (q.x, q.y)));
                for (xs, ys) in quads {
                    for i in 0..4 {
                        b0 = (b0.0.min(xs[i]), b0.1.min(ys[i]));
                        b1 = (b1.0.max(xs[i]), b1.1.max(ys[i]));
                    }
                }
                let lf = self.pal.layers[p.layer].feature(p.feature);
                let origins: Vec<[f64; 3]> = std::iter::once([p.x[0], p.y[0], p.alpha])
                    .chain(p.parts.iter().map(|q| [q.x[0], q.y[0], q.alpha]))
                    .collect();
                tracing::debug!(target: "rgis::labels", "{}", json!({"layer": self.pal.layers[p.layer].name, "fid": lf.fid, "text": lf.label_text, "bbox": [b0.0, b0.1, b1.0, b1.1], "origins": origins}));
            }
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
                    pm.fill_nonempty(
                        &path,
                        &paint,
                        tiny_skia::FillRule::Winding,
                        tiny_skia::Transform::identity(),
                        None,
                    );
                    paint.set_color_rgba8(stroke.0, stroke.1, stroke.2, stroke.3);
                    pm.stroke_path(
                        &path,
                        &paint,
                        &tiny_skia::Stroke::default(),
                        tiny_skia::Transform::identity(),
                        None,
                    );
                }
                continue;
            }
            // drawLabelPrivate(Background) for every part, before buffers and text.
            if let Some(bg) = &pay.background {
                let quads = std::iter::once((p.x, p.y, p.alpha))
                    .chain(p.parts.iter().map(|q| (q.x, q.y, q.alpha)));
                for (xs, ys, a) in quads {
                    self.draw_background(pm, bg, &xs, &ys, a);
                }
            }
            if let Some((graphemes, spacing)) = &pay.graphemes {
                // drawLabelPrivate per part: every grapheme's buffer, then every grapheme's text.
                let parts: Vec<(f64, f64, f64, usize)> =
                    std::iter::once((p.x[0], p.y[0], p.alpha, p.part_id))
                        .chain(p.parts.iter().map(|q| (q.x[0], q.y[0], q.alpha, q.part_id)))
                        .collect();
                let docs: Vec<_> = parts
                    .iter()
                    .filter_map(|&(x, y, a, i)| {
                        let g = graphemes.get(i)?;
                        let doc =
                            LabelDocument::new(pay.doc.font.clone(), &[g.as_str()], 1.0, *spacing);
                        let o = self.to_px(x, y);
                        let path = doc.path(o, a, align)?;
                        Some((doc, path, o, a))
                    })
                    .collect();
                if let Some(b) = &pay.buffer {
                    for (doc, path, o, a) in &docs {
                        text::draw_buffer(pm, doc, path, *o, *a, align, b);
                    }
                }
                for (_, path, _, _) in &docs {
                    text::draw_text(pm, path, pay.color, pay.opacity);
                }
                continue;
            }
            let t = std::time::Instant::now();
            let Some(path) = pay.doc.path(origin, p.alpha, align) else {
                continue;
            };
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

/// QgsPalLabeling::splitToGraphemes for the scripts used here: each character with the
/// combining marks, joiners and variation selectors that follow it.
fn split_graphemes(s: &str) -> Vec<String> {
    let extends = |c: char| matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F | 0x200D | 0xFE00..=0xFE0F);
    let mut out: Vec<String> = Vec::new();
    let mut join_next = false;
    for c in s.chars() {
        match out.last_mut() {
            Some(last) if extends(c) || join_next => last.push(c),
            _ => out.push(c.to_string()),
        }
        join_next = c == '\u{200D}';
    }
    out
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
        Geometry::MultiPolygon(mp) => {
            mp.0.iter()
                .map(|p| Shape::Polygon(geom::polygon_rings(p).map(ring).collect()))
                .collect()
        }
        Geometry::GeometryCollection(gc) => gc.0.iter().flat_map(shapes).collect(),
        _ => Vec::new(),
    }
}

/// QgsMarkerSymbolLayer::bounds in pixels (left, top, right, bottom).
fn marker_bounds(
    l: &SymbolLayer,
    px: f64,
    py: f64,
    u: &RenderUnits,
) -> Option<(f64, f64, f64, f64)> {
    let rect = |w: f64, h: f64, off: (f64, f64), angle: f64| {
        let (cx, cy) = (px + off.0, py + off.1);
        let pts = [
            (-w / 2.0, -h / 2.0),
            (w / 2.0, -h / 2.0),
            (w / 2.0, h / 2.0),
            (-w / 2.0, h / 2.0),
        ];
        let (s, c) = angle.to_radians().sin_cos();
        let (mut l, mut t, mut r, mut b) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in pts {
            let (x, y) = if angle != 0.0 {
                (x * c - y * s, x * s + y * c)
            } else {
                (x, y)
            };
            l = l.min(cx + x);
            r = r.max(cx + x);
            t = t.min(cy + y);
            b = b.max(cy + y);
        }
        (l, t, r, b)
    };
    // QgsMarkerSymbolLayer::markerOffset: half the size towards the anchor point.
    let anchored = |off: (f64, f64), anchor: (u8, u8), w: f64, h: f64| {
        let dx = match anchor.0 {
            0 => w / 2.0,
            2 => -w / 2.0,
            _ => 0.0,
        };
        let dy = match anchor.1 {
            0 => h / 2.0,
            2 => -h / 2.0,
            _ => 0.0,
        };
        (off.0 + dx, off.1 + dy)
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
        SymbolLayer::Marker {
            width,
            height,
            stroke,
            angle,
            offset,
            anchor,
            ..
        } => {
            let (w, h) = (width.px(u), height.px(u));
            let off = rotated(
                anchored((offset.0.px(u), offset.1.px(u)), *anchor, w, h),
                *angle,
            );
            let (l, t, r, b) = rect(w, h, off, *angle);
            let pen = if stroke.visible {
                stroke.width.px(u)
            } else {
                0.0
            } + 1.0;
            Some((l - pen / 2.0, t - pen / 2.0, r + pen / 2.0, b + pen / 2.0))
        }
        SymbolLayer::SvgMarker {
            size,
            fixed_aspect,
            path,
            stroke_width,
            angle,
            offset,
            anchor,
            ..
        } => {
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
            let off = rotated(
                anchored((offset.0.px(u), offset.1.px(u)), *anchor, w, h),
                *angle,
            );
            let (l, t, r, b) = rect(w, h, off, *angle);
            let sw = stroke_width.px(u) + 0.5;
            Some((l - sw / 2.0, t - sw / 2.0, r + sw / 2.0, b + sw / 2.0))
        }
        // QgsFontMarkerSymbolLayer::bounds: character advance × symbol size.
        SymbolLayer::FontMarker {
            font,
            style,
            chr,
            size,
            angle,
            offset,
            anchor,
            ..
        } => {
            let px = size.px(u);
            let req = crate::text::FontRequest {
                family: font.clone(),
                weight: 400,
                italic: false,
                style: style.clone(),
            };
            let w = crate::text::font(&req, (px.round() as i32).max(2))?.horizontal_advance(chr);
            let off = rotated(
                anchored((offset.0.px(u), offset.1.px(u)), *anchor, px, px),
                *angle,
            );
            Some(rect(w, px, off, *angle))
        }
        _ => None,
    }
}

/// Project-wide labeling engine settings.
pub fn engine_settings(project: &crate::project::Project) -> EngineSettings {
    EngineSettings::from_project(|k| project.text_prop(&format!("PAL/{k}")).map(String::from))
}
