//! Minimal PSD/PSB writer used to build test documents.
#![allow(dead_code)]

use std::io::Write;

/// Descriptor value.
pub enum V {
    Num(f64),
    Unit(&'static str, f64),
    Text(String),
    Enum(&'static str, &'static str),
    Long(i32),
    Bool(bool),
    Obj(&'static str, Vec<(&'static str, V)>),
    Raw(Vec<u8>),
}

struct Buf(Vec<u8>);

impl Buf {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn len(&mut self, psb: bool, v: usize) {
        if psb {
            self.u64(v as u64)
        } else {
            self.u32(v as u32)
        }
    }
    fn key(&mut self, k: &str) {
        if k.len() == 4 {
            self.u32(0);
        } else {
            self.u32(k.len() as u32);
        }
        self.raw(k.as_bytes());
    }
    fn unicode(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().chain([0]).collect();
        self.u32(units.len() as u32);
        for u in units {
            self.u16(u);
        }
    }
    fn descriptor(&mut self, class: &str, items: &[(&str, V)]) {
        self.unicode("");
        self.key(class);
        self.u32(items.len() as u32);
        for (k, v) in items {
            self.key(k);
            self.value(v);
        }
    }
    fn value(&mut self, v: &V) {
        match v {
            V::Num(n) => {
                self.raw(b"doub");
                self.f64(*n);
            }
            V::Unit(u, n) => {
                self.raw(b"UntF");
                self.raw(u.as_bytes());
                self.f64(*n);
            }
            V::Text(s) => {
                self.raw(b"TEXT");
                self.unicode(s);
            }
            V::Enum(t, e) => {
                self.raw(b"enum");
                self.key(t);
                self.key(e);
            }
            V::Long(n) => {
                self.raw(b"long");
                self.i32(*n);
            }
            V::Bool(b) => {
                self.raw(b"bool");
                self.u8(*b as u8);
            }
            V::Obj(class, items) => {
                self.raw(b"Objc");
                self.descriptor(class, items);
            }
            V::Raw(data) => {
                self.raw(b"tdta");
                self.u32(data.len() as u32);
                self.raw(data);
            }
        }
    }
}

pub fn descriptor(class: &str, items: &[(&str, V)]) -> Vec<u8> {
    let mut b = Buf(vec![]);
    b.descriptor(class, items);
    b.0
}

/// Channel compression written by the builder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compression {
    Raw,
    Rle,
    Zip,
    ZipPredicted,
}

#[derive(Clone)]
pub struct Layer {
    name: String,
    rect: [i32; 4],
    channels: Vec<(i16, Vec<u8>)>,
    blend: [u8; 4],
    opacity: u8,
    clipping: bool,
    hidden: bool,
    compression: Compression,
    blocks: Vec<([u8; 4], Vec<u8>)>,
    mask: Option<([i32; 4], u8, Vec<u8>)>,
}

impl Layer {
    fn new(name: &str, rect: [i32; 4]) -> Layer {
        Layer {
            name: name.into(),
            rect,
            channels: vec![],
            blend: *b"norm",
            opacity: 255,
            clipping: false,
            hidden: false,
            compression: Compression::Raw,
            blocks: vec![],
            mask: None,
        }
    }

    /// Pixel layer at (`left`, `top`) whose RGBA comes from `f(x, y)` in layer coordinates.
    pub fn pixels(name: &str, left: i32, top: i32, w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> Layer {
        let mut planes: [Vec<u8>; 4] = std::array::from_fn(|_| Vec::with_capacity(w * h));
        for y in 0..h {
            for x in 0..w {
                let p = f(x, y);
                for c in 0..4 {
                    planes[c].push(p[c]);
                }
            }
        }
        let mut l = Layer::new(name, [top, left, top + h as i32, left + w as i32]);
        let [r, g, b, a] = planes;
        l.channels = vec![(-1, a), (0, r), (1, g), (2, b)];
        l
    }

    pub fn solid(name: &str, left: i32, top: i32, w: usize, h: usize, rgba: [u8; 4]) -> Layer {
        Layer::pixels(name, left, top, w, h, |_, _| rgba)
    }

    pub fn text(name: &str, spec: &Text) -> Layer {
        let mut l = Layer::new(name, [0; 4]);
        l.channels = vec![(-1, vec![]), (0, vec![]), (1, vec![]), (2, vec![])];
        l.blocks.push((*b"TySh", spec.tysh()));
        l
    }

    /// Opening record of a group; push it after the group's children.
    pub fn group(name: &str) -> Layer {
        let mut l = Layer::new(name, [0; 4]);
        l.channels = vec![(-1, vec![]), (0, vec![]), (1, vec![]), (2, vec![])];
        let mut b = Buf(vec![]);
        b.u32(1);
        b.raw(b"8BIMpass");
        l.blocks.push((*b"lsct", b.0));
        l
    }

    /// Divider that starts a group's children; push it before them.
    pub fn group_end() -> Layer {
        let mut l = Layer::new("</Layer group>", [0; 4]);
        l.channels = vec![(-1, vec![]), (0, vec![]), (1, vec![]), (2, vec![])];
        let mut b = Buf(vec![]);
        b.u32(3);
        l.blocks.push((*b"lsct", b.0));
        l
    }

    pub fn block(mut self, key: &[u8; 4], data: Vec<u8>) -> Self {
        self.blocks.push((*key, data));
        self
    }

    pub fn opacity(mut self, v: u8) -> Self {
        self.opacity = v;
        self
    }
    pub fn blend(mut self, key: &[u8; 4]) -> Self {
        self.blend = *key;
        self
    }
    pub fn clipped(mut self) -> Self {
        self.clipping = true;
        self
    }
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
    pub fn compression(mut self, c: Compression) -> Self {
        self.compression = c;
        self
    }
    pub fn fill(mut self, v: u8) -> Self {
        self.blocks.push((*b"iOpa", vec![v, 0, 0, 0]));
        self
    }
    pub fn effects(mut self, fx: &[Effect]) -> Self {
        self.blocks.push((*b"lfx2", effects_block(fx)));
        self
    }
    /// Raster mask over `rect = [top, left, bottom, right]` with values from `f(x, y)`.
    pub fn mask(mut self, rect: [i32; 4], default: u8, f: impl Fn(usize, usize) -> u8) -> Self {
        let (w, h) = ((rect[3] - rect[1]) as usize, (rect[2] - rect[0]) as usize);
        let data = (0..w * h).map(|i| f(i % w, i / w)).collect();
        self.mask = Some((rect, default, data));
        self
    }
    /// Cached pixels, as Photoshop stores them for type layers.
    pub fn with_pixels(mut self, left: i32, top: i32, w: usize, h: usize, rgba: [u8; 4]) -> Self {
        let px = Layer::solid("", left, top, w, h, rgba);
        self.rect = px.rect;
        self.channels = px.channels;
        self
    }
}

#[derive(Clone, Copy)]
pub enum Effect {
    Stroke { size: f64, rgb: [u8; 3], position: &'static str },
    Shadow { size: f64, distance: f64, angle: f64, spread: f64, rgb: [u8; 3], opacity: f64 },
    Glow { size: f64, rgb: [u8; 3], opacity: f64 },
    Overlay { rgb: [u8; 3] },
}

fn color(rgb: [u8; 3]) -> V {
    V::Obj(
        "RGBC",
        vec![("Rd  ", V::Num(rgb[0] as f64)), ("Grn ", V::Num(rgb[1] as f64)), ("Bl  ", V::Num(rgb[2] as f64))],
    )
}

fn effects_block(fx: &[Effect]) -> Vec<u8> {
    let mut items = vec![("Scl ", V::Unit("#Prc", 100.0)), ("masterFXSwitch", V::Bool(true))];
    for e in fx {
        let normal = V::Enum("BlnM", "Nrml");
        items.push(match *e {
            Effect::Stroke { size, rgb, position } => (
                "FrFX",
                V::Obj(
                    "FrFX",
                    vec![
                        ("enab", V::Bool(true)),
                        ("Styl", V::Enum("FStl", position)),
                        ("PntT", V::Enum("FrFl", "SClr")),
                        ("Md  ", normal),
                        ("Opct", V::Unit("#Prc", 100.0)),
                        ("Sz  ", V::Unit("#Pxl", size)),
                        ("Clr ", color(rgb)),
                    ],
                ),
            ),
            Effect::Shadow { size, distance, angle, spread, rgb, opacity } => (
                "DrSh",
                V::Obj(
                    "DrSh",
                    vec![
                        ("enab", V::Bool(true)),
                        ("Md  ", V::Enum("BlnM", "Mltp")),
                        ("Clr ", color(rgb)),
                        ("Opct", V::Unit("#Prc", opacity)),
                        ("uglg", V::Bool(false)),
                        ("lagl", V::Unit("#Ang", angle)),
                        ("Dstn", V::Unit("#Pxl", distance)),
                        ("Ckmt", V::Unit("#Prc", spread)),
                        ("blur", V::Unit("#Pxl", size)),
                        ("layerConceals", V::Bool(true)),
                    ],
                ),
            ),
            Effect::Glow { size, rgb, opacity } => (
                "OrGl",
                V::Obj(
                    "OrGl",
                    vec![
                        ("enab", V::Bool(true)),
                        ("Md  ", V::Enum("BlnM", "Scrn")),
                        ("Clr ", color(rgb)),
                        ("Opct", V::Unit("#Prc", opacity)),
                        ("Ckmt", V::Unit("#Prc", 0.0)),
                        ("blur", V::Unit("#Pxl", size)),
                    ],
                ),
            ),
            Effect::Overlay { rgb } => (
                "SoFi",
                V::Obj(
                    "SoFi",
                    vec![
                        ("enab", V::Bool(true)),
                        ("Md  ", normal),
                        ("Opct", V::Unit("#Prc", 100.0)),
                        ("Clr ", color(rgb)),
                    ],
                ),
            ),
        });
    }
    let mut b = Buf(vec![]);
    b.u32(0);
    b.u32(16);
    b.descriptor("null", &items);
    b.0
}

/// One style run of a type layer.
#[derive(Clone)]
pub struct Run {
    pub text: String,
    pub font: String,
    pub size: f64,
    pub rgb: [f64; 3],
    pub tracking: f64,
    pub faux_bold: bool,
    pub underline: bool,
}

impl Run {
    pub fn new(text: &str, font: &str, size: f64, rgb: [f64; 3]) -> Run {
        Run { text: text.into(), font: font.into(), size, rgb, tracking: 0.0, faux_bold: false, underline: false }
    }
}

/// A type layer.
#[derive(Clone)]
pub struct Text {
    pub runs: Vec<Run>,
    pub x: f64,
    pub y: f64,
    /// 0 left, 1 right, 2 center, 3-6 justified.
    pub justification: u8,
    pub box_bounds: Option<[f64; 4]>,
    /// Text-space bounds Photoshop stores; also the warp rectangle.
    pub bounds: Option<[f64; 4]>,
    pub warp: Option<(&'static str, f64)>,
    pub anti_alias: &'static str,
    pub leading: Option<f64>,
    pub vertical: bool,
    /// Extra EngineData style properties for every run.
    pub style: &'static str,
}

fn engine_string(s: &str) -> Vec<u8> {
    let mut out = b"(\xFE\xFF".to_vec();
    for u in s.encode_utf16() {
        for b in u.to_be_bytes() {
            if matches!(b, b'(' | b')' | b'\\') {
                out.push(b'\\');
            }
            out.push(b);
        }
    }
    out.push(b')');
    out
}

impl Text {
    pub fn new(text: &str, font: &str, size: f64, rgb: [f64; 3], x: f64, y: f64) -> Text {
        Text {
            runs: vec![Run::new(text, font, size, rgb)],
            x,
            y,
            justification: 0,
            box_bounds: None,
            bounds: None,
            warp: None,
            anti_alias: "AnCr",
            leading: None,
            vertical: false,
            style: "",
        }
    }

    pub fn runs(runs: Vec<Run>, x: f64, y: f64) -> Text {
        Text { runs, ..Text::new("", "", 12.0, [0.0; 3], x, y) }
    }

    pub fn center(mut self) -> Self {
        self.justification = 2;
        self
    }

    pub fn vertical(mut self) -> Self {
        self.vertical = true;
        self
    }

    pub fn style(mut self, extra: &'static str) -> Self {
        self.style = extra;
        self
    }

    pub fn boxed(mut self, b: [f64; 4]) -> Self {
        self.box_bounds = Some(b);
        self
    }

    pub fn warp(mut self, style: &'static str, bend: f64, bounds: Option<[f64; 4]>) -> Self {
        self.warp = Some((style, bend));
        self.bounds = bounds.or(self.bounds);
        self
    }

    fn engine_data(&self) -> Vec<u8> {
        let text: String = self.runs.iter().map(|r| r.text.as_str()).collect::<String>().replace('\n', "\r") + "\r";
        let total = text.encode_utf16().count();
        let mut fonts: Vec<&str> = vec![];
        for r in &self.runs {
            if !fonts.contains(&r.font.as_str()) {
                fonts.push(&r.font);
            }
        }
        let mut e = Vec::new();
        e.extend_from_slice(b"<<\n/EngineDict <<\n/Editor << /Text ");
        e.extend(engine_string(&text));
        write!(
            e,
            " >>\n/ParagraphRun << /RunArray [ << /ParagraphSheet << /DefaultStyleSheet 0 /Properties << /Justification {} >> >> >> ] /RunLengthArray [ {total} ] >>\n/StyleRun << /RunArray [",
            self.justification
        )
        .unwrap();
        let mut lengths = vec![];
        for (i, r) in self.runs.iter().enumerate() {
            let mut len = r.text.encode_utf16().count();
            if i + 1 == self.runs.len() {
                len += 1;
            }
            lengths.push(len.to_string());
            let font = fonts.iter().position(|f| *f == r.font).unwrap();
            let leading = match self.leading {
                Some(l) => format!("/AutoLeading false /Leading {l}"),
                None => "/AutoLeading true".into(),
            };
            write!(
                e,
                " << /StyleSheet << /StyleSheetData << /Font {font} /FontSize {} {leading} /Tracking {} /FauxBold {} /Underline {} /FillColor << /Type 1 /Values [ 1.0 {} {} {} ] >> {} >> >> >>",
                r.size,
                r.tracking,
                r.faux_bold,
                r.underline,
                r.rgb[0] / 255.0,
                r.rgb[1] / 255.0,
                r.rgb[2] / 255.0,
                self.style
            )
            .unwrap();
        }
        writeln!(e, " ] /RunLengthArray [ {} ] >>", lengths.join(" ")).unwrap();
        let shape = match self.box_bounds {
            Some([l, t, r, b]) => format!("/ShapeType 1 /Cookie << /Photoshop << /BoxBounds [ {l} {t} {r} {b} ] >> >>"),
            None => "/ShapeType 0".into(),
        };
        write!(
            e,
            "/Rendered << /Shapes << /WritingDirection {} /Children [ << {shape} >> ] >> >>\n>>\n",
            if self.vertical { 2 } else { 0 }
        )
        .unwrap();
        e.extend_from_slice(b"/ResourceDict << /FontSet [");
        for f in &fonts {
            e.extend_from_slice(b" << /Name ");
            e.extend(engine_string(f));
            e.extend_from_slice(b" /Type 0 >>");
        }
        e.extend_from_slice(
            b" ] /StyleSheetSet [ << /StyleSheetData << /Font 0 /FontSize 12 >> >> ] /ParagraphSheetSet [ << /Properties << >> >> ] /TheNormalStyleSheet 0 /TheNormalParagraphSheet 0 >>\n>>",
        );
        e
    }

    fn tysh(&self) -> Vec<u8> {
        let mut b = Buf(vec![]);
        b.u16(1);
        for v in [1.0, 0.0, 0.0, 1.0, self.x, self.y] {
            b.f64(v);
        }
        b.u16(50);
        b.u32(16);
        let text: String = self.runs.iter().map(|r| r.text.as_str()).collect();
        let mut items = vec![
            ("Txt ", V::Text(text)),
            ("AntA", V::Enum("Annt", self.anti_alias)),
            ("TextIndex", V::Long(0)),
            ("EngineData", V::Raw(self.engine_data())),
        ];
        if let Some([l, t, r, bt]) = self.bounds {
            items.push((
                "bounds",
                V::Obj(
                    "bounds",
                    vec![
                        ("Left", V::Unit("#Pnt", l)),
                        ("Top ", V::Unit("#Pnt", t)),
                        ("Rght", V::Unit("#Pnt", r)),
                        ("Btom", V::Unit("#Pnt", bt)),
                    ],
                ),
            ));
        }
        b.descriptor("TxLr", &items);
        b.u16(1);
        b.u32(16);
        let (style, bend) = self.warp.unwrap_or(("warpNone", 0.0));
        b.descriptor(
            "warp",
            &[
                ("warpStyle", V::Enum("warpStyle", style)),
                ("warpValue", V::Num(bend)),
                ("warpPerspective", V::Num(0.0)),
                ("warpPerspectiveOther", V::Num(0.0)),
                ("warpRotate", V::Enum("Ornt", "Hrzn")),
            ],
        );
        for _ in 0..4 {
            b.i32(0);
        }
        b.0
    }
}

fn packbits(row: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    let mut i = 0;
    while i < row.len() {
        let mut run = 1;
        while i + run < row.len() && run < 128 && row[i + run] == row[i] {
            run += 1;
        }
        if run >= 3 {
            out.push((1 - run as i32) as i8 as u8);
            out.push(row[i]);
            i += run;
        } else {
            let start = i;
            while i < row.len()
                && i - start < 128
                && !(i + 2 < row.len() && row[i] == row[i + 1] && row[i] == row[i + 2])
            {
                i += 1;
            }
            out.push((i - start - 1) as u8);
            out.extend_from_slice(&row[start..i]);
        }
    }
    out
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// Builds a PSD (or PSB) in memory.
pub struct Psd {
    pub width: u32,
    pub height: u32,
    pub psb: bool,
    pub depth: u16,
    pub layers: Vec<Layer>,
    pub composite: Option<[u8; 3]>,
    pub global_angle: Option<i32>,
}

impl Psd {
    pub fn new(width: u32, height: u32) -> Psd {
        Psd { width, height, psb: false, depth: 8, layers: vec![], composite: None, global_angle: None }
    }

    /// Adds a layer above the previous ones.
    pub fn layer(mut self, l: Layer) -> Self {
        self.layers.push(l);
        self
    }

    /// Samples at the document depth, big-endian.
    fn samples(&self, v: &[u8]) -> Vec<u8> {
        match self.depth {
            16 => v.iter().flat_map(|&s| (s as u16 * 257).to_be_bytes()).collect(),
            _ => v.to_vec(),
        }
    }

    fn channel(&self, data: &[u8], w: usize, c: Compression) -> Vec<u8> {
        let data = self.samples(data);
        let bps = self.depth as usize / 8;
        let mut b = Buf(vec![]);
        match c {
            Compression::Raw => {
                b.u16(0);
                b.raw(&data);
            }
            Compression::Rle => {
                b.u16(1);
                let rows: Vec<Vec<u8>> = data.chunks(w * bps).map(packbits).collect();
                for r in &rows {
                    if self.psb {
                        b.u32(r.len() as u32)
                    } else {
                        b.u16(r.len() as u16)
                    }
                }
                rows.iter().for_each(|r| b.raw(r));
            }
            Compression::Zip => {
                b.u16(2);
                b.raw(&zlib(&data));
            }
            Compression::ZipPredicted => {
                b.u16(3);
                let mut d = data.clone();
                if bps == 1 {
                    for row in d.chunks_mut(w) {
                        for x in (1..row.len()).rev() {
                            row[x] = row[x].wrapping_sub(row[x - 1]);
                        }
                    }
                } else {
                    for row in d.chunks_mut(w * 2) {
                        for x in (1..w).rev() {
                            let cur = u16::from_be_bytes([row[2 * x], row[2 * x + 1]]);
                            let prev = u16::from_be_bytes([row[2 * x - 2], row[2 * x - 1]]);
                            row[2 * x..2 * x + 2].copy_from_slice(&cur.wrapping_sub(prev).to_be_bytes());
                        }
                    }
                }
                b.raw(&zlib(&d));
            }
        }
        b.0
    }

    pub fn build(&self) -> Vec<u8> {
        let mut b = Buf(vec![]);
        b.raw(b"8BPS");
        b.u16(if self.psb { 2 } else { 1 });
        b.raw(&[0; 6]);
        b.u16(3);
        b.u32(self.height);
        b.u32(self.width);
        b.u16(self.depth);
        b.u16(3);
        b.u32(0);

        let mut res = Buf(vec![]);
        if let Some(angle) = self.global_angle {
            res.raw(b"8BIM");
            res.u16(1037);
            res.u16(0);
            res.u32(4);
            res.i32(angle);
        }
        b.u32(res.0.len() as u32);
        b.raw(&res.0);

        let mut info = Buf(vec![]);
        if !self.layers.is_empty() {
            info.i16(self.layers.len() as i16);
            let mut image_data = Buf(vec![]);
            for l in &self.layers {
                let w = (l.rect[3] - l.rect[1]) as usize;
                for v in l.rect {
                    info.i32(v);
                }
                let mut chans: Vec<(i16, Vec<u8>)> = l
                    .channels
                    .iter()
                    .map(|(id, d)| (*id, if d.is_empty() { vec![0, 0] } else { self.channel(d, w, l.compression) }))
                    .collect();
                if let Some((rect, _, data)) = &l.mask {
                    let mw = (rect[3] - rect[1]) as usize;
                    chans.push((-2, self.channel(data, mw, l.compression)));
                }
                info.u16(chans.len() as u16);
                for (id, d) in &chans {
                    info.i16(*id);
                    info.len(self.psb, d.len());
                    image_data.raw(d);
                }
                info.raw(b"8BIM");
                info.raw(&l.blend);
                info.u8(l.opacity);
                info.u8(l.clipping as u8);
                info.u8(if l.hidden { 2 } else { 0 });
                info.u8(0);
                let mut extra = Buf(vec![]);
                match &l.mask {
                    Some((rect, default, _)) => {
                        extra.u32(20);
                        rect.iter().for_each(|&v| extra.i32(v));
                        extra.u8(*default);
                        extra.u8(0);
                        extra.u16(0);
                    }
                    None => extra.u32(0),
                }
                extra.u32(0);
                let name: Vec<u8> =
                    l.name.chars().map(|c| if c.is_ascii() { c as u8 } else { b'?' }).take(255).collect();
                extra.u8(name.len() as u8);
                extra.raw(&name);
                while extra.0.len() % 4 != 0 {
                    extra.u8(0);
                }
                let mut luni = Buf(vec![]);
                luni.unicode(&l.name);
                for (key, data) in [(*b"luni", luni.0)].iter().chain(l.blocks.iter()) {
                    extra.raw(b"8BIM");
                    extra.raw(key);
                    let mut d = data.clone();
                    if d.len() % 2 == 1 {
                        d.push(0);
                    }
                    extra.u32(d.len() as u32);
                    extra.raw(&d);
                }
                info.u32(extra.0.len() as u32);
                info.raw(&extra.0);
            }
            info.raw(&image_data.0);
            if info.0.len() % 2 == 1 {
                info.u8(0);
            }
        }
        let mut lm = Buf(vec![]);
        lm.len(self.psb, info.0.len());
        lm.raw(&info.0);
        lm.u32(0);
        b.len(self.psb, lm.0.len());
        b.raw(&lm.0);

        b.u16(1);
        let (w, h) = (self.width as usize, self.height as usize);
        let rgb = self.composite.unwrap_or([255; 3]);
        let rows: Vec<Vec<u8>> = rgb.iter().map(|&c| packbits(&self.samples(&vec![c; w]))).collect();
        for r in &rows {
            for _ in 0..h {
                if self.psb {
                    b.u32(r.len() as u32)
                } else {
                    b.u16(r.len() as u16)
                }
            }
        }
        for r in &rows {
            for _ in 0..h {
                b.raw(r);
            }
        }
        b.0
    }
}

/// A system font for text tests, or `None` (tests then skip).
pub fn test_font() -> Option<(psd_compiler::FontDb, &'static str)> {
    let mut db = psd_compiler::FontDb::new();
    db.add_system_fonts();
    ["DejaVuSans-Bold", "DejaVuSans", "LiberationSans-Bold", "Arial-BoldMT", "ArialMT"]
        .into_iter()
        .find(|f| db.contains(f))
        .map(|f| (db, f))
}
