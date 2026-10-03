//! What writing a document back needs rendered: edited layers, edited embedded files and the
//! composite image.

use super::canvas::Raster;
use super::{Ctx, RenderOptions, Warning};
use crate::color::ColorSpace;
use crate::fonts::FontDb;
use crate::psd::write::{Edits, Pixels};
use crate::psd::{Document, LayerKind, Rect};

/// Straight 8-bit pixels of a premultiplied raster, cropped to its opaque part.
fn pixels_of(r: &Raster) -> Pixels {
    let (mut x0, mut y0, mut x1, mut y1) = (r.w, r.h, 0, 0);
    for y in 0..r.h {
        for x in 0..r.w {
            if r.px[(y * r.w + x) * 4 + 3] >= 0.5 / 255.0 {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
    }
    if x1 <= x0 {
        return Pixels { rect: Rect::default(), rgba: vec![] };
    }
    let mut rgba = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
    for y in y0..y1 {
        for x in x0..x1 {
            let p = &r.px[(y * r.w + x) * 4..][..4];
            let a = p[3].clamp(0.0, 1.0);
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            let c = |v: f32| if a > 0.0 { q(v / a) } else { 0 };
            rgba.extend([c(p[0]), c(p[1]), c(p[2]), q(a)]);
        }
    }
    let rect = Rect { left: r.x + x0 as i32, top: r.y + y0 as i32, right: r.x + x1 as i32, bottom: r.y + y1 as i32 };
    Pixels { rect, rgba }
}

/// What writing `doc` back replaces: pixels of edited layers, edited embedded files and the
/// composite image, with the warnings from rendering them.
pub(crate) fn psd_edits(
    doc: &Document,
    fonts: &FontDb,
    options: &RenderOptions,
) -> crate::Result<(Edits, Vec<Warning>)> {
    let ctx = Ctx { doc, fonts, options, cs: ColorSpace::new(doc) };
    let mut edits = Edits::default();
    let mut warnings = vec![];
    for (id, sub) in &doc.edited {
        if let Some(original) = doc.linked.get(id) {
            let (data, w) = sub.to_psd(original, fonts, options)?;
            edits.linked.insert(id.clone(), data);
            warnings.extend(w.into_iter().map(|w| Warning { message: format!("in smart object: {w}"), ..w }));
        }
    }
    for (i, l) in doc.layers.iter().enumerate() {
        let mut w = vec![];
        let raster = if l.edited && l.kind == LayerKind::Text {
            super::layer::text_raster(doc, &ctx.cs, l, fonts, 0, &mut w).ok()
        } else if l.kind == LayerKind::SmartObject && l.smart_id().is_some_and(|id| doc.edited.contains_key(&id)) {
            Some(super::smart::render(&ctx, l, 0, &mut w))
        } else {
            None
        };
        warnings.extend(w.into_iter().map(|message| Warning { layer: Some(l.name.clone()), message }));
        if let Some(mut r) = raster {
            if let Some(r) = r.as_mut() {
                ctx.cs.encode_linear(&mut r.px);
            }
            let px = r.as_ref().map_or_else(|| Pixels { rect: Rect::default(), rgba: vec![] }, pixels_of);
            edits.layers.insert(i, px);
        }
    }
    let out = super::render_inner(doc, fonts, options, false);
    warnings.extend(out.warnings);
    edits.composite = Some(out.image.data);
    Ok((edits, warnings))
}
