//! Rasterizes one leaf layer into paint operations: effects below, content, inner effects above.

use rayon::prelude::*;

use super::canvas::{prefault, Alpha, Paint, Raster};
use super::distance::{blur, distances};
use super::effects::{self, Effects, StrokePosition, SIGMA_PER_SIZE};
use super::TextMask;
use crate::blend::BlendMode;
use crate::fonts::FontDb;
use crate::psd::descriptor;
use crate::psd::{ColorMode, Document, Layer, LayerKind};
use crate::text::path::{self, Seg};
use crate::text::{layout, AntiAlias, TextLayer};

/// Largest text raster, in pixels, before a layer is skipped as runaway.
const MAX_TEXT_PIXELS: u64 = 40_000_000;

pub(crate) struct LayerOutput {
    pub paints: Vec<Paint>,
    pub base: Option<Alpha>,
    pub warnings: Vec<String>,
    pub text_mask: Option<TextMask>,
}

impl LayerOutput {
    fn empty(warnings: Vec<String>) -> Self {
        LayerOutput { paints: vec![], base: None, warnings, text_mask: None }
    }
}

pub(crate) fn pixel_raster(doc: &Document, l: &Layer) -> Option<Raster> {
    let (w, h) = (l.bounds.width(), l.bounds.height());
    if w == 0 || h == 0 {
        return None;
    }
    let n = w * h;
    let ch = |id: i16| l.channels.get(&id).filter(|c| c.len() >= n).map(Vec::as_slice);
    let alpha = ch(-1);
    let mode = doc.color_mode;
    let channels = match mode {
        ColorMode::Grayscale | ColorMode::Bitmap => [ch(0), None, None, None],
        ColorMode::Cmyk => [ch(0), ch(1), ch(2), ch(3)],
        _ => [ch(0), ch(1), ch(2), None],
    };
    let mut px = vec![0f32; n * 4];
    px.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let get = |c: Option<&[u8]>, x: usize, default: u8| c.map_or(default, |v| v[y * w + x]) as f32 / 255.0;
        for (x, p) in row.chunks_exact_mut(4).enumerate() {
            let a = get(alpha, x, 255);
            let rgb = match mode {
                ColorMode::Grayscale | ColorMode::Bitmap => [get(channels[0], x, 0); 3],
                ColorMode::Cmyk => {
                    let k = get(channels[3], x, 255);
                    [get(channels[0], x, 255) * k, get(channels[1], x, 255) * k, get(channels[2], x, 255) * k]
                }
                _ => [get(channels[0], x, 0), get(channels[1], x, 0), get(channels[2], x, 0)],
            };
            p.copy_from_slice(&[rgb[0] * a, rgb[1] * a, rgb[2] * a, a]);
        }
    });
    Some(Raster { x: l.bounds.left, y: l.bounds.top, w, h, px })
}

pub(crate) fn apply_mask(r: &mut Raster, l: &Layer) {
    let Some(m) = l.mask.as_ref().filter(|m| !m.disabled) else { return };
    for y in 0..r.h {
        for x in 0..r.w {
            let v = m.at(r.x + x as i32, r.y + y as i32) as f32 / 255.0;
            if v < 1.0 {
                let i = (y * r.w + x) * 4;
                r.px[i..i + 4].iter_mut().for_each(|c| *c *= v);
            }
        }
    }
}

fn text_raster(l: &Layer, fonts: &FontDb, pad: usize, warnings: &mut Vec<String>) -> Option<Raster> {
    let tl = match TextLayer::parse(l.type_data.as_deref()?) {
        Ok(t) => t,
        Err(e) => {
            warnings.push(format!("text could not be parsed ({e}); skipped"));
            return None;
        }
    };
    let laid = layout::layout(&tl, fonts);
    for f in &laid.missing_fonts {
        warnings.push(format!("font {f} not found; using a fallback"));
    }
    if tl.vertical {
        warnings.push("vertical text is drawn horizontally".into());
    }
    let t = tl.transform;
    let scale = (t[0] * t[3] - t[1] * t[2]).abs().sqrt();
    let glyphs: Vec<(path::Path, [f32; 4], f64)> = laid
        .glyphs
        .iter()
        .map(|g| {
            (
                path::map(&g.path, |x, y| (t[0] * x + t[2] * y + t[4], t[1] * x + t[3] * y + t[5])),
                g.color,
                g.bold * scale,
            )
        })
        .collect();
    let bold = glyphs.iter().map(|g| g.2).fold(0.0, f64::max);
    let [x0, y0, x1, y1] = path::bounds(glyphs.iter().map(|g| &g.0))?;
    let margin = pad as i32 + 1 + bold.ceil() as i32;
    let ix = x0.floor() as i32 - margin;
    let iy = y0.floor() as i32 - margin;
    let w = (x1.ceil() as i32 + margin - ix).max(1) as u32;
    let h = (y1.ceil() as i32 + margin - iy).max(1) as u32;
    if w as u64 * h as u64 > MAX_TEXT_PIXELS {
        warnings.push("text raster too large; skipped".into());
        return None;
    }
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    prefault(pixmap.data_mut());
    let (ox, oy) = (ix as f64, iy as f64);
    let pt = |x: f64, y: f64| ((x - ox) as f32, (y - oy) as f32);
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
        pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
        if *bold > 0.0 {
            let stroke =
                tiny_skia::Stroke { width: *bold as f32, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
            pixmap.stroke_path(&path, &paint, &stroke, tiny_skia::Transform::identity(), None);
        }
    }
    let mut px: Vec<f32> = pixmap.data().iter().map(|&v| v as f32 / 255.0).collect();
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
    Some(Raster { x: ix, y: iy, w: w as usize, h: h as usize, px })
}

fn layer_effects(doc: &Document, l: &Layer) -> Effects {
    l.effects_data
        .as_deref()
        .and_then(|b| descriptor::parse_block(b, 8).ok())
        .map(|d| effects::parse(&d, doc.global_angle))
        .unwrap_or_default()
}

fn shift(src: &[f32], w: usize, h: usize, dx: i32, dy: i32) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
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

/// Renders leaf layer `index` of `doc`.
pub(crate) fn render_layer(
    doc: &Document,
    index: usize,
    fonts: &FontDb,
    keep_text: bool,
    want_mask: bool,
) -> LayerOutput {
    let l = &doc.layers[index];
    let mut warnings = vec![];
    if l.kind == LayerKind::Adjustment {
        warnings.push("adjustment layers are not applied".into());
        return LayerOutput::empty(warnings);
    }
    let fx = layer_effects(doc, l);
    warnings.extend(fx.unsupported.iter().map(|u| format!("{u} effect is not supported")));
    let pad = fx.reach().ceil() as usize;
    let is_text = l.kind == LayerKind::Text && !keep_text;
    let content =
        if is_text { text_raster(l, fonts, pad, &mut warnings) } else { pixel_raster(doc, l).map(|r| r.padded(pad)) };
    let Some(mut content) = content else { return LayerOutput::empty(warnings) };
    apply_mask(&mut content, l);

    let text_mask = (is_text && want_mask).then(|| TextMask {
        layer: index,
        name: l.name.clone(),
        x: content.x,
        y: content.y,
        width: content.w,
        height: content.h,
        alpha: content.px.chunks_exact(4).map(|p| (p[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8).collect(),
    });

    let fill = l.fill_opacity as f32 / 255.0;
    let a = content.alpha();
    let (x, y, w, h) = (content.x, content.y, content.w, content.h);
    let dist = fx.needs_distances().then(|| distances(&a, w, h, fx.needs_inward(), fx.max_distance()));
    let grown = |spread: f64| match &dist {
        Some(d) if spread > 0.0 => effects::dilate(d, w, h, spread),
        _ => a.clone(),
    };
    let stroke = |s: &effects::Stroke| {
        let cov = effects::stroke_coverage(dist.as_ref().expect("strokes need distances"), w, h, s.size, s.position);
        Paint { raster: Raster::solid(x, y, w, h, &cov, s.color), mode: s.mode, opacity: s.opacity }
    };

    let mut paints = vec![];
    for s in &fx.shadows {
        let mut m = grown(s.spread);
        let soft = (s.size - s.spread).max(0.0);
        blur(&mut m, w, h, soft * SIGMA_PER_SIZE, soft);
        let angle = s.angle.to_radians();
        let (dx, dy) = ((-s.distance * angle.cos()).round() as i32, (s.distance * angle.sin()).round() as i32);
        let raster = if s.knocked_out {
            let mut out = shift(&m, w, h, dx, dy);
            out.iter_mut().zip(&a).for_each(|(v, &al)| *v *= 1.0 - al * fill.clamp(0.0, 1.0));
            Raster::solid(x, y, w, h, &out, s.color)
        } else {
            Raster::solid(x + dx, y + dy, w, h, &m, s.color)
        };
        paints.push(Paint { raster, mode: s.mode, opacity: s.opacity });
    }
    for g in &fx.glows {
        let mut m = grown(g.spread);
        let soft = (g.size - g.spread).max(0.0);
        blur(&mut m, w, h, soft * SIGMA_PER_SIZE, soft);
        paints.push(Paint { raster: Raster::solid(x, y, w, h, &m, g.color), mode: g.mode, opacity: g.opacity });
    }
    paints.extend(fx.strokes.iter().filter(|s| s.position == StrokePosition::Outside).map(stroke));
    for o in &fx.overlays {
        for (p, &al) in content.px.chunks_exact_mut(4).zip(&a) {
            for c in 0..3 {
                p[c] = p[c] * (1.0 - o.opacity) + o.color[c] * al * o.opacity;
            }
        }
    }
    let base = Alpha { x, y, w, h, a };
    let mode = if l.blend_mode == BlendMode::PassThrough { BlendMode::Normal } else { l.blend_mode };
    paints.push(Paint { raster: content, mode, opacity: fill });
    paints.extend(fx.strokes.iter().filter(|s| s.position != StrokePosition::Outside).map(stroke));
    LayerOutput { paints, base: Some(base), warnings, text_mask }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_moves_and_clips() {
        let src = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(shift(&src, 2, 2, 1, 0), [0.0, 1.0, 0.0, 3.0]);
        assert_eq!(shift(&src, 2, 2, 0, -1), [3.0, 4.0, 0.0, 0.0]);
        assert_eq!(shift(&src, 2, 2, 5, 5), [0.0; 4]);
    }
}
