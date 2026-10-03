//! Layer masks: the user mask and the vector mask, with density and feather, as one coverage field.

use super::distance::blur;
use super::vector;
use crate::psd::{Document, Layer, Mask};

/// One factor of a region: a grid of values and the value everywhere outside it.
#[derive(Clone, Debug)]
struct Plane {
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    v: Vec<f32>,
    outside: f32,
}

impl Plane {
    fn at(&self, x: i32, y: i32) -> f32 {
        let (i, j) = (x - self.x, y - self.y);
        if i >= 0 && j >= 0 && (i as usize) < self.w && (j as usize) < self.h {
            self.v[j as usize * self.w + i as usize]
        } else {
            self.outside
        }
    }

    /// Blurs by `feather` pixels, treating everything outside as `outside`.
    fn feather(mut self, feather: f64) -> Plane {
        if feather <= 0.0 {
            return self;
        }
        let pad = (feather * 3.0).ceil() as usize + 1;
        let (w, h) = (self.w + 2 * pad, self.h + 2 * pad);
        let mut v = vec![self.outside; w * h];
        for y in 0..self.h {
            v[(y + pad) * w + pad..(y + pad) * w + pad + self.w].copy_from_slice(&self.v[y * self.w..(y + 1) * self.w]);
        }
        let o = self.outside;
        v.iter_mut().for_each(|p| *p -= o);
        blur(&mut v, w, h, feather, feather * 3.0);
        v.iter_mut().for_each(|p| *p = (*p + o).clamp(0.0, 1.0));
        self.x -= pad as i32;
        self.y -= pad as i32;
        (self.w, self.h, self.v) = (w, h, v);
        self
    }

    fn density(mut self, density: f32) -> Plane {
        if density < 1.0 {
            let d = density.clamp(0.0, 1.0);
            self.v.iter_mut().for_each(|p| *p = d * *p + 1.0 - d);
            self.outside = d * self.outside + 1.0 - d;
        }
        self
    }
}

/// A coverage field over the whole document plane: the product of its planes, 1 when empty.
#[derive(Clone, Debug, Default)]
pub(crate) struct Region {
    planes: Vec<Plane>,
}

impl Region {
    pub fn is_empty(&self) -> bool {
        self.planes.is_empty()
    }

    pub fn at(&self, x: i32, y: i32) -> f32 {
        self.planes.iter().map(|p| p.at(x, y)).product()
    }

    /// Values over the document rectangle `(x, y, w, h)`.
    pub fn grid(&self, x: i32, y: i32, w: usize, h: usize) -> Vec<f32> {
        let mut out = vec![1.0; w * h];
        for p in &self.planes {
            for j in 0..h {
                for i in 0..w {
                    out[j * w + i] *= p.at(x + i as i32, y + j as i32);
                }
            }
        }
        out
    }

    fn push(&mut self, p: Plane) {
        if p.outside == 1.0 && p.v.iter().all(|&v| v >= 1.0) {
            return;
        }
        self.planes.push(p);
    }
}

fn user_plane(m: &Mask) -> Plane {
    let r = m.rect;
    let (w, h) = (r.width(), r.height());
    let v = if m.data.len() == w * h { m.data.iter().map(|&v| v as f32 / 255.0).collect() } else { vec![] };
    let (w, h) = if v.is_empty() { (0, 0) } else { (w, h) };
    Plane { x: r.left, y: r.top, w, h, v, outside: m.default as f32 / 255.0 }.feather(m.feather).density(m.density)
}

/// The vector mask of `l` as a plane, rasterized where it matters.
pub(crate) fn vector_plane(doc: &Document, l: &Layer) -> Option<(i32, i32, usize, usize, Vec<f32>, f32)> {
    let data = l.block(b"vmsk").or(l.block(b"vsms"))?;
    let vm = vector::parse(data, doc.width, doc.height)?;
    if vm.disabled {
        return None;
    }
    let pad = 2;
    let (cx0, cy0, cx1, cy1) = (-pad, -pad, doc.width as i32 + pad, doc.height as i32 + pad);
    let (x0, y0, x1, y1, outside) = match vm.bounds() {
        Some(b) if !vm.invert && !vm.fill_all => (
            (b[0].floor() as i32 - pad).max(cx0),
            (b[1].floor() as i32 - pad).max(cy0),
            (b[2].ceil() as i32 + pad).min(cx1),
            (b[3].ceil() as i32 + pad).min(cy1),
            0.0,
        ),
        _ => (cx0, cy0, cx1, cy1, if vm.invert || (vm.fill_all && vm.paths.is_empty()) { 1.0 } else { 0.0 }),
    };
    if x1 <= x0 || y1 <= y0 {
        return Some((0, 0, 0, 0, vec![], outside));
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    Some((x0, y0, w, h, vm.rasterize(x0, y0, w, h), outside))
}

/// The mask of `l`: its user mask, and its vector mask when `with_vector` (layers whose pixels do
/// not already carry the shape).
pub(crate) fn region(doc: &Document, l: &Layer, with_vector: bool) -> Region {
    let mut r = Region::default();
    if let Some(m) = l.mask.as_ref().filter(|m| !m.disabled) {
        r.push(user_plane(m));
    }
    if with_vector {
        if let Some((x, y, w, h, v, outside)) = vector_plane(doc, l) {
            let p = Plane { x, y, w, h, v, outside }.feather(l.vector_feather).density(l.vector_density);
            r.push(p);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psd::Rect;

    fn mask(data: Vec<u8>, default: u8, density: f32, feather: f64) -> Mask {
        Mask { rect: Rect { top: 0, left: 0, bottom: 1, right: 2 }, default, disabled: false, data, density, feather }
    }

    #[test]
    fn user_mask_with_default_and_density() {
        let mut r = Region::default();
        r.push(user_plane(&mask(vec![0, 255], 0, 1.0, 0.0)));
        assert_eq!(r.grid(-1, 0, 4, 1), [0.0, 0.0, 1.0, 0.0]);
        let mut d = Region::default();
        d.push(user_plane(&mask(vec![0, 255], 0, 0.5, 0.0)));
        assert_eq!(d.grid(0, 0, 2, 1), [0.5, 1.0]);
        assert_eq!(d.at(5, 5), 0.5);
    }

    #[test]
    fn feather_softens_edges() {
        let mut r = Region::default();
        r.push(user_plane(&mask(vec![255, 255], 0, 1.0, 1.0)));
        let g = r.grid(-2, 0, 6, 1);
        assert!(g[0] < g[1] && g[1] < g[2], "{g:?}");
        assert!(g[2] > 0.0 && g[2] < 1.0);
    }

    #[test]
    fn empty_region_is_opaque() {
        let r = Region::default();
        assert!(r.is_empty());
        assert_eq!(r.at(3, 4), 1.0);
    }
}
