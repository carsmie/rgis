//! Port of QGIS's PAL labeling engine (src/core/pal): feature parts, label candidates, obstacle
//! costs, the conflict graph, and the FALP + chain search solver — same algorithm, same costs,
//! same tie-breaking order where QGIS's is deterministic.
//!
//! Faster than QGIS mainly by construction: candidates are plain rectangles tested with
//! closed-form predicates instead of GEOS prepared geometries, and the conflict cache is only
//! used for rotated candidates (axis-aligned conflicts are a four-comparison test).

use std::collections::HashMap;
use std::f64::consts::{FRAC_1_SQRT_2, FRAC_PI_2, FRAC_PI_4, PI};

use super::index::{BBox, GridIndex};
use super::line::{self, Direction, TextMetrics};
use super::rtree::PalRtree;
use super::settings::{
    EngineSettings, ObstacleType, OverlapHandling, Placement, PointPosition, TextAnchor,
};
use super::shape::{self, Pt, Shape};
use super::stdsort::std_sort;

/// Qgis::LabelLinePlacementFlag.
const ON_LINE: u32 = 1;
const ABOVE_LINE: u32 = 2;
const BELOW_LINE: u32 = 4;
const MAP_ORIENTATION: u32 = 8;

/// The line placement part of QgsLabelFeature (map units, radians).
#[derive(Debug, Clone)]
pub struct LineLabel {
    pub flags: u32,
    pub anchor_percent: f64,
    pub anchor_strict: bool,
    /// Resolved (QgsLabelFeature::lineAnchorTextPoint, never Follow).
    pub text_point: TextAnchor,
    pub overrun: f64,
    pub overrun_smooth: f64,
    pub max_angle_inside: f64,
    pub max_angle_outside: f64,
    /// Per-grapheme metrics (curved placements only).
    pub metrics: Option<TextMetrics>,
    pub ignore_whitespace: bool,
}

const EPSILON: f64 = 1e-9;

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= 4.0 * f64::EPSILON * a.abs().max(b.abs()).max(1.0)
}

fn near_eps(a: f64, b: f64, eps: f64) -> bool {
    (a - b).abs() <= eps
}

/// Qgis::LabelQuadrantPosition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Quadrant {
    AboveLeft,
    Above,
    AboveRight,
    Left,
    Over,
    Right,
    BelowLeft,
    Below,
    BelowRight,
}

/// QgsLabelFeature: what a layer's label provider registers for one feature.
#[allow(dead_code)] // some fields are for line/polygon placements not ported yet
#[derive(Debug, Clone)]
pub struct LabelFeature {
    pub fid: i64,
    pub sub_part: i32,
    /// Label geometry parts in map units (QgsPalLabeling::prepareGeometry output, unmulti'd).
    pub parts: Vec<Shape>,
    /// Label size in map units (width, height).
    pub size: (f64, f64),
    pub fixed_position: Option<Pt>,
    pub fixed_angle: Option<f64>,
    pub has_fixed_quadrant: bool,
    pub quad_offset: (f64, f64),
    pub position_offset: (f64, f64),
    pub offset_from_symbol: bool,
    pub always_show: bool,
    /// Label distance in map units.
    pub dist_label: f64,
    pub max_distance: f64,
    /// Feature priority (0 = high … 1 = low), when data defined.
    pub priority: Option<f64>,
    pub overlap: OverlapHandling,
    pub allow_degraded: bool,
    pub prefer_closer: bool,
    pub position_order: Vec<PointPosition>,
    /// QgsMargins (left, top, right, bottom) in map units.
    pub visual_margin: (f64, f64, f64, f64),
    pub symbol_size: (f64, f64),
    pub obstacle: bool,
    pub obstacle_factor: f64,
    /// Explicit obstacle geometry (point symbol bounds), else the label geometry.
    pub obstacle_parts: Option<Vec<Shape>>,
    pub label_text: String,
    pub z_index: f64,
    /// Index of the provider's drawing payload for this label.
    pub payload: usize,
    pub line: Option<LineLabel>,
    /// Repeat distance in map units (0 = no repeats).
    pub repeat_distance: f64,
    /// Minimum length of merged lines in map units (thinning minimum feature size).
    pub min_size: f64,
    /// QgsLabelThinningSettings::noRepeatDistance: same-text labels closer than this (map
    /// units) conflict (0 = off).
    pub no_repeat_distance: f64,
}

/// pal::Layer.
#[allow(dead_code)]
pub struct PalLayer {
    pub name: String,
    pub arrangement: Placement,
    pub priority: f64,
    pub obstacle_type: ObstacleType,
    pub centroid_inside: bool,
    pub upside_down: i64,
    pub label_layer: bool,
    /// Index in the map's layer rendering order (higher = drawn later / on top).
    pub render_order: usize,
    /// QgsLabelLineSettings::mergeLines.
    pub merge_lines: bool,
    features: Vec<LabelFeature>,
    keys: std::collections::HashSet<(i64, i32)>,
    parts: Vec<Part>,
    obstacles: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Point,
    Line,
    Polygon,
}

/// pal::FeaturePart: one simple geometry of a label feature.
#[derive(Debug, Clone)]
struct Part {
    layer: usize,
    feature: usize,
    kind: Kind,
    shape: Shape,
    /// Coordinates PAL works on (exterior ring for polygons).
    x: Vec<f64>,
    y: Vec<f64>,
    bbox: BBox,
    holes: Vec<Part>,
    is_hole: bool,
    /// FeaturePart::totalRepeats: how many parts a line was chopped into.
    total_repeats: usize,
}

#[allow(dead_code)]
impl Part {
    fn new(layer: usize, feature: usize, shape: Shape) -> Part {
        let (kind, coords): (Kind, Vec<Pt>) = match &shape {
            Shape::Point(p) => (Kind::Point, vec![*p]),
            Shape::Line(l) => (Kind::Line, l.clone()),
            Shape::Polygon(r) => (Kind::Polygon, r.first().cloned().unwrap_or_default()),
        };
        let holes = match &shape {
            Shape::Polygon(r) => r[1..]
                .iter()
                .map(|h| {
                    let mut p = Part::new(layer, feature, Shape::Polygon(vec![h.clone()]));
                    p.is_hole = true;
                    p
                })
                .collect(),
            _ => Vec::new(),
        };
        let x: Vec<f64> = coords.iter().map(|c| c.0).collect();
        let y: Vec<f64> = coords.iter().map(|c| c.1).collect();
        let bbox = BBox {
            xmin: x.iter().copied().fold(f64::MAX, f64::min),
            ymin: y.iter().copied().fold(f64::MAX, f64::min),
            xmax: x.iter().copied().fold(f64::MIN, f64::max),
            ymax: y.iter().copied().fold(f64::MIN, f64::max),
        };
        Part {
            layer,
            feature,
            kind,
            shape,
            x,
            y,
            bbox,
            holes,
            is_hole: false,
            total_repeats: 1,
        }
    }

    fn length(&self) -> f64 {
        match &self.shape {
            Shape::Line(l) => shape::line_length(l),
            Shape::Polygon(r) => r.iter().map(|r| shape::line_length(r)).sum(),
            Shape::Point(_) => 0.0,
        }
    }

    fn area(&self) -> f64 {
        match &self.shape {
            Shape::Polygon(r) => shape::polygon_area(r),
            _ => 0.0,
        }
    }

    fn contains_point(&self, p: Pt) -> bool {
        match &self.shape {
            Shape::Polygon(r) => shape::polygon_contains_properly(r, p),
            _ => false,
        }
    }

    /// PointSet::minDistanceToPoint: squared distance to the exterior ring / line / point.
    fn min_dist2(&self, p: Pt) -> f64 {
        match &self.shape {
            Shape::Point(q) => (p.0 - q.0).powi(2) + (p.1 - q.1).powi(2),
            Shape::Line(l) => shape::min_dist2_to_line(l, p),
            Shape::Polygon(r) => r
                .first()
                .map(|e| shape::min_dist2_to_line(e, p))
                .unwrap_or(0.0),
        }
    }
}

/// pal::LabelPosition.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct LabelPosition {
    layer: usize,
    feature: usize,
    part: usize,
    pub x: [f64; 4],
    pub y: [f64; 4],
    /// Rotation in radians, counter-clockwise (0..2π).
    pub alpha: f64,
    pub w: f64,
    pub h: f64,
    cost: f64,
    pub quadrant: Quadrant,
    bbox: BBox,
    /// LabelPosition::boundingBoxForCandidateConflicts: bbox grown by the duplicate distance.
    cbox: BBox,
    prob_feat: usize,
    id: usize,
    num_overlaps: i32,
    conflicts_with_obstacle: bool,
    hard_conflict: bool,
    global_id: usize,
    /// Grapheme index of this part (curved labels).
    pub part_id: usize,
    /// Further parts of a curved label (LabelPosition::nextPart chain), one per grapheme.
    next: Vec<SubPart>,
}

/// A further part of a multi-part (curved) candidate.
#[derive(Debug, Clone)]
pub struct SubPart {
    pub x: [f64; 4],
    pub y: [f64; 4],
    pub alpha: f64,
    pub part_id: usize,
    bbox: BBox,
}

impl SubPart {
    fn quad(&self) -> [Pt; 4] {
        [
            (self.x[0], self.y[0]),
            (self.x[1], self.y[1]),
            (self.x[2], self.y[2]),
            (self.x[3], self.y[3]),
        ]
    }
}

fn quad_ring(q: &[Pt; 4]) -> Shape {
    Shape::Polygon(vec![vec![q[0], q[1], q[2], q[3], q[0]]])
}

impl LabelPosition {
    #[allow(clippy::too_many_arguments)]
    fn new(
        part: &Part,
        x1: f64,
        y1: f64,
        w: f64,
        h: f64,
        alpha: f64,
        cost: f64,
        quadrant: Quadrant,
        upright_only: bool,
    ) -> LabelPosition {
        let mut alpha = alpha;
        while alpha > 2.0 * PI {
            alpha -= 2.0 * PI;
        }
        while alpha < 0.0 {
            alpha += 2.0 * PI;
        }
        let beta = alpha + FRAC_PI_2;
        let (dx1, dy1) = (alpha.cos() * w, alpha.sin() * w);
        let (dx2, dy2) = (beta.cos() * h, beta.sin() * h);
        let mut x = [x1, x1 + dx1, x1 + dx1 + dx2, x1 + dx2];
        let mut y = [y1, y1 + dy1, y1 + dy1 + dy2, y1 + dy2];
        if alpha > FRAC_PI_2 && alpha <= 3.0 * FRAC_PI_2 && upright_only {
            x.swap(0, 2);
            y.swap(0, 2);
            x.swap(1, 3);
            y.swap(1, 3);
            if alpha < PI {
                alpha += PI;
            } else {
                alpha -= PI;
            }
        }
        let bbox = BBox {
            xmin: x.iter().copied().fold(f64::MAX, f64::min),
            ymin: y.iter().copied().fold(f64::MAX, f64::min),
            xmax: x.iter().copied().fold(f64::MIN, f64::max),
            ymax: y.iter().copied().fold(f64::MIN, f64::max),
        };
        LabelPosition {
            layer: part.layer,
            feature: part.feature,
            part: usize::MAX,
            x,
            y,
            alpha,
            w,
            h,
            cost,
            quadrant,
            bbox,
            cbox: bbox,
            prob_feat: 0,
            id: 0,
            num_overlaps: 0,
            conflicts_with_obstacle: false,
            hard_conflict: false,
            global_id: 0,
            part_id: 0,
            next: Vec::new(),
        }
    }

    fn quad(&self) -> [Pt; 4] {
        [
            (self.x[0], self.y[0]),
            (self.x[1], self.y[1]),
            (self.x[2], self.y[2]),
            (self.x[3], self.y[3]),
        ]
    }

    /// LabelPosition::setNextPart: append a part, growing the bounding box.
    fn push_part(&mut self, p: LabelPosition) {
        self.bbox = BBox {
            xmin: self.bbox.xmin.min(p.bbox.xmin),
            ymin: self.bbox.ymin.min(p.bbox.ymin),
            xmax: self.bbox.xmax.max(p.bbox.xmax),
            ymax: self.bbox.ymax.max(p.bbox.ymax),
        };
        self.next.push(SubPart {
            x: p.x,
            y: p.y,
            alpha: p.alpha,
            part_id: p.part_id,
            bbox: p.bbox,
        });
        self.cbox = self.bbox;
    }

    /// Every part as (quad, bbox, alpha), this one first.
    fn all_quads(&self) -> impl Iterator<Item = ([Pt; 4], BBox, f64)> + '_ {
        let own = (self.quad(), self.own_bbox(), self.alpha);
        std::iter::once(own).chain(self.next.iter().map(|p| (p.quad(), p.bbox, p.alpha)))
    }

    fn own_bbox(&self) -> BBox {
        BBox {
            xmin: self.x.iter().copied().fold(f64::MAX, f64::min),
            ymin: self.y.iter().copied().fold(f64::MAX, f64::min),
            xmax: self.x.iter().copied().fold(f64::MIN, f64::max),
            ymax: self.y.iter().copied().fold(f64::MIN, f64::max),
        }
    }

    /// LabelPosition::angleDifferential.
    fn angle_differential(&self) -> f64 {
        let mut diff = 0.0;
        let mut last = self.alpha;
        for p in &self.next {
            let mut d = (p.alpha - last).abs();
            if d > 2.0 * PI {
                d -= 2.0 * PI;
            }
            diff += d.min(2.0 * PI - d);
            last = p.alpha;
        }
        diff
    }

    /// LabelPosition::getDistanceToPoint over the part chain.
    fn distance_to_point(&self, p: Pt) -> f64 {
        let mut d = self.own_distance_to_point(p);
        for part in &self.next {
            if d <= 0.0 {
                break;
            }
            let q = part.quad();
            let ring = [q[0], q[1], q[2], q[3], q[0]];
            let pd = if shape::polygon_contains_properly(&[ring.to_vec()], p) {
                -1.0
            } else {
                shape::min_dist2_to_line(&ring, p).sqrt()
            };
            d = d.min(pd);
        }
        d
    }

    /// LabelPosition::getDistanceToPoint (no outer bounds) of this part.
    fn own_distance_to_point(&self, p: Pt) -> f64 {
        let contains = if self.alpha == 0.0 {
            self.x[0] <= p.0 && self.x[1] >= p.0 && self.y[0] <= p.1 && self.y[2] >= p.1
        } else {
            let q = self.quad();
            shape::polygon_contains_properly(&[vec![q[0], q[1], q[2], q[3], q[0]]], p)
        };
        if contains {
            return -1.0;
        }
        if self.alpha == 0.0 {
            let dx = (self.x[0] - p.0).max(0.0).max(p.0 - self.x[1]);
            let dy = (self.y[0] - p.1).max(0.0).max(p.1 - self.y[2]);
            (dx * dx + dy * dy).sqrt()
        } else {
            let q = self.quad();
            shape::min_dist2_to_line(&[q[0], q[1], q[2], q[3], q[0]], p).sqrt()
        }
    }

    /// Any part intersects the shape (crossesLine / intersectsWithPolygon).
    fn intersects_shape(&self, s: &Shape) -> bool {
        self.all_quads()
            .any(|(q, _, _)| shape::quad_intersects(&q, s))
    }

    /// LabelPosition::polygonIntersectionCost (0..12): the parts' average, rounded up.
    fn polygon_intersection_cost(&self, poly: &Part) -> i32 {
        let mut total = 0.0;
        let mut n = 0;
        for (q, _, _) in self.all_quads() {
            n += 1;
            let mut cost: f64 = 0.0;
            if shape::quad_intersects(&q, &poly.shape) {
                cost += 1.0;
                for i in 0..4 {
                    let mut p = q[i];
                    for _ in 0..2 {
                        if poly.contains_point(p) {
                            cost += 1.0;
                        }
                        p = (
                            (q[i].0 + q[(i + 1) % 4].0) / 2.0,
                            (q[i].1 + q[(i + 1) % 4].1) / 2.0,
                        );
                    }
                }
                let c = ((q[0].0 + q[2].0) / 2.0, (q[0].1 + q[2].1) / 2.0);
                if poly.contains_point(c) {
                    cost += 4.0;
                }
            }
            total += 12.0 * cost / 13.0;
        }
        (total / n as f64).ceil() as i32
    }

    /// LabelPosition::crossesBoundary: some part intersects the polygon without being inside.
    fn crosses_boundary(&self, s: &Shape) -> bool {
        self.all_quads().any(|(q, _, _)| {
            shape::quad_intersects(&q, s)
                && !match s {
                    Shape::Polygon(r) => shape::polygon_contains_quad(r, &q),
                    _ => false,
                }
        })
    }
}

/// QgsLabelingEngine output: the placed labels, sorted for drawing.
#[allow(dead_code)]
pub struct Placed {
    pub layer: usize,
    pub feature: usize,
    pub x: [f64; 4],
    pub y: [f64; 4],
    pub alpha: f64,
    pub quadrant: Quadrant,
    pub payload: usize,
    pub conflicts_with_obstacle: bool,
    /// Grapheme index of the first part, and the further parts (curved labels).
    pub part_id: usize,
    pub parts: Vec<SubPart>,
}

pub struct Pal {
    pub layers: Vec<PalLayer>,
    settings: EngineSettings,
    /// Map units per centimetre at the output resolution.
    map_units_per_cm: f64,
}

#[allow(dead_code)]
impl PalLayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        arrangement: Placement,
        priority: f64,
        obstacle_type: ObstacleType,
        centroid_inside: bool,
        upside_down: i64,
        label_layer: bool,
        render_order: usize,
    ) -> PalLayer {
        PalLayer {
            name,
            arrangement,
            priority: priority.clamp(0.0001, 1.0),
            obstacle_type,
            centroid_inside,
            upside_down,
            label_layer,
            render_order,
            merge_lines: false,
            features: Vec::new(),
            keys: Default::default(),
            parts: Vec::new(),
            obstacles: Vec::new(),
        }
    }

    pub fn feature(&self, i: usize) -> &LabelFeature {
        &self.features[i]
    }

    /// pal::Layer::registerFeature.
    pub fn register(&mut self, layer_index: usize, lf: LabelFeature) -> bool {
        if lf.size.0 < 0.0 || lf.size.1 < 0.0 || !self.keys.insert((lf.fid, lf.sub_part)) {
            return false;
        }
        let fi = self.features.len();
        let well_defined = lf.size.0 > 0.0000001 && lf.size.1 > 0.0000001;
        let geometry_is_obstacle = lf.obstacle_parts.is_none();
        let mut added = false;
        let mut biggest: Option<(f64, Part)> = None;
        for s in &lf.parts {
            let part = Part::new(layer_index, fi, s.clone());
            if (part.kind == Kind::Line && part.x.len() < 2)
                || (part.kind == Kind::Polygon && part.x.len() < 3)
            {
                continue;
            }
            if lf.obstacle && geometry_is_obstacle {
                self.obstacles.push(part.clone());
            }
            if !self.label_layer || !well_defined {
                continue;
            }
            match part.kind {
                Kind::Line | Kind::Polygon if !lf_label_every_part(&lf) => {
                    let size = if part.kind == Kind::Line {
                        part.length()
                    } else {
                        part.area()
                    };
                    if biggest.as_ref().is_none_or(|(b, _)| size > *b) {
                        biggest = Some((size, part));
                    }
                }
                _ => {
                    self.parts.push(part);
                    added = true;
                }
            }
        }
        if lf.obstacle
            && let Some(obs) = &lf.obstacle_parts
        {
            for s in obs {
                let part = Part::new(layer_index, fi, s.clone());
                if (part.kind == Kind::Line && part.x.len() < 2)
                    || (part.kind == Kind::Polygon && part.x.len() < 3)
                {
                    continue;
                }
                self.obstacles.push(part);
            }
        }
        if let Some((_, part)) = biggest {
            self.parts.push(part);
            added = true;
        }
        self.features.push(lf);
        if !added {
            self.keys
                .remove(&(self.features[fi].fid, self.features[fi].sub_part));
        }
        added
    }

    fn max_point_candidates(&self) -> usize {
        match self.keys.len() {
            n if n > 1000 => 4,
            n if n > 500 => 6,
            n if n > 200 => 8,
            n if n > 100 => 12,
            _ => 0,
        }
    }

    fn max_line_candidates(&self) -> usize {
        match self.keys.len() {
            n if n > 1000 => 5,
            n if n > 500 => 10,
            n if n > 200 => 20,
            n if n > 100 => 40,
            _ => 0,
        }
    }

    fn max_polygon_candidates(&self) -> usize {
        match self.keys.len() {
            n if n > 1000 => 5,
            n if n > 500 => 15,
            n if n > 200 => 20,
            n if n > 100 => 25,
            _ => 0,
        }
    }

    fn is_curved(&self) -> bool {
        matches!(
            self.arrangement,
            Placement::Curved | Placement::PerimeterCurved
        )
    }

    /// Layer::joinConnectedFeatures: merge line parts with the same label text that continue
    /// each other, longest first, then drop merged lines below the minimum size.
    fn join_connected(&mut self) {
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, p) in self.parts.iter().enumerate() {
            let text = &self.features[p.feature].label_text;
            if p.kind != Kind::Line || text.is_empty() {
                continue;
            }
            match groups.iter_mut().find(|(t, _)| t == text) {
                Some((_, v)) => v.push(i),
                None => groups.push((text.clone(), vec![i])),
            }
        }
        let mut removed = vec![false; self.parts.len()];
        for (_, mut group) in groups {
            let parts = &self.parts;
            std_sort(&mut group, &|&a: &usize, &b: &usize| {
                parts[a].length() > parts[b].length()
            });
            while group.len() > 1 {
                let target = group.remove(0);
                let mut left = group.clone();
                while let Some(pos) = left
                    .iter()
                    .position(|&o| is_connected(&self.parts[target], &self.parts[o]))
                {
                    let other = left.remove(pos);
                    let a: Vec<Pt> = self.parts[target]
                        .x
                        .iter()
                        .copied()
                        .zip(self.parts[target].y.iter().copied())
                        .collect();
                    let b: Vec<Pt> = self.parts[other]
                        .x
                        .iter()
                        .copied()
                        .zip(self.parts[other].y.iter().copied())
                        .collect();
                    if let Some(merged) = line_merge(&a, &b) {
                        let (layer, feature) =
                            (self.parts[target].layer, self.parts[target].feature);
                        self.parts[target] = Part::new(layer, feature, Shape::Line(merged));
                        removed[other] = true;
                        group.retain(|&g| g != other);
                    }
                }
            }
        }
        let mut i = 0;
        self.parts.retain(|_| {
            let keep = !removed[i];
            i += 1;
            keep
        });
        let features = &self.features;
        self.parts.retain(|p| {
            let min = features[p.feature].min_size;
            min == 0.0 || p.length() >= min
        });
    }

    /// Layer::chopFeaturesAtRepeatDistance: cut lines into equal pieces of at least the
    /// repeat distance (and the label width), each labelled on its own.
    fn chop_at_repeat_distance(&mut self) {
        let mut out = Vec::with_capacity(self.parts.len());
        for part in std::mem::take(&mut self.parts) {
            let lf = &self.features[part.feature];
            let interval = lf.repeat_distance;
            let len = if interval != 0.0 && part.kind == Kind::Line {
                part.length()
            } else {
                0.0
            };
            if interval == 0.0 || part.kind != Kind::Line || len <= interval {
                out.push(part);
                continue;
            }
            let interval = interval * (lf.size.0 / lf.repeat_distance).ceil();
            let segments = (len / interval).floor() as usize;
            if segments <= 1 {
                out.push(part);
                continue;
            }
            let interval = len / segments as f64;
            let pts: Vec<Pt> = part.x.iter().copied().zip(part.y.iter().copied()).collect();
            let n = pts.len();
            let mut cum = vec![0.0; n];
            for i in 1..n {
                cum[i] = cum[i - 1] + (pts[i].0 - pts[i - 1].0).hypot(pts[i].1 - pts[i - 1].1);
            }
            let (mut cur, mut lambda) = (0usize, 0.0);
            let mut piece: Vec<Pt> = Vec::new();
            let mut pieces = Vec::new();
            for _ in 0..segments {
                lambda += interval;
                while cur < n && lambda > cum[cur] {
                    piece.push(pts[cur]);
                    cur += 1;
                }
                if cur >= n {
                    pieces.push(std::mem::take(&mut piece));
                    break;
                }
                let c = (lambda - cum[cur - 1]) / (cum[cur] - cum[cur - 1]);
                let p = (
                    pts[cur - 1].0 + c * (pts[cur].0 - pts[cur - 1].0),
                    pts[cur - 1].1 + c * (pts[cur].1 - pts[cur - 1].1),
                );
                piece.push(p);
                pieces.push(std::mem::replace(&mut piece, vec![p]));
            }
            let total = pieces.len();
            for coords in pieces {
                let mut p = Part::new(part.layer, part.feature, Shape::Line(coords));
                p.total_repeats = total;
                out.push(p);
            }
        }
        self.parts = out;
    }
}

/// qgsDoubleNear with its default (absolute) epsilon.
fn same(a: Pt, b: Pt) -> bool {
    (a.0 - b.0).abs() <= 4.0 * f64::EPSILON && (a.1 - b.1).abs() <= 4.0 * f64::EPSILON
}

/// FeaturePart::isConnected: exactly one end of `b` touches an end of `a`, and b's other end
/// is not on `a`.
fn is_connected(a: &Part, b: &Part) -> bool {
    let (Some(&ax0), Some(&ax1), Some(&bx0), Some(&bx1)) =
        (a.x.first(), a.x.last(), b.x.first(), b.x.last())
    else {
        return false;
    };
    let (a0, a1) = ((ax0, a.y[0]), (ax1, *a.y.last().unwrap()));
    let (b0, b1) = ((bx0, b.y[0]), (bx1, *b.y.last().unwrap()));
    let start_touches = same(a0, b0) || same(a1, b0);
    let end_touches = same(a0, b1) || same(a1, b1);
    if start_touches == end_touches {
        return false;
    }
    let other = if start_touches { b1 } else { b0 };
    // GEOSPreparedIntersectsXY: the point lies exactly on the line.
    !a.x.windows(2).zip(a.y.windows(2)).any(|(xs, ys)| {
        let (p, q) = ((xs[0], ys[0]), (xs[1], ys[1]));
        (q.0 - p.0) * (other.1 - p.1) - (q.1 - p.1) * (other.0 - p.0) == 0.0
            && other.0 >= p.0.min(q.0)
            && other.0 <= p.0.max(q.0)
            && other.1 >= p.1.min(q.1)
            && other.1 <= p.1.max(q.1)
    })
}

/// GEOSLineMerge of two lines sharing one endpoint. GEOS's LineMerger starts at the
/// lexicographically smallest end node (x, then y) and walks to the other end; EdgeString then
/// reverses the result when more of its edges were walked against their direction.
fn line_merge(a: &[Pt], b: &[Pt]) -> Option<Vec<Pt>> {
    let (a0, a1, b0, b1) = (a[0], *a.last()?, b[0], *b.last()?);
    // The merged line with both inputs forward when possible, and how many run reversed.
    let (mut m, reversed): (Vec<Pt>, [bool; 2]) = if same(a1, b0) {
        (a.iter().chain(&b[1..]).copied().collect(), [false, false])
    } else if same(a1, b1) {
        (
            a.iter()
                .copied()
                .chain(b.iter().rev().skip(1).copied())
                .collect(),
            [false, true],
        )
    } else if same(a0, b1) {
        (b.iter().chain(&a[1..]).copied().collect(), [false, false])
    } else if same(a0, b0) {
        (
            b.iter().rev().chain(&a[1..]).copied().collect(),
            [true, false],
        )
    } else {
        return None;
    };
    // Walk from the smallest end: walking backwards flips each edge's direction.
    let (f, l) = (m[0], *m.last()?);
    let from_end = (l.0, l.1) < (f.0, f.1);
    let backwards = reversed.iter().filter(|&&r| r != from_end).count();
    if backwards > 1 {
        // More edges against the walk: GEOS reverses the walk, i.e. ends at the smallest node.
        if !from_end {
            m.reverse();
        }
    } else if from_end {
        m.reverse();
    }
    Some(m)
}

fn lf_label_every_part(_lf: &LabelFeature) -> bool {
    false
}

/// Candidates of one feature in the problem (pal::Feats).
struct Feats {
    layer: usize,
    part: usize,
    candidates: Vec<LabelPosition>,
    priority: f64,
}

fn quadrant_from_offset(q: (f64, f64)) -> Quadrant {
    match (q.0.partial_cmp(&0.0), q.1.partial_cmp(&0.0)) {
        (Some(std::cmp::Ordering::Less), Some(std::cmp::Ordering::Less)) => Quadrant::AboveLeft,
        (Some(std::cmp::Ordering::Less), Some(std::cmp::Ordering::Greater)) => Quadrant::BelowLeft,
        (Some(std::cmp::Ordering::Less), _) => Quadrant::Left,
        (Some(std::cmp::Ordering::Greater), Some(std::cmp::Ordering::Less)) => Quadrant::AboveRight,
        (Some(std::cmp::Ordering::Greater), Some(std::cmp::Ordering::Greater)) => {
            Quadrant::BelowRight
        }
        (Some(std::cmp::Ordering::Greater), _) => Quadrant::Right,
        (_, Some(std::cmp::Ordering::Less)) => Quadrant::Above,
        (_, Some(std::cmp::Ordering::Greater)) => Quadrant::Below,
        _ => Quadrant::Over,
    }
}

/// QTransform::rotate(degrees) applied to (x, y).
fn qt_rotate(angle_rad: f64, x: f64, y: f64) -> (f64, f64) {
    let deg = angle_rad * 180.0 / PI;
    if deg == 0.0 {
        return (x, y);
    }
    // QTransform::rotate special-cases multiples of 90 degrees.
    let (s, c) = if deg == 90.0 || deg == -270.0 {
        (1.0, 0.0)
    } else if deg == 270.0 || deg == -90.0 {
        (-1.0, 0.0)
    } else if deg == 180.0 || deg == -180.0 {
        (0.0, -1.0)
    } else {
        let a = deg.to_radians();
        (a.sin(), a.cos())
    };
    (x * c - y * s, x * s + y * c)
}

struct CandidateContext<'a> {
    layer: &'a PalLayer,
    lf: &'a LabelFeature,
    part: &'a Part,
    upright_only: bool,
    /// Pal::maximumLineCandidatesPerMapUnit.
    line_per_unit: f64,
}

impl CandidateContext<'_> {
    fn width(&self) -> f64 {
        self.lf.size.0
    }
    fn height(&self) -> f64 {
        self.lf.size.1
    }

    fn pos(
        &self,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        alpha: f64,
        cost: f64,
        q: Quadrant,
    ) -> LabelPosition {
        LabelPosition::new(self.part, x, y, w, h, alpha, cost, q, self.upright_only)
    }

    /// FeaturePart::createCandidateCenteredOverPoint.
    fn centered_over_point(&self, x: f64, y: f64, out: &mut Vec<LabelPosition>, angle: f64) {
        let (w, h) = (self.width(), self.height());
        out.push(self.pos(
            x - w / 2.0,
            y - h / 2.0,
            w,
            h,
            angle,
            0.00005,
            Quadrant::Over,
        ));
    }

    /// FeaturePart::createCandidatesOverPoint.
    fn over_point(&self, x: f64, y: f64, out: &mut Vec<LabelPosition>, angle: f64) {
        let lf = self.lf;
        let (w, h) = (self.width(), self.height());
        let mut xdiff = -w / 2.0;
        let mut ydiff = -h / 2.0;
        if !near(lf.quad_offset.0, 0.0) {
            xdiff += w / 2.0 * lf.quad_offset.0;
        }
        if !near(lf.quad_offset.1, 0.0) {
            ydiff += h / 2.0 * lf.quad_offset.1;
        }
        if lf.fixed_position.is_none() && !near(angle, 0.0) {
            let xd = xdiff * angle.cos() - ydiff * angle.sin();
            let yd = xdiff * angle.sin() + ydiff * angle.cos();
            xdiff = xd;
            ydiff = yd;
        }
        if self.layer.arrangement == Placement::AroundPoint {
            if near(lf.quad_offset.0, 0.0) {
                ydiff += lf.quad_offset.1 * lf.dist_label;
            } else if near(lf.quad_offset.1, 0.0) {
                xdiff += lf.quad_offset.0 * lf.dist_label;
            } else {
                xdiff += lf.quad_offset.0 * FRAC_1_SQRT_2 * lf.dist_label;
                ydiff += lf.quad_offset.1 * FRAC_1_SQRT_2 * lf.dist_label;
            }
        } else {
            if !near(lf.position_offset.0, 0.0) {
                xdiff += lf.position_offset.0;
            }
            if !near(lf.position_offset.1, 0.0) {
                ydiff += lf.position_offset.1;
            }
        }
        out.push(self.pos(
            x + xdiff,
            y + ydiff,
            w,
            h,
            angle,
            0.0001,
            quadrant_from_offset(lf.quad_offset),
        ));
    }

    /// FeaturePart::createCandidatesAtOrderedPositionsOverPoint.
    fn ordered_positions(
        &self,
        x: f64,
        y: f64,
        out: &mut Vec<LabelPosition>,
        angle: f64,
        max_point: usize,
    ) {
        let lf = self.lf;
        let (w, h) = (self.width(), self.height());
        let dist = lf.dist_label;
        let (sw, sh) = if lf.offset_from_symbol {
            (lf.symbol_size.0 / 2.0, lf.symbol_size.1 / 2.0)
        } else {
            (0.0, 0.0)
        };
        let mut per_position = 1usize;
        let mut step = 0.0;
        if lf.max_distance > dist && !near(lf.max_distance, 0.0) {
            let ray = lf.max_distance - dist;
            per_position = 2.max((self.line_per_unit * 1.5 * ray).ceil() as usize);
            step = ray / (per_position - 1) as f64;
        }
        let mut cost = 0.0001;
        let max_candidates = max_point * per_position;
        let mut created = 0;
        let mut add = |pos: PointPosition, distance: f64, out: &mut Vec<LabelPosition>| -> bool {
            let (lx, ly, quadrant) =
                ordered_position(x, y, w, h, pos, distance, lf.visual_margin, sw, sh, angle);
            out.push(self.pos(lx, ly, w, h, angle, cost, quadrant));
            created += 1;
            cost += 0.001;
            !(max_candidates > 0 && created >= max_candidates)
        };
        if lf.prefer_closer {
            let mut d = dist;
            for _ in 0..per_position {
                for p in &lf.position_order {
                    if !add(*p, d, out) {
                        return;
                    }
                }
                d += step;
            }
        } else {
            for p in &lf.position_order {
                let mut d = dist;
                for _ in 0..per_position {
                    if !add(*p, d, out) {
                        return;
                    }
                    d += step;
                }
            }
        }
    }

    /// FeaturePart::createCandidatesAroundPoint.
    fn around_point(
        &self,
        x: f64,
        y: f64,
        out: &mut Vec<LabelPosition>,
        angle: f64,
        max_point: usize,
    ) {
        let lf = self.lf;
        let (w, h) = (self.width(), self.height());
        let dist = lf.dist_label;
        let ray_count = if max_point == 0 { 16 } else { max_point as i32 };
        let (candidates_per_ray, ray_step) =
            if lf.max_distance > dist && !near(lf.max_distance, 0.0) {
                let ray = lf.max_distance - dist;
                let n = 2.max((self.line_per_unit * 1.5 * ray).ceil() as usize);
                (n, ray / (n - 1) as f64)
            } else {
                (1, 0.0)
            };
        let increment = 2.0 * PI / ray_count as f64;
        let a90 = FRAC_PI_2;
        let a180 = PI;
        let a270 = a180 + a90;
        let a360 = 2.0 * PI;
        let (mut gamma1, mut gamma2) = if dist > 0.0 {
            (
                (h / 2.0).atan2(dist + w / 2.0),
                (w / 2.0).atan2(dist + h / 2.0),
            )
        } else {
            (a90 / 3.0, a90 / 3.0)
        };
        if gamma1 > a90 / 3.0 {
            gamma1 = a90 / 3.0;
        }
        if gamma2 > a90 / 3.0 {
            gamma2 = a90 / 3.0;
        }
        let mut angle_to = FRAC_PI_4;
        let mut ray_cost = 0i32;
        let mut ray_cost_inc = 2i32;
        for _ in 0..ray_count {
            let (mut dx, mut dy) = (0.0, 0.0);
            if angle_to > a360 {
                angle_to -= a360;
            }
            let mut ray_dist = dist;
            let scaled = 0.0020 * ray_cost as f64 / (ray_count - 1) as f64;
            for j in 0..candidates_per_ray {
                let mut quadrant = Quadrant::Over;
                if angle_to < gamma1 || angle_to > a360 - gamma1 {
                    dx = ray_dist;
                    let mut iota = angle_to + gamma1;
                    if iota > a360 - gamma1 {
                        iota -= a360;
                    }
                    dy = -h + h * iota / (2.0 * gamma1);
                    quadrant = Quadrant::Right;
                } else if angle_to < a90 - gamma2 {
                    dx = ray_dist * angle_to.cos();
                    dy = ray_dist * angle_to.sin();
                    quadrant = Quadrant::AboveRight;
                } else if angle_to < a90 + gamma2 {
                    dx = -w * (angle_to - a90 + gamma2) / (2.0 * gamma2);
                    dy = ray_dist;
                    quadrant = Quadrant::Above;
                } else if angle_to < a180 - gamma1 {
                    dx = ray_dist * angle_to.cos() - w;
                    dy = ray_dist * angle_to.sin();
                    quadrant = Quadrant::AboveLeft;
                } else if angle_to < a180 + gamma1 {
                    dx = -ray_dist - w;
                    dy = -(angle_to - a180 + gamma1) * h / (2.0 * gamma1);
                    quadrant = Quadrant::Left;
                } else if angle_to < a270 - gamma2 {
                    dx = ray_dist * angle_to.cos() - w;
                    dy = ray_dist * angle_to.sin() - h;
                    quadrant = Quadrant::BelowLeft;
                } else if angle_to < a270 + gamma2 {
                    dy = -ray_dist - h;
                    dx = -w + (angle_to - a270 + gamma2) * w / (2.0 * gamma2);
                    quadrant = Quadrant::Below;
                } else if angle_to < a360 {
                    dx = ray_dist * angle_to.cos();
                    dy = ray_dist * angle_to.sin() - h;
                    quadrant = Quadrant::BelowRight;
                }
                let (rx, ry) = qt_rotate(angle, dx, dy);
                let mut cost = if ray_count == 1 {
                    0.0001
                } else {
                    0.0001 + scaled
                };
                if j > 0 {
                    cost += j as f64 * 0.0020 + 0.0020 / ray_count as f64;
                }
                out.push(self.pos(x + rx, y + ry, w, h, angle, cost, quadrant));
                ray_dist += ray_step;
            }
            ray_cost += ray_cost_inc;
            if ray_cost == ray_count {
                ray_cost = ray_count - 1;
                ray_cost_inc = -2;
            } else if ray_cost > ray_count {
                ray_cost = ray_count - 2;
                ray_cost_inc = -2;
            }
            angle_to += increment;
        }
    }
}

/// FeaturePart::createCurvedCandidatesAlongLine's offsets (PathOffset).
#[derive(Debug, Clone, Copy, PartialEq)]
enum PathOffset {
    Positive,
    None,
    Negative,
}

/// PointSet::getPointByDistance over (segment lengths, distances to segment starts).
fn point_by_distance(x: &[f64], y: &[f64], seg: &[f64], start: &[f64], dl: f64) -> Pt {
    let n = x.len();
    let mut i = 0usize;
    if dl >= 0.0 {
        while i < n && start[i] <= dl {
            i += 1;
        }
        i = i.saturating_sub(1);
    }
    if i < n - 1 {
        let (dx, dy, di) = if dl < 0.0 {
            let (dx, dy) = (x[n - 1] - x[0], y[n - 1] - y[0]);
            (dx, dy, dx.hypot(dy))
        } else {
            (x[i + 1] - x[i], y[i + 1] - y[i], seg[i])
        };
        let r = dl - start[i];
        (x[i] + dx * r / di, y[i] + dy * r / di)
    } else {
        (x[i], y[i])
    }
}

/// Segment lengths and distances to each vertex, as the line candidate generators compute them.
fn segment_distances(x: &[f64], y: &[f64]) -> (Vec<f64>, Vec<f64>, f64) {
    let n = x.len();
    let mut seg = vec![0.0; n - 1];
    let mut start = vec![0.0; n];
    let mut total = 0.0;
    for i in 0..n - 1 {
        start[i] = if i == 0 {
            0.0
        } else {
            start[i - 1] + seg[i - 1]
        };
        seg[i] = (x[i + 1] - x[i]).hypot(y[i + 1] - y[i]);
        total += seg[i];
    }
    start[n - 1] = total;
    (seg, start, total)
}

impl CandidateContext<'_> {
    /// FeaturePart::createCandidatesAlongLine (the near-straight-segments pass for hint-only
    /// anchors is not ported; those fall back to the midpoint candidates).
    fn along_line(
        &self,
        out: &mut Vec<LabelPosition>,
        allow_overrun: bool,
        max_line_candidates: usize,
    ) {
        let lf = self.lf;
        let Some(ll) = &lf.line else { return };
        let w = lf.size.0;
        if allow_overrun {
            let len = self.part.length();
            if (self.part.total_repeats > 1 && len < w) || len < w - 2.0 * w.min(ll.overrun) {
                return;
            }
        }
        self.along_line_near_midpoint(out, 0.0, max_line_candidates);
    }

    /// FeaturePart::createCandidatesAlongLineNearMidpoint.
    fn along_line_near_midpoint(
        &self,
        out: &mut Vec<LabelPosition>,
        initial_cost: f64,
        max_line_candidates: usize,
    ) {
        let lf = self.lf;
        let Some(ll) = &lf.line else { return };
        let (x, y) = (&self.part.x, &self.part.y);
        let n = x.len();
        let (w, h) = lf.size;
        let dist = lf.dist_label;
        let mut flags = ll.flags;
        if flags == 0 {
            flags = ON_LINE;
        }
        let (seg, start, mut total) = segment_distances(x, y);
        let mut step = total - w;
        let mut along = 0.0;
        let closed = n > 1 && x[0] == x[n - 1] && y[0] == y[n - 1];
        if total > w {
            step = h.min(w).min(step / max_line_candidates.max(1) as f64);
        } else if !closed {
            along = -(w - total) / 2.0;
            step = -1.0;
            total = w;
        } else {
            along = f64::MAX;
        }
        let anchor = total * ll.anchor_percent.min(0.99);
        if ll.anchor_strict {
            along = match ll.text_point {
                TextAnchor::Start | TextAnchor::Follow => anchor.min(total * 0.99 - w),
                TextAnchor::Center => (anchor - w / 2.0).min(total * 0.99 - w),
                TextAnchor::End => (anchor - w).min(total * 0.99 - w),
            };
            step = -1.0;
        }
        while along <= total - w || ll.anchor_strict {
            let (sx, sy) = point_by_distance(x, y, &seg, &start, along);
            let (ex, ey) = point_by_distance(x, y, &seg, &start, along + w);
            let len = if along < 0.0 {
                (x[n - 1] - x[0]).hypot(y[n - 1] - y[0])
            } else {
                (ex - sx).hypot(ey - sy)
            };
            let mut cost = len / w;
            cost = if cost > 0.98 {
                0.0001
            } else {
                (1.0 - cost) / 100.0
            };
            let text_anchor = match ll.text_point {
                TextAnchor::Start | TextAnchor::Follow => along,
                TextAnchor::Center => along + w / 2.0,
                TextAnchor::End => along + w,
            };
            cost += if total > 0.0 {
                (anchor - text_anchor).abs() / total
            } else {
                0.0
            } / 1000.0;
            cost += initial_cost;
            let angle =
                if near_eps(ey, sy, 4.0 * f64::EPSILON) && near_eps(ex, sx, 4.0 * f64::EPSILON) {
                    0.0
                } else {
                    (ey - sy).atan2(ex - sx)
                };
            let beta = angle + FRAC_PI_2;
            if self.layer.arrangement == Placement::Line {
                let rtl = angle > FRAC_PI_2 || angle <= -FRAC_PI_2;
                let reversed = flags & MAP_ORIENTATION != 0 && rtl;
                let above =
                    (!reversed && flags & ABOVE_LINE != 0) || (reversed && flags & BELOW_LINE != 0);
                let below =
                    (!reversed && flags & BELOW_LINE != 0) || (reversed && flags & ABOVE_LINE != 0);
                if above {
                    let c = cost + if reversed { 0.001 } else { 0.0 };
                    out.push(self.pos(
                        sx + beta.cos() * dist,
                        sy + beta.sin() * dist,
                        w,
                        h,
                        angle,
                        c,
                        Quadrant::Over,
                    ));
                }
                if below {
                    let c = cost + if reversed { 0.0 } else { 0.001 };
                    out.push(self.pos(
                        sx - beta.cos() * (dist + h),
                        sy - beta.sin() * (dist + h),
                        w,
                        h,
                        angle,
                        c,
                        Quadrant::Over,
                    ));
                }
                if flags & ON_LINE != 0 {
                    out.push(self.pos(
                        sx - h * beta.cos() / 2.0,
                        sy - h * beta.sin() / 2.0,
                        w,
                        h,
                        angle,
                        cost + 0.002,
                        Quadrant::Over,
                    ));
                }
            }
            along += step;
            if step < 0.0 {
                break;
            }
        }
    }

    /// FeaturePart::createHorizontalCandidatesAlongLine.
    fn horizontal_along_line(
        &self,
        out: &mut Vec<LabelPosition>,
        angle: f64,
        max_line_candidates: usize,
    ) {
        let lf = self.lf;
        let Some(ll) = &lf.line else { return };
        let (x, y) = (&self.part.x, &self.part.y);
        let (w, h) = lf.size;
        let (seg, start, total) = segment_distances(x, y);
        let anchor = total * ll.anchor_percent;
        let (mut along, step) = if ll.anchor_strict {
            (anchor, -1.0)
        } else {
            (0.0, total / (max_line_candidates + 1) as f64)
        };
        let (c, s) = (angle.cos(), angle.sin());
        let (hx, hy) = (h / 2.0 * s, h / 2.0 * c);
        while along <= total {
            let (cx, cy) = point_by_distance(x, y, &seg, &start, along);
            let cost = if total > 0.0 {
                (anchor - along).abs() / total
            } else {
                0.0
            } / 1000.0;
            let (lx, ly) = match ll.text_point {
                TextAnchor::Start | TextAnchor::Follow => (cx + hx, cy - hy),
                TextAnchor::Center => (cx - w / 2.0 * c + hx, cy - w / 2.0 * s - hy),
                TextAnchor::End => (cx - w * c + hx, cy - w * s - hy),
            };
            out.push(self.pos(lx, ly, w, h, angle, cost, Quadrant::Over));
            along += step;
            if step < 0.0 {
                break;
            }
        }
    }
}

/// std::clamp as libstdc++ evaluates it (also when hi < lo).
fn std_clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo
    } else if hi < v {
        hi
    } else {
        v
    }
}

impl CandidateContext<'_> {
    /// FeaturePart::createDefaultCurvedCandidatesAlongLine (default curved mode: no stretch to
    /// fit, no permissible zone, labels not repeated).
    fn curved_along_line(
        &self,
        out: &mut Vec<LabelPosition>,
        allow_overrun: bool,
        max_line_candidates: usize,
    ) {
        let lf = self.lf;
        let Some(ll) = &lf.line else { return };
        let Some(m) = &ll.metrics else { return };
        let count = m.count();
        if count == 0 {
            return;
        }
        let total_char_width: f64 = m.widths.iter().sum();
        let mut shape: Vec<Pt> = self
            .part
            .x
            .iter()
            .copied()
            .zip(self.part.y.iter().copied())
            .collect();
        let mut shape_len = line::length(&shape);
        // Overrun never exceeds the label (hint) or always fits it (strict).
        let overrun = if ll.anchor_strict {
            ll.overrun.max(total_char_width * 1.05)
        } else {
            ll.overrun.min(total_char_width * 0.95)
        };
        if total_char_width > shape_len
            && (!allow_overrun || shape_len < total_char_width - 2.0 * overrun)
        {
            return;
        }
        // Anchor on the original line, before the overrun extension.
        let original_point = line::interpolate_point(&shape, shape_len * ll.anchor_percent);
        if allow_overrun && overrun > 0.0 {
            line::extend_line_by_distance(&mut shape, overrun, overrun, ll.overrun_smooth);
            shape_len += 2.0 * overrun;
        }
        let _ = shape_len;

        let mut flags = ll.flags;
        if flags == 0 {
            flags = ON_LINE;
        }
        let above_below = flags & (ABOVE_LINE | BELOW_LINE) != 0;
        let offset_distance = lf.dist_label + m.heights[0] / 2.0;
        let (mut positive, mut negative): (Option<Vec<Pt>>, Option<Vec<Pt>>) = (None, None);
        let (mut positive_has_negative, mut negative_has_negative) = (false, false);
        if above_below && !near(offset_distance, 0.0) {
            let want_positive = flags & MAP_ORIENTATION != 0 || flags & ABOVE_LINE != 0;
            let want_negative = flags & MAP_ORIENTATION != 0 || flags & BELOW_LINE != 0;
            let (pd, nd) = if offset_distance >= 0.0 || flags & MAP_ORIENTATION == 0 {
                positive_has_negative = offset_distance < 0.0;
                negative_has_negative = offset_distance > 0.0;
                (offset_distance, -offset_distance)
            } else {
                // Negative distances swap above and below.
                if flags & ABOVE_LINE != 0 && flags & BELOW_LINE == 0 {
                    flags = (flags & !ABOVE_LINE) | BELOW_LINE;
                } else if flags & BELOW_LINE != 0 && flags & ABOVE_LINE == 0 {
                    flags = (flags & !BELOW_LINE) | ABOVE_LINE;
                }
                positive_has_negative = offset_distance > 0.0;
                negative_has_negative = offset_distance < 0.0;
                (-offset_distance, offset_distance)
            };
            if want_positive {
                positive = Some(line::offset_curve(&shape, pd));
            }
            if want_negative {
                negative = Some(line::offset_curve(&shape, nd));
            }
        }

        let label_width = lf.size.0;
        let mut positions = Vec::new();
        let mut backup: Option<LabelPosition> = None;
        for offset in [PathOffset::Positive, PathOffset::None, PathOffset::Negative] {
            let current: Option<&[Pt]> = match offset {
                PathOffset::Positive if above_below => positive.as_deref(),
                PathOffset::None if flags & ON_LINE != 0 => Some(&shape),
                PathOffset::Negative if above_below => negative.as_deref(),
                _ => None,
            };
            let Some(current) = current else { continue };
            let (dists, total) = line::edge_distances(current);
            if near(total, 0.0) {
                continue;
            }
            let line_anchor = match original_point {
                Some(p) => line::line_locate_point(current, p),
                None => {
                    let a = total * ll.anchor_percent;
                    if offset == PathOffset::Negative {
                        total - a
                    } else {
                        a
                    }
                }
            };
            let mut delta = (m.heights[0] / 6.0).max(total / max_line_candidates.max(1) as f64);
            let mut start = 0.0;
            let mut single = false;
            if ll.anchor_strict {
                start = match ll.text_point {
                    TextAnchor::Start => std_clamp(line_anchor, 0.0, total * 0.999),
                    TextAnchor::Center => std_clamp(
                        line_anchor - label_width / 2.0,
                        0.0,
                        total * 0.999 - label_width / 2.0,
                    ),
                    TextAnchor::End => {
                        std_clamp(line_anchor - label_width, 0.0, total * 0.999 - label_width)
                    }
                    TextAnchor::Follow => 0.0,
                };
                single = true;
                delta = delta.max(f64::MIN_POSITIVE);
            }
            let mut tested = false;
            let mut along = start;
            while along <= total {
                if single && tested {
                    break;
                }
                tested = true;
                let here = along;
                along += delta;
                let direction = if flags & MAP_ORIENTATION != 0 {
                    Direction::RespectPainterOrientation
                } else {
                    Direction::FollowLine
                };
                let upright = self.upright_only && (!single || flags & MAP_ORIENTATION == 0);
                let (max_in, max_out) = if single {
                    (-1.0, -1.0)
                } else {
                    (ll.max_angle_inside.abs(), ll.max_angle_outside.abs())
                };
                let placement = line::curved_text_placement(
                    m, current, &dists, here, direction, max_in, max_out, upright, false,
                );
                tracing::trace!(target: "rgis::label_cands", fid = lf.fid, ?offset, here, total, delta, placed = placement.graphemes.len(), flipped = placement.flipped, "curved candidate");
                let mut rtl = if upright {
                    placement.flipped
                } else {
                    placement.right_to_left
                };
                let Some(first) = placement.graphemes.first() else {
                    continue;
                };
                // FeaturePart::curvedPlacementAtOffset: one position per grapheme.
                let mk = |g: &line::GraphemePlacement| {
                    let mut p = LabelPosition::new(
                        self.part,
                        g.x,
                        g.y,
                        g.width,
                        g.height,
                        g.angle,
                        0.0001,
                        Quadrant::Over,
                        false,
                    );
                    p.part_id = g.index;
                    p
                };
                let mut lp = mk(first);
                for g in &placement.graphemes[1..] {
                    if ll.ignore_whitespace && g.is_whitespace {
                        continue;
                    }
                    lp.push_part(mk(g));
                }

                let mut backup_only = false;
                if flags & MAP_ORIENTATION != 0 {
                    if (offset == PathOffset::Positive && positive_has_negative)
                        || (offset == PathOffset::Negative && negative_has_negative)
                    {
                        rtl = !rtl;
                    }
                    if offset != PathOffset::None && !rtl && flags & ABOVE_LINE == 0 {
                        if single && offset == PathOffset::Positive {
                            backup_only = true;
                        } else {
                            continue;
                        }
                    }
                    if offset != PathOffset::None && rtl && flags & BELOW_LINE == 0 {
                        if single && offset == PathOffset::Positive {
                            backup_only = true;
                        } else {
                            continue;
                        }
                    }
                }
                backup = None;

                let angle_diff = lp.angle_differential();
                let angle_diff_avg = if count > 1 {
                    angle_diff / (count - 1) as f64
                } else {
                    0.0
                };
                let flexible = !single && ll.anchor_percent > 0.1 && ll.anchor_percent < 0.9;
                let mut cost = (angle_diff_avg / 100.0).max(0.0001);
                let text_anchor = match ll.text_point {
                    TextAnchor::Start | TextAnchor::Follow => here,
                    TextAnchor::Center => here + label_width / 2.0,
                    TextAnchor::End => here + label_width,
                };
                let cost_center = (line_anchor - text_anchor).abs() / total;
                cost += cost_center / if flexible { 100.0 } else { 10.0 };
                if offset != PathOffset::None && rtl {
                    cost += 0.001;
                } else if offset == PathOffset::None {
                    cost += 0.002;
                }
                lp.cost = cost;
                if backup_only {
                    backup = Some(lp);
                } else {
                    positions.push(lp);
                }
            }
        }
        out.extend(positions);
        if let Some(b) = backup {
            out.push(b);
        }
    }
}

/// createCandidateAtOrderedPositionOverPoint.
#[allow(clippy::too_many_arguments)]
fn ordered_position(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    position: PointPosition,
    mut distance: f64,
    m: (f64, f64, f64, f64),
    sw: f64,
    sh: f64,
    angle: f64,
) -> (f64, f64, Quadrant) {
    use PointPosition::*;
    let (left, top, right, bottom) = m;
    let (quadrant, alpha, dx, dy) = match position {
        TopLeft => (
            Quadrant::AboveLeft,
            3.0 * FRAC_PI_4,
            -w + right - sw,
            -bottom + sh,
        ),
        TopSlightlyLeft => (
            Quadrant::AboveRight,
            FRAC_PI_2,
            -w / 4.0 - left,
            -bottom + sh,
        ),
        TopMiddle => (Quadrant::Above, FRAC_PI_2, -w / 2.0, -bottom + sh),
        TopSlightlyRight => (
            Quadrant::AboveLeft,
            FRAC_PI_2,
            -w * 3.0 / 4.0 + right,
            -bottom + sh,
        ),
        TopRight => (Quadrant::AboveRight, FRAC_PI_4, -left + sw, -bottom + sh),
        MiddleLeft => (Quadrant::Left, PI, -w + right - sw, -h / 2.0),
        MiddleRight => (Quadrant::Right, 0.0, -left + sw, -h / 2.0),
        BottomLeft => (
            Quadrant::BelowLeft,
            5.0 * FRAC_PI_4,
            -w + right - sw,
            -h + top - sh,
        ),
        BottomSlightlyLeft => (
            Quadrant::BelowRight,
            3.0 * FRAC_PI_2,
            -w / 4.0 - left,
            -h + top - sh,
        ),
        BottomMiddle => (Quadrant::Below, 3.0 * FRAC_PI_2, -w / 2.0, -h + top - sh),
        BottomSlightlyRight => (
            Quadrant::BelowLeft,
            3.0 * FRAC_PI_2,
            -w * 3.0 / 4.0 + right,
            -h + top - sh,
        ),
        BottomRight => (
            Quadrant::BelowRight,
            7.0 * FRAC_PI_4,
            -left + sw,
            -h + top - sh,
        ),
        OverPoint => {
            distance = 0.0;
            (Quadrant::Over, 0.0, -w / 2.0, -h / 2.0)
        }
    };
    let (dx, dy) = qt_rotate(angle, dx, dy);
    let rx = alpha.cos() * distance + x;
    let ry = alpha.sin() * distance + y;
    (rx + dx, ry + dy, quadrant)
}

/// PriorityQueue (pal/priorityqueue.cpp), min-heap on overlap counts.
struct PriorityQueue {
    heap: Vec<usize>,
    p: Vec<f64>,
    pos: Vec<i64>,
    size: usize,
}

impl PriorityQueue {
    fn new(n: usize, max_id: usize) -> PriorityQueue {
        PriorityQueue {
            heap: vec![0; n],
            p: vec![0.0; n],
            pos: vec![-1; max_id + 1],
            size: 0,
        }
    }
    fn greater(l: f64, r: f64) -> bool {
        l > r
    }
    fn is_in(&self, key: usize) -> bool {
        key < self.pos.len() && self.pos[key] >= 0
    }
    fn insert(&mut self, key: usize, p: f64) {
        if self.size == self.heap.len() || key >= self.pos.len() {
            return;
        }
        self.heap[self.size] = key;
        self.pos[key] = self.size as i64;
        self.p[self.size] = p;
        self.size += 1;
        self.upheap(key);
    }
    fn get_best(&mut self) -> usize {
        let ret = self.heap[0];
        self.size -= 1;
        self.pos[ret] = -1;
        if self.size > 0 {
            self.pos[self.heap[self.size]] = 0;
            self.heap[0] = self.heap[self.size];
            self.p[0] = self.p[self.size];
            self.downheap(0);
        }
        ret
    }
    fn remove(&mut self, key: usize) {
        if key >= self.pos.len() {
            return;
        }
        let i = self.pos[key];
        if i >= 0 {
            let i = i as usize;
            self.size -= 1;
            self.pos[self.heap[self.size]] = i as i64;
            self.pos[key] = -1;
            self.heap[i] = self.heap[self.size];
            self.p[i] = self.p[self.size];
            self.downheap(i);
        }
    }
    fn upheap(&mut self, key: usize) {
        let mut i = self.pos[key];
        if i < -1 {
            return;
        }
        while i > 0 {
            let iu = i as usize;
            let parent = (iu - 1) / 2;
            if Self::greater(self.p[parent], self.p[iu]) {
                self.pos[self.heap[iu]] = parent as i64;
                self.pos[self.heap[parent]] = iu as i64;
                self.heap.swap(iu, parent);
                self.p.swap(iu, parent);
                i = parent as i64;
            } else {
                break;
            }
        }
    }
    fn downheap(&mut self, mut id: usize) {
        loop {
            let left = 2 * id + 1;
            let right = 2 * id + 2;
            let min_child = if left < self.size {
                if right < self.size {
                    if Self::greater(self.p[right], self.p[left]) {
                        left
                    } else {
                        right
                    }
                } else {
                    left
                }
            } else {
                break;
            };
            if Self::greater(self.p[id], self.p[min_child]) {
                self.pos[self.heap[id]] = min_child as i64;
                self.pos[self.heap[min_child]] = id as i64;
                self.heap.swap(id, min_child);
                self.p.swap(id, min_child);
                id = min_child;
            } else {
                break;
            }
        }
    }
    fn decrease_key(&mut self, key: usize) {
        if key >= self.pos.len() {
            return;
        }
        let i = self.pos[key];
        if i < 0 {
            return;
        }
        self.p[i as usize] -= 1.0;
        self.upheap(key);
        let p = self.pos[key] as usize;
        self.downheap(p);
    }
}

struct Chain {
    feat: Vec<usize>,
    label: Vec<i64>,
    delta: f64,
}

/// pal::Problem.
struct Problem {
    positions: Vec<LabelPosition>,
    first: Vec<usize>,
    count: Vec<usize>,
    unlabeled_cost: Vec<f64>,
    total: usize,
    all_index: PalRtree,
    active_index: PalRtree,
    sol: Vec<i64>,
    conflict_cache: HashMap<(usize, usize), bool>,
}

fn grid_extent(b: &BBox) -> BBox {
    let m = (b.xmax - b.xmin).max(b.ymax - b.ymin) * 0.5;
    BBox {
        xmin: b.xmin - m,
        ymin: b.ymin - m,
        xmax: b.xmax + m,
        ymax: b.ymax + m,
    }
}

impl Problem {
    /// Pal::candidatesAreConflicting / LabelPosition::isInConflict.
    fn conflicting(&mut self, layers: &[PalLayer], a: usize, b: usize) -> bool {
        let (la, lb) = (&self.positions[a], &self.positions[b]);
        if la.prob_feat == lb.prob_feat {
            return false;
        }
        let fa = &layers[la.layer].features[la.feature];
        let fb = &layers[lb.layer].features[lb.feature];
        let min_separation = fa.no_repeat_distance.max(fb.no_repeat_distance);
        let duplicate = min_separation > 0.0 && fa.label_text == fb.label_text;
        if !duplicate
            && (fa.overlap == OverlapHandling::AllowOverlapAtNoCost
                || fb.overlap == OverlapHandling::AllowOverlapAtNoCost)
        {
            return false;
        }
        let single = la.next.is_empty() && lb.next.is_empty();
        if single && near(la.alpha, 0.0) && near(lb.alpha, 0.0) {
            if la.bbox.intersects(&lb.bbox) {
                return true;
            }
            // Corner to corner, as LabelPosition::isInConflict does for axis-parallel labels.
            let m2 = min_separation * min_separation;
            return duplicate
                && la.quad().iter().any(|p| {
                    lb.quad()
                        .iter()
                        .any(|q| (p.0 - q.0).powi(2) + (p.1 - q.1).powi(2) < m2)
                });
        }
        if duplicate {
            // GEOSPreparedDistanceWithin over all parts.
            return la.all_quads().any(|(qa, _, _)| {
                lb.all_quads()
                    .any(|(qb, _, _)| shape::quad_distance(&qa, &qb) <= min_separation)
            });
        }
        let key = (
            la.global_id.min(lb.global_id),
            la.global_id.max(lb.global_id),
        );
        if let Some(r) = self.conflict_cache.get(&key) {
            return *r;
        }
        // LabelPosition::isInConflictMultiPart: any part against any part.
        let r = la.all_quads().any(|(qa, ba, _)| {
            lb.all_quads().any(|(qb, bb, _)| {
                ba.intersects(&bb) && shape::quad_intersects(&qb, &quad_ring(&qa))
            })
        });
        self.conflict_cache.insert(key, r);
        r
    }

    fn conflicts_of(&mut self, layers: &[PalLayer], lp: usize, active: bool) -> Vec<usize> {
        let b = self.positions[lp].cbox;
        let ids = if active {
            self.active_index.query(&b)
        } else {
            self.all_index.query(&b)
        };
        ids.into_iter()
            .filter(|&o| self.conflicting(layers, lp, o))
            .collect()
    }

    /// Problem::reduce.
    fn reduce(&mut self, layers: &[PalLayer]) {
        let mut ok = vec![false; self.positions.len()];
        loop {
            let mut finished = true;
            for f in 0..self.first.len() {
                let total = self.count[f];
                for ci in 0..total {
                    let idx = self.first[f] + ci;
                    if ok[idx] || self.positions[idx].num_overlaps != 0 {
                        continue;
                    }
                    finished = false;
                    ok[idx] = true;
                    for k in ci + 1..total {
                        let lpid = self.first[f] + k;
                        ok[lpid] = true;
                        let b = self.positions[lpid].cbox;
                        for o in self.all_index.query(&b) {
                            if self.conflicting(layers, lpid, o) {
                                self.positions[o].num_overlaps -= 1;
                                self.positions[lpid].num_overlaps -= 1;
                            }
                        }
                        self.all_index.remove(lpid, &self.positions[lpid].cbox);
                    }
                    self.count[f] = ci + 1;
                    break;
                }
            }
            if finished {
                break;
            }
        }
    }

    fn ignore_label(&mut self, layers: &[PalLayer], lp: usize, list: &mut PriorityQueue) {
        if list.is_in(lp) {
            list.remove(lp);
            let b = self.positions[lp].cbox;
            for o in self.all_index.query(&b) {
                if o != lp && list.is_in(o) && self.conflicting(layers, o, lp) {
                    list.decrease_key(o);
                }
            }
        }
    }

    /// Problem::init_sol_falp.
    fn init_sol_falp(&mut self, layers: &[PalLayer], display_all: bool) {
        let nf = self.first.len();
        self.sol = vec![-1; nf];
        let mut list = PriorityQueue::new(self.total, self.positions.len());
        for f in 0..nf {
            for ci in 0..self.count[f] {
                let label = self.first[f] + ci;
                list.insert(label, self.positions[label].num_overlaps as f64);
            }
        }
        while list.size > 0 {
            let label = list.get_best();
            let feat = self.positions[label].prob_feat;
            self.sol[feat] = label as i64;
            for ci in self.first[feat]..self.first[feat] + self.count[feat] {
                self.ignore_label(layers, ci, &mut list);
            }
            for c in self.conflicts_of(layers, label, false) {
                self.ignore_label(layers, c, &mut list);
            }
            let b = self.positions[label].cbox;
            self.active_index.insert(label, &b);
        }
        if display_all {
            for f in 0..nf {
                if self.sol[f] != -1 {
                    continue;
                }
                let mut best: Option<usize> = None;
                let mut nb = i32::MAX;
                for ci in 0..self.count[f] {
                    let lp = self.first[f] + ci;
                    let n = self.conflicts_of(layers, lp, true).len() as i32;
                    self.positions[lp].num_overlaps = n;
                    if n < nb {
                        best = Some(lp);
                        nb = n;
                    }
                }
                if let Some(b) = best {
                    self.sol[f] = b as i64;
                    let bb = self.positions[b].cbox;
                    self.active_index.insert(b, &bb);
                }
            }
        }
    }

    fn cost_of(&self, label: i64, feat: usize) -> f64 {
        if label < 0 {
            self.unlabeled_cost[feat]
        } else {
            self.positions[label as usize].cost
        }
    }

    /// Problem::chain.
    fn chain(&mut self, layers: &[PalLayer], mut seed: usize, max_degree: usize) -> Option<Chain> {
        let mut delta = 0.0;
        let mut delta_best = f64::MAX;
        let mut retained: Option<Chain> = None;
        // (feat, old_label, new_label)
        let mut current: Vec<(usize, i64, i64)> = Vec::new();
        let mut tmpsol = self.sol.clone();
        loop {
            let total = self.count[seed];
            let mut delta_min = f64::MAX;
            let mut next_seed: Option<usize> = None;
            let mut retained_label: i64 = -2;
            delta -= self.cost_of(tmpsol[seed], seed);
            for i in -1..total as i64 {
                if tmpsol[seed] == -1 && i == -1 {
                    continue;
                }
                if i >= 0 && (i as usize + self.first[seed]) as i64 == tmpsol[seed] {
                    continue;
                }
                if i == -1 {
                    // the seed's label is removed: end of chain
                    let d = delta + self.unlabeled_cost[seed];
                    if retained.is_none() || d < delta_best {
                        delta_best = d;
                        let mut c = Chain {
                            feat: Vec::new(),
                            label: Vec::new(),
                            delta: d,
                        };
                        for (f, _, n) in &current {
                            c.feat.push(*f);
                            c.label.push(*n);
                        }
                        c.feat.push(seed);
                        c.label.push(-1);
                        retained = Some(c);
                    }
                    continue;
                }
                let lid = self.first[seed] + i as usize;
                let mut delta_tmp = delta;
                let mut conflicts: Vec<usize> = Vec::new();
                let mut cycle = false;
                for o in self.conflicts_of(layers, lid, true) {
                    let feat = self.positions[o].prob_feat;
                    if current.iter().any(|(f, _, _)| *f == feat) {
                        cycle = true;
                        break;
                    }
                    if !conflicts.contains(&feat) {
                        conflicts.push(feat);
                        delta_tmp += self.positions[o].cost + self.unlabeled_cost[feat];
                    }
                }
                if cycle {
                    continue;
                }
                let lcost = self.positions[lid].cost;
                if conflicts.is_empty() {
                    if retained.is_none() || delta + lcost < delta_best {
                        delta_best = delta + lcost;
                        let mut c = Chain {
                            feat: Vec::new(),
                            label: Vec::new(),
                            delta: delta + lcost,
                        };
                        for (f, _, n) in &current {
                            c.feat.push(*f);
                            c.label.push(*n);
                        }
                        c.feat.push(seed);
                        c.label.push(lid as i64);
                        retained = Some(c);
                    }
                } else if conflicts.len() == 1 {
                    if delta_tmp < delta_min {
                        delta_min = delta_tmp;
                        retained_label = lid as i64;
                        next_seed = Some(conflicts[0]);
                    }
                } else {
                    let mut c = Chain {
                        feat: Vec::new(),
                        label: Vec::new(),
                        delta: delta + lcost,
                    };
                    for (f, _, n) in &current {
                        c.feat.push(*f);
                        c.label.push(*n);
                    }
                    c.feat.push(seed);
                    c.label.push(lid as i64);
                    for ftid in conflicts {
                        c.feat.push(ftid);
                        c.label.push(-1);
                        c.delta += self.unlabeled_cost[ftid];
                    }
                    if c.delta < delta_best {
                        delta_best = c.delta;
                        retained = Some(c);
                    }
                }
            }
            let Some(next) = next_seed else { break };
            if current.len() > max_degree {
                break;
            }
            let old = tmpsol[seed];
            current.push((seed, old, retained_label));
            if old != -1 {
                self.active_index
                    .remove(old as usize, &self.positions[old as usize].cbox);
            }
            if retained_label != -1 {
                let b = self.positions[retained_label as usize].cbox;
                self.active_index.insert(retained_label as usize, &b);
            }
            tmpsol[seed] = retained_label;
            delta += self.positions[retained_label as usize].cost;
            seed = next;
        }
        for (_, old, new) in current {
            if new != -1 {
                self.active_index
                    .remove(new as usize, &self.positions[new as usize].cbox);
            }
            if old != -1 {
                let b = self.positions[old as usize].cbox;
                self.active_index.insert(old as usize, &b);
            }
        }
        retained
    }

    /// Problem::chainSearch.
    fn chain_search(&mut self, layers: &[PalLayer], display_all: bool) {
        let nf = self.first.len();
        if nf == 0 {
            return;
        }
        let mut ok = vec![false; nf];
        self.init_sol_falp(layers, display_all);
        let mut iter = 0usize;
        loop {
            let mut seed = (iter + 1) % nf;
            while ok[seed] && seed != iter {
                seed = (seed + 1) % nf;
            }
            if seed == iter {
                break;
            }
            iter = (iter + 1) % nf;
            let chain = self.chain(layers, seed, 50);
            match chain {
                Some(c) if c.delta < -EPSILON => {
                    for (fid, lid) in c.feat.iter().zip(&c.label) {
                        let (fid, lid) = (*fid, *lid);
                        if self.sol[fid] >= 0 {
                            let old = self.sol[fid] as usize;
                            self.active_index.remove(old, &self.positions[old].cbox);
                            let b = self.positions[old].cbox;
                            for o in self.all_index.query(&b) {
                                if self.conflicting(layers, old, o) {
                                    ok[self.positions[o].prob_feat] = false;
                                }
                            }
                        }
                        self.sol[fid] = lid;
                        if lid >= 0 {
                            let b = self.positions[lid as usize].cbox;
                            self.active_index.insert(lid as usize, &b);
                        }
                        ok[fid] = false;
                    }
                }
                _ => ok[seed] = true,
            }
        }
    }
}

impl Pal {
    pub fn new(settings: EngineSettings, map_units_per_cm: f64) -> Pal {
        Pal {
            layers: Vec::new(),
            settings,
            map_units_per_cm,
        }
    }

    fn upright_only(layer: &PalLayer, lf: &LabelFeature) -> bool {
        match layer.upside_down {
            0 => true,
            1 => {
                lf.fixed_angle.is_none()
                    || (lf.fixed_position.is_none() && lf.fixed_angle == Some(0.0))
            }
            _ => false,
        }
    }

    fn feature_priority(&self, layer: &PalLayer, lf: &LabelFeature) -> f64 {
        if lf.always_show {
            return -0.2;
        }
        lf.priority.unwrap_or(layer.priority)
    }

    /// FeaturePart::createCandidates.
    fn create_candidates(&self, li: usize, part: &Part) -> Vec<LabelPosition> {
        let layer = &self.layers[li];
        let lf = &layer.features[part.feature];
        let ctx = CandidateContext {
            layer,
            lf,
            part,
            upright_only: Self::upright_only(layer, lf),
            line_per_unit: self.settings.candidates_line_per_cm / self.map_units_per_cm,
        };
        let angle = lf.fixed_angle.unwrap_or(0.0);
        let mut out = Vec::new();
        if let Some(p) = lf.fixed_position {
            out.push(ctx.pos(p.0, p.1, lf.size.0, lf.size.1, angle, 0.0, Quadrant::Over));
            return out;
        }
        let max_point = layer.max_point_candidates();
        match part.kind {
            Kind::Point => {
                let (x, y) = (part.x[0], part.y[0]);
                if layer.arrangement == Placement::OrderedPositionsAroundPoint {
                    ctx.ordered_positions(x, y, &mut out, angle, max_point);
                } else if layer.arrangement == Placement::OverPoint || lf.has_fixed_quadrant {
                    ctx.over_point(x, y, &mut out, angle);
                } else {
                    ctx.around_point(x, y, &mut out, angle, max_point);
                }
            }
            Kind::Polygon => {
                let (cx, cy) = shape::centroid(&part.shape);
                let (cx, cy) = if layer.centroid_inside && !part.contains_point((cx, cy)) {
                    point_on_surface(&part.shape).unwrap_or((cx, cy))
                } else {
                    (cx, cy)
                };
                match layer.arrangement {
                    Placement::AroundPoint => {
                        if near(lf.dist_label, 0.0) {
                            ctx.centered_over_point(cx, cy, &mut out, angle);
                        }
                        ctx.around_point(cx, cy, &mut out, angle, max_point);
                    }
                    Placement::OverPoint => ctx.over_point(cx, cy, &mut out, angle),
                    _ => {
                        tracing::debug!(
                            "polygon label placement {:?} not implemented yet",
                            layer.arrangement
                        );
                    }
                }
            }
            Kind::Line => match layer.arrangement {
                Placement::Curved | Placement::PerimeterCurved => {
                    // createDefaultCurvedCandidatesAlongLine: no overrun on repeated labels.
                    ctx.curved_along_line(
                        &mut out,
                        part.total_repeats <= 1,
                        self.max_line_candidates(layer, part),
                    );
                }
                Placement::Horizontal => ctx.horizontal_along_line(
                    &mut out,
                    angle,
                    self.max_line_candidates(layer, part),
                ),
                Placement::Line => {
                    ctx.along_line(&mut out, true, self.max_line_candidates(layer, part))
                }
                _ => tracing::debug!(
                    "line label placement {:?} not implemented yet",
                    layer.arrangement
                ),
            },
        }
        for c in &mut out {
            c.part = usize::MAX;
        }
        out
    }

    /// Pal::extractProblem + Pal::solveProblem; `extent` is the visible extent grown by the
    /// extent buffer.
    pub fn solve(&mut self, extent: BBox) -> Vec<Placed> {
        let partial = self.settings.partial_labels;
        let version2 = self.settings.version2;
        let gext = grid_extent(&extent);
        // Pal::extractProblem: merge connected lines, then chop at the repeat distance.
        for layer in &mut self.layers {
            if layer.merge_lines {
                layer.join_connected();
            }
            layer.chop_at_repeat_distance();
            if tracing::enabled!(target: "rgis::label_parts", tracing::Level::DEBUG) {
                for p in layer.parts.iter().filter(|p| p.kind == Kind::Line) {
                    let lf = &layer.features[p.feature];
                    let n = p.x.len() - 1;
                    tracing::debug!(target: "rgis::label_parts", "{}", serde_json::json!({"fid": lf.fid, "text": lf.label_text, "length": p.length(), "first": [p.x[0], p.y[0]], "last": [p.x[n], p.y[n]], "repeats": p.total_repeats}));
                }
            }
        }
        // Pal::extractProblem: index bounds = extent buffered by 1000 × its larger side.
        let buf = (extent.xmax - extent.xmin).max(extent.ymax - extent.ymin) * 1000.0;
        let rtree_bounds = BBox {
            xmin: extent.xmin - buf,
            ymin: extent.ymin - buf,
            xmax: extent.xmax + buf,
            ymax: extent.ymax + buf,
        };
        let mut first_round = GridIndex::new(gext, 64);
        let mut feats: Vec<Feats> = Vec::new();
        let mut first_round_positions: Vec<(usize, usize)> = Vec::new();
        let mut obstacles: Vec<(usize, Part)> = Vec::new();
        let mut unplaced = 0usize;
        let mut next_global = 0usize;
        let map_rect = (extent.xmin, extent.ymin, extent.xmax, extent.ymax);

        // Providers were added bottom layer first; PAL walks them in reverse.
        for li in (0..self.layers.len()).rev() {
            let nparts = self.layers[li].parts.len();
            for pi in 0..nparts {
                let part = &self.layers[li].parts[pi];
                for h in &part.holes {
                    obstacles.push((li, h.clone()));
                }
                let mut candidates = self.create_candidates(li, part);
                let extent_ring = Shape::Polygon(vec![vec![
                    (extent.xmin, extent.ymin),
                    (extent.xmax, extent.ymin),
                    (extent.xmax, extent.ymax),
                    (extent.xmin, extent.ymax),
                    (extent.xmin, extent.ymin),
                ]]);
                // LabelPosition::intersects / within over all parts.
                candidates.retain(|c| {
                    if partial {
                        c.all_quads().any(|(q, b, a)| {
                            b.intersects(&extent)
                                && (near(a, 0.0) || shape::quad_intersects(&q, &extent_ring))
                        })
                    } else {
                        c.all_quads()
                            .all(|(q, _, _)| shape::rect_contains_quad(map_rect, &q))
                    }
                });
                let lf = &self.layers[li].features[part.feature];
                let priority = self.feature_priority(&self.layers[li], lf);
                if !candidates.is_empty() {
                    for c in &mut candidates {
                        c.global_id = next_global;
                        next_global += 1;
                        c.part = pi;
                    }
                    std_sort(&mut candidates, &|a: &LabelPosition, b: &LabelPosition| {
                        a.cost < b.cost
                    });
                    feats.push(Feats {
                        layer: li,
                        part: pi,
                        candidates,
                        priority,
                    });
                } else if lf.allow_degraded {
                    // FeaturePart::createCandidatePointOnSurface
                    if let Some(p) = point_on_surface(&part.shape) {
                        let mut c = LabelPosition::new(
                            part,
                            p.0,
                            p.1,
                            lf.size.0,
                            lf.size.1,
                            0.0,
                            0.0,
                            Quadrant::Over,
                            true,
                        );
                        c.global_id = next_global;
                        next_global += 1;
                        c.part = pi;
                        feats.push(Feats {
                            layer: li,
                            part: pi,
                            candidates: vec![c],
                            priority,
                        });
                    }
                } else {
                    unplaced += 1;
                }
            }
            for o in &self.layers[li].obstacles {
                obstacles.push((li, o.clone()));
            }
        }
        let _ = unplaced;

        // Index all first-round candidates for obstacle costing.
        for (fi, f) in feats.iter().enumerate() {
            for (ci, c) in f.candidates.iter().enumerate() {
                first_round.insert(first_round_positions.len(), c.bbox);
                first_round_positions.push((fi, ci));
            }
        }

        // Obstacle costs (CostCalculator::addObstacleCostPenalty).
        for (oli, obstacle) in &obstacles {
            let olayer = &self.layers[*oli];
            let ofeat = &olayer.features[obstacle.feature];
            for id in first_round.query(&obstacle.bbox) {
                let (fi, ci) = first_round_positions[id];
                let (cli, _) = (feats[fi].layer, feats[fi].part);
                let lp = &feats[fi].candidates[ci];
                let lf = &self.layers[cli].features[lp.feature];
                let same = self.layers[cli].name == olayer.name
                    && lf.fid == ofeat.fid
                    && lf.sub_part == ofeat.sub_part;
                if lf.overlap == OverlapHandling::AllowOverlapAtNoCost
                    || (!obstacle.is_hole && same)
                    || (obstacle.is_hole && !same)
                {
                    continue;
                }
                let n = match obstacle.kind {
                    Kind::Point => {
                        let d = lp.distance_to_point((obstacle.x[0], obstacle.y[0]));
                        if d < 0.0 {
                            2
                        } else if d < lf.dist_label {
                            1
                        } else {
                            0
                        }
                    }
                    Kind::Line => i32::from(lp.intersects_shape(&obstacle.shape)),
                    Kind::Polygon => match olayer.obstacle_type {
                        ObstacleType::PolygonInterior => lp.polygon_intersection_cost(obstacle),
                        ObstacleType::PolygonBoundary => {
                            if lp.crosses_boundary(&obstacle.shape) {
                                6
                            } else {
                                0
                            }
                        }
                        ObstacleType::PolygonWhole => {
                            if lp.intersects_shape(&obstacle.shape) {
                                12
                            } else {
                                0
                            }
                        }
                    },
                };
                let factor = ofeat.obstacle_factor;
                let cost = factor * n as f64;
                let priority = 2.0 * (1.0 - self.feature_priority(&self.layers[cli], lf));
                let lp = &mut feats[fi].candidates[ci];
                if n > 0 {
                    lp.conflicts_with_obstacle = true;
                }
                if version2 && n > 0 && priority < factor && !near_eps(priority, factor, 0.001) {
                    lp.hard_conflict = true;
                }
                lp.cost += cost;
            }
        }

        // Finalise candidates per feature.
        let nf = feats.len();
        let mut prob = Problem {
            positions: Vec::new(),
            first: vec![0; nf],
            count: vec![0; nf],
            unlabeled_cost: vec![0.0; nf],
            total: 0,
            all_index: PalRtree::new(rtree_bounds),
            active_index: PalRtree::new(rtree_bounds),
            sol: Vec::new(),
            conflict_cache: HashMap::new(),
        };
        let bb = [extent.xmin, extent.xmax, extent.xmax, extent.xmin];
        let bby = [extent.ymin, extent.ymin, extent.ymax, extent.ymax];
        let mut kept: Vec<Vec<LabelPosition>> = Vec::with_capacity(nf);
        for (fi, feat) in feats.iter_mut().enumerate() {
            prob.unlabeled_cost[fi] = 2f64.powf(10.0 - 10.0 * feat.priority);
            let layer = &self.layers[feat.layer];
            let part = &layer.parts[feat.part];
            let lf = &layer.features[part.feature];
            let max_candidates = match part.kind {
                Kind::Point => layer.max_point_candidates(),
                Kind::Line => self.max_line_candidates(layer, part),
                Kind::Polygon => 16.max(self.max_polygon_candidates(layer, part)),
            };
            let mut cands = std::mem::take(&mut feat.candidates);
            let prune = |cands: &mut Vec<LabelPosition>| -> bool {
                if !version2 || cands.is_empty() {
                    return false;
                }
                let first = cands.remove(0);
                cands.retain(|c| !c.hard_conflict);
                cands.insert(0, first);
                if cands.len() == 1
                    && cands[0].hard_conflict
                    && lf.overlap == OverlapHandling::PreventOverlap
                {
                    cands.clear();
                    return true;
                }
                false
            };
            if lf.overlap == OverlapHandling::PreventOverlap {
                prune(&mut cands);
            }
            if cands.is_empty() {
                kept.push(cands);
                continue;
            }
            finalize_costs(&mut cands, part, layer.arrangement, &bb, &bby);
            std_sort(&mut cands, &|a: &LabelPosition, b: &LabelPosition| {
                a.cost < b.cost
            });
            if lf.overlap != OverlapHandling::PreventOverlap {
                prune(&mut cands);
            }
            if max_candidates > 0 && cands.len() > max_candidates {
                cands.truncate(max_candidates);
            }
            prob.count[fi] = cands.len();
            if tracing::enabled!(target: "rgis::label_cands", tracing::Level::TRACE) {
                for c in &cands {
                    tracing::trace!(target: "rgis::label_cands", "{}", serde_json::json!({"fid": lf.fid, "cost": c.cost, "bbox": [c.bbox.xmin, c.bbox.ymin, c.bbox.xmax, c.bbox.ymax]}));
                }
            }
            kept.push(cands);
        }
        // Assign problem ids in order and index them.
        let mut next = 0usize;
        for (fi, cands) in kept.into_iter().enumerate() {
            prob.first[fi] = next;
            for mut c in cands {
                let d = self.layers[c.layer].features[c.feature].no_repeat_distance;
                if d > 0.0 {
                    c.cbox = BBox {
                        xmin: c.bbox.xmin - d,
                        ymin: c.bbox.ymin - d,
                        xmax: c.bbox.xmax + d,
                        ymax: c.bbox.ymax + d,
                    };
                }
                c.prob_feat = fi;
                c.id = next;
                prob.all_index.insert(next, &c.cbox);
                prob.positions.push(c);
                next += 1;
            }
        }
        prob.total = next;
        // Overlap counts.
        for lp in 0..prob.positions.len() {
            prob.positions[lp].num_overlaps = 0;
            if prob.positions[lp].cost >= 1.0 {
                prob.positions[lp].cost -= prob.positions[lp].cost.trunc();
            }
            let n = prob.conflicts_of(&self.layers, lp, false).len() as i32;
            prob.positions[lp].num_overlaps = n;
        }

        tracing::debug!(
            features = nf,
            candidates = prob.positions.len(),
            overlaps = prob.positions.iter().map(|p| p.num_overlaps as i64).sum::<i64>() / 2,
            registered = ?self.layers.iter().map(|l| l.keys.len()).collect::<Vec<_>>(),
            "label problem"
        );
        prob.reduce(&self.layers);
        prob.total = prob.count.iter().sum();
        prob.chain_search(&self.layers, self.settings.use_all_labels);

        // Problem::getSolution
        let mut out = Vec::new();
        for f in 0..nf {
            let label = prob.sol[f];
            let start = prob.first[f];
            let found = start < prob.positions.len();
            let chosen = if label != -1 {
                Some(label as usize)
            } else if found {
                let lp = &prob.positions[start];
                let lf = &self.layers[lp.layer].features[lp.feature];
                (self.settings.use_all_labels
                    || lf.overlap == OverlapHandling::AllowOverlapIfRequired
                    || lf.always_show)
                    .then_some(start)
            } else {
                None
            };
            if let Some(c) = chosen {
                let lp = &prob.positions[c];
                out.push(Placed {
                    layer: lp.layer,
                    feature: lp.feature,
                    x: lp.x,
                    y: lp.y,
                    alpha: lp.alpha,
                    quadrant: lp.quadrant,
                    payload: self.layers[lp.layer].features[lp.feature].payload,
                    conflicts_with_obstacle: lp.conflicts_with_obstacle,
                    part_id: lp.part_id,
                    parts: lp.next.clone(),
                });
            }
        }
        out
    }

    fn max_line_candidates(&self, layer: &PalLayer, part: &Part) -> usize {
        let l = part.length();
        if l <= 0.0 {
            return 1;
        }
        let per_unit = self.settings.candidates_line_per_cm / self.map_units_per_cm;
        let for_length = (per_unit * l).ceil() as usize;
        match layer.max_line_candidates() {
            0 => for_length,
            m => for_length.min(m),
        }
    }

    fn max_polygon_candidates(&self, layer: &PalLayer, part: &Part) -> usize {
        let a = part.area();
        if a <= 0.0 {
            return 1;
        }
        let per_unit2 = self.settings.candidates_polygon_per_cm2 / self.map_units_per_cm.powi(2);
        let for_area = (per_unit2 * a).ceil() as usize;
        match layer.max_polygon_candidates() {
            0 => for_area,
            m => for_area.min(m),
        }
    }
}

/// CostCalculator::finalizeCandidatesCosts.
fn finalize_costs(
    cands: &mut Vec<LabelPosition>,
    part: &Part,
    arrangement: Placement,
    bbx: &[f64; 4],
    bby: &[f64; 4],
) {
    std_sort(cands, &|a: &LabelPosition, b: &LabelPosition| {
        a.cost < b.cost
    });
    let mut discrim = 0.0;
    let mut stop;
    loop {
        discrim += 1.0;
        stop = 0;
        while stop < cands.len() && cands[stop].cost < discrim {
            stop += 1;
        }
        if !(stop == 0 && discrim < cands.last().map(|c| c.cost).unwrap_or(0.0) + 2.0) {
            break;
        }
    }
    if discrim > 1.5 {
        for c in cands.iter_mut().take(stop) {
            c.cost = 0.0021;
        }
    }
    cands.truncate(stop);
    if part.kind == Kind::Polygon && matches!(arrangement, Placement::Free | Placement::Horizontal)
    {
        // TODO: polygon ring/centroid distance costs (used with free/horizontal placement)
    }
    // FeaturePart::addSizePenalty
    let size_cost = match part.kind {
        Kind::Line => {
            let l = part.length();
            let bbox_length = (bbx[2] - bbx[0]).max(bby[2] - bby[0]);
            if l <= 0.0 || l >= bbox_length / 4.0 {
                return;
            }
            1.0 - l / (bbox_length / 4.0)
        }
        Kind::Polygon => {
            let a = part.area();
            let bbox_area = (bbx[2] - bbx[0]) * (bby[2] - bby[0]);
            if a <= 0.0 || a >= bbox_area / 16.0 {
                return;
            }
            1.0 - a / (bbox_area / 16.0)
        }
        Kind::Point => return,
    };
    for c in cands.iter_mut() {
        c.cost += size_cost / 100.0;
    }
}

/// GEOSPointOnSurface (approximation for points/lines; scanline interior point for polygons).
fn point_on_surface(s: &Shape) -> Option<Pt> {
    match s {
        Shape::Point(p) => Some(*p),
        Shape::Line(l) => {
            if l.len() > 2 {
                Some(l[l.len() / 2])
            } else {
                l.first().copied()
            }
        }
        Shape::Polygon(rings) => {
            let ext = rings.first()?;
            let ymin = ext.iter().map(|p| p.1).fold(f64::MAX, f64::min);
            let ymax = ext.iter().map(|p| p.1).fold(f64::MIN, f64::max);
            let y = (ymin + ymax) / 2.0;
            let mut xs: Vec<f64> = Vec::new();
            for r in rings {
                for w in r.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if (a.1 > y) != (b.1 > y) {
                        xs.push(a.0 + (y - a.1) / (b.1 - a.1) * (b.0 - a.0));
                    }
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mut best: Option<(f64, f64)> = None;
            for pair in xs.chunks(2) {
                if let [a, b] = pair
                    && best.is_none_or(|(w, _)| b - a > w)
                {
                    best = Some((b - a, (a + b) / 2.0));
                }
            }
            best.map(|(_, x)| (x, y))
        }
    }
}
