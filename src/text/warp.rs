//! Photoshop warp presets as bicubic Bezier patches over the warp rectangle.

use std::f64::consts::PI;

use crate::psd::descriptor::{Descriptor, Value};

/// Preset envelope of a warped type layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum WarpStyle {
    None,
    Arc,
    ArcLower,
    ArcUpper,
    Arch,
    Bulge,
    ShellLower,
    ShellUpper,
    Flag,
    Wave,
    Fish,
    Rise,
    Fisheye,
    Inflate,
    Squeeze,
    Twist,
}

impl WarpStyle {
    pub(crate) fn from_key(key: &str) -> WarpStyle {
        match key {
            "warpArc" => WarpStyle::Arc,
            "warpArcLower" => WarpStyle::ArcLower,
            "warpArcUpper" => WarpStyle::ArcUpper,
            "warpArch" => WarpStyle::Arch,
            "warpBulge" => WarpStyle::Bulge,
            "warpShellLower" => WarpStyle::ShellLower,
            "warpShellUpper" => WarpStyle::ShellUpper,
            "warpFlag" => WarpStyle::Flag,
            "warpWave" => WarpStyle::Wave,
            "warpFish" => WarpStyle::Fish,
            "warpRise" => WarpStyle::Rise,
            "warpFisheye" => WarpStyle::Fisheye,
            "warpInflate" => WarpStyle::Inflate,
            "warpSqueeze" => WarpStyle::Squeeze,
            "warpTwist" => WarpStyle::Twist,
            _ => WarpStyle::None,
        }
    }
}

/// Warp settings; `bend`, `hdist` and `vdist` are fractions in `-1..=1`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Warp {
    pub style: WarpStyle,
    pub bend: f64,
    pub hdist: f64,
    pub vdist: f64,
    pub vertical: bool,
    /// A custom envelope, which replaces the preset.
    pub mesh: Option<Mesh>,
}

/// A custom (or quilt) envelope: a grid of bicubic patches in absolute coordinates.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Mesh {
    /// Patch edges along x and y in the unwarped rectangle, each including both ends.
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
    /// `(3 (xs.len() - 1) + 1) x (3 (ys.len() - 1) + 1)` control points, row-major.
    pub points: Vec<Point>,
}

impl Warp {
    pub const NONE: Warp =
        Warp { style: WarpStyle::None, bend: 0.0, hdist: 0.0, vdist: 0.0, vertical: false, mesh: None };

    /// Reads a warp descriptor; `rect` is the unwarped rectangle a custom mesh spans.
    pub fn from_descriptor(d: &Descriptor) -> Warp {
        let key = d.enumerated("warpStyle").unwrap_or("warpNone");
        let mesh = if key == "warpCustom" { custom_mesh(d) } else { None };
        Warp {
            style: if mesh.is_some() { WarpStyle::None } else { WarpStyle::from_key(key) },
            bend: d.num("warpValue").unwrap_or(0.0) / 100.0,
            hdist: d.num("warpPerspective").unwrap_or(0.0) / 100.0,
            vdist: d.num("warpPerspectiveOther").unwrap_or(0.0) / 100.0,
            vertical: d.enumerated("warpRotate") == Some("Vrtc"),
            mesh,
        }
    }

    pub fn is_identity(&self) -> bool {
        if let Some(m) = &self.mesh {
            return m.is_identity();
        }
        self.style == WarpStyle::None || self.bend.abs() + self.hdist.abs() + self.vdist.abs() < 1e-6
    }
}

fn numbers(v: &[Value]) -> Vec<f64> {
    v.iter()
        .filter_map(|v| match v {
            Value::Number(n) | Value::Unit(_, n) => Some(*n),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        })
        .collect()
}

/// The `customEnvelopeWarp` of a warp descriptor, spanning its `bounds`.
fn custom_mesh(d: &Descriptor) -> Option<Mesh> {
    let b = d.desc("bounds")?;
    let get = |k| b.num(k).unwrap_or(0.0);
    let (l, t, r, bt) = (get("Left"), get("Top "), get("Rght"), get("Btom"));
    if r <= l || bt <= t {
        return None;
    }
    let env = d.desc("customEnvelopeWarp")?;
    let pts = env.desc("meshPoints")?;
    let (hs, vs) = (numbers(pts.list("Hrzn")?), numbers(pts.list("Vrtc")?));
    let points: Vec<Point> = hs.into_iter().zip(vs).collect();
    let slices = |key: &str, lo: f64, hi: f64, n: usize| -> Vec<f64> {
        let given = env.list(key).map(numbers).unwrap_or_default();
        match given.len() {
            k if k == n + 1 => given,
            k if k + 1 == n && k > 0 => [vec![lo], given, vec![hi]].concat(),
            _ => (0..=n).map(|i| lo + (hi - lo) * i as f64 / n as f64).collect(),
        }
    };
    let cols = d.num("deformNumCols").map(|c| c as usize);
    let rows = d.num("deformNumRows").map(|c| c as usize);
    // Points per side are 3n + 1; take the grid shape from the descriptor or the point count.
    let (nx, ny) = match (cols, rows) {
        (Some(c), Some(r)) if c >= 4 && r >= 4 && (c - 1) % 3 == 0 && (r - 1) % 3 == 0 => ((c - 1) / 3, (r - 1) / 3),
        _ => {
            let x = env.list("quiltSliceX").map_or(0, |v| v.len());
            let y = env.list("quiltSliceY").map_or(0, |v| v.len());
            let guess = |k: usize| if k >= 2 { k - 1 } else { 1 };
            (guess(x), guess(y))
        }
    };
    if points.len() != (3 * nx + 1) * (3 * ny + 1) {
        return None;
    }
    Some(Mesh { xs: slices("quiltSliceX", l, r, nx), ys: slices("quiltSliceY", t, bt, ny), points })
}

impl Mesh {
    /// The undistorted mesh over `rect`.
    #[cfg(test)]
    pub fn flat(rect: [f64; 4]) -> Mesh {
        let [l, t, r, b] = rect;
        let points =
            (0..16).map(|k| (l + (r - l) * (k % 4) as f64 / 3.0, t + (b - t) * (k / 4) as f64 / 3.0)).collect();
        Mesh { xs: vec![l, r], ys: vec![t, b], points }
    }

    pub fn rect(&self) -> [f64; 4] {
        [self.xs[0], self.ys[0], *self.xs.last().unwrap(), *self.ys.last().unwrap()]
    }

    fn is_identity(&self) -> bool {
        let cols = 3 * (self.xs.len() - 1) + 1;
        self.points.iter().enumerate().all(|(k, &(x, y))| {
            let (i, j) = (k % cols, k / cols);
            let ex = lerp_slices(&self.xs, i);
            let ey = lerp_slices(&self.ys, j);
            (x - ex).abs() < 1e-3 && (y - ey).abs() < 1e-3
        })
    }

    /// Maps `(x, y)` of the unwarped rectangle through the patch containing it.
    pub fn eval(&self, x: f64, y: f64) -> Point {
        let (i, u) = cell(&self.xs, x);
        let (j, v) = cell(&self.ys, y);
        let cols = 3 * (self.xs.len() - 1) + 1;
        let p: Patch = std::array::from_fn(|k| self.points[(3 * j + k / 4) * cols + 3 * i + k % 4]);
        eval(&p, [0.0, 0.0, 1.0, 1.0], u, v)
    }
}

/// Position of control point `i` along slices `s` when undistorted.
fn lerp_slices(s: &[f64], i: usize) -> f64 {
    let (c, k) = ((i / 3).min(s.len() - 2), i - 3 * (i / 3).min(s.len() - 2));
    s[c] + (s[c + 1] - s[c]) * k as f64 / 3.0
}

/// The patch index along slices `s` holding `x`, and the parameter inside it (may leave 0..1 at the ends).
fn cell(s: &[f64], x: f64) -> (usize, f64) {
    let n = s.len() - 1;
    let i = s[1..n].iter().take_while(|&&e| x >= e).count();
    let w = s[i + 1] - s[i];
    (i, if w.abs() < 1e-12 { 0.0 } else { (x - s[i]) / w })
}

/// A warp ready to map points: a preset patch over a rectangle, or a custom mesh.
pub(crate) enum Envelope {
    Patch(Box<Patch>, [f64; 4]),
    Mesh(Mesh),
}

impl Envelope {
    /// The envelope of `warp`; presets span `rect`, custom meshes their own bounds.
    pub fn new(warp: &Warp, rect: [f64; 4]) -> Envelope {
        match &warp.mesh {
            Some(m) => Envelope::Mesh(m.clone()),
            None => Envelope::Patch(Box::new(patch(warp, rect)), rect),
        }
    }

    /// The unwarped rectangle.
    pub fn rect(&self) -> [f64; 4] {
        match self {
            Envelope::Patch(_, r) => *r,
            Envelope::Mesh(m) => m.rect(),
        }
    }

    pub fn eval(&self, x: f64, y: f64) -> Point {
        match self {
            Envelope::Patch(p, r) => eval(p, *r, x, y),
            Envelope::Mesh(m) => m.eval(x, y),
        }
    }
}

pub(crate) type Point = (f64, f64);
pub(crate) type Patch = [Point; 16];
type Rows = [[Point; 4]; 4];

const Z: Point = (0.0, 0.0);
const T: f64 = 1.0 / 3.0;
const S: f64 = 1.0 / 6.0;
const E: f64 = 1.0 / 18.0;
const Q: f64 = 1.0 / 24.0;

/// Control-point offsets at bend +50%, in units of the rectangle's width and height.
/// Offsets scale linearly with bend; negative bends negate them except for Squeeze.
const FISH: Rows = [
    [Z, (0.0, -1.0), (0.0, 1.0), Z],
    [Z, (0.0, -T), (0.0, T), Z],
    [Z, (0.0, T), (0.0, -T), Z],
    [Z, (0.0, 1.0), (0.0, -1.0), Z],
];
const FISHEYE: Rows = [[Z; 4], [Z, (-T, -T), (T, -T), Z], [Z, (-T, T), (T, T), Z], [Z; 4]];
const FLAG: Rows = [[Z, (0.0, -1.0), (0.0, 1.0), Z]; 4];
const INFLATE: Rows = [
    [Z, (0.0, -S), (0.0, -S), Z],
    [(-S, 0.0), (-Q, -Q), (Q, -Q), (S, 0.0)],
    [(-S, 0.0), (-Q, Q), (Q, Q), (S, 0.0)],
    [Z, (0.0, S), (0.0, S), Z],
];
const RISE: Rows = [[(0.0, 1.0), (0.0, 1.0), Z, Z]; 4];
const WAVE: Rows = [[Z; 4], [Z, (0.0, 2.0 * T), (0.0, -2.0 * T), Z], [Z, (0.0, 2.0 * T), (0.0, -2.0 * T), Z], [Z; 4]];
const SQUEEZE: Rows = [
    [Z, (0.0, -S), (0.0, -S), Z],
    [(S, 0.0), (E, 0.0), (-E, 0.0), (-S, 0.0)],
    [(S, 0.0), (E, 0.0), (-E, 0.0), (-S, 0.0)],
    [Z, (0.0, S), (0.0, S), Z],
];
const SQUEEZE_NEGATIVE: Rows = [
    [Z, (0.0, S), (0.0, S), Z],
    [(-S, 0.0), (0.0, E), (0.0, E), (S, 0.0)],
    [(-S, 0.0), (0.0, -E), (0.0, -E), (S, 0.0)],
    [Z, (0.0, -S), (0.0, -S), Z],
];

fn offsets(style: WarpStyle, bend: f64) -> Option<(Rows, f64)> {
    let table = match style {
        WarpStyle::Fish => FISH,
        WarpStyle::Fisheye => FISHEYE,
        WarpStyle::Flag => FLAG,
        WarpStyle::Inflate => INFLATE,
        WarpStyle::Rise => RISE,
        WarpStyle::Wave => WAVE,
        WarpStyle::Squeeze if bend < 0.0 => return Some((SQUEEZE_NEGATIVE, -bend / 0.5)),
        WarpStyle::Squeeze => SQUEEZE,
        _ => return None,
    };
    Some((table, bend / 0.5))
}

fn lerp(a: Point, b: Point, t: f64) -> Point {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

fn mid(a: Point, b: Point) -> Point {
    lerp(a, b, 0.5)
}

fn scale_about(p: Point, c: Point, f: f64) -> Point {
    (c.0 + (p.0 - c.0) * f, c.1 + (p.1 - c.1) * f)
}

fn set_rows(p: &mut Patch, top: [Point; 4], bottom: [Point; 4]) {
    for j in 0..4 {
        for i in 0..4 {
            p[j * 4 + i] = lerp(top[i], bottom[i], j as f64 / 3.0);
        }
    }
}

fn row(p: &Patch, j: usize) -> [Point; 4] {
    [p[j * 4], p[j * 4 + 1], p[j * 4 + 2], p[j * 4 + 3]]
}

/// Builds the 16 control points (row-major, top row first) for `warp` over `rect = [l, t, r, b]`.
pub(crate) fn patch(warp: &Warp, rect: [f64; 4]) -> Patch {
    let [l, t, r, b] = rect;
    let (w, h) = (r - l, b - t);
    let cx = (l + r) / 2.0;
    let mut p = [Z; 16];
    for j in 0..4 {
        for i in 0..4 {
            p[j * 4 + i] = (l + w * i as f64 / 3.0, t + h * j as f64 / 3.0);
        }
    }
    let k = warp.bend.clamp(-1.0, 1.0);
    let theta = k.abs() * PI;

    let arc_edge = |y: f64, bulge_up: bool| -> [Point; 4] {
        if theta < 1e-9 {
            return [(l, y), (l + w / 3.0, y), (r - w / 3.0, y), (r, y)];
        }
        let radius = w / 2.0 / (theta / 2.0).sin();
        let handle = 4.0 / 3.0 * (theta / 4.0).tan() * radius;
        let dir = if bulge_up { -1.0 } else { 1.0 };
        let (c, s) = ((theta / 2.0).cos(), (theta / 2.0).sin());
        [(l, y), (l + handle * c, y + dir * handle * s), (r - handle * c, y + dir * handle * s), (r, y)]
    };

    let concentric = |bend: f64| -> Patch {
        let mut q = p;
        let theta = bend.abs() * PI;
        if theta < 1e-9 {
            return q;
        }
        let radius = w / 2.0 / (theta / 2.0).sin();
        let (c, s) = ((theta / 2.0).cos(), (theta / 2.0).sin());
        let cy = b + radius * c;
        let ring = |rr: f64| -> [Point; 4] {
            let handle = 4.0 / 3.0 * (theta / 4.0).tan() * rr;
            let p0 = (cx - rr * s, cy - rr * c);
            let p3 = (cx + rr * s, cy - rr * c);
            [p0, (p0.0 + handle * c, p0.1 - handle * s), (p3.0 - handle * c, p3.1 - handle * s), p3]
        };
        set_rows(&mut q, ring(radius + h), ring(radius));
        if bend < 0.0 {
            let src = q;
            for j in 0..4 {
                for i in 0..4 {
                    let (x, y) = src[(3 - j) * 4 + i];
                    q[j * 4 + i] = (x, t + b - y);
                }
            }
        }
        q
    };

    match warp.style {
        WarpStyle::None => {}
        WarpStyle::Arc => p = concentric(k),
        WarpStyle::ArcLower => {
            let top = row(&p, 0);
            set_rows(&mut p, top, arc_edge(b, k < 0.0));
        }
        WarpStyle::ArcUpper => {
            let bottom = row(&p, 3);
            set_rows(&mut p, arc_edge(t, k > 0.0), bottom);
        }
        WarpStyle::Arch => set_rows(&mut p, arc_edge(t, k > 0.0), arc_edge(b, k > 0.0)),
        WarpStyle::Bulge => set_rows(&mut p, arc_edge(t, k > 0.0), arc_edge(b, k < 0.0)),
        WarpStyle::ShellUpper => {
            let a = concentric(k);
            for i in [0, 1, 2, 3, 4, 7] {
                p[i] = a[i];
            }
        }
        WarpStyle::ShellLower => {
            let a = concentric(-k);
            for i in [8, 11, 12, 13, 14, 15] {
                p[i] = a[i];
            }
        }
        WarpStyle::Twist => {
            let c = (cx, (t + b) / 2.0);
            let (angle, scale) = (k * PI / 2.0, 1.0 + 2.0 * k.abs());
            let (sin, cos) = angle.sin_cos();
            for i in [5, 6, 9, 10] {
                let (dx, dy) = ((p[i].0 - c.0) * scale, (p[i].1 - c.1) * scale);
                p[i] = (c.0 + dx * cos - dy * sin, c.1 + dx * sin + dy * cos);
            }
        }
        style => {
            if let Some((table, f)) = offsets(style, k) {
                for (i, &(dx, dy)) in table.iter().flatten().enumerate() {
                    p[i].0 += dx * f * w;
                    p[i].1 += dy * f * h;
                }
            }
        }
    }
    if warp.hdist != 0.0 {
        distort_horizontal(&mut p, warp.hdist);
    }
    if warp.vdist != 0.0 {
        distort_vertical(&mut p, warp.vdist);
    }
    p
}

/// Column `i` scales about the midpoint of its end points by `1 + h(2i/3 - 1)`.
fn distort_horizontal(p: &mut Patch, h: f64) {
    for i in 0..4 {
        let f = 1.0 + h * (2.0 * i as f64 / 3.0 - 1.0);
        let c = mid(p[i], p[12 + i]);
        for j in 0..4 {
            p[j * 4 + i] = scale_about(p[j * 4 + i], c, f);
        }
    }
}

/// Top and bottom rows scale about their chord midpoints by `1 - v` and `1 + v`; inner rows follow linearly.
fn distort_vertical(p: &mut Patch, v: f64) {
    let shift = |j: usize, f: f64| -> [Point; 4] {
        let c = mid(p[j * 4], p[j * 4 + 3]);
        std::array::from_fn(|i| {
            let q = p[j * 4 + i];
            let s = scale_about(q, c, f);
            (s.0 - q.0, s.1 - q.1)
        })
    };
    let (top, bottom) = (shift(0, 1.0 - v), shift(3, 1.0 + v));
    for j in 0..4 {
        let t = j as f64 / 3.0;
        for i in 0..4 {
            let d = lerp(top[i], bottom[i], t);
            p[j * 4 + i].0 += d.0;
            p[j * 4 + i].1 += d.1;
        }
    }
}

/// Maps `(x, y)` inside `rect` through the patch.
pub(crate) fn eval(p: &Patch, rect: [f64; 4], x: f64, y: f64) -> Point {
    let u = (x - rect[0]) / (rect[2] - rect[0]);
    let v = (y - rect[1]) / (rect[3] - rect[1]);
    let bernstein = |t: f64| {
        let s = 1.0 - t;
        [s * s * s, 3.0 * t * s * s, 3.0 * t * t * s, t * t * t]
    };
    let (bu, bv) = (bernstein(u), bernstein(v));
    let mut out = (0.0, 0.0);
    for j in 0..4 {
        for i in 0..4 {
            let k = bv[j] * bu[i];
            out.0 += k * p[j * 4 + i].0;
            out.1 += k * p[j * 4 + i].1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECT: [f64; 4] = [10.0, 20.0, 110.0, 70.0];
    const ALL: [WarpStyle; 15] = [
        WarpStyle::Arc,
        WarpStyle::ArcLower,
        WarpStyle::ArcUpper,
        WarpStyle::Arch,
        WarpStyle::Bulge,
        WarpStyle::ShellLower,
        WarpStyle::ShellUpper,
        WarpStyle::Flag,
        WarpStyle::Wave,
        WarpStyle::Fish,
        WarpStyle::Rise,
        WarpStyle::Fisheye,
        WarpStyle::Inflate,
        WarpStyle::Squeeze,
        WarpStyle::Twist,
    ];

    fn warp(style: WarpStyle, bend: f64) -> Warp {
        Warp { style, bend, ..Warp::NONE }
    }

    fn near(a: Point, b: Point) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn zero_bend_is_identity_for_every_style() {
        for style in ALL {
            let p = patch(&warp(style, 0.0), RECT);
            for (x, y) in [(10.0, 20.0), (35.0, 33.0), (110.0, 70.0), (60.0, 45.0)] {
                assert!(near(eval(&p, RECT, x, y), (x, y)), "{style:?}");
            }
        }
    }

    #[test]
    fn custom_mesh_maps_through_its_patches() {
        let mut m = Mesh::flat(RECT);
        assert!(Warp { mesh: Some(m.clone()), ..Warp::NONE }.is_identity());
        assert!(near(m.eval(35.0, 33.0), (35.0, 33.0)));
        // Pull the bottom-right corner out: it moves, the opposite corner stays.
        m.points[15] = (130.0, 90.0);
        assert!(near(m.eval(110.0, 70.0), (130.0, 90.0)));
        assert!(near(m.eval(10.0, 20.0), (10.0, 20.0)));
        assert!(!Warp { mesh: Some(m), ..Warp::NONE }.is_identity());
    }

    #[test]
    fn quilt_mesh_picks_the_patch_by_slice() {
        // Two patches side by side; shift every control point of the right one down by 10.
        let xs = vec![0.0, 30.0, 100.0];
        let ys = vec![0.0, 60.0];
        let points: Vec<Point> = (0..4)
            .flat_map(|j| {
                let xs = xs.clone();
                (0..7).map(move |i| {
                    let x = lerp_slices(&xs, i);
                    (x, 20.0 * j as f64 + if i > 3 { 10.0 } else { 0.0 })
                })
            })
            .collect();
        let m = Mesh { xs, ys, points };
        assert!(near(m.eval(15.0, 30.0), (15.0, 30.0)));
        let (_, y) = m.eval(100.0, 30.0);
        assert!((y - 40.0).abs() < 1e-9, "{y}");
    }

    #[test]
    fn identity_detection() {
        assert!(Warp::NONE.is_identity());
        assert!(warp(WarpStyle::Arc, 0.0).is_identity());
        assert!(!warp(WarpStyle::Arc, 0.2).is_identity());
        assert!(!Warp { hdist: 0.1, ..warp(WarpStyle::Flag, 0.0) }.is_identity());
    }

    #[test]
    fn mirrored_styles_flip_with_bend_sign() {
        for style in [WarpStyle::Flag, WarpStyle::Wave, WarpStyle::Fish, WarpStyle::Rise] {
            let (pos, neg) = (patch(&warp(style, 0.4), RECT), patch(&warp(style, -0.4), RECT));
            for (x, y) in [(30.0, 25.0), (80.0, 60.0)] {
                let (a, b) = (eval(&pos, RECT, x, y), eval(&neg, RECT, x, y));
                assert!((a.1 - y + (b.1 - y)).abs() < 1e-9, "{style:?}");
                assert!((a.0 - b.0).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn arc_bends_the_baseline_up() {
        let p = patch(&warp(WarpStyle::Arc, 0.5), RECT);
        let centre = eval(&p, RECT, 60.0, 70.0);
        let corner = eval(&p, RECT, 10.0, 70.0);
        assert!(centre.1 < corner.1 - 5.0);
        assert!((centre.0 - 60.0).abs() < 1e-9);
    }

    #[test]
    fn arch_keeps_the_rectangle_corners() {
        let p = patch(&warp(WarpStyle::Arch, 0.7), RECT);
        assert!(near(eval(&p, RECT, 10.0, 20.0), (10.0, 20.0)));
        assert!(near(eval(&p, RECT, 110.0, 70.0), (110.0, 70.0)));
    }

    #[test]
    fn flag_at_half_bend_matches_table() {
        let p = patch(&warp(WarpStyle::Flag, 0.5), RECT);
        let h = RECT[3] - RECT[1];
        assert!(near(p[1], (10.0 + 100.0 / 3.0, 20.0 - h)));
        assert!(near(p[2], (10.0 + 200.0 / 3.0, 20.0 + h)));
    }

    #[test]
    fn squeeze_negative_uses_its_own_table() {
        let p = patch(&warp(WarpStyle::Squeeze, -0.5), RECT);
        assert!(near(p[5], (10.0 + 100.0 / 3.0, 20.0 + 50.0 / 3.0 + 50.0 / 18.0)));
        assert!(near(p[4], (10.0 - 100.0 / 6.0, 20.0 + 50.0 / 3.0)));
    }

    #[test]
    fn horizontal_distortion_scales_columns() {
        let p = patch(&Warp { hdist: 0.5, ..warp(WarpStyle::Flag, 0.0) }, RECT);
        let left_height = p[12].1 - p[0].1;
        let right_height = p[15].1 - p[3].1;
        assert!((left_height - 25.0).abs() < 1e-9);
        assert!((right_height - 75.0).abs() < 1e-9);
    }

    #[test]
    fn vertical_distortion_scales_rows() {
        let p = patch(&Warp { vdist: 0.5, ..warp(WarpStyle::Flag, 0.0) }, RECT);
        assert!((p[3].0 - p[0].0 - 50.0).abs() < 1e-9);
        assert!((p[15].0 - p[12].0 - 150.0).abs() < 1e-9);
    }

    #[test]
    fn parses_style_keys() {
        assert_eq!(WarpStyle::from_key("warpFisheye"), WarpStyle::Fisheye);
        assert_eq!(WarpStyle::from_key("warpNone"), WarpStyle::None);
    }
}
