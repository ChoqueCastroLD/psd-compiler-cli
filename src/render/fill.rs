//! Solid, gradient and pattern fills, shared by fill layers, overlays, strokes and gradient maps.

use std::f64::consts::PI;

use super::canvas::Raster;
use crate::color::{self, ColorSpace};
use crate::psd::descriptor::{Descriptor, Value};
use crate::psd::{Document, Pattern};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    Linear,
    Radial,
    Angle,
    Reflected,
    Diamond,
}

#[derive(Clone, Debug, PartialEq)]
struct Stop<T> {
    at: f64,
    mid: f64,
    value: T,
}

/// A gradient's color and transparency ramps.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Gradient {
    colors: Vec<Stop<[f32; 3]>>,
    alphas: Vec<Stop<f32>>,
    smooth: f64,
    /// Colors hold Lab (L, a, b scaled to 0..=1), for Lab documents.
    lab: bool,
}

fn stops<T: Clone>(list: Option<&[Value]>, value: impl Fn(&Descriptor) -> T) -> Vec<Stop<T>> {
    let mut out: Vec<Stop<T>> = list
        .unwrap_or(&[])
        .iter()
        .filter_map(|v| match v {
            Value::Descriptor(d) => Some(Stop {
                at: d.num("Lctn").unwrap_or(0.0) / 4096.0,
                mid: (d.num("Mdpn").unwrap_or(50.0) / 100.0).clamp(0.001, 0.999),
                value: value(d),
            }),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.at.total_cmp(&b.at));
    out
}

impl Gradient {
    /// Reads a `Grdn` descriptor. Noise gradients become a ramp through their limits.
    pub fn parse(g: &Descriptor, cs: &ColorSpace) -> Gradient {
        if g.enumerated("GrdF") == Some("ClNs") {
            return Gradient::noise(g);
        }
        let lab = cs.is_lab();
        let mut colors = stops(g.list("Clrs"), |d| {
            let rgb = match d.enumerated("Type") {
                Some("BckC") => [1.0; 3],
                _ => color::from_key(d, "Clr ", cs),
            };
            if !lab {
                return rgb;
            }
            // Lab stops keep their out-of-gamut values.
            let c = d.desc("Clr ");
            let [l, a, b] = match c.and_then(|c| Some([c.num("Lmnc")?, c.num("A   ")?, c.num("B   ")?])) {
                Some(v) => v,
                None => color::rgb_to_lab(rgb.map(|v| v as f64)),
            };
            [l / 100.0, (a + 128.0) / 255.0, (b + 128.0) / 255.0].map(|v| v as f32)
        });
        if colors.is_empty() {
            colors = vec![Stop { at: 0.0, mid: 0.5, value: [0.0; 3] }, Stop { at: 1.0, mid: 0.5, value: [1.0; 3] }];
        }
        let mut alphas = stops(g.list("Trns"), |d| (d.num("Opct").unwrap_or(100.0) / 100.0) as f32);
        if alphas.is_empty() {
            alphas = vec![Stop { at: 0.0, mid: 0.5, value: 1.0 }];
        }
        let smooth = (g.num("Intr").unwrap_or(4096.0) / 4096.0).clamp(0.0, 1.0);
        Gradient { colors, alphas, smooth, lab }
    }

    /// Builds a gradient from `(location, midpoint, value)` stops, all in 0..=1.
    pub fn from_stops(colors: Vec<(f64, f64, [f32; 3])>, alphas: Vec<(f64, f64, f32)>, smooth: f64) -> Gradient {
        fn build<T>(v: Vec<(f64, f64, T)>, default: T) -> Vec<Stop<T>> {
            let mut s: Vec<Stop<T>> =
                v.into_iter().map(|(at, mid, value)| Stop { at, mid: mid.clamp(0.001, 0.999), value }).collect();
            s.sort_by(|a, b| a.at.total_cmp(&b.at));
            if s.is_empty() {
                s.push(Stop { at: 0.0, mid: 0.5, value: default });
            }
            s
        }
        Gradient {
            colors: build(colors, [0.0; 3]),
            alphas: build(alphas, 1.0),
            smooth: smooth.clamp(0.0, 1.0),
            lab: false,
        }
    }

    fn noise(g: &Descriptor) -> Gradient {
        let limit = |key: &str| -> Vec<f64> {
            g.list(key)
                .unwrap_or(&[])
                .iter()
                .filter_map(|v| match v {
                    Value::Integer(i) => Some(*i as f64 / 100.0),
                    Value::Number(n) => Some(*n / 100.0),
                    _ => None,
                })
                .collect()
        };
        let (lo, hi) = (limit("Mnm "), limit("Mxm "));
        let pick = |v: &[f64]| -> [f32; 3] {
            let c =
                [v.get(1).copied().unwrap_or(0.0), v.get(2).copied().unwrap_or(0.0), v.get(3).copied().unwrap_or(0.0)];
            match g.enumerated("ClrS") {
                Some("HSBl") => color::hsb_to_rgb(c[0], c[1], c[2]).map(|x| x as f32),
                Some("LbCl") => color::lab_to_rgb(c[0] * 100.0, c[1] * 255.0 - 128.0, c[2] * 255.0 - 128.0),
                _ => c.map(|x| x.clamp(0.0, 1.0) as f32),
            }
        };
        Gradient {
            colors: vec![Stop { at: 0.0, mid: 0.5, value: pick(&lo) }, Stop { at: 1.0, mid: 0.5, value: pick(&hi) }],
            alphas: vec![Stop { at: 0.0, mid: 0.5, value: 1.0 }],
            smooth: 0.0,
            lab: false,
        }
    }

    /// Channel `ch` of ramp `s` at `t`. Smooth ramps are cubic Hermite curves: end stops at 0 or
    /// 1 lean half their segment's slope, end stops before a flat run are flat, and inner stops
    /// take the slope between their neighbors; no curve leaves its segment's range.
    fn ramp<T>(&self, s: &[Stop<T>], t: f64, ch: impl Fn(&T) -> f32) -> f32 {
        let first = &s[0];
        if t <= first.at || s.len() == 1 {
            return ch(&first.value);
        }
        let v = |i: usize| ch(&s[i].value) as f64;
        let slope = |i: usize| {
            let span = s[i + 1].at - s[i].at;
            if span > 0.0 {
                (v(i + 1) - v(i)) / span
            } else {
                0.0
            }
        };
        let last = s.len() - 1;
        let tangent = |i: usize| -> f64 {
            if i == 0 {
                return if s[0].at <= 1e-6 { 0.5 * slope(0) } else { 0.0 };
            }
            if i == last {
                return if s[last].at >= 1.0 - 1e-6 { 0.5 * slope(last - 1) } else { 0.0 };
            }
            let span = s[i + 1].at - s[i - 1].at;
            if span > 0.0 {
                (v(i + 1) - v(i - 1)) / span
            } else {
                0.0
            }
        };
        for i in 0..last {
            let (a, b) = (&s[i], &s[i + 1]);
            if t <= b.at {
                let span = b.at - a.at;
                if span <= 0.0 {
                    return ch(&b.value);
                }
                let u = (t - a.at) / span;
                let u = if u < b.mid { 0.5 * u / b.mid } else { 0.5 + 0.5 * (u - b.mid) / (1.0 - b.mid) };
                let (va, vb) = (v(i), v(i + 1));
                let linear = va + (vb - va) * u;
                if self.smooth <= 0.0 {
                    return linear as f32;
                }
                let (u2, u3) = (u * u, u * u * u);
                let hermite = (2.0 * u3 - 3.0 * u2 + 1.0) * va
                    + (u3 - 2.0 * u2 + u) * tangent(i) * span
                    + (3.0 * u2 - 2.0 * u3) * vb
                    + (u3 - u2) * tangent(i + 1) * span;
                return (linear + (hermite - linear) * self.smooth).clamp(va.min(vb), va.max(vb)) as f32;
            }
        }
        ch(&s[last].value)
    }

    fn color(&self, t: f64) -> [f32; 3] {
        let c = [0, 1, 2].map(|i| self.ramp(&self.colors, t, |c| c[i]));
        if self.lab {
            let [l, a, b] = c.map(|v| v as f64);
            return color::lab_to_rgb(l * 100.0, a * 255.0 - 128.0, b * 255.0 - 128.0);
        }
        c
    }

    /// Straight RGBA at position `t` in 0..=1.
    pub fn sample(&self, t: f64) -> [f32; 4] {
        let t = t.clamp(0.0, 1.0);
        let c = self.color(t);
        let a = self.ramp(&self.alphas, t, |a| *a);
        [c[0], c[1], c[2], a]
    }

    /// 256-entry lookup table, for gradient maps.
    pub fn table(&self) -> Vec<[f32; 4]> {
        (0..256).map(|i| self.sample(i as f64 / 255.0)).collect()
    }
}

/// How a gradient is laid out over its reference box.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GradientFill {
    pub gradient: Gradient,
    pub style: Style,
    pub angle: f64,
    pub scale: f64,
    pub reverse: bool,
    pub align: bool,
    pub offset: (f64, f64),
}

impl GradientFill {
    pub fn parse(d: &Descriptor, cs: &ColorSpace) -> GradientFill {
        let offset = d
            .desc("Ofst")
            .map_or((0.0, 0.0), |o| (o.num("Hrzn").unwrap_or(0.0) / 100.0, o.num("Vrtc").unwrap_or(0.0) / 100.0));
        GradientFill {
            gradient: d
                .desc("Grad")
                .map(|g| Gradient::parse(g, cs))
                .unwrap_or_else(|| Gradient::parse(&Descriptor::default(), cs)),
            style: match d.enumerated("Type") {
                Some("Rdl ") => Style::Radial,
                Some("Angl") => Style::Angle,
                Some("Rflc") => Style::Reflected,
                Some("Dmnd") => Style::Diamond,
                _ => Style::Linear,
            },
            angle: d.num("Angl").unwrap_or(90.0),
            scale: (d.num("Scl ").unwrap_or(100.0) / 100.0).max(0.001),
            reverse: d.bool("Rvrs").unwrap_or(false),
            align: d.bool("Algn").unwrap_or(true),
            offset,
        }
    }

    /// Gradient position for a pixel center at `(px, py)` given the reference box `[x0, y0, x1, y1]`.
    pub fn position(&self, px: f64, py: f64, b: [f64; 4]) -> f64 {
        let (bw, bh) = (b[2] - b[0], b[3] - b[1]);
        let cx = (b[0] + b[2]) / 2.0 + self.offset.0 * bw;
        let cy = (b[1] + b[3]) / 2.0 + self.offset.1 * bh;
        let th = self.angle.to_radians();
        let (cos, sin) = (th.cos(), th.sin());
        let fit = |side: f64, k: f64| if k.abs() > 1e-9 { side / k.abs() } else { f64::INFINITY };
        let half = fit(bw, cos).min(fit(bh, sin)).max(1.0) * self.scale / 2.0;
        // Photoshop snaps the end points to whole pixels, which matters for small boxes.
        // (Rounding errors in the angle must not drop a whole pixel.)
        let snap = |v: f64| (v + 1e-6).floor();
        let (sx, sy) = match self.style {
            Style::Linear => (snap(cx - cos * half), snap(cy + sin * half)),
            _ => (snap(cx), snap(cy)),
        };
        let (ex, ey) = (snap(cx + cos * half), snap(cy - sin * half));
        let (mut vx, mut vy) = (ex - sx, ey - sy);
        let mut len = vx.hypot(vy);
        if len < 1.0 {
            (vx, vy, len) = (cos, -sin, 1.0);
        }
        let (ux, uy) = (vx / len, vy / len);
        // Photoshop measures from pixel corners.
        let (dx, dy) = (px - 0.5 - sx, py - 0.5 - sy);
        let along = dx * ux + dy * uy;
        let across = dx * uy - dy * ux;
        let t = match self.style {
            Style::Linear => along / len,
            Style::Reflected => along.abs() / len,
            Style::Radial => (dx * dx + dy * dy).sqrt() / len,
            Style::Diamond => (along.abs() + across.abs()) / len,
            Style::Angle => {
                let a = (-dy).atan2(dx) - (-uy).atan2(ux);
                1.0 - (a / (2.0 * PI)).rem_euclid(1.0)
            }
        };
        if self.reverse {
            1.0 - t
        } else {
            t
        }
    }
}

/// A pattern placed on the document.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PatternFill {
    pub id: String,
    pub scale: f64,
    pub angle: f64,
    pub phase: (f64, f64),
    pub align: bool,
}

impl PatternFill {
    pub fn parse(d: &Descriptor) -> Option<PatternFill> {
        let p = d.desc("Ptrn")?;
        let id = match p.get("Idnt")? {
            Value::Text(t) => t.trim_end_matches('\0').to_string(),
            _ => return None,
        };
        let phase =
            d.desc("phase").map_or((0.0, 0.0), |o| (o.num("Hrzn").unwrap_or(0.0), o.num("Vrtc").unwrap_or(0.0)));
        Some(PatternFill {
            id,
            scale: (d.num("Scl ").unwrap_or(100.0) / 100.0).max(0.001),
            angle: d.num("Angl").unwrap_or(0.0),
            phase,
            align: d.bool("Algn").unwrap_or(true),
        })
    }

    /// Straight RGBA of the pattern at document pixel center `(x, y)`.
    pub fn sample(&self, p: &Pattern, x: f64, y: f64, origin: (f64, f64)) -> [f32; 4] {
        let (mut u, mut v) = (x - origin.0 - self.phase.0, y - origin.1 - self.phase.1);
        if self.angle != 0.0 {
            let th = self.angle.to_radians();
            let (c, s) = (th.cos(), th.sin());
            (u, v) = (u * c - v * s, u * s + v * c);
        }
        // Scaled patterns map pixel corners, not centers, onto texel centers.
        let (u, v) = ((u - 0.5) / self.scale, (v - 0.5) / self.scale);
        let (w, h) = (p.width as f64, p.height as f64);
        let fetch = |i: i64, j: i64| {
            let i = i.rem_euclid(p.width as i64) as usize;
            let j = j.rem_euclid(p.height as i64) as usize;
            let o = (j * p.width + i) * 4;
            [0, 1, 2, 3].map(|c| p.rgba[o + c] as f32 / 255.0)
        };
        let (u, v) = (u.rem_euclid(w), v.rem_euclid(h));
        if self.scale == 1.0 && self.angle == 0.0 {
            return fetch((u + 0.5).floor() as i64, (v + 0.5).floor() as i64);
        }
        let (i, j) = (u.floor() as i64, v.floor() as i64);
        let (fx, fy) = ((u - u.floor()) as f32, (v - v.floor()) as f32);
        let q = [fetch(i, j), fetch(i + 1, j), fetch(i, j + 1), fetch(i + 1, j + 1)];
        let mut out = [0f32; 4];
        let wsum = [(1.0 - fx) * (1.0 - fy), fx * (1.0 - fy), (1.0 - fx) * fy, fx * fy];
        let alpha: f32 = (0..4).map(|k| q[k][3] * wsum[k]).sum();
        for c in 0..3 {
            out[c] = if alpha > 0.0 { (0..4).map(|k| q[k][c] * q[k][3] * wsum[k]).sum::<f32>() / alpha } else { 0.0 };
        }
        out[3] = alpha;
        out
    }
}

/// What a fill layer, an overlay or a stroke is painted with.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Fill {
    Solid([f32; 3]),
    Gradient(GradientFill),
    Pattern(PatternFill),
}

impl Fill {
    /// Reads `SoCo`/`GdFl`/`PtFl`-style content, or effect content selected by `PntT`.
    pub fn parse(d: &Descriptor, cs: &ColorSpace) -> Fill {
        let kind = d.enumerated("PntT");
        if kind == Some("GrFl") || (kind.is_none() && d.desc("Grad").is_some()) {
            Fill::Gradient(GradientFill::parse(d, cs))
        } else if (kind == Some("Ptrn") || kind.is_none()) && d.desc("Ptrn").is_some() {
            PatternFill::parse(d).map_or(Fill::Solid([0.0; 3]), Fill::Pattern)
        } else {
            Fill::Solid(color::from_key(d, "Clr ", cs))
        }
    }

    /// Paints the fill over the document rectangle `(x, y, w, h)` with coverage `cov` (or full).
    ///
    /// `bounds` is the box an aligned gradient or pattern is laid out on.
    pub fn render(
        &self,
        doc: &Document,
        cs: &ColorSpace,
        x: i32,
        y: i32,
        w: usize,
        h: usize,
        cov: Option<&[f32]>,
        bounds: [f64; 4],
    ) -> Raster {
        let mut r = Raster { x, y, w, h, px: vec![0.0; w * h * 4] };
        // CMYK plates need the pattern in working values.
        let working = match self {
            Fill::Pattern(p) if cs.plane().is_some() => doc.patterns.get(&p.id).map(|pat| {
                let mut pat = pat.clone();
                cs.srgb_rgba8_to_working(&mut pat.rgba);
                pat
            }),
            _ => None,
        };
        let canvas = [0.0, 0.0, doc.width as f64, doc.height as f64];
        let sample: Box<dyn Fn(f64, f64) -> [f32; 4] + Sync> = match self {
            Fill::Solid(c) => {
                let c = *c;
                Box::new(move |_, _| [c[0], c[1], c[2], 1.0])
            }
            Fill::Gradient(g) => {
                let b = if g.align { bounds } else { canvas };
                Box::new(move |px, py| g.gradient.sample(g.position(px, py, b)))
            }
            Fill::Pattern(p) => match working.as_ref().or(doc.patterns.get(&p.id)) {
                Some(pat) => {
                    let origin = if p.align { (0.0, 0.0) } else { (bounds[0], bounds[1]) };
                    let linear = cs.is_linear();
                    Box::new(move |px, py| {
                        let s = p.sample(pat, px, py, origin);
                        if linear {
                            let c = cs.srgb_to_working([s[0], s[1], s[2]]);
                            [c[0], c[1], c[2], s[3]]
                        } else {
                            s
                        }
                    })
                }
                None => Box::new(|_, _| [0.0; 4]),
            },
        };
        for j in 0..h {
            for i in 0..w {
                let k = cov.map_or(1.0, |c| c[j * w + i]);
                if k <= 0.0 {
                    continue;
                }
                let s = sample((x + i as i32) as f64 + 0.5, (y + j as i32) as f64 + 0.5);
                let a = s[3] * k;
                let o = (j * w + i) * 4;
                r.px[o..o + 4].copy_from_slice(&[s[0] * a, s[1] * a, s[2] * a, a]);
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(at: f64, value: [f32; 3]) -> Stop<[f32; 3]> {
        Stop { at, mid: 0.5, value }
    }

    fn bw() -> Gradient {
        Gradient {
            colors: vec![stop(0.0, [0.0; 3]), stop(1.0, [1.0; 3])],
            alphas: vec![Stop { at: 0.0, mid: 0.5, value: 1.0 }],
            smooth: 0.0,
            lab: false,
        }
    }

    #[test]
    fn ramp_with_midpoint() {
        let mut g = bw();
        assert_eq!(g.sample(0.5), [0.5, 0.5, 0.5, 1.0]);
        g.colors[1].mid = 0.25;
        assert_eq!(g.sample(0.25)[0], 0.5);
        assert_eq!(g.sample(2.0), [1.0; 4]);
    }

    #[test]
    fn smooth_ramps_are_hermite_curves() {
        // Two stops at the ends lean half their slope: halfway to a smoothstep.
        let g = Gradient { smooth: 1.0, ..bw() };
        let u: f64 = 0.25;
        let ss = u * u * (3.0 - 2.0 * u);
        assert!((g.sample(u)[0] as f64 - (u + ss) / 2.0).abs() < 1e-6);
        // A peak and stops before flat runs are flat: a full smoothstep, never past the stops.
        let g = Gradient { colors: vec![stop(0.1, [0.0; 3]), stop(0.5, [1.0; 3]), stop(0.9, [0.0; 3])], ..g };
        let v = 0.1 + 0.4 * u;
        assert!((g.sample(v)[0] as f64 - ss).abs() < 1e-6);
        assert_eq!(g.sample(0.05)[0], 0.0);
        assert_eq!(g.sample(0.5)[0], 1.0);
    }

    #[test]
    fn linear_geometry_spans_the_box() {
        let f = GradientFill {
            gradient: bw(),
            style: Style::Linear,
            angle: 0.0,
            scale: 1.0,
            reverse: false,
            align: true,
            offset: (0.0, 0.0),
        };
        let b = [0.0, 0.0, 100.0, 10.0];
        // Positions are measured from pixel corners: (x, y) is the center of pixel (x - 0.5, y - 0.5).
        assert!((f.position(0.5, 5.5, b)).abs() < 1e-9);
        assert!((f.position(100.5, 5.5, b) - 1.0).abs() < 1e-9);
        let up = GradientFill { angle: 90.0, ..f.clone() };
        assert!((up.position(50.5, 10.5, b)).abs() < 1e-9);
        // Steep angles span the line through the center as far as the box clips it, not the
        // box's projection: about 10 px here, not 27.
        let steep = GradientFill { angle: 80.0, ..f.clone() };
        let rise = steep.position(50.5, 0.5, b) - steep.position(50.5, 10.5, b);
        assert!(rise > 0.9, "{rise}");
        let radial = GradientFill { style: Style::Radial, ..f };
        assert!((radial.position(50.5, 5.5, b)).abs() < 1e-9);
        assert!((radial.position(100.5, 5.5, b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pattern_tiles() {
        let p = Pattern { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 255] };
        let f = PatternFill { id: String::new(), scale: 1.0, angle: 0.0, phase: (0.0, 0.0), align: true };
        assert_eq!(f.sample(&p, 0.5, 0.5, (0.0, 0.0)), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(f.sample(&p, 3.5, 7.5, (0.0, 0.0)), [0.0, 0.0, 1.0, 1.0]);
    }
}
