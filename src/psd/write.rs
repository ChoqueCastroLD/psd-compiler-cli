//! Writing a document back as PSD/PSB: the original file, with the records and channels of edited
//! layers, the embedded files of edited smart objects and the composite image replaced. Everything
//! else is copied byte for byte.

use std::collections::HashMap;

use super::reader::Reader;
use super::{Document, Rect, LONG_KEYS};
use crate::error::{bail, Result};

/// New pixels for one layer: straight 8-bit RGBA over `rect`.
pub(crate) struct Pixels {
    pub rect: Rect,
    pub rgba: Vec<u8>,
}

/// What to replace in the original file.
#[derive(Default)]
pub(crate) struct Edits {
    /// New pixels by layer index.
    pub layers: HashMap<usize, Pixels>,
    /// New contents of embedded files by unique id.
    pub linked: HashMap<String, Vec<u8>>,
    /// New composite image: straight 8-bit RGBA, document sized.
    pub composite: Option<Vec<u8>>,
}

struct Out {
    data: Vec<u8>,
    psb: bool,
}

impl Out {
    fn raw(&mut self, b: &[u8]) {
        self.data.extend_from_slice(b);
    }
    fn u16(&mut self, v: u16) {
        self.raw(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.raw(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.raw(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.raw(&v.to_be_bytes());
    }
    /// A section or channel length: 8 bytes in PSB, else 4.
    fn length(&mut self, v: usize) -> Result<()> {
        if self.psb {
            self.u64(v as u64);
        } else {
            let Ok(v) = u32::try_from(v) else { bail!("section too large for PSD; save as PSB") };
            self.u32(v);
        }
        Ok(())
    }
    /// A tagged block length: 8 bytes for the long keys of PSB.
    fn block_length(&mut self, key: &[u8; 4], v: usize) -> Result<()> {
        if self.psb && LONG_KEYS.contains(&key) {
            self.u64(v as u64);
            Ok(())
        } else {
            let Ok(v) = u32::try_from(v) else { bail!("block too large") };
            self.u32(v);
            Ok(())
        }
    }
    fn pad(&mut self, from: usize, to: usize) {
        while (self.data.len() - from) % to != 0 {
            self.data.push(0);
        }
    }
    fn block(&mut self, key: &[u8; 4], body: &[u8]) -> Result<()> {
        self.raw(b"8BIM");
        self.raw(key);
        let padded = body.len().next_multiple_of(4);
        self.block_length(key, padded)?;
        self.raw(body);
        self.data.resize(self.data.len() + padded - body.len(), 0);
        Ok(())
    }
}

/// PackBits, as Photoshop writes it.
fn packbits(row: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < row.len() {
        let mut run = 1;
        while i + run < row.len() && run < 128 && row[i + run] == row[i] {
            run += 1;
        }
        if run >= 2 {
            out.push((1 - run as i32) as i8 as u8);
            out.push(row[i]);
            i += run;
            continue;
        }
        let start = i;
        while i < row.len() && i - start < 128 {
            if i + 1 < row.len() && row[i + 1] == row[i] {
                break;
            }
            i += 1;
        }
        if i == start {
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&row[start..i]);
    }
}

/// Encodes `planes` (8-bit, `w` x `h` each) as one compressed block: RLE for 8-bit documents,
/// raw otherwise. Counts of all planes come first, as in the composite and in layer channels.
fn encode_planes(planes: &[Vec<u8>], w: usize, h: usize, depth: u16, psb: bool) -> Result<Vec<u8>> {
    let mut out = Out { data: vec![], psb };
    match depth {
        8 => {
            out.u16(1);
            let rows: Vec<Vec<u8>> = planes
                .iter()
                .flat_map(|p| p.chunks(w.max(1)).take(h))
                .map(|r| {
                    let mut v = vec![];
                    packbits(r, &mut v);
                    v
                })
                .collect();
            for r in &rows {
                if psb {
                    out.u32(r.len() as u32);
                } else {
                    let Ok(n) = u16::try_from(r.len()) else { bail!("row too long for PSD") };
                    out.u16(n);
                }
            }
            rows.iter().for_each(|r| out.raw(r));
        }
        16 => {
            out.u16(0);
            for p in planes {
                p.iter().for_each(|&v| out.u16(v as u16 * 257));
            }
        }
        d => bail!("writing {d}-bit documents is not supported"),
    }
    Ok(out.data)
}

/// The channel planes for straight RGBA: alpha (-1) and the color channels.
fn plane(rgba: &[u8], id: i16, gray: bool) -> Option<Vec<u8>> {
    let c = match id {
        -1 => 3,
        0..=2 if !gray => id as usize,
        0 if gray => 0,
        _ => return None,
    };
    Some(rgba.chunks_exact(4).map(|p| p[c]).collect())
}

/// Writes `doc`, parsed from `original`, with `edits` applied.
pub(crate) fn write(original: &[u8], doc: &Document, edits: &Edits) -> Result<Vec<u8>> {
    let mut r = Reader::new(original);
    if &r.tag()? != b"8BPS" {
        bail!("missing 8BPS signature");
    }
    let psb = r.u16()? == 2;
    r.psb = psb;
    r.pos = 26;
    let mut out = Out { data: original[..26].to_vec(), psb };
    // Color mode data and image resources.
    for _ in 0..2 {
        let start = r.pos;
        let n = r.u32()? as usize;
        r.skip(n)?;
        out.raw(&original[start..r.pos]);
    }
    let lm_len = r.length()?;
    let lm_start = r.pos;
    let lm_end = lm_start + lm_len;
    if lm_end > original.len() {
        bail!("truncated layer and mask section");
    }
    let mut lm = Out { data: vec![], psb };
    let mut transparency = false;
    if lm_len > 0 {
        let info_len = r.length()?;
        let info = &original[r.pos..r.pos + info_len];
        r.pos += info_len;
        if info_len > 0 {
            let (body, t) = layer_info(info, psb, doc, edits)?;
            transparency = t;
            lm.length(body.len())?;
            lm.raw(&body);
        } else {
            lm.length(0)?;
        }
        if r.pos + 4 <= lm_end {
            let start = r.pos;
            let n = r.u32()? as usize;
            r.skip(n)?;
            lm.raw(&original[start..r.pos]);
        }
        while r.pos + 12 <= lm_end {
            let start = r.pos;
            let sig = r.tag()?;
            if &sig != b"8BIM" && &sig != b"8B64" {
                r.pos = start;
                break;
            }
            let key = r.tag()?;
            let len = if psb && LONG_KEYS.contains(&&key) { r.u64()? as usize } else { r.u32()? as usize };
            let body = &original[r.pos..(r.pos + len).min(lm_end)];
            let next = (r.pos + len.next_multiple_of(4)).min(lm_end);
            match &key {
                b"Lr16" | b"Lr32" | b"Layr" if !body.is_empty() => {
                    let (new, t) = layer_info(body, psb, doc, edits)?;
                    transparency = t;
                    lm.block(&key, &new)?;
                }
                b"lnk2" | b"lnkD" | b"lnk3" if !edits.linked.is_empty() => lm.block(&key, &linked(body, edits)?)?,
                _ => lm.raw(&original[start..next]),
            }
            r.pos = next;
        }
        lm.raw(&original[r.pos..lm_end]);
    }
    out.length(lm.data.len())?;
    out.raw(&lm.data);
    match &edits.composite {
        Some(rgba) => out.raw(&composite(doc, rgba, transparency, psb)?),
        None => out.raw(&original[lm_end..]),
    }
    Ok(out.data)
}

struct RecordSpan {
    start: usize,
    /// End of the fixed part, before the extra data length.
    fixed_end: usize,
    channels: Vec<(i16, usize)>,
    extra: (usize, usize),
}

/// Rewrites a layer info section; also returns whether the composite's first alpha channel holds
/// its transparency (a negative layer count).
fn layer_info(data: &[u8], psb: bool, doc: &Document, edits: &Edits) -> Result<(Vec<u8>, bool)> {
    let mut r = Reader::at(data, 0, psb);
    let count = r.i16()?;
    let n = count.unsigned_abs() as usize;
    if n != doc.layers.len() {
        bail!("the document does not match the file it was parsed from");
    }
    let mut records = Vec::with_capacity(n);
    for _ in 0..n {
        let start = r.pos;
        r.skip(16)?;
        let cc = r.u16()? as usize;
        let mut channels = Vec::with_capacity(cc);
        for _ in 0..cc {
            let id = r.i16()?;
            channels.push((id, r.length()?));
        }
        r.skip(12)?;
        let fixed_end = r.pos;
        let extra = r.u32()? as usize;
        let extra_start = r.pos;
        r.skip(extra)?;
        records.push(RecordSpan { start, fixed_end, channels, extra: (extra_start, r.pos) });
    }
    let channel_start = r.pos;
    let mut out = Out { data: vec![], psb };
    out.raw(&count.to_be_bytes());
    let gray = matches!(doc.color_mode, super::ColorMode::Grayscale);
    let mut new_channels: Vec<Vec<Option<Vec<u8>>>> = Vec::with_capacity(n);
    for (i, rec) in records.iter().enumerate() {
        let Some(px) = edits.layers.get(&i) else {
            out.raw(&data[rec.start..rec.extra.1]);
            new_channels.push(vec![None; rec.channels.len()]);
            continue;
        };
        let (w, h) = (px.rect.width(), px.rect.height());
        let chans: Vec<Option<Vec<u8>>> = rec
            .channels
            .iter()
            .map(|&(id, _)| plane(&px.rgba, id, gray).map(|p| encode_planes(&[p], w, h, doc.depth, psb)).transpose())
            .collect::<Result<_>>()?;
        let rect = px.rect;
        for v in [rect.top, rect.left, rect.bottom, rect.right] {
            out.i32(v);
        }
        out.u16(rec.channels.len() as u16);
        for (&(id, len), new) in rec.channels.iter().zip(&chans) {
            out.raw(&id.to_be_bytes());
            out.length(new.as_ref().map_or(len, Vec::len))?;
        }
        let blend_start = rec.fixed_end - 12;
        out.raw(&data[blend_start..rec.fixed_end]);
        let extra = extra_data(&data[rec.extra.0..rec.extra.1], psb, &doc.layers[i])?;
        out.u32(extra.len() as u32);
        out.raw(&extra);
        new_channels.push(chans);
    }
    let mut pos = channel_start;
    for (rec, chans) in records.iter().zip(&new_channels) {
        for (&(_, len), new) in rec.channels.iter().zip(chans) {
            let end = (pos + len).min(data.len());
            match new {
                Some(d) => out.raw(d),
                None => out.raw(&data[pos..end]),
            }
            pos = end;
        }
    }
    out.pad(0, 4);
    Ok((out.data, count < 0))
}

/// Layer extra data with the tagged blocks that changed in `layer` replaced.
fn extra_data(data: &[u8], psb: bool, layer: &super::Layer) -> Result<Vec<u8>> {
    let mut r = Reader::at(data, 0, psb);
    let mask = r.u32()? as usize;
    r.skip(mask)?;
    let ranges = r.u32()? as usize;
    r.skip(ranges)?;
    let name = r.u8()? as usize;
    r.skip(name + (4 - (1 + name) % 4) % 4)?;
    let mut out = Out { data: data[..r.pos].to_vec(), psb };
    while r.pos + 12 <= data.len() {
        let start = r.pos;
        let sig = r.tag()?;
        if &sig != b"8BIM" && &sig != b"8B64" {
            r.pos = start;
            break;
        }
        let key = r.tag()?;
        let len = if psb && LONG_KEYS.contains(&&key) { r.u64()? as usize } else { r.u32()? as usize };
        let end = (r.pos + len).min(data.len());
        let body = &data[r.pos..end];
        match layer.blocks.get(&key) {
            Some(new) if new.as_slice() != body => {
                out.raw(&sig);
                out.raw(&key);
                let padded = new.len().next_multiple_of(4);
                out.block_length(&key, padded)?;
                out.raw(new);
                out.data.resize(out.data.len() + padded - new.len(), 0);
            }
            _ => out.raw(&data[start..end]),
        }
        r.pos = end;
    }
    out.raw(&data[r.pos..]);
    Ok(out.data)
}

/// A `lnk2`/`lnkD`/`lnk3` block with the data of edited embedded files replaced.
fn linked(data: &[u8], edits: &Edits) -> Result<Vec<u8>> {
    let mut r = Reader::new(data);
    let mut out = Out { data: vec![], psb: false };
    while r.remaining() >= 8 {
        let len = r.u64()? as usize;
        let start = r.pos;
        let end = (start + len).min(data.len());
        let next = (start + len).next_multiple_of(4).min(data.len());
        let replaced = (|| -> Result<Option<Vec<u8>>> {
            let mut e = Reader::at(&data[..end], start, false);
            let kind = e.tag()?;
            e.u32()?;
            let id_len = e.u8()? as usize;
            let id = String::from_utf8_lossy(e.bytes(id_len)?).into_owned();
            let Some(new) = edits.linked.get(&id).filter(|_| &kind == b"liFD") else { return Ok(None) };
            e.unicode()?;
            e.skip(8)?;
            let size_at = e.pos;
            let size = e.u64()? as usize;
            if e.u8()? != 0 {
                e.u32()?;
                super::descriptor::read(&mut e)?;
            }
            let data_at = e.pos;
            let mut entry = data[start..size_at].to_vec();
            entry.extend_from_slice(&(new.len() as u64).to_be_bytes());
            entry.extend_from_slice(&data[size_at + 8..data_at]);
            entry.extend_from_slice(new);
            entry.extend_from_slice(&data[(data_at + size).min(end)..end]);
            Ok(Some(entry))
        })()?;
        match replaced {
            Some(entry) => {
                out.u64(entry.len() as u64);
                out.raw(&entry);
                out.pad(0, 4);
            }
            None => out.raw(&data[start - 8..next]),
        }
        r.pos = next;
    }
    out.raw(&data[r.pos..]);
    Ok(out.data)
}

/// The composite image section from straight RGBA: color flattened onto white (Photoshop's
/// matte), then the transparency channel or the document's own alpha channels.
fn composite(doc: &Document, rgba: &[u8], transparency: bool, psb: bool) -> Result<Vec<u8>> {
    let (w, h) = (doc.width as usize, doc.height as usize);
    let gray = matches!(doc.color_mode, super::ColorMode::Grayscale);
    let color = if gray { 1 } else { 3 };
    let flat = |c: usize| -> Vec<u8> {
        rgba.chunks_exact(4)
            .map(|p| {
                let a = p[3] as u32;
                ((p[c] as u32 * a + 255 * (255 - a) + 127) / 255) as u8
            })
            .collect()
    };
    let mut planes: Vec<Vec<u8>> = (0..color).map(flat).collect();
    for c in color..doc.channel_count as usize {
        planes.push(match doc.composite.get(c) {
            _ if c == color && transparency => rgba.chunks_exact(4).map(|p| p[3]).collect(),
            Some(p) if p.len() == w * h => p.clone(),
            _ => vec![0; w * h],
        });
    }
    encode_planes(&planes, w, h, doc.depth, psb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unpack(mut src: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        while let [n, rest @ ..] = src {
            let n = *n as i8;
            if n >= 0 {
                out.extend_from_slice(&rest[..n as usize + 1]);
                src = &rest[n as usize + 1..];
            } else {
                out.extend(std::iter::repeat_n(rest[0], (1 - n as i32) as usize));
                src = &rest[1..];
            }
        }
        out
    }

    #[test]
    fn packbits_roundtrips() {
        let mut row: Vec<u8> = (0..300).map(|i| (i / 7) as u8).collect();
        row.extend([9; 200]);
        row.extend((0..150).map(|i| (i * 31 % 256) as u8));
        for r in [&row[..], &[5][..], &[1, 2][..], &[3, 3][..], &[][..]] {
            let mut enc = vec![];
            packbits(r, &mut enc);
            assert_eq!(unpack(&enc), r);
        }
    }
}
