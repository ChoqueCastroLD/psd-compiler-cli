//! Photoshop warp presets as bicubic Bezier patches over the warp rectangle.

use std::f64::consts::PI;

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
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Warp {
    pub style: WarpStyle,
    pub bend: f64,
    pub hdist: f64,
    pub vdist: f64,
    pub vertical: bool,
}

impl Warp {
    #[cfg(test)]
    pub const NONE: Warp = Warp { style: WarpStyle::None, bend: 0.0, hdist: 0.0, vdist: 0.0, vertical: false };

    pub fn is_identity(&self) -> bool {
        self.style == WarpStyle::None || self.bend.abs() + self.hdist.abs() + self.vdist.abs() < 1e-6
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
