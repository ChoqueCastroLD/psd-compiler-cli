//! Layer styles: shadows, glows, satin, overlays, strokes and bevels.
//!
//! [`Effects::prepare`] turns a layer's coverage into effect layers (in parallel with other
//! layers); [`assemble`] then folds the interior ones into the layer's own pixels.

use super::canvas::Raster;
use super::distance::{blur, distances, downsample, Distances, SS};
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
            let a = k * a * self.opacity * scale;
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

fn light_angle(d: &Descriptor, global: f64) -> f64 {
    if d.bool("uglg").unwrap_or(true) {
        global
    } else {
        d.num("lagl").unwrap_or(global)
    }
}

/// Reads an `lfx2`/`lmfx` descriptor.
pub(crate) fn parse(fx: &Descriptor, doc: &Document, cs: &ColorSpace) -> Effects {
    let mut e = Effects::default();
    if fx.bool("masterFXSwitch") == Some(false) {
        return e;
    }
    let (ga, galt) = (doc.global_angle, doc.global_altitude);
    let scale = fx.num("Scl ").unwrap_or(100.0) / 100.0;
    let px = |d: &Descriptor, key: &str, default: f64| d.num(key).unwrap_or(default) * scale;
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

fn offset(angle: f64, distance: f64) -> (i32, i32) {
    let a = angle.to_radians();
    ((-distance * a.cos()).round() as i32, (distance * a.sin()).round() as i32)
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
        a: &[f32],
        path: Option<&[f32]>,
        rect: (i32, i32, usize, usize),
        fill: f32,
        bounds: [f64; 4],
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
        let fill_tint = |f: &Fill| match f {
            Fill::Solid(c) => Tint::Solid(*c),
            _ => {
                let r = f.render(doc, x, y, w, h, None, bounds);
                Tint::Map(
                    r.px.chunks_exact(4)
                        .map(|q| if q[3] > 0.0 { [q[0] / q[3], q[1] / q[3], q[2] / q[3], q[3]] } else { [0.0; 4] })
                        .collect(),
                )
            }
        };
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

        for s in &self.drop_shadows {
            let m = soft(grown(s.spread), s.size, s.spread, 0.0);
            let (dx, dy) = offset(s.angle, s.distance);
            let mut cov = shift(&m, w, h, dx, dy, 0.0);
            if s.knocked_out {
                cov.iter_mut().zip(a).for_each(|(v, &al)| *v *= 1.0 - al * fill.clamp(0.0, 1.0));
            }
            p.below.push(Layered { cov, tint: Tint::Solid(s.color), mode: s.mode, opacity: s.opacity });
        }
        for g in &self.outer_glows {
            let mut cov = soft(grown(g.spread), g.size, g.spread, 0.0);
            let tint = glow_tint(g, &mut cov);
            p.below.push(Layered { cov, tint, mode: g.mode, opacity: g.opacity });
        }

        for o in &self.pattern_overlays {
            p.inner.push(Layered { cov: vec![1.0; w * h], tint: fill_tint(&o.fill), mode: o.mode, opacity: o.opacity });
        }
        for o in &self.gradient_overlays {
            p.inner.push(Layered { cov: vec![1.0; w * h], tint: fill_tint(&o.fill), mode: o.mode, opacity: o.opacity });
        }
        for o in &self.color_overlays {
            p.inner.push(Layered { cov: vec![1.0; w * h], tint: fill_tint(&o.fill), mode: o.mode, opacity: o.opacity });
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
            p.inner.push(Layered { cov: cov.collect(), tint: Tint::Solid(s.color), mode: s.mode, opacity: s.opacity });
        }
        for g in &self.inner_glows {
            let edge = soft(shrunk_inverse(g.spread), g.size, g.spread, 1.0);
            let mut cov: Vec<f32> = if g.center { edge.iter().map(|v| 1.0 - v).collect() } else { edge };
            let tint = glow_tint(g, &mut cov);
            p.inner.push(Layered { cov, tint, mode: g.mode, opacity: g.opacity });
        }
        for s in &self.inner_shadows {
            let m = soft(shrunk_inverse(s.spread), s.size, s.spread, 1.0);
            let (dx, dy) = offset(s.angle, s.distance);
            let cov = shift(&m, w, h, dx, dy, 1.0);
            p.inner.push(Layered { cov, tint: Tint::Solid(s.color), mode: s.mode, opacity: s.opacity });
        }
        let traced = path.filter(|_| !self.strokes.is_empty()).map(|v| {
            let union: Vec<f32> = v.iter().zip(a).map(|(&p, &q)| p.max(q)).collect();
            distances(&union, w, h, true, self.max_distance() + 1.0)
        });
        for s in &self.strokes {
            let d = traced.as_ref().or(dist.as_ref()).expect("strokes need distances");
            let r = (s.size * SS as f64) as f32;
            let (inner_r, outer_r) = match s.position {
                StrokePosition::Outside => (0.0, r),
                StrokePosition::Inside => (r, 0.0),
                StrokePosition::Center => (r / 2.0, r / 2.0),
            };
            let inside = downsample(|i| d.inside[i] && inner_r > 0.0 && d.inward[i] <= inner_r * inner_r, w, h);
            let band = downsample(|i| !d.inside[i] && outer_r > 0.0 && d.outside[i] <= outer_r * outer_r, w, h);
            let tint = fill_tint(&s.fill);
            let share: Vec<f32> =
                inside.iter().zip(a).map(|(&v, &r)| if r > 1e-6 { (v.min(r) / r).min(1.0) } else { 0.0 }).collect();
            if inner_r > 0.0 {
                p.inner.push(Layered { cov: share, tint: tint.clone(), mode: s.mode, opacity: s.opacity });
            }
            if outer_r > 0.0 {
                // Outside a pixel layer's translucent parts, the stroke shows through from below.
                let under = if path.is_none() && s.position == StrokePosition::Outside {
                    downsample(|i| d.inside[i], w, h)
                } else {
                    vec![0.0; w * h]
                };
                let beside = band
                    .iter()
                    .zip(&under)
                    .zip(a)
                    .map(|((&b, &u), &r)| (b + u * (1.0 - r)).min(1.0 - r).max(0.0))
                    .collect();
                p.beside.push(Layered { cov: beside, tint, mode: s.mode, opacity: s.opacity });
            }
        }
        for b in &self.bevels {
            let d = dist.as_ref().expect("bevels need distances");
            let (hi, lo) = bevel_light(b, d, a, w, h);
            let inner = |v: &[f32]| v.iter().zip(a).map(|(&s, &r)| if r > 0.0 { s } else { 0.0 }).collect::<Vec<f32>>();
            let outer = |v: &[f32]| v.iter().zip(a).map(|(&s, &r)| s * (1.0 - r)).collect::<Vec<f32>>();
            let (hc, ho, hm) = b.highlight;
            let (sc, so, sm) = b.shadow;
            if b.style != BevelStyle::Outer {
                p.inner.push(Layered { cov: inner(&lo), tint: Tint::Solid(sc), mode: sm, opacity: so });
                p.inner.push(Layered { cov: inner(&hi), tint: Tint::Solid(hc), mode: hm, opacity: ho });
            }
            if b.style != BevelStyle::Inner {
                p.beside.push(Layered { cov: outer(&lo), tint: Tint::Solid(sc), mode: sm, opacity: so });
                p.beside.push(Layered { cov: outer(&hi), tint: Tint::Solid(hc), mode: hm, opacity: ho });
            }
        }
        p
    }
}

/// Highlight and shadow coverage of a bevel, from a height field lit at the bevel's angle and altitude.
fn bevel_light(b: &Bevel, d: &Distances, a: &[f32], w: usize, h: usize) -> (Vec<f32>, Vec<f32>) {
    let size = b.size.max(0.5) as f32;
    let ramp = |t: f32| t.clamp(0.0, 1.0);
    let mut height = match b.style {
        BevelStyle::Inner => field(d, w, h, |inside, _, din| if inside { ramp(din / size) } else { 0.0 }),
        BevelStyle::Outer => field(d, w, h, |inside, dout, _| if inside { 1.0 } else { ramp(1.0 - dout / size) }),
        BevelStyle::Emboss => field(d, w, h, |inside, dout, din| {
            let s = if inside { din } else { -dout };
            ramp(0.5 + s / size)
        }),
        BevelStyle::Pillow => {
            field(d, w, h, |inside, dout, din| if inside { ramp(din / size) } else { ramp(dout / size) })
        }
    };
    if b.smooth {
        let s = size as f64 * 0.5;
        blur(&mut height, w, h, s * SIGMA_PER_SIZE * 2.0, s * 2.0);
        if b.style == BevelStyle::Inner {
            height.iter_mut().zip(a).for_each(|(v, &r)| *v *= r);
        }
    }
    if b.soften > 0.0 {
        blur(&mut height, w, h, b.soften * SIGMA_PER_SIZE, b.soften);
    }
    let lift = (b.depth * size as f64).max(0.01) as f32 * if b.up { 1.0 } else { -1.0 };
    let (th, alt) = (b.angle.to_radians(), b.altitude.to_radians());
    let light = [(th.cos() * alt.cos()) as f32, (-th.sin() * alt.cos()) as f32, alt.sin() as f32];
    let flat = light[2];
    let mut hi = vec![0f32; w * h];
    let mut lo = vec![0f32; w * h];
    let at =
        |x: isize, y: isize| height[(y.clamp(0, h as isize - 1) as usize) * w + x.clamp(0, w as isize - 1) as usize];
    for y in 0..h as isize {
        for x in 0..w as isize {
            let gx = (at(x + 1, y) - at(x - 1, y)) * 0.5 * lift;
            let gy = (at(x, y + 1) - at(x, y - 1)) * 0.5 * lift;
            let n = [-gx, -gy, 1.0];
            let len = (n[0] * n[0] + n[1] * n[1] + 1.0).sqrt();
            let shade = (n[0] * light[0] + n[1] * light[1] + n[2] * light[2]) / len;
            let i = y as usize * w + x as usize;
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
pub(crate) fn assemble(content: &Raster, region: &[f32], fill: f32, opacity: f32, inner: &[Layered]) -> Raster {
    let mut out = Raster { x: content.x, y: content.y, w: content.w, h: content.h, px: vec![0.0; content.px.len()] };
    for (i, (o, c)) in out.px.chunks_exact_mut(4).zip(content.px.chunks_exact(4)).enumerate() {
        let r = region[i];
        if r <= 0.0 {
            continue;
        }
        let cs = if c[3] > 0.0 { [c[0] / c[3], c[1] / c[3], c[2] / c[3]] } else { [0.0; 3] };
        let mut ac = fill;
        let mut pc = cs.map(|v| v * fill);
        for e in inner {
            let (color, ta) = e.tint.at(i);
            let k = e.cov[i] * ta * e.opacity;
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
        let s = r * opacity;
        o.copy_from_slice(&[pc[0] * s, pc[1] * s, pc[2] * s, ac * s]);
    }
    out
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
            ("Scl ", Value::Unit("#Prc".into(), 200.0)),
            ("DrSh", Value::Descriptor(shadow)),
            ("FrFX", Value::Descriptor(stroke)),
            ("OrGl", Value::Descriptor(desc(vec![("enab", Value::Bool(false))]))),
        ]);
        let e = parse(&fx, &doc(), &ColorSpace::default());
        let s = &e.drop_shadows[0];
        assert_eq!(s.color, [1.0, 0.0, 0.0]);
        assert_eq!(s.mode, BlendMode::Multiply);
        assert_eq!((s.angle, s.distance, s.size, s.spread), (90.0, 8.0, 16.0, 4.0));
        assert_eq!(s.opacity, 0.5);
        assert_eq!(e.strokes[0].position, StrokePosition::Inside);
        assert_eq!(e.strokes[0].size, 6.0);
        assert_eq!(e.strokes[0].fill, Fill::Solid([0.0, 0.0, 1.0]));
        assert!(e.outer_glows.is_empty());
        assert_eq!(e.reach(), 26.0);
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
        let p = e.prepare(&doc(), &a, None, (0, 0, w, h), 1.0, [8.0, 8.0, 13.0, 13.0]);
        assert!(p.inner.is_empty());
        let outside = area(&p.beside[0].cov);
        assert!(outside > 4.0 * 5.0 * 2.0 && outside < 81.0 - 25.0, "{outside}");
        let e = Effects { strokes: vec![stroke(1.0, StrokePosition::Inside)], ..Default::default() };
        let p = e.prepare(&doc(), &a, None, (0, 0, w, h), 1.0, [8.0, 8.0, 13.0, 13.0]);
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
        };
        let e = Effects { inner_shadows: vec![s], ..Default::default() };
        let p = e.prepare(&doc(), &a, None, (0, 0, w, h), 1.0, [0.0; 4]);
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
        let p = e.prepare(&doc(), &a, None, (0, 0, w, h), 1.0, [0.0; 4]);
        let (lo, hi) = (&p.inner[0].cov, &p.inner[1].cov);
        assert!(hi[15 * w + 6] > 0.3 && lo[15 * w + 6] == 0.0);
        assert!(lo[15 * w + 23] > 0.3 && hi[15 * w + 23] == 0.0);
        assert_eq!(hi[15 * w + 15], 0.0);
    }

    #[test]
    fn assemble_folds_overlays_and_fill() {
        let content = Raster { x: 0, y: 0, w: 1, h: 1, px: vec![1.0, 0.0, 0.0, 1.0] };
        let overlay =
            Layered { cov: vec![1.0], tint: Tint::Solid([0.0, 0.0, 1.0]), mode: BlendMode::Normal, opacity: 0.5 };
        let out = assemble(&content, &[1.0], 1.0, 1.0, &[overlay.clone()]);
        assert_eq!(out.px, [0.5, 0.0, 0.5, 1.0]);
        let out = assemble(&content, &[1.0], 0.0, 1.0, &[overlay]);
        assert_eq!(out.px, [0.0, 0.0, 0.5, 0.5]);
        let out = assemble(&content, &[0.5], 1.0, 0.5, &[]);
        assert_eq!(out.px, [0.25, 0.0, 0.0, 0.25]);
    }
}
