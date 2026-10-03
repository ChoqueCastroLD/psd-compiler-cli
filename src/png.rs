//! Parallel PNG encoder.
//!
//! Rows are filtered and deflated in independent strips. Every strip but the last ends on a sync
//! flush, so the compressed pieces concatenate into one valid zlib stream (the pigz technique);
//! the Adler-32 checksums of the strips are combined arithmetically.

use rayon::prelude::*;

const STRIP_ROWS: usize = 64;
const IDAT_CHUNK: usize = 1 << 20;
const MIN_SPARE: usize = 4096;

/// Pixel layout of 8-bit samples passed to [`encode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorType {
    /// One gray sample per pixel.
    Gray,
    /// Red, green, blue.
    Rgb,
    /// Red, green, blue, straight alpha.
    Rgba,
}

impl ColorType {
    fn channels(self) -> usize {
        match self {
            ColorType::Gray => 1,
            ColorType::Rgb => 3,
            ColorType::Rgba => 4,
        }
    }

    fn code(self) -> u8 {
        match self {
            ColorType::Gray => 0,
            ColorType::Rgb => 2,
            ColorType::Rgba => 6,
        }
    }
}

const ADLER_MOD: u32 = 65521;
const ADLER_NMAX: usize = 5552;

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(ADLER_NMAX) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= ADLER_MOD;
        b %= ADLER_MOD;
    }
    (b << 16) | a
}

/// Adler-32 of `A ++ B` from the checksums of `A` and `B` and the length of `B` (zlib's adler32_combine).
fn adler32_combine(a1: u32, a2: u32, len2: u64) -> u32 {
    let m = ADLER_MOD as u64;
    let rem = len2 % m;
    let mut s1 = (a1 & 0xffff) as u64;
    let mut s2 = (rem * s1) % m;
    s1 += (a2 & 0xffff) as u64 + m - 1;
    s2 += ((a1 >> 16) & 0xffff) as u64 + ((a2 >> 16) & 0xffff) as u64 + m - rem;
    if s1 >= m {
        s1 -= m;
    }
    if s1 >= m {
        s1 -= m;
    }
    if s2 >= m << 1 {
        s2 -= m << 1;
    }
    if s2 >= m {
        s2 -= m;
    }
    (s1 | (s2 << 16)) as u32
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Appends the filter byte and the row filtered with the type that minimizes the sum of absolute
/// residuals (libpng's heuristic).
fn filter_row(cur: &[u8], prev: Option<&[u8]>, bpp: usize, scratch: &mut [Vec<u8>; 5], out: &mut Vec<u8>) {
    let n = cur.len();
    let zeros;
    let up = match prev {
        Some(p) => p,
        None => {
            zeros = vec![0u8; n];
            &zeros
        }
    };
    for v in scratch.iter_mut() {
        v.resize(n, 0);
    }
    let [none, sub, upf, avg, pae] = scratch;
    none.copy_from_slice(cur);
    for i in 0..n {
        let left = if i >= bpp { cur[i - bpp] } else { 0 };
        let up_left = if i >= bpp { up[i - bpp] } else { 0 };
        sub[i] = cur[i].wrapping_sub(left);
        upf[i] = cur[i].wrapping_sub(up[i]);
        avg[i] = cur[i].wrapping_sub(((left as u16 + up[i] as u16) >> 1) as u8);
        pae[i] = cur[i].wrapping_sub(paeth(left, up[i], up_left));
    }
    let cost = |v: &[u8]| v.iter().map(|&x| (x as i8).unsigned_abs() as u32).sum::<u32>();
    let best = (0..5).min_by_key(|&t| cost(&scratch[t])).unwrap_or(0);
    out.push(best as u8);
    out.extend_from_slice(&scratch[best]);
}

fn deflate_strip(raw: &[u8], level: u8, last: bool) -> Vec<u8> {
    let mut c = flate2::Compress::new(flate2::Compression::new(level as u32), false);
    let mut z = Vec::with_capacity(raw.len() / 2 + MIN_SPARE);
    let flush = if last { flate2::FlushCompress::Finish } else { flate2::FlushCompress::Sync };
    loop {
        if z.capacity() - z.len() < MIN_SPARE {
            z.reserve(z.capacity().max(MIN_SPARE));
        }
        let status =
            c.compress_vec(&raw[c.total_in() as usize..], &mut z, flush).expect("deflate cannot fail on valid input");
        let flushed = c.total_in() as usize == raw.len() && z.capacity() - z.len() >= MIN_SPARE;
        let done = if last { status == flate2::Status::StreamEnd } else { flushed };
        if done {
            return z;
        }
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// Encodes 8-bit pixels as PNG with deflate `level` (0-9, clamped).
///
/// # Panics
///
/// Panics if `data` is shorter than `width * height * channels`.
pub fn encode(width: u32, height: u32, color: ColorType, data: &[u8], level: u8) -> Vec<u8> {
    let bpp = color.channels();
    let stride = width as usize * bpp;
    let rows = height as usize;
    assert!(data.len() >= stride * rows, "pixel buffer too short");
    let level = level.min(9);
    let strips = rows.div_ceil(STRIP_ROWS).max(1);
    let parts: Vec<(Vec<u8>, u32, u64)> = (0..strips)
        .into_par_iter()
        .map(|s| {
            let (y0, y1) = (s * STRIP_ROWS, ((s + 1) * STRIP_ROWS).min(rows));
            let mut raw = Vec::with_capacity((y1 - y0) * (stride + 1));
            let mut scratch: [Vec<u8>; 5] = Default::default();
            for y in y0..y1 {
                let prev = (y > 0).then(|| &data[(y - 1) * stride..y * stride]);
                filter_row(&data[y * stride..(y + 1) * stride], prev, bpp, &mut scratch, &mut raw);
            }
            (deflate_strip(&raw, level, s + 1 == strips), adler32(&raw), raw.len() as u64)
        })
        .collect();

    let mut idat = vec![0x78, 0x01];
    let mut adler = 1u32;
    for (z, a, len) in &parts {
        idat.extend_from_slice(z);
        adler = adler32_combine(adler, *a, *len);
    }
    idat.extend_from_slice(&adler.to_be_bytes());

    let mut out = Vec::with_capacity(idat.len() + 64);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, color.code(), 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    for c in idat.chunks(IDAT_CHUNK) {
        chunk(&mut out, b"IDAT", c);
    }
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(png_data: &[u8]) -> (png::OutputInfo, Vec<u8>) {
        let mut reader = png::Decoder::new(png_data).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info, buf)
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut s = 12345u32;
        (0..len)
            .map(|i| {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                if i % 7 < 4 {
                    (i / 13) as u8
                } else {
                    (s >> 16) as u8
                }
            })
            .collect()
    }

    #[test]
    fn roundtrips_every_color_type_and_level() {
        for (color, ch) in [(ColorType::Gray, 1), (ColorType::Rgb, 3), (ColorType::Rgba, 4)] {
            for level in [0, 1, 2, 6, 9] {
                let (w, h) = (97u32, 203u32);
                let data = noise(w as usize * h as usize * ch);
                let (info, out) = decode(&encode(w, h, color, &data, level));
                assert_eq!((info.width, info.height), (w, h));
                assert_eq!(out, data, "{color:?} level {level}");
            }
        }
    }

    #[test]
    fn handles_tiny_images() {
        let (info, out) = decode(&encode(1, 1, ColorType::Rgba, &[1, 2, 3, 4], 2));
        assert_eq!((info.width, out), (1, vec![1, 2, 3, 4]));
    }

    #[test]
    fn adler_combine_matches_direct() {
        let data = noise(100_000);
        for split in [0, 1, 5552, 65521, 99_999] {
            let (a, b) = data.split_at(split);
            assert_eq!(adler32_combine(adler32(a), adler32(b), b.len() as u64), adler32(&data));
        }
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn paeth_predictor() {
        assert_eq!(paeth(10, 20, 15), 15);
        assert_eq!(paeth(10, 20, 10), 20);
        assert_eq!(paeth(10, 20, 20), 10);
    }

    #[test]
    #[should_panic(expected = "pixel buffer too short")]
    fn rejects_short_buffers() {
        encode(2, 2, ColorType::Rgb, &[0; 3], 2);
    }
}
