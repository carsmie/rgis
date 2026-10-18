//! pal::PalRtree: QGIS's bundled R-tree (external/rtree/include/RTree.h, Guttman quadratic
//! split, spherical volumes, `RTree<T*, float, 2, float>`, 8 entries per node, 4 minimum) with
//! PalRtree's coordinate scaling.
//!
//! Ported operation for operation — including single-precision arithmetic — because the order
//! in which queries report candidates decides ties in PAL's FALP priority queue. (With PAL's
//! scaling, coordinates become tiny and most volumes underflow to 0 in `f32`; the tree's
//! first-index tie-breaks then decide its shape, exactly as in QGIS.)

use super::index::BBox;

const MAXNODES: usize = 8;
const MINNODES: usize = 4;
const UNIT_SPHERE_VOLUME: f32 = 3.141593;

type Rect = [f32; 4]; // min x, min y, max x, max y

#[derive(Clone, Copy, Default)]
struct Branch {
    rect: Rect,
    child: usize,
    data: usize,
}

#[derive(Clone)]
struct Node {
    count: usize,
    level: i32,
    branch: [Branch; MAXNODES],
}

impl Node {
    fn new(level: i32) -> Node {
        Node { count: 0, level, branch: [Branch::default(); MAXNODES] }
    }
}

pub struct PalRtree {
    nodes: Vec<Node>,
    free: Vec<usize>,
    root: usize,
    xmin: f64,
    ymin: f64,
    xres: f64,
    yres: f64,
    max: BBox,
}

fn combine(a: &Rect, b: &Rect) -> Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// RectSphericalVolume in f32, as compiled in QGIS.
fn volume(r: &Rect) -> f32 {
    let mut sum: f32 = 0.0;
    for i in 0..2 {
        let half = (r[i + 2] - r[i]) * 0.5f32;
        sum += half * half;
    }
    let radius = sum.sqrt();
    radius * radius * UNIT_SPHERE_VOLUME
}

fn overlap(a: &Rect, b: &Rect) -> bool {
    !(a[0] > b[2] || b[0] > a[2] || a[1] > b[3] || b[1] > a[3])
}

struct PartitionVars {
    partition: [i32; MAXNODES + 1],
    total: usize,
    min_fill: usize,
    count: [usize; 2],
    cover: [Rect; 2],
    area: [f32; 2],
    buf: [Branch; MAXNODES + 1],
}

impl PalRtree {
    /// `max_bounds`: Pal::extractProblem's extent buffered by 1000 × its larger side.
    pub fn new(max_bounds: BBox) -> PalRtree {
        let res = |lo: f64, hi: f64| (f32::MAX as f64 - 1.0) / (hi - lo);
        PalRtree {
            nodes: vec![Node::new(0)],
            free: Vec::new(),
            root: 0,
            xmin: max_bounds.xmin,
            ymin: max_bounds.ymin,
            xres: res(max_bounds.xmin, max_bounds.xmax),
            yres: res(max_bounds.ymin, max_bounds.ymax),
            max: max_bounds,
        }
    }

    fn scale(&self, b: &BBox) -> Rect {
        [
            ((b.xmin.max(self.max.xmin) - self.xmin) / self.xres) as f32,
            ((b.ymin.max(self.max.ymin) - self.ymin) / self.yres) as f32,
            ((b.xmax.min(self.max.xmax) - self.xmin) / self.xres) as f32,
            ((b.ymax.min(self.max.ymax) - self.ymin) / self.yres) as f32,
        ]
    }

    fn alloc(&mut self, level: i32) -> usize {
        if let Some(i) = self.free.pop() {
            self.nodes[i] = Node::new(level);
            i
        } else {
            self.nodes.push(Node::new(level));
            self.nodes.len() - 1
        }
    }

    pub fn insert(&mut self, data: usize, b: &BBox) {
        let rect = self.scale(b);
        let mut root = self.root;
        self.insert_rect(Branch { rect, child: usize::MAX, data }, &mut root, 0);
        self.root = root;
    }

    pub fn remove(&mut self, data: usize, b: &BBox) {
        let rect = self.scale(b);
        let mut root = self.root;
        self.remove_rect(&rect, data, &mut root);
        self.root = root;
    }

    /// Data whose rectangles overlap `b`, in the tree's traversal order.
    pub fn query(&self, b: &BBox) -> Vec<usize> {
        let rect = self.scale(b);
        let mut out = Vec::new();
        self.search(self.root, &rect, &mut out);
        out
    }

    fn search(&self, node: usize, rect: &Rect, out: &mut Vec<usize>) {
        let n = &self.nodes[node];
        for br in &n.branch[..n.count] {
            if overlap(rect, &br.rect) {
                if n.level > 0 {
                    self.search(br.child, rect, out);
                } else {
                    out.push(br.data);
                }
            }
        }
    }

    fn node_cover(&self, node: usize) -> Rect {
        let n = &self.nodes[node];
        let mut r = n.branch[0].rect;
        for br in &n.branch[1..n.count] {
            r = combine(&r, &br.rect);
        }
        r
    }

    fn insert_rect(&mut self, branch: Branch, root: &mut usize, level: i32) -> bool {
        if let Some(new_node) = self.insert_rect_rec(branch, *root, level) {
            let new_root = self.alloc(self.nodes[*root].level + 1);
            let b1 = Branch { rect: self.node_cover(*root), child: *root, data: 0 };
            self.add_branch(&b1, new_root);
            let b2 = Branch { rect: self.node_cover(new_node), child: new_node, data: 0 };
            self.add_branch(&b2, new_root);
            *root = new_root;
            return true;
        }
        false
    }

    /// Returns the new node when `node` was split.
    fn insert_rect_rec(&mut self, branch: Branch, node: usize, level: i32) -> Option<usize> {
        if self.nodes[node].level > level {
            let index = self.pick_branch(&branch.rect, node);
            let child = self.nodes[node].branch[index].child;
            match self.insert_rect_rec(branch, child, level) {
                None => {
                    let r = combine(&branch.rect, &self.nodes[node].branch[index].rect);
                    self.nodes[node].branch[index].rect = r;
                    None
                }
                Some(other) => {
                    self.nodes[node].branch[index].rect = self.node_cover(child);
                    let b = Branch { rect: self.node_cover(other), child: other, data: 0 };
                    self.add_branch(&b, node)
                }
            }
        } else {
            self.add_branch(&branch, node)
        }
    }

    fn add_branch(&mut self, branch: &Branch, node: usize) -> Option<usize> {
        let n = &mut self.nodes[node];
        if n.count < MAXNODES {
            n.branch[n.count] = *branch;
            n.count += 1;
            None
        } else {
            Some(self.split_node(node, branch))
        }
    }

    fn pick_branch(&self, rect: &Rect, node: usize) -> usize {
        let n = &self.nodes[node];
        let mut first = true;
        let (mut best_incr, mut best_area, mut best) = (-1f32, -1f32, 0);
        for (i, br) in n.branch[..n.count].iter().enumerate() {
            let area = volume(&br.rect);
            let increase = volume(&combine(rect, &br.rect)) - area;
            if increase < best_incr || first {
                best = i;
                best_area = area;
                best_incr = increase;
                first = false;
            } else if increase == best_incr && area < best_area {
                best = i;
                best_area = area;
                best_incr = increase;
            }
        }
        best
    }

    fn split_node(&mut self, node: usize, branch: &Branch) -> usize {
        let level = self.nodes[node].level;
        let mut pv = PartitionVars {
            partition: [-1; MAXNODES + 1],
            total: MAXNODES + 1,
            min_fill: MINNODES,
            count: [0; 2],
            cover: [[0.0; 4]; 2],
            area: [0.0; 2],
            buf: [Branch::default(); MAXNODES + 1],
        };
        pv.buf[..MAXNODES].copy_from_slice(&self.nodes[node].branch);
        pv.buf[MAXNODES] = *branch;
        choose_partition(&mut pv);
        let new_node = self.alloc(level);
        self.nodes[node].count = 0;
        for i in 0..pv.total {
            let target = if pv.partition[i] == 0 { node } else { new_node };
            let b = pv.buf[i];
            self.add_branch(&b, target);
        }
        new_node
    }

    fn remove_rect(&mut self, rect: &Rect, data: usize, root: &mut usize) -> bool {
        let mut reinsert: Vec<usize> = Vec::new();
        if !self.remove_rect_rec(rect, data, *root, &mut reinsert) {
            // ReInsert pushes to the front of a linked list: newest first.
            for &tmp in reinsert.iter().rev() {
                let n = self.nodes[tmp].clone();
                for b in &n.branch[..n.count] {
                    self.insert_rect(*b, root, n.level);
                }
                self.free.push(tmp);
            }
            if self.nodes[*root].count == 1 && self.nodes[*root].level > 0 {
                let child = self.nodes[*root].branch[0].child;
                self.free.push(*root);
                *root = child;
            }
            return false;
        }
        true
    }

    /// Returns true when not found.
    fn remove_rect_rec(&mut self, rect: &Rect, data: usize, node: usize, reinsert: &mut Vec<usize>) -> bool {
        if self.nodes[node].level > 0 {
            for i in 0..self.nodes[node].count {
                if overlap(rect, &self.nodes[node].branch[i].rect) {
                    let child = self.nodes[node].branch[i].child;
                    if !self.remove_rect_rec(rect, data, child, reinsert) {
                        if self.nodes[child].count >= MINNODES {
                            self.nodes[node].branch[i].rect = self.node_cover(child);
                        } else {
                            reinsert.push(child);
                            disconnect(&mut self.nodes[node], i);
                        }
                        return false;
                    }
                }
            }
            true
        } else {
            let n = &mut self.nodes[node];
            for i in 0..n.count {
                if n.branch[i].data == data {
                    disconnect(n, i);
                    return false;
                }
            }
            true
        }
    }
}

fn disconnect(n: &mut Node, i: usize) {
    n.branch[i] = n.branch[n.count - 1];
    n.count -= 1;
}

fn classify(i: usize, group: usize, pv: &mut PartitionVars) {
    pv.partition[i] = group as i32;
    pv.cover[group] = if pv.count[group] == 0 { pv.buf[i].rect } else { combine(&pv.buf[i].rect, &pv.cover[group]) };
    pv.area[group] = volume(&pv.cover[group]);
    pv.count[group] += 1;
}

fn pick_seeds(pv: &mut PartitionVars) {
    let mut area = [0f32; MAXNODES + 1];
    for i in 0..pv.total {
        area[i] = volume(&pv.buf[i].rect);
    }
    let mut first = true;
    let (mut worst, mut seed0, mut seed1) = (0f32, 0, 0);
    for a in 0..pv.total - 1 {
        for b in a + 1..pv.total {
            let waste = volume(&combine(&pv.buf[a].rect, &pv.buf[b].rect)) - area[a] - area[b];
            if first || waste > worst {
                first = false;
                worst = waste;
                seed0 = a;
                seed1 = b;
            }
        }
    }
    classify(seed0, 0, pv);
    classify(seed1, 1, pv);
}

fn choose_partition(pv: &mut PartitionVars) {
    pick_seeds(pv);
    while pv.count[0] + pv.count[1] < pv.total
        && pv.count[0] < pv.total - pv.min_fill
        && pv.count[1] < pv.total - pv.min_fill
    {
        let mut first = true;
        let (mut biggest, mut chosen, mut better) = (0f32, 0, 0);
        for i in 0..pv.total {
            if pv.partition[i] != -1 {
                continue;
            }
            let r = &pv.buf[i].rect;
            let growth0 = volume(&combine(r, &pv.cover[0])) - pv.area[0];
            let growth1 = volume(&combine(r, &pv.cover[1])) - pv.area[1];
            let mut diff = growth1 - growth0;
            let group = if diff >= 0.0 {
                0
            } else {
                diff = -diff;
                1
            };
            if first || diff > biggest {
                first = false;
                biggest = diff;
                chosen = i;
                better = group;
            } else if diff == biggest && pv.count[group] < pv.count[better] {
                chosen = i;
                better = group;
            }
        }
        classify(chosen, better, pv);
    }
    if pv.count[0] + pv.count[1] < pv.total {
        let group = if pv.count[0] >= pv.total - pv.min_fill { 1 } else { 0 };
        for i in 0..pv.total {
            if pv.partition[i] == -1 {
                classify(i, group, pv);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_query_remove() {
        let mut t = PalRtree::new(BBox { xmin: -1000.0, ymin: -1000.0, xmax: 1000.0, ymax: 1000.0 });
        for i in 0..100 {
            let x = (i % 10) as f64 * 10.0;
            let y = (i / 10) as f64 * 10.0;
            t.insert(i, &BBox { xmin: x, ymin: y, xmax: x + 5.0, ymax: y + 5.0 });
        }
        let mut q = t.query(&BBox { xmin: 0.0, ymin: 0.0, xmax: 12.0, ymax: 12.0 });
        q.sort();
        assert_eq!(q, vec![0, 1, 10, 11]);
        t.remove(11, &BBox { xmin: 10.0, ymin: 10.0, xmax: 15.0, ymax: 15.0 });
        let mut q = t.query(&BBox { xmin: 0.0, ymin: 0.0, xmax: 12.0, ymax: 12.0 });
        q.sort();
        assert_eq!(q, vec![0, 1, 10]);
        assert_eq!(t.query(&BBox { xmin: -1000.0, ymin: -1000.0, xmax: 1000.0, ymax: 1000.0 }).len(), 99);
    }
}
