//! The GEOS predicates PAL uses, on plain coordinates: label candidates are (possibly rotated)
//! rectangles, tested against points, lines and polygons (with holes). Boundary contact counts
//! as intersection, as in GEOS.

pub type Pt = (f64, f64);

/// A PAL geometry (pal::PointSet::geos()): a point, a line or a polygon (rings closed,
/// exterior first).
#[derive(Debug, Clone)]
pub enum Shape {
    Point(Pt),
    Line(Vec<Pt>),
    Polygon(Vec<Vec<Pt>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Location {
    Inside,
    Boundary,
    Outside,
}

fn orient(a: Pt, b: Pt, c: Pt) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

fn on_segment(a: Pt, b: Pt, p: Pt) -> bool {
    p.0 >= a.0.min(b.0) && p.0 <= a.0.max(b.0) && p.1 >= a.1.min(b.1) && p.1 <= a.1.max(b.1)
}

/// Closed segments a–b and c–d share at least one point.
pub fn segments_intersect(a: Pt, b: Pt, c: Pt, d: Pt) -> bool {
    let d1 = orient(c, d, a);
    let d2 = orient(c, d, b);
    let d3 = orient(a, b, c);
    let d4 = orient(a, b, d);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0)) {
        return true;
    }
    (d1 == 0.0 && on_segment(c, d, a))
        || (d2 == 0.0 && on_segment(c, d, b))
        || (d3 == 0.0 && on_segment(a, b, c))
        || (d4 == 0.0 && on_segment(a, b, d))
}

fn ring_location(ring: &[Pt], p: Pt) -> Location {
    let mut inside = false;
    let n = ring.len();
    if n < 2 {
        return Location::Outside;
    }
    for i in 0..n - 1 {
        let (a, b) = (ring[i], ring[i + 1]);
        if orient(a, b, p) == 0.0 && on_segment(a, b, p) {
            return Location::Boundary;
        }
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
    }
    if inside {
        Location::Inside
    } else {
        Location::Outside
    }
}

fn polygon_location(rings: &[Vec<Pt>], p: Pt) -> Location {
    let Some(ext) = rings.first() else {
        return Location::Outside;
    };
    match ring_location(ext, p) {
        Location::Inside => {}
        other => return other,
    }
    for hole in &rings[1..] {
        match ring_location(hole, p) {
            Location::Inside => return Location::Outside,
            Location::Boundary => return Location::Boundary,
            Location::Outside => {}
        }
    }
    Location::Inside
}

/// GEOSPreparedContainsProperly(polygon, point): strictly inside.
pub fn polygon_contains_properly(rings: &[Vec<Pt>], p: Pt) -> bool {
    polygon_location(rings, p) == Location::Inside
}

/// Point in polygon including the boundary.
pub fn polygon_covers(rings: &[Vec<Pt>], p: Pt) -> bool {
    polygon_location(rings, p) != Location::Outside
}

fn quad_ring(q: &[Pt; 4]) -> [Pt; 5] {
    [q[0], q[1], q[2], q[3], q[0]]
}

fn edges(ring: &[Pt]) -> impl Iterator<Item = (Pt, Pt)> + '_ {
    ring.windows(2).map(|w| (w[0], w[1]))
}

fn quad_covers(q: &[Pt; 4], p: Pt) -> bool {
    let r = quad_ring(q);
    ring_location(&r, p) != Location::Outside
}

/// GEOSIntersects(shape, quad).
pub fn quad_intersects(q: &[Pt; 4], shape: &Shape) -> bool {
    let qr = quad_ring(q);
    match shape {
        Shape::Point(p) => quad_covers(q, *p),
        Shape::Line(line) => {
            if line.first().is_some_and(|p| quad_covers(q, *p)) {
                return true;
            }
            edges(line).any(|(a, b)| edges(&qr).any(|(c, d)| segments_intersect(a, b, c, d)))
        }
        Shape::Polygon(rings) => {
            if rings.is_empty() {
                return false;
            }
            if polygon_covers(rings, q[0]) {
                return true;
            }
            for ring in rings {
                if ring.first().is_some_and(|p| quad_covers(q, *p)) {
                    return true;
                }
                if edges(ring).any(|(a, b)| edges(&qr).any(|(c, d)| segments_intersect(a, b, c, d))) {
                    return true;
                }
            }
            false
        }
    }
}

/// GEOSPreparedContains(polygon, quad): every point of the quad lies in the polygon
/// (boundary contact allowed) — corners covered and no polygon ring crossing into it.
pub fn polygon_contains_quad(rings: &[Vec<Pt>], q: &[Pt; 4]) -> bool {
    if !q.iter().all(|p| polygon_covers(rings, *p)) {
        return false;
    }
    // A hole (or exterior vertex) strictly inside the quad breaks containment.
    let qr = quad_ring(q);
    for ring in rings {
        for p in ring {
            if ring_location(&qr, *p) == Location::Inside {
                return false;
            }
        }
    }
    true
}

/// Axis-aligned rectangle contains the quad (inclusive), for map boundaries.
pub fn rect_contains_quad(r: (f64, f64, f64, f64), q: &[Pt; 4]) -> bool {
    q.iter().all(|p| p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3)
}

fn seg_dist2(p: Pt, a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (x, y) = (a.0 + t * dx, a.1 + t * dy);
    (p.0 - x).powi(2) + (p.1 - y).powi(2)
}

/// Squared distance from a point to a line string (or ring) (PointSet::minDistanceToPoint).
pub fn min_dist2_to_line(line: &[Pt], p: Pt) -> f64 {
    if line.len() == 1 {
        return (p.0 - line[0].0).powi(2) + (p.1 - line[0].1).powi(2);
    }
    edges(line).map(|(a, b)| seg_dist2(p, a, b)).fold(f64::MAX, f64::min)
}

/// Centroid like GEOSGetCentroid: area-weighted for polygons (holes subtract), length-weighted
/// for lines.
pub fn centroid(shape: &Shape) -> Pt {
    match shape {
        Shape::Point(p) => *p,
        Shape::Line(l) => {
            let (mut sx, mut sy, mut total) = (0.0, 0.0, 0.0);
            for (a, b) in edges(l) {
                let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
                sx += len * (a.0 + b.0) / 2.0;
                sy += len * (a.1 + b.1) / 2.0;
                total += len;
            }
            if total > 0.0 {
                (sx / total, sy / total)
            } else {
                l.first().copied().unwrap_or((0.0, 0.0))
            }
        }
        Shape::Polygon(rings) => {
            // GEOS Centroid: triangles fanned from the first exterior vertex.
            let Some(base) = rings.first().and_then(|r| r.first()).copied() else {
                return (0.0, 0.0);
            };
            let (mut cx, mut cy, mut area) = (0.0, 0.0, 0.0);
            for (i, ring) in rings.iter().enumerate() {
                // Exterior counted clockwise-positive, holes with the opposite sign.
                let ring_area: f64 = edges(ring).map(|(a, b)| a.0 * b.1 - b.0 * a.1).sum::<f64>() / 2.0;
                let is_cw = ring_area < 0.0;
                let sign = if (i == 0) == is_cw { 1.0 } else { -1.0 };
                for (a, b) in edges(ring) {
                    let a2 = sign * ((a.0 - base.0) * (b.1 - base.1) - (b.0 - base.0) * (a.1 - base.1));
                    cx += a2 * (base.0 + a.0 + b.0);
                    cy += a2 * (base.1 + a.1 + b.1);
                    area += a2;
                }
            }
            if area == 0.0 {
                return base;
            }
            (cx / 3.0 / area, cy / 3.0 / area)
        }
    }
}

/// |area| of the exterior ring minus holes.
pub fn polygon_area(rings: &[Vec<Pt>]) -> f64 {
    let ring = |r: &Vec<Pt>| (edges(r).map(|(a, b)| a.0 * b.1 - b.0 * a.1).sum::<f64>() / 2.0).abs();
    let mut a = rings.first().map(ring).unwrap_or(0.0);
    for h in rings.iter().skip(1) {
        a -= ring(h);
    }
    a.abs()
}

pub fn line_length(l: &[Pt]) -> f64 {
    edges(l).map(|(a, b)| ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates() {
        let sq = vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]];
        assert!(polygon_contains_properly(&sq, (5.0, 5.0)));
        assert!(!polygon_contains_properly(&sq, (10.0, 5.0)));
        let q = [(9.0, 9.0), (12.0, 9.0), (12.0, 12.0), (9.0, 12.0)];
        assert!(quad_intersects(&q, &Shape::Polygon(sq.clone())));
        assert!(!polygon_contains_quad(&sq, &q));
        let inner = [(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 2.0)];
        assert!(polygon_contains_quad(&sq, &inner));
        assert_eq!(centroid(&Shape::Polygon(sq)), (5.0, 5.0));
        assert!(quad_intersects(&q, &Shape::Point((10.0, 10.0))));
        assert!(!quad_intersects(&q, &Shape::Line(vec![(0.0, 0.0), (5.0, 5.0)])));
    }
}
