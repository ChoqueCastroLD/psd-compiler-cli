//! Color conversion: embedded ICC profiles to sRGB, CMYK, CIE L*a*b*, HSB and descriptor colors.

use std::sync::Arc;

use moxcms::{ColorProfile, Layout, Transform8BitExecutor, TransformOptions};

use crate::psd::descriptor::Descriptor;
use crate::psd::{ColorMode, Document};

/// How document colors become sRGB.
///
/// RGB, grayscale and similar documents composite in their own space and are converted once at the
/// end ([`ColorSpace::finish`]). CMYK and Lab pixels are converted as they are decoded, so the
/// compositor always works on three channels.
#[derive(Clone, Default)]
pub(crate) struct ColorSpace {
    cmyk: Option<Arc<Transform8BitExecutor>>,
    /// sRGB to CMYK ink, for writing CMYK documents.
    to_cmyk: Option<Arc<Transform8BitExecutor>>,
    output: Option<(Arc<Transform8BitExecutor>, bool)>,
    /// 32-bit documents composite in linear light, as their pixels and descriptor colors are.
    linear: bool,
    /// Duotone appearance of each gray level, as sRGB.
    duotone: Option<Arc<Vec<[u8; 3]>>>,
    /// CMYK documents composite in two passes, as stored (255 - ink) values: the cyan, magenta
    /// and yellow plates (0), then black with the color's luminance in the other two channels
    /// (1), for adjustments like Threshold that look at the whole color. `None` converts
    /// CMYK to sRGB as it is decoded.
    plane: Option<u8>,
}

fn options() -> TransformOptions {
    TransformOptions { prefer_fixed_point: false, ..TransformOptions::default() }
}

fn is_identity(t: &Transform8BitExecutor) -> bool {
    let src: Vec<u8> = (0..=255u8).flat_map(|v| [v, v, v, 255, 255 - v, v / 2, 0, 255]).collect();
    let mut dst = vec![0u8; src.len()];
    t.transform(&src, &mut dst).is_ok() && src.iter().zip(&dst).all(|(a, b)| a.abs_diff(*b) <= 1)
}

impl ColorSpace {
    pub fn new(doc: &Document) -> ColorSpace {
        let mut cs = ColorSpace { linear: doc.depth == 32, ..ColorSpace::default() };
        if doc.color_mode == ColorMode::Duotone && !doc.duotone.is_empty() {
            // The ink table already gives the final colors.
            cs.duotone = Some(Arc::new(doc.duotone.clone()));
            return cs;
        }
        let Some(profile) = doc.icc_profile.as_deref().and_then(|b| ColorProfile::new_from_slice(b).ok()) else {
            return cs;
        };
        // 32-bit pixels are linear light, already encoded to sRGB as they are decoded.
        if doc.depth == 32 {
            return cs;
        }
        let srgb = ColorProfile::new_srgb();
        match doc.color_mode {
            ColorMode::Cmyk => {
                cs.cmyk = profile.create_transform_8bit(Layout::Rgba, &srgb, Layout::Rgb, options()).ok();
                cs.to_cmyk = srgb.create_transform_8bit(Layout::Rgb, &profile, Layout::Rgba, options()).ok();
            }
            ColorMode::Rgb => {
                cs.output = profile
                    .create_transform_8bit(Layout::Rgba, &srgb, Layout::Rgba, options())
                    .ok()
                    .filter(|t| !is_identity(t.as_ref()))
                    .map(|t| (t, false));
            }
            ColorMode::Grayscale | ColorMode::Duotone => {
                cs.output = profile
                    .create_transform_8bit(Layout::GrayAlpha, &srgb, Layout::Rgba, options())
                    .ok()
                    .map(|t| (t, true));
            }
            _ => {}
        }
        cs
    }

    /// The same transforms working on one set of CMYK plates (see [`ColorSpace::plane`]).
    pub fn with_plane(&self, plane: u8) -> ColorSpace {
        ColorSpace { plane: Some(plane), ..self.clone() }
    }

    /// The CMYK plates this pass composites, if it works on plates.
    pub fn plane(&self) -> Option<u8> {
        self.plane
    }

    /// Working values on plate set `plane` of one CMYK color given as ink (0 = none).
    ///
    /// The luminance is that of the plain conversion `(1 - ink) * (1 - black)`, which is what
    /// Photoshop's Threshold measures.
    pub fn plates(plane: u8, ink: [u8; 4]) -> [f32; 3] {
        let v = |c: usize| (255 - ink[c]) as f32 / 255.0;
        if plane == 0 {
            [v(0), v(1), v(2)]
        } else {
            let l = crate::blend::lum([v(0), v(1), v(2)]) * v(3);
            [v(3), l, l]
        }
    }

    /// Converts an sRGB color in 0..=1 to working values.
    pub fn srgb_to_working(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.linear {
            return rgb.map(|v| srgb_decode(v as f64) as f32);
        }
        let Some(plane) = self.plane else { return rgb };
        let rgb8 = rgb.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
        let mut ink = [0u8; 4];
        self.rgb_to_cmyk(&rgb8, &mut ink);
        Self::plates(plane, ink)
    }

    /// Encodes premultiplied linear-light pixels (32-bit documents) to sRGB in place.
    pub fn encode_linear(&self, px: &mut [f32]) {
        if !self.linear {
            return;
        }
        for p in px.chunks_exact_mut(4) {
            if p[3] > 0.0 {
                for c in 0..3 {
                    p[c] = srgb_encode((p[c] / p[3]) as f64) as f32 * p[3];
                }
            }
        }
    }

    /// Converts straight sRGB RGBA8 pixels to working values in place (CMYK plates only; see
    /// [`ColorSpace::srgb_to_working`] for linear light).
    pub fn srgb_rgba8_to_working(&self, rgba: &mut [u8]) {
        let Some(plane) = self.plane else { return };
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let mut ink = vec![0u8; rgb.len() / 3 * 4];
        self.rgb_to_cmyk(&rgb, &mut ink);
        for (p, k) in rgba.chunks_exact_mut(4).zip(ink.chunks_exact(4)) {
            let v = Self::plates(plane, [k[0], k[1], k[2], k[3]]);
            for c in 0..3 {
                p[c] = (v[c] * 255.0).round() as u8;
            }
        }
    }

    /// Whether the document works in linear light (32-bit).
    pub fn is_linear(&self) -> bool {
        self.linear
    }

    /// Converts interleaved CMYK ink values (0 = no ink) to sRGB.
    pub fn cmyk_to_rgb(&self, cmyk: &[u8], rgb: &mut [u8]) {
        if let Some(t) = &self.cmyk {
            if t.transform(cmyk, rgb).is_ok() {
                return;
            }
        }
        for (s, d) in cmyk.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
            let k = 255 - s[3] as u32;
            for c in 0..3 {
                d[c] = ((255 - s[c] as u32) * k / 255) as u8;
            }
        }
    }

    /// Converts interleaved sRGB to CMYK ink values (0 = no ink): through the document's profile,
    /// else the naive inverse of [`ColorSpace::cmyk_to_rgb`].
    pub fn rgb_to_cmyk(&self, rgb: &[u8], cmyk: &mut [u8]) {
        if let Some(t) = &self.to_cmyk {
            if t.transform(rgb, cmyk).is_ok() {
                return;
            }
        }
        for (s, d) in rgb.chunks_exact(3).zip(cmyk.chunks_exact_mut(4)) {
            let max = s.iter().copied().max().unwrap_or(0) as u32;
            d[3] = (255 - max) as u8;
            for c in 0..3 {
                d[c] = (s[c] as u32 * 255 + max / 2).checked_div(max).map_or(0, |v| (255 - v) as u8);
            }
        }
    }

    /// One CMYK color with components in 0..=1 of ink.
    pub fn cmyk(&self, c: f64, m: f64, y: f64, k: f64) -> [f32; 3] {
        let ink = [c, m, y, k].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
        if let Some(plane) = self.plane {
            return Self::plates(plane, ink);
        }
        let mut rgb = [0u8; 3];
        self.cmyk_to_rgb(&ink, &mut rgb);
        rgb.map(|v| v as f32 / 255.0)
    }

    /// Converts a finished straight-alpha RGBA8 image from the document's space to sRGB.
    pub fn finish(&self, rgba: &mut [u8]) {
        if let Some(table) = &self.duotone {
            for p in rgba.chunks_exact_mut(4) {
                let c = table[p[0] as usize];
                p[..3].copy_from_slice(&c);
            }
            return;
        }
        let Some((t, gray)) = &self.output else { return };
        let src: Vec<u8> =
            if *gray { rgba.chunks_exact(4).flat_map(|p| [p[0], p[3]]).collect() } else { rgba.to_vec() };
        let _ = t.transform(&src, rgba);
    }
}

/// The look of a duotone (or monotone, tritone, quadtone) image: the sRGB color of each gray level.
///
/// Photoshop stores a 256-entry Lab preview of the inks in image resource 1066 (`preview`); without
/// it the inks and their curves from the color mode data are mixed on white paper. Empty when
/// neither can be read, e.g. inks from a color book with no preview.
pub(crate) fn duotone(color_data: &[u8], preview: Option<&[u8]>) -> Vec<[u8; 3]> {
    let be = |b: &[u8], i: usize| b.get(i..i + 2).map(|v| u16::from_be_bytes([v[0], v[1]]));
    if let Some(p) = preview {
        let count = be(p, 2).unwrap_or(0) as usize;
        let at = 4 + 10 * count;
        if let Some(n) = be(p, at).filter(|&n| n == 256) {
            if let Some(lab) = p.get(at + 2..at + 2 + 3 * n as usize) {
                return lab
                    .chunks_exact(3)
                    .map(|c| {
                        let rgb = lab_to_rgb(c[0] as f64 / 255.0 * 100.0, c[1] as f64 - 128.0, c[2] as f64 - 128.0);
                        rgb.map(|v| (v * 255.0).round() as u8)
                    })
                    .collect();
            }
        }
    }
    let inks = be(color_data, 2).unwrap_or(0).min(4) as usize;
    if inks == 0 || color_data.len() < 4 + 40 + 256 + 112 {
        return vec![];
    }
    let mut colors = vec![];
    for i in 0..inks {
        let c = &color_data[4 + 10 * i..14 + 10 * i];
        let v = |k: usize| u16::from_be_bytes([c[2 + 2 * k], c[3 + 2 * k]]) as f64;
        let rgb = match u16::from_be_bytes([c[0], c[1]]) {
            0 => [v(0), v(1), v(2)].map(|x| (x / 65535.0) as f32),
            2 => ColorSpace::default().cmyk(
                1.0 - v(0) / 65535.0,
                1.0 - v(1) / 65535.0,
                1.0 - v(2) / 65535.0,
                1.0 - v(3) / 65535.0,
            ),
            7 => lab_to_rgb(v(0) / 100.0, v(1) as i16 as f64 / 100.0, v(2) as i16 as f64 / 100.0),
            8 => [(1.0 - v(0) / 10000.0) as f32; 3],
            _ => return vec![],
        };
        colors.push(rgb);
    }
    // Transfer curves: 13 points at these dot percentages, in tenths of a percent, -1 when unset.
    const AT: [f64; 13] = [0.0, 5.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 95.0, 100.0];
    let curves: Vec<Vec<(f64, f64)>> = (0..inks)
        .map(|i| {
            let base = 4 + 40 + 256 + 28 * i;
            let mut pts: Vec<(f64, f64)> = (0..13)
                .filter_map(|k| {
                    let v = be(color_data, base + 2 * k)? as i16;
                    (v >= 0).then(|| (AT[k] / 100.0, v as f64 / 1000.0))
                })
                .collect();
            if pts.len() < 2 {
                pts = vec![(0.0, 0.0), (1.0, 1.0)];
            }
            pts
        })
        .collect();
    let curve = |pts: &[(f64, f64)], t: f64| {
        let i = pts.windows(2).position(|w| t <= w[1].0).unwrap_or(pts.len() - 2);
        let ((x0, y0), (x1, y1)) = (pts[i], pts[i + 1]);
        let k = if x1 > x0 { ((t - x0) / (x1 - x0)).clamp(0.0, 1.0) } else { 1.0 };
        y0 + (y1 - y0) * k
    };
    (0..256)
        .map(|g| {
            let dot = 1.0 - g as f64 / 255.0;
            let mut out = [1.0f64; 3];
            for (c, pts) in colors.iter().zip(&curves) {
                let k = curve(pts, dot).clamp(0.0, 1.0);
                for ch in 0..3 {
                    out[ch] *= 1.0 - k * (1.0 - c[ch] as f64);
                }
            }
            out.map(|v| (v * 255.0).round() as u8)
        })
        .collect()
}

/// CIE L*a*b* (D50, L in 0..=100) to sRGB in 0..=1.
pub(crate) fn lab_to_rgb(l: f64, a: f64, b: f64) -> [f32; 3] {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let inv = |t: f64| if t > 6.0 / 29.0 { t * t * t } else { 3.0 * (6.0f64 / 29.0).powi(2) * (t - 4.0 / 29.0) };
    let (x, y, z) = (0.96422 * inv(fx), inv(fy), 0.82521 * inv(fz));
    let lin = [
        3.1338561 * x - 1.6168667 * y - 0.4906146 * z,
        -0.9787684 * x + 1.9161415 * y + 0.0334540 * z,
        0.0719453 * x - 0.2289914 * y + 1.4052427 * z,
    ];
    lin.map(|v| srgb_encode(v) as f32)
}

/// sRGB in 0..=1 to CIE L*a*b* (D50); the inverse of [`lab_to_rgb`].
pub(crate) fn rgb_to_lab(rgb: [f64; 3]) -> [f64; 3] {
    let [r, g, b] = rgb.map(srgb_decode);
    let x = 0.4360747 * r + 0.3850649 * g + 0.1430804 * b;
    let y = 0.2225045 * r + 0.7168786 * g + 0.0606169 * b;
    let z = 0.0139322 * r + 0.0971045 * g + 0.7141733 * b;
    let f =
        |t: f64| if t > (6.0f64 / 29.0).powi(3) { t.cbrt() } else { t / (3.0 * (6.0f64 / 29.0).powi(2)) + 4.0 / 29.0 };
    let (fx, fy, fz) = (f(x / 0.96422), f(y), f(z / 0.82521));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Inverse of [`srgb_encode`].
pub(crate) fn srgb_decode(v: f64) -> f64 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// The sRGB transfer curve: linear light to encoded, in 0..=1.
pub(crate) fn srgb_encode(v: f64) -> f64 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

pub(crate) fn hsb_to_rgb(h: f64, s: f64, v: f64) -> [f64; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let i = h.floor();
    let f = h - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    match i as i32 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// Reads a color object (`RGBC`, `Grsc`, `HSBC`, `CMYC`, `LbCl`) as sRGB.
pub(crate) fn from_object(c: &Descriptor, cs: &ColorSpace) -> Option<[f32; 3]> {
    let rgb = if let (Some(r), Some(g), Some(b)) = (c.num("Rd  "), c.num("Grn "), c.num("Bl  ")) {
        [r / 255.0, g / 255.0, b / 255.0]
    } else if let (Some(r), Some(g), Some(b)) = (c.num("redFloat"), c.num("greenFloat"), c.num("blueFloat")) {
        [r, g, b]
    } else if let Some(g) = c.num("Gry ") {
        [1.0 - g / 100.0; 3]
    } else if let (Some(h), Some(s), Some(b)) = (c.num("H   "), c.num("Strt"), c.num("Brgh")) {
        hsb_to_rgb(h / 360.0, s / 100.0, b / 100.0)
    } else if let (Some(cy), Some(m), Some(y), Some(k)) = (c.num("Cyn "), c.num("Mgnt"), c.num("Ylw "), c.num("Blck")) {
        return Some(cs.cmyk(cy / 100.0, m / 100.0, y / 100.0, k / 100.0));
    } else if let (Some(l), Some(a), Some(b)) = (c.num("Lmnc"), c.num("A   "), c.num("B   ")) {
        return Some(cs.srgb_to_working(lab_to_rgb(l, a, b)));
    } else {
        return None;
    };
    let rgb = rgb.map(|v| v.clamp(0.0, 1.0) as f32);
    // 32-bit documents give linear-light values already.
    Some(if cs.linear { rgb } else { cs.srgb_to_working(rgb) })
}

/// The color stored under `key` (usually `Clr `), black when absent.
pub(crate) fn from_key(d: &Descriptor, key: &str, cs: &ColorSpace) -> [f32; 3] {
    d.desc(key).and_then(|c| from_object(c, cs)).unwrap_or([0.0; 3])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psd::descriptor::Value;

    fn desc(items: Vec<(&str, Value)>) -> Descriptor {
        Descriptor { class: String::new(), items: items.into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
    }

    #[test]
    fn lab_white_black_and_red() {
        let near = |a: [f32; 3], b: [f32; 3]| a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 0.01);
        assert!(near(lab_to_rgb(100.0, 0.0, 0.0), [1.0; 3]));
        assert!(near(lab_to_rgb(0.0, 0.0, 0.0), [0.0; 3]));
        let red = lab_to_rgb(54.29, 80.81, 69.89);
        assert!(red[0] > 0.98 && red[1] < 0.05 && red[2] < 0.05, "{red:?}");
    }

    #[test]
    fn lab_roundtrip() {
        for rgb in [[0.2, 0.5, 0.9], [1.0, 0.0, 0.0], [0.0; 3], [1.0; 3], [0.7, 0.7, 0.1]] {
            let lab = rgb_to_lab(rgb);
            let back = lab_to_rgb(lab[0], lab[1], lab[2]);
            assert!(back.iter().zip(&rgb).all(|(a, b)| (*a as f64 - b).abs() < 2e-3), "{rgb:?} -> {lab:?} -> {back:?}");
        }
    }

    #[test]
    fn duotone_from_preview_or_inks() {
        // Version, one ink (Lab), then the 256-entry Lab table.
        let mut preview = vec![0, 1, 0, 1, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0];
        preview.extend((0..256).flat_map(|i| [i as u8, 128, 128]));
        let t = duotone(&[], Some(&preview));
        assert_eq!((t[0], t[255]), ([0; 3], [255; 3]));

        // Monotone in pure red with a linear curve.
        let mut data = vec![0, 1, 0, 1, 0, 0, 255, 255, 0, 0, 0, 0, 0, 0];
        data.extend([255; 30]);
        data.extend([0; 256]);
        let mut curve: Vec<u8> =
            [0i16, -1, -1, -1, -1, -1, 500, -1, -1, -1, -1, -1, 1000, 0].iter().flat_map(|v| v.to_be_bytes()).collect();
        curve.extend(std::iter::repeat_n(255, 28 * 3));
        data.extend(curve);
        data.extend([0; 2 + 110]);
        let t = duotone(&data, None);
        assert_eq!(t[0], [255, 0, 0]);
        assert_eq!(t[255], [255, 255, 255]);
        assert_eq!(t[128][0], 255);
        assert!((126..=129).contains(&t[128][1]), "{:?}", t[128]);
    }

    #[test]
    fn naive_cmyk_roundtrip() {
        let cs = ColorSpace::default();
        let rgb = [200u8, 30, 90, 0, 0, 0, 255, 255, 255];
        let (mut ink, mut back) = ([0u8; 12], [0u8; 9]);
        cs.rgb_to_cmyk(&rgb, &mut ink);
        cs.cmyk_to_rgb(&ink, &mut back);
        assert!(rgb.iter().zip(&back).all(|(a, b)| a.abs_diff(*b) <= 1), "{back:?}");
    }

    #[test]
    fn naive_cmyk_without_profile() {
        let cs = ColorSpace::default();
        assert_eq!(cs.cmyk(0.0, 0.0, 0.0, 0.0), [1.0; 3]);
        assert_eq!(cs.cmyk(1.0, 0.0, 0.0, 0.0), [0.0, 1.0, 1.0]);
        assert_eq!(cs.cmyk(0.0, 0.0, 0.0, 1.0), [0.0; 3]);
    }

    #[test]
    fn color_objects() {
        let cs = ColorSpace::default();
        let gray = desc(vec![("Gry ", Value::Number(100.0))]);
        assert_eq!(from_object(&gray, &cs), Some([0.0; 3]));
        let hsb =
            desc(vec![("H   ", Value::Number(120.0)), ("Strt", Value::Number(100.0)), ("Brgh", Value::Number(100.0))]);
        assert_eq!(from_object(&hsb, &cs), Some([0.0, 1.0, 0.0]));
        assert_eq!(from_object(&desc(vec![]), &cs), None);
    }
}
