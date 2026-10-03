use rayon::prelude::*;

use crate::blend::BlendMode;

/// Pixel counts below which work runs on the calling thread: layers already render in parallel,
/// and dispatching tiny jobs costs more than doing them.
pub(crate) const PARALLEL_MIN: usize = 1 << 18;

/// Touches one element per page of a freshly zeroed buffer.
///
/// Zeroed allocations map the shared zero page; without this the first write to each page takes a
/// copy-on-write fault and a TLB shootdown on every thread.
pub(crate) fn prefault<T: Copy>(v: &mut [T]) {
    let step = (4096 / std::mem::size_of::<T>().max(1)).max(1);
    for i in (0..v.len()).step_by(step) {
        // SAFETY: `i < v.len()`, so the pointer is in bounds and properly aligned.
        unsafe { std::ptr::write_volatile(v.as_mut_ptr().add(i), v[i]) };
    }
}

/// Zero-filled, prefaulted buffer.
pub(crate) fn zeroed(len: usize) -> Vec<f32> {
    let mut v = vec![0.0; len];
    prefault(&mut v);
    v
}

/// Premultiplied RGBA float raster placed at `(x, y)` in document space.
#[derive(Clone, Debug)]
pub(crate) struct Raster {
    pub x: i32,
    pub y: i32,
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Raster {
    pub fn alpha(&self) -> Vec<f32> {
        self.px.chunks_exact(4).map(|p| p[3]).collect()
    }

    /// Solid `color` with per-pixel coverage `cov`, covering the same rectangle as `self`.
    pub fn solid(x: i32, y: i32, w: usize, h: usize, cov: &[f32], color: [f32; 3]) -> Raster {
        let mut px = vec![0.0; w * h * 4];
        for (p, &k) in px.chunks_exact_mut(4).zip(cov) {
            p.copy_from_slice(&[color[0] * k, color[1] * k, color[2] * k, k]);
        }
        Raster { x, y, w, h, px }
    }

    /// Grows the raster by `pad` transparent pixels on every side.
    pub fn padded(self, pad: usize) -> Raster {
        if pad == 0 {
            return self;
        }
        let (w, h) = (self.w + 2 * pad, self.h + 2 * pad);
        let mut px = vec![0.0; w * h * 4];
        for y in 0..self.h {
            let dst = ((y + pad) * w + pad) * 4;
            px[dst..dst + self.w * 4].copy_from_slice(&self.px[y * self.w * 4..(y + 1) * self.w * 4]);
        }
        Raster { x: self.x - pad as i32, y: self.y - pad as i32, w, h, px }
    }
}

/// One compositing operation.
pub(crate) struct Paint {
    pub raster: Raster,
    pub mode: BlendMode,
    pub opacity: f32,
}

/// Single-channel alpha over a document rectangle, used as the base of a clipping group.
#[derive(Clone, Debug, Default)]
pub(crate) struct Alpha {
    pub x: i32,
    pub y: i32,
    pub w: usize,
    pub h: usize,
    pub a: Vec<f32>,
}

impl Alpha {
    pub fn of(r: &Raster) -> Alpha {
        Alpha { x: r.x, y: r.y, w: r.w, h: r.h, a: r.alpha() }
    }

    #[inline]
    pub fn at(&self, x: i32, y: i32) -> f32 {
        let (u, v) = (x - self.x, y - self.y);
        if u < 0 || v < 0 || u as usize >= self.w || v as usize >= self.h {
            0.0
        } else {
            self.a[v as usize * self.w + u as usize]
        }
    }
}

/// Premultiplied RGBA float surface the layers are composited onto.
pub(crate) struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, px: zeroed(w * h * 4) }
    }

    pub fn alpha(&self) -> Vec<f32> {
        self.px.chunks_exact(4).map(|p| p[3]).collect()
    }

    /// Composites `p` with an extra `opacity`, optionally masked by a clipping base.
    pub fn paint(&mut self, p: &Paint, opacity: f32, clip: Option<&Alpha>) {
        let r = &p.raster;
        let mut x0 = r.x.max(0);
        let mut y0 = r.y.max(0);
        let mut x1 = (r.x + r.w as i32).min(self.w as i32);
        let mut y1 = (r.y + r.h as i32).min(self.h as i32);
        if let Some(c) = clip {
            x0 = x0.max(c.x);
            y0 = y0.max(c.y);
            x1 = x1.min(c.x + c.w as i32);
            y1 = y1.min(c.y + c.h as i32);
        }
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let k = p.opacity * opacity;
        let mode = p.mode;
        let stride = self.w * 4;
        let rows = &mut self.px[y0 as usize * stride..y1 as usize * stride];
        let body = |(i, row): (usize, &mut [f32])| {
            let y = y0 + i as i32;
            let src_row = (y - r.y) as usize * r.w;
            let src = &r.px[(src_row + (x0 - r.x) as usize) * 4..(src_row + (x1 - r.x) as usize) * 4];
            let dst = &mut row[x0 as usize * 4..x1 as usize * 4];
            match clip {
                None if mode.is_normal() => source_over(dst, src, k),
                _ => blend_row(dst, src, mode, |j| k * clip.map_or(1.0, |c| c.at(x0 + j as i32, y))),
            }
        };
        if ((x1 - x0) * (y1 - y0)) as usize >= PARALLEL_MIN {
            rows.par_chunks_mut(stride).enumerate().for_each(body);
        } else {
            rows.chunks_mut(stride).enumerate().for_each(body);
        }
    }
}

fn source_over(dst: &mut [f32], src: &[f32], k: f32) {
    if k >= 1.0 {
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            let ia = 1.0 - s[3];
            for c in 0..4 {
                d[c] = s[c] + d[c] * ia;
            }
        }
    } else {
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            let ia = 1.0 - s[3] * k;
            for c in 0..4 {
                d[c] = s[c] * k + d[c] * ia;
            }
        }
    }
}

fn blend_row(dst: &mut [f32], src: &[f32], mode: BlendMode, weight: impl Fn(usize) -> f32) {
    for (j, (d, s)) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)).enumerate() {
        let k = weight(j);
        let sa = s[3] * k;
        if sa <= 0.0 {
            continue;
        }
        let sc = [s[0] * k, s[1] * k, s[2] * k];
        let da = d[3];
        if mode.is_normal() || da <= 0.0 {
            for c in 0..3 {
                d[c] = sc[c] + d[c] * (1.0 - sa);
            }
        } else {
            let cs = sc.map(|v| v / sa);
            let cb = [d[0] / da, d[1] / da, d[2] / da];
            let mixed = mode.apply(cb, cs);
            for c in 0..3 {
                d[c] = sc[c] * (1.0 - da) + d[c] * (1.0 - sa) + sa * da * mixed[c];
            }
        }
        d[3] = sa + da * (1.0 - sa);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(x: i32, y: i32, w: usize, h: usize, rgba: [f32; 4]) -> Raster {
        let a = rgba[3];
        let px = [rgba[0] * a, rgba[1] * a, rgba[2] * a, a].repeat(w * h);
        Raster { x, y, w, h, px }
    }

    fn pixel(c: &Canvas, x: usize, y: usize) -> [f32; 4] {
        let i = (y * c.w + x) * 4;
        [c.px[i], c.px[i + 1], c.px[i + 2], c.px[i + 3]]
    }

    fn near(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    fn paint(r: Raster, mode: BlendMode, opacity: f32) -> Paint {
        Paint { raster: r, mode, opacity }
    }

    #[test]
    fn source_over_with_opacity() {
        let mut c = Canvas::new(2, 1);
        c.paint(&paint(raster(0, 0, 2, 1, [0.0, 0.0, 1.0, 1.0]), BlendMode::Normal, 1.0), 1.0, None);
        c.paint(&paint(raster(0, 0, 1, 1, [1.0, 0.0, 0.0, 1.0]), BlendMode::Normal, 0.5), 1.0, None);
        assert!(near(pixel(&c, 0, 0), [0.5, 0.0, 0.5, 1.0]));
        assert!(near(pixel(&c, 1, 0), [0.0, 0.0, 1.0, 1.0]));
    }

    #[test]
    fn clips_to_canvas_bounds() {
        let mut c = Canvas::new(2, 2);
        c.paint(&paint(raster(-5, 1, 6, 4, [1.0; 4]), BlendMode::Normal, 1.0), 1.0, None);
        assert!(near(pixel(&c, 0, 1), [1.0; 4]));
        assert!(near(pixel(&c, 1, 1), [0.0; 4]));
        assert!(near(pixel(&c, 0, 0), [0.0; 4]));
    }

    #[test]
    fn clipping_base_masks_the_paint() {
        let mut c = Canvas::new(3, 1);
        let base = Alpha { x: 1, y: 0, w: 1, h: 1, a: vec![0.5] };
        c.paint(&paint(raster(0, 0, 3, 1, [1.0; 4]), BlendMode::Normal, 1.0), 1.0, Some(&base));
        assert!(near(pixel(&c, 0, 0), [0.0; 4]));
        assert!(near(pixel(&c, 1, 0), [0.5; 4]));
        assert!(near(pixel(&c, 2, 0), [0.0; 4]));
    }

    #[test]
    fn multiply_over_opaque_backdrop() {
        let mut c = Canvas::new(1, 1);
        c.paint(&paint(raster(0, 0, 1, 1, [0.5, 1.0, 1.0, 1.0]), BlendMode::Normal, 1.0), 1.0, None);
        c.paint(&paint(raster(0, 0, 1, 1, [0.5, 0.5, 0.0, 1.0]), BlendMode::Multiply, 1.0), 1.0, None);
        assert!(near(pixel(&c, 0, 0), [0.25, 0.5, 0.0, 1.0]));
    }

    #[test]
    fn blend_on_transparent_backdrop_acts_normal() {
        let mut c = Canvas::new(1, 1);
        c.paint(&paint(raster(0, 0, 1, 1, [0.2, 0.4, 0.6, 1.0]), BlendMode::Screen, 1.0), 1.0, None);
        assert!(near(pixel(&c, 0, 0), [0.2, 0.4, 0.6, 1.0]));
    }

    #[test]
    fn large_paints_take_the_parallel_path() {
        let mut c = Canvas::new(1024, 512);
        c.paint(&paint(raster(0, 0, 1024, 512, [1.0, 0.0, 0.0, 1.0]), BlendMode::Normal, 1.0), 0.5, None);
        assert!(near(pixel(&c, 1023, 511), [0.5, 0.0, 0.0, 0.5]));
    }

    #[test]
    fn padding_and_solid() {
        let r = raster(3, 4, 1, 1, [1.0; 4]).padded(2);
        assert_eq!((r.x, r.y, r.w, r.h), (1, 2, 5, 5));
        assert_eq!(r.alpha().iter().sum::<f32>(), 1.0);
        assert_eq!(r.alpha()[12], 1.0);
        let s = Raster::solid(0, 0, 2, 1, &[1.0, 0.5], [1.0, 0.0, 0.0]);
        assert_eq!(s.px, [1.0, 0.0, 0.0, 1.0, 0.5, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn alpha_lookup_outside_is_zero() {
        let a = Alpha { x: 1, y: 1, w: 1, h: 1, a: vec![0.7] };
        assert_eq!(a.at(1, 1), 0.7);
        assert_eq!(a.at(0, 1), 0.0);
        assert_eq!(a.at(2, 1), 0.0);
    }
}
