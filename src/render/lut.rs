//! Color Lookup adjustments: 3D LUT files (CUBE, 3DL, LOOK) and ICC abstract or device link
//! profiles embedded in the `clrL` block, all reduced to one sampled 3D table.

use crate::color;
use crate::psd::descriptor::Descriptor;

/// Grid size used when an ICC profile is sampled into a table.
const ICC_GRID: usize = 33;

/// A 3D table of RGB outputs, red index fastest.
#[derive(Debug)]
pub(crate) struct Lut3d {
    n: usize,
    data: Vec<[f32; 3]>,
}

impl Lut3d {
    fn from_fn(n: usize, f: impl Fn([f64; 3]) -> [f64; 3]) -> Lut3d {
        let s = |i: usize| i as f64 / (n - 1) as f64;
        let data = (0..n * n * n)
            .map(|i| f([s(i % n), s(i / n % n), s(i / (n * n))]).map(|v| v.clamp(0.0, 1.0) as f32))
            .collect();
        Lut3d { n, data }
    }

    /// Trilinear lookup of straight RGB in 0..=1.
    pub fn apply(&self, c: [f32; 3]) -> [f32; 3] {
        let n = self.n;
        let mut i0 = [0usize; 3];
        let mut t = [0f32; 3];
        for k in 0..3 {
            let x = c[k].clamp(0.0, 1.0) * (n - 1) as f32;
            i0[k] = (x as usize).min(n - 2);
            t[k] = x - i0[k] as f32;
        }
        let at = |r: usize, g: usize, b: usize| self.data[(i0[0] + r) + n * ((i0[1] + g) + n * (i0[2] + b))];
        let mut out = [0f32; 3];
        for (corner, w) in (0..8).map(|k| {
            let (r, g, b) = (k & 1, k >> 1 & 1, k >> 2 & 1);
            let w = [r, g, b].iter().zip(&t).map(|(&o, &t)| if o == 1 { t } else { 1.0 - t }).product::<f32>();
            (at(r, g, b), w)
        }) {
            for ch in 0..3 {
                out[ch] += corner[ch] * w;
            }
        }
        out
    }
}

/// What a Color Lookup layer does.
pub(crate) enum Lookup {
    /// No table chosen yet.
    Identity,
    Table(Lut3d),
}

/// Reads the descriptor of a `clrL` block; `Err` names what could not be used.
pub(crate) fn parse(d: &Descriptor) -> Result<Lookup, &'static str> {
    let kind = d.enumerated("lookupType");
    let data = d.raw("LUT3DFileData");
    let profile = d.raw("profile");
    match kind {
        Some("abstractProfile") | Some("deviceLinkProfile") => {
            let p = profile.ok_or("color lookup without its profile")?;
            icc(p).map(Lookup::Table).ok_or("unsupported color lookup profile")
        }
        _ => {
            let Some(data) = data else {
                return match profile {
                    Some(p) => icc(p).map(Lookup::Table).ok_or("unsupported color lookup profile"),
                    None => Ok(Lookup::Identity),
                };
            };
            let text = String::from_utf8_lossy(data);
            // The table order names the loops from outer to inner: `bgrOrder` runs red fastest.
            let blue_fastest = match d.enumerated("tableOrder") {
                Some("bgrOrder") => Some(false),
                Some("rgbOrder") => Some(true),
                _ => None,
            };
            let bgr_values = d.enumerated("dataOrder") == Some("bgrOrder");
            let lut = match d.enumerated("LUTFormat") {
                Some("LUTFormat3DL") => three_dl(&text, blue_fastest.unwrap_or(true), bgr_values),
                Some("LUTFormatLOOK") => look(&text),
                Some("LUTFormatCUBE") => cube(&text, blue_fastest.unwrap_or(false), bgr_values),
                _ => cube(&text, blue_fastest.unwrap_or(false), bgr_values)
                    .or_else(|| three_dl(&text, blue_fastest.unwrap_or(true), bgr_values))
                    .or_else(|| look(&text)),
            };
            lut.map(Lookup::Table).ok_or("unreadable color lookup table")
        }
    }
}

/// Puts `values` (n³ entries in file order) into red-fastest order.
fn reorder(n: usize, values: Vec<[f32; 3]>, blue_fastest: bool, bgr: bool) -> Option<Lut3d> {
    if n < 2 || values.len() != n * n * n {
        return None;
    }
    let values: Vec<[f32; 3]> = if bgr { values.into_iter().map(|[b, g, r]| [r, g, b]).collect() } else { values };
    let data = if blue_fastest {
        (0..n * n * n)
            .map(|i| {
                let (r, g, b) = (i % n, i / n % n, i / (n * n));
                values[b + n * (g + n * r)]
            })
            .collect()
    } else {
        values
    };
    Some(Lut3d { n, data })
}

/// Adobe/Resolve `.cube`: `LUT_3D_SIZE`, optional domain, then rows of three floats.
fn cube(text: &str, blue_fastest: bool, bgr: bool) -> Option<Lut3d> {
    let mut n = 0;
    let mut values = vec![];
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut words = line.split_whitespace();
        let first = words.next()?;
        let nums = |w: std::str::SplitWhitespace| -> Option<[f32; 3]> {
            let v: Vec<f32> = w.map(str::parse).collect::<Result<_, _>>().ok()?;
            (v.len() == 3).then(|| [v[0], v[1], v[2]])
        };
        match first {
            "LUT_3D_SIZE" => n = words.next()?.parse().ok()?,
            // Inputs outside 0..=1 can't occur here, so a wider domain only matters at its ends.
            "DOMAIN_MIN" | "DOMAIN_MAX" => {
                nums(words)?;
            }
            "LUT_1D_SIZE" => return None,
            _ if first.parse::<f32>().is_ok() => values.push(nums(line.split_whitespace())?),
            _ => {}
        }
    }
    reorder(n, values, blue_fastest, bgr)
}

/// Autodesk `.3dl`: a line of mesh inputs, then integer rows scaled to the output bit depth.
fn three_dl(text: &str, blue_fastest: bool, bgr: bool) -> Option<Lut3d> {
    let rows: Vec<Vec<u32>> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('<'))
        .filter_map(|l| l.split_whitespace().map(str::parse).collect::<Result<Vec<u32>, _>>().ok())
        .collect();
    let mesh = rows.iter().find(|r| r.len() != 3)?;
    let n = mesh.len();
    let values: Vec<&Vec<u32>> = rows.iter().filter(|r| r.len() == 3).collect();
    let max = values.iter().flat_map(|r| r.iter()).copied().max()?;
    let scale = (max.max(1) + 1).next_power_of_two() - 1;
    let values = values.iter().map(|r| [0, 1, 2].map(|c| r[c] as f32 / scale as f32)).collect();
    reorder(n, values, blue_fastest, bgr)
}

/// SpeedGrade/IRIDAS `.look`: XML with the size and hex-encoded little-endian floats.
fn look(text: &str) -> Option<Lut3d> {
    let lut = &text[text.find("<LUT>")?..];
    let tag = |name: &str| -> Option<&str> {
        let start = lut.find(&format!("<{name}>"))? + name.len() + 2;
        let end = start + lut[start..].find(&format!("</{name}>"))?;
        Some(lut[start..end].trim().trim_matches('"'))
    };
    let n: usize = tag("size")?.parse().ok()?;
    let data = tag("data")?;
    let hex: Vec<u8> = data.bytes().filter(u8::is_ascii_hexdigit).collect();
    let floats: Vec<f32> = hex
        .chunks_exact(8)
        .map(|c| {
            let b: Vec<u8> = c
                .chunks_exact(2)
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap_or("00"), 16).unwrap_or(0))
                .collect();
            f32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
        .collect();
    let values = floats.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
    reorder(n, values, false, false)
}

/// Samples an ICC abstract (Lab to Lab) or device link (RGB to RGB) profile into a table.
fn icc(p: &[u8]) -> Option<Lut3d> {
    let class = p.get(12..16)?;
    let input = p.get(16..20)?;
    let pcs = p.get(20..24)?;
    let lut = IccLut::parse(p, find_tag(p, b"A2B0")?)?;
    if lut.inputs != 3 || lut.outputs != 3 {
        return None;
    }
    match (class, input) {
        (b"abst", b"Lab ") if pcs == b"Lab " => Some(Lut3d::from_fn(ICC_GRID, |rgb| {
            let lab = color::rgb_to_lab(rgb);
            let out = lut.eval(lut.encode_lab(lab));
            let lab = lut.decode_lab(out);
            color::lab_to_rgb(lab[0], lab[1], lab[2]).map(f64::from)
        })),
        (b"link", b"RGB ") if pcs == b"RGB " => Some(Lut3d::from_fn(ICC_GRID, |rgb| lut.eval(rgb))),
        _ => None,
    }
}

fn be16(p: &[u8], at: usize) -> Option<u16> {
    p.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn be32(p: &[u8], at: usize) -> Option<u32> {
    p.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn find_tag(p: &[u8], sig: &[u8; 4]) -> Option<usize> {
    let count = be32(p, 128)? as usize;
    (0..count.min(1000)).find_map(|i| {
        let at = 132 + 12 * i;
        (p.get(at..at + 4)? == sig).then(|| be32(p, at + 4).map(|o| o as usize)).flatten()
    })
}

/// A one-dimensional curve: a sampled table or a parametric function.
enum Curve {
    Table(Vec<f64>),
    Param(u16, Vec<f64>),
}

impl Curve {
    fn identity() -> Curve {
        Curve::Table(vec![])
    }

    /// A `curv` or `para` element at `at`; also returns its padded size.
    fn parse(p: &[u8], at: usize) -> Option<(Curve, usize)> {
        match p.get(at..at + 4)? {
            b"curv" => {
                let n = be32(p, at + 8)? as usize;
                let t: Vec<f64> = (0..n).map(|i| be16(p, at + 12 + 2 * i).map(|v| v as f64)).collect::<Option<_>>()?;
                let size = (12 + 2 * n).next_multiple_of(4);
                Some((
                    match n {
                        0 => Curve::identity(),
                        1 => Curve::Param(0, vec![t[0] / 256.0]),
                        _ => Curve::Table(t.into_iter().map(|v| v / 65535.0).collect()),
                    },
                    size,
                ))
            }
            b"para" => {
                let kind = be16(p, at + 8)?;
                let count = [1, 3, 4, 5, 7].get(kind as usize).copied()?;
                let g: Vec<f64> = (0..count)
                    .map(|i| be32(p, at + 12 + 4 * i).map(|v| v as i32 as f64 / 65536.0))
                    .collect::<Option<_>>()?;
                Some((Curve::Param(kind, g), (12 + 4 * count).next_multiple_of(4)))
            }
            _ => None,
        }
    }

    fn eval(&self, x: f64) -> f64 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Curve::Table(t) if t.len() < 2 => x,
            Curve::Table(t) => interp(t, x),
            Curve::Param(kind, g) => {
                let gamma = g[0];
                let y = match kind {
                    0 => x.powf(gamma),
                    1 => {
                        if x >= -g[2] / g[1] {
                            (g[1] * x + g[2]).max(0.0).powf(gamma)
                        } else {
                            0.0
                        }
                    }
                    2 => {
                        if x >= -g[2] / g[1] {
                            (g[1] * x + g[2]).max(0.0).powf(gamma) + g[3]
                        } else {
                            g[3]
                        }
                    }
                    3 => {
                        if x >= g[4] {
                            (g[1] * x + g[2]).max(0.0).powf(gamma)
                        } else {
                            g[3] * x
                        }
                    }
                    _ => {
                        if x >= g[4] {
                            (g[1] * x + g[2]).max(0.0).powf(gamma) + g[5]
                        } else {
                            g[3] * x + g[6]
                        }
                    }
                };
                y.clamp(0.0, 1.0)
            }
        }
    }
}

fn interp(t: &[f64], x: f64) -> f64 {
    let f = x * (t.len() - 1) as f64;
    let i = (f as usize).min(t.len() - 2);
    t[i] + (t[i + 1] - t[i]) * (f - i as f64)
}

/// The A2B0 transform of an ICC profile: `lut8`/`lut16` (`mft1`/`mft2`) or `lutAtoB` (`mAB `).
struct IccLut {
    inputs: usize,
    outputs: usize,
    a: Vec<Curve>,
    grid: Vec<usize>,
    clut: Vec<f64>,
    m: Vec<Curve>,
    /// 3x3 matrix and offset applied after the M curves (`mAB ` only).
    matrix: Option<[f64; 12]>,
    b: Vec<Curve>,
    /// Lab is encoded the legacy 16-bit way (`mft2`).
    legacy_lab: bool,
}

impl IccLut {
    fn parse(p: &[u8], at: usize) -> Option<IccLut> {
        let ty = p.get(at..at + 4)?;
        let inputs = *p.get(at + 8)? as usize;
        let outputs = *p.get(at + 9)? as usize;
        if !(1..=4).contains(&inputs) || !(1..=4).contains(&outputs) {
            return None;
        }
        match ty {
            b"mft1" | b"mft2" => {
                let g = *p.get(at + 10)? as usize;
                let wide = ty == b"mft2";
                let (n_in, n_out, mut pos) = if wide {
                    (be16(p, at + 48)? as usize, be16(p, at + 50)? as usize, at + 52)
                } else {
                    (256, 256, at + 48)
                };
                let size = if wide { 2 } else { 1 };
                let max = if wide { 65535.0 } else { 255.0 };
                let mut read = |count: usize| -> Option<Vec<f64>> {
                    let v = (0..count)
                        .map(|i| {
                            let o = pos + i * size;
                            if wide {
                                be16(p, o).map(|v| v as f64 / max)
                            } else {
                                p.get(o).map(|&v| v as f64 / max)
                            }
                        })
                        .collect::<Option<Vec<f64>>>()?;
                    pos += count * size;
                    Some(v)
                };
                let a = (0..inputs).map(|_| read(n_in).map(Curve::Table)).collect::<Option<Vec<_>>>()?;
                let grid = vec![g; inputs];
                let clut = read(g.checked_pow(inputs as u32)? * outputs)?;
                let b = (0..outputs).map(|_| read(n_out).map(Curve::Table)).collect::<Option<Vec<_>>>()?;
                Some(IccLut { inputs, outputs, a, grid, clut, m: vec![], matrix: None, b, legacy_lab: wide })
            }
            b"mAB " => {
                let off = |k: usize| be32(p, at + 12 + 4 * k).map(|v| v as usize);
                let (ob, om, omc, oc, oa) = (off(0)?, off(1)?, off(2)?, off(3)?, off(4)?);
                let curves = |o: usize, n: usize| -> Option<Vec<Curve>> {
                    if o == 0 {
                        return Some((0..n).map(|_| Curve::identity()).collect());
                    }
                    let mut pos = at + o;
                    (0..n)
                        .map(|_| {
                            let (c, size) = Curve::parse(p, pos)?;
                            pos += size;
                            Some(c)
                        })
                        .collect()
                };
                let b = curves(ob, outputs)?;
                let m = curves(omc, outputs)?;
                let a = curves(oa, inputs)?;
                let matrix = if om != 0 {
                    let mut v = [0.0; 12];
                    for (k, x) in v.iter_mut().enumerate() {
                        *x = be32(p, at + om + 4 * k)? as i32 as f64 / 65536.0;
                    }
                    Some(v)
                } else {
                    None
                };
                let (grid, clut) = if oc != 0 {
                    let c = at + oc;
                    let grid: Vec<usize> =
                        (0..inputs).map(|i| p.get(c + i).map(|&g| g as usize)).collect::<Option<_>>()?;
                    let precision = *p.get(c + 16)? as usize;
                    let count = grid.iter().product::<usize>() * outputs;
                    let data = c + 20;
                    let clut = (0..count)
                        .map(|i| match precision {
                            1 => p.get(data + i).map(|&v| v as f64 / 255.0),
                            _ => be16(p, data + 2 * i).map(|v| v as f64 / 65535.0),
                        })
                        .collect::<Option<Vec<_>>>()?;
                    (grid, clut)
                } else {
                    (vec![], vec![])
                };
                Some(IccLut { inputs, outputs, a, grid, clut, m, matrix, b, legacy_lab: false })
            }
            _ => None,
        }
    }

    /// Lab to the transform's 0..=1 input encoding.
    fn encode_lab(&self, lab: [f64; 3]) -> [f64; 3] {
        if self.legacy_lab {
            [lab[0] / 100.0 * 65280.0 / 65535.0, (lab[1] + 128.0) * 256.0 / 65535.0, (lab[2] + 128.0) * 256.0 / 65535.0]
        } else {
            [lab[0] / 100.0, (lab[1] + 128.0) / 255.0, (lab[2] + 128.0) / 255.0]
        }
    }

    fn decode_lab(&self, v: [f64; 3]) -> [f64; 3] {
        if self.legacy_lab {
            [v[0] * 65535.0 / 65280.0 * 100.0, v[1] * 65535.0 / 256.0 - 128.0, v[2] * 65535.0 / 256.0 - 128.0]
        } else {
            [v[0] * 100.0, v[1] * 255.0 - 128.0, v[2] * 255.0 - 128.0]
        }
    }

    fn eval(&self, x: [f64; 3]) -> [f64; 3] {
        let mut v: Vec<f64> = x.iter().zip(&self.a).map(|(&x, c)| c.eval(x)).collect();
        if !self.clut.is_empty() {
            v = self.clut_lookup(&v);
        }
        v = v.iter().zip(self.m.iter().chain(std::iter::repeat(&Curve::identity()))).map(|(&x, c)| c.eval(x)).collect();
        if let Some(m) = &self.matrix {
            let (a, b, c) = (v[0], v[1], v[2]);
            v = (0..3)
                .map(|r| (m[3 * r] * a + m[3 * r + 1] * b + m[3 * r + 2] * c + m[9 + r]).clamp(0.0, 1.0))
                .collect();
        }
        let out: Vec<f64> = v.iter().zip(&self.b).map(|(&x, c)| c.eval(x)).collect();
        [out[0], out[1], out[2]]
    }

    /// Multilinear interpolation in the color lookup table, first input slowest.
    fn clut_lookup(&self, x: &[f64]) -> Vec<f64> {
        let d = self.inputs;
        let mut base = 0;
        let mut frac = vec![0.0; d];
        let mut strides = vec![self.outputs; d];
        for i in (0..d.saturating_sub(1)).rev() {
            strides[i] = strides[i + 1] * self.grid[i + 1];
        }
        for i in 0..d {
            let g = self.grid[i].max(2);
            let f = x[i].clamp(0.0, 1.0) * (g - 1) as f64;
            let k = (f as usize).min(g - 2);
            frac[i] = f - k as f64;
            base += k * strides[i];
        }
        let mut out = vec![0.0; self.outputs];
        for corner in 0..1usize << d {
            let mut w = 1.0;
            let mut at = base;
            for i in 0..d {
                if corner >> i & 1 == 1 {
                    w *= frac[i];
                    at += strides[i];
                } else {
                    w *= 1.0 - frac[i];
                }
            }
            if w == 0.0 {
                continue;
            }
            for (o, v) in out.iter_mut().enumerate() {
                *v += w * self.clut.get(at + o).copied().unwrap_or(0.0);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn cube_tables_interpolate_red_fastest() {
        // Swaps red and blue.
        let mut text = String::from("TITLE \"swap\"\nLUT_3D_SIZE 2\n");
        for i in 0..8 {
            let (r, g, b) = (i & 1, i >> 1 & 1, i >> 2 & 1);
            text += &format!("{b} {g} {r}\n");
        }
        let lut = cube(&text, false, false).unwrap();
        assert!(near(lut.apply([1.0, 0.5, 0.0]), [0.0, 0.5, 1.0]));
        assert!(near(lut.apply([0.25, 0.0, 0.75]), [0.75, 0.0, 0.25]));
    }

    #[test]
    fn three_dl_is_blue_fastest_and_scaled() {
        // Identity with 10-bit outputs.
        let mut text = String::from("0 1023\n");
        for r in 0..2 {
            for g in 0..2 {
                for b in 0..2 {
                    text += &format!("{} {} {}\n", r * 1023, g * 1023, b * 1023);
                }
            }
        }
        let lut = three_dl(&text, true, false).unwrap();
        assert!(near(lut.apply([0.2, 0.6, 0.9]), [0.2, 0.6, 0.9]));
    }

    #[test]
    fn look_reads_hex_floats() {
        let mut hex = String::new();
        for i in 0..8 {
            for c in [i & 1, i >> 1 & 1, i >> 2 & 1] {
                let v = 1.0 - c as f32;
                hex += &v.to_le_bytes().iter().map(|b| format!("{b:02X}")).collect::<String>();
            }
        }
        let text = format!("<?xml version=\"1.0\"?><look><LUT><size>\"2\"</size><data>\"{hex}\"</data></LUT></look>");
        let lut = look(&text).unwrap();
        assert!(near(lut.apply([1.0, 0.0, 0.25]), [0.0, 1.0, 0.75]));
    }

    #[test]
    fn mab_device_link_with_parametric_curves() {
        // An inverting RGB device link: gamma-1 A curves, a 2x2x2 CLUT, no M, matrix or B curves.
        let mut p = vec![0u8; 128];
        p[12..16].copy_from_slice(b"link");
        p[16..20].copy_from_slice(b"RGB ");
        p[20..24].copy_from_slice(b"RGB ");
        p.extend(1u32.to_be_bytes());
        p.extend(b"A2B0");
        p.extend(144u32.to_be_bytes());
        p.extend(0u32.to_be_bytes());
        let lut = p.len();
        p.extend(b"mAB \0\0\0\0");
        p.extend([3, 3, 0, 0]);
        let curve: Vec<u8> = [&b"para"[..], &[0; 4], &[0, 0, 0, 0], &65536u32.to_be_bytes()].concat();
        let b_off = 32usize;
        let a_off = b_off + 3 * curve.len();
        let clut_off = a_off + 3 * curve.len();
        for o in [b_off, 0, 0, clut_off, a_off] {
            p.extend((o as u32).to_be_bytes());
        }
        for _ in 0..6 {
            p.extend(&curve);
        }
        let mut grid = [0u8; 20];
        grid[..3].copy_from_slice(&[2, 2, 2]);
        grid[16] = 1;
        p.extend(grid);
        for i in 0..8 {
            // First input slowest.
            let (r, g, b) = (i >> 2 & 1, i >> 1 & 1, i & 1);
            p.extend([255 - 255 * r as u8, 255 - 255 * g as u8, 255 - 255 * b as u8]);
        }
        assert_eq!(lut, 144);
        let t = icc(&p).unwrap();
        assert!(near(t.apply([0.25, 0.5, 1.0]), [0.75, 0.5, 0.0]));
    }
}
