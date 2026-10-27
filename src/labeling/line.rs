//! Line geometry for line labels (pal::PointSet helpers) and the curved text layout
//! (QgsTextRendererUtils::generateCurvedTextPlacement), ported from QGIS. Coordinates are map
//! units, y up, as in PAL.

use std::f64::consts::{FRAC_PI_2, PI};

use super::shape::Pt;

/// PointSet::length / GEOSLength.
pub fn length(l: &[Pt]) -> f64 {
    l.windows(2)
        .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
        .sum()
}

/// PointSet::edgeDistances: per-vertex distance from the previous vertex, and the total.
pub fn edge_distances(l: &[Pt]) -> (Vec<f64>, f64) {
    let mut d = vec![0.0; l.len()];
    let mut total = 0.0;
    for i in 1..l.len() {
        d[i] = ((l[i - 1].0 - l[i].0).powi(2) + (l[i - 1].1 - l[i].1).powi(2)).sqrt();
        total += d[i];
    }
    (d, total)
}

/// GEOSInterpolate: the point `distance` along the line (clamped to its ends).
pub fn interpolate_point(l: &[Pt], distance: f64) -> Option<Pt> {
    let first = *l.first()?;
    if distance <= 0.0 {
        return Some(first);
    }
    let mut walked = 0.0;
    for w in l.windows(2) {
        let seg = (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1);
        if walked + seg >= distance && seg > 0.0 {
            let t = (distance - walked) / seg;
            return Some((
                w[0].0 + t * (w[1].0 - w[0].0),
                w[0].1 + t * (w[1].1 - w[0].1),
            ));
        }
        walked += seg;
    }
    l.last().copied()
}

/// GEOSProject: distance along the line of the point closest to `p` (first on ties).
pub fn line_locate_point(l: &[Pt], p: Pt) -> f64 {
    let (mut best, mut best_d2, mut walked) = (0.0, f64::MAX, 0.0);
    for w in l.windows(2) {
        let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
        let len2 = dx * dx + dy * dy;
        let seg = len2.sqrt();
        let t = if len2 > 0.0 {
            (((p.0 - w[0].0) * dx + (p.1 - w[0].1) * dy) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (cx, cy) = (w[0].0 + t * dx, w[0].1 + t * dy);
        let d2 = (p.0 - cx).powi(2) + (p.1 - cy).powi(2);
        if d2 < best_d2 {
            best_d2 = d2;
            best = walked + t * seg;
        }
        walked += seg;
    }
    best
}

/// QgsGeometryUtils::interpolatePointOnLine.
fn point_on_line(x1: f64, y1: f64, x2: f64, y2: f64, f: f64) -> Pt {
    (x1 + (x2 - x1) * f, y1 + (y2 - y1) * f)
}

/// PointSet::extendLineByDistance. The end trace starts from the (new) start point, as in
/// QGIS; with the smoothing distances labels use this only matters for very short lines.
pub fn extend_line_by_distance(l: &mut Vec<Pt>, start: f64, end: f64, smooth: f64) {
    let n = l.len();
    if n < 2 {
        return;
    }
    let (mut x0, mut y0) = l[0];
    if start > 0.0 {
        let (mut x1, mut y1) = l[1];
        let mut consumed = 0.0;
        let (mut lx, mut ly) = (x0, y0);
        for &(tx, ty) in &l[1..] {
            let seg = (tx - lx).hypot(ty - ly);
            consumed += seg;
            if consumed >= smooth {
                let c = (consumed - smooth) / seg;
                x1 = lx + c * (tx - lx);
                y1 = ly + c * (ty - ly);
                break;
            }
            lx = tx;
            ly = ty;
        }
        let d = (x1 - x0).hypot(y1 - y0);
        (x0, y0) = point_on_line(x1, y1, x0, y0, (start + d) / d);
    }
    if end > 0.0 {
        let (xe0, ye0) = l[n - 1];
        let (mut xe1, mut ye1) = l[n - 2];
        let mut consumed = 0.0;
        let (mut lx, mut ly) = (x0, y0);
        for i in (0..n - 1).rev() {
            let (tx, ty) = l[i];
            let seg = (tx - lx).hypot(ty - ly);
            consumed += seg;
            if consumed >= smooth {
                let c = (consumed - smooth) / seg;
                xe1 = lx + c * (tx - lx);
                ye1 = ly + c * (ty - ly);
                break;
            }
            lx = tx;
            ly = ty;
        }
        let d = (xe1 - xe0).hypot(ye1 - ye0);
        l.push(point_on_line(xe1, ye1, xe0, ye0, (end + d) / d));
    }
    if start > 0.0 {
        l.insert(0, (x0, y0));
    }
}

/// GEOSOffsetCurve(line, distance, quadsegs 0, JOIN_MITRE, mitre limit 2): positive distances
/// offset to the left of the line direction. Outer corners get the mitre point (bevelled
/// beyond the limit), inner corners the intersection of the offset segments.
pub fn offset_curve(l: &[Pt], distance: f64) -> Vec<Pt> {
    let mut pts: Vec<Pt> = Vec::with_capacity(l.len());
    for &p in l {
        if pts.last().is_none_or(|q: &Pt| q.0 != p.0 || q.1 != p.1) {
            pts.push(p);
        }
    }
    if pts.len() < 2 || distance == 0.0 {
        return pts;
    }
    let segs: Vec<(Pt, Pt, Pt)> = pts
        .windows(2)
        .map(|w| {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            let len = dx.hypot(dy);
            let (nx, ny) = (-dy / len * distance, dx / len * distance);
            (
                (w[0].0 + nx, w[0].1 + ny),
                (w[1].0 + nx, w[1].1 + ny),
                (dx / len, dy / len),
            )
        })
        .collect();
    let mut out = vec![segs[0].0];
    for i in 1..segs.len() {
        let (a0, a1, da) = segs[i - 1];
        let (b0, _, db) = segs[i];
        let cross = da.0 * db.1 - da.1 * db.0;
        let dot = da.0 * db.0 + da.1 * db.1;
        if cross.abs() < 1e-10 && dot > 0.0 {
            out.push(a1);
            continue;
        }
        // Intersection of the two offset lines.
        let denom = da.0 * db.1 - da.1 * db.0;
        let mitre = if denom.abs() > 1e-12 {
            let t = ((b0.0 - a0.0) * db.1 - (b0.1 - a0.1) * db.0) / denom;
            Some((a0.0 + t * da.0, a0.1 + t * da.1))
        } else {
            None
        };
        // Left offsets are outside right turns (cross < 0), right offsets outside left turns.
        let outer = (distance > 0.0) == (cross < 0.0);
        let vertex = pts[i];
        match mitre {
            Some(m) if !outer => out.push(m),
            Some(m) if (m.0 - vertex.0).hypot(m.1 - vertex.1) <= 2.0 * distance.abs() => {
                out.push(m)
            }
            _ => {
                // OffsetSegmentGenerator::addLimitedMitreJoin: a bevel perpendicular to the
                // outer bisector at the mitre limit distance, cut by both offset lines.
                let (ox, oy) = (da.0 - db.0, da.1 - db.1);
                let ol = ox.hypot(oy);
                if ol == 0.0 {
                    out.push(a1);
                    out.push(b0);
                    continue;
                }
                let u = (ox / ol, oy / ol);
                let lim = 2.0 * distance.abs();
                let mid = (vertex.0 + u.0 * lim, vertex.1 + u.1 * lim);
                let cut = |p: Pt, d: Pt| -> Pt {
                    let den = d.0 * u.0 + d.1 * u.1;
                    if den.abs() < 1e-12 {
                        return p;
                    }
                    let t = ((mid.0 - p.0) * u.0 + (mid.1 - p.1) * u.1) / den;
                    (p.0 + t * d.0, p.1 + t * d.1)
                };
                out.push(cut(a1, da));
                out.push(cut(b0, db));
            }
        }
    }
    out.push(segs[segs.len() - 1].1);
    out
}

/// Per-grapheme metrics of a curved label (QgsPrecalculatedTextMetrics), in map units.
#[derive(Debug, Clone, Default)]
pub struct TextMetrics {
    pub graphemes: Vec<String>,
    pub widths: Vec<f64>,
    pub heights: Vec<f64>,
    pub descents: Vec<f64>,
}

impl TextMetrics {
    pub fn count(&self) -> usize {
        self.graphemes.len()
    }
    fn max_height(&self) -> f64 {
        self.heights.iter().copied().fold(0.0, f64::max)
    }
    fn max_descent(&self) -> f64 {
        self.descents.iter().copied().fold(0.0, f64::max)
    }
}

/// One placed grapheme: lower-left corner, size and angle (CCW radians), map units.
#[derive(Debug, Clone, Copy)]
pub struct GraphemePlacement {
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub angle: f64,
    pub is_whitespace: bool,
}

#[derive(Debug, Default)]
pub struct CurvePlacement {
    pub graphemes: Vec<GraphemePlacement>,
    pub upside_down_count: usize,
    pub right_to_left: bool,
    pub flipped: bool,
}

/// QgsTextRendererUtils::LabelLineDirection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    FollowLine,
    RespectPainterOrientation,
}

/// QgsTextRendererUtils::findLineCircleIntersection.
#[allow(clippy::too_many_arguments)]
fn line_circle_intersection(
    cx: f64,
    cy: f64,
    radius: f64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    res: &mut Pt,
) {
    let m = if radius < 10.0 { 10000.0 } else { 1.0 };
    let (cx, cy, radius, x1, y1, x2, y2) =
        (cx * m, cy * m, radius * m, x1 * m, y1 * m, x2 * m, y2 * m);
    let (dx, dy) = (x2 - x1, y2 - y1);
    let a = dx * dx + dy * dy;
    let b = 2.0 * (dx * (x1 - cx) + dy * (y1 - cy));
    let c = (x1 - cx).powi(2) + (y1 - cy).powi(2) - radius * radius;
    let det = b * b - 4.0 * a * c;
    if a <= 0.000000000001 || det < 0.0 {
        return;
    }
    let t = if (det - 0.0).abs() <= 4.0 * f64::EPSILON {
        -b / (2.0 * a)
    } else {
        (-b + det.sqrt()) / (2.0 * a)
    };
    *res = ((x1 + t * dx) / m, (y1 + t * dy) / m);
}

/// QgsTextRendererUtils::nextCharPosition. Returns the character start and end.
#[allow(clippy::too_many_arguments)]
fn next_char_position(
    char_width: f64,
    dists: &[f64],
    x: &[f64],
    y: &[f64],
    index: &mut usize,
    along: &mut f64,
    extend: bool,
) -> Option<(Pt, Pt)> {
    let n = x.len();
    if *index >= n || near0(dists[*index]) {
        return None;
    }
    let (mut sx, mut sy) = (x[*index - 1], y[*index - 1]);
    let (mut ex, mut ey) = (x[*index], y[*index]);
    let seg_len = dists[*index];
    let (sdx, sdy) = (ex - sx, ey - sy);
    let start = (sx + sdx * *along / seg_len, sy + sdy * *along / seg_len);
    let mut end = (0.0, 0.0);
    if seg_len - *along >= char_width {
        *along += char_width;
        end = (sx + sdx * *along / seg_len, sy + sdy * *along / seg_len);
    } else {
        loop {
            *index += 1;
            if *index >= n {
                if extend {
                    let (ldx, ldy) = (ex - sx, ey - sy);
                    let l = ldx.hypot(ldy);
                    if near0(l) {
                        return None;
                    }
                    ex = sx + ldx / l * char_width;
                    ey = sy + ldy / l * char_width;
                    *index -= 1;
                    break;
                }
                return None;
            }
            sx = ex;
            sy = ey;
            ex = x[*index];
            ey = y[*index];
            if ((start.0 - ex).powi(2) + (start.1 - ey).powi(2)).sqrt() >= char_width {
                break;
            }
        }
        line_circle_intersection(start.0, start.1, char_width, sx, sy, ex, ey, &mut end);
        *along = ((sx - end.0).powi(2) + (sy - end.1).powi(2)).sqrt();
    }
    Some((start, end))
}

fn near0(v: f64) -> bool {
    v.abs() <= 4.0 * f64::EPSILON
}

/// QgsTextRendererUtils::generateCurvedTextPlacement (start-of-text anchor, no extra
/// character/word spacing). Angle limits < 0 disable the checks.
#[allow(clippy::too_many_arguments)]
pub fn curved_text_placement(
    m: &TextMetrics,
    line: &[Pt],
    dists: &[f64],
    offset_along_line: f64,
    direction: Direction,
    max_concave: f64,
    max_convex: f64,
    upright_only: bool,
    second_attempt: bool,
) -> CurvePlacement {
    let mut out = CurvePlacement::default();
    let x: Vec<f64> = line.iter().map(|p| p.0).collect();
    let y: Vec<f64> = line.iter().map(|p| p.1).collect();
    let n = x.len();
    let mut along = offset_along_line;
    let mut index = 1;
    while index < n && along > dists[index] {
        along -= dists[index];
        index += 1;
    }
    if index >= n || near0(dists[index]) {
        return out;
    }
    let count = m.count();
    if direction == Direction::RespectPainterOrientation && !second_attempt {
        let (mut d, mut end_index) = (along, index);
        let (mut start, mut end) = ((0.0, 0.0), (0.0, 0.0));
        for i in 0..count {
            let Some((s, e)) =
                next_char_position(m.widths[i], dists, &x, &y, &mut end_index, &mut d, false)
            else {
                return out;
            };
            if i == 0 {
                start = s;
            }
            end = e;
        }
        let line_angle = (-(end.1 - start.1)).atan2(end.0 - start.0) * 180.0 / PI;
        if !(-90.0..=90.0).contains(&line_angle) {
            out.right_to_left = true;
        }
    }
    if second_attempt {
        out.right_to_left = false;
        out.flipped = true;
    }
    let mut angle = (-(y[index] - y[index - 1])).atan2(x[index] - x[index - 1]);
    let (max_h, max_desc) = (m.max_height(), m.max_descent());
    for i in 0..count {
        let last_angle = angle;
        let k = if out.flipped { count - i - 1 } else { i };
        let w = m.widths[k];
        if near0(w) {
            continue;
        }
        let Some((mut s, e)) = next_char_position(w, dists, &x, &y, &mut index, &mut along, false)
        else {
            out.graphemes.clear();
            return out;
        };
        angle = (s.1 - e.1).atan2(e.0 - s.0);
        if max_concave >= 0.0 || max_convex >= 0.0 {
            let mut delta = last_angle - angle;
            while delta > PI {
                delta -= 2.0 * PI;
            }
            while delta < -PI {
                delta += 2.0 * PI;
            }
            if (max_concave >= 0.0 && delta > 0.0 && delta > max_concave)
                || (max_convex >= 0.0 && delta < 0.0 && delta < -max_convex)
            {
                out.graphemes.clear();
                return out;
            }
        }
        // Shift down: the draw position is the baseline, the path is the mean line.
        let mut dist = 0.9 * max_h / 2.0 - (max_desc - m.descents[k]);
        if out.flipped {
            dist = -dist;
        }
        s.0 += dist * (angle + FRAC_PI_2).cos();
        s.1 -= dist * (angle + FRAC_PI_2).sin();
        let mut render = angle;
        let index_out = if out.flipped { count - i - 1 } else { i };
        let g = &m.graphemes[index_out];
        let mut p = GraphemePlacement {
            index: index_out,
            x: s.0,
            y: s.1,
            width: w,
            height: m.heights[k],
            angle: 0.0,
            is_whitespace: g.is_empty() || g.starts_with(char::is_whitespace),
        };
        if out.flipped {
            p.x += w * render.cos();
            p.y -= w * render.sin();
            render += PI;
        }
        p.angle = -render;
        out.graphemes.push(p);
        while render >= 2.0 * PI {
            render -= 2.0 * PI;
        }
        while render < 0.0 {
            render += 2.0 * PI;
        }
        if render > FRAC_PI_2 && render < 1.5 * PI {
            out.upside_down_count += 1;
        }
    }
    if !second_attempt && upright_only && out.upside_down_count as f64 >= count as f64 / 2.0 {
        return curved_text_placement(
            m,
            line,
            dists,
            offset_along_line,
            direction,
            max_concave,
            max_convex,
            upright_only,
            true,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_left_and_right() {
        let l = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        let left = offset_curve(&l, 1.0);
        assert_eq!(left, vec![(0.0, 1.0), (9.0, 1.0), (9.0, 10.0)]);
        let right = offset_curve(&l, -1.0);
        assert_eq!(right, vec![(0.0, -1.0), (11.0, -1.0), (11.0, 10.0)]);
    }

    #[test]
    fn locates_and_interpolates() {
        let l = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
        assert_eq!(interpolate_point(&l, 15.0), Some((10.0, 5.0)));
        assert!((line_locate_point(&l, (12.0, 4.0)) - 14.0).abs() < 1e-12);
        let mut e = l.to_vec();
        extend_line_by_distance(&mut e, 2.0, 0.0, 0.0);
        assert_eq!(e[0], (-2.0, 0.0));
    }

    #[test]
    fn places_characters_along_a_straight_line() {
        let m = TextMetrics {
            graphemes: vec!["a".into(), "b".into()],
            widths: vec![2.0, 3.0],
            heights: vec![4.0, 4.0],
            descents: vec![1.0, 1.0],
        };
        let line = [(0.0, 0.0), (10.0, 0.0)];
        let (d, _) = edge_distances(&line);
        let p = curved_text_placement(
            &m,
            &line,
            &d,
            1.0,
            Direction::FollowLine,
            -1.0,
            -1.0,
            true,
            false,
        );
        assert_eq!(p.graphemes.len(), 2);
        assert!((p.graphemes[0].x - 1.0).abs() < 1e-9 && (p.graphemes[1].x - 3.0).abs() < 1e-9);
        // Mean line on the path: lower-left corner 0.9 · h / 2 below it.
        assert!((p.graphemes[0].y + 1.8).abs() < 1e-9);
        assert_eq!(p.upside_down_count, 0);
        // Right to left: placed flipped, so the text still reads left to right.
        let line = [(10.0, 0.0), (0.0, 0.0)];
        let p = curved_text_placement(
            &m,
            &line,
            &d,
            1.0,
            Direction::FollowLine,
            -1.0,
            -1.0,
            true,
            false,
        );
        assert!(p.flipped);
        assert!(p.graphemes[0].index == 1);
    }
}
