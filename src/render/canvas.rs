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

/// Runs `f` over the rows of `rows` (each `stride` long), in parallel when the area is large.
pub(crate) fn for_rows<T: Send>(rows: &mut [T], stride: usize, area: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    if stride == 0 {
        return;
    }
    if area >= PARALLEL_MIN {
        rows.par_chunks_mut(stride).enumerate().for_each(|(i, r)| f(i, r));
    } else {
        rows.chunks_mut(stride).enumerate().for_each(|(i, r)| f(i, r));
    }
}

/// Premultiplied RGBA float raster placed at `(x, y)` in document space.
#[derive(Clone, Debug, Default)]
pub(crate) struct Raster {
    pub x: i32,
    pub y: i32,
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Raster {
    pub fn new(x: i32, y: i32, w: usize, h: usize) -> Raster {
        Raster { x, y, w, h, px: zeroed(w * h * 4) }
    }

    pub fn alpha(&self) -> Vec<f32> {
        self.px.chunks_exact(4).map(|p| p[3]).collect()
    }

    /// Solid `color` with per-pixel coverage `cov`.
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

    /// Copy of the part of `self` inside the given rectangle (transparent where `self` has no pixels).
    pub fn crop(&self, x: i32, y: i32, w: usize, h: usize) -> Raster {
        let mut out = Raster::new(x, y, w, h);
        out.copy_from(self);
        out
    }

    /// Overwrites the overlapping part of `self` with `src`.
    pub fn copy_from(&mut self, src: &Raster) {
        let Some((x0, y0, x1, y1)) = overlap(self, src) else { return };
        let n = (x1 - x0) as usize * 4;
        for y in y0..y1 {
            let d = self.index(x0, y);
            let s = src.index(x0, y);
            self.px[d..d + n].copy_from_slice(&src.px[s..s + n]);
        }
    }

    #[inline]
    pub fn index(&self, x: i32, y: i32) -> usize {
        ((y - self.y) as usize * self.w + (x - self.x) as usize) * 4
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w as i32 && y < self.y + self.h as i32
    }

    /// Composites `src` onto `self` with `mode` and `opacity`.
    ///
    /// `ga`, when present, accumulates the alpha painted so far (the group alpha of a non-isolated
    /// group, used to take the backdrop back out and to limit adjustments to the group's content).
    pub fn paint(&mut self, src: &Raster, mode: BlendMode, opacity: f32, ga: Option<&mut Vec<f32>>) {
        let Some((x0, y0, x1, y1)) = overlap(self, src) else { return };
        if opacity <= 0.0 {
            return;
        }
        let stride = self.w * 4;
        let (sx, sy) = (self.x, self.y);
        let rows = &mut self.px[(y0 - sy) as usize * stride..(y1 - sy) as usize * stride];
        let area = ((x1 - x0) * (y1 - y0)) as usize;
        let row = |i: usize, row: &mut [f32]| {
            let y = y0 + i as i32;
            let s0 = src.index(x0, y);
            let s = &src.px[s0..s0 + (x1 - x0) as usize * 4];
            let d = &mut row[(x0 - sx) as usize * 4..(x1 - sx) as usize * 4];
            if mode.is_normal() {
                source_over(d, s, opacity);
            } else {
                blend_row(d, s, mode, opacity);
            }
        };
        for_rows(rows, stride, area, row);
        if let Some(ga) = ga {
            let w = self.w;
            for y in y0..y1 {
                for x in x0..x1 {
                    let a = src.px[src.index(x, y) + 3] * opacity;
                    let g = &mut ga[(y - sy) as usize * w + (x - sx) as usize];
                    *g = a + *g * (1.0 - a);
                }
            }
        }
    }

    /// Replaces each pixel by `lerp(self, other, weight)`, in premultiplied space.
    pub fn lerp_to(&mut self, other: &Raster, weight: impl Fn(i32, i32) -> f32 + Sync) {
        let stride = self.w * 4;
        let (sx, sy, w) = (self.x, self.y, self.w);
        let area = self.w * self.h;
        for_rows(&mut self.px, stride, area, |i, row| {
            let y = sy + i as i32;
            for x in 0..w {
                let k = weight(sx + x as i32, y);
                if k <= 0.0 {
                    continue;
                }
                let o = &other.px[other.index(sx + x as i32, y)..][..4];
                for c in 0..4 {
                    row[x * 4 + c] += (o[c] - row[x * 4 + c]) * k;
                }
            }
        });
    }

    /// Applies an adjustment: `adjusted = mode(backdrop, f(backdrop))`, mixed in by `weight`.
    pub fn adjust(
        &mut self,
        f: &(dyn Fn([f32; 3]) -> [f32; 3] + Sync),
        mode: BlendMode,
        weight: impl Fn(i32, i32) -> f32 + Sync,
    ) {
        let stride = self.w * 4;
        let (sx, sy, w) = (self.x, self.y, self.w);
        let area = self.w * self.h;
        for_rows(&mut self.px, stride, area, |i, row| {
            let y = sy + i as i32;
            for (x, p) in row.chunks_exact_mut(4).enumerate().take(w) {
                let a = p[3];
                if a <= 0.0 {
                    continue;
                }
                let k = weight(sx + x as i32, y);
                if k <= 0.0 {
                    continue;
                }
                let c = [p[0] / a, p[1] / a, p[2] / a].map(|v| v.clamp(0.0, 1.0));
                let t = f(c).map(|v| v.clamp(0.0, 1.0));
                let t = if mode.is_normal() { t } else { mode.apply(c, t) };
                for ch in 0..3 {
                    p[ch] = (c[ch] + (t[ch] - c[ch]) * k) * a;
                }
            }
        });
    }
}

fn overlap(a: &Raster, b: &Raster) -> Option<(i32, i32, i32, i32)> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w as i32).min(b.x + b.w as i32);
    let y1 = (a.y + a.h as i32).min(b.y + b.h as i32);
    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
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

fn blend_row(dst: &mut [f32], src: &[f32], mode: BlendMode, k: f32) {
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        if s[3] <= 0.0 {
            continue;
        }
        let cs = [s[0] / s[3], s[1] / s[3], s[2] / s[3]].map(|v| v.clamp(0.0, 1.0));
        let sa = s[3] * k;
        let da = d[3];
        if da <= 0.0 {
            for c in 0..3 {
                d[c] = cs[c] * sa + d[c] * (1.0 - sa);
            }
        } else {
            let cb = [d[0] / da, d[1] / da, d[2] / da].map(|v| v.clamp(0.0, 1.0));
            let mixed = mode.apply(cb, cs);
            for c in 0..3 {
                d[c] = cs[c] * sa * (1.0 - da) + d[c] * (1.0 - sa) + sa * da * mixed[c];
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

    fn pixel(c: &Raster, x: i32, y: i32) -> [f32; 4] {
        let i = c.index(x, y);
        [c.px[i], c.px[i + 1], c.px[i + 2], c.px[i + 3]]
    }

    fn near(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn source_over_with_opacity() {
        let mut c = Raster::new(0, 0, 2, 1);
        c.paint(&raster(0, 0, 2, 1, [0.0, 0.0, 1.0, 1.0]), BlendMode::Normal, 1.0, None);
        c.paint(&raster(0, 0, 1, 1, [1.0, 0.0, 0.0, 1.0]), BlendMode::Normal, 0.5, None);
        assert!(near(pixel(&c, 0, 0), [0.5, 0.0, 0.5, 1.0]));
        assert!(near(pixel(&c, 1, 0), [0.0, 0.0, 1.0, 1.0]));
    }

    #[test]
    fn clips_to_canvas_bounds_and_offsets() {
        let mut c = Raster::new(10, 10, 2, 2);
        c.paint(&raster(5, 11, 6, 4, [1.0; 4]), BlendMode::Normal, 1.0, None);
        assert!(near(pixel(&c, 10, 11), [1.0; 4]));
        assert!(near(pixel(&c, 11, 11), [0.0; 4]));
        assert!(near(pixel(&c, 10, 10), [0.0; 4]));
    }

    #[test]
    fn multiply_over_opaque_backdrop() {
        let mut c = Raster::new(0, 0, 1, 1);
        c.paint(&raster(0, 0, 1, 1, [0.5, 1.0, 1.0, 1.0]), BlendMode::Normal, 1.0, None);
        c.paint(&raster(0, 0, 1, 1, [0.5, 0.5, 0.0, 1.0]), BlendMode::Multiply, 1.0, None);
        assert!(near(pixel(&c, 0, 0), [0.25, 0.5, 0.0, 1.0]));
    }

    #[test]
    fn blend_on_transparent_backdrop_acts_normal() {
        let mut c = Raster::new(0, 0, 1, 1);
        c.paint(&raster(0, 0, 1, 1, [0.2, 0.4, 0.6, 1.0]), BlendMode::Screen, 1.0, None);
        assert!(near(pixel(&c, 0, 0), [0.2, 0.4, 0.6, 1.0]));
    }

    #[test]
    fn large_paints_take_the_parallel_path() {
        let mut c = Raster::new(0, 0, 1024, 512);
        c.paint(&raster(0, 0, 1024, 512, [1.0, 0.0, 0.0, 1.0]), BlendMode::Normal, 0.5, None);
        assert!(near(pixel(&c, 1023, 511), [0.5, 0.0, 0.0, 0.5]));
    }

    #[test]
    fn group_alpha_accumulates() {
        let mut c = Raster::new(0, 0, 1, 1);
        let mut ga = vec![0.0];
        c.paint(&raster(0, 0, 1, 1, [1.0, 1.0, 1.0, 0.5]), BlendMode::Normal, 1.0, Some(&mut ga));
        c.paint(&raster(0, 0, 1, 1, [1.0, 1.0, 1.0, 0.5]), BlendMode::Normal, 1.0, Some(&mut ga));
        assert!((ga[0] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn lerp_and_adjust() {
        let mut c = raster(0, 0, 1, 1, [1.0, 0.0, 0.0, 1.0]);
        c.lerp_to(&raster(0, 0, 1, 1, [0.0, 0.0, 1.0, 1.0]), |_, _| 0.25);
        assert!(near(pixel(&c, 0, 0), [0.75, 0.0, 0.25, 1.0]));
        let mut d = raster(0, 0, 1, 1, [0.2, 0.4, 0.6, 0.5]);
        d.adjust(&|c| c.map(|v| 1.0 - v), BlendMode::Normal, |_, _| 1.0);
        assert!(near(pixel(&d, 0, 0), [0.4, 0.3, 0.2, 0.5]));
    }

    #[test]
    fn padding_crop_and_solid() {
        let r = raster(3, 4, 1, 1, [1.0; 4]).padded(2);
        assert_eq!((r.x, r.y, r.w, r.h), (1, 2, 5, 5));
        assert_eq!(r.alpha().iter().sum::<f32>(), 1.0);
        assert_eq!(r.alpha()[12], 1.0);
        let c = r.crop(3, 4, 2, 1);
        assert_eq!(c.alpha(), [1.0, 0.0]);
        let s = Raster::solid(0, 0, 2, 1, &[1.0, 0.5], [1.0, 0.0, 0.0]);
        assert_eq!(s.px, [1.0, 0.0, 0.0, 1.0, 0.5, 0.0, 0.0, 0.5]);
    }
}
