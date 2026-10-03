//! Vector masks and shape strokes: path records from `vmsk`/`vsms` rasterized with tiny-skia.

use tiny_skia::{FillRule, LineCap, LineJoin, Mask, MaskType, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

use crate::psd::descriptor::{Descriptor, Value};
use crate::psd::reader::Reader;

/// A Bézier knot: preceding control point, anchor, leaving control point, in document pixels.
type Knot = [(f64, f64); 3];

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Subpath {
    pub closed: bool,
    /// Boolean operation with what came before: 0 exclude, 1 combine, 2 subtract, 3 intersect,
    /// -1 merged into the previous component.
    pub op: i16,
    pub knots: Vec<Knot>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct VectorMask {
    pub invert: bool,
    pub disabled: bool,
    /// The initial fill rule record: with no paths the mask reveals everything.
    pub fill_all: bool,
    pub paths: Vec<Subpath>,
}

fn fixed(r: &mut Reader) -> Option<f64> {
    Some(r.i32().ok()? as f64 / (1 << 24) as f64)
}

/// Parses a `vmsk`/`vsms` block for a `width x height` document.
pub(crate) fn parse(data: &[u8], width: u32, height: u32) -> Option<VectorMask> {
    let mut r = Reader::new(data);
    let _version = r.u32().ok()?;
    let flags = r.u32().ok()?;
    let mut m = VectorMask { invert: flags & 1 != 0, disabled: flags & 4 != 0, ..Default::default() };
    let (w, h) = (width as f64, height as f64);
    let mut remaining = 0usize;
    while r.remaining() >= 26 {
        let start = r.pos;
        let selector = r.u16().ok()?;
        match selector {
            0 | 3 => {
                remaining = r.u16().ok()? as usize;
                let op = r.i16().ok()?;
                m.paths.push(Subpath { closed: selector == 0, op, knots: vec![] });
            }
            1 | 2 | 4 | 5 if remaining > 0 => {
                let mut k = [(0.0, 0.0); 3];
                for p in &mut k {
                    let y = fixed(&mut r)?;
                    let x = fixed(&mut r)?;
                    *p = (x * w, y * h);
                }
                if let Some(s) = m.paths.last_mut() {
                    s.knots.push(k);
                }
                remaining -= 1;
            }
            8 => m.fill_all = r.u16().ok()? != 0,
            _ => {}
        }
        r.pos = start + 26;
    }
    Some(m)
}

fn build(subpaths: &[&Subpath], ox: f64, oy: f64) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let pt = |p: (f64, f64)| ((p.0 - ox) as f32, (p.1 - oy) as f32);
    for s in subpaths {
        let Some(first) = s.knots.first() else { continue };
        let (x, y) = pt(first[1]);
        pb.move_to(x, y);
        let n = s.knots.len();
        let segs = if s.closed { n } else { n - 1 };
        for i in 0..segs {
            let a = s.knots[i];
            let b = s.knots[(i + 1) % n];
            let ((c1x, c1y), (c2x, c2y), (x, y)) = (pt(a[2]), pt(b[0]), pt(b[1]));
            pb.cubic_to(c1x, c1y, c2x, c2y, x, y);
        }
        if s.closed {
            pb.close();
        }
    }
    pb.finish()
}

/// Groups subpaths into components: a subpath with operation -1 joins the previous one.
fn components(paths: &[Subpath]) -> Vec<Vec<&Subpath>> {
    let mut out: Vec<Vec<&Subpath>> = vec![];
    for s in paths.iter().filter(|s| s.knots.len() > 1) {
        match out.last_mut() {
            Some(last) if s.op == -1 => last.push(s),
            _ => out.push(vec![s]),
        }
    }
    out
}

fn coverage(path: Option<tiny_skia::Path>, w: usize, h: usize, rule: FillRule) -> Vec<f32> {
    let Some(path) = path else { return vec![0.0; w * h] };
    let Some(mut mask) = Mask::new(w as u32, h as u32) else { return vec![0.0; w * h] };
    mask.fill_path(&path, rule, true, Transform::identity());
    mask.data().iter().map(|&v| v as f32 / 255.0).collect()
}

impl VectorMask {
    /// Coverage of the mask over the document rectangle `(x, y, w, h)`.
    pub fn rasterize(&self, x: i32, y: i32, w: usize, h: usize) -> Vec<f32> {
        let comps = components(&self.paths);
        let start = if self.fill_all && comps.is_empty() { 1.0 } else { 0.0 };
        let mut m = vec![start; w * h];
        for (i, c) in comps.iter().enumerate() {
            let plane = coverage(build(c, x as f64, y as f64), w, h, FillRule::EvenOdd);
            let op = c[0].op;
            if i == 0 && (op == 2 || op == 3) {
                m.iter_mut().for_each(|v| *v = 1.0 - *v);
            }
            for (v, p) in m.iter_mut().zip(&plane) {
                *v = match op {
                    0 => *v + p - 2.0 * *v * p,
                    2 => (*v - p).max(0.0),
                    3 => *v * p,
                    _ => *v + p - *v * p,
                };
            }
        }
        if self.invert {
            m.iter_mut().for_each(|v| *v = 1.0 - *v);
        }
        m.iter_mut().for_each(|v| *v = v.clamp(0.0, 1.0));
        m
    }

    /// Bounding box of all anchors and control points.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        let pts = self.paths.iter().flat_map(|s| s.knots.iter().flatten());
        pts.fold(None, |b, &(x, y)| {
            Some(match b {
                None => [x, y, x, y],
                Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Align {
    Inside,
    Center,
    Outside,
}

/// A shape layer's stroke (`vstk`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ShapeStroke {
    pub width: f64,
    pub align: Align,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter: f64,
    pub dashes: Vec<f64>,
    pub dash_offset: f64,
    pub opacity: f32,
    pub fill_enabled: bool,
    pub content: Option<Descriptor>,
}

pub(crate) fn parse_stroke(d: &Descriptor) -> Option<ShapeStroke> {
    let fill_enabled = d.bool("fillEnabled").unwrap_or(true);
    if !d.bool("strokeEnabled").unwrap_or(true) {
        return Some(ShapeStroke {
            width: 0.0,
            align: Align::Center,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter: 100.0,
            dashes: vec![],
            dash_offset: 0.0,
            opacity: 0.0,
            fill_enabled,
            content: None,
        });
    }
    let width = d.num("strokeStyleLineWidth").unwrap_or(1.0);
    let dashes = d
        .list("strokeStyleLineDashSet")
        .unwrap_or(&[])
        .iter()
        .filter_map(|v| match v {
            Value::Number(n) | Value::Unit(_, n) => Some(n * width),
            _ => None,
        })
        .collect();
    Some(ShapeStroke {
        width,
        align: match d.enumerated("strokeStyleLineAlignment") {
            Some("strokeStyleAlignInside") => Align::Inside,
            Some("strokeStyleAlignOutside") => Align::Outside,
            _ => Align::Center,
        },
        cap: match d.enumerated("strokeStyleLineCapType") {
            Some("strokeStyleRoundCap") => LineCap::Round,
            Some("strokeStyleSquareCap") => LineCap::Square,
            _ => LineCap::Butt,
        },
        join: match d.enumerated("strokeStyleLineJoinType") {
            Some("strokeStyleRoundJoin") => LineJoin::Round,
            Some("strokeStyleBevelJoin") => LineJoin::Bevel,
            _ => LineJoin::Miter,
        },
        miter: d.num("strokeStyleMiterLimit").unwrap_or(100.0),
        dash_offset: d.num("strokeStyleLineDashOffset").unwrap_or(0.0) * width,
        dashes,
        opacity: (d.num("strokeStyleOpacity").unwrap_or(100.0) / 100.0) as f32,
        fill_enabled,
        content: d.desc("strokeStyleContent").cloned(),
    })
}

impl ShapeStroke {
    pub fn visible(&self) -> bool {
        self.width > 0.0 && self.opacity > 0.0
    }

    /// Coverage of the stroke over the document rectangle `(x, y, w, h)`.
    pub fn rasterize(&self, mask: &VectorMask, x: i32, y: i32, w: usize, h: usize) -> Vec<f32> {
        let sided = self.align != Align::Center;
        let pen = if sided { 2.0 * self.width } else { self.width };
        let mut out = vec![0f32; w * h];
        let Some(mut pixmap) = Pixmap::new(w as u32, h as u32) else { return out };
        let mut paint = Paint::default();
        paint.set_color_rgba8(0, 0, 0, 255);
        let mut stroke = Stroke {
            width: pen as f32,
            line_cap: self.cap,
            line_join: self.join,
            miter_limit: self.miter as f32,
            ..Default::default()
        };
        if self.dashes.len() >= 2 {
            let mut d: Vec<f32> = self.dashes.iter().map(|&v| v.max(0.01) as f32).collect();
            if d.len() % 2 == 1 {
                d.extend(d.clone());
            }
            stroke.dash = StrokeDash::new(d, self.dash_offset as f32);
        }
        for c in components(&mask.paths) {
            if let Some(path) = build(&c, x as f64, y as f64) {
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        let line = Mask::from_pixmap(pixmap.as_ref(), MaskType::Alpha);
        for (o, &v) in out.iter_mut().zip(line.data()) {
            *o = v as f32 / 255.0;
        }
        if sided {
            let fill = mask.rasterize(x, y, w, h);
            for (o, f) in out.iter_mut().zip(fill) {
                *o *= if self.align == Align::Inside { f } else { 1.0 - f };
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(selector: u16, body: &[u8]) -> Vec<u8> {
        let mut v = selector.to_be_bytes().to_vec();
        v.extend_from_slice(body);
        v.resize(26, 0);
        v
    }

    fn knot(points: [(f64, f64); 3]) -> Vec<u8> {
        let mut b = vec![];
        for (x, y) in points {
            b.extend(((y * (1 << 24) as f64) as i32).to_be_bytes());
            b.extend(((x * (1 << 24) as f64) as i32).to_be_bytes());
        }
        record(1, &b)
    }

    fn square(x0: f64, y0: f64, x1: f64, y1: f64, op: i16) -> Vec<u8> {
        let mut v = record(0, &[&4u16.to_be_bytes()[..], &op.to_be_bytes()].concat());
        for p in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
            v.extend(knot([p, p, p]));
        }
        v
    }

    fn block(paths: &[Vec<u8>], flags: u32) -> Vec<u8> {
        let mut v = 3u32.to_be_bytes().to_vec();
        v.extend(flags.to_be_bytes());
        v.extend(record(6, &[]));
        paths.iter().for_each(|p| v.extend(p));
        v
    }

    #[test]
    fn rasterizes_square_and_operations() {
        let m = parse(&block(&[square(0.25, 0.25, 0.75, 0.75, 1)], 0), 8, 8).unwrap();
        assert_eq!(m.paths.len(), 1);
        let a = m.rasterize(0, 0, 8, 8);
        assert_eq!(a.iter().sum::<f32>(), 16.0);
        assert_eq!(m.bounds(), Some([2.0, 2.0, 6.0, 6.0]));

        let sub = parse(&block(&[square(0.0, 0.0, 1.0, 1.0, 1), square(0.0, 0.0, 0.5, 1.0, 2)], 0), 8, 8).unwrap();
        assert_eq!(sub.rasterize(0, 0, 8, 8).iter().sum::<f32>(), 32.0);

        let inv = parse(&block(&[square(0.25, 0.25, 0.75, 0.75, 1)], 1), 8, 8).unwrap();
        assert_eq!(inv.rasterize(0, 0, 8, 8).iter().sum::<f32>(), 48.0);
    }

    #[test]
    fn sided_strokes_stay_on_their_side() {
        let m = parse(&block(&[square(0.25, 0.25, 0.75, 0.75, 1)], 0), 16, 16).unwrap();
        let mut s = parse_stroke(&Descriptor::default()).unwrap();
        s.width = 2.0;
        s.align = Align::Inside;
        let inside = s.rasterize(&m, 0, 0, 16, 16);
        assert!((inside.iter().sum::<f32>() - (64.0 - 16.0)).abs() < 1.0);
        s.align = Align::Outside;
        let outside = s.rasterize(&m, 0, 0, 16, 16);
        assert!((outside.iter().sum::<f32>() - (144.0 - 64.0)).abs() < 2.0);
    }
}
