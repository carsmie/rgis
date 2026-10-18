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
use super::settings::{EngineSettings, ObstacleType, OverlapHandling, Placement, PointPosition};
use super::shape::{self, Pt, Shape};

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
}

/// pal::Layer.
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
}

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
        Part { layer, feature, kind, shape, x, y, bbox, holes, is_hole: false }
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
            Shape::Polygon(r) => r.first().map(|e| shape::min_dist2_to_line(e, p)).unwrap_or(0.0),
        }
    }
}

/// pal::LabelPosition.
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
    prob_feat: usize,
    id: usize,
    num_overlaps: i32,
    conflicts_with_obstacle: bool,
    hard_conflict: bool,
    global_id: usize,
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
            prob_feat: 0,
            id: 0,
            num_overlaps: 0,
            conflicts_with_obstacle: false,
            hard_conflict: false,
            global_id: 0,
        }
    }

    fn quad(&self) -> [Pt; 4] {
        [(self.x[0], self.y[0]), (self.x[1], self.y[1]), (self.x[2], self.y[2]), (self.x[3], self.y[3])]
    }

    pub fn cost(&self) -> f64 {
        self.cost
    }

    /// LabelPosition::getDistanceToPoint (no outer bounds).
    fn distance_to_point(&self, p: Pt) -> f64 {
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

    fn intersects_shape(&self, s: &Shape) -> bool {
        shape::quad_intersects(&self.quad(), s)
    }

    /// LabelPosition::polygonIntersectionCost (0..12).
    fn polygon_intersection_cost(&self, poly: &Part) -> i32 {
        let mut cost: f64 = 0.0;
        if self.intersects_shape(&poly.shape) {
            cost += 1.0;
            for i in 0..4 {
                let mut p = (self.x[i], self.y[i]);
                for _ in 0..2 {
                    if poly.contains_point(p) {
                        cost += 1.0;
                    }
                    p = ((self.x[i] + self.x[(i + 1) % 4]) / 2.0, (self.y[i] + self.y[(i + 1) % 4]) / 2.0);
                }
            }
            let c = ((self.x[0] + self.x[2]) / 2.0, (self.y[0] + self.y[2]) / 2.0);
            if poly.contains_point(c) {
                cost += 4.0;
            }
        }
        cost = 12.0 * cost / 13.0;
        cost.ceil() as i32
    }
}

/// QgsLabelingEngine output: the placed labels, sorted for drawing.
pub struct Placed {
    pub layer: usize,
    pub feature: usize,
    pub x: [f64; 4],
    pub y: [f64; 4],
    pub alpha: f64,
    pub quadrant: Quadrant,
    pub payload: usize,
    pub conflicts_with_obstacle: bool,
}

pub struct Pal {
    pub layers: Vec<PalLayer>,
    settings: EngineSettings,
    /// Map units per centimetre at the output resolution.
    map_units_per_cm: f64,
}

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
            if (part.kind == Kind::Line && part.x.len() < 2) || (part.kind == Kind::Polygon && part.x.len() < 3) {
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
                    let size = if part.kind == Kind::Line { part.length() } else { part.area() };
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
        if lf.obstacle {
            if let Some(obs) = &lf.obstacle_parts {
                for s in obs {
                    let part = Part::new(layer_index, fi, s.clone());
                    if (part.kind == Kind::Line && part.x.len() < 2) || (part.kind == Kind::Polygon && part.x.len() < 3) {
                        continue;
                    }
                    self.obstacles.push(part);
                }
            }
        }
        if let Some((_, part)) = biggest {
            self.parts.push(part);
            added = true;
        }
        self.features.push(lf);
        if !added {
            self.keys.remove(&(self.features[fi].fid, self.features[fi].sub_part));
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
        matches!(self.arrangement, Placement::Curved | Placement::PerimeterCurved)
    }
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
        (Some(std::cmp::Ordering::Greater), Some(std::cmp::Ordering::Greater)) => Quadrant::BelowRight,
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

    fn pos(&self, x: f64, y: f64, w: f64, h: f64, alpha: f64, cost: f64, q: Quadrant) -> LabelPosition {
        LabelPosition::new(self.part, x, y, w, h, alpha, cost, q, self.upright_only)
    }

    /// FeaturePart::createCandidateCenteredOverPoint.
    fn centered_over_point(&self, x: f64, y: f64, out: &mut Vec<LabelPosition>, angle: f64) {
        let (w, h) = (self.width(), self.height());
        out.push(self.pos(x - w / 2.0, y - h / 2.0, w, h, angle, 0.00005, Quadrant::Over));
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
        out.push(self.pos(x + xdiff, y + ydiff, w, h, angle, 0.0001, quadrant_from_offset(lf.quad_offset)));
    }

    /// FeaturePart::createCandidatesAtOrderedPositionsOverPoint.
    fn ordered_positions(&self, x: f64, y: f64, out: &mut Vec<LabelPosition>, angle: f64, max_point: usize) {
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
            let (lx, ly, quadrant) = ordered_position(x, y, w, h, pos, distance, lf.visual_margin, sw, sh, angle);
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
    fn around_point(&self, x: f64, y: f64, out: &mut Vec<LabelPosition>, angle: f64, max_point: usize) {
        let lf = self.lf;
        let (w, h) = (self.width(), self.height());
        let dist = lf.dist_label;
        let ray_count = if max_point == 0 { 16 } else { max_point as i32 };
        let (candidates_per_ray, ray_step) = if lf.max_distance > dist && !near(lf.max_distance, 0.0) {
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
            ((h / 2.0).atan2(dist + w / 2.0), (w / 2.0).atan2(dist + h / 2.0))
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
                let mut cost = if ray_count == 1 { 0.0001 } else { 0.0001 + scaled };
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
        TopLeft => (Quadrant::AboveLeft, 3.0 * FRAC_PI_4, -w + right - sw, -bottom + sh),
        TopSlightlyLeft => (Quadrant::AboveRight, FRAC_PI_2, -w / 4.0 - left, -bottom + sh),
        TopMiddle => (Quadrant::Above, FRAC_PI_2, -w / 2.0, -bottom + sh),
        TopSlightlyRight => (Quadrant::AboveLeft, FRAC_PI_2, -w * 3.0 / 4.0 + right, -bottom + sh),
        TopRight => (Quadrant::AboveRight, FRAC_PI_4, -left + sw, -bottom + sh),
        MiddleLeft => (Quadrant::Left, PI, -w + right - sw, -h / 2.0),
        MiddleRight => (Quadrant::Right, 0.0, -left + sw, -h / 2.0),
        BottomLeft => (Quadrant::BelowLeft, 5.0 * FRAC_PI_4, -w + right - sw, -h + top - sh),
        BottomSlightlyLeft => (Quadrant::BelowRight, 3.0 * FRAC_PI_2, -w / 4.0 - left, -h + top - sh),
        BottomMiddle => (Quadrant::Below, 3.0 * FRAC_PI_2, -w / 2.0, -h + top - sh),
        BottomSlightlyRight => (Quadrant::BelowLeft, 3.0 * FRAC_PI_2, -w * 3.0 / 4.0 + right, -h + top - sh),
        BottomRight => (Quadrant::BelowRight, 7.0 * FRAC_PI_4, -left + sw, -h + top - sh),
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
        PriorityQueue { heap: vec![0; n], p: vec![0.0; n], pos: vec![-1; max_id + 1], size: 0 }
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
    all_index: GridIndex,
    active_index: GridIndex,
    sol: Vec<i64>,
    conflict_cache: HashMap<(usize, usize), bool>,
}

fn grid_extent(b: &BBox) -> BBox {
    let m = (b.xmax - b.xmin).max(b.ymax - b.ymin) * 0.5;
    BBox { xmin: b.xmin - m, ymin: b.ymin - m, xmax: b.xmax + m, ymax: b.ymax + m }
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
        if fa.overlap == OverlapHandling::AllowOverlapAtNoCost || fb.overlap == OverlapHandling::AllowOverlapAtNoCost {
            return false;
        }
        if near(la.alpha, 0.0) && near(lb.alpha, 0.0) {
            return la.bbox.intersects(&lb.bbox);
        }
        let key = (la.global_id.min(lb.global_id), la.global_id.max(lb.global_id));
        if let Some(r) = self.conflict_cache.get(&key) {
            return *r;
        }
        let q = la.quad();
        let r = shape::quad_intersects(&lb.quad(), &Shape::Polygon(vec![vec![q[0], q[1], q[2], q[3], q[0]]]));
        self.conflict_cache.insert(key, r);
        r
    }

    fn conflicts_of(&mut self, layers: &[PalLayer], lp: usize, active: bool) -> Vec<usize> {
        let b = self.positions[lp].bbox;
        let ids = if active { self.active_index.query(&b) } else { self.all_index.query(&b) };
        ids.into_iter().filter(|&o| self.conflicting(layers, lp, o)).collect()
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
                        let b = self.positions[lpid].bbox;
                        for o in self.all_index.query(&b) {
                            if self.conflicting(layers, lpid, o) {
                                self.positions[o].num_overlaps -= 1;
                                self.positions[lpid].num_overlaps -= 1;
                            }
                        }
                        self.all_index.remove(lpid);
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
            let b = self.positions[lp].bbox;
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
            let b = self.positions[label].bbox;
            self.active_index.insert(label, b);
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
                    let bb = self.positions[b].bbox;
                    self.active_index.insert(b, bb);
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
                        let mut c = Chain { feat: Vec::new(), label: Vec::new(), delta: d };
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
                        let mut c = Chain { feat: Vec::new(), label: Vec::new(), delta: delta + lcost };
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
                    let mut c = Chain { feat: Vec::new(), label: Vec::new(), delta: delta + lcost };
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
                self.active_index.remove(old as usize);
            }
            if retained_label != -1 {
                let b = self.positions[retained_label as usize].bbox;
                self.active_index.insert(retained_label as usize, b);
            }
            tmpsol[seed] = retained_label;
            delta += self.positions[retained_label as usize].cost;
            seed = next;
        }
        for (_, old, new) in current {
            if new != -1 {
                self.active_index.remove(new as usize);
            }
            if old != -1 {
                let b = self.positions[old as usize].bbox;
                self.active_index.insert(old as usize, b);
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
                            self.active_index.remove(old);
                            let b = self.positions[old].bbox;
                            for o in self.all_index.query(&b) {
                                if self.conflicting(layers, old, o) {
                                    ok[self.positions[o].prob_feat] = false;
                                }
                            }
                        }
                        self.sol[fid] = lid;
                        if lid >= 0 {
                            let b = self.positions[lid as usize].bbox;
                            self.active_index.insert(lid as usize, b);
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
        Pal { layers: Vec::new(), settings, map_units_per_cm }
    }

    fn upright_only(layer: &PalLayer, lf: &LabelFeature) -> bool {
        match layer.upside_down {
            0 => true,
            1 => lf.fixed_angle.is_none() || (lf.fixed_position.is_none() && lf.fixed_angle == Some(0.0)),
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
                        tracing::debug!("polygon label placement {:?} not implemented yet", layer.arrangement);
                    }
                }
            }
            Kind::Line => {
                tracing::debug!("line label placement {:?} not implemented yet", layer.arrangement);
            }
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
                candidates.retain(|c| {
                    if partial {
                        c.bbox.intersects(&extent)
                            && (near(c.alpha, 0.0)
                                || shape::quad_intersects(
                                    &c.quad(),
                                    &Shape::Polygon(vec![vec![
                                        (extent.xmin, extent.ymin),
                                        (extent.xmax, extent.ymin),
                                        (extent.xmax, extent.ymax),
                                        (extent.xmin, extent.ymax),
                                        (extent.xmin, extent.ymin),
                                    ]]),
                                ))
                    } else {
                        shape::rect_contains_quad(map_rect, &c.quad())
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
                    candidates.sort_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(std::cmp::Ordering::Equal));
                    feats.push(Feats { layer: li, part: pi, candidates, priority });
                } else if lf.allow_degraded {
                    // FeaturePart::createCandidatePointOnSurface
                    if let Some(p) = point_on_surface(&part.shape) {
                        let mut c = LabelPosition::new(part, p.0, p.1, lf.size.0, lf.size.1, 0.0, 0.0, Quadrant::Over, true);
                        c.global_id = next_global;
                        next_global += 1;
                        c.part = pi;
                        feats.push(Feats { layer: li, part: pi, candidates: vec![c], priority });
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
                let same = self.layers[cli].name == olayer.name && lf.fid == ofeat.fid && lf.sub_part == ofeat.sub_part;
                if lf.overlap == OverlapHandling::AllowOverlapAtNoCost || (!obstacle.is_hole && same) || (obstacle.is_hole && !same) {
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
                            let inter = lp.intersects_shape(&obstacle.shape);
                            let contains = match &obstacle.shape {
                                Shape::Polygon(r) => shape::polygon_contains_quad(r, &lp.quad()),
                                _ => false,
                            };
                            if inter && !contains {
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
            all_index: GridIndex::new(gext, 64),
            active_index: GridIndex::new(gext, 64),
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
                if cands.len() == 1 && cands[0].hard_conflict && lf.overlap == OverlapHandling::PreventOverlap {
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
            cands.sort_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(std::cmp::Ordering::Equal));
            if lf.overlap != OverlapHandling::PreventOverlap {
                prune(&mut cands);
            }
            if max_candidates > 0 && cands.len() > max_candidates {
                cands.truncate(max_candidates);
            }
            prob.count[fi] = cands.len();
            kept.push(cands);
        }
        // Assign problem ids in order and index them.
        let mut next = 0usize;
        for (fi, cands) in kept.into_iter().enumerate() {
            prob.first[fi] = next;
            for mut c in cands {
                c.prob_feat = fi;
                c.id = next;
                prob.all_index.insert(next, c.bbox);
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
                (self.settings.use_all_labels || lf.overlap == OverlapHandling::AllowOverlapIfRequired || lf.always_show)
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
fn finalize_costs(cands: &mut Vec<LabelPosition>, part: &Part, arrangement: Placement, bbx: &[f64; 4], bby: &[f64; 4]) {
    cands.sort_by(|a, b| a.cost.partial_cmp(&b.cost).unwrap_or(std::cmp::Ordering::Equal));
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
    if part.kind == Kind::Polygon && matches!(arrangement, Placement::Free | Placement::Horizontal) {
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
                if let [a, b] = pair {
                    if best.is_none_or(|(w, _)| b - a > w) {
                        best = Some((b - a, (a + b) / 2.0));
                    }
                }
            }
            best.map(|(_, x)| (x, y))
        }
    }
}
