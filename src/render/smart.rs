//! Smart objects: re-rendered from their embedded file through the placement transform and warp.

use std::io::Cursor;

use rayon::prelude::*;

use super::canvas::Raster;
use super::filters::Stack;
use super::{Ctx, RenderOptions};
use crate::psd::descriptor::{Descriptor, Value};
use crate::psd::Document;
use crate::psd::Layer;
use crate::text::warp::{Envelope, Warp};

/// Rows per work unit when drawing.
const BAND: usize = 32;
/// Output pixels per mesh cell edge for warped objects.
const CELL: f64 = 6.0;

/// A premultiplied RGBA image.
struct Source {
    w: usize,
    h: usize,
    px: Vec<f32>,
}

impl Source {
    fn from_rgba8(w: usize, h: usize, rgba: &[u8]) -> Source {
        let px = rgba
            .chunks_exact(4)
            .flat_map(|p| {
                let a = p[3] as f32 / 255.0;
                [p[0] as f32 / 255.0 * a, p[1] as f32 / 255.0 * a, p[2] as f32 / 255.0 * a, a]
            })
            .collect();
        Source { w, h, px }
    }

    /// Half size, averaging 2x2 blocks.
    fn halve(&self) -> Source {
        let (w, h) = (self.w.div_ceil(2), self.h.div_ceil(2));
        let mut px = vec![0f32; w * h * 4];
        px.par_chunks_mut(w * 4).enumerate().for_each(|(y, out)| {
            let rows = [2 * y, (2 * y + 1).min(self.h - 1)];
            for x in 0..w {
                let cols = [2 * x, (2 * x + 1).min(self.w - 1)];
                for c in 0..4 {
                    let s: f32 = rows.iter().flat_map(|&r| cols.map(|k| self.px[(r * self.w + k) * 4 + c])).sum();
                    out[x * 4 + c] = s / 4.0;
                }
            }
        });
        Source { w, h, px }
    }

    /// Bilinear sample at `(x, y)` in pixel units, clamped to the edge pixels.
    fn sample(&self, x: f64, y: f64) -> [f32; 4] {
        let x = (x - 0.5).clamp(0.0, (self.w - 1) as f64);
        let y = (y - 0.5).clamp(0.0, (self.h - 1) as f64);
        let (x0, y0) = (x.floor(), y.floor());
        // Snap near-integer positions so unscaled placements copy pixels exactly.
        let snap = |f: f64| {
            if f < 1e-6 {
                0.0
            } else if f > 1.0 - 1e-6 {
                1.0
            } else {
                f as f32
            }
        };
        let (fx, fy) = (snap(x - x0), snap(y - y0));
        let (x0, y0) = (x0 as i64, y0 as i64);
        let at = |i: i64, j: i64| -> [f32; 4] {
            let (i, j) = (i.min(self.w as i64 - 1) as usize, j.min(self.h as i64 - 1) as usize);
            let k = (j * self.w + i) * 4;
            [self.px[k], self.px[k + 1], self.px[k + 2], self.px[k + 3]]
        };
        let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (d[i] - c[i]) * fx;
            top + (bottom - top) * fy
        })
    }
}

/// Mip levels of a source, level `k` being `2^k` times smaller.
struct Pyramid(Vec<Source>);

impl Pyramid {
    fn new(src: Source) -> Pyramid {
        let mut levels = vec![src];
        while let Some(last) = levels.last().filter(|s| s.w > 1 || s.h > 1) {
            let next = last.halve();
            levels.push(next);
        }
        Pyramid(levels)
    }

    /// Trilinear sample at level-0 position `(x, y)` for a footprint of `lod` (log2 source pixels
    /// per output pixel).
    fn sample(&self, x: f64, y: f64, lod: f64) -> [f32; 4] {
        let lod = lod.clamp(0.0, (self.0.len() - 1) as f64);
        let k = lod.floor() as usize;
        let at = |k: usize| {
            let s = (1u64 << k) as f64;
            self.0[k].sample(x / s, y / s)
        };
        let a = at(k);
        let f = (lod - k as f64) as f32;
        if f < 1e-3 || k + 1 >= self.0.len() {
            return a;
        }
        let b = at(k + 1);
        std::array::from_fn(|i| a[i] + (b[i] - a[i]) * f)
    }
}

/// A projective map as a row-major 3x3 matrix.
#[derive(Clone, Copy)]
struct Projective([f64; 9]);

impl Projective {
    /// The map taking rectangle `r` onto quad `q` (corners clockwise from top left).
    fn rect_to_quad(r: [f64; 4], q: [(f64, f64); 4]) -> Option<Projective> {
        let [(x0, y0), (x1, y1), (x2, y2), (x3, y3)] = q;
        let (sx, sy) = (x0 - x1 + x2 - x3, y0 - y1 + y2 - y3);
        let (a, b, c, d, e, f, g, h);
        if sx.abs() < 1e-9 && sy.abs() < 1e-9 {
            (a, b, c, d, e, f, g, h) = (x1 - x0, x2 - x1, x0, y1 - y0, y2 - y1, y0, 0.0, 0.0);
        } else {
            let (dx1, dx2, dy1, dy2) = (x1 - x2, x3 - x2, y1 - y2, y3 - y2);
            let den = dx1 * dy2 - dx2 * dy1;
            if den.abs() < 1e-12 {
                return None;
            }
            g = (sx * dy2 - dx2 * sy) / den;
            h = (dx1 * sy - sx * dy1) / den;
            (a, b, c) = (x1 - x0 + g * x1, x3 - x0 + h * x3, x0);
            (d, e, f) = (y1 - y0 + g * y1, y3 - y0 + h * y3, y0);
        }
        let square = Projective([a, b, c, d, e, f, g, h, 1.0]);
        let (w, ht) = (r[2] - r[0], r[3] - r[1]);
        if w.abs() < 1e-12 || ht.abs() < 1e-12 {
            return None;
        }
        let unit = Projective([1.0 / w, 0.0, -r[0] / w, 0.0, 1.0 / ht, -r[1] / ht, 0.0, 0.0, 1.0]);
        Some(square.then(&unit))
    }

    /// `self ∘ other`.
    fn then(&self, other: &Projective) -> Projective {
        let (a, b) = (&self.0, &other.0);
        Projective(std::array::from_fn(|k| {
            let (i, j) = (k / 3, k % 3);
            (0..3).map(|m| a[i * 3 + m] * b[m * 3 + j]).sum()
        }))
    }

    fn inverse(&self) -> Option<Projective> {
        let m = &self.0;
        let cof = [
            m[4] * m[8] - m[5] * m[7],
            m[2] * m[7] - m[1] * m[8],
            m[1] * m[5] - m[2] * m[4],
            m[5] * m[6] - m[3] * m[8],
            m[0] * m[8] - m[2] * m[6],
            m[2] * m[3] - m[0] * m[5],
            m[3] * m[7] - m[4] * m[6],
            m[1] * m[6] - m[0] * m[7],
            m[0] * m[4] - m[1] * m[3],
        ];
        let det = m[0] * cof[0] + m[1] * cof[3] + m[2] * cof[6];
        (det.abs() > 1e-18).then(|| Projective(cof.map(|c| c / det)))
    }

    fn apply(&self, (x, y): (f64, f64)) -> (f64, f64) {
        let m = &self.0;
        let w = m[6] * x + m[7] * y + m[8];
        ((m[0] * x + m[1] * y + m[2]) / w, (m[3] * x + m[4] * y + m[5]) / w)
    }
}

fn numbers(v: &[Value]) -> Vec<f64> {
    v.iter()
        .filter_map(|v| match v {
            Value::Number(n) | Value::Unit(_, n) => Some(*n),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        })
        .collect()
}

fn rect_of(d: &Descriptor) -> Option<[f64; 4]> {
    let get = |k| d.num(k);
    let r = [get("Left")?, get("Top ")?, get("Rght")?, get("Btom")?];
    (r[2] > r[0] && r[3] > r[1]).then_some(r)
}

/// Renders the embedded source of smart object `l`, clipped to the canvas grown by `margin`, or
/// `None` to use its cached pixels.
pub(crate) fn render(ctx: &Ctx, l: &Layer, margin: usize, warnings: &mut Vec<String>) -> Option<Raster> {
    let doc = ctx.doc;
    let placed = l.placed()?;
    let id = placed.text("Idnt")?;
    let edited = doc.edited.get(id);
    if edited.is_none() && !ctx.options.render_smart_objects {
        return None;
    }
    let filters = placed.desc("filterFX").map(Stack::parse);
    if let Some((_, unsupported)) = &filters {
        if !unsupported.is_empty() {
            let names = unsupported.join(", ");
            // The cached pixels carry the whole stack; prefer them unless the contents changed.
            if edited.is_none() {
                warnings.push(format!("smart filter not supported ({names}); using the cached pixels"));
                return None;
            }
            warnings.push(format!("smart filter not supported ({names}); skipped"));
        }
        if l.block(b"FMsk").is_some() && placed.desc("filterFX").and_then(|f| f.bool("filterMaskEnable")) != Some(false)
        {
            warnings.push("smart filter masks are not supported; filters apply everywhere".into());
        }
    }
    let filters = filters.map(|f| f.0).filter(|f| !f.is_empty());
    let src = match edited {
        Some(d) => from_document(d, ctx, warnings),
        None => match doc.linked.get(id) {
            Some(data) => decode(data, doc.base_dir.clone(), ctx, warnings),
            None => match doc.read_external(id) {
                Ok((data, dir)) => decode(&data, Some(dir), ctx, warnings),
                Err(e) => {
                    warnings.push(format!("{e}; using the cached pixels"));
                    return None;
                }
            },
        },
    }?;
    if src.w == 0 || src.h == 0 {
        return None;
    }
    let corners = placed.list("nonAffineTransform").or(placed.list("Trnf")).map(numbers).filter(|c| c.len() == 8)?;
    let quad = [(corners[0], corners[1]), (corners[2], corners[3]), (corners[4], corners[5]), (corners[6], corners[7])];
    let warp_desc = placed.desc("warp");
    let warp = warp_desc.map(Warp::from_descriptor).unwrap_or(Warp::NONE);
    let size = placed.desc("Sz  ").and_then(|s| Some((s.num("Wdth")?, s.num("Hght")?)));
    let (sw, sh) = size.filter(|s| s.0 > 0.0 && s.1 > 0.0).unwrap_or((src.w as f64, src.h as f64));
    let rect = warp_desc.and_then(|d| d.desc("bounds")).and_then(rect_of).unwrap_or([0.0, 0.0, sw, sh]);
    let env = (!warp.is_identity()).then(|| Envelope::new(&warp, rect));
    let rect = env.as_ref().map_or(rect, Envelope::rect);
    let m = Projective::rect_to_quad(rect, quad)?;
    let inv = m.inverse()?;
    let reach = filters.as_ref().map_or(0, Stack::reach);
    let grow = (margin + reach) as f64;
    let clip = [-grow, -grow, doc.width as f64 + grow, doc.height as f64 + grow];
    let drawn = draw(&Pyramid::new(src), rect, env.as_ref(), m, inv, clip);
    let Some(filters) = filters else { return Some(drawn) };
    // Filters see the whole area they reach into, then the result is trimmed back.
    let (w, h) = (doc.width as usize, doc.height as usize);
    let (m, r) = (margin as i32, reach as i32);
    let area = if filters.needs_canvas() {
        (-m - r, -m - r, w as i32 + m + r, h as i32 + m + r)
    } else {
        (drawn.x - r, drawn.y - r, drawn.x + drawn.w as i32 + r, drawn.y + drawn.h as i32 + r)
    };
    let full = drawn.crop(area.0, area.1, (area.2 - area.0) as usize, (area.3 - area.1) as usize);
    let out = filters.apply(full, (w, h));
    let (x0, y0) = (out.x.max(-m), out.y.max(-m));
    let (x1, y1) = ((out.x + out.w as i32).min(w as i32 + m), (out.y + out.h as i32).min(h as i32 + m));
    if x1 <= x0 || y1 <= y0 {
        return Some(Raster::new(0, 0, 0, 0));
    }
    Some(out.crop(x0, y0, (x1 - x0) as usize, (y1 - y0) as usize))
}

/// Draws the source, spanning `rect` before the warp `env` and map `m`, into the doc rectangle `clip`.
fn draw(
    src: &Pyramid,
    rect: [f64; 4],
    env: Option<&Envelope>,
    m: Projective,
    inv: Projective,
    clip: [f64; 4],
) -> Raster {
    let base = &src.0[0];
    let (bw, bh) = (base.w as f64, base.h as f64);
    let (kx, ky) = (bw / (rect[2] - rect[0]), bh / (rect[3] - rect[1]));
    let c = [(rect[0], rect[1]), (rect[2], rect[1]), (rect[2], rect[3]), (rect[0], rect[3])].map(|p| m.apply(p));
    let len = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1);
    let ex = len(c[0], c[1]).min(len(c[3], c[2])).max(1e-3);
    let ey = len(c[0], c[3]).min(len(c[1], c[2])).max(1e-3);
    // Reach a pixel past the edges, in source pixels, so edge pixels get their partial coverage.
    let (mx, my) = ((bw / ex).max(1.0) / kx, (bh / ey).max(1.0) / ky);
    let span = [rect[0] - mx, rect[1] - my, rect[2] + mx, rect[3] + my];
    let (nx, ny) = match env {
        None => (1, 1),
        Some(_) => {
            let (ex, ey) = (len(c[0], c[1]).max(len(c[3], c[2])), len(c[0], c[3]).max(len(c[1], c[2])));
            ((ex / CELL).ceil().clamp(8.0, 256.0) as usize, (ey / CELL).ceil().clamp(8.0, 256.0) as usize)
        }
    };
    // Mesh vertices: position after the warp (before `m`) and in source pixels.
    let mut verts = Vec::with_capacity((nx + 1) * (ny + 1));
    for j in 0..=ny {
        for i in 0..=nx {
            let x = span[0] + (span[2] - span[0]) * i as f64 / nx as f64;
            let y = span[1] + (span[3] - span[1]) * j as f64 / ny as f64;
            let q = env.map_or((x, y), |e| e.eval(x, y));
            verts.push((q, ((x - rect[0]) * kx, (y - rect[1]) * ky)));
        }
    }
    struct Tri {
        q: [(f64, f64); 3],
        s: [(f64, f64); 3],
        bounds: [f64; 4],
        lod: f64,
        /// Source pixels per output pixel across lines of constant x and of constant y.
        grad: (f64, f64),
    }
    let mut tris = Vec::with_capacity(nx * ny * 2);
    for j in 0..ny {
        for i in 0..nx {
            let k = j * (nx + 1) + i;
            for idx in [[k, k + 1, k + nx + 2], [k, k + nx + 2, k + nx + 1]] {
                let (q, s) = (idx.map(|v| verts[v].0), idx.map(|v| verts[v].1));
                let o = q.map(|p| m.apply(p));
                let bounds = [
                    o.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
                    o.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
                    o.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
                    o.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
                ];
                if !bounds.iter().all(|v| v.is_finite()) {
                    continue;
                }
                // Source pixels per output pixel, from the areas of the two triangles.
                let area = |p: [(f64, f64); 3]| {
                    ((p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1)).abs()
                };
                let (ao, as_) = (area(o), area(s));
                let lod = if ao > 1e-12 { 0.5 * (as_ / ao).log2() } else { 0.0 };
                // Gradients of the source coordinates over the output triangle.
                let (e1, e2) = ((o[1].0 - o[0].0, o[1].1 - o[0].1), (o[2].0 - o[0].0, o[2].1 - o[0].1));
                let det = e1.0 * e2.1 - e2.0 * e1.1;
                let grad = |d1: f64, d2: f64| {
                    if det.abs() < 1e-12 {
                        return f64::INFINITY;
                    }
                    let (gx, gy) = ((d1 * e2.1 - d2 * e1.1) / det, (d2 * e1.0 - d1 * e2.0) / det);
                    gx.hypot(gy).max(1e-9)
                };
                let grad = (grad(s[1].0 - s[0].0, s[2].0 - s[0].0), grad(s[1].1 - s[0].1, s[2].1 - s[0].1));
                tris.push(Tri { q, s, bounds, lod, grad });
            }
        }
    }
    let x0 = tris.iter().map(|t| t.bounds[0]).fold(f64::INFINITY, f64::min).max(clip[0]).floor() as i32;
    let y0 = tris.iter().map(|t| t.bounds[1]).fold(f64::INFINITY, f64::min).max(clip[1]).floor() as i32;
    let x1 = tris.iter().map(|t| t.bounds[2]).fold(f64::NEG_INFINITY, f64::max).min(clip[2]).ceil() as i32;
    let y1 = tris.iter().map(|t| t.bounds[3]).fold(f64::NEG_INFINITY, f64::max).min(clip[3]).ceil() as i32;
    if x1 <= x0 || y1 <= y0 {
        return Raster::new(0, 0, 0, 0);
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let bands = h.div_ceil(BAND);
    let mut lists = vec![vec![]; bands];
    for (t, tri) in tris.iter().enumerate() {
        let first = ((tri.bounds[1] - y0 as f64).floor().max(0.0) as usize / BAND).min(bands - 1);
        let last = ((tri.bounds[3] - y0 as f64).ceil().max(0.0) as usize / BAND).min(bands - 1);
        for list in &mut lists[first..=last] {
            list.push(t);
        }
    }
    let mut out = Raster::new(x0, y0, w, h);
    out.px.par_chunks_mut(w * BAND * 4).zip(lists).enumerate().for_each(|(b, (rows, list))| {
        let mut done = vec![false; rows.len() / 4];
        for &t in &list {
            let tri = &tris[t];
            let [(ax, ay), (bx, by), (cx, cy)] = tri.q;
            let den = (by - cy) * (ax - cx) + (cx - bx) * (ay - cy);
            if den.abs() < 1e-18 {
                continue;
            }
            let lo_y = ((tri.bounds[1] - y0 as f64).floor() as i64).max((b * BAND) as i64);
            let hi_y = ((tri.bounds[3] - y0 as f64).ceil() as i64).min((b * BAND + rows.len() / (w * 4)) as i64);
            let lo_x = ((tri.bounds[0] - x0 as f64).floor() as i64).max(0);
            let hi_x = ((tri.bounds[2] - x0 as f64).ceil() as i64).min(w as i64);
            for y in lo_y..hi_y {
                let r = y as usize - b * BAND;
                for x in lo_x..hi_x {
                    let i = r * w + x as usize;
                    if done[i] {
                        continue;
                    }
                    let (qx, qy) = inv.apply((x0 as f64 + x as f64 + 0.5, y0 as f64 + y as f64 + 0.5));
                    let l1 = ((by - cy) * (qx - cx) + (cx - bx) * (qy - cy)) / den;
                    let l2 = ((cy - ay) * (qx - cx) + (ax - cx) * (qy - cy)) / den;
                    let l3 = 1.0 - l1 - l2;
                    const EPS: f64 = -1e-9;
                    if l1 < EPS || l2 < EPS || l3 < EPS {
                        continue;
                    }
                    let [s1, s2, s3] = tri.s;
                    let sx = l1 * s1.0 + l2 * s2.0 + l3 * s3.0;
                    let sy = l1 * s1.1 + l2 * s2.1 + l3 * s3.1;
                    // Coverage from the distance to the source edges, in output pixels.
                    let edge = |v: f64, size: f64, g: f64| (v.min(size - v) / g + 0.5).clamp(0.0, 1.0) as f32;
                    let k = edge(sx, bw, tri.grad.0) * edge(sy, bh, tri.grad.1);
                    if k > 0.0 {
                        let p = src.sample(sx, sy, tri.lod);
                        rows[i * 4..i * 4 + 4].copy_from_slice(&p.map(|v| v * k));
                    }
                    done[i] = true;
                }
            }
        }
    });
    out
}

/// Renders an embedded document.
fn from_document(d: &Document, ctx: &Ctx, warnings: &mut Vec<String>) -> Option<Source> {
    let options = RenderOptions { text_masks: false, ..ctx.options.clone() };
    let out = super::render(d, ctx.fonts, &options);
    warnings.extend(out.warnings.into_iter().map(|w| format!("in smart object: {w}")));
    Some(Source::from_rgba8(out.image.width as usize, out.image.height as usize, &out.image.data))
}

/// Decodes an embedded or linked PSD, PSB, PNG or JPEG file; a linked PSD resolves its own links
/// against `dir`.
fn decode(data: &[u8], dir: Option<std::path::PathBuf>, ctx: &Ctx, warnings: &mut Vec<String>) -> Option<Source> {
    let result = if data.starts_with(b"8BPS") {
        match Document::parse(data) {
            Ok(mut d) => {
                if let Some(dir) = dir {
                    d.set_base_dir(dir);
                }
                return from_document(&d, ctx, warnings);
            }
            Err(e) => Err(e.to_string()),
        }
    } else if data.starts_with(b"\x89PNG") {
        decode_png(data)
    } else if data.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(data)
    } else {
        Err("unsupported file type".into())
    };
    match result {
        Ok(s) => Some(s),
        Err(e) => {
            warnings.push(format!("smart object contents could not be read ({e}); using the cached pixels"));
            None
        }
    }
}

fn decode_png(data: &[u8]) -> Result<Source, String> {
    let mut dec = png::Decoder::new(Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    let buf = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf.to_vec(),
        png::ColorType::Rgb => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("unexpanded palette".into()),
    };
    if rgba.len() != w * h * 4 {
        return Err("truncated image".into());
    }
    Ok(Source::from_rgba8(w, h, &rgba))
}

fn decode_jpeg(data: &[u8]) -> Result<Source, String> {
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(data));
    let px = dec.decode().map_err(|e| e.to_string())?;
    let info = dec.info().ok_or("no image")?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba: Vec<u8> = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => px.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        jpeg_decoder::PixelFormat::L8 => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        jpeg_decoder::PixelFormat::L16 => px.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], 255]).collect(),
        // Photoshop writes CMYK JPEGs inverted.
        jpeg_decoder::PixelFormat::CMYK32 => px
            .chunks_exact(4)
            .flat_map(|p| {
                let k = p[3] as u32;
                [p[0], p[1], p[2]].map(|c| (c as u32 * k / 255) as u8).into_iter().chain([255])
            })
            .collect(),
    };
    if rgba.len() != w * h * 4 {
        return Err("truncated image".into());
    }
    Ok(Source::from_rgba8(w, h, &rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_to_quad_maps_corners_and_inverts() {
        let quad = [(10.0, 20.0), (110.0, 30.0), (100.0, 90.0), (5.0, 70.0)];
        let m = Projective::rect_to_quad([0.0, 0.0, 50.0, 40.0], quad).unwrap();
        for (p, q) in [(0.0, 0.0), (50.0, 0.0), (50.0, 40.0), (0.0, 40.0)].into_iter().zip(quad) {
            let r = m.apply(p);
            assert!((r.0 - q.0).abs() < 1e-9 && (r.1 - q.1).abs() < 1e-9, "{p:?} -> {r:?}");
        }
        let back = m.inverse().unwrap().apply(m.apply((12.0, 31.0)));
        assert!((back.0 - 12.0).abs() < 1e-9 && (back.1 - 31.0).abs() < 1e-9);
    }

    fn checker(w: usize, h: usize) -> Source {
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| if (i % w + i / w) % 2 == 0 { [255, 0, 0, 255] } else { [0, 0, 255, 255] })
            .collect();
        Source::from_rgba8(w, h, &rgba)
    }

    #[test]
    fn identity_placement_copies_pixels() {
        let src = checker(4, 3);
        let expect = src.px.clone();
        let rect = [0.0, 0.0, 4.0, 3.0];
        let quad = [(5.0, 6.0), (9.0, 6.0), (9.0, 9.0), (5.0, 9.0)];
        let m = Projective::rect_to_quad(rect, quad).unwrap();
        let r = draw(&Pyramid::new(src), rect, None, m, m.inverse().unwrap(), [0.0, 0.0, 20.0, 20.0]);
        for y in 0..3 {
            for x in 0..4 {
                let i = ((y + 6 - r.y as usize) * r.w + x + 5 - r.x as usize) * 4;
                let j = (y * 4 + x) * 4;
                assert_eq!(&r.px[i..i + 4], &expect[j..j + 4], "({x}, {y})");
            }
        }
    }

    #[test]
    fn downscaling_averages() {
        let src = checker(64, 64);
        let rect = [0.0, 0.0, 64.0, 64.0];
        let quad = [(0.0, 0.0), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)];
        let m = Projective::rect_to_quad(rect, quad).unwrap();
        let r = draw(&Pyramid::new(src), rect, None, m, m.inverse().unwrap(), [0.0, 0.0, 8.0, 8.0]);
        let i = (4 * r.w + 4) * 4;
        assert!((r.px[i] - 0.5).abs() < 0.05 && (r.px[i + 2] - 0.5).abs() < 0.05, "{:?}", &r.px[i..i + 4]);
    }
}
