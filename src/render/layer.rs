//! Rasterizes one leaf layer: its content (pixels, text or synthesized fill), mask and effects.

use rayon::prelude::*;

use super::adjust::{self, ColorFn};
use super::canvas::{zeroed, Raster};
use super::effects::{self, Effects, Prepared};
use super::fill::Fill;
use super::mask::{self, Region};
use super::vector;
use super::{Ctx, TextMask};
use crate::color::{self, ColorSpace};
use crate::psd::descriptor;
use crate::psd::reader::Reader;
use crate::psd::{ColorMode, Document, Layer, LayerKind, FILL_KEYS};
use crate::text::path::{self, Seg};
use crate::text::{layout, AntiAlias, TextLayer};

/// Largest text raster, in pixels, before a layer is skipped as runaway.
/// Pixels per band when drawing type.
const TEXT_BAND_PIXELS: usize = 1 << 22;

#[derive(Default)]
pub(crate) struct LayerOutput {
    /// Premultiplied pixels with the layer's raw alpha, padded for its effects.
    pub content: Option<Raster>,
    /// The layer's mask.
    pub mask: Region,
    /// Raw alpha times mask over the content rectangle.
    pub coverage: Vec<f32>,
    pub effects: Effects,
    pub prepared: Prepared,
    pub adjust: Option<ColorFn>,
    pub warnings: Vec<String>,
    pub text_mask: Option<TextMask>,
}

/// Converts one row of samples to premultiplied RGBA for the document's color mode.
///
/// `ch` holds up to four color channels and `alpha` the transparency, each `n` samples long.
pub(crate) fn decode_row(
    doc: &Document,
    cs: &ColorSpace,
    ch: [Option<&[u8]>; 4],
    alpha: Option<&[u8]>,
    n: usize,
    out: &mut [f32],
) {
    let get = |c: Option<&[u8]>, i: usize, default: u8| c.map_or(default, |v| v[i]);
    let mut rgb = vec![[0f32; 3]; n];
    match doc.color_mode {
        ColorMode::Rgb => {
            for (i, p) in rgb.iter_mut().enumerate() {
                *p = [0, 1, 2].map(|c| get(ch[c], i, 0) as f32 / 255.0);
            }
        }
        ColorMode::Cmyk if cs.plane().is_some() => {
            let plane = cs.plane().unwrap_or(0);
            for (i, p) in rgb.iter_mut().enumerate() {
                *p = ColorSpace::plates(plane, [0, 1, 2, 3].map(|c| 255 - get(ch[c], i, 255)));
            }
        }
        ColorMode::Cmyk => {
            let ink: Vec<u8> = (0..n).flat_map(|i| [0, 1, 2, 3].map(|c| 255 - get(ch[c], i, 255))).collect();
            let mut out8 = vec![0u8; n * 3];
            cs.cmyk_to_rgb(&ink, &mut out8);
            for (p, c) in rgb.iter_mut().zip(out8.chunks_exact(3)) {
                *p = [c[0], c[1], c[2]].map(|v| v as f32 / 255.0);
            }
        }
        ColorMode::Lab => {
            for (i, p) in rgb.iter_mut().enumerate() {
                let l = get(ch[0], i, 0) as f64 / 255.0 * 100.0;
                *p = color::lab_to_rgb(l, get(ch[1], i, 128) as f64 - 128.0, get(ch[2], i, 128) as f64 - 128.0);
            }
        }
        ColorMode::Indexed => {
            for (i, p) in rgb.iter_mut().enumerate() {
                let idx = get(ch[0], i, 0);
                let c = doc.palette.get(idx as usize).copied().unwrap_or([idx; 3]);
                *p = c.map(|v| v as f32 / 255.0);
            }
        }
        _ => {
            for (i, p) in rgb.iter_mut().enumerate() {
                *p = [get(ch[0], i, 0) as f32 / 255.0; 3];
            }
        }
    }
    if cs.is_linear() {
        for p in &mut rgb {
            *p = cs.srgb_to_working(*p);
        }
    }
    for (i, (p, c)) in out.chunks_exact_mut(4).zip(&rgb).enumerate() {
        let mut a = get(alpha, i, 255) as f32 / 255.0;
        if doc.color_mode == ColorMode::Indexed && doc.transparent_index == Some(get(ch[0], i, 0)) {
            a = 0.0;
        }
        p.copy_from_slice(&[c[0] * a, c[1] * a, c[2] * a, a]);
    }
}

/// Row `y` of a `w`-wide plane.
pub(crate) fn row(c: Option<&[u8]>, y: usize, w: usize) -> Option<&[u8]> {
    c.map(|v| &v[y * w..(y + 1) * w])
}

fn has_pixels(l: &Layer) -> bool {
    l.bounds.width() > 0 && l.bounds.height() > 0 && l.channels.keys().any(|&id| id >= -1)
}

pub(crate) fn pixel_raster(doc: &Document, cs: &ColorSpace, l: &Layer) -> Option<Raster> {
    let (w, h) = (l.bounds.width(), l.bounds.height());
    if w == 0 || h == 0 || !has_pixels(l) {
        return None;
    }
    let n = w * h;
    let ch = |id: i16| l.channels.get(&id).filter(|c| c.len() >= n).map(Vec::as_slice);
    let colors = [ch(0), ch(1), ch(2), ch(3)];
    let alpha = ch(-1);
    let mut px = vec![0f32; n * 4];
    px.par_chunks_mut(w * 4).enumerate().for_each(|(y, out)| {
        let s = |c| row(c, y, w);
        decode_row(doc, cs, colors.map(s), s(alpha), w, out);
    });
    Some(Raster { x: l.bounds.left, y: l.bounds.top, w, h, px })
}

/// A fill or shape layer drawn from its description, for layers stored without pixels. With
/// `bake`, the vector mask is drawn into the fill; otherwise the fill covers the canvas.
fn shape_raster(doc: &Document, cs: &ColorSpace, l: &Layer, bake: bool) -> Option<Raster> {
    let desc = match FILL_KEYS.iter().find_map(|k| l.block(k)) {
        Some(b) => descriptor::parse_block(b, 4).ok(),
        // The fill of a stroked shape: its type key, then the descriptor.
        None => l.block(b"vscg").and_then(|b| descriptor::parse_block(b, 8).ok()),
    }?;
    let fill = Fill::parse(&desc, cs);
    let vm = l.block(b"vmsk").or(l.block(b"vsms")).and_then(|b| vector::parse(b, doc.width, doc.height));
    let vm = vm.filter(|m| !m.disabled);
    let stroke = l
        .block(b"vstk")
        .and_then(|b| descriptor::parse_block(b, 4).ok())
        .and_then(|d| vector::parse_stroke(&d))
        .filter(|s| s.visible());
    let reach = stroke.as_ref().map_or(0.0, |s| s.width) + 2.0;
    let canvas = (0, 0, doc.width as i32, doc.height as i32);
    let path_box = vm.as_ref().filter(|m| bake && !m.invert).and_then(|m| m.bounds());
    let (x0, y0, x1, y1) = match path_box {
        Some(b) => (
            (b[0] - reach).floor() as i32,
            (b[1] - reach).floor() as i32,
            (b[2] + reach).ceil() as i32,
            (b[3] + reach).ceil() as i32,
        ),
        None => canvas,
    };
    let (x0, y0, x1, y1) = (x0.max(-64), y0.max(-64), x1.min(canvas.2 + 64), y1.min(canvas.3 + 64));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    // Aligned gradients and patterns span the visible area: the vector path, else a raster mask
    // that hides everything outside it, else the layer.
    let rect_box = |r: &crate::psd::Rect| [r.left as f64, r.top as f64, r.right as f64, r.bottom as f64];
    let mask_box = l
        .mask
        .as_ref()
        .filter(|m| !m.disabled && m.default == 0 && m.rect.width() > 0 && m.rect.height() > 0)
        .map(|m| rect_box(&m.rect));
    let bounds =
        vm.as_ref().filter(|m| !m.invert).and_then(|m| m.bounds()).or(mask_box).unwrap_or(if l.bounds.width() > 0 {
            rect_box(&l.bounds)
        } else {
            [0.0, 0.0, doc.width as f64, doc.height as f64]
        });
    let fill_on = stroke.as_ref().is_none_or(|s| s.fill_enabled);
    let cov = match (&vm, fill_on) {
        (_, false) => Some(vec![0.0; w * h]),
        (Some(m), true) if bake => Some(m.rasterize(x0, y0, w, h)),
        _ => None,
    };
    let mut r = fill.render(doc, cs, x0, y0, w, h, cov.as_deref(), bounds);
    if let (Some(s), Some(m)) = (&stroke, &vm) {
        let line = s.rasterize(m, x0, y0, w, h);
        let paint = s.content.as_ref().map_or(Fill::Solid([0.0; 3]), |d| Fill::parse(d, cs));
        let sr = paint.render(doc, cs, x0, y0, w, h, Some(&line), bounds);
        r.paint(&sr, crate::blend::BlendMode::Normal, s.opacity, None);
    }
    Some(r)
}

/// Re-renders type layer `l`, clipped to the canvas grown by `pad`. `Err` when its text cannot
/// be read, so the cached pixels stand in.
pub(crate) fn text_raster(
    doc: &Document,
    cs: &ColorSpace,
    l: &Layer,
    fonts: &crate::fonts::FontDb,
    pad: usize,
    warnings: &mut Vec<String>,
) -> Result<Option<Raster>, ()> {
    let Some(block) = l.block(b"TySh") else { return Err(()) };
    let tl = match TextLayer::parse(block) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("text could not be parsed ({e}); using the cached pixels"));
            return Err(());
        }
    };
    Ok(draw_text(doc, cs, &tl, fonts, pad, warnings))
}

fn draw_text(
    doc: &Document,
    cs: &ColorSpace,
    tl: &TextLayer,
    fonts: &crate::fonts::FontDb,
    pad: usize,
    warnings: &mut Vec<String>,
) -> Option<Raster> {
    let laid = layout::layout(tl, fonts);
    for (f, used) in &laid.substitutions {
        warnings.push(match used {
            Some(u) => format!("font {f} not found; using {u}"),
            None => format!("font {f} not found; using a fallback"),
        });
    }
    let t = tl.transform;
    let scale = (t[0] * t[3] - t[1] * t[2]).abs().sqrt();
    let glyphs: Vec<(path::Path, [f32; 4], f64)> = laid
        .glyphs
        .iter()
        .map(|g| {
            (
                path::map(&g.path, |x, y| (t[0] * x + t[2] * y + t[4], t[1] * x + t[3] * y + t[5])),
                {
                    let c = cs.srgb_to_working([g.color[0], g.color[1], g.color[2]]);
                    [c[0], c[1], c[2], g.color[3]]
                },
                g.bold * scale,
            )
        })
        .collect();
    let bold = glyphs.iter().map(|g| g.2).fold(0.0, f64::max);
    let [x0, y0, x1, y1] = path::bounds(glyphs.iter().map(|g| &g.0))?;
    let margin = pad as i32 + 1 + bold.ceil() as i32;
    // Nothing beyond the canvas and the reach of the layer's effects can show.
    let clip = (-margin, -margin, doc.width as i32 + margin, doc.height as i32 + margin);
    let ix = (x0.floor() as i32 - margin).max(clip.0);
    let iy = (y0.floor() as i32 - margin).max(clip.1);
    let ix1 = (x1.ceil() as i32 + margin).min(clip.2);
    let iy1 = (y1.ceil() as i32 + margin).min(clip.3);
    if ix1 <= ix || iy1 <= iy {
        return None;
    }
    let (w, h) = ((ix1 - ix) as usize, (iy1 - iy) as usize);
    let (ox, oy) = (ix as f64, iy as f64);
    let pt = |x: f64, y: f64| ((x - ox) as f32, (y - oy) as f32);
    let mut paths = Vec::with_capacity(glyphs.len());
    for (p, color, bold) in &glyphs {
        let mut pb = tiny_skia::PathBuilder::new();
        for seg in p {
            match *seg {
                Seg::Move(x, y) => {
                    let (x, y) = pt(x, y);
                    pb.move_to(x, y)
                }
                Seg::Line(x, y) => {
                    let (x, y) = pt(x, y);
                    pb.line_to(x, y)
                }
                Seg::Quad(x1, y1, x, y) => {
                    let ((x1, y1), (x, y)) = (pt(x1, y1), pt(x, y));
                    pb.quad_to(x1, y1, x, y)
                }
                Seg::Cubic(x1, y1, x2, y2, x, y) => {
                    let ((x1, y1), (x2, y2), (x, y)) = (pt(x1, y1), pt(x2, y2), pt(x, y));
                    pb.cubic_to(x1, y1, x2, y2, x, y)
                }
                Seg::Close => pb.close(),
            }
        }
        let Some(path) = pb.finish() else { continue };
        let [r, g, b, a] = color.map(|c| c.clamp(0.0, 1.0));
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::from_rgba(r, g, b, a)?);
        paint.anti_alias = tl.anti_alias != AntiAlias::None;
        paths.push((path, paint, *bold));
    }
    // Drawn in bands so the 8-bit canvas stays small however large the layer is.
    let band = (TEXT_BAND_PIXELS / w).clamp(1, h);
    let mut px = zeroed(w * h * 4);
    for (i, out) in px.chunks_mut(w * band * 4).enumerate() {
        let rows = out.len() / (w * 4);
        let mut pixmap = tiny_skia::Pixmap::new(w as u32, rows as u32)?;
        let shift = tiny_skia::Transform::from_translate(0.0, -((i * band) as f32));
        let (top, bottom) = ((i * band) as f32, (i * band + rows) as f32);
        for (path, paint, bold) in &paths {
            let reach = *bold as f32;
            if path.bounds().top() - reach > bottom || path.bounds().bottom() + reach < top {
                continue;
            }
            pixmap.fill_path(path, paint, tiny_skia::FillRule::Winding, shift, None);
            if *bold > 0.0 {
                let stroke =
                    tiny_skia::Stroke { width: reach, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
                pixmap.stroke_path(path, paint, &stroke, shift, None);
            }
        }
        out.iter_mut().zip(pixmap.data()).for_each(|(o, &v)| *o = v as f32 / 255.0);
    }
    let exponent = tl.anti_alias.coverage_exponent();
    if exponent != 1.0 {
        for p in px.chunks_exact_mut(4) {
            let a = p[3];
            if a > 0.0 && a < 1.0 {
                let k = a.powf(exponent) / a;
                p.iter_mut().for_each(|c| *c *= k);
            }
        }
    }
    Some(Raster { x: ix, y: iy, w, h, px })
}

/// The layer style of `l`.
pub(crate) fn layer_effects(doc: &Document, cs: &ColorSpace, l: &Layer) -> Effects {
    l.block(b"lmfx")
        .or(l.block(b"lfx2"))
        .or(l.block(b"lfxs"))
        .and_then(|b| descriptor::parse_block(b, 8).ok())
        .map(|d| effects::parse(&d, doc, cs))
        .map(|mut e| {
            // Patterns linked with the layer start at its effects reference point.
            let mut r = Reader::new(l.block(b"fxrp").unwrap_or_default());
            if let (Ok(x), Ok(y)) = (r.f64(), r.f64()) {
                let fills =
                    e.pattern_overlays.iter_mut().map(|o| &mut o.fill).chain(e.strokes.iter_mut().map(|s| &mut s.fill));
                for f in fills {
                    if let Fill::Pattern(p) = f {
                        if p.align {
                            p.phase = (p.phase.0 + x, p.phase.1 + y);
                        }
                    }
                }
            }
            e
        })
        .unwrap_or_default()
}

/// Raw alpha of `content` times `mask`.
pub(crate) fn coverage(content: &Raster, mask: &Region) -> Vec<f32> {
    let mut a = content.alpha();
    if !mask.is_empty() {
        let m = mask.grid(content.x, content.y, content.w, content.h);
        a.iter_mut().zip(&m).for_each(|(v, k)| *v *= k);
    }
    a
}

/// A shape layer's vector coverage over `content`, times its mask: what its strokes follow.
fn path_coverage(doc: &Document, l: &Layer, content: &Raster, mask: &Region) -> Option<Vec<f32>> {
    let vm = l.block(b"vmsk").or(l.block(b"vsms")).and_then(|b| vector::parse(b, doc.width, doc.height))?;
    if vm.disabled {
        return None;
    }
    let mut v = vm.rasterize(content.x, content.y, content.w, content.h);
    if !mask.is_empty() {
        let m = mask.grid(content.x, content.y, content.w, content.h);
        v.iter_mut().zip(&m).for_each(|(a, k)| *a *= k);
    }
    Some(v)
}

/// Renders leaf layer `index`.
pub(crate) fn render_layer(ctx: &Ctx, index: usize) -> LayerOutput {
    let (doc, cs) = (ctx.doc, &ctx.cs);
    let l = &doc.layers[index];
    let mut out = LayerOutput::default();
    if l.kind == LayerKind::Adjustment {
        match adjust::parse(l, cs, doc.color_mode) {
            Ok(f) => out.adjust = Some(f),
            Err(e) => out.warnings.push(format!("{e}; skipped")),
        }
        out.mask = mask::region(doc, l, true);
        return out;
    }
    out.effects = layer_effects(doc, cs, l);
    let pad = out.effects.reach().ceil() as usize;
    let is_text = l.kind == LayerKind::Text && !ctx.options.keep_text_raster;
    let mut baked_vector = false;
    let soft_vector = l.vector_density < 1.0 || l.vector_feather > 0.0;
    let content = if is_text {
        text_raster(doc, cs, l, ctx.fonts, pad, &mut out.warnings)
            .unwrap_or_else(|()| pixel_raster(doc, cs, l).map(|r| r.padded(pad)))
    } else if l.kind == LayerKind::SmartObject {
        super::smart::render(ctx, l, pad, &mut out.warnings).or_else(|| pixel_raster(doc, cs, l)).map(|r| r.padded(pad))
    } else if l.kind == LayerKind::Fill && (!has_pixels(l) || soft_vector) {
        // Stored pixels hold the shape at full density, so a soft vector mask redraws the fill.
        baked_vector = !soft_vector;
        shape_raster(doc, cs, l, baked_vector).map(|r| r.padded(pad))
    } else {
        pixel_raster(doc, cs, l).map(|r| r.padded(pad))
    };
    let Some(content) = content else { return out };
    out.mask =
        mask::region(doc, l, !baked_vector && !is_text && (l.kind != LayerKind::Fill || !has_pixels(l) || soft_vector));
    out.coverage = coverage(&content, &out.mask);
    if !out.effects.is_empty() {
        let b = &l.bounds;
        let path_box = l
            .block(b"vmsk")
            .or(l.block(b"vsms"))
            .and_then(|b| vector::parse(b, doc.width, doc.height))
            .filter(|m| !m.disabled && !m.invert)
            .and_then(|m| m.bounds());
        // Aligned gradients and patterns span a shape's path, else the layer's pixels.
        let pixel_box = if b.width() > 0 {
            [b.left as f64, b.top as f64, b.right as f64, b.bottom as f64]
        } else {
            let pad = pad as f64;
            [
                content.x as f64 + pad,
                content.y as f64 + pad,
                (content.x + content.w as i32) as f64 - pad,
                (content.y + content.h as i32) as f64 - pad,
            ]
        };
        // Strokes lay theirs out on the layer's pixels.
        let bounds = (path_box.unwrap_or(pixel_box), pixel_box);
        let fill = l.fill_opacity as f32 / 255.0;
        let rect = (content.x, content.y, content.w, content.h);
        let path = (!out.effects.strokes.is_empty()).then(|| path_coverage(doc, l, &content, &out.mask)).flatten();
        out.prepared = out.effects.prepare(doc, cs, &out.coverage, path.as_deref(), rect, fill, bounds);
    }
    out.text_mask = (is_text && ctx.options.text_masks).then(|| TextMask {
        layer: index,
        name: l.name.clone(),
        x: content.x,
        y: content.y,
        width: content.w,
        height: content.h,
        alpha: content.px.chunks_exact(4).map(|p| (p[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8).collect(),
    });
    out.content = Some(content);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_color_modes() {
        let mut doc = Document::blank(2, 1);
        let cs = ColorSpace::default();
        let mut out = [0f32; 8];
        doc.color_mode = ColorMode::Grayscale;
        decode_row(&doc, &cs, [Some(&[0, 255]), None, None, None], Some(&[255, 0]), 2, &mut out);
        assert_eq!(out, [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        doc.color_mode = ColorMode::Cmyk;
        let none = [255u8, 255];
        decode_row(&doc, &cs, [Some(&[0, 255]), Some(&none), Some(&none), Some(&none)], None, 2, &mut out);
        assert_eq!(out, [0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        doc.color_mode = ColorMode::Indexed;
        doc.palette = vec![[255, 0, 0], [0, 0, 255]];
        doc.transparent_index = Some(1);
        decode_row(&doc, &cs, [Some(&[0, 1]), None, None, None], None, 2, &mut out);
        assert_eq!(out, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        doc.color_mode = ColorMode::Lab;
        decode_row(&doc, &cs, [Some(&[255, 0]), Some(&[128, 128]), Some(&[128, 128]), None], None, 2, &mut out);
        assert!(out[0] > 0.99 && out[4] < 0.01);
    }
}
