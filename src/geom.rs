//! Geometry helpers: extents, reprojection, hit testing and GeoJSON / GML encoding.

use geo_types::{Coord, Geometry, LineString, Polygon};

use crate::crs::Transform;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub xmin: f64,
    pub ymin: f64,
    pub xmax: f64,
    pub ymax: f64,
}

impl Rect {
    pub fn new(xmin: f64, ymin: f64, xmax: f64, ymax: f64) -> Rect {
        Rect {
            xmin,
            ymin,
            xmax,
            ymax,
        }
    }
    pub fn empty() -> Rect {
        Rect::new(
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        )
    }
    pub fn is_empty(&self) -> bool {
        !(self.xmin <= self.xmax && self.ymin <= self.ymax)
    }
    pub fn width(&self) -> f64 {
        self.xmax - self.xmin
    }
    pub fn height(&self) -> f64 {
        self.ymax - self.ymin
    }
    pub fn expand(&mut self, x: f64, y: f64) {
        self.xmin = self.xmin.min(x);
        self.ymin = self.ymin.min(y);
        self.xmax = self.xmax.max(x);
        self.ymax = self.ymax.max(y);
    }
    pub fn union(&mut self, o: &Rect) {
        if !o.is_empty() {
            self.expand(o.xmin, o.ymin);
            self.expand(o.xmax, o.ymax);
        }
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.xmin <= o.xmax && o.xmin <= self.xmax && self.ymin <= o.ymax && o.ymin <= self.ymax
    }
    pub fn contains_rect(&self, o: &Rect) -> bool {
        o.xmin >= self.xmin && o.xmax <= self.xmax && o.ymin >= self.ymin && o.ymax <= self.ymax
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.xmin && x <= self.xmax && y >= self.ymin && y <= self.ymax
    }
    pub fn swapped(&self) -> Rect {
        Rect::new(self.ymin, self.xmin, self.ymax, self.xmax)
    }
    /// Parse "a,b,c,d" (extra trailing values such as a CRS are ignored).
    pub fn parse(s: &str) -> Option<Rect> {
        let v: Vec<f64> = s
            .split(',')
            .take(4)
            .map(|p| p.trim().parse().ok())
            .collect::<Option<_>>()?;
        (v.len() == 4).then(|| Rect::new(v[0], v[1], v[2], v[3]))
    }
}

pub fn for_each_coord(g: &Geometry<f64>, f: &mut impl FnMut(Coord<f64>)) {
    match g {
        Geometry::Point(p) => f(p.0),
        Geometry::Line(l) => {
            f(l.start);
            f(l.end)
        }
        Geometry::LineString(ls) => ls.0.iter().for_each(|c| f(*c)),
        Geometry::Polygon(p) => polygon_rings(p).for_each(|r| r.0.iter().for_each(|c| f(*c))),
        Geometry::MultiPoint(mp) => mp.0.iter().for_each(|p| f(p.0)),
        Geometry::MultiLineString(m) => m.0.iter().for_each(|ls| ls.0.iter().for_each(|c| f(*c))),
        Geometry::MultiPolygon(m) => {
            m.0.iter()
                .for_each(|p| polygon_rings(p).for_each(|r| r.0.iter().for_each(|c| f(*c))))
        }
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|g| for_each_coord(g, f)),
        Geometry::Rect(r) => for_each_coord(&Geometry::Polygon(r.to_polygon()), f),
        Geometry::Triangle(t) => for_each_coord(&Geometry::Polygon(t.to_polygon()), f),
    }
}

pub fn polygon_rings(p: &Polygon<f64>) -> impl Iterator<Item = &LineString<f64>> {
    std::iter::once(p.exterior()).chain(p.interiors().iter())
}

pub fn bbox(g: &Geometry<f64>) -> Rect {
    let mut r = Rect::empty();
    for_each_coord(g, &mut |c| r.expand(c.x, c.y));
    r
}

/// Reproject a geometry; vertices that fail to transform are dropped.
pub fn transform(g: &Geometry<f64>, t: &Transform) -> Geometry<f64> {
    if t.is_identity() {
        return g.clone();
    }
    let ls = |l: &LineString<f64>| -> LineString<f64> {
        l.0.iter()
            .filter_map(|c| t.point(c.x, c.y).ok())
            .map(|(x, y)| Coord { x, y })
            .collect()
    };
    let poly =
        |p: &Polygon<f64>| Polygon::new(ls(p.exterior()), p.interiors().iter().map(ls).collect());
    match g {
        Geometry::Point(p) => match t.point(p.x(), p.y()) {
            Ok((x, y)) => Geometry::Point((x, y).into()),
            Err(_) => Geometry::MultiPoint(geo_types::MultiPoint(vec![])),
        },
        Geometry::LineString(l) => Geometry::LineString(ls(l)),
        Geometry::Polygon(p) => Geometry::Polygon(poly(p)),
        Geometry::MultiPoint(m) => Geometry::MultiPoint(
            m.0.iter()
                .filter_map(|p| t.point(p.x(), p.y()).ok())
                .map(|xy| xy.into())
                .collect::<Vec<geo_types::Point>>()
                .into(),
        ),
        Geometry::MultiLineString(m) => Geometry::MultiLineString(m.0.iter().map(ls).collect()),
        Geometry::MultiPolygon(m) => Geometry::MultiPolygon(m.0.iter().map(poly).collect()),
        Geometry::GeometryCollection(gc) => {
            Geometry::GeometryCollection(gc.0.iter().map(|g| transform(g, t)).collect())
        }
        Geometry::Line(l) => transform(
            &Geometry::LineString(LineString::from(vec![l.start, l.end])),
            t,
        ),
        Geometry::Rect(r) => transform(&Geometry::Polygon(r.to_polygon()), t),
        Geometry::Triangle(tr) => transform(&Geometry::Polygon(tr.to_polygon()), t),
    }
}

fn ring_contains(ring: &LineString<f64>, x: f64, y: f64) -> bool {
    let pts = &ring.0;
    let mut inside = false;
    let n = pts.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (pts[i], pts[j]);
        if (a.y > y) != (b.y > y) && x < (b.x - a.x) * (y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn polygon_contains(p: &Polygon<f64>, x: f64, y: f64) -> bool {
    ring_contains(p.exterior(), x, y) && !p.interiors().iter().any(|r| ring_contains(r, x, y))
}

fn seg_dist(p: Coord<f64>, a: Coord<f64>, b: Coord<f64>) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.x + t * dx, a.y + t * dy);
    ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt()
}

fn line_dist(l: &LineString<f64>, p: Coord<f64>) -> f64 {
    l.0.windows(2)
        .map(|w| seg_dist(p, w[0], w[1]))
        .fold(f64::INFINITY, f64::min)
}

/// QGIS GetFeatureInfo semantics: does the geometry intersect the square search
/// rectangle centred at (x, y) with half-size `tol`?
pub fn hits(g: &Geometry<f64>, x: f64, y: f64, tol: f64) -> bool {
    let p = Coord { x, y };
    // Distance to the boundary within `tol` approximates rectangle intersection.
    let near = |l: &LineString<f64>| line_dist(l, p) <= tol;
    match g {
        Geometry::Point(pt) => (pt.x() - x).abs() <= tol && (pt.y() - y).abs() <= tol,
        Geometry::MultiPoint(m) => {
            m.0.iter()
                .any(|pt| (pt.x() - x).abs() <= tol && (pt.y() - y).abs() <= tol)
        }
        Geometry::LineString(l) => near(l),
        Geometry::MultiLineString(m) => m.0.iter().any(near),
        Geometry::Polygon(pg) => polygon_contains(pg, x, y) || polygon_rings(pg).any(near),
        Geometry::MultiPolygon(m) => {
            m.0.iter()
                .any(|pg| polygon_contains(pg, x, y) || polygon_rings(pg).any(near))
        }
        Geometry::GeometryCollection(gc) => gc.0.iter().any(|g| hits(g, x, y, tol)),
        other => bbox(other).contains(x, y),
    }
}

fn segments_cross(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>, d: Coord<f64>) -> bool {
    let orient = |p: Coord<f64>, q: Coord<f64>, r: Coord<f64>| {
        (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x)
    };
    let (d1, d2, d3, d4) = (
        orient(c, d, a),
        orient(c, d, b),
        orient(a, b, c),
        orient(a, b, d),
    );
    ((d1 > 0.0) != (d2 > 0.0) || d1 == 0.0 || d2 == 0.0)
        && ((d3 > 0.0) != (d4 > 0.0) || d3 == 0.0 || d4 == 0.0)
}

fn segment_hits_rect(a: Coord<f64>, b: Coord<f64>, r: &Rect) -> bool {
    if r.contains(a.x, a.y) || r.contains(b.x, b.y) {
        return true;
    }
    let corners = [
        Coord {
            x: r.xmin,
            y: r.ymin,
        },
        Coord {
            x: r.xmax,
            y: r.ymin,
        },
        Coord {
            x: r.xmax,
            y: r.ymax,
        },
        Coord {
            x: r.xmin,
            y: r.ymax,
        },
    ];
    (0..4).any(|i| segments_cross(a, b, corners[i], corners[(i + 1) % 4]))
}

/// Exact intersection test between a geometry and a rectangle (OGR spatial filter semantics).
pub fn intersects_rect(g: &Geometry<f64>, r: &Rect) -> bool {
    if !bbox(g).intersects(r) {
        return false;
    }
    let line_hits = |l: &LineString<f64>| {
        l.0.windows(2).any(|w| segment_hits_rect(w[0], w[1], r))
            || l.0.iter().any(|c| r.contains(c.x, c.y))
    };
    let poly_hits =
        |p: &Polygon<f64>| polygon_rings(p).any(line_hits) || polygon_contains(p, r.xmin, r.ymin);
    match g {
        Geometry::Point(p) => r.contains(p.x(), p.y()),
        Geometry::MultiPoint(m) => m.0.iter().any(|p| r.contains(p.x(), p.y())),
        Geometry::LineString(l) => line_hits(l),
        Geometry::MultiLineString(m) => m.0.iter().any(line_hits),
        Geometry::Polygon(p) => poly_hits(p),
        Geometry::MultiPolygon(m) => m.0.iter().any(poly_hits),
        Geometry::GeometryCollection(gc) => gc.0.iter().any(|g| intersects_rect(g, r)),
        _ => true,
    }
}

pub fn round(v: f64, precision: usize) -> f64 {
    let m = 10f64.powi(precision as i32);
    (v * m).round() / m
}

fn box_ring(b: &Rect) -> LineString<f64> {
    LineString::from(vec![
        (b.xmin, b.ymin),
        (b.xmax, b.ymin),
        (b.xmax, b.ymax),
        (b.xmin, b.ymax),
        (b.xmin, b.ymin),
    ])
}

fn n_coords(g: &Geometry<f64>) -> usize {
    let mut n = 0;
    for_each_coord(g, &mut |_| n += 1);
    n
}

/// generalizeWkbGeometryByBoundingBox: replace a geometry by its bounding box (a 2-point
/// line for lines, a box ring/polygon otherwise) unless it is already that small.
fn generalize_by_box(g: &Geometry<f64>, env: &Rect, ring: bool) -> Geometry<f64> {
    let is_line = matches!(g, Geometry::LineString(_) | Geometry::MultiLineString(_));
    if n_coords(g) <= if is_line { 2 } else { 5 } {
        return g.clone();
    }
    match (is_line, ring) {
        (true, false) => LineString::from(vec![(env.xmin, env.ymin), (env.xmax, env.ymax)]).into(),
        (true, true) => box_ring(env).into(),
        _ => Polygon::new(box_ring(env), vec![]).into(),
    }
}

/// The "Distance" vertex filter of QgsMapToPixelSimplifier::simplifyGeometry for one curve.
fn simplify_curve(l: &LineString<f64>, tol: f64, ring_hint: bool, env: &Rect) -> Geometry<f64> {
    let pts = &l.0;
    let n = pts.len();
    if n == 0 {
        return Geometry::LineString(l.clone());
    }
    let generalizable = n > if ring_hint { 4 } else { 2 };
    let ring = ring_hint && pts[0].x == pts[n - 1].x && pts[0].y == pts[n - 1].y;
    let tol2 = tol * tol;
    let mut out: Vec<Coord<f64>> = Vec::with_capacity(n / 2 + 2);
    let (mut last, mut has_long) = (pts[0], false);
    for (i, c) in pts.iter().enumerate() {
        // QGIS computes the squared length in single precision.
        let (vx, vy) = ((c.x - last.x) as f32, (c.y - last.y) as f32);
        let long = i > 0 && generalizable && ((vx * vx + vy * vy) as f64) > tol2;
        if i == 0 || !generalizable || long || (!ring && (i == 1 || i + 2 >= n)) {
            out.push(*c);
            last = *c;
            has_long |= long;
        }
    }
    if out.len() < if ring { 4 } else { 2 } {
        return if has_long {
            Geometry::LineString(l.clone())
        } else {
            generalize_by_box(&Geometry::LineString(l.clone()), env, ring)
        };
    }
    if ring && (last.x != out[0].x || last.y != out[0].y) {
        out.push(out[0]);
    }
    Geometry::LineString(LineString(out))
}

fn as_line(g: Geometry<f64>, fallback: &LineString<f64>) -> LineString<f64> {
    match g {
        Geometry::LineString(l) => l,
        Geometry::Polygon(p) => p.exterior().clone(),
        _ => fallback.clone(),
    }
}

fn simplify_geom(g: &Geometry<f64>, tol: f64, ring: bool) -> Geometry<f64> {
    let env = bbox(g);
    if !env.is_empty() && env.width() < tol && env.height() < tol {
        return generalize_by_box(g, &env, ring);
    }
    let ring_of = |r: &LineString<f64>| {
        as_line(
            simplify_geom(&Geometry::LineString(r.clone()), tol, true),
            r,
        )
    };
    match g {
        Geometry::LineString(l) => simplify_curve(l, tol, ring, &env),
        Geometry::Polygon(p) => Geometry::Polygon(Polygon::new(
            ring_of(p.exterior()),
            p.interiors().iter().map(ring_of).collect(),
        )),
        Geometry::MultiLineString(m) => Geometry::MultiLineString(
            m.0.iter()
                .map(|l| {
                    as_line(
                        simplify_geom(&Geometry::LineString(l.clone()), tol, false),
                        l,
                    )
                })
                .collect(),
        ),
        Geometry::MultiPolygon(m) => Geometry::MultiPolygon(
            m.0.iter()
                .map(
                    |p| match simplify_geom(&Geometry::Polygon(p.clone()), tol, false) {
                        Geometry::Polygon(s) => s,
                        _ => p.clone(),
                    },
                )
                .collect(),
        ),
        Geometry::GeometryCollection(gc) => Geometry::GeometryCollection(
            gc.0.iter().map(|g| simplify_geom(g, tol, false)).collect(),
        ),
        other => other.clone(),
    }
}

/// QgsMapToPixelSimplifier with SimplifyGeometry | SimplifyEnvelope and the "Distance"
/// algorithm, as QGIS applies it while rendering (`tol` in layer units).
pub fn simplify(g: &Geometry<f64>, tol: f64) -> Geometry<f64> {
    if tol <= 0.0 {
        return g.clone();
    }
    simplify_geom(g, tol, false)
}

/// Sutherland–Hodgman clipping of a ring against an axis-aligned rectangle.
fn clip_ring(ring: &LineString<f64>, r: &Rect) -> Vec<Coord<f64>> {
    let mut pts: Vec<Coord<f64>> = ring.0.clone();
    type Edge = (
        fn(&Coord<f64>, &Rect) -> bool,
        fn(Coord<f64>, Coord<f64>, &Rect) -> Coord<f64>,
    );
    let edges: [Edge; 4] = [
        (
            |c, r| c.x >= r.xmin,
            |a, b, r| Coord {
                x: r.xmin,
                y: a.y + (b.y - a.y) * (r.xmin - a.x) / (b.x - a.x),
            },
        ),
        (
            |c, r| c.x <= r.xmax,
            |a, b, r| Coord {
                x: r.xmax,
                y: a.y + (b.y - a.y) * (r.xmax - a.x) / (b.x - a.x),
            },
        ),
        (
            |c, r| c.y >= r.ymin,
            |a, b, r| Coord {
                x: a.x + (b.x - a.x) * (r.ymin - a.y) / (b.y - a.y),
                y: r.ymin,
            },
        ),
        (
            |c, r| c.y <= r.ymax,
            |a, b, r| Coord {
                x: a.x + (b.x - a.x) * (r.ymax - a.y) / (b.y - a.y),
                y: r.ymax,
            },
        ),
    ];
    for (inside, cross) in edges {
        if pts.is_empty() {
            break;
        }
        let input = std::mem::take(&mut pts);
        let mut prev = input[input.len() - 1];
        for cur in input {
            match (inside(&cur, r), inside(&prev, r)) {
                (true, true) => pts.push(cur),
                (true, false) => {
                    pts.push(cross(prev, cur, r));
                    pts.push(cur);
                }
                (false, true) => pts.push(cross(prev, cur, r)),
                (false, false) => {}
            }
            prev = cur;
        }
    }
    if let (Some(first), Some(last)) = (pts.first().copied(), pts.last().copied())
        && first != last
    {
        pts.push(first);
    }
    pts
}

/// Liang–Barsky clipping of a polyline, split into the pieces inside the rectangle.
fn clip_line(l: &LineString<f64>, r: &Rect, out: &mut Vec<LineString<f64>>) {
    let mut cur: Vec<Coord<f64>> = vec![];
    for w in l.0.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let (mut t0, mut t1) = (0.0f64, 1.0f64);
        let mut visible = true;
        for (p, q) in [
            (-dx, a.x - r.xmin),
            (dx, r.xmax - a.x),
            (-dy, a.y - r.ymin),
            (dy, r.ymax - a.y),
        ] {
            if p == 0.0 {
                if q < 0.0 {
                    visible = false;
                    break;
                }
            } else {
                let t = q / p;
                if p < 0.0 {
                    t0 = t0.max(t);
                } else {
                    t1 = t1.min(t);
                }
            }
        }
        if !visible || t0 > t1 {
            if cur.len() >= 2 {
                out.push(LineString(std::mem::take(&mut cur)));
            }
            cur.clear();
            continue;
        }
        let p0 = Coord {
            x: a.x + t0 * dx,
            y: a.y + t0 * dy,
        };
        let p1 = Coord {
            x: a.x + t1 * dx,
            y: a.y + t1 * dy,
        };
        if cur.last() != Some(&p0) {
            if cur.len() >= 2 {
                out.push(LineString(std::mem::take(&mut cur)));
            }
            cur.clear();
            cur.push(p0);
        }
        cur.push(p1);
        if t1 < 1.0 {
            out.push(LineString(std::mem::take(&mut cur)));
        }
    }
    if cur.len() >= 2 {
        out.push(LineString(cur));
    }
}

/// Clip a geometry to a rectangle for drawing (QGIS `clip_to_extent`). Polygon rings are
/// clipped independently, so `r` should be larger than the visible area to keep the
/// artificial edges along the clip boundary out of view.
/// clip_to_rect for an owned geometry whose bbox `b` is already known: a geometry inside
/// `r` (most features of a dense tile) is returned as is instead of re-scanned and cloned.
pub fn clip_owned(g: Geometry<f64>, b: &Rect, r: &Rect) -> Option<Geometry<f64>> {
    if !b.intersects(r) {
        return None;
    }
    if b.xmin >= r.xmin && b.xmax <= r.xmax && b.ymin >= r.ymin && b.ymax <= r.ymax {
        return Some(g);
    }
    clip_to_rect(&g, r)
}

pub fn clip_to_rect(g: &Geometry<f64>, r: &Rect) -> Option<Geometry<f64>> {
    let b = bbox(g);
    if !b.intersects(r) {
        return None;
    }
    if b.xmin >= r.xmin && b.xmax <= r.xmax && b.ymin >= r.ymin && b.ymax <= r.ymax {
        return Some(g.clone());
    }
    let poly = |p: &Polygon<f64>| -> Option<Polygon<f64>> {
        let ext = clip_ring(p.exterior(), r);
        if ext.len() < 4 {
            return None;
        }
        let holes = p
            .interiors()
            .iter()
            .map(|h| clip_ring(h, r))
            .filter(|h| h.len() >= 4)
            .map(LineString)
            .collect();
        Some(Polygon::new(LineString(ext), holes))
    };
    let out = match g {
        Geometry::Polygon(p) => Geometry::Polygon(poly(p)?),
        Geometry::MultiPolygon(m) => {
            let v: Vec<Polygon<f64>> = m.0.iter().filter_map(poly).collect();
            if v.is_empty() {
                return None;
            }
            Geometry::MultiPolygon(v.into())
        }
        Geometry::LineString(_) | Geometry::MultiLineString(_) => {
            let mut parts = vec![];
            match g {
                Geometry::LineString(l) => clip_line(l, r, &mut parts),
                Geometry::MultiLineString(m) => {
                    m.0.iter().for_each(|l| clip_line(l, r, &mut parts))
                }
                _ => unreachable!(),
            }
            if parts.is_empty() {
                return None;
            }
            Geometry::MultiLineString(geo_types::MultiLineString(parts))
        }
        Geometry::GeometryCollection(gc) => {
            Geometry::GeometryCollection(gc.0.iter().filter_map(|g| clip_to_rect(g, r)).collect())
        }
        other => other.clone(),
    };
    Some(out)
}

/// A rounded JSON number, written as an integer when it has no fraction (as QGIS does).
pub fn jnum(v: f64, precision: usize) -> serde_json::Value {
    let r = round(v, precision);
    if r.fract() == 0.0 && r.abs() < 9e15 {
        serde_json::json!(r as i64)
    } else {
        serde_json::json!(r)
    }
}

pub fn to_geojson(g: &Geometry<f64>, precision: usize) -> serde_json::Value {
    use serde_json::{Value, json};
    let c = |c: &Coord<f64>| json!([jnum(c.x, precision), jnum(c.y, precision)]);
    let ls = |l: &LineString<f64>| Value::Array(l.0.iter().map(c).collect());
    let poly = |p: &Polygon<f64>| Value::Array(polygon_rings(p).map(ls).collect());
    match g {
        Geometry::Point(p) => json!({"type": "Point", "coordinates": c(&p.0)}),
        Geometry::LineString(l) => json!({"type": "LineString", "coordinates": ls(l)}),
        Geometry::Polygon(p) => json!({"type": "Polygon", "coordinates": poly(p)}),
        Geometry::MultiPoint(m) => {
            json!({"type": "MultiPoint", "coordinates": m.0.iter().map(|p| c(&p.0)).collect::<Vec<_>>()})
        }
        Geometry::MultiLineString(m) => {
            json!({"type": "MultiLineString", "coordinates": m.0.iter().map(ls).collect::<Vec<_>>()})
        }
        Geometry::MultiPolygon(m) => {
            json!({"type": "MultiPolygon", "coordinates": m.0.iter().map(poly).collect::<Vec<_>>()})
        }
        Geometry::GeometryCollection(gc) => json!({
            "type": "GeometryCollection",
            "geometries": gc.0.iter().map(|g| to_geojson(g, precision)).collect::<Vec<_>>()
        }),
        Geometry::Line(l) => to_geojson(
            &Geometry::LineString(LineString::from(vec![l.start, l.end])),
            precision,
        ),
        Geometry::Rect(r) => to_geojson(&Geometry::Polygon(r.to_polygon()), precision),
        Geometry::Triangle(t) => to_geojson(&Geometry::Polygon(t.to_polygon()), precision),
    }
}

/// GML 3.1.1 encoding. `swap` writes y before x (lat/lon for EPSG geographic CRSs).
pub fn to_gml3(g: &Geometry<f64>, srs: &str, precision: usize, swap: bool) -> String {
    let pos = |c: &Coord<f64>| {
        let (a, b) = if swap { (c.y, c.x) } else { (c.x, c.y) };
        format!("{} {}", round(a, precision), round(b, precision))
    };
    let pos_list = |l: &LineString<f64>| l.0.iter().map(pos).collect::<Vec<_>>().join(" ");
    let ring = |l: &LineString<f64>| {
        format!(
            "<gml:LinearRing><gml:posList srsDimension=\"2\">{}</gml:posList></gml:LinearRing>",
            pos_list(l)
        )
    };
    let poly = |p: &Polygon<f64>| {
        let mut s = format!(
            "<gml:Polygon><gml:exterior>{}</gml:exterior>",
            ring(p.exterior())
        );
        for i in p.interiors() {
            s += &format!("<gml:interior>{}</gml:interior>", ring(i));
        }
        s + "</gml:Polygon>"
    };
    let line = |l: &LineString<f64>| {
        format!(
            "<gml:LineString><gml:posList srsDimension=\"2\">{}</gml:posList></gml:LineString>",
            pos_list(l)
        )
    };
    let point = |c: &Coord<f64>| {
        format!(
            "<gml:Point><gml:pos srsDimension=\"2\">{}</gml:pos></gml:Point>",
            pos(c)
        )
    };
    let body = match g {
        Geometry::Point(p) => {
            return point(&p.0).replacen(
                "<gml:Point>",
                &format!("<gml:Point srsName=\"{srs}\">"),
                1,
            );
        }
        Geometry::LineString(l) => {
            return line(l).replacen(
                "<gml:LineString>",
                &format!("<gml:LineString srsName=\"{srs}\">"),
                1,
            );
        }
        Geometry::Polygon(p) => {
            return poly(p).replacen(
                "<gml:Polygon>",
                &format!("<gml:Polygon srsName=\"{srs}\">"),
                1,
            );
        }
        Geometry::MultiPoint(m) => (
            "MultiPoint",
            m.0.iter()
                .map(|p| format!("<gml:pointMember>{}</gml:pointMember>", point(&p.0)))
                .collect::<String>(),
        ),
        Geometry::MultiLineString(m) => (
            "MultiCurve",
            m.0.iter()
                .map(|l| format!("<gml:curveMember>{}</gml:curveMember>", line(l)))
                .collect(),
        ),
        Geometry::MultiPolygon(m) => (
            "MultiSurface",
            m.0.iter()
                .map(|p| format!("<gml:surfaceMember>{}</gml:surfaceMember>", poly(p)))
                .collect(),
        ),
        Geometry::GeometryCollection(gc) => (
            "MultiGeometry",
            gc.0.iter()
                .map(|g| {
                    format!(
                        "<gml:geometryMember>{}</gml:geometryMember>",
                        to_gml3(g, srs, precision, swap)
                    )
                })
                .collect(),
        ),
        Geometry::Line(l) => {
            return to_gml3(
                &Geometry::LineString(LineString::from(vec![l.start, l.end])),
                srs,
                precision,
                swap,
            );
        }
        Geometry::Rect(r) => {
            return to_gml3(&Geometry::Polygon(r.to_polygon()), srs, precision, swap);
        }
        Geometry::Triangle(t) => {
            return to_gml3(&Geometry::Polygon(t.to_polygon()), srs, precision, swap);
        }
    };
    format!("<gml:{0} srsName=\"{srs}\">{1}</gml:{0}>", body.0, body.1)
}

/// A polyline (pixel coordinates) moved `dist` to its left, like GEOS offsetCurve with
/// mitre joins and mitre limit 2 as QgsSymbolLayerUtils::offsetLine asks for: each
/// segment shifted along its normal, neighbours joined at their mitre point, cut to a
/// bevel where the mitre would reach beyond `limit * |dist|`. Closed lines stay closed.
pub fn offset_polyline(pts: &[(f64, f64)], dist: f64, limit: f64) -> Vec<(f64, f64)> {
    let mut p: Vec<(f64, f64)> = pts.to_vec();
    p.dedup();
    if p.len() < 2 || dist == 0.0 {
        return p;
    }
    let closed = p.len() > 2 && p.first() == p.last();
    // Segment normals (to the left of the direction of travel) and shifted endpoints.
    let segs: Vec<((f64, f64), (f64, f64))> = p
        .windows(2)
        .map(|w| {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            let len = (dx * dx + dy * dy).sqrt();
            let n = (-dy / len * dist, dx / len * dist);
            ((w[0].0 + n.0, w[0].1 + n.1), (w[1].0 + n.0, w[1].1 + n.1))
        })
        .collect();
    let join = |a: &((f64, f64), (f64, f64)), b: &((f64, f64), (f64, f64)), v: (f64, f64), out: &mut Vec<(f64, f64)>| {
        // Intersection of the two shifted segment lines.
        let (p1, p2, p3, p4) = (a.0, a.1, b.0, b.1);
        let d1 = (p2.0 - p1.0, p2.1 - p1.1);
        let d2 = (p4.0 - p3.0, p4.1 - p3.1);
        let den = d1.0 * d2.1 - d1.1 * d2.0;
        if den.abs() < 1e-12 {
            out.push(p2); // collinear
            return;
        }
        let t = ((p3.0 - p1.0) * d2.1 - (p3.1 - p1.1) * d2.0) / den;
        let m = (p1.0 + t * d1.0, p1.1 + t * d1.1);
        // Mitre length from the original vertex `v`.
        let mitre = ((m.0 - v.0).powi(2) + (m.1 - v.1).powi(2)).sqrt();
        if mitre <= limit * dist.abs() {
            out.push(m);
        } else {
            out.push(p2);
            out.push(p3);
        }
    };
    let mut out = vec![];
    if closed {
        let last = segs.last().expect("non-empty");
        join(last, &segs[0], p[0], &mut out);
    } else {
        out.push(segs[0].0);
    }
    for (i, w) in segs.windows(2).enumerate() {
        join(&w[0], &w[1], p[i + 1], &mut out);
    }
    if closed {
        out.push(out[0]);
    } else {
        out.push(segs.last().expect("non-empty").1);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn offset_polyline_mitres_and_bevels() {
        use super::offset_polyline;
        // GEOS's left normal (-dy, dx) applied in pixel space (y down), as QGIS does: +dist
        // moves a rightward line down the screen.
        let o = offset_polyline(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 2.0, 2.0);
        assert_eq!(o, vec![(0.0, 2.0), (8.0, 2.0), (8.0, 10.0)]);
        // A sharp turn (mitre beyond 2x the distance) is bevelled: two points.
        let o = offset_polyline(&[(0.0, 0.0), (10.0, 0.0), (0.0, 1.0)], 1.0, 2.0);
        assert_eq!(o.len(), 4, "{o:?}");
    }

    use super::*;
    use geo_types::polygon;

    #[test]
    fn hit_testing() {
        let p: Geometry<f64> =
            polygon![(x: 0., y: 0.), (x: 10., y: 0.), (x: 10., y: 10.), (x: 0., y: 10.)].into();
        assert!(hits(&p, 5.0, 5.0, 0.1));
        assert!(!hits(&p, 15.0, 5.0, 0.1));
        assert!(hits(&p, 10.05, 5.0, 0.1));
        assert_eq!(bbox(&p), Rect::new(0., 0., 10., 10.));
    }

    #[test]
    fn clipping_and_simplification() {
        let sq: Geometry<f64> =
            polygon![(x: 0., y: 0.), (x: 10., y: 0.), (x: 10., y: 10.), (x: 0., y: 10.)].into();
        let c = clip_to_rect(&sq, &Rect::new(5., 5., 20., 20.)).unwrap();
        assert_eq!(bbox(&c), Rect::new(5., 5., 10., 10.));
        assert!(clip_to_rect(&sq, &Rect::new(20., 20., 30., 30.)).is_none());
        // A line crossing the rectangle twice is split into the inside pieces.
        let zig: Geometry<f64> = LineString::from(vec![
            (0., 5.),
            (4., 5.),
            (4., 20.),
            (6., 20.),
            (6., 5.),
            (10., 5.),
        ])
        .into();
        match clip_to_rect(&zig, &Rect::new(2., 0., 8., 10.)).unwrap() {
            Geometry::MultiLineString(m) => assert_eq!(m.0.len(), 2),
            other => panic!("{other:?}"),
        }
        // Dense vertices collapse, tiny rings become their bounding box.
        let dense: Geometry<f64> = LineString::from(
            (0..=100)
                .map(|i| (i as f64 * 0.01, 0.0))
                .collect::<Vec<_>>(),
        )
        .into();
        // QGIS keeps the 1st, 2nd and last two vertices of a line; in between, vertices
        // farther than the tolerance from the last kept one: 0, .01, .27, .53, .79, .99, 1
        match simplify(&dense, 0.255) {
            Geometry::LineString(l) => assert_eq!(l.0.len(), 7),
            other => panic!("{other:?}"),
        }
        match simplify(&sq, 50.0) {
            Geometry::Polygon(p) => assert_eq!(p.exterior().0.len(), 5),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exact_rect_intersection() {
        // An L-shaped polygon whose bbox overlaps the query rect but whose area does not.
        let l: Geometry<f64> = polygon![(x: 0., y: 0.), (x: 10., y: 0.), (x: 10., y: 2.), (x: 2., y: 2.), (x: 2., y: 10.), (x: 0., y: 10.)].into();
        assert!(!intersects_rect(&l, &Rect::new(5., 5., 8., 8.)));
        assert!(intersects_rect(&l, &Rect::new(5., 1., 8., 8.)));
        // Rectangle entirely inside the polygon.
        assert!(intersects_rect(&l, &Rect::new(0.5, 0.5, 1.0, 1.0)));
        // Line crossing the rectangle without vertices inside it.
        let line: Geometry<f64> = LineString::from(vec![(0., 5.), (10., 5.)]).into();
        assert!(intersects_rect(&line, &Rect::new(4., 4., 6., 6.)));
    }

    #[test]
    fn gml_axis_swap() {
        let p: Geometry<f64> = geo_types::Point::new(10.0, 60.0).into();
        let s = to_gml3(&p, "urn:ogc:def:crs:EPSG::4326", 8, true);
        assert!(
            s.contains("<gml:pos srsDimension=\"2\">60 10</gml:pos>"),
            "{s}"
        );
    }
}
