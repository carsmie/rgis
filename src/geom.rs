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

#[cfg(test)]
mod tests {
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
