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
    /// Multichannel, duotone or Lab.
    Other,
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
pub enum LayerKind {
    /// Raster pixels (also shapes, smart objects and fills, through their cached pixels).
    Pixel,
    /// Editable type layer, re-rendered from its text data.
    Text,
    /// Adjustment layer; not rendered.
    Adjustment,
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
}

impl Mask {
    pub fn at(&self, x: i32, y: i32) -> u8 {
        let r = &self.rect;
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom && self.data.len() == r.width() * r.height() {
            self.data[(y - r.top) as usize * r.width() + (x - r.left) as usize]
        } else {
            self.default
        }
    }
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
    pub(crate) channels: HashMap<i16, Vec<u8>>,
    pub(crate) type_data: Option<Vec<u8>>,
    pub(crate) effects_data: Option<Vec<u8>>,
}

impl Layer {
    /// Text content of a type layer, with `\r` paragraph separators.
    pub fn text(&self) -> Option<String> {
        let tl = crate::text::TextLayer::parse(self.type_data.as_deref()?).ok()?;
        Some(tl.chars.iter().collect::<String>().trim_end_matches('\r').to_string())
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
    pub(crate) composite: Vec<Vec<u8>>,
}

const ADJUSTMENT_KEYS: [&[u8; 4]; 17] = [
    b"levl", b"curv", b"brit", b"hue2", b"hue ", b"blnc", b"vibA", b"expA", b"selc", b"mixr", b"grdm", b"phfl",
    b"nvrt", b"post", b"thrs", b"CgEd", b"clrL",
];

const LONG_KEYS: [&[u8; 4]; 14] = [
    b"LMsk", b"Lr16", b"Lr32", b"Layr", b"Mt16", b"Mt32", b"Mtrn", b"Alph", b"FMsk", b"lnk2", b"FEid", b"FXid",
    b"PxSD", b"cinf",
];

const LAYER_KEYS: [&[u8; 4]; 7] = [b"TySh", b"lfx2", b"lmfx", b"lsct", b"lsdk", b"iOpa", b"luni"];
const GLOBAL_KEYS: [&[u8; 4]; 3] = [b"Layr", b"Lr16", b"Lr32"];

/// Tagged blocks we care about, borrowed from the file buffer; `present` lists every key seen.
struct Blocks<'a> {
    data: HashMap<[u8; 4], &'a [u8]>,
    present: Vec<[u8; 4]>,
}

impl<'a> Blocks<'a> {
    fn read(r: &mut Reader<'a>, end: usize, pad4: bool, keep: &[&[u8; 4]]) -> Result<Self> {
        let mut blocks = Blocks { data: HashMap::new(), present: vec![] };
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
}

struct Record {
    layer: Layer,
    channels: Vec<(i16, usize)>,
    mask_info: Option<(Rect, u8, u8)>,
}

fn read_mask_info(r: &mut Reader) -> Result<Option<(Rect, u8, u8)>> {
    let len = r.u32()? as usize;
    let end = r.pos + len;
    let mut info = None;
    if len >= 18 {
        let rect = Rect::read(r)?;
        let default = r.u8()?;
        let flags = r.u8()?;
        info = Some((rect, default, flags));
        if len >= 36 {
            let mut q = Reader::at(r.data, r.pos, r.psb);
            if flags & 0x10 != 0 {
                let params = q.u8()?;
                for bit in 0..4 {
                    if params & (1 << bit) != 0 {
                        q.skip(if bit % 2 == 0 { 1 } else { 8 })?;
                    }
                }
            }
            if q.pos + 18 <= end {
                let real_flags = q.u8()?;
                let real_default = q.u8()?;
                info = Some((Rect::read(&mut q)?, real_default, real_flags));
            }
        }
    }
    r.pos = end;
    Ok(info)
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
    let mask_info = read_mask_info(r)?;
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
    let type_data = blocks.get(b"TySh").map(<[u8]>::to_vec);
    let kind = match section_type {
        Some(1 | 2) => LayerKind::Group,
        Some(3) => LayerKind::GroupEnd,
        _ if blocks.present.iter().any(|k| ADJUSTMENT_KEYS.contains(&k)) => LayerKind::Adjustment,
        _ if type_data.is_some() => LayerKind::Text,
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
        channels: HashMap::new(),
        type_data,
        effects_data: blocks.get(b"lmfx").or(blocks.get(b"lfx2")).map(<[u8]>::to_vec),
    };
    Ok(Record { layer, channels, mask_info })
}

fn read_layer_info(r: &mut Reader, end: usize, depth: u16) -> Result<Vec<Layer>> {
    if r.pos + 2 > end {
        return Ok(vec![]);
    }
    let count = r.i16()?.unsigned_abs() as usize;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        records.push(read_record(r)?);
    }
    let mut layers = Vec::with_capacity(count);
    for mut rec in records {
        let l = &mut rec.layer;
        for &(id, len) in &rec.channels {
            let channel_end = r.pos.saturating_add(len).min(end);
            if len >= 2 {
                let compression = Compression::from_u16(r.u16()?)?;
                let rect = if id == -2 || id == -3 { rec.mask_info.map(|m| m.0).unwrap_or_default() } else { l.bounds };
                if rect.width() > 0 && rect.height() > 0 {
                    let px = decode_channel(r, compression, rect.width(), rect.height(), depth, channel_end)?;
                    l.channels.insert(id, px);
                }
            }
            r.pos = channel_end;
        }
        if let Some((rect, default, flags)) = rec.mask_info {
            if let Some(data) = l.channels.remove(&-2) {
                l.mask = Some(Mask { rect, default, disabled: flags & 2 != 0, data });
            }
        }
        layers.push(rec.layer);
    }
    Ok(layers)
}

fn read_composite(r: &mut Reader, width: usize, height: usize, channels: usize, depth: u16) -> Result<Vec<Vec<u8>>> {
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
                out.push(to_8bit(raw, width, height, depth)?);
            }
        }
        Compression::Raw => {
            for _ in 0..channels {
                let mut raw = r.bytes((stride * height).min(end - r.pos))?.to_vec();
                raw.resize(stride * height, 0);
                out.push(to_8bit(raw, width, height, depth)?);
            }
        }
        _ => {}
    }
    Ok(out)
}

impl Document {
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
            _ => ColorMode::Other,
        };
        let color_data = r.u32()? as usize;
        r.skip(color_data)?;

        let resources_len = r.u32()? as usize;
        let resources_end = r.pos + resources_len;
        let mut global_angle = 120.0;
        while r.pos + 12 <= resources_end {
            if &r.tag()? != b"8BIM" {
                break;
            }
            let id = r.u16()?;
            let name_len = r.u8()? as usize;
            r.skip(name_len + (name_len + 1) % 2)?;
            let size = r.u32()? as usize;
            let body = r.bytes(size)?;
            if id == 1037 && size >= 4 {
                global_angle = Reader::new(body).i32()? as f64;
            }
            r.skip(size & 1)?;
        }
        r.pos = resources_end;

        let layer_mask_len = r.length()?;
        let layer_mask_end = r.pos + layer_mask_len;
        let mut layers = vec![];
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
                if let Some(b) = GLOBAL_KEYS.iter().find_map(|k| globals.get(k)) {
                    layers = read_layer_info(&mut Reader::at(b, 0, r.psb), b.len(), depth)?;
                }
            }
        }
        r.pos = layer_mask_end;
        let composite = read_composite(&mut r, width as usize, height as usize, channel_count as usize, depth)?;
        Ok(Document { width, height, depth, color_mode, layers, channel_count, global_angle, composite })
    }
}
