//! Layer tree compositing.
//!
//! The model follows the PDF transparency rules that Photoshop implements (and psd-tools ports):
//! every group composites into a [`Comp`] that tracks the accumulated group alpha and shape next to
//! the color, which is what knockout, pass-through groups and isolated adjustments need.

pub(crate) mod adjust;
pub(crate) mod canvas;
pub(crate) mod distance;
pub(crate) mod effects;
pub(crate) mod fill;
mod filters;
mod layer;
mod lut;
pub(crate) mod mask;
pub(crate) mod save;
mod smart;
pub(crate) mod vector;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use rayon::prelude::*;

use crate::blend::BlendMode;
use crate::color::{self, ColorSpace};
use crate::fonts::FontDb;
use crate::image::Image;
use crate::psd::descriptor;
use crate::psd::{BlendRange, Document, Layer, LayerKind};
use canvas::Raster;
use effects::{Effects, Prepared};
use layer::{coverage, layer_effects, render_layer, LayerOutput};
use mask::Region;

/// Options for [`render`].
#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    /// Use the raster Photoshop cached for type layers instead of re-rendering the text.
    pub keep_text_raster: bool,
    /// Collect the coverage of every re-rendered type layer in [`Rendered::text_masks`].
    pub text_masks: bool,
    /// Re-render smart objects from their embedded files instead of using the cached pixels.
    /// Smart objects whose contents were edited are always re-rendered.
    pub render_smart_objects: bool,
}

/// Something that could not be rendered exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// Name of the layer concerned, if any.
    pub layer: Option<String>,
    /// Human-readable description.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.layer {
            Some(l) => write!(f, "layer \"{l}\": {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Coverage of one re-rendered type layer, cropped to its bounds.
#[derive(Clone, Debug)]
pub struct TextMask {
    /// Index of the layer in [`Document::layers`].
    pub layer: usize,
    /// Layer name.
    pub name: String,
    /// Left edge in document pixels (may be negative).
    pub x: i32,
    /// Top edge in document pixels (may be negative).
    pub y: i32,
    /// Width of `alpha`.
    pub width: usize,
    /// Height of `alpha`.
    pub height: usize,
    /// 8-bit coverage, row-major.
    pub alpha: Vec<u8>,
}

impl TextMask {
    /// The mask placed on a transparent `width x height` canvas.
    pub fn to_canvas(&self, width: u32, height: u32) -> Vec<u8> {
        let (cw, ch) = (width as i32, height as i32);
        let mut out = vec![0u8; width as usize * height as usize];
        for row in 0..self.height as i32 {
            let y = self.y + row;
            if y < 0 || y >= ch {
                continue;
            }
            let x0 = self.x.max(0);
            let x1 = (self.x + self.width as i32).min(cw);
            if x0 >= x1 {
                continue;
            }
            let src = row as usize * self.width + (x0 - self.x) as usize;
            let dst = y as usize * width as usize + x0 as usize;
            let n = (x1 - x0) as usize;
            out[dst..dst + n].copy_from_slice(&self.alpha[src..src + n]);
        }
        out
    }
}

/// Result of [`render`].
#[derive(Clone, Debug)]
pub struct Rendered {
    /// The flattened image.
    pub image: Image,
    /// Features that were skipped or approximated.
    pub warnings: Vec<Warning>,
    /// Type layer coverage, when [`RenderOptions::text_masks`] is set.
    pub text_masks: Vec<TextMask>,
}

/// What every layer renderer needs.
pub(crate) struct Ctx<'a> {
    pub doc: &'a Document,
    pub fonts: &'a FontDb,
    pub options: &'a RenderOptions,
    pub cs: ColorSpace,
}

enum Node {
    Layer(usize),
    Group(usize, Vec<Node>),
}

impl Node {
    fn index(&self) -> usize {
        match self {
            Node::Layer(i) | Node::Group(i, _) => *i,
        }
    }
}

fn build_tree(doc: &Document) -> Vec<Node> {
    let mut stack: Vec<Vec<Node>> = vec![vec![]];
    for (i, l) in doc.layers.iter().enumerate() {
        match l.kind {
            LayerKind::GroupEnd => stack.push(vec![]),
            LayerKind::Group => {
                let children = if stack.len() > 1 { stack.pop().unwrap_or_default() } else { vec![] };
                stack.last_mut().expect("root level").push(Node::Group(i, children));
            }
            _ => stack.last_mut().expect("root level").push(Node::Layer(i)),
        }
    }
    while stack.len() > 1 {
        let orphans = stack.pop().unwrap_or_default();
        stack.last_mut().expect("root level").extend(orphans);
    }
    stack.pop().unwrap_or_default()
}

fn visible_leaves(nodes: &[Node], doc: &Document, out: &mut Vec<usize>) {
    for n in nodes {
        if doc.layers[n.index()].hidden {
            continue;
        }
        match n {
            Node::Layer(i) => out.push(*i),
            Node::Group(_, children) => visible_leaves(children, doc, out),
        }
    }
}

type Rect = (i32, i32, i32, i32);

fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))),
        (a, b) => a.or(b),
    }
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let r = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
    (r.0 < r.2 && r.1 < r.3).then_some(r)
}

fn pad(r: Rect, p: i32) -> Rect {
    (r.0 - p, r.1 - p, r.2 + p, r.3 + p)
}

fn raster_rect(r: &Raster) -> Rect {
    (r.x, r.y, r.x + r.w as i32, r.y + r.h as i32)
}

fn ratio(v: u8) -> f32 {
    v as f32 / 255.0
}

/// Knockout setting: 0 none, 1 shallow, 2 deep.
fn knockout(l: &Layer) -> u8 {
    l.block(b"knko").and_then(|b| b.first().copied()).unwrap_or(0).min(2)
}

/// Modes for which fill opacity fades the color toward the mode's neutral instead of thinning it.
fn fill_neutral(mode: BlendMode) -> Option<f32> {
    match mode {
        BlendMode::ColorDodge | BlendMode::LinearDodge | BlendMode::Difference => Some(0.0),
        BlendMode::ColorBurn | BlendMode::LinearBurn => Some(1.0),
        BlendMode::VividLight | BlendMode::LinearLight | BlendMode::HardMix => Some(0.5),
        _ => None,
    }
}

/// A compositing context: one group being painted over its backdrop.
struct Comp {
    /// Premultiplied result so far.
    cv: Raster,
    /// Group alpha: everything painted in this context, without the backdrop.
    ga: Vec<f32>,
    /// Group shape.
    sg: Vec<f32>,
    /// The backdrop the context started from; transparent when `None`.
    init: Option<Raster>,
    /// What deep knockout reveals (the Background layer); the initial backdrop when `None`.
    deep: Option<Arc<Raster>>,
    /// Adjustments only affect what this context painted.
    adjust_isolated: bool,
}

impl Comp {
    fn new(r: Rect, init: Option<Raster>, deep: Option<Arc<Raster>>, adjust_isolated: bool) -> Comp {
        let (w, h) = ((r.2 - r.0) as usize, (r.3 - r.1) as usize);
        let cv = match &init {
            Some(i) if raster_rect(i) == r => i.clone(),
            Some(i) => i.crop(r.0, r.1, w, h),
            None => Raster::new(r.0, r.1, w, h),
        };
        let init = init.map(|_| cv.clone());
        Comp { ga: vec![0.0; w * h], sg: vec![0.0; w * h], cv, init, deep, adjust_isolated }
    }

    fn rect(&self) -> Rect {
        raster_rect(&self.cv)
    }

    fn at(&self, x: i32, y: i32) -> usize {
        (y - self.cv.y) as usize * self.cv.w + (x - self.cv.x) as usize
    }

    /// Paints `src` (premultiplied) with `mode`. `shape` is the coverage over `src`'s rectangle,
    /// its alpha when `None`.
    fn paint(&mut self, src: &Raster, shape: Option<&[f32]>, mode: BlendMode, knock: u8) {
        let Some((x0, y0, x1, y1)) = intersect(self.rect(), raster_rect(src)) else { return };
        let shape_at = |x: i32, y: i32| {
            let j = (y - src.y) as usize * src.w + (x - src.x) as usize;
            shape.map_or(src.px[j * 4 + 3], |s| s[j])
        };
        if knock == 0 {
            self.cv.paint(src, mode, 1.0, Some(&mut self.ga));
            for y in y0..y1 {
                for x in x0..x1 {
                    let (i, s) = (self.at(x, y), shape_at(x, y));
                    self.sg[i] = s + self.sg[i] * (1.0 - s);
                }
            }
            return;
        }
        let backdrop = if knock == 2 { self.deep.as_deref().or(self.init.as_ref()) } else { self.init.as_ref() };
        for y in y0..y1 {
            for x in x0..x1 {
                let i = self.at(x, y);
                let shape = shape_at(x, y);
                if shape <= 0.0 {
                    continue;
                }
                let s = &src.px[src.index(x, y)..][..4];
                let a_s = s[3].min(shape);
                let cs = if s[3] > 0.0 { [s[0] / s[3], s[1] / s[3], s[2] / s[3]] } else { [0.0; 3] };
                let ko = backdrop.filter(|b| b.contains(x, y)).map_or([0.0; 4], |b| {
                    let k = b.index(x, y);
                    [b.px[k], b.px[k + 1], b.px[k + 2], b.px[k + 3]]
                });
                let a_ko = ko[3];
                let c_ko = if a_ko > 0.0 { [ko[0] / a_ko, ko[1] / a_ko, ko[2] / a_ko] } else { [0.0; 3] };
                let mixed = if mode.is_normal() { cs } else { mode.apply(c_ko, cs) };
                let a0 = self.init.as_ref().map_or(0.0, |r| r.px[i * 4 + 3]);
                let ga = (1.0 - shape) * self.ga[i] + (shape - a_s) * a_ko + a_s;
                let alpha = a0 + ga - a0 * ga;
                let p = &mut self.cv.px[i * 4..i * 4 + 4];
                for c in 0..3 {
                    let numer =
                        (1.0 - shape) * p[c] + (shape - a_s) * ko[c] + a_s * ((1.0 - a_ko) * cs[c] + a_ko * mixed[c]);
                    p[c] = if alpha > 1e-6 { (numer / alpha).clamp(0.0, 1.0) * alpha } else { 0.0 };
                }
                p[3] = alpha;
                self.ga[i] = ga;
                self.sg[i] = shape + self.sg[i] * (1.0 - shape);
            }
        }
    }

    fn weight<'a>(&'a self, w: impl Fn(i32, i32) -> f32 + Sync + 'a) -> impl Fn(i32, i32) -> f32 + Sync + 'a {
        move |x, y| if self.adjust_isolated { w(x, y) * self.sg[self.at(x, y)] } else { w(x, y) }
    }

    fn adjust(
        &mut self,
        f: &(dyn Fn([f32; 3]) -> [f32; 3] + Sync),
        mode: BlendMode,
        w: impl Fn(i32, i32) -> f32 + Sync,
    ) {
        let mut cv = std::mem::take(&mut self.cv);
        cv.adjust(f, mode, self.weight(w));
        self.cv = cv;
    }

    /// Like [`Comp::adjust`] with a precomputed adjusted image `target`.
    fn adjust_to(&mut self, target: &Raster, mode: BlendMode, w: impl Fn(i32, i32) -> f32 + Sync) {
        let (x0, y0, cw) = (self.cv.x, self.cv.y, self.cv.w);
        let mut px = std::mem::take(&mut self.cv.px);
        let weight = self.weight(w);
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            let a = p[3];
            if a <= 0.0 {
                continue;
            }
            let (x, y) = (x0 + (i % cw) as i32, y0 + (i / cw) as i32);
            let k = weight(x, y);
            let t = &target.px[target.index(x, y)..][..4];
            if k <= 0.0 || t[3] <= 0.0 {
                continue;
            }
            let c = [p[0] / a, p[1] / a, p[2] / a].map(|v| v.clamp(0.0, 1.0));
            let t = [t[0] / t[3], t[1] / t[3], t[2] / t[3]].map(|v| v.clamp(0.0, 1.0));
            let t = if mode.is_normal() { t } else { mode.apply(c, t) };
            for ch in 0..3 {
                p[ch] = (c[ch] + (t[ch] - c[ch]) * k) * a;
            }
        }
        drop(weight);
        self.cv.px = px;
    }
}

struct Compositor<'a> {
    ctx: &'a Ctx<'a>,
    outputs: HashMap<usize, LayerOutput>,
    warnings: Vec<Warning>,
}

impl Compositor<'_> {
    fn layer(&self, n: &Node) -> &Layer {
        &self.ctx.doc.layers[n.index()]
    }

    fn composite(&mut self, nodes: &[Node], comp: &mut Comp, clipping: bool) {
        let mut i = 0;
        while i < nodes.len() {
            let mut j = i + 1;
            while clipping && j < nodes.len() && self.layer(&nodes[j]).clipping {
                j += 1;
            }
            let (base, clips) = (&nodes[i], &nodes[i + 1..j]);
            i = j;
            if self.layer(base).hidden {
                continue;
            }
            let clips: Vec<&Node> = clips.iter().filter(|c| !self.layer(c).hidden).collect();
            self.draw(base, &clips, comp);
        }
    }

    fn composite_refs(&mut self, nodes: &[&Node], comp: &mut Comp) {
        for n in nodes {
            self.draw(n, &[], comp);
        }
    }

    fn draw(&mut self, node: &Node, clips: &[&Node], comp: &mut Comp) {
        let doc = self.ctx.doc;
        match node {
            Node::Layer(i) => {
                let l = &doc.layers[*i];
                let Some(out) = self.outputs.remove(i) else { return };
                if let Some(f) = &out.adjust {
                    self.draw_adjustment(l, f, &out.mask, clips, comp);
                } else if let Some(content) = out.content {
                    let source =
                        Source { content, coverage: out.coverage, effects: out.effects, prepared: out.prepared };
                    self.draw_source(l, self.mode(l.blend_mode), source, clips, comp);
                }
            }
            Node::Group(i, children) => self.draw_group(*i, children, clips, comp),
        }
    }

    /// `m` as this pass applies it.
    fn mode(&self, m: BlendMode) -> BlendMode {
        if self.ctx.cs.plane() == Some(1) {
            m.on_grays()
        } else {
            m
        }
    }

    fn draw_adjustment(&mut self, l: &Layer, f: &adjust::ColorFn, mask: &Region, clips: &[&Node], comp: &mut Comp) {
        let o = ratio(l.opacity) * ratio(l.fill_opacity);
        let w = |x: i32, y: i32| mask.at(x, y) * o;
        if clips.is_empty() {
            comp.adjust(f.as_ref(), self.mode(l.blend_mode), w);
            return;
        }
        let mut target = comp.cv.clone();
        target.adjust(f.as_ref(), BlendMode::Normal, |_, _| 1.0);
        let mut sub = Comp::new(comp.rect(), Some(target), None, false);
        self.composite_refs(clips, &mut sub);
        comp.adjust_to(&sub.cv, self.mode(l.blend_mode), w);
    }

    /// Bounds of what `node` paints, if anything.
    fn extent(&self, node: &Node) -> Option<Rect> {
        let l = self.layer(node);
        if l.hidden {
            return None;
        }
        match node {
            Node::Layer(i) => self.outputs.get(i).and_then(|o| o.content.as_ref()).map(raster_rect),
            Node::Group(_, children) => {
                let inner = children.iter().fold(None, |acc, c| union(acc, self.extent(c)));
                let reach = layer_effects(self.ctx.doc, &self.ctx.cs, l).reach().ceil() as i32;
                inner.map(|r| pad(r, reach))
            }
        }
    }

    fn draw_group(&mut self, index: usize, children: &[Node], clips: &[&Node], comp: &mut Comp) {
        let (doc, cs) = (self.ctx.doc, &self.ctx.cs);
        let l = &doc.layers[index];
        let effects = layer_effects(doc, cs, l);
        let mask = mask::region(doc, l, true);
        let (o, f) = (ratio(l.opacity), ratio(l.fill_opacity));
        let reach = effects.reach().ceil() as i32;
        let artboard = artboard(doc, l, cs);
        let pass_through = l.blend_mode == BlendMode::PassThrough && artboard.is_none();
        if pass_through {
            let knocks = children.iter().any(|c| knockout(self.layer(c)) > 0);
            if o >= 1.0 && f >= 1.0 && mask.is_empty() && clips.is_empty() && effects.is_empty() && !knocks {
                self.composite(children, comp, true);
                return;
            }
            let isolate = f < 1.0 || !clips.is_empty() || !effects.is_empty();
            let mut sub =
                Comp::new(comp.rect(), Some(comp.cv.clone()), comp.deep.clone(), comp.adjust_isolated || isolate);
            self.composite(children, &mut sub, true);
            if !isolate {
                let m = |x: i32, y: i32| mask.at(x, y) * o;
                comp.cv.lerp_to(&sub.cv, m);
                let (x0, y0, w) = (comp.cv.x, comp.cv.y, comp.cv.w);
                for (i, (g, s)) in comp.ga.iter_mut().zip(comp.sg.iter_mut()).enumerate() {
                    let k = m(x0 + (i % w) as i32, y0 + (i / w) as i32);
                    let (a, b) = (k * sub.ga[i], k * sub.sg[i]);
                    *g = a + *g * (1.0 - a);
                    *s = b + *s * (1.0 - b);
                }
                return;
            }
            // Take the backdrop back out: what the group added, as a source of its own.
            let mut content = sub.cv;
            for (i, p) in content.px.chunks_exact_mut(4).enumerate() {
                let g = sub.ga[i];
                let b = &comp.cv.px[i * 4..i * 4 + 4];
                for c in 0..3 {
                    p[c] = (p[c] - (1.0 - g) * b[c]).clamp(0.0, g);
                }
                p[3] = g;
            }
            let content = self.trim(content, reach);
            let source = Source::new(content, mask, effects);
            self.draw_source(l, BlendMode::Normal, source, clips, comp);
            return;
        }
        let canvas = pad(comp.rect(), reach);
        let rect = match &artboard {
            Some((r, _)) => intersect(*r, canvas),
            None => {
                let inner = children.iter().fold(None, |acc, c| union(acc, self.extent(c)));
                inner.and_then(|r| intersect(pad(r, reach), canvas))
            }
        };
        let Some(rect) = rect else {
            self.skip(children);
            return;
        };
        let init = artboard.and_then(|(r, bg)| {
            let (w, h) = ((rect.2 - rect.0) as usize, (rect.3 - rect.1) as usize);
            let _ = r;
            bg.map(|c| Raster::solid(rect.0, rect.1, w, h, &vec![1.0; w * h], c))
        });
        let mut sub = Comp::new(rect, init, None, false);
        self.composite(children, &mut sub, true);
        let mode = if l.blend_mode == BlendMode::PassThrough { BlendMode::Normal } else { self.mode(l.blend_mode) };
        let source = Source::new(sub.cv, mask, effects);
        self.draw_source(l, mode, source, clips, comp);
    }

    /// Drops the transparent border of a group result, keeping `reach` pixels for its effects.
    fn trim(&self, r: Raster, reach: i32) -> Raster {
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for (i, p) in r.px.chunks_exact(4).enumerate() {
            if p[3] > 0.0 {
                let (x, y) = (i % r.w, i / r.w);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
        if x0 >= x1 {
            return Raster::new(r.x, r.y, 0, 0);
        }
        let b = pad((r.x + x0 as i32, r.y + y0 as i32, r.x + x1 as i32, r.y + y1 as i32), reach);
        r.crop(b.0, b.1, (b.2 - b.0) as usize, (b.3 - b.1) as usize)
    }

    fn skip(&mut self, nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Layer(i) => {
                    self.outputs.remove(i);
                }
                Node::Group(_, c) => self.skip(c),
            }
        }
    }

    /// Composites one source: a layer's content or a group's result, with its clipped layers,
    /// mask and effects.
    fn draw_source(&mut self, l: &Layer, mode: BlendMode, mut s: Source, clips: &[&Node], comp: &mut Comp) {
        if s.content.w == 0 || s.content.h == 0 {
            self.skip_refs(clips);
            return;
        }
        // Interior effects cover the clipped layers too, unless they blend with the layer as a
        // group: then the clipped layers paint over them, within the layer's shape whatever its
        // fill.
        let interior_grouped = l.block(b"infx").is_some_and(|b| b.first() == Some(&1));
        if !clips.is_empty() && !interior_grouped {
            let c = &s.content;
            let mut sub = Comp::new(raster_rect(c), Some(c.clone()), None, false);
            self.composite_refs(clips, &mut sub);
            for (p, q) in s.content.px.chunks_exact_mut(4).zip(sub.cv.px.chunks_exact(4)) {
                let raw = p[3];
                if raw > 0.0 && q[3] > 0.0 {
                    for ch in 0..3 {
                        p[ch] = (q[ch] / q[3]).clamp(0.0, 1.0) * raw;
                    }
                }
            }
        }
        let (o, mut f) = (ratio(l.opacity), ratio(l.fill_opacity));
        let c = &s.content;
        let rect = (c.x, c.y, c.w, c.h);
        if s.prepared.is_empty() && !s.effects.is_empty() {
            let b = &l.bounds;
            let bounds = if b.width() > 0 && l.kind != LayerKind::Group {
                [b.left as f64, b.top as f64, b.right as f64, b.bottom as f64]
            } else {
                alpha_bounds(c)
            };
            s.prepared = s.effects.prepare(self.ctx.doc, &self.ctx.cs, &s.coverage, None, rect, f, (bounds, bounds));
        }
        for e in &s.prepared.below {
            comp.paint(&e.raster(c.x, c.y, c.w, c.h, o, None), None, e.mode, 0);
        }
        for e in &s.prepared.beside {
            comp.paint(&e.raster(c.x, c.y, c.w, c.h, o, Some(&s.coverage)), None, e.mode, 0);
        }
        if let Some(neutral) = fill_neutral(mode).filter(|_| f < 1.0) {
            for p in s.content.px.chunks_exact_mut(4) {
                let a = p[3];
                for ch in 0..3 {
                    p[ch] = (neutral + f * (p[ch] / a.max(1e-6) - neutral)) * a;
                }
            }
            f = 1.0;
        }
        let grouped = if interior_grouped { s.prepared.interior } else { 0 };
        let mut body = effects::assemble(&s.content, &s.coverage, (f, grouped), 1.0, &s.prepared.inner);
        if !clips.is_empty() && interior_grouped {
            let mut sub = Comp::new(raster_rect(&body), Some(body.clone()), None, false);
            self.composite_refs(clips, &mut sub);
            for ((p, q), &shape) in body.px.chunks_exact_mut(4).zip(sub.cv.px.chunks_exact(4)).zip(&s.coverage) {
                let a = q[3].min(shape.max(p[3]));
                if q[3] > 0.0 {
                    for ch in 0..3 {
                        p[ch] = (q[ch] / q[3]).clamp(0.0, 1.0) * a;
                    }
                }
                p[3] = a;
            }
        }
        if o < 1.0 {
            body.px.iter_mut().for_each(|v| *v *= o);
        }
        if mode == BlendMode::Dissolve {
            dissolve(&mut body);
        }
        if l.blend_if {
            blend_if(&l.blend_ranges, &mut body, &comp.cv);
        }
        comp.paint(&body, Some(&s.coverage), mode, knockout(l));
    }

    fn skip_refs(&mut self, nodes: &[&Node]) {
        for n in nodes {
            self.skip(std::slice::from_ref(*n));
        }
    }
}

/// Blend If: hides the parts of `body` whose own or underlying (`under`) gray or channel values
/// fall outside the ranges, fading linearly across split sliders.
fn blend_if(ranges: &[BlendRange], body: &mut Raster, under: &Raster) {
    let keep = |r: [u8; 4], v: f32| -> f32 {
        let v = v * 255.0;
        let (b0, b1, w0, w1) = (r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32);
        let black = if v < b0 {
            0.0
        } else if v < b1 {
            (v - b0) / (b1 - b0)
        } else {
            1.0
        };
        let white = if v > w1 {
            0.0
        } else if v > w0 {
            (w1 - v) / (w1 - w0)
        } else {
            1.0
        };
        black * white
    };
    let straight = |p: &[f32]| {
        let a = p[3].max(1e-6);
        [p[0] / a, p[1] / a, p[2] / a]
    };
    let weight = |range: [u8; 4], c: [f32; 3], i: usize| match i {
        0 => keep(range, 0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]),
        _ => keep(range, c[(i - 1).min(2)]),
    };
    for (i, p) in body.px.chunks_exact_mut(4).enumerate() {
        if p[3] <= 0.0 {
            continue;
        }
        let (x, y) = (body.x + (i % body.w) as i32, body.y + (i / body.w) as i32);
        let (ux, uy) = (x - under.x, y - under.y);
        let below = if ux >= 0 && uy >= 0 && (ux as usize) < under.w && (uy as usize) < under.h {
            let o = (uy as usize * under.w + ux as usize) * 4;
            straight(&under.px[o..o + 4])
        } else {
            [0.0; 3]
        };
        let this = straight(p);
        let k: f32 = ranges
            .iter()
            .take(4)
            .enumerate()
            .map(|(j, r)| weight(r.this, this, j) * weight(r.under, below, j))
            .product();
        if k < 1.0 {
            p.iter_mut().for_each(|c| *c *= k);
        }
    }
}

/// Dissolve: each pixel is either fully painted or not, with its alpha as the probability.
fn dissolve(r: &mut Raster) {
    for (i, p) in r.px.chunks_exact_mut(4).enumerate() {
        let a = p[3];
        if a <= 0.0 || a >= 1.0 {
            continue;
        }
        let (x, y) = ((r.x + (i % r.w) as i32) as u32, (r.y + (i / r.w) as i32) as u32);
        let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        let k = if (h & 0xFFFF) as f32 / 65536.0 < a { 1.0 / a } else { 0.0 };
        p.iter_mut().for_each(|c| *c *= k);
    }
}

struct Source {
    content: Raster,
    coverage: Vec<f32>,
    effects: Effects,
    prepared: Prepared,
}

impl Source {
    fn new(content: Raster, mask: Region, effects: Effects) -> Source {
        let coverage = coverage(&content, &mask);
        Source { content, coverage, effects, prepared: Prepared::default() }
    }
}

fn alpha_bounds(r: &Raster) -> [f64; 4] {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for (i, p) in r.px.chunks_exact(4).enumerate() {
        if p[3] > 0.0 {
            let (x, y) = ((r.x + (i % r.w) as i32) as f64, (r.y + (i / r.w) as i32) as f64);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1.0), y1.max(y + 1.0));
        }
    }
    if x0 > x1 {
        [r.x as f64, r.y as f64, (r.x + r.w as i32) as f64, (r.y + r.h as i32) as f64]
    } else {
        [x0, y0, x1, y1]
    }
}

/// The frame and background color of an artboard group.
fn artboard(doc: &Document, l: &Layer, cs: &ColorSpace) -> Option<(Rect, Option<[f32; 3]>)> {
    let d = [b"artb", b"artd", b"abdd"]
        .iter()
        .rev()
        .filter_map(|k| l.block(k))
        .find_map(|b| descriptor::parse_block(b, 4).ok())?;
    let r = d.desc("artboardRect")?;
    let v = |k: &str| r.num(k).unwrap_or(0.0).round() as i32;
    let rect = (v("Left"), v("Top "), v("Rght"), v("Btom"));
    if rect.2 <= rect.0 || rect.3 <= rect.1 {
        return None;
    }
    let bg = match d.num("artboardBackgroundType").unwrap_or(1.0) as i32 {
        1 => Some([1.0; 3]),
        2 => Some([0.0; 3]),
        4 => d.desc("Clr ").and_then(|c| color::from_object(c, cs)),
        _ => None,
    };
    let _ = doc;
    Some((rect, bg))
}

/// The Background layer's pixels, for deep knockout.
fn background(ctx: &Ctx, tree: &[Node]) -> Option<Arc<Raster>> {
    let Node::Layer(i) = tree.first()? else { return None };
    let l = &ctx.doc.layers[*i];
    if l.kind != LayerKind::Pixel || l.channels.contains_key(&-1) {
        return None;
    }
    layer::pixel_raster(ctx.doc, &ctx.cs, l).map(Arc::new)
}

fn composite_image(ctx: &Ctx) -> Raster {
    let doc = ctx.doc;
    let (w, h) = (doc.width as usize, doc.height as usize);
    let mut r = Raster::new(0, 0, w, h);
    let n = w * h;
    let ch = |c: usize| doc.composite.get(c).filter(|v| v.len() >= n).map(Vec::as_slice);
    let colors = doc.color_mode.channels().min(4);
    let alpha = if doc.merged_alpha || doc.layers.is_empty() { ch(colors) } else { None };
    let channels = [0, 1, 2, 3].map(|c| if c < colors { ch(c) } else { None });
    r.px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let s = |c| layer::row(c, y, w);
        layer::decode_row(doc, &ctx.cs, channels.map(s), None, w, row);
        // Photoshop mattes the merged image onto white; take the white back out.
        if let Some(a) = s(alpha) {
            for (p, &a) in row.chunks_exact_mut(4).zip(a) {
                let a = a as f32 / 255.0;
                for c in &mut p[..3] {
                    *c = (*c - (1.0 - a)).clamp(0.0, a);
                }
                p[3] = a;
            }
        }
    });
    r
}

impl Document {
    /// The merged image Photoshop stored in the file, in sRGB, or `None` when there is none
    /// (files saved without "Maximize compatibility").
    pub fn stored_composite(&self) -> Option<Image> {
        let n = self.width as usize * self.height as usize;
        let colors = self.color_mode.channels().min(4);
        if !self.real_composite || n == 0 || (0..colors).any(|c| self.composite.get(c).is_none_or(|p| p.len() < n)) {
            return None;
        }
        let options = RenderOptions::default();
        let fonts = FontDb::new();
        let ctx = Ctx { doc: self, fonts: &fonts, options: &options, cs: ColorSpace::new(self) };
        let mut cv = composite_image(&ctx);
        ctx.cs.encode_linear(&mut cv.px);
        let mut image = Image::from_premultiplied(self.width, self.height, &cv.px);
        ctx.cs.finish(&mut image.data);
        Some(image)
    }
}

/// Flattens `doc` into an image, re-rendering type layers with `fonts`.
///
/// Leaf layers render in parallel on the current rayon pool; compositing then follows the layer tree.
pub fn render(doc: &Document, fonts: &FontDb, options: &RenderOptions) -> Rendered {
    render_inner(doc, fonts, options, true)
}

/// Renders `doc`; without `finish` the image stays in the document's color space.
pub(crate) fn render_inner(doc: &Document, fonts: &FontDb, options: &RenderOptions, finish: bool) -> Rendered {
    let cs = ColorSpace::new(doc);
    if doc.color_mode == crate::psd::ColorMode::Cmyk {
        // Photoshop blends and adjusts CMYK per plate: composite the color plates and black
        // separately, then convert the result.
        let (cmy, k) = rayon::join(
            || composite(doc, fonts, options, cs.with_plane(0)),
            || composite(doc, fonts, options, cs.with_plane(1)),
        );
        let (cmy, warnings, text_masks) = cmy;
        let image = merge_plates(&cs, doc.width, doc.height, &cmy.px, &k.0.px);
        return Rendered { image, warnings, text_masks };
    }
    let (mut cv, warnings, text_masks) = composite(doc, fonts, options, cs.clone());
    cs.encode_linear(&mut cv.px);
    let mut image = Image::from_premultiplied(doc.width, doc.height, &cv.px);
    if finish {
        cs.finish(&mut image.data);
    }
    Rendered { image, warnings, text_masks }
}

/// Straight sRGB from the premultiplied cyan-magenta-yellow and black plate composites.
fn merge_plates(cs: &ColorSpace, width: u32, height: u32, cmy: &[f32], k: &[f32]) -> Image {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let n = width as usize * height as usize;
    let mut ink = vec![0u8; n * 4];
    let mut alpha = vec![0u8; n];
    for (i, (p, b)) in cmy.chunks_exact(4).zip(k.chunks_exact(4)).enumerate() {
        let unmul = |p: &[f32], c: usize| if p[3] > 0.0 { p[c] / p[3] } else { 1.0 };
        ink[i * 4..i * 4 + 4].copy_from_slice(
            &[0, 1, 2].map(|c| 255 - q(unmul(p, c))).into_iter().chain([255 - q(unmul(b, 0))]).collect::<Vec<_>>(),
        );
        alpha[i] = q(p[3]);
    }
    let mut rgb = vec![0u8; n * 3];
    ink.par_chunks(1 << 16).zip(rgb.par_chunks_mut(3 << 14)).for_each(|(i, o)| cs.cmyk_to_rgb(i, o));
    let data = rgb
        .chunks_exact(3)
        .zip(&alpha)
        .flat_map(|(c, &a)| if a > 0 { [c[0], c[1], c[2], a] } else { [0; 4] })
        .collect();
    Image { width, height, data }
}

fn composite(
    doc: &Document,
    fonts: &FontDb,
    options: &RenderOptions,
    cs: ColorSpace,
) -> (Raster, Vec<Warning>, Vec<TextMask>) {
    let ctx = Ctx { doc, fonts, options, cs };
    let (w, h) = (doc.width as usize, doc.height as usize);
    let mut warnings = vec![];
    if doc.color_mode == crate::psd::ColorMode::Duotone && doc.duotone.is_empty() {
        warnings.push(Warning { layer: None, message: "duotone inks could not be read; showing grayscale".into() });
    }
    let mut text_masks = vec![];
    let cv = if doc.layers.is_empty() {
        composite_image(&ctx)
    } else {
        let tree = build_tree(doc);
        let mut leaves = vec![];
        visible_leaves(&tree, doc, &mut leaves);
        let results: Vec<(usize, LayerOutput)> = leaves.par_iter().map(|&i| (i, render_layer(&ctx, i))).collect();
        let mut outputs = HashMap::with_capacity(results.len());
        for (i, mut out) in results {
            let name = &doc.layers[i].name;
            warnings.extend(out.warnings.drain(..).map(|message| Warning { layer: Some(name.clone()), message }));
            text_masks.extend(out.text_mask.take());
            outputs.insert(i, out);
        }
        let deep = background(&ctx, &tree);
        let mut comp = Comp::new((0, 0, w as i32, h as i32), None, deep, false);
        let mut c = Compositor { ctx: &ctx, outputs, warnings: vec![] };
        c.composite(&tree, &mut comp, true);
        warnings.append(&mut c.warnings);
        comp.cv
    };
    (cv, warnings, text_masks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_mask_placement_clips_to_canvas() {
        let m =
            TextMask { layer: 0, name: String::new(), x: -1, y: 1, width: 3, height: 2, alpha: vec![1, 2, 3, 4, 5, 6] };
        assert_eq!(m.to_canvas(3, 2), [0, 0, 0, 2, 3, 0]);
        let far = TextMask { x: 10, ..m };
        assert_eq!(far.to_canvas(3, 2), [0; 6]);
    }

    #[test]
    fn warning_display() {
        let w = Warning { layer: Some("Title".into()), message: "font X not found".into() };
        assert_eq!(w.to_string(), "layer \"Title\": font X not found");
        assert_eq!(Warning { layer: None, message: "m".into() }.to_string(), "m");
    }

    fn solid(x: i32, y: i32, w: usize, h: usize, rgba: [f32; 4]) -> Raster {
        let a = rgba[3];
        Raster { x, y, w, h, px: [rgba[0] * a, rgba[1] * a, rgba[2] * a, a].repeat(w * h) }
    }

    #[test]
    fn knockout_reveals_the_initial_backdrop() {
        let backdrop = solid(0, 0, 1, 1, [0.0, 0.0, 1.0, 1.0]);
        let mut c = Comp::new((0, 0, 1, 1), Some(backdrop), None, false);
        c.paint(&solid(0, 0, 1, 1, [1.0, 0.0, 0.0, 1.0]), None, BlendMode::Normal, 0);
        // Fill 0 with full shape: a hole down to the backdrop.
        c.paint(&solid(0, 0, 1, 1, [0.0, 1.0, 0.0, 0.0]), Some(&[1.0]), BlendMode::Normal, 1);
        assert_eq!(c.cv.px, [0.0, 0.0, 1.0, 1.0]);
        let mut t = Comp::new((0, 0, 1, 1), None, None, false);
        t.paint(&solid(0, 0, 1, 1, [1.0, 0.0, 0.0, 1.0]), None, BlendMode::Normal, 0);
        t.paint(&solid(0, 0, 1, 1, [0.0, 1.0, 0.0, 0.5]), Some(&[1.0]), BlendMode::Normal, 2);
        assert_eq!(t.cv.px, [0.0, 0.5, 0.0, 0.5]);
    }

    #[test]
    fn isolated_adjustments_follow_group_shape() {
        let mut c = Comp::new((0, 0, 2, 1), Some(solid(0, 0, 2, 1, [1.0, 1.0, 1.0, 1.0])), None, true);
        c.paint(&solid(0, 0, 1, 1, [1.0, 1.0, 1.0, 1.0]), None, BlendMode::Normal, 0);
        c.adjust(&|c| c.map(|v| 1.0 - v), BlendMode::Normal, |_, _| 1.0);
        assert_eq!(c.cv.px, [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
    }
}
