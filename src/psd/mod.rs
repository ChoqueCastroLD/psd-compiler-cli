//! PSD/PSB container parsing: header, layer records, channel data and tagged blocks.

mod channel;
pub(crate) mod descriptor;
pub(crate) mod engine;
pub(crate) mod reader;

use std::collections::HashMap;
use std::path::Path;

use channel::{decode_channel, decode_packbits, to_8bit, Compression};
use reader::Reader;

use crate::blend::BlendMode;
use crate::error::{bail, Result};

/// Color mode of a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    /// 1-bit black and white.
    Bitmap,
    /// Single gray channel.
    Grayscale,
    /// Palette based color (rendered as RGB).
    Indexed,
    /// Red, green, blue.
    Rgb,
    /// Cyan, magenta, yellow, black.
    Cmyk,
    /// Independent ink channels.
    Multichannel,
    /// One to four inks over a grayscale channel (rendered as grayscale).
    Duotone,
    /// CIE L*a*b*.
    Lab,
}

impl ColorMode {
    /// Number of color channels; any further channels are alpha.
    pub(crate) fn channels(self) -> usize {
        match self {
            ColorMode::Rgb | ColorMode::Lab => 3,
            ColorMode::Cmyk => 4,
            ColorMode::Multichannel => usize::MAX,
            _ => 1,
        }
    }
}

/// Layer rectangle in document pixels; `right` and `bottom` are exclusive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Top edge.
    pub top: i32,
    /// Left edge.
    pub left: i32,
    /// Bottom edge (exclusive).
    pub bottom: i32,
    /// Right edge (exclusive).
    pub right: i32,
}

impl Rect {
    /// Width in pixels (zero for inverted rectangles).
    pub fn width(&self) -> usize {
        (self.right - self.left).max(0) as usize
    }

    /// Height in pixels (zero for inverted rectangles).
    pub fn height(&self) -> usize {
        (self.bottom - self.top).max(0) as usize
    }

    fn read(r: &mut Reader) -> Result<Rect> {
        Ok(Rect { top: r.i32()?, left: r.i32()?, bottom: r.i32()?, right: r.i32()? })
    }
}

/// What a layer record represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayerKind {
    /// Raster pixels.
    Pixel,
    /// Editable type layer, re-rendered from its text data.
    Text,
    /// Solid color, gradient or pattern fill; a shape layer when it has a vector mask.
    Fill,
    /// Adjustment layer (levels, curves, hue/saturation, ...), applied to the layers below.
    Adjustment,
    /// Placed (smart object) layer.
    SmartObject,
    /// Start of a group. Records come bottom-to-top, so this closes the group's children.
    Group,
    /// Hidden divider that marks where a group's children begin.
    GroupEnd,
}

#[derive(Clone, Debug)]
pub(crate) struct Mask {
    pub rect: Rect,
    pub default: u8,
    pub disabled: bool,
    pub data: Vec<u8>,
    pub density: f32,
    pub feather: f64,
}

/// One layer record. Layers are stored bottom-to-top, as in the file.
#[derive(Clone, Debug)]
pub struct Layer {
    /// Unicode layer name.
    pub name: String,
    /// Pixel bounds of the stored raster.
    pub bounds: Rect,
    /// Blend mode (pass-through for groups that use it).
    pub blend_mode: BlendMode,
    /// Layer opacity, 0-255.
    pub opacity: u8,
    /// Fill opacity, 0-255; affects content but not effects.
    pub fill_opacity: u8,
    /// Whether the layer is clipped to the layer below.
    pub clipping: bool,
    /// Whether the layer's visibility is off.
    pub hidden: bool,
    /// Record type.
    pub kind: LayerKind,
    pub(crate) mask: Option<Mask>,
    pub(crate) vector_density: f32,
    pub(crate) vector_feather: f64,
    pub(crate) channels: HashMap<i16, Vec<u8>>,
    pub(crate) blocks: HashMap<[u8; 4], Vec<u8>>,
}

impl Layer {
    pub(crate) fn block(&self, key: &[u8; 4]) -> Option<&[u8]> {
        self.blocks.get(key).map(Vec::as_slice)
    }

    /// Text content of a type layer, with `\r` paragraph separators.
    pub fn text(&self) -> Option<String> {
        let tl = crate::text::TextLayer::parse(self.block(b"TySh")?).ok()?;
        Some(tl.chars.iter().collect::<String>().trim_end_matches('\r').to_string())
    }

    /// Replaces the text of a type layer; `\n` or `\r` start a new paragraph.
    ///
    /// Each new paragraph keeps the paragraph settings of the old paragraph at the same position
    /// (the last one when there are more) and the character style used most in it. The layer is
    /// re-rendered from the new text by [`render`](crate::render).
    pub fn set_text(&mut self, text: &str) -> Result<()> {
        let Some(block) = self.blocks.get_mut(b"TySh") else {
            bail!("layer {:?} is not a type layer", self.name);
        };
        *block = crate::text::edit::replace_text(block, text)?;
        Ok(())
    }
}

/// A parsed PSD or PSB document.
#[derive(Clone, Debug)]
pub struct Document {
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// Bits per channel: 1, 8, 16 or 32.
    pub depth: u16,
    /// Color mode.
    pub color_mode: ColorMode,
    /// Layer records, bottom-to-top.
    pub layers: Vec<Layer>,
    pub(crate) channel_count: u16,
    pub(crate) global_angle: f64,
    pub(crate) global_altitude: f64,
    pub(crate) palette: Vec<[u8; 3]>,
    pub(crate) transparent_index: Option<u8>,
    pub(crate) composite: Vec<Vec<u8>>,
    pub(crate) patterns: HashMap<String, Pattern>,
    pub(crate) linked: HashMap<String, Vec<u8>>,
    pub(crate) icc_profile: Option<Vec<u8>>,
}

/// A pattern from the document's pattern table, as 8-bit RGBA.
#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub(crate) const ADJUSTMENT_KEYS: [&[u8; 4]; 18] = [
    b"levl", b"curv", b"brit", b"hue2", b"hue ", b"blnc", b"vibA", b"expA", b"selc", b"mixr", b"grdm", b"phfl",
    b"nvrt", b"post", b"thrs", b"CgEd", b"clrL", b"blwh",
];

pub(crate) const FILL_KEYS: [&[u8; 4]; 3] = [b"SoCo", b"GdFl", b"PtFl"];

const LONG_KEYS: [&[u8; 4]; 14] = [
    b"LMsk", b"Lr16", b"Lr32", b"Layr", b"Mt16", b"Mt32", b"Mtrn", b"Alph", b"FMsk", b"lnk2", b"FEid", b"FXid",
    b"PxSD", b"cinf",
];

const LAYER_KEYS: [&[u8; 4]; 49] = [
    b"TySh", b"lfx2", b"lmfx", b"lsct", b"lsdk", b"iOpa", b"luni", b"vmsk", b"vsms", b"vstk", b"vscg", b"SoLd",
    b"SoLE", b"PlLd", b"knko", b"clbl", b"infx", b"tsly", b"lmgm", b"vmgm", b"brst", b"SoCo", b"GdFl", b"PtFl",
    b"levl", b"curv", b"brit", b"hue2", b"hue ", b"blnc", b"vibA", b"expA", b"selc", b"mixr", b"grdm", b"phfl",
    b"nvrt", b"post", b"thrs", b"CgEd", b"clrL", b"blwh", b"lclr", b"shmd", b"fxrp", b"lyvr", b"artb", b"artd",
    b"abdd",
];
const GLOBAL_KEYS: [&[u8; 4]; 10] =
    [b"Layr", b"Lr16", b"Lr32", b"Patt", b"Pat2", b"Pat3", b"lnk2", b"lnkD", b"lnk3", b"lnkE"];

/// Tagged blocks we care about, borrowed from the file buffer; `present` lists every key seen.
struct Blocks<'a> {
    data: HashMap<[u8; 4], &'a [u8]>,
    extra: Vec<([u8; 4], &'a [u8])>,
    present: Vec<[u8; 4]>,
}

impl<'a> Blocks<'a> {
    fn read(r: &mut Reader<'a>, end: usize, pad4: bool, keep: &[&[u8; 4]]) -> Result<Self> {
        let mut blocks = Blocks { data: HashMap::new(), extra: vec![], present: vec![] };
        while r.pos + 12 <= end {
            let sig = r.tag()?;
            if &sig != b"8BIM" && &sig != b"8B64" {
                r.pos -= 3;
                continue;
            }
            let key = r.tag()?;
            let len = if r.psb && LONG_KEYS.contains(&&key) { r.u64()? as usize } else { r.u32()? as usize };
            let body = r.bytes(len.min(end.saturating_sub(r.pos)))?;
            if keep.contains(&&key) {
                blocks.data.entry(key).or_insert(body);
                blocks.extra.push((key, body));
            }
            blocks.present.push(key);
            if pad4 {
                r.pos = r.pos.next_multiple_of(4).min(end);
            }
        }
        Ok(blocks)
    }

    fn get(&self, key: &[u8; 4]) -> Option<&'a [u8]> {
        self.data.get(key).copied()
    }

    fn all(&self, key: [u8; 4]) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.extra.iter().filter(move |(k, _)| *k == key).map(|(_, b)| *b)
    }
}

struct Record {
    layer: Layer,
    channels: Vec<(i16, usize)>,
    mask_info: Vec<u8>,
}

struct MaskInfo {
    user: Option<(Rect, u8, u8)>,
    real: Option<(Rect, u8, u8)>,
    user_density: f32,
    user_feather: f64,
    vector_density: f32,
    vector_feather: f64,
}

fn parse_mask_info(data: &[u8], has_real: bool) -> MaskInfo {
    let mut info = MaskInfo {
        user: None,
        real: None,
        user_density: 1.0,
        user_feather: 0.0,
        vector_density: 1.0,
        vector_feather: 0.0,
    };
    let mut r = Reader::new(data);
    let mut read = || -> Result<()> {
        if data.len() < 18 {
            return Ok(());
        }
        let rect = Rect::read(&mut r)?;
        let default = r.u8()?;
        let flags = r.u8()?;
        info.user = Some((rect, default, flags));
        if has_real && data.len() >= 36 {
            let real_flags = r.u8()?;
            let real_default = r.u8()?;
            info.real = Some((Rect::read(&mut r)?, real_default, real_flags));
        }
        if flags & 0x10 != 0 {
            let params = r.u8()?;
            if params & 1 != 0 {
                info.user_density = r.u8()? as f32 / 255.0;
            }
            if params & 2 != 0 {
                info.user_feather = r.f64()?;
            }
            if params & 4 != 0 {
                info.vector_density = r.u8()? as f32 / 255.0;
            }
            if params & 8 != 0 {
                info.vector_feather = r.f64()?;
            }
        }
        Ok(())
    };
    let _ = read();
    info
}

fn read_record(r: &mut Reader) -> Result<Record> {
    let bounds = Rect::read(r)?;
    let channel_count = r.u16()? as usize;
    let mut channels = Vec::with_capacity(channel_count.min(64));
    for _ in 0..channel_count {
        let id = r.i16()?;
        channels.push((id, r.length()?));
    }
    if &r.tag()? != b"8BIM" {
        bail!("bad blend mode signature in layer record");
    }
    let blend_key = r.tag()?;
    let opacity = r.u8()?;
    let clipping = r.u8()? != 0;
    let flags = r.u8()?;
    r.u8()?;
    let extra = r.u32()? as usize;
    let extra_end = r.pos + extra;
    let mask_len = r.u32()? as usize;
    let mask_info = r.bytes(mask_len)?.to_vec();
    let ranges = r.u32()? as usize;
    r.skip(ranges)?;
    let name_len = r.u8()? as usize;
    let mut name: String = r.bytes(name_len)?.iter().map(|&c| c as char).collect();
    r.skip((4 - (1 + name_len) % 4) % 4)?;
    let blocks = Blocks::read(r, extra_end, false, &LAYER_KEYS)?;
    r.pos = extra_end;

    if let Some(b) = blocks.get(b"luni") {
        if let Ok(s) = Reader::new(b).unicode() {
            name = s;
        }
    }
    let section = blocks.get(b"lsct").or(blocks.get(b"lsdk"));
    let section_type = section.filter(|b| b.len() >= 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let blend_mode = match section.filter(|b| b.len() >= 12) {
        Some(b) => BlendMode::from_key(&b[8..12]),
        None => BlendMode::from_key(&blend_key),
    };
    let has = |keys: &[&[u8; 4]]| blocks.present.iter().any(|k| keys.contains(&k));
    let kind = match section_type {
        Some(1 | 2) => LayerKind::Group,
        Some(3) => LayerKind::GroupEnd,
        _ if blocks.get(b"TySh").is_some() => LayerKind::Text,
        _ if has(&FILL_KEYS) || has(&[b"vscg"]) => LayerKind::Fill,
        _ if has(&ADJUSTMENT_KEYS) => LayerKind::Adjustment,
        _ if has(&[b"SoLd", b"SoLE", b"PlLd"]) => LayerKind::SmartObject,
        _ => LayerKind::Pixel,
    };
    let layer = Layer {
        name,
        bounds,
        blend_mode,
        opacity,
        fill_opacity: blocks.get(b"iOpa").and_then(|b| b.first().copied()).unwrap_or(255),
        clipping,
        hidden: flags & 2 != 0,
        kind,
        mask: None,
        vector_density: 1.0,
        vector_feather: 0.0,
        channels: HashMap::new(),
        blocks: blocks.data.iter().map(|(k, v)| (*k, v.to_vec())).collect(),
    };
    Ok(Record { layer, channels, mask_info })
}

fn read_layer_info(r: &mut Reader, end: usize, depth: u16) -> Result<Vec<Layer>> {
    if r.pos + 2 > end {
        return Ok(vec![]);
    }
    let count = r.i16()?.unsigned_abs() as usize;
    let mut records = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        records.push(read_record(r)?);
    }
    let mut layers = Vec::with_capacity(count);
    for mut rec in records {
        let has_real = rec.channels.iter().any(|c| c.0 == -3);
        let info = parse_mask_info(&rec.mask_info, has_real);
        let l = &mut rec.layer;
        for &(id, len) in &rec.channels {
            let channel_end = r.pos.saturating_add(len).min(end);
            if len >= 2 {
                let compression = Compression::from_u16(r.u16()?)?;
                let rect = match id {
                    -2 => info.user.map(|m| m.0).unwrap_or_default(),
                    -3 => info.real.map(|m| m.0).unwrap_or_default(),
                    _ => l.bounds,
                };
                if rect.width() > 0 && rect.height() > 0 {
                    let px = decode_channel(r, compression, rect.width(), rect.height(), depth, channel_end, id < 0)?;
                    l.channels.insert(id, px);
                }
            }
            r.pos = channel_end;
        }
        let has_vector = l.blocks.contains_key(b"vmsk") || l.blocks.contains_key(b"vsms");
        let user = match (info.real, l.channels.remove(&-3)) {
            (Some(real), Some(data)) => Some((real, data)),
            // A mask rendered from the vector mask, which is drawn from its path instead.
            _ if has_vector && info.user.is_some_and(|u| u.2 & 8 != 0) => None,
            _ => info.user.zip(l.channels.remove(&-2)),
        };
        if let Some(((rect, default, flags), data)) = user {
            l.mask = Some(Mask {
                rect,
                default,
                disabled: flags & 2 != 0,
                data,
                density: info.user_density,
                feather: info.user_feather,
            });
        } else if let Some((rect, default, flags)) = info.user.filter(|u| u.0.width() == 0 && u.2 & 2 == 0) {
            if default == 0 && !l.blocks.contains_key(b"vmsk") && !l.blocks.contains_key(b"vsms") {
                l.mask = Some(Mask {
                    rect,
                    default,
                    disabled: flags & 2 != 0,
                    data: vec![],
                    density: info.user_density,
                    feather: 0.0,
                });
            }
        }
        l.vector_density = info.vector_density;
        l.vector_feather = info.vector_feather;
        layers.push(rec.layer);
    }
    Ok(layers)
}

fn read_composite(
    r: &mut Reader,
    width: usize,
    height: usize,
    channels: usize,
    depth: u16,
    color_channels: usize,
) -> Result<Vec<Vec<u8>>> {
    if r.remaining() <= 2 {
        return Ok(vec![]);
    }
    let compression = Compression::from_u16(r.u16()?)?;
    let stride = if depth == 1 { width.div_ceil(8) } else { width * depth as usize / 8 };
    let end = r.data.len();
    let mut out = Vec::with_capacity(channels);
    match compression {
        Compression::Rle => {
            let mut counts = Vec::with_capacity(height * channels);
            for _ in 0..height * channels {
                counts.push(if r.psb { r.u32()? as usize } else { r.u16()? as usize });
            }
            for c in 0..channels {
                let mut raw = Vec::with_capacity(stride * height);
                for &n in &counts[c * height..(c + 1) * height] {
                    let src = r.bytes(n.min(end - r.pos))?;
                    decode_packbits(src, &mut raw, stride);
                }
                out.push(to_8bit(raw, width, height, depth, c >= color_channels)?);
            }
        }
        Compression::Raw => {
            for c in 0..channels {
                let mut raw = r.bytes((stride * height).min(end - r.pos))?.to_vec();
                raw.resize(stride * height, 0);
                out.push(to_8bit(raw, width, height, depth, c >= color_channels)?);
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Reads one `Patt`/`Pat2`/`Pat3` block into `out`, keyed by pattern id.
fn read_patterns(data: &[u8], out: &mut HashMap<String, Pattern>) {
    let mut r = Reader::new(data);
    while r.remaining() >= 4 {
        let Ok(len) = r.u32() else { break };
        let start = r.pos;
        let next = (start + len as usize).next_multiple_of(4);
        if let Ok((id, p)) =
            read_pattern(&mut Reader::at(&data[..(start + len as usize).min(data.len())], start, false))
        {
            out.entry(id).or_insert(p);
        }
        if next <= r.pos || next > data.len() {
            break;
        }
        r.pos = next;
    }
}

fn read_pattern(r: &mut Reader) -> Result<(String, Pattern)> {
    let _version = r.u32()?;
    let mode = r.u32()?;
    let _height = r.u16()?;
    let _width = r.u16()?;
    let _name = r.unicode()?;
    let id_len = r.u8()? as usize;
    let id = String::from_utf8_lossy(r.bytes(id_len)?).into_owned();
    let mut palette = vec![];
    if mode == 2 {
        palette = r.bytes(768)?.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect::<Vec<_>>();
        r.skip(4)?;
    }
    let _vma_version = r.u32()?;
    let vma_len = r.u32()? as usize;
    let vma_end = (r.pos + vma_len).min(r.data.len());
    let rect = Rect::read(r)?;
    let count = r.u32()? as usize;
    let (w, h) = (rect.width(), rect.height());
    if w == 0 || h == 0 || w * h > 1 << 28 {
        bail!("bad pattern size");
    }
    let mut planes: Vec<Option<Vec<u8>>> = vec![];
    for _ in 0..count + 2 {
        if r.pos + 4 > vma_end {
            break;
        }
        let written = r.u32()?;
        if written == 0 {
            planes.push(None);
            continue;
        }
        let len = r.u32()? as usize;
        if len == 0 {
            planes.push(None);
            continue;
        }
        let end = (r.pos + len).min(vma_end);
        let depth = r.u32()?;
        let prect = Rect::read(r)?;
        let _depth16 = r.u16()?;
        let compression = r.u8()?;
        let (pw, ph) = (prect.width(), prect.height());
        let depth = if depth == 0 { 8 } else { depth as u16 };
        let plane = match compression {
            0 => {
                let stride = pw * depth as usize / 8;
                let mut raw = r.bytes((stride * ph).min(end.saturating_sub(r.pos)))?.to_vec();
                raw.resize(stride * ph, 0);
                to_8bit(raw, pw, ph, depth, false)?
            }
            1 => decode_channel(r, Compression::Rle, pw, ph, depth, end, false)?,
            2 => decode_channel(r, Compression::Zip, pw, ph, depth, end, false)?,
            _ => decode_channel(r, Compression::ZipPredicted, pw, ph, depth, end, false)?,
        };
        planes.push((pw == w && ph == h).then_some(plane));
        r.pos = end;
    }
    let plane = |i: usize| planes.get(i).and_then(|p| p.as_deref());
    let colors = match mode {
        1 | 2 | 8 => 1,
        4 => 4,
        _ => 3,
    };
    let alpha = if count > colors { plane(colors) } else { None }.or(plane(count + 1));
    let mut rgba = vec![255u8; w * h * 4];
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        let get = |c: usize| plane(c).map_or(0, |p| p[i]);
        match mode {
            1 | 8 => px[..3].fill(get(0)),
            2 => px[..3].copy_from_slice(palette.get(get(0) as usize).unwrap_or(&[0, 0, 0])),
            4 => {
                let k = get(3) as u32;
                for c in 0..3 {
                    px[c] = (get(c) as u32 * k / 255) as u8;
                }
            }
            _ => {
                for c in 0..3 {
                    px[c] = get(c);
                }
            }
        }
        if let Some(a) = alpha {
            px[3] = a[i];
        }
    }
    Ok((id, Pattern { width: w, height: h, rgba }))
}

/// Reads the embedded files of a `lnk2`/`lnkD`/`lnk3` block into `out`, keyed by unique id.
fn read_linked(data: &[u8], out: &mut HashMap<String, Vec<u8>>) {
    let mut r = Reader::new(data);
    while r.remaining() >= 8 {
        let Ok(len) = r.u64() else { break };
        let start = r.pos;
        let Some(next) = start.checked_add(len as usize).map(|n| n.next_multiple_of(4)) else { break };
        let mut e = Reader::at(&data[..(start + len as usize).min(data.len())], start, false);
        let mut entry = || -> Result<()> {
            let kind = e.tag()?;
            let _version = e.u32()?;
            let id_len = e.u8()? as usize;
            let id = String::from_utf8_lossy(e.bytes(id_len)?).into_owned();
            let _file_name = e.unicode()?;
            let _file_type = e.tag()?;
            let _creator = e.tag()?;
            let size = e.u64()? as usize;
            if e.u8()? != 0 {
                e.u32()?;
                descriptor::read(&mut e)?;
            }
            if &kind == b"liFD" {
                out.insert(id, e.bytes(size)?.to_vec());
            }
            Ok(())
        };
        let _ = entry();
        if next <= start || next > data.len() {
            break;
        }
        r.pos = next;
    }
}

impl Document {
    #[cfg(test)]
    pub(crate) fn blank(width: u32, height: u32) -> Document {
        Document {
            width,
            height,
            depth: 8,
            color_mode: ColorMode::Rgb,
            layers: vec![],
            channel_count: 3,
            global_angle: 120.0,
            global_altitude: 30.0,
            palette: vec![],
            transparent_index: None,
            composite: vec![],
            patterns: HashMap::new(),
            linked: HashMap::new(),
            icc_profile: None,
        }
    }

    /// Replaces the text of every type layer named `name` (see [`Layer::set_text`]) and returns
    /// how many were changed.
    pub fn set_text(&mut self, name: &str, text: &str) -> Result<usize> {
        let mut n = 0;
        for l in self.layers.iter_mut().filter(|l| l.name == name && l.kind == LayerKind::Text) {
            l.set_text(text)?;
            n += 1;
        }
        Ok(n)
    }

    /// Reads and parses a PSD or PSB file.
    pub fn open(path: impl AsRef<Path>) -> Result<Document> {
        Document::parse(&std::fs::read(path)?)
    }

    /// Parses a PSD or PSB file held in memory.
    pub fn parse(data: &[u8]) -> Result<Document> {
        let mut r = Reader::new(data);
        if &r.tag()? != b"8BPS" {
            bail!("missing 8BPS signature");
        }
        let version = r.u16()?;
        if version != 1 && version != 2 {
            bail!("unsupported version {version}");
        }
        r.psb = version == 2;
        r.skip(6)?;
        let channel_count = r.u16()?;
        let height = r.u32()?;
        let width = r.u32()?;
        let depth = r.u16()?;
        if width == 0 || height == 0 {
            bail!("empty canvas ({width}x{height})");
        }
        if ![1, 8, 16, 32].contains(&depth) {
            bail!("unsupported bit depth {depth}");
        }
        let color_mode = match r.u16()? {
            0 => ColorMode::Bitmap,
            1 => ColorMode::Grayscale,
            2 => ColorMode::Indexed,
            3 => ColorMode::Rgb,
            4 => ColorMode::Cmyk,
            7 => ColorMode::Multichannel,
            8 => ColorMode::Duotone,
            9 => ColorMode::Lab,
            _ => bail!("unknown color mode"),
        };
        let color_data_len = r.u32()? as usize;
        let color_data = r.bytes(color_data_len)?;
        let palette = if color_mode == ColorMode::Indexed && color_data.len() >= 768 {
            (0..256).map(|i| [color_data[i], color_data[256 + i], color_data[512 + i]]).collect()
        } else {
            vec![]
        };

        let resources_len = r.u32()? as usize;
        let resources_end = r.pos + resources_len;
        let mut global_angle = 120.0;
        let mut global_altitude = 30.0;
        let mut transparent_index = None;
        let mut icc_profile = None;
        while r.pos + 12 <= resources_end {
            if &r.tag()? != b"8BIM" {
                break;
            }
            let id = r.u16()?;
            let name_len = r.u8()? as usize;
            r.skip(name_len + (name_len + 1) % 2)?;
            let size = r.u32()? as usize;
            let body = r.bytes(size)?;
            match id {
                1037 if size >= 4 => global_angle = Reader::new(body).i32()? as f64,
                1049 if size >= 4 => global_altitude = Reader::new(body).i32()? as f64,
                1039 => icc_profile = Some(body.to_vec()),
                1047 if size >= 2 => transparent_index = Some(Reader::new(body).u16()?.min(255) as u8),
                _ => {}
            }
            r.skip(size & 1)?;
        }
        r.pos = resources_end;

        let layer_mask_len = r.length()?;
        let layer_mask_end = r.pos + layer_mask_len;
        let mut layers = vec![];
        let mut patterns = HashMap::new();
        let mut linked = HashMap::new();
        if layer_mask_len > 0 {
            let info_len = r.length()?;
            let info_end = r.pos + info_len;
            if info_len > 0 {
                layers = read_layer_info(&mut r, info_end, depth)?;
            }
            r.pos = info_end;
            if r.pos + 4 <= layer_mask_end {
                let global_mask = r.u32()? as usize;
                r.skip(global_mask)?;
            }
            let globals = Blocks::read(&mut r, layer_mask_end, true, &GLOBAL_KEYS)?;
            if layers.is_empty() {
                if let Some(b) = [b"Layr", b"Lr16", b"Lr32"].iter().find_map(|k| globals.get(k)) {
                    layers = read_layer_info(&mut Reader::at(b, 0, r.psb), b.len(), depth)?;
                }
            }
            for key in [b"Patt", b"Pat2", b"Pat3"] {
                globals.all(*key).for_each(|b| read_patterns(b, &mut patterns));
            }
            for key in [b"lnk2", b"lnkD", b"lnk3"] {
                globals.all(*key).for_each(|b| read_linked(b, &mut linked));
            }
        }
        r.pos = layer_mask_end;
        let composite = read_composite(
            &mut r,
            width as usize,
            height as usize,
            channel_count as usize,
            depth,
            color_mode.channels(),
        )?;
        Ok(Document {
            width,
            height,
            depth,
            color_mode,
            layers,
            channel_count,
            global_angle,
            global_altitude,
            palette,
            transparent_index,
            composite,
            patterns,
            linked,
            icc_profile,
        })
    }
}
