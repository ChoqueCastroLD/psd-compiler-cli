//! Adjustment layers: each block becomes a color function applied to the pixels below.

use super::fill::Gradient;
use crate::blend::{lum, set_lum};
use crate::color::{self, ColorSpace};
use crate::psd::descriptor;
use crate::psd::reader::Reader;
use crate::psd::{ColorMode, Layer, ADJUSTMENT_KEYS};

/// A color transform on straight RGB in 0..=1.
pub(crate) type ColorFn = Box<dyn Fn([f32; 3]) -> [f32; 3] + Send + Sync>;

/// Parses the adjustment of layer `l`; `Err` names an adjustment that is not supported.
pub(crate) fn parse(l: &Layer, cs: &ColorSpace, mode: ColorMode) -> Result<ColorFn, &'static str> {
    let Some((key, data)) = ADJUSTMENT_KEYS.iter().find_map(|k| l.block(k).map(|b| (**k, b))) else {
        return Err("unknown adjustment");
    };
    let r = &mut Reader::new(data);
    // Adjustments that treat each channel alike, which CMYK plate passes can run directly.
    let separable = matches!(&key, b"levl" | b"curv" | b"brit" | b"CgEd" | b"expA" | b"nvrt" | b"post" | b"thrs");
    let f = match &key {
        b"levl" => levels(r, cs),
        b"curv" => curves(data, cs),
        b"brit" | b"CgEd" => brightness_contrast(l),
        b"hue2" | b"hue " => hue_saturation(r),
        b"blnc" => color_balance(r),
        b"vibA" => vibrance(data),
        b"expA" => exposure(r, if mode == ColorMode::Grayscale { 1.75 } else { 2.2 }),
        b"selc" => selective_color(r),
        b"mixr" => channel_mixer(r),
        b"grdm" => gradient_map(r),
        b"phfl" => photo_filter(r, cs),
        b"nvrt" => Some(Box::new(|c: [f32; 3]| c.map(|v| 1.0 - v)) as ColorFn),
        b"post" => posterize(r),
        b"thrs" => threshold(r, cs),
        b"blwh" => black_white(data, cs),
        b"clrL" => {
            // Version and descriptor version, then the descriptor.
            let d = descriptor::parse_block(data, 6).map_err(|_| "malformed color lookup")?;
            return match super::lut::parse(&d)? {
                super::lut::Lookup::Identity => Ok(Box::new(|c| c)),
                super::lut::Lookup::Table(t) => Ok(Box::new(move |c| t.apply(c))),
            };
        }
        _ => None,
    };
    let f = f.ok_or("malformed adjustment")?;
    if cs.plane() != Some(1) || separable {
        return Ok(f);
    }
    // The black pass holds black and the color's luminance: adjust both as grays.
    Ok(Box::new(move |c: [f32; 3]| {
        let l = lum(f([c[1]; 3]));
        [f([c[0]; 3])[0], l, l]
    }))
}

pub(crate) fn lut_fn(luts: [Option<Vec<f32>>; 4]) -> ColorFn {
    let apply = |lut: &[f32], v: f32| {
        let x = v.clamp(0.0, 1.0) * (lut.len() - 1) as f32;
        let i = (x as usize).min(lut.len() - 2);
        let t = x - i as f32;
        lut[i] + (lut[i + 1] - lut[i]) * t
    };
    Box::new(move |c| {
        let mut o = c;
        for ch in 0..3 {
            if let Some(l) = &luts[ch + 1] {
                o[ch] = apply(l, o[ch]);
            }
        }
        if let Some(l) = &luts[0] {
            o = o.map(|v| apply(l, v));
        }
        o
    })
}

/// The composite table and the tables of the channels this pass works on, from the composite and
/// up to four per-channel tables.
fn channel_luts(luts: [Option<Vec<f32>>; 5], cs: &ColorSpace) -> [Option<Vec<f32>>; 4] {
    let [all, a, b, c, k] = luts;
    match cs.plane() {
        Some(1) => [all, k, None, None],
        _ => [all, a, b, c],
    }
}

pub(crate) fn table(f: impl Fn(f64) -> f64) -> Vec<f32> {
    (0..1024).map(|i| f(i as f64 / 1023.0).clamp(0.0, 1.0) as f32).collect()
}

fn levels(r: &mut Reader, cs: &ColorSpace) -> Option<ColorFn> {
    r.u16().ok()?;
    let mut luts: [Option<Vec<f32>>; 5] = Default::default();
    for lut in &mut luts {
        let v: Vec<f64> = (0..5).map(|_| r.u16().map(|x| x as f64)).collect::<Result<_, _>>().ok()?;
        let (ib, iw, ob, ow, g) =
            (v[0] / 255.0, v[1] / 255.0, v[2] / 255.0, v[3] / 255.0, (v[4] / 100.0).clamp(0.01, 9.99));
        if (ib, iw, ob, ow, g) == (0.0, 1.0, 0.0, 1.0, 1.0) {
            continue;
        }
        let scale = if (iw - ib).abs() > 1e-9 { iw - ib } else { 1.0 };
        *lut = Some(table(|t| ((t - ib) / scale).clamp(0.0, 1.0).powf(1.0 / g) * (ow - ob) + ob));
    }
    Some(lut_fn(channel_luts(luts, cs)))
}

/// Natural cubic spline through `pts` (sorted by x), flat outside the end points.
pub(crate) fn spline(pts: &[(f64, f64)]) -> impl Fn(f64) -> f64 + '_ {
    let n = pts.len();
    let mut m = vec![0.0; n];
    if n > 2 {
        let mut c = vec![0.0; n];
        let mut d = vec![0.0; n];
        for i in 1..n - 1 {
            let (h0, h1) = (pts[i].0 - pts[i - 1].0, pts[i + 1].0 - pts[i].0);
            let rhs = 6.0 * ((pts[i + 1].1 - pts[i].1) / h1 - (pts[i].1 - pts[i - 1].1) / h0);
            let b = 2.0 * (h0 + h1) - h0 * c[i - 1];
            c[i] = h1 / b;
            d[i] = (rhs - h0 * d[i - 1]) / b;
        }
        for i in (1..n - 1).rev() {
            m[i] = d[i] - c[i] * m[i + 1];
        }
    }
    move |x| {
        if x <= pts[0].0 {
            return pts[0].1;
        }
        if x >= pts[n - 1].0 {
            return pts[n - 1].1;
        }
        let i = pts.windows(2).position(|w| x <= w[1].0).unwrap_or(n - 2);
        let (x0, y0, x1, y1) = (pts[i].0, pts[i].1, pts[i + 1].0, pts[i + 1].1);
        let h = x1 - x0;
        let (a, b) = ((x1 - x) / h, (x - x0) / h);
        a * y0 + b * y1 + ((a * a * a - a) * m[i] + (b * b * b - b) * m[i + 1]) * h * h / 6.0
    }
}

fn curve_points(r: &mut Reader) -> Option<Vec<(f64, f64)>> {
    let n = r.u16().ok()? as usize;
    if !(2..=19).contains(&n) {
        return None;
    }
    let mut pts = Vec::with_capacity(n);
    for _ in 0..n {
        let out = r.u16().ok()? as f64 / 255.0;
        let inp = r.u16().ok()? as f64 / 255.0;
        pts.push((inp, out));
    }
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    pts.dedup_by(|a, b| a.0 == b.0);
    Some(pts)
}

fn curves(data: &[u8], cs: &ColorSpace) -> Option<ColorFn> {
    let mut r = Reader::new(data);
    let is_map = r.u8().ok()? != 0;
    let version = r.u16().ok()?;
    let bits = r.u32().ok()?;
    let mut luts: [Option<Vec<f32>>; 5] = Default::default();
    let read_curve = |r: &mut Reader| -> Option<Vec<f32>> {
        if is_map {
            let m = r.bytes(256).ok()?;
            Some(m.iter().map(|&v| v as f32 / 255.0).collect())
        } else {
            let pts = curve_points(r)?;
            Some(table(spline(&pts)))
        }
    };
    let ids: Vec<u32> =
        if version == 1 { (0..32).filter(|i| bits & (1 << i) != 0).collect() } else { (0..bits).collect() };
    for id in ids {
        let c = read_curve(&mut r)?;
        if let Some(slot) = luts.get_mut(id as usize) {
            *slot = Some(c);
        }
    }
    if r.remaining() >= 10 && r.tag().ok() == Some(*b"Crv ") {
        r.u16().ok()?;
        let count = r.u32().ok()?;
        for _ in 0..count {
            let id = r.u16().ok()? as usize;
            let c = read_curve(&mut r)?;
            if let Some(slot) = luts.get_mut(id) {
                *slot = Some(c);
            }
        }
    }
    Some(lut_fn(channel_luts(luts, cs)))
}

fn brightness_contrast(l: &Layer) -> Option<ColorFn> {
    let d = l.block(b"CgEd").and_then(|b| descriptor::parse_block(b, 4).ok());
    let (b, c, legacy) = match &d {
        Some(d) => (d.num("Brgh").unwrap_or(0.0), d.num("Cntr").unwrap_or(0.0), d.bool("useLegacy").unwrap_or(false)),
        None => {
            let mut r = Reader::new(l.block(b"brit")?);
            (r.i16().ok()? as f64, r.i16().ok()? as f64, true)
        }
    };
    Some(brightness_contrast_fn(b, c, legacy))
}

/// Brightness and contrast in -150..=150 and -100..=100 (legacy: -100..=100 for both).
pub(crate) fn brightness_contrast_fn(b: f64, c: f64, legacy: bool) -> ColorFn {
    if legacy {
        // Raising contrast brightens first; lowering it brightens the flattened result.
        let (k, b) = (if c >= 0.0 { 100.0 / (100.0 - c).max(0.5) } else { (100.0 + c) / 100.0 }, b / 255.0);
        let lut = table(|t| if c >= 0.0 { (t + b - 0.5) * k + 0.5 } else { (t - 0.5) * k + 0.5 + b });
        return lut_fn([Some(lut), None, None, None]);
    }
    let bb = b / 150.0;
    let cc = c / 100.0;
    let xs = [0.0, 63.0 / 255.0, 191.0 / 255.0, 1.0];
    let ys = [0.0, xs[1] - cc * 25.0 / 255.0, xs[2] + cc * 25.0 / 255.0, 1.0];
    let pts: Vec<(f64, f64)> = xs.iter().copied().zip(ys).collect();
    let contrast = spline(&pts);
    let pol = |a: f64, x: f64, r: f64| a * x.powf(r);
    let bright = |t: f64| {
        let ba = bb.abs();
        let h = 0.5
            * (ba * (pol(1.65, t, 0.35) + pol(-1.0, t, 10.0))
                + (1.0 - ba) * (pol(1.96, t, 0.4) + pol(1.0, t, 4.0))
                + pol(1.0, t, 1.25));
        bb * t * (1.0 - t) * h
    };
    let n = 1024;
    let rotated: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            (t - bright(t), contrast(t) + bright(t))
        })
        .collect();
    let lut = table(|t| {
        let i = rotated.partition_point(|p| p.0 < t);
        if i == 0 {
            rotated[0].1
        } else if i >= n {
            rotated[n - 1].1
        } else {
            let (a, b) = (rotated[i - 1], rotated[i]);
            let w = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 0.0 };
            a.1 + (b.1 - a.1) * w
        }
    });
    lut_fn([Some(lut), None, None, None])
}

pub(crate) fn rgb_to_hsl(c: [f32; 3]) -> [f32; 3] {
    let hi = c[0].max(c[1]).max(c[2]);
    let lo = c[0].min(c[1]).min(c[2]);
    let l = (hi + lo) / 2.0;
    let d = hi - lo;
    if d <= 1e-9 {
        return [0.0, 0.0, l];
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-9);
    let h = if hi == c[0] {
        ((c[1] - c[2]) / d).rem_euclid(6.0)
    } else if hi == c[1] {
        (c[2] - c[0]) / d + 2.0
    } else {
        (c[0] - c[1]) / d + 4.0
    };
    [h / 6.0, s.min(1.0), l]
}

pub(crate) fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h6 = h.rem_euclid(1.0) * 6.0;
    let x = c * (1.0 - (h6 % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h6 as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r + m, g + m, b + m]
}

fn lighten(v: f32, l: f32) -> f32 {
    if l > 0.0 {
        v * (1.0 - l) + l
    } else {
        v * (1.0 + l)
    }
}

fn saturate(s: f32, k: f32) -> f32 {
    (if k > 0.0 { s / (1.0 - k + 0.01) } else { s * (1.0 + k) }).clamp(0.0, 1.0)
}

/// How two hue-range saturation changes combine, sampled on a -100..=100 grid in steps of 10.
/// Empirical values from psd-tools (MIT).
#[rustfmt::skip]
const SATURATION_GRID: [[i8; 21]; 21] = [
    [100,100,100,100,100,100,100,100,100,100,100,100,100,100,100,100,100,100,100,100,100],
    [100, 94, 92, 91, 91, 90, 90, 90, 90, 90, 90, 89, 89, 89, 89, 89, 89, 89, 88, 88, 88],
    [100, 92, 89, 86, 84, 83, 82, 81, 81, 80, 80, 79, 79, 78, 78, 77, 77, 76, 76, 75, 74],
    [100, 91, 86, 82, 79, 77, 74, 73, 71, 70, 70, 68, 67, 66, 65, 64, 62, 61, 59, 58, 56],
    [100, 91, 84, 79, 75, 71, 68, 65, 63, 61, 60, 58, 56, 54, 52, 49, 47, 44, 41, 36, 32],
    [100, 90, 83, 77, 71, 66, 62, 59, 55, 52, 50, 47, 44, 41, 37, 33, 28, 22, 17,  9, -1],
    [100, 90, 82, 74, 68, 62, 57, 52, 47, 43, 40, 36, 31, 26, 21, 13,  5, -5,-15,-25,-35],
    [100, 90, 81, 73, 65, 59, 52, 46, 39, 35, 30, 24, 19, 11,  2, -8,-18,-28,-38,-48,-58],
    [100, 90, 81, 71, 63, 55, 47, 39, 33, 26, 20, 13,  4, -6,-16,-26,-36,-46,-56,-66,-76],
    [100, 90, 80, 70, 61, 52, 43, 35, 26, 17, 10,  1, -9,-19,-29,-39,-49,-59,-69,-79,-89],
    [100, 90, 80, 70, 60, 50, 40, 30, 20, 10,  0,-10,-20,-30,-40,-49,-60,-70,-80,-90,-100],
    [100, 89, 79, 68, 58, 47, 36, 24, 13,  1,-10,-19,-30,-40,-50,-60,-70,-79,-89,-99,-100],
    [100, 89, 79, 67, 56, 44, 31, 19,  4, -9,-20,-30,-40,-50,-60,-70,-79,-90,-99,-100,-100],
    [100, 89, 78, 66, 54, 41, 26, 11, -6,-19,-30,-40,-50,-59,-70,-79,-89,-99,-100,-100,-100],
    [100, 89, 78, 65, 52, 37, 21,  2,-16,-29,-40,-50,-60,-70,-79,-90,-99,-100,-100,-100,-100],
    [100, 89, 77, 64, 49, 33, 13, -8,-26,-39,-49,-60,-70,-79,-90,-99,-100,-100,-100,-100,-100],
    [100, 89, 77, 62, 47, 28,  5,-18,-36,-49,-60,-70,-79,-89,-99,-100,-100,-100,-100,-100,-100],
    [100, 89, 76, 61, 44, 22, -5,-28,-46,-59,-70,-79,-90,-99,-100,-100,-100,-100,-100,-100,-100],
    [100, 88, 76, 59, 41, 17,-15,-38,-56,-69,-80,-89,-99,-100,-100,-100,-100,-100,-100,-100,-100],
    [100, 88, 75, 58, 36,  9,-25,-48,-66,-79,-90,-99,-100,-100,-100,-100,-100,-100,-100,-100,-100],
    [100, 88, 74, 56, 32, -1,-35,-58,-76,-89,-100,-100,-100,-100,-100,-100,-100,-100,-100,-100,-100],
];

fn combine_saturation(a: f32, b: f32) -> f32 {
    let g = |i: usize, j: usize| SATURATION_GRID[20 - i][20 - j] as f32 / 100.0;
    let (x, y) = ((a.clamp(-1.0, 1.0) + 1.0) * 10.0, (b.clamp(-1.0, 1.0) + 1.0) * 10.0);
    let (i, j) = ((x as usize).min(19), (y as usize).min(19));
    let (u, v) = (x - i as f32, y - j as f32);
    let top = g(i, j) + (g(i, j + 1) - g(i, j)) * v;
    let bottom = g(i + 1, j) + (g(i + 1, j + 1) - g(i + 1, j)) * v;
    top + (bottom - top) * u
}

/// Hue/Saturation, following the reverse-engineered model of psd-tools (MIT).
fn hue_saturation(r: &mut Reader) -> Option<ColorFn> {
    r.u16().ok()?;
    let colorize = r.u8().ok()? != 0;
    r.u8().ok()?;
    let triple = |r: &mut Reader| -> Option<[f32; 3]> {
        Some([r.i16().ok()? as f32 / 360.0, r.i16().ok()? as f32 / 100.0, r.i16().ok()? as f32 / 100.0])
    };
    let col = triple(r)?;
    let master = triple(r)?;
    let mut ranges = vec![];
    for _ in 0..6 {
        let range: Vec<f32> = (0..4).map(|_| r.i16().map(|v| v as f32 / 360.0)).collect::<Result<_, _>>().ok()?;
        let hsl = triple(r)?;
        if hsl != [0.0; 3] {
            ranges.push(([range[0], range[1], range[2], range[3]], hsl));
        }
    }
    if colorize {
        return Some(Box::new(move |c| {
            let hsl = rgb_to_hsl(c);
            hsl_to_rgb(col[0].rem_euclid(1.0), col[1], lighten(hsl[2], col[2]).clamp(0.0, 1.0))
        }));
    }
    Some(Box::new(move |c| {
        let c = c.map(|v| lighten(v, master[2]).clamp(0.0, 1.0));
        let [h, s, l] = rgb_to_hsl(c);
        let (mut dh, mut ds, mut dl) = (0.0, 0.0, 0.0);
        for (rg, adj) in &ranges {
            let m = range_weight(h, rg);
            dh += adj[0] * m;
            dl = (dl + adj[2] * m).clamp(-1.0, 1.0);
            let share = if adj[1] > 0.0 { 1.0 - (1.0 - m).powf(1.5 / (1.0 - adj[1].powi(4) + 0.05)) } else { m };
            ds = combine_saturation(ds, adj[1] * share);
        }
        let v = l + s * l.min(1.0 - l);
        let sv = if v > 1e-9 { 2.0 * (1.0 - l / v) } else { 0.0 };
        let (v, sv) = if dl >= 0.0 {
            (v, (sv * (1.0 - dl)).clamp(0.0, 1.0))
        } else {
            let div = 1.0 / (sv + 1e-3);
            ((v * (1.0 + dl * sv)).clamp(0.0, 1.0), (1.0 + (div - 1.0) / (dl.abs() - div + 1e-3)).clamp(0.0, 1.0))
        };
        let l = v * (1.0 - sv * 0.5);
        let d = l.min(1.0 - l);
        let s = if d > 1e-9 { (v - l) / d } else { 0.0 };
        let s = saturate(saturate(s, master[1]), ds);
        hsl_to_rgb((h + dh + master[0]).rem_euclid(1.0), s, l)
    }))
}

/// Trapezoid weight of hue `h` in a hue range `[bl, tl, tr, br]` (all in turns).
fn range_weight(h: f32, r: &[f32; 4]) -> f32 {
    let x = (h - r[0]).rem_euclid(1.0);
    let (a, b, c) = ((r[1] - r[0]).rem_euclid(1.0), (r[2] - r[0]).rem_euclid(1.0), (r[3] - r[0]).rem_euclid(1.0));
    if x >= a && x <= b {
        1.0
    } else if x < a {
        if a > 1e-9 {
            x / a
        } else {
            0.0
        }
    } else if x <= c && c - b > 1e-9 {
        (c - x) / (c - b)
    } else {
        0.0
    }
}

fn color_balance(r: &mut Reader) -> Option<ColorFn> {
    let mut v = [[0f32; 3]; 3];
    for range in &mut v {
        for c in range.iter_mut() {
            *c = r.i16().ok()? as f32 / 100.0;
        }
    }
    let preserve = r.u8().unwrap_or(1) != 0;
    Some(Box::new(move |c| {
        let mut o = [0f32; 3];
        for ch in 0..3 {
            let x = c[ch];
            let shadows = ((0.333 - x) / 0.25 + 0.5).clamp(0.0, 1.0) * 0.7;
            let mid_a = ((x - 0.333) / 0.25 + 0.5).clamp(0.0, 1.0);
            let mid_b = ((1.0 - x - 0.333) / 0.25 + 0.5).clamp(0.0, 1.0);
            let mids = mid_a * mid_b * 0.7;
            let highs = ((x - 0.667) / 0.25 + 0.5).clamp(0.0, 1.0) * 0.7;
            o[ch] = (x + v[0][ch] * shadows + v[1][ch] * mids + v[2][ch] * highs).clamp(0.0, 1.0);
        }
        if preserve {
            let hsl = rgb_to_hsl(o);
            hsl_to_rgb(hsl[0], hsl[1], rgb_to_hsl(c)[2])
        } else {
            o
        }
    }))
}

fn vibrance(data: &[u8]) -> Option<ColorFn> {
    let d = descriptor::parse_block(data, 4).ok()?;
    let vib = d.num("vibrance").unwrap_or(0.0) as f32 / 100.0;
    let sat = d.num("Strt").unwrap_or(0.0) as f32 / 100.0;
    Some(Box::new(move |c| {
        let hsl = rgb_to_hsl(c);
        let s = hsl[1];
        let boost = if vib > 0.0 { vib * (1.0 - s) * (1.0 - s) } else { vib };
        let s = saturate(saturate(s, boost.clamp(-1.0, 1.0)), sat);
        hsl_to_rgb(hsl[0], s, hsl[2])
    }))
}

fn exposure(r: &mut Reader, trc: f64) -> Option<ColorFn> {
    r.u16().ok()?;
    let read = |r: &mut Reader| r.u32().map(|b| f32::from_bits(b) as f64);
    let (e, offset, gamma) = (read(r).ok()?, read(r).ok()?, read(r).ok()?.clamp(0.01, 9.99));
    let lut = table(|t| ((t.powf(trc) * e.exp2() + offset).clamp(0.0, 1.0)).powf(1.0 / gamma).powf(1.0 / trc));
    Some(lut_fn([Some(lut), None, None, None]))
}

fn selective_color(r: &mut Reader) -> Option<ColorFn> {
    r.u16().ok()?;
    let absolute = r.u16().ok()? == 1;
    let mut adj = [[0f32; 4]; 10];
    for a in &mut adj {
        for v in a.iter_mut() {
            *v = r.i16().ok()? as f32 / 100.0;
        }
    }
    Some(Box::new(move |c| {
        let hi = c[0].max(c[1]).max(c[2]);
        let lo = c[0].min(c[1]).min(c[2]);
        let mid = c[0] + c[1] + c[2] - hi - lo;
        let mut weights = [0f32; 10];
        let primary = if hi == c[0] {
            1
        } else if hi == c[1] {
            3
        } else {
            5
        };
        weights[primary] = hi - mid;
        let secondary = match (c[0] == lo, c[1] == lo) {
            (true, _) => 4,
            (_, true) => 6,
            _ => 2,
        };
        weights[secondary] = mid - lo;
        weights[7] = ((lo - 0.5) * 2.0).max(0.0);
        weights[9] = ((0.5 - hi) * 2.0).max(0.0);
        weights[8] = 1.0 - ((hi - 0.5).abs() + (lo - 0.5).abs());
        let k = 1.0 - hi;
        let mut o = c;
        for ch in 0..3 {
            let ink = 1.0 - c[ch];
            let mut delta = 0.0;
            for (i, w) in weights.iter().enumerate() {
                if *w <= 0.0 {
                    continue;
                }
                let a = adj[i];
                let scale = if absolute { 1.0 } else { ink };
                let d_ink = (a[ch] * scale).clamp(-ink, 1.0 - ink);
                let d_k = a[3] * if absolute { 1.0 } else { k };
                delta += w * (d_ink + d_k * (1.0 - ink));
            }
            o[ch] = (c[ch] - delta).clamp(0.0, 1.0);
        }
        o
    }))
}

fn channel_mixer(r: &mut Reader) -> Option<ColorFn> {
    r.u16().ok()?;
    let mono = r.u16().ok()? != 0;
    let mut rows = [[0f32; 5]; 4];
    for row in &mut rows {
        for v in row.iter_mut() {
            *v = match r.i16() {
                Ok(x) => x as f32 / 100.0,
                Err(_) => 0.0,
            };
        }
    }
    Some(Box::new(move |c| {
        let mix = |row: &[f32; 5]| row[0] * c[0] + row[1] * c[1] + row[2] * c[2] + row[4];
        if mono {
            [mix(&rows[0]); 3]
        } else {
            [mix(&rows[0]), mix(&rows[1]), mix(&rows[2])]
        }
    }))
}

fn gradient_map(r: &mut Reader) -> Option<ColorFn> {
    let version = r.u16().ok()?;
    let reverse = r.u8().ok()? != 0;
    r.u8().ok()?;
    if version == 3 {
        r.tag().ok()?;
    }
    r.unicode().ok()?;
    let mut colors = vec![];
    for _ in 0..r.u16().ok()? {
        let at = r.u32().ok()? as f64 / 4096.0;
        let mid = r.u32().ok()? as f64 / 100.0;
        let space = r.u16().ok()?;
        let v: Vec<f64> = (0..4).map(|_| r.u16().map(|x| x as f64 / 65535.0)).collect::<Result<_, _>>().ok()?;
        r.u16().ok()?;
        let rgb = match space {
            1 => color::hsb_to_rgb(v[0], v[1], v[2]).map(|x| x as f32),
            8 => [1.0 - (v[0] * 65535.0 / 10000.0) as f32; 3],
            _ => [v[0] as f32, v[1] as f32, v[2] as f32],
        };
        colors.push((at, mid, rgb.map(|x| x.clamp(0.0, 1.0))));
    }
    let mut alphas = vec![];
    for _ in 0..r.u16().ok()? {
        let at = r.u32().ok()? as f64 / 4096.0;
        let mid = r.u32().ok()? as f64 / 100.0;
        let a = (r.u16().ok()? as f32 / 255.0).min(1.0);
        alphas.push((at, mid, a));
    }
    r.u16().ok()?;
    let smooth = r.u16().map(|v| v as f64 / 4096.0).unwrap_or(1.0);
    let lut = Gradient::from_stops(colors, alphas, smooth).table();
    Some(Box::new(move |c| {
        let l = lum(c).clamp(0.0, 1.0);
        let l = if reverse { 1.0 - l } else { l };
        let s = lut[(l * 255.0).round() as usize];
        [s[0], s[1], s[2]]
    }))
}

fn photo_filter(r: &mut Reader, cs: &ColorSpace) -> Option<ColorFn> {
    let version = r.u16().ok()?;
    let filter = if version == 3 {
        let x = r.i32().ok()? as f64 / 100.0;
        let y = r.i32().ok()? as f64 / 100.0;
        let z = r.i32().ok()? as f64 / 100.0;
        let f = |t: f64| if t > 0.008856 { t.cbrt() } else { 7.787 * t + 16.0 / 116.0 };
        let (fx, fy, fz) = (f(x / 96.422), f(y / 100.0), f(z / 82.521));
        color::lab_to_rgb(116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
    } else {
        let space = r.u16().ok()?;
        let v: Vec<f64> = (0..4).map(|_| r.u16().map(|x| x as f64 / 65535.0)).collect::<Result<_, _>>().ok()?;
        match space {
            2 => cs.cmyk(1.0 - v[0], 1.0 - v[1], 1.0 - v[2], 1.0 - v[3]),
            _ => [v[0] as f32, v[1] as f32, v[2] as f32],
        }
    };
    let density = r.u32().ok()? as f32 / 100.0;
    let preserve = r.u8().unwrap_or(1) != 0;
    Some(Box::new(move |c| {
        let m = [0, 1, 2].map(|i| c[i] + (c[i] * filter[i] - c[i]) * density);
        if preserve {
            set_lum(m, lum(c))
        } else {
            m
        }
    }))
}

fn posterize(r: &mut Reader) -> Option<ColorFn> {
    let levels = r.u16().ok()?.clamp(2, 255) as f32;
    Some(Box::new(move |c| c.map(|v| ((v * 255.0 / 256.0 * levels).floor() / (levels - 1.0)).min(1.0))))
}

fn threshold(r: &mut Reader, cs: &ColorSpace) -> Option<ColorFn> {
    let level = (r.u16().ok()? as f32 - 1.0 / 255.0) * 255.0 / 256.0;
    let on = move |l: f32| if (l * 255.0).round() > level { 1.0 } else { 0.0 };
    Some(match cs.plane() {
        // CMYK: no color ink; black from the luminance carried by the black pass.
        Some(0) => Box::new(|_| [1.0; 3]),
        Some(_) => Box::new(move |c| [on(c[1]); 3]),
        None => Box::new(move |c| [on(lum(c)); 3]),
    })
}

fn black_white(data: &[u8], cs: &ColorSpace) -> Option<ColorFn> {
    let d = descriptor::parse_block(data, 4).ok()?;
    let w = |k: &str, def: f64| d.num(k).unwrap_or(def) as f32 / 100.0;
    let (red, yellow, green, cyan, blue, magenta) =
        (w("Rd  ", 40.0), w("Yllw", 60.0), w("Grn ", 40.0), w("Cyn ", 60.0), w("Bl  ", 20.0), w("Mgnt", 80.0));
    let tint = d.bool("useTint").unwrap_or(false).then(|| color::from_key(&d, "tintColor", cs));
    Some(Box::new(move |c| {
        let hi = c[0].max(c[1]).max(c[2]);
        let lo = c[0].min(c[1]).min(c[2]);
        let mid = c[0] + c[1] + c[2] - hi - lo;
        let primary = if hi == c[0] {
            red
        } else if hi == c[1] {
            green
        } else {
            blue
        };
        let secondary = match (c[0] == lo, c[1] == lo) {
            (true, _) => cyan,
            (_, true) => magenta,
            _ => yellow,
        };
        let g = (lo + (mid - lo) * secondary + (hi - mid) * primary).clamp(0.0, 1.0);
        match tint {
            Some(t) => set_lum(t, g),
            None => [g; 3],
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spline_passes_through_points() {
        let pts = [(0.0, 0.0), (0.25, 0.5), (1.0, 1.0)];
        let s = spline(&pts);
        assert!((s(0.25) - 0.5).abs() < 1e-9);
        assert!((s(1.0) - 1.0).abs() < 1e-9);
        assert!(s(0.5) > 0.5);
        let line = [(0.0, 0.0), (1.0, 1.0)];
        assert!((spline(&line)(0.3) - 0.3).abs() < 1e-9);
    }

    #[test]
    fn hsl_round_trip() {
        for c in [[0.2, 0.4, 0.6], [1.0, 0.0, 0.0], [0.5, 0.5, 0.5], [0.9, 0.8, 0.1]] {
            let h = rgb_to_hsl(c);
            let back = hsl_to_rgb(h[0], h[1], h[2]);
            assert!(c.iter().zip(&back).all(|(a, b)| (a - b).abs() < 1e-5), "{c:?} {back:?}");
        }
    }

    #[test]
    fn hue_range_trapezoid() {
        let r = [0.9, 0.95, 0.05, 0.1];
        assert_eq!(range_weight(0.0, &r), 1.0);
        assert!((range_weight(0.925, &r) - 0.5).abs() < 1e-5);
        assert_eq!(range_weight(0.5, &r), 0.0);
    }

    #[test]
    fn simple_adjustments() {
        let p = posterize(&mut Reader::new(&[0, 2])).unwrap();
        assert_eq!(p([0.2, 0.6, 1.0]), [0.0, 1.0, 1.0]);
        let t = threshold(&mut Reader::new(&[0, 128]), &ColorSpace::default()).unwrap();
        assert_eq!(t([0.6, 0.6, 0.6]), [1.0; 3]);
        assert_eq!(t([0.4, 0.4, 0.4]), [0.0; 3]);
    }
}
