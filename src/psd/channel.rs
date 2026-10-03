use std::io::Read;

use super::reader::Reader;
use crate::error::{bail, Result};

/// Channel compression method, as stored before each channel's pixel data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compression {
    Raw,
    Rle,
    Zip,
    ZipPredicted,
}

impl Compression {
    pub fn from_u16(v: u16) -> Result<Self> {
        Ok(match v {
            0 => Compression::Raw,
            1 => Compression::Rle,
            2 => Compression::Zip,
            3 => Compression::ZipPredicted,
            _ => bail!("unknown channel compression {v}"),
        })
    }
}

/// Appends exactly `want` bytes decoded from a PackBits run to `out`, zero-filling short input.
pub(crate) fn decode_packbits(src: &[u8], out: &mut Vec<u8>, want: usize) {
    let start = out.len();
    let mut i = 0;
    while i < src.len() && out.len() - start < want {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let end = (i + n as usize + 1).min(src.len());
            out.extend_from_slice(&src[i..end]);
            i = end;
        } else if n != -128 {
            if let Some(&v) = src.get(i) {
                out.extend(std::iter::repeat_n(v, (1 - n as isize) as usize));
            }
            i += 1;
        }
    }
    out.resize(start + want, 0);
}

/// Reverses Photoshop's horizontal delta prediction for 8 and 16 bit samples.
pub(crate) fn unpredict(data: &mut [u8], width: usize, depth: u16) {
    match depth {
        8 => {
            for row in data.chunks_exact_mut(width) {
                for x in 1..row.len() {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
            }
        }
        16 => {
            for row in data.chunks_exact_mut(width * 2) {
                for x in 1..width {
                    let prev = u16::from_be_bytes([row[2 * x - 2], row[2 * x - 1]]);
                    let cur = u16::from_be_bytes([row[2 * x], row[2 * x + 1]]).wrapping_add(prev);
                    row[2 * x..2 * x + 2].copy_from_slice(&cur.to_be_bytes());
                }
            }
        }
        32 => {
            let n = width * 4;
            let mut planes = vec![0u8; n];
            for row in data.chunks_exact_mut(n) {
                for x in 1..n {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
                planes.copy_from_slice(row);
                for x in 0..width {
                    for b in 0..4 {
                        row[x * 4 + b] = planes[b * width + x];
                    }
                }
            }
        }
        _ => {}
    }
}

fn row_bytes(width: usize, depth: u16) -> usize {
    if depth == 1 {
        width.div_ceil(8)
    } else {
        width * (depth as usize / 8)
    }
}

/// What a channel holds, which decides how deeper samples map to 8 bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sample {
    Color,
    /// Alpha and masks: never gamma encoded.
    Linear,
    /// Lab a/b: 16-bit samples are 256 per unit around 32768, not a 0..=65535 ramp.
    Chroma,
}

impl Sample {
    pub(crate) fn of(c: usize, color_channels: usize, lab: bool) -> Sample {
        if c >= color_channels {
            Sample::Linear
        } else if lab && c > 0 {
            Sample::Chroma
        } else {
            Sample::Color
        }
    }
}

/// Converts raw samples of any supported depth to 8 bits per sample.
///
/// 32-bit color is linear light and gets the sRGB curve; linear channels (alpha, masks) do not.
pub(crate) fn to_8bit(raw: Vec<u8>, width: usize, height: usize, depth: u16, kind: Sample) -> Result<Vec<u8>> {
    let linear = kind == Sample::Linear;
    Ok(match depth {
        8 => raw,
        16 if kind == Sample::Chroma => {
            raw.chunks_exact(2).map(|c| ((u16::from_be_bytes([c[0], c[1]]) as u32 + 128) >> 8).min(255) as u8).collect()
        }
        16 => raw
            .chunks_exact(2)
            .map(|c| ((u16::from_be_bytes([c[0], c[1]]) as u32 * 255 + 32767) / 65535) as u8)
            .collect(),
        32 => raw
            .chunks_exact(4)
            .map(|c| {
                let v = f32::from_be_bytes([c[0], c[1], c[2], c[3]]).clamp(0.0, 1.0);
                let v = if linear { v } else { crate::color::srgb_encode(v as f64) as f32 };
                (v * 255.0 + 0.5) as u8
            })
            .collect(),
        1 => {
            let stride = width.div_ceil(8);
            let mut out = vec![0u8; width * height];
            for y in 0..height {
                for x in 0..width {
                    let ink = raw[y * stride + x / 8] & (0x80 >> (x % 8)) != 0;
                    out[y * width + x] = if ink { 0 } else { 255 };
                }
            }
            out
        }
        _ => bail!("unsupported bit depth {depth}"),
    })
}

/// Decodes one layer channel ending at byte `end`, returning 8-bit samples.
pub(crate) fn decode_channel(
    r: &mut Reader,
    compression: Compression,
    width: usize,
    height: usize,
    depth: u16,
    end: usize,
    kind: Sample,
) -> Result<Vec<u8>> {
    let stride = row_bytes(width, depth);
    let size = stride * height;
    let available = end.saturating_sub(r.pos);
    let mut raw = Vec::with_capacity(size);
    match compression {
        Compression::Raw => raw.extend_from_slice(r.bytes(size.min(available))?),
        Compression::Rle => {
            let mut counts = Vec::with_capacity(height);
            for _ in 0..height {
                counts.push(if r.psb { r.u32()? as usize } else { r.u16()? as usize });
            }
            for n in counts {
                let src = r.bytes(n.min(end.saturating_sub(r.pos)))?;
                decode_packbits(src, &mut raw, stride);
            }
        }
        Compression::Zip | Compression::ZipPredicted => {
            let src = r.bytes(available)?;
            let _ = flate2::read::ZlibDecoder::new(src).take(size as u64).read_to_end(&mut raw);
            raw.resize(size, 0);
            if compression == Compression::ZipPredicted {
                unpredict(&mut raw, width, depth);
            }
        }
    }
    raw.resize(size, 0);
    to_8bit(raw, width, height, depth, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packbits(src: &[u8], want: usize) -> Vec<u8> {
        let mut out = vec![];
        decode_packbits(src, &mut out, want);
        out
    }

    #[test]
    fn packbits_literal_and_repeat_runs() {
        let src = [0xFE, 0xAA, 0x02, 0x80, 0x00, 0x2A, 0xFD, 0xAA, 0x03, 0x80, 0x00, 0x2A, 0x22, 0xF7, 0xAA];
        let expected = [
            0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0xAA, 0xAA, 0xAA, 0xAA, 0x80, 0x00, 0x2A, 0x22, 0xAA, 0xAA, 0xAA, 0xAA,
            0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA,
        ];
        assert_eq!(packbits(&src, expected.len()), expected);
    }

    #[test]
    fn packbits_ignores_noop_and_pads_short_input() {
        assert_eq!(packbits(&[0x80, 0x00, 0x07], 3), [7, 0, 0]);
    }

    #[test]
    fn packbits_truncates_long_input() {
        assert_eq!(packbits(&[0xF9, 0x01], 4), [1, 1, 1, 1]);
    }

    #[test]
    fn unpredict_8bit_accumulates_per_row() {
        let mut d = [1, 1, 1, 10, 255, 2];
        unpredict(&mut d, 3, 8);
        assert_eq!(d, [1, 2, 3, 10, 9, 11]);
    }

    #[test]
    fn unpredict_16bit_accumulates_big_endian() {
        let mut d = [0x01, 0x00, 0x00, 0xFF, 0x00, 0x01];
        unpredict(&mut d, 3, 16);
        assert_eq!(d, [0x01, 0x00, 0x01, 0xFF, 0x02, 0x00]);
    }

    #[test]
    fn unpredicts_32_bit_byte_planes() {
        let values = [0.5f32, 1.0];
        let mut planes = vec![0u8; 8];
        for (x, v) in values.iter().enumerate() {
            for (b, byte) in v.to_be_bytes().iter().enumerate() {
                planes[b * 2 + x] = *byte;
            }
        }
        let mut deltas = planes.clone();
        for x in (1..8).rev() {
            deltas[x] = planes[x].wrapping_sub(planes[x - 1]);
        }
        unpredict(&mut deltas, 2, 32);
        assert_eq!(&deltas[..4], 0.5f32.to_be_bytes());
        assert_eq!(&deltas[4..], 1.0f32.to_be_bytes());
    }

    #[test]
    fn converts_depths_to_8bit() {
        assert_eq!(to_8bit(vec![0xAB, 0xCD], 1, 1, 16, Sample::Color).unwrap(), [0xAB]);
        assert_eq!(to_8bit(1.0f32.to_be_bytes().to_vec(), 1, 1, 32, Sample::Color).unwrap(), [255]);
        assert_eq!(to_8bit(0.25f32.to_be_bytes().to_vec(), 1, 1, 32, Sample::Linear).unwrap(), [64]);
        assert_eq!(to_8bit(vec![0b1010_0000], 3, 1, 1, Sample::Color).unwrap(), [0, 255, 0]);
        assert!(to_8bit(vec![], 0, 0, 7, Sample::Color).is_err());
        // Lab a/b: 32768 is neutral, 256 per unit (a = 80.8 here).
        assert_eq!(to_8bit(53453u16.to_be_bytes().to_vec(), 1, 1, 16, Sample::Chroma).unwrap(), [209]);
        assert_eq!(to_8bit(32768u16.to_be_bytes().to_vec(), 1, 1, 16, Sample::Chroma).unwrap(), [128]);
    }

    #[test]
    fn decodes_zip_with_prediction() {
        use flate2::{write::ZlibEncoder, Compression as Level};
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), Level::default());
        enc.write_all(&[5, 1, 1, 7, 0, 0]).unwrap();
        let data = enc.finish().unwrap();
        let mut r = Reader::new(&data);
        let out = decode_channel(&mut r, Compression::ZipPredicted, 3, 2, 8, data.len(), Sample::Color).unwrap();
        assert_eq!(out, [5, 6, 7, 7, 7, 7]);
    }

    #[test]
    fn rejects_unknown_compression() {
        assert!(Compression::from_u16(4).is_err());
    }
}
