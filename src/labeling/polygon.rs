//! PAL's polygon geometry helpers (pal::GeomFunction / pal::PointSet), ported as QGIS has them
//! so free/horizontal polygon candidates land where QGIS puts them: its heapsorts (unstable,
//! so ties order the same), convex hull, splitPolygons and the oriented hull bounding box.

/// pal util.h EPSILON.
const EPSILON: f64 = 1e-9;

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= 4.0 * f64::EPSILON
}

/// GeomFunction::cross_product.
pub fn cross_product(x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) -> f64 {
    (x2 - x1) * (y3 - y1) - (x3 - x1) * (y2 - y1)
}

/// GeomFunction::isSegIntersects (strict crossing).
#[allow(clippy::too_many_arguments)]
fn is_seg_intersects(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    x3: f64,
    y3: f64,
    x4: f64,
    y4: f64,
) -> bool {
    cross_product(x1, y1, x2, y2, x3, y3) * cross_product(x1, y1, x2, y2, x4, y4) < 0.0
        && cross_product(x3, y3, x4, y4, x1, y1) * cross_product(x3, y3, x4, y4, x2, y2) < 0.0
}

/// GeomFunction::computeLineIntersection (infinite lines).
#[allow(clippy::too_many_arguments)]
fn line_intersection(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    x3: f64,
    y3: f64,
    x4: f64,
    y4: f64,
) -> Option<(f64, f64)> {
    let (a1, b1, c1) = (y2 - y1, x1 - x2, x2 * y1 - x1 * y2);
    let (a2, b2, c2) = (y4 - y3, x3 - x4, x4 * y3 - x3 * y4);
    let denom = a1 * b2 - a2 * b1;
    if near(denom, 0.0) {
        return None;
    }
    Some(((b1 * c2 - b2 * c1) / denom, (a2 * c1 - a1 * c2) / denom))
}

/// geomfunction.cpp heapsort: sort `sid[..n]` by `key[sid[i]]` ascending (ids are identity).
fn heapsort(sid: &mut [usize], key: &[f64], n: usize) {
    let mut n = n;
    let mut i = n / 2;
    loop {
        let tx;
        if i > 0 {
            i -= 1;
            tx = sid[i];
        } else {
            n -= 1;
            if n == 0 {
                return;
            }
            tx = sid[n];
            sid[n] = sid[0];
        }
        let mut parent = i;
        let mut child = i * 2 + 1;
        while child < n {
            if child + 1 < n && key[sid[child + 1]] > key[sid[child]] {
                child += 1;
            }
            if key[sid[child]] > key[tx] {
                sid[parent] = sid[child];
                parent = child;
                child = parent * 2 + 1;
            } else {
                break;
            }
        }
        sid[parent] = tx;
    }
}

/// geomfunction.cpp heapsort2: sort `heap` ascending, permuting `x` alongside.
fn heapsort2(x: &mut [usize], heap: &mut [f64]) {
    let mut n = heap.len();
    let mut i = n / 2;
    loop {
        let (t, tx);
        if i > 0 {
            i -= 1;
            t = heap[i];
            tx = x[i];
        } else {
            n -= 1;
            if n == 0 {
                return;
            }
            t = heap[n];
            tx = x[n];
            heap[n] = heap[0];
            x[n] = x[0];
        }
        let mut parent = i;
        let mut child = i * 2 + 1;
        while child < n {
            if child + 1 < n && heap[child + 1] > heap[child] {
                child += 1;
            }
            if heap[child] > t {
                heap[parent] = heap[child];
                x[parent] = x[child];
                parent = child;
                child = parent * 2 + 1;
            } else {
                break;
            }
        }
        heap[parent] = t;
        x[parent] = tx;
    }
}

/// GeomFunction::convexHullId (Graham scan) over all points; indices into x/y.
pub fn convex_hull(x: &[f64], y: &[f64]) -> Vec<usize> {
    let n = x.len();
    let mut hull: Vec<usize> = (0..n).collect();
    if n <= 3 {
        return hull;
    }
    let mut stack = vec![0usize; n];
    let mut tan = vec![0.0f64; n];
    heapsort(&mut hull, y, n);
    let mut r = 1;
    while r < n && near(y[hull[r]], y[hull[0]]) {
        r += 1;
    }
    heapsort(&mut hull, x, r);
    for i in r..n {
        tan[i] = if near(y[hull[i]], y[hull[0]]) {
            f32::MAX as f64
        } else {
            (x[hull[0]] - x[hull[i]]) / (y[hull[i]] - y[hull[0]])
        };
    }
    if r < n {
        heapsort2(&mut hull[r..], &mut tan[r..]);
    }
    stack[0] = hull[0];
    if r == 1 {
        stack[1] = hull[1];
        r += 1;
    } else {
        stack[1] = hull[r - 1];
    }
    let (mut top, mut second) = (1usize, 0usize);
    let cp = |a: usize, b: usize, c: usize| cross_product(x[a], y[a], x[b], y[b], x[c], y[c]);
    let d2 = |a: usize, b: usize| (x[b] - x[a]).powi(2) + (y[b] - y[a]).powi(2);
    for &h in &hull[r..n] {
        let mut result = cp(stack[second], stack[top], h);
        if near(result, 0.0) {
            // Collinear: keep the farthest.
            if d2(stack[second], h) > d2(stack[second], stack[top]) {
                stack[top] = h;
            }
        } else if result > 0.0 {
            second += 1;
            top += 1;
            stack[top] = h;
        } else {
            while result < 0.0 && top > 1 {
                second -= 1;
                top -= 1;
                result = cp(stack[second], stack[top], h);
            }
            second += 1;
            top += 1;
            stack[top] = h;
        }
    }
    stack.truncate(top + 1);
    stack
}

/// GeomFunction::reorderPolygon: orient the ring by its convex hull; false = bad hull
/// (PAL then drops the part).
pub fn reorder_polygon(x: &mut [f64], y: &mut [f64]) -> bool {
    let h = convex_hull(x, y);
    if h.len() < 3 {
        return false;
    }
    let (a, b, c) = (h[0], h[1], h[2]);
    let inc = if (a < b && b < c) || (a > b && b < c && c < a) || (a < b && b > c && c < a) {
        1
    } else if (a > b && b > c) || (a > b && b < c && c > a) || (a < b && b > c && c > a) {
        -1
    } else {
        return false;
    };
    if inc == -1 {
        x.reverse();
        y.reverse();
    }
    true
}

/// A pal::PointSet produced by splitPolygons, with the convex hull it computed.
pub struct SplitShape {
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    pub hull: Vec<usize>,
}

/// PointSet::extractShape: points imin..=imax (wrapping), plus the cut point when new.
#[allow(clippy::too_many_arguments)]
fn extract_shape(
    x: &[f64],
    y: &[f64],
    nb: usize,
    imin: usize,
    imax: usize,
    fps: usize,
    fpe: usize,
    fpt: (f64, f64),
) -> (Vec<f64>, Vec<f64>) {
    let n = x.len();
    let (mut nx, mut ny) = (vec![0.0; nb], vec![0.0; nb]);
    let (mut i, mut j) = (imin, 0usize);
    while i != (imax + 1) % n {
        if j < nb {
            nx[j] = x[i];
            ny[j] = y[i];
        }
        i = (i + 1) % n;
        j += 1;
    }
    if fps != fpe && j < nb {
        nx[j] = fpt.0;
        ny[j] = fpt.1;
    }
    (nx, ny)
}

/// PointSet::splitPolygons: cut concave shapes at their deepest "holes" (bays between the
/// ring and its hull) larger than the label, until convex enough.
pub fn split_polygons(x: Vec<f64>, y: Vec<f64>, label_w: f64, label_h: f64) -> Vec<SplitShape> {
    let label_area = label_w * label_h;
    let mut input: std::collections::VecDeque<(Vec<f64>, Vec<f64>)> = [(x, y)].into();
    let mut output = Vec::new();
    let dist = |ax: f64, ay: f64, bx: f64, by: f64| (bx - ax).hypot(by - ay);
    let sqr = |ax: f64, ay: f64, bx: f64, by: f64| (bx - ax).powi(2) + (by - ay).powi(2);
    while let Some((x, y)) = input.pop_front() {
        let nbp = x.len();
        let hull = convex_hull(&x, &y);
        let hn = hull.len();
        let mut best_area = 0.0;
        let mut retained: Option<usize> = None;
        let (mut hole_s, mut hole_e) = (0usize, 0usize);
        for ihs in 0..hn {
            let ihn = (ihs + 1) % hn;
            let ips = hull[ihs];
            let ipn = (ips + 1) % nbp;
            if ipn == hull[ihn] {
                continue;
            }
            // A bay: its deepest point.
            let (mut bestcp, mut pt) = (0.0, None);
            let mut i = ips;
            while i != hull[ihn] {
                let cp = cross_product(
                    x[hull[ihs]],
                    y[hull[ihs]],
                    x[hull[ihn]],
                    y[hull[ihn]],
                    x[i],
                    y[i],
                )
                .abs();
                if cp - bestcp > EPSILON {
                    bestcp = cp;
                    pt = Some(i);
                }
                i = (i + 1) % nbp;
            }
            if let Some(pt) = pt {
                let (hs, he) = (hull[ihs], hull[ihn]);
                let base = dist(x[hs], y[hs], x[he], y[he]);
                let b = dist(x[hs], y[hs], x[pt], y[pt]);
                let c = dist(x[he], y[he], x[pt], y[pt]);
                let s = (base + b + c) / 2.0;
                let area = (s * (s - base) * (s - b) * (s - c)).abs();
                if area - best_area > EPSILON {
                    best_area = area;
                    retained = Some(pt);
                    hole_s = ihs;
                    hole_e = ihn;
                }
            }
        }
        let best_area = best_area.sqrt();
        let Some(rp) = retained.filter(|_| best_area > label_area) else {
            output.push(SplitShape { x, y, hull });
            continue;
        };
        // The second cut point: nearest shape point/segment outside the bay.
        let mut c = f64::MAX;
        let (mut fps, mut fpe): (Option<usize>, Option<usize>) = (None, None);
        let mut fpt = (0.0, 0.0);
        let (mut ptx, mut pty) = (0.0, 0.0);
        let end = (hull[hole_s] + nbp - 1) % nbp;
        let mut i = (hull[hole_e] + 1) % nbp;
        while i != end {
            let j = (i + 1) % nbp;
            let seg_length = dist(x[i], y[i], x[j], y[j]);
            let (cx, cy) = ((x[i] + x[j]) / 2.0, (y[i] + y[j]) / 2.0);
            let (dx, dy) = (cy - y[i], cx - x[i]);
            let (ex, ey, fx, fy) = (cx - dx, cy + dy, cx + dx, cy - dy);
            let (b, ps, pe);
            if seg_length < EPSILON
                || (cross_product(ex, ey, fx, fy, x[rp], y[rp]) / seg_length).abs()
                    > seg_length / 2.0
            {
                let (di, dj) = (sqr(x[i], y[i], x[rp], y[rp]), sqr(x[j], y[j], x[rp], y[rp]));
                if di < dj {
                    (b, ps, pe) = (di, i, i);
                } else {
                    (b, ps, pe) = (dj, j, j);
                }
            } else {
                let d = cross_product(x[i], y[i], x[j], y[j], x[rp], y[rp]) / seg_length;
                (b, ps, pe) = (d * d, i, j);
                if let Some(p) =
                    line_intersection(x[i], y[i], x[j], y[j], x[rp], y[rp], x[rp] - dx, y[rp] + dy)
                {
                    (ptx, pty) = p;
                }
            }
            let (px, py) = if ps == pe { (x[pe], y[pe]) } else { (ptx, pty) };
            let mut valid = true;
            let mut k = hull[hole_s];
            while k != hull[hole_e] {
                let l = (k + 1) % nbp;
                if is_seg_intersects(x[rp], y[rp], px, py, x[k], y[k], x[l], y[l]) {
                    valid = false;
                    break;
                }
                k = l;
            }
            if valid && b < c {
                c = b;
                fps = Some(ps);
                fpe = Some(pe);
                fpt = (ptx, pty);
            }
            i = j;
        }
        let (Some(fps), Some(fpe)) = (fps, fpe) else {
            // No valid cut: QGIS drops the shape.
            continue;
        };
        let imin = rp;
        let mut imax = if (fps < rp && fpe < rp) || (fps > rp && fpe > rp) {
            fps.min(fpe)
        } else {
            fps.max(fpe)
        };
        let extra = usize::from(fpe != fps);
        let nb1 = if imax > imin {
            imax - imin + 1 + extra
        } else {
            imax + nbp - imin + 1 + extra
        };
        let other = if imax == fps { fpe } else { fps };
        let nb2 = if other < imin {
            imin - other + 1 + extra
        } else {
            imin + nbp - other + 1 + extra
        };
        if imax == imin || nb1 <= 2 || nb2 <= 2 || nb1 == nbp || nb2 == nbp {
            output.push(SplitShape { x, y, hull });
            continue;
        }
        input.push_back(extract_shape(&x, &y, nb1, imin, imax, fps, fpe, fpt));
        imax = other;
        input.push_back(extract_shape(&x, &y, nb2, imax, imin, fps, fpe, fpt));
    }
    output
}

/// pal::OrientedConvexHullBoundingBox.
#[derive(Debug, Clone, Copy)]
pub struct OrientedBox {
    pub x: [f64; 4],
    pub y: [f64; 4],
    pub alpha: f64,
    pub width: f64,
    pub length: f64,
}

/// PointSet::computeConvexHullOrientedBoundingBox: the smallest-area box over whole-degree
/// rotations 0..89.
pub fn oriented_bbox(x: &[f64], y: &[f64], hull: &[usize]) -> Option<OrientedBox> {
    let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for &h in hull {
        bbox[0] = bbox[0].min(x[h]);
        bbox[2] = bbox[2].max(x[h]);
        bbox[1] = bbox[1].min(y[h]);
        bbox[3] = bbox[3].max(y[h]);
    }
    let dref = bbox[2] - bbox[0];
    if near(dref, 0.0) {
        return None;
    }
    let mut best_area = f64::MAX;
    let (mut best_alpha, mut best_len, mut best_w) = (-1.0, 0.0, 0.0);
    let mut best_bb = [0.0f64; 16];
    for alpha_d in 0..90 {
        let alpha = alpha_d as f64 * std::f64::consts::PI / 180.0;
        let (d1, d2) = (alpha.cos() * dref, alpha.sin() * dref);
        let mut bb = [0.0f64; 16];
        (bb[0], bb[1]) = (bbox[0], bbox[3]);
        (bb[4], bb[5]) = (bbox[0], bbox[1]);
        (bb[8], bb[9]) = (bbox[2], bbox[1]);
        (bb[12], bb[13]) = (bbox[2], bbox[3]);
        (bb[2], bb[3]) = (bb[0] + d1, bb[1] + d2);
        (bb[6], bb[7]) = (bb[4] - d2, bb[5] + d1);
        (bb[10], bb[11]) = (bb[8] - d1, bb[9] - d2);
        (bb[14], bb[15]) = (bb[12] + d2, bb[13] - d1);
        for i in (0..16).step_by(4) {
            let q = i / 4;
            let alpha_seg =
                (if q > 0 { q - 1 } else { 3 }) as f64 * std::f64::consts::FRAC_PI_2 + alpha;
            let mut best_cp = f64::MAX;
            for &h in hull {
                let cp = cross_product(bb[i + 2], bb[i + 3], bb[i], bb[i + 1], x[h], y[h]);
                if cp < best_cp {
                    best_cp = cp;
                }
            }
            let dist = best_cp / dref;
            let (e1, e2) = (alpha_seg.cos() * dist, alpha_seg.sin() * dist);
            bb[i] += e1;
            bb[i + 1] += e2;
            bb[i + 2] += e1;
            bb[i + 3] += e2;
        }
        let width = cross_product(bb[6], bb[7], bb[4], bb[5], bb[12], bb[13]) / dref;
        let length = cross_product(bb[2], bb[3], bb[0], bb[1], bb[8], bb[9]) / dref;
        let area = (width * length).abs();
        if best_area - area > EPSILON {
            best_area = area;
            best_len = length;
            best_w = width;
            best_alpha = alpha;
            best_bb = bb;
        }
    }
    let mut out = OrientedBox {
        x: [0.0; 4],
        y: [0.0; 4],
        alpha: best_alpha,
        width: best_w,
        length: best_len,
    };
    for i in (0..16).step_by(4) {
        let b = &best_bb;
        // A failed intersection leaves QGIS's zero-initialised corner.
        if let Some((px, py)) = line_intersection(
            b[i],
            b[i + 1],
            b[i + 2],
            b[i + 3],
            b[(i + 4) % 16],
            b[(i + 5) % 16],
            b[(i + 6) % 16],
            b[(i + 7) % 16],
        ) {
            out.x[i / 4] = px;
            out.y[i / 4] = py;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hull_of_square_with_inner_point() {
        let x = [0.0, 10.0, 10.0, 0.0, 5.0];
        let y = [0.0, 0.0, 10.0, 10.0, 5.0];
        let mut h = convex_hull(&x, &y);
        h.sort();
        assert_eq!(h, vec![0, 1, 2, 3]);
    }

    #[test]
    fn concave_l_is_split() {
        // An L with a big bay; a small label splits it in two.
        let x = vec![0.0, 100.0, 100.0, 25.0, 25.0, 0.0, 0.0];
        let y = vec![0.0, 0.0, 25.0, 25.0, 100.0, 100.0, 0.0];
        let parts = split_polygons(x, y, 5.0, 2.0);
        assert!(parts.len() >= 2, "{}", parts.len());
    }

    #[test]
    fn oriented_box_of_axis_rectangle() {
        let x = [0.0, 20.0, 20.0, 0.0];
        let y = [0.0, 0.0, 10.0, 10.0];
        let h = convex_hull(&x, &y);
        let b = oriented_bbox(&x, &y, &h).unwrap();
        assert!((b.width * b.length).abs() - 200.0 < 1e-6);
    }
}
