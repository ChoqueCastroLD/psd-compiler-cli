//! Minimal baseline TIFF writer: 8-bit RGB(A), Deflate with the horizontal predictor.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use rayon::prelude::*;
use std::io::Write;

/// Target uncompressed size of one strip.
const STRIP_BYTES: usize = 1 << 16;

fn predict(row: &mut [u8], spp: usize) {
    for i in (spp..row.len()).rev() {
        row[i] = row[i].wrapping_sub(row[i - spp]);
    }
}

/// Encodes `data` (`width * height * spp` bytes, `spp` 3 or 4) with Deflate `level` (0-9).
pub(crate) fn encode(width: u32, height: u32, spp: usize, data: &[u8], level: u8) -> std::io::Result<Vec<u8>> {
    let stride = width as usize * spp;
    let rows_per_strip = (STRIP_BYTES / stride.max(1)).max(1);
    let strips: Vec<Vec<u8>> = data
        .par_chunks(stride * rows_per_strip)
        .map(|chunk| {
            let mut raw = chunk.to_vec();
            raw.chunks_exact_mut(stride).for_each(|row| predict(row, spp));
            let mut z = ZlibEncoder::new(Vec::new(), Compression::new(level.min(9) as u32));
            z.write_all(&raw).and_then(|_| z.finish())
        })
        .collect::<std::io::Result<_>>()?;

    let mut out = b"MM\0\x2a\0\0\0\0".to_vec();
    let mut offsets = vec![];
    if strips.is_empty() {
        return Err(std::io::Error::other("empty image"));
    }
    for s in &strips {
        offsets.push(out.len() as u64);
        out.extend_from_slice(s);
        if out.len() % 2 == 1 {
            out.push(0);
        }
    }
    // Out-of-line values: bits per sample, resolution, strip offsets and byte counts.
    let mut extra: Vec<u8> = vec![];
    let ifd_entries = if spp == 4 { 15 } else { 14 };
    let ifd_at = out.len() as u64;
    let extra_at = ifd_at + 2 + ifd_entries * 12 + 4;
    let mut put = |bytes: &[u8]| {
        let at = extra_at + extra.len() as u64;
        extra.extend_from_slice(bytes);
        at as u32
    };
    let bits = put(&[0, 8].repeat(spp));
    let res = put(&[0, 0, 0, 72, 0, 0, 0, 1]);
    let n = strips.len() as u32;
    let strip_offsets = match n {
        1 => offsets[0] as u32,
        _ => put(&offsets.iter().flat_map(|&o| (o as u32).to_be_bytes()).collect::<Vec<_>>()),
    };
    let counts: Vec<u32> = strips.iter().map(|s| s.len() as u32).collect();
    let strip_counts = match n {
        1 => counts[0],
        _ => put(&counts.iter().flat_map(|c| c.to_be_bytes()).collect::<Vec<_>>()),
    };
    if extra_at + extra.len() as u64 > u32::MAX as u64 {
        return Err(std::io::Error::other("image too large for TIFF"));
    }

    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    const RATIONAL: u16 = 5;
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, LONG, 1, width),
        (257, LONG, 1, height),
        (258, SHORT, spp as u32, bits),
        (259, SHORT, 1, 8),
        (262, SHORT, 1, 2),
        (273, LONG, n, strip_offsets),
        (277, SHORT, 1, spp as u32),
        (278, LONG, 1, rows_per_strip as u32),
        (279, LONG, n, strip_counts),
        (282, RATIONAL, 1, res),
        (283, RATIONAL, 1, res),
        (284, SHORT, 1, 1),
        (296, SHORT, 1, 2),
        (317, SHORT, 1, 2),
    ];
    if spp == 4 {
        entries.push((338, SHORT, 1, 2));
    }
    out[4..8].copy_from_slice(&(ifd_at as u32).to_be_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for (tag, ty, count, value) in entries {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&ty.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        if ty == SHORT && count == 1 {
            out.extend_from_slice(&(value as u16).to_be_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&extra);
    Ok(out)
}
