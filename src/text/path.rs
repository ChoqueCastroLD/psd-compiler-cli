use rustybuzz::ttf_parser;

/// Path segment in text or document space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Seg {
    Move(f64, f64),
    Line(f64, f64),
    Quad(f64, f64, f64, f64),
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

pub(crate) type Path = Vec<Seg>;

impl Seg {
    fn map(self, f: &impl Fn(f64, f64) -> (f64, f64)) -> Seg {
        match self {
            Seg::Move(x, y) => {
                let (x, y) = f(x, y);
                Seg::Move(x, y)
            }
            Seg::Line(x, y) => {
                let (x, y) = f(x, y);
                Seg::Line(x, y)
            }
            Seg::Quad(x1, y1, x, y) => {
                let ((x1, y1), (x, y)) = (f(x1, y1), f(x, y));
                Seg::Quad(x1, y1, x, y)
            }
            Seg::Cubic(x1, y1, x2, y2, x, y) => {
                let ((x1, y1), (x2, y2), (x, y)) = (f(x1, y1), f(x2, y2), f(x, y));
                Seg::Cubic(x1, y1, x2, y2, x, y)
            }
            Seg::Close => Seg::Close,
        }
    }

    pub fn points(&self) -> impl Iterator<Item = (f64, f64)> {
        let pts: [Option<(f64, f64)>; 3] = match *self {
            Seg::Move(x, y) | Seg::Line(x, y) => [Some((x, y)), None, None],
            Seg::Quad(a, b, x, y) => [Some((a, b)), Some((x, y)), None],
            Seg::Cubic(a, b, c, d, x, y) => [Some((a, b)), Some((c, d)), Some((x, y))],
            Seg::Close => [None; 3],
        };
        pts.into_iter().flatten()
    }
}

pub(crate) fn map(path: &[Seg], f: impl Fn(f64, f64) -> (f64, f64)) -> Path {
    path.iter().map(|s| s.map(&f)).collect()
}

/// Bounding box `[left, top, right, bottom]` of all points, including control points.
pub(crate) fn bounds<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Option<[f64; 4]> {
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for p in paths {
        for (x, y) in p.iter().flat_map(Seg::points) {
            b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
    }
    (b[0] <= b[2]).then_some(b)
}

/// Replaces curves with line segments no longer than about `step`, as needed before a non-affine map.
pub(crate) fn flatten(path: &[Seg], step: f64) -> Path {
    let mut out = Vec::with_capacity(path.len() * 4);
    let (mut cx, mut cy) = (0.0, 0.0);
    let pieces = |len: f64, min: f64| (len / step).ceil().max(min) as usize;
    for seg in path {
        match *seg {
            Seg::Move(x, y) => {
                out.push(Seg::Move(x, y));
                (cx, cy) = (x, y);
            }
            Seg::Line(x, y) => {
                let n = pieces((x - cx).hypot(y - cy), 1.0);
                for i in 1..=n {
                    let t = i as f64 / n as f64;
                    out.push(Seg::Line(cx + (x - cx) * t, cy + (y - cy) * t));
                }
                (cx, cy) = (x, y);
            }
            Seg::Quad(x1, y1, x, y) => {
                let n = pieces((x1 - cx).hypot(y1 - cy) + (x - x1).hypot(y - y1), 2.0);
                for i in 1..=n {
                    let t = i as f64 / n as f64;
                    let u = 1.0 - t;
                    let bez = |p0: f64, p1: f64, p2: f64| u * u * p0 + 2.0 * u * t * p1 + t * t * p2;
                    out.push(Seg::Line(bez(cx, x1, x), bez(cy, y1, y)));
                }
                (cx, cy) = (x, y);
            }
            Seg::Cubic(x1, y1, x2, y2, x, y) => {
                let len = (x1 - cx).hypot(y1 - cy) + (x2 - x1).hypot(y2 - y1) + (x - x2).hypot(y - y2);
                let n = pieces(len, 2.0);
                for i in 1..=n {
                    let t = i as f64 / n as f64;
                    let u = 1.0 - t;
                    let bez = |p0: f64, p1: f64, p2: f64, p3: f64| {
                        u * u * u * p0 + 3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t * p3
                    };
                    out.push(Seg::Line(bez(cx, x1, x2, x), bez(cy, y1, y2, y)));
                }
                (cx, cy) = (x, y);
            }
            Seg::Close => out.push(Seg::Close),
        }
    }
    out
}

/// Collects a glyph outline, mapping font units through `f`.
pub(crate) struct Outline<F> {
    pub path: Path,
    pub f: F,
}

impl<F: Fn(f32, f32) -> (f64, f64)> ttf_parser::OutlineBuilder for Outline<F> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = (self.f)(x, y);
        self.path.push(Seg::Move(x, y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = (self.f)(x, y);
        self.path.push(Seg::Line(x, y));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let ((x1, y1), (x, y)) = ((self.f)(x1, y1), (self.f)(x, y));
        self.path.push(Seg::Quad(x1, y1, x, y));
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let ((x1, y1), (x2, y2), (x, y)) = ((self.f)(x1, y1), (self.f)(x2, y2), (self.f)(x, y));
        self.path.push(Seg::Cubic(x1, y1, x2, y2, x, y));
    }

    fn close(&mut self) {
        self.path.push(Seg::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end(p: &Path) -> (f64, f64) {
        p.iter().rev().find_map(|s| s.points().last()).unwrap()
    }

    #[test]
    fn flatten_keeps_endpoints_and_bounds_step() {
        let p = vec![Seg::Move(0.0, 0.0), Seg::Cubic(0.0, 10.0, 10.0, 10.0, 10.0, 0.0), Seg::Close];
        let f = flatten(&p, 1.0);
        assert!(f.iter().all(|s| matches!(s, Seg::Move(..) | Seg::Line(..) | Seg::Close)));
        let e = end(&f);
        assert!((e.0 - 10.0).abs() < 1e-9 && e.1.abs() < 1e-9);
        let pts: Vec<_> = f.iter().flat_map(Seg::points).collect();
        assert!(pts.windows(2).all(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1) <= 1.01));
    }

    #[test]
    fn flatten_quad_passes_through_midpoint() {
        let f = flatten(&[Seg::Move(0.0, 0.0), Seg::Quad(1.0, 2.0, 2.0, 0.0)], 0.01);
        assert!(f.iter().flat_map(Seg::points).any(|(x, y)| (x - 1.0).abs() < 0.01 && (y - 1.0).abs() < 0.01));
    }

    #[test]
    fn map_and_bounds() {
        let p = vec![Seg::Move(1.0, 2.0), Seg::Quad(3.0, -1.0, 5.0, 2.0), Seg::Close];
        let m = map(&p, |x, y| (x * 2.0, y + 1.0));
        assert_eq!(m[1], Seg::Quad(6.0, 0.0, 10.0, 3.0));
        assert_eq!(bounds([&m]), Some([2.0, 0.0, 10.0, 3.0]));
        assert_eq!(bounds(std::iter::empty::<&Path>()), None);
    }
}
