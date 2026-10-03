//! Distance transforms and blurs for layer effects, on a supersampled grid.

use rayon::prelude::*;

use super::canvas::PARALLEL_MIN;

pub(crate) const INF: f32 = 1e20;

/// Supersampling factor of the effect grid.
pub(crate) const SS: usize = 4;

/// 1-D squared distance transform (Felzenszwalb & Huttenlocher) over the finite entries of `f`.
fn dt1d(f: &[f32], d: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    let mut k: isize = -1;
    for q in 0..f.len() {
        if f[q] >= INF {
            continue;
        }
        loop {
            if k < 0 {
                k = 0;
                v[0] = q;
                z[0] = f32::NEG_INFINITY;
                z[1] = f32::INFINITY;
                break;
            }
            let p = v[k as usize];
            let s = ((f[q] + (q * q) as f32) - (f[p] + (p * p) as f32)) / (2.0 * (q as f32 - p as f32));
            if s <= z[k as usize] {
                k -= 1;
                continue;
            }
            k += 1;
            v[k as usize] = q;
            z[k as usize] = s;
            z[k as usize + 1] = f32::INFINITY;
            break;
        }
    }
    if k < 0 {
        d.fill(INF);
        return;
    }
    let mut k = 0;
    for q in 0..f.len() {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let p = v[k];
        d[q] = (q as f32 - p as f32).powi(2) + f[p];
    }
}

fn for_rows(buf: &mut [f32], n: usize, f: impl Fn(&mut [f32]) + Send + Sync) {
    if buf.len() >= PARALLEL_MIN {
        buf.par_chunks_mut(n).for_each(f);
    } else {
        buf.chunks_mut(n).for_each(f);
    }
}

fn dt_row(row: &mut [f32]) {
    if row.iter().all(|&v| v >= INF) || row.iter().all(|&v| v == 0.0) {
        return;
    }
    let f = row.to_vec();
    let (mut v, mut z) = (vec![0usize; row.len()], vec![0f32; row.len() + 1]);
    dt1d(&f, row, &mut v, &mut z);
}

/// Squared distance from every cell to the nearest `seed` cell.
/// Exact up to `rmax` cells; anything farther comes back as [`INF`].
pub(crate) fn edt(seed: &[bool], w: usize, h: usize, rmax: f32) -> Vec<f32> {
    let r2 = rmax * rmax;
    let cap = rmax.ceil() as u32 + 1;
    let mut down = vec![0u32; w * h];
    let mut run = vec![cap; w];
    for y in 0..h {
        for x in 0..w {
            run[x] = if seed[y * w + x] { 0 } else { (run[x] + 1).min(cap) };
            down[y * w + x] = run[x];
        }
    }
    run.fill(cap);
    let mut g = vec![0f32; w * h];
    for y in (0..h).rev() {
        for x in 0..w {
            let i = y * w + x;
            run[x] = if seed[i] { 0 } else { (run[x] + 1).min(cap) };
            let dy = run[x].min(down[i]) as f32;
            g[i] = if dy * dy > r2 { INF } else { dy * dy };
        }
    }
    for_rows(&mut g, w, dt_row);
    g
}

/// Inside/outside mask on the supersampled grid from anti-aliased coverage (bilinear, threshold 0.5).
pub(crate) fn supersample(a: &[f32], w: usize, h: usize) -> Vec<bool> {
    let (sw, sh) = (w * SS, h * SS);
    let pw = w + 2;
    let mut padded = vec![0f32; pw * (h + 2)];
    for y in 0..h {
        padded[(y + 1) * pw + 1..(y + 1) * pw + 1 + w].copy_from_slice(&a[y * w..(y + 1) * w]);
    }
    let taps = |n: usize| -> Vec<(usize, f32)> {
        (0..n)
            .map(|s| {
                let f = (s as f32 + 0.5) / SS as f32 - 0.5;
                let f0 = f.floor();
                ((f0 as isize + 1) as usize, f - f0)
            })
            .collect()
    };
    let (tx, ty) = (taps(sw), taps(sh));
    let mut mask = vec![false; sw * sh];
    for (sy, &(yi, fy)) in ty.iter().enumerate() {
        let (r0, r1) = (&padded[yi * pw..(yi + 1) * pw], &padded[(yi + 1) * pw..(yi + 2) * pw]);
        for (out, &(xi, fx)) in mask[sy * sw..(sy + 1) * sw].iter_mut().zip(&tx) {
            let top = r0[xi] + (r0[xi + 1] - r0[xi]) * fx;
            let bottom = r1[xi] + (r1[xi + 1] - r1[xi]) * fx;
            *out = top + (bottom - top) * fy >= 0.5;
        }
    }
    mask
}

/// Coverage per pixel: the fraction of its `SS x SS` cells where `pred` holds.
pub(crate) fn downsample(pred: impl Fn(usize) -> bool, w: usize, h: usize) -> Vec<f32> {
    let sw = w * SS;
    let norm = 1.0 / (SS * SS) as f32;
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut n = 0;
            for j in 0..SS {
                for i in 0..SS {
                    n += pred((y * SS + j) * sw + x * SS + i) as u32;
                }
            }
            out[y * w + x] = n as f32 * norm;
        }
    }
    out
}

/// Inside mask and squared distances (in cells) to the shape and to its complement.
pub(crate) struct Distances {
    pub inside: Vec<bool>,
    pub outside: Vec<f32>,
    pub inward: Vec<f32>,
}

/// Computes distances exact up to `rmax` pixels; `inward` is empty unless `need_inward`.
pub(crate) fn distances(a: &[f32], w: usize, h: usize, need_inward: bool, rmax: f64) -> Distances {
    let reach = (rmax * SS as f64) as f32 + 2.0;
    let inside = supersample(a, w, h);
    let outside = edt(&inside, w * SS, h * SS, reach);
    let inward = if need_inward {
        let holes: Vec<bool> = inside.iter().map(|&b| !b).collect();
        edt(&holes, w * SS, h * SS, reach)
    } else {
        vec![]
    };
    Distances { inside, outside, inward }
}

/// Blocked transpose of a `w x h` row-major buffer into `h x w`.
pub(crate) fn transpose(src: &[f32], dst: &mut [f32], w: usize, h: usize) {
    const B: usize = 32;
    for by in (0..h).step_by(B) {
        for bx in (0..w).step_by(B) {
            for y in by..(by + B).min(h) {
                for x in bx..(bx + B).min(w) {
                    dst[x * h + y] = src[y * w + x];
                }
            }
        }
    }
}

/// Separable Gaussian blur with standard deviation `sigma`, truncated at `max_radius` pixels.
/// Pixels outside the buffer count as zero.
pub(crate) fn blur(a: &mut [f32], w: usize, h: usize, sigma: f64, max_radius: f64) {
    if sigma < 0.2 {
        return;
    }
    let r = (sigma * 3.0).min(max_radius).ceil().max(1.0) as isize;
    let mut kernel: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp() as f32).collect();
    let sum: f32 = kernel.iter().sum();
    kernel.iter_mut().for_each(|v| *v /= sum);
    let convolve = |src: &[f32], dst: &mut [f32]| {
        let n = src.len() as isize;
        for i in 0..n {
            let (lo, hi) = ((i - r).max(0), (i + r).min(n - 1));
            dst[i as usize] = (lo..=hi).map(|j| src[j as usize] * kernel[(j - i + r) as usize]).sum();
        }
    };
    let pass = |dst: &mut [f32], src: &[f32], n: usize| {
        if src.len() >= PARALLEL_MIN {
            dst.par_chunks_mut(n).zip(src.par_chunks(n)).for_each(|(d, s)| convolve(s, d));
        } else {
            dst.chunks_mut(n).zip(src.chunks(n)).for_each(|(d, s)| convolve(s, d));
        }
    };
    let mut tmp = vec![0f32; a.len()];
    pass(&mut tmp, a, w);
    let mut columns = vec![0f32; a.len()];
    transpose(&tmp, &mut columns, w, h);
    pass(&mut tmp, &columns, h);
    transpose(&tmp, a, h, w);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(seed: &[bool], w: usize, h: usize) -> Vec<f32> {
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                (0..w * h)
                    .filter(|&j| seed[j])
                    .map(|j| ((j % w) as f32 - x).powi(2) + ((j / w) as f32 - y).powi(2))
                    .fold(INF, f32::min)
            })
            .collect()
    }

    fn pseudo_random_seeds(w: usize, h: usize, density: u32) -> Vec<bool> {
        let mut s = 0x2545F491u32;
        (0..w * h)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s % 100 < density
            })
            .collect()
    }

    #[test]
    fn edt_matches_brute_force_within_reach() {
        let (w, h) = (37, 23);
        for density in [1, 5, 30] {
            let seed = pseudo_random_seeds(w, h, density);
            let rmax = 9.0;
            let fast = edt(&seed, w, h, rmax);
            for (i, (&f, &b)) in fast.iter().zip(&brute(&seed, w, h)).enumerate() {
                if b <= rmax * rmax {
                    assert_eq!(f, b, "cell {i} density {density}");
                } else {
                    assert!(f > rmax * rmax, "cell {i} should be out of reach");
                }
            }
        }
    }

    #[test]
    fn edt_without_seeds_is_infinite() {
        assert!(edt(&[false; 12], 4, 3, 5.0).iter().all(|&v| v >= INF));
    }

    #[test]
    fn supersample_thresholds_coverage() {
        let m = supersample(&[1.0, 0.0], 2, 1);
        assert_eq!(m.len(), 2 * SS * SS);
        assert!(m[SS / 2 * 2 * SS]);
        assert!(!m[SS / 2 * 2 * SS + 2 * SS - 1]);
        assert!(supersample(&[0.4], 1, 1).iter().all(|&b| !b));
    }

    #[test]
    fn downsample_averages_cells() {
        let cov = downsample(|i| i % 2 == 0, 1, 1);
        assert_eq!(cov, [0.5]);
    }

    #[test]
    fn distances_reach_outward_and_inward() {
        let (w, h) = (9, 9);
        let mut a = vec![0f32; w * h];
        for y in 3..6 {
            for x in 3..6 {
                a[y * w + x] = 1.0;
            }
        }
        let d = distances(&a, w, h, true, 2.0);
        let centre = (4 * SS + SS / 2) * w * SS + 4 * SS + SS / 2;
        assert!(d.inside[centre]);
        assert_eq!(d.outside[centre], 0.0);
        assert!(d.inward[centre] > 0.0);
    }

    #[test]
    fn blur_preserves_mass_and_symmetry() {
        let (w, h) = (31, 31);
        let mut a = vec![0f32; w * h];
        a[15 * w + 15] = 1.0;
        blur(&mut a, w, h, 2.0, 100.0);
        let sum: f32 = a.iter().sum();
        assert!((sum - 1.0).abs() < 1e-4);
        assert!((a[15 * w + 12] - a[15 * w + 18]).abs() < 1e-7);
        assert!((a[12 * w + 15] - a[15 * w + 12]).abs() < 1e-7);
        assert!(a[15 * w + 15] > a[15 * w + 16]);
    }

    #[test]
    fn blur_respects_truncation() {
        let (w, h) = (21, 1);
        let mut a = vec![0f32; w];
        a[10] = 1.0;
        blur(&mut a, w, h, 3.0, 2.0);
        assert_eq!(a[7], 0.0);
        assert!(a[8] > 0.0);
    }

    #[test]
    fn tiny_sigma_is_a_no_op() {
        let mut a = vec![0.0, 1.0, 0.0];
        blur(&mut a, 3, 1, 0.1, 10.0);
        assert_eq!(a, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn transpose_roundtrip() {
        let src: Vec<f32> = (0..70).map(|i| i as f32).collect();
        let mut t = vec![0f32; 70];
        let mut back = vec![0f32; 70];
        transpose(&src, &mut t, 10, 7);
        assert_eq!(t[3 * 7 + 2], src[2 * 10 + 3]);
        transpose(&t, &mut back, 7, 10);
        assert_eq!(back, src);
    }
}
