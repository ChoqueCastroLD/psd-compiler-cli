//! Exact-area path coverage: every pixel gets the area of it the path covers, like Photoshop's
//! vector rasterizer (tiny-skia's anti-aliasing steps in quarters along near-horizontal edges).

use tiny_skia::{Path, PathSegment};

/// Flattening tolerance in pixels.
const TOLERANCE: f32 = 0.02;

struct Accumulator {
    w: usize,
    h: usize,
    /// Rows of `w + 2` cells; column 0 gathers everything left of the canvas.
    a: Vec<f32>,
}

impl Accumulator {
    /// Splits the line where it crosses the canvas sides, so that parts beside the canvas run
    /// along its edges.
    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        let mut cuts = [0.0f32, 1.0, 1.0, 1.0];
        let mut n = 1;
        for edge in [0.0, self.w as f32] {
            let t = (edge - p0.0) / (p1.0 - p0.0);
            if t > 0.0 && t < 1.0 {
                cuts[n] = t;
                n += 1;
            }
        }
        cuts[..n].sort_by(f32::total_cmp);
        cuts[n] = 1.0;
        let at = |t: f32| (p0.0 + (p1.0 - p0.0) * t, p0.1 + (p1.1 - p0.1) * t);
        for i in 0..n {
            let (a, b) = (at(cuts[i]), at(cuts[i + 1]));
            let clamp = |p: (f32, f32)| (p.0.clamp(0.0, self.w as f32), p.1);
            self.span(clamp(a), clamp(b));
        }
    }

    fn span(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32)) {
        if y0 == y1 || !(y0.is_finite() && y1.is_finite() && x0.is_finite() && x1.is_finite()) {
            return;
        }
        let (dir, (x0, y0), (x1, y1)) = if y0 < y1 { (1.0, (x0, y0), (x1, y1)) } else { (-1.0, (x1, y1), (x0, y0)) };
        let dxdy = (x1 - x0) / (y1 - y0);
        let top = y0.max(0.0);
        let bottom = y1.min(self.h as f32);
        if top >= bottom {
            return;
        }
        let stride = self.w + 2;
        // Shifted one column right, so that edges on the canvas sides stay inside the row.
        let lim = (self.w + 1) as f32;
        let mut x = x0 + (top - y0) * dxdy + 1.0;
        let mut y = top;
        while y < bottom {
            let row = y.floor();
            let next = (row + 1.0).min(bottom);
            let dy = next - y;
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let line = &mut self.a[row as usize * stride..(row as usize + 1) * stride];
            let (lo, hi) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let (lo, hi) = (lo.clamp(0.0, lim), hi.clamp(0.0, lim));
            let lo_floor = lo.floor();
            let lo_i = lo_floor as usize;
            let hi_ceil = hi.ceil();
            let hi_i = hi_ceil as usize;
            if hi_i <= lo_i + 1 {
                let mid = 0.5 * (lo + hi) - lo_floor;
                line[lo_i] += d - d * mid;
                if lo_i + 1 < stride {
                    line[lo_i + 1] += d * mid;
                }
            } else {
                let s = 1.0 / (hi - lo);
                let lo_f = lo - lo_floor;
                let a0 = 0.5 * s * (1.0 - lo_f) * (1.0 - lo_f);
                let hi_f = hi - hi_ceil + 1.0;
                let am = 0.5 * s * hi_f * hi_f;
                line[lo_i] += d * a0;
                if hi_i == lo_i + 2 {
                    line[lo_i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - lo_f);
                    line[lo_i + 1] += d * (a1 - a0);
                    for c in &mut line[lo_i + 2..hi_i - 1] {
                        *c += d * s;
                    }
                    let a2 = a1 + (hi_i - lo_i - 3) as f32 * s;
                    line[hi_i - 1] += d * (1.0 - a2 - am);
                }
                if hi_i < stride {
                    line[hi_i] += d * am;
                }
            }
            x = xnext;
            y = next;
        }
    }

    fn quad(&mut self, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)) {
        let dd = ((p0.0 - 2.0 * p1.0 + p2.0).powi(2) + (p0.1 - 2.0 * p1.1 + p2.1).powi(2)).sqrt();
        let n = ((dd / (4.0 * TOLERANCE)).sqrt().ceil() as usize).clamp(1, 256);
        let mut prev = p0;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let p =
                (u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0, u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1);
            self.line(prev, p);
            prev = p;
        }
    }

    fn cubic(&mut self, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)) {
        let dd = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
            ((a.0 - 2.0 * b.0 + c.0).powi(2) + (a.1 - 2.0 * b.1 + c.1).powi(2)).sqrt()
        };
        let m = dd(p0, p1, p2).max(dd(p1, p2, p3));
        let n = ((0.75 * m / TOLERANCE).sqrt().ceil() as usize).clamp(1, 256);
        let mut prev = p0;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let (a, b, c, e) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            let p = (a * p0.0 + b * p1.0 + c * p2.0 + e * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + e * p3.1);
            self.line(prev, p);
            prev = p;
        }
    }
}

/// Coverage of `path` over a `w x h` canvas, under the even-odd or the non-zero rule.
pub(crate) fn fill(path: &Path, w: usize, h: usize, even_odd: bool) -> Vec<f32> {
    let mut acc = Accumulator { w, h, a: vec![0.0; (w + 2) * h] };
    let (mut start, mut at) = ((0.0, 0.0), (0.0, 0.0));
    let pt = |p: tiny_skia::Point| (p.x, p.y);
    for seg in path.segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                acc.line(at, start);
                start = pt(p);
                at = start;
            }
            PathSegment::LineTo(p) => {
                acc.line(at, pt(p));
                at = pt(p);
            }
            PathSegment::QuadTo(c, p) => {
                acc.quad(at, pt(c), pt(p));
                at = pt(p);
            }
            PathSegment::CubicTo(c1, c2, p) => {
                acc.cubic(at, pt(c1), pt(c2), pt(p));
                at = pt(p);
            }
            PathSegment::Close => {
                acc.line(at, start);
                at = start;
            }
        }
    }
    acc.line(at, start);
    let mut out = vec![0f32; w * h];
    for (row, o) in acc.a.chunks(w + 2).zip(out.chunks_mut(w)) {
        let mut sum = row[0] as f64;
        for (o, &c) in o.iter_mut().zip(&row[1..]) {
            sum += c as f64;
            let v = sum.abs() as f32;
            let v = if even_odd {
                let v = v % 2.0;
                if v > 1.0 {
                    2.0 - v
                } else {
                    v
                }
            } else {
                v.min(1.0)
            };
            // Rounding leaves crumbs; full and empty pixels must be exact.
            *o = if v < 1e-3 {
                0.0
            } else if v > 1.0 - 1e-3 {
                1.0
            } else {
                v
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::PathBuilder;

    #[test]
    fn covers_exact_areas() {
        // A square from (1.25, 1.5) to (3.75, 3.5): edge pixels are partly covered.
        let p = PathBuilder::from_rect(tiny_skia::Rect::from_ltrb(1.25, 1.5, 3.75, 3.5).unwrap());
        let c = fill(&p, 5, 5, false);
        let at = |x: usize, y: usize| c[y * 5 + x];
        assert!((at(1, 1) - 0.375).abs() < 1e-5, "{}", at(1, 1));
        assert!((at(2, 2) - 1.0).abs() < 1e-5);
        assert!((at(3, 3) - 0.375).abs() < 1e-5);
        assert!((c.iter().sum::<f32>() - 5.0).abs() < 1e-4);
    }

    #[test]
    fn even_odd_cuts_holes() {
        let mut pb = PathBuilder::new();
        pb.push_rect(tiny_skia::Rect::from_ltrb(0.0, 0.0, 6.0, 6.0).unwrap());
        pb.push_rect(tiny_skia::Rect::from_ltrb(2.0, 2.0, 4.0, 4.0).unwrap());
        let p = pb.finish().unwrap();
        assert_eq!(fill(&p, 6, 6, true)[2 * 6 + 2], 0.0);
        assert_eq!(fill(&p, 6, 6, false)[2 * 6 + 2], 1.0);
    }

    #[test]
    fn clips_to_the_canvas() {
        let p = PathBuilder::from_rect(tiny_skia::Rect::from_ltrb(-3.0, -2.0, 2.5, 9.0).unwrap());
        let c = fill(&p, 4, 4, false);
        assert_eq!(&c[..4], &[1.0, 1.0, 0.5, 0.0]);
        assert_eq!(&c[12..], &[1.0, 1.0, 0.5, 0.0]);
    }
}
