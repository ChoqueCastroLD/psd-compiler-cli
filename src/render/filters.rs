//! Smart filters: the filter stack of a smart object, applied to its re-rendered pixels.
//!
//! Filters run in document space on premultiplied pixels, oldest first, each faded into its input
//! by its blending options. Kernels were calibrated against Photopea.

use std::collections::BTreeMap;

use super::adjust::{self, ColorFn};
use super::canvas::{for_rows, Raster};
use crate::blend::BlendMode;
use crate::psd::descriptor::{Descriptor, Value};

/// Largest reach, in pixels, of any filter (bigger radii are clamped).
const MAX_REACH: f64 = 500.0;

enum Fill {
    Wrap,
    Repeat,
    Transparent,
}

enum Rank {
    Median,
    Maximum,
    Minimum,
}

enum Op {
    /// Weighted taps `(dx, dy, weight)` summed over premultiplied pixels.
    Taps(Vec<(i32, i32, f32)>),
    /// Taps plus a constant added to the colors.
    Custom(Vec<(i32, i32, f32)>, f32),
    /// Separable kernel, centered.
    Separable(Vec<f32>),
    Unsharp {
        sigma: f64,
        amount: f32,
        threshold: f32,
    },
    HighPass(f64),
    Rank(Rank, usize),
    Offset {
        dx: i32,
        dy: i32,
        fill: Fill,
    },
    Color(ColorFn),
    Average,
    Mosaic(usize),
}

struct Filter {
    op: Op,
    mode: BlendMode,
    opacity: f32,
}

/// The enabled filters of a smart object, in application order.
pub(crate) struct Stack(Vec<Filter>);

impl Stack {
    /// Parses a `filterFX` descriptor; also returns the names of filters that are not supported.
    pub fn parse(d: &Descriptor) -> (Stack, Vec<String>) {
        let (mut filters, mut unsupported) = (vec![], vec![]);
        if d.bool("enab") == Some(false) {
            return (Stack(filters), unsupported);
        }
        for item in d.list("filterFXList").unwrap_or_default() {
            let Value::Descriptor(f) = item else { continue };
            if f.bool("enab") == Some(false) {
                continue;
            }
            let empty = Descriptor::default();
            let params = f.desc("Fltr").unwrap_or(&empty);
            let code = match f.get("filterID") {
                Some(Value::Integer(i)) => String::from_utf8_lossy(&(*i as u32).to_be_bytes()).into_owned(),
                _ => params.class.clone(),
            };
            let Some(op) = op(&code, params) else {
                let name = f.text("Nm  ").map_or_else(|| code.trim().to_owned(), str::to_owned);
                unsupported.push(name);
                continue;
            };
            let blend = f.desc("blendOptions");
            let mode = blend
                .and_then(|b| b.enumerated("Md  "))
                .map_or(BlendMode::Normal, |m| BlendMode::from_key(m.as_bytes()));
            let opacity = blend.and_then(|b| b.num("Opct")).map_or(1.0, |o| (o / 100.0).clamp(0.0, 1.0) as f32);
            filters.push(Filter { op, mode, opacity });
        }
        (Stack(filters), unsupported)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a filter works on the whole canvas (Offset), not just around the pixels.
    pub fn needs_canvas(&self) -> bool {
        self.0.iter().any(|f| matches!(f.op, Op::Offset { .. }))
    }

    /// How far, in pixels, the stack pulls pixels from.
    pub fn reach(&self) -> usize {
        self.0.iter().map(|f| reach(&f.op)).sum()
    }

    /// Applies the stack to `r`; `canvas` is the document size, which Offset wraps around.
    pub fn apply(&self, mut r: Raster, canvas: (usize, usize)) -> Raster {
        for f in &self.0 {
            let out = run(&f.op, &r, canvas);
            r = fade(r, out, f.mode, f.opacity);
        }
        r
    }
}

fn op(code: &str, p: &Descriptor) -> Option<Op> {
    let radius = |key: &str| p.num(key).unwrap_or(1.0).clamp(0.0, MAX_REACH);
    Some(match code {
        "GsnB" => Op::Separable(gaussian(radius("Rds "))),
        "boxblur" => {
            let r = radius("Rds ").round() as usize;
            Op::Separable(vec![1.0 / (2 * r + 1) as f32; 2 * r + 1])
        }
        "MtnB" => motion(p.num("Angl").unwrap_or(0.0), p.num("Dstn").unwrap_or(10.0).clamp(0.0, MAX_REACH)),
        "Blr " => Op::Taps(kernel(3, &[1, 2, 1, 2, 16, 2, 1, 2, 1], 28.0)),
        "BlrM" => Op::Separable(vec![0.25, 0.5, 0.25]),
        "Shrp" => Op::Taps(kernel(3, &[0, -1, 0, -1, 8, -1, 0, -1, 0], 4.0)),
        "ShrM" => Op::Taps(kernel(3, &[-1, -1, -1, -1, 12, -1, -1, -1, -1], 4.0)),
        "ShrE" => Op::Unsharp { sigma: 1.0, amount: 0.5, threshold: 0.02 },
        "UnsM" => Op::Unsharp {
            sigma: radius("Rds "),
            amount: (p.num("Amnt").unwrap_or(50.0) / 100.0) as f32,
            threshold: (p.num("Thsh").unwrap_or(0.0) / 255.0) as f32,
        },
        "HghP" => Op::HighPass(radius("Rds ")),
        "Mdn " => Op::Rank(Rank::Median, radius("Rds ").round() as usize),
        "Mxm " => Op::Rank(Rank::Maximum, radius("Rds ").round() as usize),
        "Mnm " => Op::Rank(Rank::Minimum, radius("Rds ").round() as usize),
        "Ofst" => Op::Offset {
            dx: p.num("Hrzn").unwrap_or(0.0).clamp(-1e6, 1e6) as i32,
            dy: p.num("Vrtc").unwrap_or(0.0).clamp(-1e6, 1e6) as i32,
            fill: match p.enumerated("Fl  ") {
                Some("Wrp ") => Fill::Wrap,
                Some("Rpt ") => Fill::Repeat,
                _ => Fill::Transparent,
            },
        },
        "Cstm" => {
            let m: Vec<i64> = p
                .list("Mtrx")?
                .iter()
                .filter_map(|v| match v {
                    Value::Integer(i) => Some(*i),
                    Value::Number(n) | Value::Unit(_, n) => Some(*n as i64),
                    _ => None,
                })
                .collect();
            if m.len() != 25 {
                return None;
            }
            let scale = p.num("Scl ").filter(|s| *s != 0.0).unwrap_or(1.0);
            let offset = p.num("Ofst").unwrap_or(0.0) as f32 / 255.0;
            Op::Custom(kernel(5, &m, scale), offset)
        }
        "Msc " => Op::Mosaic(p.num("ClSz").unwrap_or(10.0).clamp(1.0, MAX_REACH).round() as usize),
        "Invr" => Op::Color(Box::new(|c| c.map(|v| 1.0 - v))),
        "Slrz" => Op::Color(Box::new(|c| c.map(|v| if v > 0.5 { 1.0 - v } else { v }))),
        "Avrg" => Op::Average,
        "BrgC" => Op::Color(adjust::brightness_contrast_fn(
            p.num("Brgh").unwrap_or(0.0),
            p.num("Cntr").unwrap_or(0.0),
            p.bool("useLegacy").unwrap_or(false),
        )),
        "Crvs" => Op::Color(curves(p)?),
        _ => return None,
    })
}

/// Composite curves (a single adjustment of points or a 256-entry map).
fn curves(p: &Descriptor) -> Option<ColorFn> {
    let adjs = p.list("Adjs")?;
    let [Value::Descriptor(a)] = adjs else { return None };
    let lut = if let Some(pts) = a.list("Crv ") {
        let mut pts: Vec<(f64, f64)> = pts
            .iter()
            .filter_map(|v| match v {
                Value::Descriptor(d) => Some((d.num("Hrzn")? / 255.0, d.num("Vrtc")? / 255.0)),
                _ => None,
            })
            .collect();
        pts.sort_by(|a, b| a.0.total_cmp(&b.0));
        pts.dedup_by(|a, b| a.0 == b.0);
        if pts.len() < 2 {
            return None;
        }
        adjust::table(adjust::spline(&pts))
    } else {
        let map: Vec<f32> = a
            .list("Mpng")?
            .iter()
            .filter_map(|v| match v {
                Value::Integer(i) => Some(*i as f32 / 255.0),
                Value::Number(n) => Some(*n as f32 / 255.0),
                _ => None,
            })
            .collect();
        if map.len() != 256 {
            return None;
        }
        map
    };
    Some(adjust::lut_fn([Some(lut), None, None, None]))
}

fn gaussian(sigma: f64) -> Vec<f32> {
    if sigma < 0.2 {
        return vec![1.0];
    }
    let r = (sigma * 3.0).ceil() as i64;
    let k: Vec<f64> = (-r..=r).map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f64 = k.iter().sum();
    k.iter().map(|v| (v / sum) as f32).collect()
}

/// A square kernel of side `n` (odd), divided by `scale`.
fn kernel(n: i32, m: &[i64], scale: f64) -> Vec<(i32, i32, f32)> {
    let h = n / 2;
    m.iter()
        .enumerate()
        .filter(|(_, &v)| v != 0)
        .map(|(i, &v)| (i as i32 % n - h, i as i32 / n - h, (v as f64 / scale) as f32))
        .collect()
}

/// Motion blur along `angle` degrees (counter-clockwise from the x axis), `distance` pixels long.
fn motion(angle: f64, distance: f64) -> Op {
    // Photopea's streaks run about two pixels longer than the distance.
    let len = distance + 2.0;
    let (dx, dy) = (angle.to_radians().cos(), -angle.to_radians().sin());
    let n = (len * 4.0).ceil().max(1.0) as usize;
    let mut taps: BTreeMap<(i32, i32), f64> = BTreeMap::new();
    for i in 0..=n {
        let s = (i as f64 / n as f64 - 0.5) * len;
        let (x, y) = (s * dx, s * dy);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        for (ox, oy, w) in
            [(0, 0, (1.0 - fx) * (1.0 - fy)), (1, 0, fx * (1.0 - fy)), (0, 1, (1.0 - fx) * fy), (1, 1, fx * fy)]
        {
            if w > 0.0 {
                *taps.entry((x0 as i32 + ox, y0 as i32 + oy)).or_default() += w;
            }
        }
    }
    let sum: f64 = taps.values().sum();
    // Pixels sample along the streak, so taps point back along it.
    Op::Taps(taps.into_iter().map(|((x, y), w)| (-x, -y, (w / sum) as f32)).collect())
}

fn reach(op: &Op) -> usize {
    match op {
        Op::Taps(t) | Op::Custom(t, _) => {
            t.iter().map(|&(x, y, _)| x.unsigned_abs().max(y.unsigned_abs())).max().unwrap_or(0) as usize
        }
        Op::Separable(k) => k.len() / 2,
        Op::Unsharp { sigma, .. } | Op::HighPass(sigma) => gaussian(*sigma).len() / 2,
        Op::Rank(_, r) => *r,
        Op::Offset { dx, dy, .. } => dx.unsigned_abs().max(dy.unsigned_abs()).min(MAX_REACH as u32) as usize,
        Op::Color(_) => 0,
        // Every pixel of the layer counts; the stack only sees what was drawn around the canvas.
        Op::Average => 0,
        Op::Mosaic(n) => *n,
    }
}

fn run(op: &Op, r: &Raster, canvas: (usize, usize)) -> Raster {
    match op {
        Op::Taps(t) => clamp(taps(r, t)),
        Op::Custom(t, offset) => {
            let mut out = taps(r, t);
            for p in out.px.chunks_exact_mut(4) {
                for c in 0..3 {
                    p[c] += offset * p[3];
                }
            }
            clamp(out)
        }
        Op::Separable(k) => separable(r, k),
        Op::Unsharp { sigma, amount, threshold } => {
            let blurred = separable(r, &gaussian(*sigma));
            let mut out = r.clone();
            per_pixel(&mut out, |x, y, p| {
                let b = straight(&blurred.px[blurred.index(x, y)..][..4]);
                let c = straight(p);
                for ch in 0..3 {
                    let d = c[ch] - b[ch];
                    if d.abs() >= *threshold {
                        p[ch] = (c[ch] + d * amount).clamp(0.0, 1.0) * p[3];
                    }
                }
            });
            out
        }
        Op::HighPass(sigma) => {
            let blurred = separable(r, &gaussian(*sigma));
            let mut out = r.clone();
            per_pixel(&mut out, |x, y, p| {
                let b = straight(&blurred.px[blurred.index(x, y)..][..4]);
                let c = straight(p);
                for ch in 0..3 {
                    p[ch] = (c[ch] - b[ch] + 0.5).clamp(0.0, 1.0) * p[3];
                }
            });
            out
        }
        Op::Rank(kind, radius) => rank(r, kind, *radius),
        Op::Offset { dx, dy, fill } => offset(r, *dx, *dy, fill, canvas),
        Op::Color(f) => {
            let mut out = r.clone();
            per_pixel(&mut out, |_, _, p| {
                let c = f(straight(p)).map(|v| v.clamp(0.0, 1.0));
                for ch in 0..3 {
                    p[ch] = c[ch] * p[3];
                }
            });
            out
        }
        Op::Average => {
            let mut sum = [0f64; 4];
            for p in r.px.chunks_exact(4) {
                for c in 0..4 {
                    sum[c] += p[c] as f64;
                }
            }
            let mut out = r.clone();
            if sum[3] > 0.0 {
                let c = [0, 1, 2].map(|i| (sum[i] / sum[3]) as f32);
                per_pixel(&mut out, |_, _, p| {
                    for ch in 0..3 {
                        p[ch] = c[ch] * p[3];
                    }
                });
            }
            out
        }
        Op::Mosaic(n) => mosaic(r, *n),
    }
}

/// Straight color of a premultiplied pixel.
fn straight(p: &[f32]) -> [f32; 3] {
    if p[3] <= 0.0 {
        return [0.0; 3];
    }
    [0, 1, 2].map(|i| (p[i] / p[3]).clamp(0.0, 1.0))
}

fn per_pixel(r: &mut Raster, f: impl Fn(i32, i32, &mut [f32]) + Sync) {
    let (sx, sy, w) = (r.x, r.y, r.w);
    let area = r.w * r.h;
    for_rows(&mut r.px, w * 4, area, |i, row| {
        for (x, p) in row.chunks_exact_mut(4).enumerate() {
            if p[3] > 0.0 {
                f(sx + x as i32, sy + i as i32, p);
            }
        }
    });
}

/// Keeps alpha in 0..=1 and colors within alpha, after kernels with negative weights.
fn clamp(mut r: Raster) -> Raster {
    for p in r.px.chunks_exact_mut(4) {
        p[3] = p[3].clamp(0.0, 1.0);
        for c in 0..3 {
            p[c] = p[c].clamp(0.0, p[3]);
        }
    }
    r
}

fn taps(r: &Raster, taps: &[(i32, i32, f32)]) -> Raster {
    let mut out = Raster::new(r.x, r.y, r.w, r.h);
    let (w, h) = (r.w as i32, r.h as i32);
    for_rows(&mut out.px, r.w * 4, r.w * r.h * taps.len().max(1), |y, row| {
        for &(dx, dy, k) in taps {
            let sy = y as i32 + dy;
            if sy < 0 || sy >= h {
                continue;
            }
            let src = &r.px[sy as usize * r.w * 4..][..r.w * 4];
            let (lo, hi) = ((-dx).max(0), (w - dx).min(w));
            for x in lo..hi {
                let (d, s) = (x as usize * 4, (x + dx) as usize * 4);
                for c in 0..4 {
                    row[d + c] += src[s + c] * k;
                }
            }
        }
    });
    out
}

fn separable(r: &Raster, k: &[f32]) -> Raster {
    let h = k.len() as i32 / 2;
    let horizontal: Vec<_> = k.iter().enumerate().map(|(i, &w)| (i as i32 - h, 0, w)).collect();
    let vertical: Vec<_> = k.iter().enumerate().map(|(i, &w)| (0, i as i32 - h, w)).collect();
    taps(&taps(r, &horizontal), &vertical)
}

/// Median, maximum or minimum of each channel over a disc.
fn rank(r: &Raster, kind: &Rank, radius: usize) -> Raster {
    if radius == 0 {
        return r.clone();
    }
    let rad = radius as i32;
    let disc: Vec<(i32, i32)> = (-rad..=rad)
        .flat_map(|y| (-rad..=rad).map(move |x| (x, y)))
        .filter(|&(x, y)| x * x + y * y <= rad * rad + rad)
        .collect();
    let mut out = Raster::new(r.x, r.y, r.w, r.h);
    let (w, h) = (r.w as i32, r.h as i32);
    for_rows(&mut out.px, r.w * 4, r.w * r.h * disc.len(), |y, row| {
        let mut vals = Vec::with_capacity(disc.len());
        for x in 0..w {
            for c in 0..4 {
                vals.clear();
                vals.extend(disc.iter().map(|&(dx, dy)| {
                    let (sx, sy) = (x + dx, y as i32 + dy);
                    if sx < 0 || sy < 0 || sx >= w || sy >= h {
                        0.0
                    } else {
                        r.px[(sy as usize * r.w + sx as usize) * 4 + c]
                    }
                }));
                row[x as usize * 4 + c] = match kind {
                    Rank::Maximum => vals.iter().copied().fold(0.0, f32::max),
                    Rank::Minimum => vals.iter().copied().fold(1.0, f32::min),
                    Rank::Median => {
                        let mid = vals.len() / 2;
                        *vals.select_nth_unstable_by(mid, f32::total_cmp).1
                    }
                };
            }
        }
    });
    // Colors are ranked separately from alpha; keep them within it.
    clamp(out)
}

/// Moves the pixels by `(dx, dy)`; inside the canvas, uncovered pixels wrap around it, repeat its
/// edge, or stay transparent.
fn offset(r: &Raster, dx: i32, dy: i32, fill: &Fill, (cw, ch): (usize, usize)) -> Raster {
    let mut out = Raster::new(r.x, r.y, r.w, r.h);
    let (cw, ch) = (cw as i32, ch as i32);
    let (rx, ry, w) = (r.x, r.y, r.w);
    for_rows(&mut out.px, w * 4, r.w * r.h, |i, row| {
        let y = ry + i as i32;
        for x in 0..w as i32 {
            let x = rx + x;
            let (mut sx, mut sy) = (x - dx, y - dy);
            if x >= 0 && y >= 0 && x < cw && y < ch && cw > 0 && ch > 0 {
                match fill {
                    Fill::Wrap => (sx, sy) = (sx.rem_euclid(cw), sy.rem_euclid(ch)),
                    Fill::Repeat => (sx, sy) = (sx.clamp(0, cw - 1), sy.clamp(0, ch - 1)),
                    Fill::Transparent if sx < 0 || sy < 0 || sx >= cw || sy >= ch => continue,
                    Fill::Transparent => {}
                }
            }
            if r.contains(sx, sy) {
                let d = (x - rx) as usize * 4;
                row[d..d + 4].copy_from_slice(&r.px[r.index(sx, sy)..][..4]);
            }
        }
    });
    out
}

/// Averages square cells aligned to the document origin.
fn mosaic(r: &Raster, n: usize) -> Raster {
    let mut out = r.clone();
    let n = n.max(1) as i32;
    let (x0, y0) = (r.x.div_euclid(n) * n, r.y.div_euclid(n) * n);
    let (x1, y1) = (r.x + r.w as i32, r.y + r.h as i32);
    for cy in (y0..y1).step_by(n as usize) {
        for cx in (x0..x1).step_by(n as usize) {
            let (ax, ay, bx, by) = (cx.max(r.x), cy.max(r.y), (cx + n).min(x1), (cy + n).min(y1));
            let mut sum = [0f32; 4];
            for y in ay..by {
                for x in ax..bx {
                    let i = r.index(x, y);
                    for c in 0..4 {
                        sum[c] += r.px[i + c];
                    }
                }
            }
            let k = 1.0 / (n * n) as f32;
            for y in ay..by {
                for x in ax..bx {
                    let i = out.index(x, y);
                    for c in 0..4 {
                        out.px[i + c] = sum[c] * k;
                    }
                }
            }
        }
    }
    out
}

/// Fades the filtered `out` into its input with `mode` and `opacity`, in premultiplied space.
fn fade(input: Raster, out: Raster, mode: BlendMode, opacity: f32) -> Raster {
    let target = if mode.is_normal() {
        out
    } else {
        let mut t = input.clone();
        t.paint(&out, mode, 1.0, None);
        t
    };
    if opacity >= 1.0 {
        return target;
    }
    let mut r = input;
    r.lerp_to(&target, |_, _| opacity);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot() -> Raster {
        let mut r = Raster::new(-5, -5, 11, 11);
        let i = r.index(0, 0);
        r.px[i..i + 4].copy_from_slice(&[1.0, 0.0, 0.0, 1.0]);
        r
    }

    #[test]
    fn gaussian_keeps_mass() {
        let r = separable(&dot(), &gaussian(1.5));
        let sum: f32 = r.px.chunks(4).map(|p| p[3]).sum();
        assert!((sum - 1.0).abs() < 1e-4);
        assert!(r.px[r.index(1, 0) + 3] > r.px[r.index(2, 0) + 3]);
    }

    #[test]
    fn motion_streaks_along_the_angle() {
        let Op::Taps(t) = motion(0.0, 6.0) else { unreachable!() };
        let r = taps(&dot(), &t);
        assert!(r.px[r.index(3, 0) + 3] > 0.0 && r.px[r.index(-3, 0) + 3] > 0.0);
        assert_eq!(r.px[r.index(0, 2) + 3], 0.0);
    }

    #[test]
    fn offset_wraps_around_the_canvas() {
        let mut r = Raster::new(0, 0, 4, 1);
        r.px[12..16].copy_from_slice(&[0.0, 0.0, 1.0, 1.0]);
        let o = offset(&r, 1, 0, &Fill::Wrap, (4, 1));
        assert_eq!(&o.px[0..4], &[0.0, 0.0, 1.0, 1.0]);
        let o = offset(&r, 1, 0, &Fill::Transparent, (4, 1));
        assert_eq!(o.px[3], 0.0);
    }

    #[test]
    fn maximum_grows_and_minimum_shrinks() {
        let grown = rank(&dot(), &Rank::Maximum, 1);
        assert_eq!(grown.px[grown.index(1, 0) + 3], 1.0);
        let shrunk = rank(&grown, &Rank::Minimum, 1);
        assert_eq!(shrunk.px[shrunk.index(0, 0) + 3], 1.0);
        assert_eq!(shrunk.px[shrunk.index(1, 0) + 3], 0.0);
    }
}
