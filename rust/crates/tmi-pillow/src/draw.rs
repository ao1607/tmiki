//! `ImageDraw.polygon(xy, fill=255)` and `ImageDraw.ellipse(box, fill=255)`
//! on an "L" image (libImaging/Draw.c: `ImagingDrawPolygon` with
//! `polygon_generic` / `hline8`, and `ellipseNew`), pixel for pixel.
//!
//! The Pillow source was not consulted: both were derived from Pillow 12.3's
//! behaviour and pinned by differential fuzzing against it
//! (tests/test_rust_kernels.py through `tmi_accel._test`). What they do:
//!
//! * Coordinates are truncated toward zero (a C `(int)` cast).
//! * Polygon: an edge list (consecutive horizontal edges going the same way
//!   are merged; the closing edge is added last unless the path is closed).
//!   Horizontal edges are drawn as lines. Every scan line `y` from the
//!   lowest edge top (clamped to 0) to the highest edge bottom (clamped to
//!   the image height, not height - 1) collects, in edge order, the float32
//!   crossing `(y - y0) * dx + x0` of each edge with `ymin <= y <= ymax`;
//!   an edge ending on `y` above the last line is entered twice. A corner
//!   rule then moves the crossing of an edge starting (or, on the last line,
//!   ending) on `y`: the first earlier edge with the same rounded crossing
//!   that also starts (ends) there and is not vertical decides; with a
//!   different slope sign nothing changes, else the crossing jumps next to
//!   the two edges' crossings on the following (last line: previous) row
//!   when it lies more than a pixel beyond both. The crossings are sorted and
//!   each pair `(a, b)` fills `ROUND_UP(a) ..= ROUND_DOWN(b)`.
//! * Ellipse: a midpoint walk of one quarter in doubled coordinates
//!   (Bresenham on `a² y² + b² x² - a² b²`), each row filled between the
//!   walk's first x on it and its mirror.
//!
//! Both draw into an [`LCanvas`], which may hold only a sub-rectangle of the
//! image: rows are independent in both algorithms, so drawing a band of a
//! large image gives exactly that band of the whole drawing.

/// An "L" image of `width` x `height`, of which the pixels of `region`
/// (x0, y0, x1, y1, inside the image) are held in `px` (row-major).
#[derive(Clone, Debug)]
pub struct LCanvas {
    pub width: i32,
    pub height: i32,
    pub region: [i32; 4],
    pub px: Vec<u8>,
    /// Per held row, the first and last x filled (image coordinates);
    /// `(i32::MAX, i32::MIN)` for a blank row.
    pub ext: Vec<(i32, i32)>,
}

impl LCanvas {
    pub fn new(width: i32, height: i32, region: [i32; 4]) -> LCanvas {
        let r = [region[0].max(0), region[1].max(0), region[2].min(width), region[3].min(height)];
        let (w, h) = ((r[2] - r[0]).max(0) as usize, (r[3] - r[1]).max(0) as usize);
        LCanvas { width, height, region: r, px: vec![0; w * h], ext: vec![(i32::MAX, i32::MIN); h] }
    }

    /// The whole image.
    pub fn full(width: i32, height: i32) -> LCanvas {
        LCanvas::new(width, height, [0, 0, width, height])
    }

    pub fn stride(&self) -> usize {
        (self.region[2] - self.region[0]).max(0) as usize
    }

    /// `hline8` with ink 255: Pillow's clipping to the image, then to the
    /// held region.
    #[inline]
    fn hline(&mut self, mut x0: i32, y: i32, mut x1: i32) {
        if y < 0 || y >= self.height {
            return;
        }
        if x0 < 0 {
            x0 = 0;
        } else if x0 >= self.width {
            return;
        }
        if x1 < 0 {
            return;
        } else if x1 >= self.width {
            x1 = self.width - 1;
        }
        let [rx0, ry0, rx1, ry1] = self.region;
        if y < ry0 || y >= ry1 {
            return;
        }
        let (a, b) = (x0.max(rx0), x1.min(rx1 - 1));
        if a > b {
            return;
        }
        let s = (y - ry0) as usize * self.stride();
        self.px[s + (a - rx0) as usize..=s + (b - rx0) as usize].fill(255);
        let e = &mut self.ext[(y - ry0) as usize];
        *e = (e.0.min(a), e.1.max(b));
    }
}

/// C's `(int)` cast of a double (truncation); the caller keeps coordinates
/// well inside the int range (`coord_ok`).
#[inline]
fn to_int(v: f64) -> i32 {
    v as i32
}

/// Whether a coordinate can be drawn (Pillow's cast would be undefined, or
/// the walks would overflow, far outside this range).
pub fn coord_ok(v: f64) -> bool {
    v.is_finite() && v.abs() < 1.0e7
}

#[derive(Clone, Copy, Debug)]
struct Edge {
    xmin: i32,
    xmax: i32,
    ymin: i32,
    ymax: i32,
    dx: f32,
    x0: i32,
    y0: i32,
}

fn add_edge(x0: i32, y0: i32, x1: i32, y1: i32) -> Edge {
    let (xmin, xmax) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
    let (ymin, ymax) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
    let dx = if y0 == y1 { 0.0 } else { (x1 - x0) as f32 / (y1 - y0) as f32 };
    Edge { xmin, xmax, ymin, ymax, dx, x0, y0 }
}

/// The float32 crossing of an edge's line with row `y`.
#[inline]
fn edge_x(e: &Edge, y: i32) -> f32 {
    (y - e.y0) as f32 * e.dx + e.x0 as f32
}

/// `ROUND_UP`: half away from zero; the positive branch adds in float, the
/// negative one in double (`fabs` returns a double).
#[inline]
fn round_up(f: f32) -> i32 {
    if f >= 0.0 {
        (f + 0.5f32).floor() as i32
    } else {
        -((f.abs() as f64 + 0.5).floor() as i32)
    }
}

/// `ROUND_DOWN`: half toward zero, same float / double split.
#[inline]
fn round_down(f: f32) -> i32 {
    if f >= 0.0 {
        (f - 0.5f32).ceil() as i32
    } else {
        -((f.abs() as f64 - 0.5).ceil() as i32)
    }
}

/// Buffers `polygon_with` reuses from one polygon to the next.
#[derive(Default)]
pub struct PolyScratch {
    xy: Vec<(i32, i32)>,
    edges: Vec<Edge>,
    table: Vec<Edge>,
    xx: Vec<f32>,
}

/// `ImageDraw.Draw(img).polygon(pts, fill=255)` (ink 255, no outline).
pub fn polygon(c: &mut LCanvas, pts: &[(f64, f64)]) {
    polygon_with(c, pts, &mut PolyScratch::default());
}

/// [`polygon`] with reusable buffers.
pub fn polygon_with(c: &mut LCanvas, pts: &[(f64, f64)], s: &mut PolyScratch) {
    let count = pts.len();
    if count == 0 {
        return;
    }
    let PolyScratch { xy, edges, table, xx } = s;
    xy.clear();
    xy.extend(pts.iter().map(|&(x, y)| (to_int(x), to_int(y))));
    edges.clear();
    let mut i = 0;
    while i + 1 < count {
        let ((x0, y0), (x1, y1)) = (xy[i], xy[i + 1]);
        if y0 == y1 && i != 0 && y0 == xy[i - 1].1 {
            // A horizontal edge right after another one, going the same way.
            let prev_x = xy[i - 1].0;
            if let Some(last) = edges.last_mut() {
                if x1 > x0 && x0 > prev_x {
                    last.xmax = x1;
                    i += 1;
                    continue;
                } else if x1 < x0 && x0 < prev_x {
                    last.xmin = x1;
                    i += 1;
                    continue;
                }
            }
        }
        edges.push(add_edge(x0, y0, x1, y1));
        i += 1;
    }
    if xy[count - 1] != xy[0] {
        let ((x0, y0), (x1, y1)) = (xy[count - 1], xy[0]);
        edges.push(add_edge(x0, y0, x1, y1));
    }
    polygon_generic(c, edges, table, xx);
}

fn polygon_generic(c: &mut LCanvas, e: &[Edge], table: &mut Vec<Edge>, xx: &mut Vec<f32>) {
    if e.is_empty() {
        return;
    }
    let mut ymin = c.height - 1;
    let mut ymax = 0;
    table.clear();
    for &edge in e {
        ymin = ymin.min(edge.ymin);
        ymax = ymax.max(edge.ymax);
        if edge.ymin == edge.ymax {
            c.hline(edge.xmin, edge.ymin, edge.xmax);
            continue;
        }
        table.push(edge);
    }
    ymin = ymin.max(0);
    ymax = ymax.min(c.height);
    // Only the held rows: each row depends on the edges and on `ymax` alone.
    let (from, to) = (ymin.max(c.region[1]), ymax.min(c.region[3] - 1));
    for y in from..=to {
        xx.clear();
        for (i, cur) in table.iter().enumerate() {
            if y < cur.ymin || y > cur.ymax {
                continue;
            }
            let x = edge_x(cur, y);
            xx.push(x);
            if y == cur.ymax && y < ymax {
                xx.push(x);
            } else if cur.dx != 0.0 && (y == cur.ymin || y == cur.ymax) {
                if let Some(v) = corner(table, i, cur, y, ymax, x) {
                    *xx.last_mut().unwrap() = v;
                }
            }
        }
        xx.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for k in (1..xx.len()).step_by(2) {
            c.hline(round_up(xx[k - 1]), y, round_down(xx[k]));
        }
    }
}

/// The corner rule (see the module doc): the new crossing of `cur` on `y`,
/// if it moves.
fn corner(table: &[Edge], i: usize, cur: &Edge, y: i32, ymax: i32, x: f32) -> Option<f32> {
    for other in &table[..i] {
        if y < other.ymin || y > other.ymax || other.dx == 0.0 {
            continue;
        }
        if x.round() != edge_x(other, y).round() {
            continue;
        }
        if !((y == cur.ymin && y == other.ymin) || (y == cur.ymax && y == other.ymax)) {
            continue;
        }
        if (cur.dx > 0.0 && other.dx <= 0.0) || (cur.dx < 0.0 && other.dx >= 0.0) {
            return None;
        }
        let next = if y == ymax { y - 1 } else { y + 1 };
        let adj = edge_x(cur, next);
        if next < other.ymin || next > other.ymax {
            return None;
        }
        let adj_o = edge_x(other, next);
        if x > adj + 1.0 && x > adj_o + 1.0 {
            return Some(adj.max(adj_o).round() + 1.0);
        }
        if x < adj - 1.0 && x < adj_o - 1.0 {
            return Some(adj.min(adj_o).round() - 1.0);
        }
        return None;
    }
    None
}

/// The rows of a filled ellipse with axes `a` x `b` (pixels: the bounding
/// box is `a + 1` x `b + 1`): `(row, first x, last x)` relative to the box's
/// top-left corner, in no particular order. Empty when `a + b < 1`.
pub fn ellipse_rows(a: i32, b: i32) -> Vec<(i32, i32, i32)> {
    let mut out = Vec::new();
    if a < 0 || b < 0 || a + b < 1 {
        return out;
    }
    let (a2, b2) = (a as i64 * a as i64, b as i64 * b as i64);
    let a2b2 = a2 * b2;
    let delta = |x: i64, y: i64| (a2 * y * y + b2 * x * x - a2b2).abs();
    let (mut cx, mut cy) = (a, b % 2);
    let (ex, ey) = (a % 2, b);
    let mut last_y = i32::MIN;
    loop {
        if cy != last_y {
            // The first (largest) x on this row of doubled coordinates.
            last_y = cy;
            let (l, yy) = (cx, cy);
            let x0 = (a - l) / 2;
            let x1 = (a + l) / 2;
            out.push(((yy + b) / 2, x0, x1));
            if yy > 0 {
                out.push(((b - yy) / 2, x0, x1));
            }
        }
        if cx == ex && cy == ey {
            break;
        }
        let (mut nx, mut ny) = (cx, cy + 2);
        let mut nd = delta(nx as i64, ny as i64);
        if nx > 1 {
            let d2 = delta(cx as i64 - 2, cy as i64 + 2);
            if nd > d2 {
                nx = cx - 2;
                ny = cy + 2;
                nd = d2;
            }
            let d3 = delta(cx as i64 - 2, cy as i64);
            if nd > d3 {
                nx = cx - 2;
                ny = cy;
            }
        }
        cx = nx;
        cy = ny;
    }
    out
}

/// `ImageDraw.Draw(img).ellipse(bbox, fill=255)`; `bbox` is (x0, y0, x1,
/// y1) with x1 >= x0 and y1 >= y0 (Pillow raises otherwise).
pub fn ellipse(c: &mut LCanvas, bbox: [f64; 4]) {
    let (x0, y0, x1, y1) = (to_int(bbox[0]), to_int(bbox[1]), to_int(bbox[2]), to_int(bbox[3]));
    let (a, b) = (x1 - x0, y1 - y0);
    // Skip the walk when the box misses the held rows.
    if y1 < c.region[1] || y0 >= c.region[3] {
        return;
    }
    for (row, l, r) in ellipse_rows(a, b) {
        c.hline(x0 + l, y0 + row, x0 + r);
    }
}

/// `ImageDraw.Draw(img).rectangle((x0, y0, x1, y1), outline=ink,
/// width=width)` on an RGBA image (Draw.c `ImagingDrawRectangle` with
/// `draw32`, read from the Pillow 12.3 source): `width` rings, each two
/// horizontal lines from x0 to x1 inclusive and two vertical lines of
/// `line32` (which stops one short of its end point and may run upwards).
/// The coordinates are already integers with x1 >= x0, y1 >= y0.
pub fn rectangle_outline_rgba(px: &mut [u8], w: usize, h: usize, x0: i32, y0: i32, x1: i32, y1: i32, ink: [u8; 4], width: i32) {
    let (wi, hi) = (w as i32, h as i32);
    let (y0, y1) = if y0 > y1 { (y1, y0) } else { (y0, y1) };
    let mut point = |px: &mut [u8], x: i32, y: i32| {
        if x >= 0 && x < wi && y >= 0 && y < hi {
            let i = (y as usize * w + x as usize) * 4;
            px[i..i + 4].copy_from_slice(&ink);
        }
    };
    let hline = |px: &mut [u8], point: &mut dyn FnMut(&mut [u8], i32, i32), mut a: i32, y: i32, mut b: i32| {
        if y < 0 || y >= hi {
            return;
        }
        if a < 0 {
            a = 0;
        } else if a >= wi {
            return;
        }
        if b < 0 {
            return;
        } else if b >= wi {
            b = wi - 1;
        }
        while a <= b {
            point(px, a, y);
            a += 1;
        }
    };
    // line32 with dx == 0: `dy` points from (x, ya) towards yb.
    let vline = |px: &mut [u8], point: &mut dyn FnMut(&mut [u8], i32, i32), x: i32, ya: i32, yb: i32| {
        let (dy, ys) = if yb - ya < 0 { (ya - yb, -1) } else { (yb - ya, 1) };
        let mut y = ya;
        for _ in 0..dy {
            point(px, x, y);
            y += ys;
        }
    };
    let width = if width == 0 { 1 } else { width };
    for i in 0..width {
        hline(px, &mut point, x0, y0 + i, x1);
        hline(px, &mut point, x0, y1 - i, x1);
        vline(px, &mut point, x1 - i, y0 + width, y1 - width + 1);
        vline(px, &mut point, x0 + i, y0 + width, y1 - width + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw_full(w: i32, h: i32, f: impl Fn(&mut LCanvas)) -> LCanvas {
        let mut c = LCanvas::full(w, h);
        f(&mut c);
        c
    }

    #[test]
    fn region_is_a_crop_of_the_whole() {
        let pts = [(1.2, -3.0), (30.7, 4.9), (12.0, 39.5), (-4.0, 22.2)];
        let whole = draw_full(32, 40, |c| {
            polygon(c, &pts);
            ellipse(c, [3.3, 5.1, 25.9, 33.0]);
        });
        for region in [[0, 0, 32, 40], [5, 7, 20, 31], [0, 39, 32, 40], [31, 0, 32, 40]] {
            let mut c = LCanvas::new(32, 40, region);
            polygon(&mut c, &pts);
            ellipse(&mut c, [3.3, 5.1, 25.9, 33.0]);
            let w = c.stride();
            for y in region[1]..region[3] {
                for x in region[0]..region[2] {
                    let got = c.px[(y - region[1]) as usize * w + (x - region[0]) as usize];
                    assert_eq!(got, whole.px[(y * 32 + x) as usize], "{region:?} {x} {y}");
                }
            }
        }
    }

    #[test]
    fn tiny_ellipses() {
        assert!(ellipse_rows(0, 0).is_empty());
        let mut rows = ellipse_rows(1, 1);
        rows.sort();
        assert_eq!(rows, vec![(0, 0, 1), (1, 0, 1)]);
    }
}
