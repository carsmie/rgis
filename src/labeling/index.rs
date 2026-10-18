//! Spatial index for label candidates and obstacles (the role of pal::PalRtree): a uniform
//! grid over the map extent. Queries report ids in ascending order, so results never depend
//! on insertion history.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub xmin: f64,
    pub ymin: f64,
    pub xmax: f64,
    pub ymax: f64,
}

impl BBox {
    pub fn intersects(&self, o: &BBox) -> bool {
        self.xmin <= o.xmax && o.xmin <= self.xmax && self.ymin <= o.ymax && o.ymin <= self.ymax
    }
}

pub struct GridIndex {
    origin: (f64, f64),
    cell: (f64, f64),
    n: usize,
    cells: Vec<Vec<u32>>,
    boxes: Vec<Option<BBox>>,
    stamp: Vec<u32>,
    epoch: u32,
}

impl GridIndex {
    /// An index over `extent` with `n`×`n` cells (items outside are kept in the border cells).
    pub fn new(extent: BBox, n: usize) -> GridIndex {
        let n = n.max(1);
        let w = (extent.xmax - extent.xmin).max(f64::MIN_POSITIVE);
        let h = (extent.ymax - extent.ymin).max(f64::MIN_POSITIVE);
        GridIndex {
            origin: (extent.xmin, extent.ymin),
            cell: (w / n as f64, h / n as f64),
            n,
            cells: vec![Vec::new(); n * n],
            boxes: Vec::new(),
            stamp: Vec::new(),
            epoch: 0,
        }
    }

    fn range(&self, b: &BBox) -> (usize, usize, usize, usize) {
        let clamp = |v: f64| -> usize {
            if v.is_nan() || v < 0.0 {
                0
            } else {
                (v as usize).min(self.n - 1)
            }
        };
        (
            clamp((b.xmin - self.origin.0) / self.cell.0),
            clamp((b.ymin - self.origin.1) / self.cell.1),
            clamp((b.xmax - self.origin.0) / self.cell.0),
            clamp((b.ymax - self.origin.1) / self.cell.1),
        )
    }

    pub fn insert(&mut self, id: usize, b: BBox) {
        if self.boxes.len() <= id {
            self.boxes.resize(id + 1, None);
            self.stamp.resize(id + 1, 0);
        }
        if self.boxes[id].is_some() {
            return;
        }
        self.boxes[id] = Some(b);
        let (x0, y0, x1, y1) = self.range(&b);
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.cells[y * self.n + x].push(id as u32);
            }
        }
    }

    /// Ids whose boxes intersect `b`, ascending.
    pub fn query(&mut self, b: &BBox) -> Vec<usize> {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.iter_mut().for_each(|s| *s = 0);
            self.epoch = 1;
        }
        let (x0, y0, x1, y1) = self.range(b);
        let mut out = Vec::new();
        for y in y0..=y1 {
            for x in x0..=x1 {
                for &id in &self.cells[y * self.n + x] {
                    let id = id as usize;
                    if self.stamp[id] != self.epoch {
                        self.stamp[id] = self.epoch;
                        if self.boxes[id].is_some_and(|ib| ib.intersects(b)) {
                            out.push(id);
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }
}
