//! Layer tree compositing.

pub(crate) mod canvas;
pub(crate) mod distance;
pub(crate) mod effects;
mod layer;

use std::collections::HashMap;
use std::fmt;

use rayon::prelude::*;

use crate::blend::BlendMode;
use crate::fonts::FontDb;
use crate::image::Image;
use crate::psd::{ColorMode, Document, LayerKind};
use canvas::{Alpha, Canvas, Paint, Raster};
use layer::{apply_mask, render_layer, LayerOutput};

/// Options for [`render`].
#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    /// Use the raster Photoshop cached for type layers instead of re-rendering the text.
    pub keep_text_raster: bool,
    /// Collect the coverage of every re-rendered type layer in [`Rendered::text_masks`].
    pub text_masks: bool,
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

fn composite(nodes: &[Node], doc: &Document, outputs: &mut HashMap<usize, LayerOutput>, cv: &mut Canvas) {
    let mut base: Option<Alpha> = None;
    let mut base_visible = true;
    for node in nodes {
        let l = &doc.layers[node.index()];
        if !l.clipping {
            base_visible = !l.hidden;
            base = None;
        }
        if l.hidden || (l.clipping && (!base_visible || base.is_none())) {
            continue;
        }
        let opacity = l.opacity as f32 / 255.0;
        match node {
            Node::Layer(i) => {
                let Some(mut out) = outputs.remove(i) else { continue };
                if !l.clipping {
                    base = Some(out.base.take().unwrap_or_default());
                }
                let clip = if l.clipping { base.as_ref() } else { None };
                for p in &out.paints {
                    cv.paint(p, opacity, clip);
                }
            }
            Node::Group(_, children) => {
                let pass_through = l.blend_mode == BlendMode::PassThrough;
                if pass_through && opacity >= 1.0 && l.mask.is_none() && !l.clipping {
                    let before = cv.alpha();
                    composite(children, doc, outputs, cv);
                    let added = cv.alpha().iter().zip(&before).map(|(a, b)| (a - b).max(0.0)).collect();
                    base = Some(Alpha { x: 0, y: 0, w: cv.w, h: cv.h, a: added });
                } else {
                    let mut sub = Canvas::new(cv.w, cv.h);
                    composite(children, doc, outputs, &mut sub);
                    let mut raster = Raster { x: 0, y: 0, w: cv.w, h: cv.h, px: sub.px };
                    apply_mask(&mut raster, l);
                    if !l.clipping {
                        base = Some(Alpha::of(&raster));
                    }
                    let clip = if l.clipping { base.as_ref() } else { None };
                    let mode = if pass_through { BlendMode::Normal } else { l.blend_mode };
                    cv.paint(&Paint { raster, mode, opacity }, 1.0, clip);
                }
            }
        }
    }
}

fn composite_image(doc: &Document, cv: &mut Canvas) {
    let get = |c: usize, i: usize| doc.composite.get(c).and_then(|v| v.get(i)).copied().unwrap_or(255) as f32 / 255.0;
    let extra = |n: u16, i: usize| if doc.channel_count > n { get(n as usize, i) } else { 1.0 };
    for (i, p) in cv.px.chunks_exact_mut(4).enumerate() {
        let (rgb, a) = match doc.color_mode {
            ColorMode::Grayscale | ColorMode::Bitmap => ([get(0, i); 3], extra(1, i)),
            ColorMode::Cmyk => {
                let k = get(3, i);
                ([get(0, i) * k, get(1, i) * k, get(2, i) * k], 1.0)
            }
            _ => ([get(0, i), get(1, i), get(2, i)], extra(3, i)),
        };
        p.copy_from_slice(&[rgb[0] * a, rgb[1] * a, rgb[2] * a, a]);
    }
}

/// Flattens `doc` into an image, re-rendering type layers with `fonts`.
///
/// Leaf layers render in parallel on the current rayon pool; compositing then follows the layer tree.
pub fn render(doc: &Document, fonts: &FontDb, options: &RenderOptions) -> Rendered {
    let (w, h) = (doc.width as usize, doc.height as usize);
    let mut cv = Canvas::new(w, h);
    let mut warnings = vec![];
    let mut text_masks = vec![];
    if doc.layers.is_empty() {
        composite_image(doc, &mut cv);
    } else {
        let tree = build_tree(doc);
        let mut leaves = vec![];
        visible_leaves(&tree, doc, &mut leaves);
        let results: Vec<(usize, LayerOutput)> = leaves
            .par_iter()
            .map(|&i| (i, render_layer(doc, i, fonts, options.keep_text_raster, options.text_masks)))
            .collect();
        let mut outputs = HashMap::with_capacity(results.len());
        for (i, mut out) in results {
            let name = &doc.layers[i].name;
            warnings.extend(out.warnings.drain(..).map(|message| Warning { layer: Some(name.clone()), message }));
            text_masks.extend(out.text_mask.take());
            outputs.insert(i, out);
        }
        composite(&tree, doc, &mut outputs, &mut cv);
    }
    if doc.color_mode == ColorMode::Indexed {
        warnings.push(Warning { layer: None, message: "indexed color is rendered without its palette".into() });
    }
    Rendered { image: Image::from_premultiplied(doc.width, doc.height, &cv.px), warnings, text_masks }
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
}
