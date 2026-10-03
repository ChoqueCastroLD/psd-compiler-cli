//! Layer styles: shadows, glows, satin, overlays, strokes and bevels.
//!
//! [`Effects::prepare`] turns a layer's coverage into effect layers (in parallel with other
//! layers); [`assemble`] then folds the interior ones into the layer's own pixels.

use super::adjust;
use super::canvas::Raster;
use super::distance::{blur, distances, downsample, edge_offsets, soft_distance, Distances, SS};
use super::fill::{Fill, Gradient, GradientFill};
use crate::blend::BlendMode;
use crate::color::{self, ColorSpace};
use crate::psd::descriptor::{Descriptor, Value};
use crate::psd::Document;

/// Gaussian sigma per pixel of effect size, matching Photoshop's soft shadows and glows.
pub(crate) const SIGMA_PER_SIZE: f64 = 0.45;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrokePosition {
    Outside,
    Inside,
    Center,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Shadow {
    pub color: [f32; 3],
    pub opacity: f32,
    pub mode: BlendMode,
    pub angle: f64,
    pub distance: f64,
    pub spread: f64,
    pub size: f64,
    pub knocked_out: bool,
    pub contour: Option<Vec<f32>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Glow {
    pub color: [f32; 3],
    pub gradient: Option<Gradient>,
    pub opacity: f32,
    pub mode: BlendMode,
    pub spread: f64,
    pub size: f64,
    pub center: bool,
    pub contour: Option<Vec<f32>>,
    pub range: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Satin {
    pub color: [f32; 3],
    pub opacity: f32,
    pub mode: BlendMode,
    pub angle: f64,
    pub distance: f64,
    pub size: f64,
    pub invert: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Overlay {
    pub fill: Fill,
    pub opacity: f32,
    pub mode: BlendMode,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stroke {
    pub fill: Fill,
    pub opacity: f32,
    pub mode: BlendMode,
    pub size: f64,
    pub position: StrokePosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BevelStyle {
    Inner,
    Outer,
    Emboss,
    Pillow,
    /// An inner bevel of the layer and its stroke, painted only on the stroke.
    Stroke,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Bevel {
    pub style: BevelStyle,
    pub smooth: bool,
    pub depth: f64,
    pub up: bool,
    pub size: f64,
    pub soften: f64,
    pub angle: f64,
    pub altitude: f64,
    pub highlight: ([f32; 3], f32, BlendMode),
    pub shadow: ([f32; 3], f32, BlendMode),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Effects {
    pub drop_shadows: Vec<Shadow>,
    pub inner_shadows: Vec<Shadow>,
    pub outer_glows: Vec<Glow>,
    pub inner_glows: Vec<Glow>,
    pub satins: Vec<Satin>,
    pub color_overlays: Vec<Overlay>,
    pub gradient_overlays: Vec<Overlay>,
    pub pattern_overlays: Vec<Overlay>,
    pub strokes: Vec<Stroke>,
    pub bevels: Vec<Bevel>,
}

/// What an effect paints with: one color, or straight RGBA per pixel (alpha scales coverage).
#[derive(Clone, Debug)]
pub(crate) enum Tint {
    Solid([f32; 3]),
    Map(Vec<[f32; 4]>),
}

impl Tint {
    fn at(&self, i: usize) -> ([f32; 3], f32) {
        match self {
            Tint::Solid(c) => (*c, 1.0),
            Tint::Map(m) => {
                let p = m[i];
                ([p[0], p[1], p[2]], p[3])
            }
        }
    }
}

/// One effect layer over the layer's rectangle.
#[derive(Clone, Debug)]
pub(crate) struct Layered {
    pub cov: Vec<f32>,
    pub tint: Tint,
    pub mode: BlendMode,
    pub opacity: f32,
    pub paint: Paint,
}

/// How a [`Layered`] effect folds into the layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Paint {
    /// Composites over the layer.
    Over,
    /// Replaces the layer's pixels under it and blends with the backdrop (a stroke's inner band).
    Knock,
    /// Recolors the layer's pixels, keeping their alpha (a shape's vector stroke, already part of
    /// the layer's shape).
    Recolor,
}

impl Layered {
    /// Premultiplied raster at `scale` opacity; `room`, when given, divides coverage by `1 - room`
    /// (a band beside the layer, so that the layer composited over it adds up to full coverage).
    pub fn raster(&self, x: i32, y: i32, w: usize, h: usize, scale: f32, room: Option<&[f32]>) -> Raster {
        let mut px = vec![0f32; w * h * 4];
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            let mut k = self.cov[i];
            if k <= 0.0 {
                continue;
            }
            if let Some(r) = room {
                let free = 1.0 - r[i];
                k = if free > 1e-6 { (k / free).min(1.0) } else { 0.0 };
            }
            let (c, a) = self.tint.at(i);
            let (c, a) = match self.mode.neutral().filter(|_| self.mode != BlendMode::HardMix) {
                Some(n) => {
                    let (f, a) = fade_split(k * a, self.opacity);
                    (c.map(|v| n + f * (v - n)), a * scale)
                }
                None => (c, k * a * self.opacity * scale),
            };
            p.copy_from_slice(&[c[0] * a, c[1] * a, c[2] * a, a]);
        }
        Raster { x, y, w, h, px }
    }
}

/// Effect layers of one layer, over its (padded) rectangle.
#[derive(Clone, Debug, Default)]
pub(crate) struct Prepared {
    /// Painted below the layer: drop shadows and outer glows.
    pub below: Vec<Layered>,
    /// Painted beside the layer: the outer parts of strokes and bevels.
    pub beside: Vec<Layered>,
    /// Folded into the layer, bottom to top.
    pub inner: Vec<Layered>,
    /// How many of the first `inner` effects (overlays, satins, inner glows) "Blend Interior
    /// Effects as Group" fades with the fill.
    pub interior: usize,
    /// Where the strokes start in `inner`, above the other interior effects.
    pub strokes: usize,
}

impl Prepared {
    pub fn is_empty(&self) -> bool {
        self.below.is_empty() && self.beside.is_empty() && self.inner.is_empty()
    }
}

fn mode(d: &Descriptor, default: &str) -> BlendMode {
    BlendMode::from_key(d.enumerated("Md  ").unwrap_or(default).as_bytes())
}

fn opacity(d: &Descriptor, default: f64) -> f32 {
    (d.num("Opct").unwrap_or(default) / 100.0).clamp(0.0, 1.0) as f32
}

/// Enabled instances of an effect: the multi-instance list when present, else the single entry.
fn enabled<'a>(fx: &'a Descriptor, single: &str, multi: &str) -> Vec<&'a Descriptor> {
    let all: Vec<&Descriptor> = match fx.list(multi) {
        Some(list) => list
            .iter()
            .filter_map(|v| match v {
                Value::Descriptor(d) => Some(d),
                _ => None,
            })
            .collect(),
        None => fx.desc(single).into_iter().collect(),
    };
    all.into_iter().filter(|d| d.bool("enab").unwrap_or(false)).collect()
}

/// A shape contour (`TrnS`) as a table over `[0, 1]`, or `None` when it is linear. Corner points
/// (`Cnty` false) split the curve into separately smoothed pieces.
fn contour(d: &Descriptor) -> Option<Vec<f32>> {
    let mut pts: Vec<(f64, f64, bool)> = d
        .desc("TrnS")?
        .list("Crv ")?
        .iter()
        .filter_map(|v| match v {
            Value::Descriptor(c) => {
                Some((c.num("Hrzn")? / 255.0, c.num("Vrtc")? / 255.0, c.bool("Cnty").unwrap_or(true)))
            }
            _ => None,
        })
        .collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    pts.dedup_by(|a, b| a.0 == b.0);
    let linear = pts.iter().all(|p| (p.0 - p.1).abs() < 1e-6);
    if pts.len() < 2 || (linear && pts[0].0 == 0.0 && pts[pts.len() - 1].0 == 1.0) {
        return None;
    }
    let mut lut = adjust::table(|_| 0.0);
    let n = lut.len() - 1;
    let mut start = 0;
    for end in 1..pts.len() {
        if end + 1 < pts.len() && pts[end].2 {
            continue;
        }
        let piece: Vec<(f64, f64)> = pts[start..=end].iter().map(|p| (p.0, p.1)).collect();
        let f = adjust::spline(&piece);
        let lo = if start == 0 { 0 } else { (piece[0].0 * n as f64).ceil() as usize };
        let hi = if end + 1 == pts.len() { n } else { (piece[piece.len() - 1].0 * n as f64).floor() as usize };
        for (i, v) in lut.iter_mut().enumerate().take(hi + 1).skip(lo) {
            *v = f(i as f64 / n as f64).clamp(0.0, 1.0) as f32;
        }
        start = end;
    }
    Some(lut)
}

/// `v` through contour `lut`.
fn shaped(lut: &Option<Vec<f32>>, v: f32) -> f32 {
    let Some(lut) = lut else { return v };
    let x = v.clamp(0.0, 1.0) * (lut.len() - 1) as f32;
    let i = (x as usize).min(lut.len() - 2);
    lut[i] + (lut[i + 1] - lut[i]) * (x - i as f32)
}

fn light_angle(d: &Descriptor, global: f64) -> f64 {
    if d.bool("uglg").unwrap_or(true) {
        global
    } else {
        d.num("lagl").unwrap_or(global)
    }
}

/// Reads an `lfx2`/`lmfx`/`lfxs` descriptor.
pub(crate) fn parse(fx: &Descriptor, doc: &Document, cs: &ColorSpace) -> Effects {
    let mut e = Effects::default();
    if fx.bool("masterFXSwitch") == Some(false) {
        return e;
    }
    let (ga, galt) = (doc.global_angle, doc.global_altitude);
    // Sizes are stored already scaled: `Scl ` only records the last Scale Effects.
    let px = |d: &Descriptor, key: &str, default: f64| d.num(key).unwrap_or(default);
    let shadow = |d: &Descriptor| {
        let size = px(d, "blur", 5.0);
        Shadow {
            color: color::from_key(d, "Clr ", cs),
            opacity: opacity(d, 75.0),
            mode: mode(d, "Mltp"),
            angle: light_angle(d, ga),
            distance: px(d, "Dstn", 5.0),
            spread: (d.num("Ckmt").unwrap_or(0.0) / 100.0 * size).min(size),
            size,
            knocked_out: d.bool("layerConceals").unwrap_or(true),
            contour: contour(d),
        }
    };
    let glow = |d: &Descriptor, default_mode: &str| {
        let size = px(d, "blur", 5.0);
        Glow {
            color: color::from_key(d, "Clr ", cs),
            gradient: d.desc("Grad").map(|g| Gradient::parse(g, cs)),
            opacity: opacity(d, 75.0),
            mode: mode(d, default_mode),
            spread: (d.num("Ckmt").unwrap_or(0.0) / 100.0 * size).min(size),
            size,
            center: d.enumerated("glwS") == Some("SrcC"),
            contour: contour(d),
            range: (d.num("Inpr").unwrap_or(50.0) / 100.0).clamp(0.01, 1.0) as f32,
        }
    };
    e.drop_shadows = enabled(fx, "DrSh", "dropShadowMulti").into_iter().map(shadow).collect();
    e.inner_shadows = enabled(fx, "IrSh", "innerShadowMulti").into_iter().map(shadow).collect();
    e.outer_glows = enabled(fx, "OrGl", "outerGlowMulti").into_iter().map(|d| glow(d, "Scrn")).collect();
    e.inner_glows = enabled(fx, "IrGl", "innerGlowMulti").into_iter().map(|d| glow(d, "Scrn")).collect();
    e.satins = enabled(fx, "ChFX", "satinMulti")
        .into_iter()
        .map(|d| Satin {
            color: color::from_key(d, "Clr ", cs),
            opacity: opacity(d, 50.0),
            mode: mode(d, "Mltp"),
            angle: d.num("lagl").unwrap_or(19.0),
            distance: px(d, "Dstn", 11.0),
            size: px(d, "blur", 14.0),
            invert: d.bool("Invr").unwrap_or(true),
        })
        .collect();
    let overlay =
        |d: &Descriptor| Overlay { fill: Fill::parse(d, cs), opacity: opacity(d, 100.0), mode: mode(d, "Nrml") };
    e.color_overlays = enabled(fx, "SoFi", "solidFillMulti").into_iter().map(overlay).collect();
    e.gradient_overlays = enabled(fx, "GrFl", "gradientFillMulti")
        .into_iter()
        .map(|d| Overlay { fill: Fill::Gradient(GradientFill::parse(d, cs)), ..overlay(d) })
        .collect();
    e.pattern_overlays = enabled(fx, "patternFill", "patternFillMulti").into_iter().map(overlay).collect();
    e.strokes = enabled(fx, "FrFX", "frameFXMulti")
        .into_iter()
        .map(|d| Stroke {
            fill: Fill::parse(d, cs),
            opacity: opacity(d, 100.0),
            mode: mode(d, "Nrml"),
            size: px(d, "Sz  ", 3.0),
            position: match d.enumerated("Styl") {
                Some("InsF") => StrokePosition::Inside,
                Some("CtrF") => StrokePosition::Center,
                _ => StrokePosition::Outside,
            },
        })
        .collect();
    e.bevels = enabled(fx, "ebbl", "bevelEmbossMulti")
        .into_iter()
        .map(|d| Bevel {
            style: match d.enumerated("bvlS") {
                Some("OtrB") => BevelStyle::Outer,
                Some("Embs") => BevelStyle::Emboss,
                Some("PlEb") => BevelStyle::Pillow,
                Some("strokeEmboss") => BevelStyle::Stroke,
                _ => BevelStyle::Inner,
            },
            smooth: !matches!(d.enumerated("bvlT"), Some("PrBL") | Some("Slmt")),
            depth: d.num("srgR").unwrap_or(100.0) / 100.0,
            up: d.enumerated("bvlD") != Some("Out "),
            size: px(d, "blur", 5.0),
            soften: px(d, "Sftn", 0.0),
            angle: light_angle(d, ga),
            altitude: if d.bool("uglg").unwrap_or(true) { galt } else { d.num("Lald").unwrap_or(galt) },
            highlight: (
                d.desc("hglC").and_then(|c| color::from_object(c, cs)).unwrap_or([1.0; 3]),
                (d.num("hglO").unwrap_or(75.0) / 100.0) as f32,
                BlendMode::from_key(d.enumerated("hglM").unwrap_or("Scrn").as_bytes()),
            ),
            shadow: (
                d.desc("sdwC").and_then(|c| color::from_object(c, cs)).unwrap_or([0.0; 3]),
                (d.num("sdwO").unwrap_or(75.0) / 100.0) as f32,
                BlendMode::from_key(d.enumerated("sdwM").unwrap_or("Mltp").as_bytes()),
            ),
        })
        .collect();
    e
}

fn shift(src: &[f32], w: usize, h: usize, dx: i32, dy: i32, outside: f32) -> Vec<f32> {
    let mut out = vec![outside; w * h];
    for y in 0..h as i32 {
        let sy = y - dy;
        if sy < 0 || sy >= h as i32 {
            continue;
        }
        for x in 0..w as i32 {
            let sx = x - dx;
            if sx >= 0 && sx < w as i32 {
                out[(y * w as i32 + x) as usize] = src[(sy * w as i32 + sx) as usize];
            }
        }
    }
    out
}

/// Offset of an effect in whole pixels. Halves round away from zero, also when the trigonometry
/// lands a hair short of them (5 at 120° moves 3 across: ag-psd read/blend-mode).
fn offset(angle: f64, distance: f64) -> (i32, i32) {
    let a = angle.to_radians();
    let round = |v: f64| ((v * 1e6).round() / 1e6).round() as i32;
    (round(-distance * a.cos()), round(distance * a.sin()))
}

/// Mean over each pixel's cells of `f(inside, distance outward, distance inward)` in pixels.
fn field(d: &Distances, w: usize, h: usize, f: impl Fn(bool, f32, f32) -> f32) -> Vec<f32> {
    let sw = w * SS;
    let norm = 1.0 / (SS * SS) as f32;
    let px = |v: f32| v.sqrt() / SS as f32;
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s = 0.0;
            for j in 0..SS {
                for i in 0..SS {
                    let c = (y * SS + j) * sw + x * SS + i;
                    let inward = d.inward.get(c).copied().unwrap_or(0.0);
                    s += f(d.inside[c], px(d.outside[c]), px(inward));
                }
            }
            out[y * w + x] = s * norm;
        }
    }
    out
}

impl Effects {
    pub fn is_empty(&self) -> bool {
        *self == Effects::default()
    }

    /// How far effects reach outside the layer's pixels (and how much room inner blurs need).
    pub fn reach(&self) -> f64 {
        let shadows = self.drop_shadows.iter().chain(&self.inner_shadows).map(|s| s.size + s.distance);
        let glows = self.outer_glows.iter().chain(&self.inner_glows).map(|g| g.size);
        let satins = self.satins.iter().map(|s| s.size + s.distance);
        let strokes = self.strokes.iter().map(|s| s.size);
        let bevels = self.bevels.iter().map(|b| b.size + b.soften);
        let any = !self.is_empty();
        shadows
            .chain(glows)
            .chain(satins)
            .chain(strokes)
            .chain(bevels)
            .map(|r| r + 2.0)
            .fold(if any { 2.0 } else { 0.0 }, f64::max)
    }

    fn max_distance(&self) -> f64 {
        let strokes = self.strokes.iter().map(|s| s.size);
        let spreads = self.drop_shadows.iter().chain(&self.inner_shadows).map(|s| s.spread);
        let glows = self.outer_glows.iter().chain(&self.inner_glows).map(|g| g.spread);
        let bevels = self.bevels.iter().map(|b| b.size);
        strokes.chain(spreads).chain(glows).chain(bevels).fold(0.0, f64::max)
    }

    fn needs_distances(&self) -> bool {
        !self.strokes.is_empty()
            || !self.bevels.is_empty()
            || self.drop_shadows.iter().chain(&self.inner_shadows).any(|s| s.spread > 0.0)
            || self.outer_glows.iter().chain(&self.inner_glows).any(|g| g.spread > 0.0)
    }

    /// Builds the effect layers for coverage `a` (shape times mask) over `(x, y, w, h)`.
    ///
    /// `fill` is the layer's fill opacity (for shadows the layer knocks out) and `bounds` the box
    /// gradients and patterns are laid out on.
    /// Builds the effect layers for coverage `a` over `rect`. Outside strokes show under
    /// translucent pixels, except on shape layers, whose strokes follow their `path` coverage.
    pub fn prepare(
        &self,
        doc: &Document,
        cs: &ColorSpace,
        a: &[f32],
        path: Option<&[f32]>,
        rect: (i32, i32, usize, usize),
        fill: f32,
        (bounds, stroke_box): ([f64; 4], [f64; 4]),
    ) -> Prepared {
        let (x, y, w, h) = rect;
        let mut p = Prepared::default();
        if self.is_empty() || a.iter().all(|&v| v <= 0.0) {
            return p;
        }
        let dist = self.needs_distances().then(|| distances(a, w, h, true, self.max_distance() + 1.0));
        let grown = |r: f64| match &dist {
            Some(d) if r > 0.0 => {
                let s = (r * SS as f64) as f32;
                downsample(|i| d.inside[i] || d.outside[i] <= s * s, w, h)
            }
            _ => a.to_vec(),
        };
        let shrunk_inverse = |r: f64| match &dist {
            Some(d) if r > 0.0 => {
                let s = (r * SS as f64) as f32;
                downsample(|i| !d.inside[i] || d.inward[i] <= s * s, w, h)
            }
            _ => a.iter().map(|v| 1.0 - v).collect(),
        };
        let soft = |mut m: Vec<f32>, size: f64, spread: f64, outside: f32| {
            let s = (size - spread).max(0.0);
            m.iter_mut().for_each(|v| *v -= outside);
            blur(&mut m, w, h, s * SIGMA_PER_SIZE, s);
            m.iter_mut().for_each(|v| *v = (*v + outside).clamp(0.0, 1.0));
            m
        };
        let fill_tint_in = |f: &Fill, bounds: [f64; 4]| match f {
            Fill::Solid(c) => Tint::Solid(*c),
            _ => {
                let r = f.render(doc, cs, x, y, w, h, None, bounds);
                Tint::Map(
                    r.px.chunks_exact(4)
                        .map(|q| if q[3] > 0.0 { [q[0] / q[3], q[1] / q[3], q[2] / q[3], q[3]] } else { [0.0; 4] })
                        .collect(),
                )
            }
        };
        let fill_tint = |f: &Fill| fill_tint_in(f, bounds);
        let glow_tint = |g: &Glow, m: &mut Vec<f32>| match &g.gradient {
            None => Tint::Solid(g.color),
            Some(grad) => {
                let lut = grad.table();
                let map = m
                    .iter_mut()
                    .map(|v| {
                        let s = lut[((1.0 - *v).clamp(0.0, 1.0) * 255.0).round() as usize];
                        *v = if *v > 0.0 { 1.0 } else { 0.0 };
                        s
                    })
                    .collect();
                Tint::Map(map)
            }
        };

        let mut knocked = Vec::new();
        for s in &self.drop_shadows {
            if s.knocked_out {
                knocked.push(p.below.len());
            }
            let mut m = soft(grown(s.spread), s.size, s.spread, 0.0);
            m.iter_mut().for_each(|v| *v = shaped(&s.contour, *v));
            let (dx, dy) = offset(s.angle, s.distance);
            let mut cov = shift(&m, w, h, dx, dy, 0.0);
            if s.knocked_out {
                cov.iter_mut().zip(a).for_each(|(v, &al)| *v *= 1.0 - al * fill.clamp(0.0, 1.0));
            }
            p.below.push(Layered {
                cov,
                tint: Tint::Solid(s.color),
                mode: s.mode,
                opacity: s.opacity,
                paint: Paint::Over,
            });
        }
        for g in &self.outer_glows {
            let mut cov = soft(grown(g.spread), g.size, g.spread, 0.0);
            shape_glow(g, &mut cov);
            let tint = glow_tint(g, &mut cov);
            p.below.push(Layered { cov, tint, mode: g.mode, opacity: g.opacity, paint: Paint::Over });
        }

        for o in &self.pattern_overlays {
            p.inner.push(Layered {
                cov: vec![1.0; w * h],
                tint: fill_tint(&o.fill),
                mode: o.mode,
                opacity: o.opacity,
                paint: Paint::Over,
            });
        }
        for o in &self.gradient_overlays {
            p.inner.push(Layered {
                cov: vec![1.0; w * h],
                tint: fill_tint(&o.fill),
                mode: o.mode,
                opacity: o.opacity,
                paint: Paint::Over,
            });
        }
        for o in &self.color_overlays {
            p.inner.push(Layered {
                cov: vec![1.0; w * h],
                tint: fill_tint(&o.fill),
                mode: o.mode,
                opacity: o.opacity,
                paint: Paint::Over,
            });
        }
        for s in &self.satins {
            let m = soft(a.to_vec(), s.size, 0.0, 0.0);
            let (dx, dy) = offset(s.angle, s.distance / 2.0);
            let (p1, p2) = (shift(&m, w, h, dx, dy, 0.0), shift(&m, w, h, -dx, -dy, 0.0));
            let cov = p1.iter().zip(&p2).map(|(u, v)| {
                let d = (u - v).abs();
                if s.invert {
                    1.0 - d
                } else {
                    d
                }
            });
            p.inner.push(Layered {
                cov: cov.collect(),
                tint: Tint::Solid(s.color),
                mode: s.mode,
                opacity: s.opacity,
                paint: Paint::Over,
            });
        }
        for g in &self.inner_glows {
            let edge = soft(shrunk_inverse(g.spread), g.size, g.spread, 1.0);
            let mut cov: Vec<f32> = if g.center { edge.iter().map(|v| 1.0 - v).collect() } else { edge };
            shape_glow(g, &mut cov);
            let tint = glow_tint(g, &mut cov);
            p.inner.push(Layered { cov, tint, mode: g.mode, opacity: g.opacity, paint: Paint::Over });
        }
        p.interior = p.inner.len();
        for s in &self.inner_shadows {
            let mut m = soft(shrunk_inverse(s.spread), s.size, s.spread, 1.0);
            m.iter_mut().for_each(|v| *v = 1.0 - shaped(&s.contour, 1.0 - *v));
            let (dx, dy) = offset(s.angle, s.distance);
            let cov = shift(&m, w, h, dx, dy, 1.0);
            p.inner.push(Layered {
                cov,
                tint: Tint::Solid(s.color),
                mode: s.mode,
                opacity: s.opacity,
                paint: Paint::Over,
            });
        }
        p.strokes = p.inner.len();
        // Strokes measure from pixel centers: a pixel's distance outward is the cheapest
        // `|p - q| + 1 - alpha(q)` over covered pixels, inward `|p - q| + alpha(q)` over pixels
        // that are not fully covered; the stroke covers `size + 1 - distance`.
        let shape: Option<Vec<f32>> = path.map(|v| v.iter().zip(a).map(|(&p, &q)| p.max(q)).collect());
        let src = shape.as_deref().unwrap_or(a);
        let reach = self.strokes.iter().map(|s| s.size as f32).fold(0.0, f32::max) + 1.0;
        let need = |f: fn(&StrokePosition) -> bool| self.strokes.iter().any(|s| f(&s.position));
        let edge = if self.strokes.is_empty() { vec![] } else { edge_offsets(src, w, h) };
        let outward = need(|p| *p != StrokePosition::Inside)
            .then(|| soft_distance(|i| (src[i] > 0.0).then(|| 0.5 + edge[i]), w, h, reach));
        let inward = need(|p| *p != StrokePosition::Outside)
            .then(|| soft_distance(|i| (src[i] < 1.0).then(|| 0.5 - edge[i]), w, h, reach));
        // The first stroke's coverage on the layer and beside it, for stroke embosses.
        let mut embossed: Option<(Vec<f32>, Vec<f32>)> = None;
        for s in &self.strokes {
            let r = s.size as f32;
            let mut covs = (vec![0.0; w * h], vec![0.0; w * h]);
            let (inner_r, outer_r) = match s.position {
                StrokePosition::Outside => (0.0, r),
                StrokePosition::Inside => (r, 0.0),
                StrokePosition::Center => (r / 2.0, r / 2.0),
            };
            let band = |d: &Option<Vec<f32>>, r: f32| -> Vec<f32> {
                d.as_ref().map_or(vec![0.0; w * h], |d| d.iter().map(|&d| (r + 1.0 - d).clamp(0.0, 1.0)).collect())
            };
            // Stroke gradients span the stroke's outer edge.
            let o = outer_r as f64;
            let b = stroke_box;
            let tint = fill_tint_in(&s.fill, [b[0] - o, b[1] - o, b[2] + o, b[3] + o]);
            if inner_r > 0.0 {
                let band = band(&inward, inner_r);
                // On shapes the stroke follows the path, also where the fill is transparent.
                if let Some(v) = path {
                    let cov = band.iter().zip(v).zip(a).map(|((&c, &p), &q)| c * (p - q).max(0.0)).collect();
                    p.beside.push(Layered {
                        cov,
                        tint: tint.clone(),
                        mode: s.mode,
                        opacity: s.opacity,
                        paint: Paint::Over,
                    });
                }
                let cov: Vec<f32> = band.iter().zip(a).map(|(&c, &q)| if q > 1e-6 { c } else { 0.0 }).collect();
                covs.0.clone_from(&cov);
                p.inner.push(Layered {
                    cov,
                    tint: tint.clone(),
                    mode: s.mode,
                    opacity: s.opacity,
                    paint: Paint::Knock,
                });
            }
            if outer_r > 0.0 {
                let cov: Vec<f32> = band(&outward, outer_r).iter().zip(src).map(|(&c, &q)| c * (1.0 - q)).collect();
                covs.1.clone_from(&cov);
                p.beside.push(Layered { cov, tint, mode: s.mode, opacity: s.opacity, paint: Paint::Over });
            }
            embossed.get_or_insert(covs);
        }
        if !knocked.is_empty() && !p.beside.is_empty() {
            // Strokes beside the layer knock out its shadows too.
            let mut keep = vec![1.0f32; w * h];
            for e in &p.beside {
                for (i, k) in keep.iter_mut().enumerate() {
                    *k *= 1.0 - e.cov[i] * e.tint.at(i).1 * e.opacity;
                }
            }
            for &j in &knocked {
                p.below[j].cov.iter_mut().zip(&keep).for_each(|(v, k)| *v *= k);
            }
        }
        for b in &self.bevels {
            let (hc, ho, hm) = b.highlight;
            let (sc, so, sm) = b.shadow;
            if b.style == BevelStyle::Stroke {
                // Without a stroke there is nothing to emboss.
                let Some((on, beside)) = &embossed else { continue };
                let shape: Vec<f32> = a.iter().zip(beside).map(|(&q, &o)| (q + o).min(1.0)).collect();
                let d = distances(&shape, w, h, true, b.size + 1.0);
                let (hi, lo) = bevel_light(b, &d, &shape, w, h);
                for (layer, part) in [(&mut p.inner, on), (&mut p.beside, beside)] {
                    let masked = |v: &[f32]| v.iter().zip(part).map(|(&s, &c)| s * c).collect::<Vec<f32>>();
                    layer.push(Layered {
                        cov: masked(&lo),
                        tint: Tint::Solid(sc),
                        mode: sm,
                        opacity: so,
                        paint: Paint::Over,
                    });
                    layer.push(Layered {
                        cov: masked(&hi),
                        tint: Tint::Solid(hc),
                        mode: hm,
                        opacity: ho,
                        paint: Paint::Over,
                    });
                }
                continue;
            }
            let d = dist.as_ref().expect("bevels need distances");
            let (hi, lo) = bevel_light(b, d, a, w, h);
            let inner = |v: &[f32]| v.iter().zip(a).map(|(&s, &r)| if r > 0.0 { s } else { 0.0 }).collect::<Vec<f32>>();
            let outer = |v: &[f32]| v.iter().zip(a).map(|(&s, &r)| s * (1.0 - r)).collect::<Vec<f32>>();
            if b.style != BevelStyle::Outer {
                p.inner.push(Layered {
                    cov: inner(&lo),
                    tint: Tint::Solid(sc),
                    mode: sm,
                    opacity: so,
                    paint: Paint::Over,
                });
                p.inner.push(Layered {
                    cov: inner(&hi),
                    tint: Tint::Solid(hc),
                    mode: hm,
                    opacity: ho,
                    paint: Paint::Over,
                });
            }
            if b.style != BevelStyle::Inner {
                p.beside.push(Layered {
                    cov: outer(&lo),
                    tint: Tint::Solid(sc),
                    mode: sm,
                    opacity: so,
                    paint: Paint::Over,
                });
                p.beside.push(Layered {
                    cov: outer(&hi),
                    tint: Tint::Solid(hc),
                    mode: hm,
                    opacity: ho,
                    paint: Paint::Over,
                });
            }
        }
        if cs.plane() == Some(1) {
            for e in p.below.iter_mut().chain(&mut p.inner).chain(&mut p.beside) {
                e.mode = e.mode.on_grays();
            }
        }
        p
    }
}

/// Effects in modes with a neutral color fade toward it by coverage `k` times opacity, then
/// paint wherever they reach: the fade and the alpha. Hard mix mixes like the other modes.
fn fade_split(k: f32, opacity: f32) -> (f32, f32) {
    (k * opacity, (k > 0.0) as u8 as f32)
}

/// A glow's intensity through its range and contour.
fn shape_glow(g: &Glow, cov: &mut [f32]) {
    for v in cov.iter_mut() {
        *v = shaped(&g.contour, (*v / g.range).min(1.0));
    }
}

/// Highlight and shadow coverage of a bevel, from a height field lit at the bevel's angle and altitude.
fn bevel_light(b: &Bevel, d: &Distances, a: &[f32], w: usize, h: usize) -> (Vec<f32>, Vec<f32>) {
    let size = b.size.max(0.5) as f32;
    let ramp = |t: f32| t.clamp(0.0, 1.0);
    // A smooth pillow folds the blurred shape at the edge level; each side is lit by its own slope.
    let mut fold = None;
    let halves = matches!(b.style, BevelStyle::Pillow | BevelStyle::Emboss);
    let mut height = if b.smooth {
        // Smooth bevels are lit from the blurred shape; an emboss or pillow, half inside and half
        // outside, blurs over half its size.
        let mut g = a.to_vec();
        let size = if halves { size * 0.5 } else { size };
        blur(&mut g, w, h, size as f64 * SIGMA_PER_SIZE, size as f64);
        if b.style == BevelStyle::Pillow {
            if b.soften > 0.0 {
                g.iter_mut().for_each(|v| *v = 0.5 + (*v - 0.5).abs());
            } else {
                fold = Some(g.iter().map(|&v| if v >= 0.5 { 1.0 } else { -1.0 }).collect::<Vec<f32>>());
            }
        }
        g
    } else {
        match b.style {
            BevelStyle::Inner | BevelStyle::Stroke => {
                field(d, w, h, |inside, _, din| if inside { ramp(din / size) } else { 0.0 })
            }
            BevelStyle::Outer => field(d, w, h, |inside, dout, _| if inside { 1.0 } else { ramp(1.0 - dout / size) }),
            BevelStyle::Emboss => field(d, w, h, |inside, dout, din| {
                let s = if inside { din } else { -dout };
                ramp(0.5 + s / size)
            }),
            BevelStyle::Pillow => {
                field(d, w, h, |inside, dout, din| if inside { ramp(din / size) } else { ramp(dout / size) })
            }
        }
    };
    if b.soften > 0.0 {
        // Blurs treat the raster's surroundings as 0, so blur relative to the height far outside.
        let far = if b.style == BevelStyle::Pillow { 1.0 } else { 0.0 };
        height.iter_mut().for_each(|v| *v -= far);
        blur(&mut height, w, h, b.soften * SIGMA_PER_SIZE, b.soften);
        height.iter_mut().for_each(|v| *v += far);
    }
    let lift = (b.depth * size as f64).max(0.01) as f32 * if b.up { 1.0 } else { -1.0 };
    // A smooth emboss or pillow rises 0.6 of its depth (a pillow on either side of the fold).
    let lift = if b.smooth && halves { lift * 0.6 } else { lift };
    // Chisels rise 0.35 of their depth: fit to Photoshop's outer bevels and pillows.
    let lift = if b.smooth { lift } else { lift * 0.35 };
    let (th, alt) = (b.angle.to_radians(), b.altitude.to_radians());
    let light = [(th.cos() * alt.cos()) as f32, (-th.sin() * alt.cos()) as f32, alt.sin() as f32];
    let flat = light[2];
    let mut hi = vec![0f32; w * h];
    let mut lo = vec![0f32; w * h];
    let at =
        |x: isize, y: isize| height[(y.clamp(0, h as isize - 1) as usize) * w + x.clamp(0, w as isize - 1) as usize];
    for y in 0..h as isize {
        for x in 0..w as isize {
            let i = y as usize * w + x as usize;
            let k = fold.as_ref().map_or(0.5 * lift, |f| 0.5 * lift * f[i]);
            let gx = (at(x + 1, y) - at(x - 1, y)) * k;
            let gy = (at(x, y + 1) - at(x, y - 1)) * k;
            let n = [-gx, -gy, 1.0];
            let len = (n[0] * n[0] + n[1] * n[1] + 1.0).sqrt();
            let shade = (n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / len;
            if shade > flat {
                hi[i] = ((shade - flat) / (1.0 - flat).max(1e-3)).min(1.0);
            } else {
                lo[i] = ((flat - shade) / flat.max(1e-3)).min(1.0);
            }
        }
    }
    (hi, lo)
}

/// The layer's own pixels with its interior effects folded in, masked by `region`.
///
/// `content` is premultiplied with the raw layer alpha; `region` is that alpha times the mask.
/// Fill opacity scales the layer's paint but not the effects; `opacity` scales everything.
/// Folds `inner` effects into `content`; the fill applies before them, or after the first
/// `grouped` ones.
pub(crate) fn assemble(
    content: &Raster,
    region: &[f32],
    (fill, grouped): (f32, usize),
    opacity: f32,
    inner: &[Layered],
    backdrop: Option<&Raster>,
) -> Raster {
    let mut out = Raster { x: content.x, y: content.y, w: content.w, h: content.h, px: vec![0.0; content.px.len()] };
    for (i, (o, c)) in out.px.chunks_exact_mut(4).zip(content.px.chunks_exact(4)).enumerate() {
        let r = region[i];
        if r <= 0.0 {
            continue;
        }
        let cs = if c[3] > 0.0 { [c[0] / c[3], c[1] / c[3], c[2] / c[3]] } else { [0.0; 3] };
        let (mut ac, mut pc) = if grouped == 0 { (fill, cs.map(|v| v * fill)) } else { (1.0, cs) };
        for (j, e) in inner.iter().enumerate() {
            if grouped > 0 && j == grouped {
                ac *= fill;
                pc = pc.map(|v| v * fill);
            }
            let (color, ta) = e.tint.at(i);
            if e.paint == Paint::Recolor {
                // The layer's own alpha is applied later: cover it in proportion.
                let k = (e.cov[i] * ta * e.opacity / c[3].max(1e-6)).min(1.0);
                for ch in 0..3 {
                    pc[ch] += k * (color[ch] * ac - pc[ch]);
                }
                continue;
            }
            if e.paint == Paint::Knock {
                // Inside its band the stroke replaces the layer's pixels, over the backdrop.
                let c = e.cov[i] * ta;
                if c <= 0.0 {
                    continue;
                }
                let (bp, ba) = backdrop.map_or(([0.0; 3], 0.0), |b| backdrop_at(b, content, i));
                let mixed = if e.mode.is_normal() || ba <= 1e-6 {
                    color
                } else {
                    let m = e.mode.apply(bp.map(|v| (v / ba).clamp(0.0, 1.0)), color);
                    [0, 1, 2].map(|ch| (1.0 - ba) * color[ch] + ba * m[ch])
                };
                for ch in 0..3 {
                    pc[ch] = (1.0 - c) * pc[ch] + c * e.opacity * mixed[ch];
                }
                ac = (1.0 - c) * ac + c * e.opacity;
                continue;
            }
            let (color, k) = match e.mode.neutral().filter(|_| e.mode != BlendMode::HardMix) {
                Some(n) => {
                    let (f, a) = fade_split(e.cov[i] * ta, e.opacity);
                    (color.map(|v| n + f * (v - n)), a)
                }
                None => (color, e.cov[i] * ta * e.opacity),
            };
            if k <= 0.0 {
                continue;
            }
            let under = if ac > 1e-6 { pc.map(|v| (v / ac).clamp(0.0, 1.0)) } else { color };
            let mixed = if e.mode.is_normal() { color } else { e.mode.apply(under, color) };
            for ch in 0..3 {
                pc[ch] = (1.0 - k) * pc[ch] + k * ((1.0 - ac) * color[ch] + ac * mixed[ch]);
            }
            ac = k + (1.0 - k) * ac;
        }
        if grouped >= inner.len() && grouped > 0 {
            ac *= fill;
            pc = pc.map(|v| v * fill);
        }
        let s = r * opacity;
        o.copy_from_slice(&[pc[0] * s, pc[1] * s, pc[2] * s, ac * s]);
    }
    out
}

/// Premultiplied color and alpha of `backdrop` under pixel `i` of `content`.
fn backdrop_at(backdrop: &Raster, content: &Raster, i: usize) -> ([f32; 3], f32) {
    let x = content.x + (i % content.w) as i32 - backdrop.x;
    let y = content.y + (i / content.w) as i32 - backdrop.y;
    if x < 0 || y < 0 || x as usize >= backdrop.w || y as usize >= backdrop.h {
        return ([0.0; 3], 0.0);
    }
    let o = (y as usize * backdrop.w + x as usize) * 4;
    let p = &backdrop.px[o..o + 4];
    ([p[0], p[1], p[2]], p[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: f64) -> Value {
        Value::Unit("#Pxl".into(), v)
    }

    fn desc(items: Vec<(&str, Value)>) -> Descriptor {
        Descriptor { class: String::new(), items: items.into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
    }

    fn rgb(r: f64, g: f64, b: f64) -> Value {
        Value::Descriptor(desc(vec![
            ("Rd  ", Value::Number(r)),
            ("Grn ", Value::Number(g)),
            ("Bl  ", Value::Number(b)),
        ]))
    }

    fn doc() -> Document {
        Document::blank(4, 4)
    }

    fn square(w: usize, h: usize, lo: usize, hi: usize) -> Vec<f32> {
        let mut a = vec![0f32; w * h];
        for y in lo..hi {
            for x in lo..hi {
                a[y * w + x] = 1.0;
            }
        }
        a
    }

    #[test]
    fn parses_shadow_and_stroke() {
        let shadow = desc(vec![
            ("enab", Value::Bool(true)),
            ("Md  ", Value::Enum("BlnM".into(), "Mltp".into())),
            ("Clr ", rgb(255.0, 0.0, 0.0)),
            ("Opct", Value::Unit("#Prc".into(), 50.0)),
            ("uglg", Value::Bool(false)),
            ("lagl", Value::Unit("#Ang".into(), 90.0)),
            ("Dstn", unit(4.0)),
            ("Ckmt", Value::Unit("#Prc".into(), 25.0)),
            ("blur", unit(8.0)),
        ]);
        let stroke = desc(vec![
            ("enab", Value::Bool(true)),
            ("Styl", Value::Enum("FStl".into(), "InsF".into())),
            ("Sz  ", unit(3.0)),
            ("Clr ", rgb(0.0, 0.0, 255.0)),
        ]);
        let fx = desc(vec![
            ("Scl ", Value::Unit("#Prc".into(), 200.0)), // sizes are stored scaled already
            ("DrSh", Value::Descriptor(shadow)),
            ("FrFX", Value::Descriptor(stroke)),
            ("OrGl", Value::Descriptor(desc(vec![("enab", Value::Bool(false))]))),
        ]);
        let e = parse(&fx, &doc(), &ColorSpace::default());
        let s = &e.drop_shadows[0];
        assert_eq!(s.color, [1.0, 0.0, 0.0]);
        assert_eq!(s.mode, BlendMode::Multiply);
        assert_eq!((s.angle, s.distance, s.size, s.spread), (90.0, 4.0, 8.0, 2.0));
        assert_eq!(s.opacity, 0.5);
        assert_eq!(e.strokes[0].position, StrokePosition::Inside);
        assert_eq!(e.strokes[0].size, 3.0);
        assert_eq!(e.strokes[0].fill, Fill::Solid([0.0, 0.0, 1.0]));
        assert!(e.outer_glows.is_empty());
        assert_eq!(e.reach(), 14.0);
    }

    #[test]
    fn master_switch_disables_everything() {
        let fx = desc(vec![("masterFXSwitch", Value::Bool(false)), ("SoFi", Value::Descriptor(desc(vec![])))]);
        assert!(parse(&fx, &doc(), &ColorSpace::default()).is_empty());
    }

    #[test]
    fn every_effect_kind_is_read() {
        let on = || Value::Descriptor(desc(vec![("enab", Value::Bool(true))]));
        let fx = desc(vec![
            ("IrSh", on()),
            ("IrGl", on()),
            ("ChFX", on()),
            ("GrFl", on()),
            ("patternFill", on()),
            ("ebbl", on()),
        ]);
        let e = parse(&fx, &doc(), &ColorSpace::default());
        assert_eq!(e.inner_shadows.len(), 1);
        assert_eq!(e.inner_glows.len(), 1);
        assert_eq!(e.satins.len(), 1);
        assert!(matches!(e.gradient_overlays[0].fill, Fill::Gradient(_)));
        assert_eq!(e.pattern_overlays.len(), 1);
        assert_eq!(e.bevels[0].style, BevelStyle::Inner);
    }

    #[test]
    fn strokes_split_inside_and_beside() {
        let (w, h) = (21, 21);
        let a = square(w, h, 8, 13);
        let stroke = |size, position| Stroke {
            fill: Fill::Solid([1.0; 3]),
            opacity: 1.0,
            mode: BlendMode::Normal,
            size,
            position,
        };
        let area = |v: &[f32]| v.iter().sum::<f32>();
        let e = Effects { strokes: vec![stroke(2.0, StrokePosition::Outside)], ..Default::default() };
        let p = e.prepare(
            &doc(),
            &ColorSpace::default(),
            &a,
            None,
            (0, 0, w, h),
            1.0,
            ([8.0, 8.0, 13.0, 13.0], [8.0, 8.0, 13.0, 13.0]),
        );
        assert!(p.inner.is_empty());
        let outside = area(&p.beside[0].cov);
        assert!(outside > 4.0 * 5.0 * 2.0 && outside < 81.0 - 25.0, "{outside}");
        let e = Effects { strokes: vec![stroke(1.0, StrokePosition::Inside)], ..Default::default() };
        let p = e.prepare(
            &doc(),
            &ColorSpace::default(),
            &a,
            None,
            (0, 0, w, h),
            1.0,
            ([8.0, 8.0, 13.0, 13.0], [8.0, 8.0, 13.0, 13.0]),
        );
        assert!(p.beside.is_empty());
        assert!((area(&p.inner[0].cov) - 16.0).abs() < 1.0);
    }

    #[test]
    fn inner_shadow_darkens_the_lit_edge_only() {
        let (w, h) = (24, 24);
        let a = square(w, h, 6, 18);
        let s = Shadow {
            color: [0.0; 3],
            opacity: 1.0,
            mode: BlendMode::Multiply,
            angle: 0.0,
            distance: 3.0,
            spread: 0.0,
            size: 0.0,
            knocked_out: false,
            contour: None,
        };
        let e = Effects { inner_shadows: vec![s], ..Default::default() };
        let p = e.prepare(&doc(), &ColorSpace::default(), &a, None, (0, 0, w, h), 1.0, ([0.0; 4], [0.0; 4]));
        let cov = &p.inner[0].cov;
        assert_eq!(cov[12 * w + 7], 0.0);
        assert_eq!(cov[12 * w + 16], 1.0);
        assert_eq!(cov[12 * w + 12], 0.0);
    }

    #[test]
    fn bevel_lights_one_side_and_shades_the_other() {
        let (w, h) = (30, 30);
        let a = square(w, h, 5, 25);
        let b = Bevel {
            style: BevelStyle::Inner,
            smooth: false,
            depth: 1.0,
            up: true,
            size: 5.0,
            soften: 0.0,
            angle: 135.0,
            altitude: 30.0,
            highlight: ([1.0; 3], 1.0, BlendMode::Screen),
            shadow: ([0.0; 3], 1.0, BlendMode::Multiply),
        };
        let e = Effects { bevels: vec![b], ..Default::default() };
        let p = e.prepare(&doc(), &ColorSpace::default(), &a, None, (0, 0, w, h), 1.0, ([0.0; 4], [0.0; 4]));
        let (lo, hi) = (&p.inner[0].cov, &p.inner[1].cov);
        assert!(hi[15 * w + 6] > 0.3 && lo[15 * w + 6] == 0.0);
        assert!(lo[15 * w + 23] > 0.3 && hi[15 * w + 23] == 0.0);
        assert_eq!(hi[15 * w + 15], 0.0);
    }

    #[test]
    fn chisel_outer_bevels_rise_gently() {
        let (w, h) = (30, 30);
        let a = square(w, h, 5, 25);
        let b = Bevel {
            style: BevelStyle::Outer,
            smooth: false,
            depth: 1.0,
            up: true,
            size: 5.0,
            soften: 0.0,
            angle: 90.0,
            altitude: 30.0,
            highlight: ([1.0; 3], 1.0, BlendMode::Screen),
            shadow: ([0.0; 3], 1.0, BlendMode::Multiply),
        };
        let e = Effects { bevels: vec![b], ..Default::default() };
        let p = e.prepare(&doc(), &ColorSpace::default(), &a, None, (0, 0, w, h), 1.0, ([0.0; 4], [0.0; 4]));
        let lo = &p.beside[0].cov;
        // Photoshop's chisels slope at 0.35 of their depth: sides barely shade (effect-enums).
        assert!(lo[15 * w + 2] > 0.02 && lo[15 * w + 2] < 0.1, "{}", lo[15 * w + 2]);
        assert!(lo[27 * w + 15] > 0.5 && lo[27 * w + 15] < 0.75, "{}", lo[27 * w + 15]);
        assert!(p.inner.is_empty());
    }

    #[test]
    fn stroke_embosses_light_only_the_stroke() {
        let (w, h) = (30, 30);
        let a = square(w, h, 8, 22);
        let b = Bevel {
            style: BevelStyle::Stroke,
            smooth: true,
            depth: 1.0,
            up: true,
            size: 4.0,
            soften: 0.0,
            angle: 90.0,
            altitude: 30.0,
            highlight: ([1.0; 3], 1.0, BlendMode::Screen),
            shadow: ([0.0; 3], 1.0, BlendMode::Multiply),
        };
        let stroke = Stroke {
            fill: Fill::Solid([0.0; 3]),
            opacity: 1.0,
            mode: BlendMode::Normal,
            size: 4.0,
            position: StrokePosition::Outside,
        };
        let prepare =
            |e: &Effects| e.prepare(&doc(), &ColorSpace::default(), &a, None, (0, 0, w, h), 1.0, ([0.0; 4], [0.0; 4]));
        // No stroke, no emboss.
        assert!(prepare(&Effects { bevels: vec![b.clone()], ..Default::default() }).beside.is_empty());
        let p = prepare(&Effects { bevels: vec![b], strokes: vec![stroke], ..Default::default() });
        // The stroke, then its shadow and highlight; lit on top, nothing past the stroke.
        assert_eq!(p.beside.len(), 3);
        let hi = &p.beside[2].cov;
        assert!(hi[5 * w + 15] > 0.2, "{}", hi[5 * w + 15]);
        assert_eq!(hi[2 * w + 15], 0.0);
        assert_eq!(hi[15 * w + 15], 0.0);
    }

    #[test]
    fn offsets_round_halves_away_from_zero() {
        assert_eq!(offset(120.0, 5.0), (3, 4));
        assert_eq!(offset(-60.0, 5.0), (-3, -4));
        assert_eq!(offset(-170.0, 30.0), (30, -5));
    }

    #[test]
    fn assemble_folds_overlays_and_fill() {
        let content = Raster { x: 0, y: 0, w: 1, h: 1, px: vec![1.0, 0.0, 0.0, 1.0] };
        let overlay = Layered {
            cov: vec![1.0],
            tint: Tint::Solid([0.0, 0.0, 1.0]),
            mode: BlendMode::Normal,
            opacity: 0.5,
            paint: Paint::Over,
        };
        let out = assemble(&content, &[1.0], (1.0, 0), 1.0, std::slice::from_ref(&overlay), None);
        assert_eq!(out.px, [0.5, 0.0, 0.5, 1.0]);
        let out = assemble(&content, &[1.0], (0.0, 0), 1.0, std::slice::from_ref(&overlay), None);
        assert_eq!(out.px, [0.0, 0.0, 0.5, 0.5]);
        let out = assemble(&content, &[0.5], (1.0, 0), 0.5, &[], None);
        assert_eq!(out.px, [0.25, 0.0, 0.0, 0.25]);
        // Blending interior effects as a group fades the overlay with the fill too.
        let out = assemble(&content, &[1.0], (0.5, 1), 1.0, &[overlay], None);
        assert_eq!(out.px, [0.25, 0.0, 0.25, 0.5]);
    }

    #[test]
    fn assemble_knocks_out_under_inner_strokes() {
        let content = Raster { x: 0, y: 0, w: 1, h: 1, px: vec![1.0, 0.0, 0.0, 1.0] };
        let backdrop = Raster { x: 0, y: 0, w: 1, h: 1, px: vec![0.0, 1.0, 0.0, 1.0] };
        let stroke = |mode| Layered {
            cov: vec![1.0],
            tint: Tint::Solid([0.0, 0.0, 1.0]),
            mode,
            opacity: 0.5,
            paint: Paint::Knock,
        };
        // A half-opaque stroke shows the backdrop through it, not the layer's red.
        let out = assemble(&content, &[1.0], (1.0, 0), 1.0, &[stroke(BlendMode::Normal)], Some(&backdrop));
        assert_eq!(out.px, [0.0, 0.0, 0.5, 0.5]);
        // Other modes blend with the backdrop.
        let out = assemble(&content, &[1.0], (1.0, 0), 1.0, &[stroke(BlendMode::Screen)], Some(&backdrop));
        assert_eq!(out.px, [0.0, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn recolor_keeps_the_layer_alpha() {
        // A half-covered edge pixel of a red shape whose blue vector stroke covers it as much.
        let content = Raster { x: 0, y: 0, w: 1, h: 1, px: vec![0.5, 0.0, 0.0, 0.5] };
        let stroke = Layered {
            cov: vec![0.5],
            tint: Tint::Solid([0.0, 0.0, 1.0]),
            mode: BlendMode::Normal,
            opacity: 1.0,
            paint: Paint::Recolor,
        };
        // Fully blue; the layer's coverage still halves it when painted.
        let out = assemble(&content, &[1.0], (1.0, 0), 1.0, &[stroke], None);
        assert_eq!(out.px, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn neutral_modes_fade_toward_their_neutral() {
        let burn = Layered {
            cov: vec![1.0],
            tint: Tint::Solid([0.0; 3]),
            mode: BlendMode::ColorBurn,
            opacity: 0.5,
            paint: Paint::Over,
        };
        // Color burn with black at half strength burns with mid gray at full alpha.
        let r = burn.raster(0, 0, 1, 1, 1.0, None);
        assert_eq!(r.px, [0.5, 0.5, 0.5, 1.0]);
    }
}
