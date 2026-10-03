//! Shaping and Photoshop-style line layout of a type layer, producing glyph outlines in text space.

use rustybuzz::ttf_parser::{GlyphId, Tag};

use super::path::{self, Outline, Path, Seg};
use super::warp;
use super::{Caps, Style, TextLayer};
use crate::fonts::FontDb;

const SMALL_CAPS_SCALE: f64 = 0.75;
const FAUX_ITALIC_SKEW: f64 = 0.2;
const FAUX_BOLD_WIDTH: f64 = 0.04;
const DECORATION_THICKNESS: f64 = 0.05;
const UNDERLINE_OFFSET: f64 = 0.1;
const STRIKETHROUGH_OFFSET: f64 = -0.3;
const FALLBACK_ASCENT: f64 = 0.8;
const WARP_FLATTEN_DIVISOR: f64 = 40.0;
/// Synthetic superscript and subscript: size and baseline offset, as fractions of the font size.
const SYNTHETIC_SCALE: f64 = 0.583;
const SYNTHETIC_SHIFT: f64 = 0.333;
/// Vertical text: the ideographic em box spans this much of the size above the baseline.
const EM_ASCENT: f64 = 0.88;
/// Distance from the baseline to the center of the em box.
const EM_CENTER: f64 = EM_ASCENT - 0.5;
/// How far corner punctuation moves up and right in vertical text.
const CORNER_SHIFT: f64 = 0.5;

const PARAGRAPH_BREAK: char = '\r';
const LINE_BREAK: char = '\u{3}';

/// A filled outline in text space.
pub(crate) struct Glyph {
    pub path: Path,
    pub color: [f32; 4],
    /// Faux-bold stroke width in text units; zero when not bold.
    pub bold: f64,
}

pub(crate) struct Layout {
    pub glyphs: Vec<Glyph>,
    /// Fonts that were not found, and the face drawn instead.
    pub substitutions: Vec<(String, Option<String>)>,
}

struct PlacedGlyph {
    id: u16,
    font: usize,
    dx: f64,
    dy: f64,
    /// Vertical text: the font has no vertical form, so the glyph moves to the upper right.
    corner: bool,
}

struct Item {
    ch: char,
    style: usize,
    advance: f64,
    glyphs: Vec<PlacedGlyph>,
}

struct Faces<'a> {
    db: &'a FontDb,
    cache: Vec<Option<Option<rustybuzz::Face<'a>>>>,
}

impl<'a> Faces<'a> {
    fn new(db: &'a FontDb) -> Self {
        Faces { db, cache: vec![] }
    }

    fn get(&mut self, i: usize) -> Option<&rustybuzz::Face<'a>> {
        if self.cache.len() <= i {
            self.cache.resize_with(i + 1, || None);
        }
        let db = self.db;
        self.cache[i].get_or_insert_with(|| db.face(i)).as_ref()
    }

    fn has_glyph(&mut self, i: usize, c: char) -> bool {
        self.get(i).is_some_and(|f| f.glyph_index(c).is_some())
    }
}

fn apply_caps(c: char, caps: Caps) -> (char, f64) {
    let upper = || c.to_uppercase().next().unwrap_or(c);
    match caps {
        Caps::All => (upper(), 1.0),
        Caps::Small if c.is_lowercase() => (upper(), SMALL_CAPS_SCALE),
        _ => (c, 1.0),
    }
}

struct Run {
    font: Option<usize>,
    upright: bool,
    start: usize,
    chars: Vec<(char, char, f64)>,
}

/// Characters set upright in vertical text (CJK, kana, hangul, fullwidth forms and symbols);
/// everything else is turned 90 degrees clockwise, as are brackets and long marks whose upright
/// form is their turned shape.
pub(crate) fn upright(c: char) -> bool {
    let turned = matches!(c as u32, 0x3008..=0x3011 | 0x3014..=0x301C | 0x30FC | 0xFF08 | 0xFF09 | 0xFF3B | 0xFF3D | 0xFF5B..=0xFF60);
    !turned
        && matches!(c as u32,
        0x1100..=0x11FF
            | 0x2E80..=0x2FFF
            | 0x3000..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE1F
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
            | 0x1F000..=0x1FAFF
            | 0x20000..=0x3FFFF)
}

/// Commas and stops that move to the upper right of the em box in vertical text.
fn corner_punctuation(c: char) -> bool {
    matches!(c, '\u{3001}' | '\u{3002}' | '\u{FF0C}' | '\u{FF0E}')
}

/// Shapes `chars` (all sharing `style`) and appends one item per character.
///
/// `kerning[i]` is the AutoKerning flag of character `i`. Photoshop lets that flag govern the pair
/// formed with the previous character, so each glyph takes the kerned or unkerned advance depending
/// on the flag of the character that follows it.
fn shape_span(
    faces: &mut Faces,
    chars: &[char],
    kerning: &[bool],
    style: &Style,
    primary: Option<usize>,
    out: &mut Vec<Item>,
    first_style: usize,
    vertical: bool,
) {
    let mut runs: Vec<Run> = vec![];
    for (i, &c) in chars.iter().enumerate() {
        let (shaped, scale) = apply_caps(c, style.caps);
        let font = match primary {
            Some(p) if shaped.is_control() || faces.has_glyph(p, shaped) => Some(p),
            _ if shaped.is_whitespace() || shaped.is_control() => primary,
            _ => faces.db.fallback_for(shaped).or(primary),
        };
        let up = vertical && upright(shaped);
        match runs.last_mut() {
            Some(run) if run.font == font && run.upright == up => run.chars.push((c, shaped, scale)),
            _ => runs.push(Run { font, upright: up, start: i, chars: vec![(c, shaped, scale)] }),
        }
    }
    for run in runs {
        let base = out.len();
        out.extend(run.chars.iter().enumerate().map(|(i, &(ch, _, _))| Item {
            ch,
            style: first_style + run.start + i,
            advance: 0.0,
            glyphs: vec![],
        }));
        let Some(font) = run.font else { continue };
        let Some(face) = faces.get(font) else { continue };
        let upem = face.units_per_em() as f64;
        let text: String = run.chars.iter().map(|&(_, s, _)| if s.is_control() { ' ' } else { s }).collect();
        let shape = |kern: bool| {
            let mut buf = rustybuzz::UnicodeBuffer::new();
            buf.push_str(&text);
            let mut features: Vec<rustybuzz::Feature> =
                style.features.iter().map(|(t, v)| rustybuzz::Feature::new(Tag::from_bytes(t), *v, ..)).collect();
            if !kern {
                features.push(rustybuzz::Feature::new(Tag::from_bytes(b"kern"), 0, ..));
            }
            if run.upright {
                features.push(rustybuzz::Feature::new(Tag::from_bytes(b"vert"), 1, ..));
            }
            rustybuzz::shape(face, &features, buf)
        };
        let kern_after = |i: usize| kerning.get(run.start + i + 1).copied().unwrap_or(style.kerning);
        let all_on = (0..run.chars.len()).all(kern_after);
        let all_off = (0..run.chars.len()).all(|i| !kern_after(i));
        let shaped = shape(!all_off);
        let unkerned = (!all_on && !all_off).then(|| shape(false)).filter(|g| g.len() == shaped.len());
        // Glyphs `vert` left alone, for corner punctuation.
        let unsubstituted: Vec<u32> = if run.upright && run.chars.iter().any(|c| corner_punctuation(c.1)) {
            text.chars().map(|c| face.glyph_index(c).map_or(0, |g| g.0 as u32)).collect()
        } else {
            vec![]
        };

        let mut byte_to_char = vec![0usize; text.len() + 1];
        for (ci, (b, _)) in text.char_indices().enumerate() {
            byte_to_char[b] = ci;
        }
        let mut pen = vec![0f64; run.chars.len()];
        for (gi, (info, pos)) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()).enumerate() {
            let ci = byte_to_char[info.cluster as usize];
            let pos = match &unkerned {
                Some(u) if !kern_after(ci) => &u.glyph_positions()[gi],
                _ => pos,
            };
            let (ch, _, caps_scale) = run.chars[ci];
            let scale = style.size * caps_scale / upem;
            let item = &mut out[base + ci];
            if !ch.is_control() && !ch.is_whitespace() {
                let corner = corner_punctuation(run.chars[ci].1) && unsubstituted.get(ci) == Some(&info.glyph_id);
                item.glyphs.push(PlacedGlyph {
                    id: info.glyph_id as u16,
                    font,
                    dx: pen[ci] + pos.x_offset as f64 * scale * style.hscale,
                    dy: pos.y_offset as f64 * scale * style.vscale,
                    corner,
                });
            }
            let advance = pos.x_advance as f64 * scale * style.hscale;
            pen[ci] += advance;
            item.advance += advance;
        }
    }
    let start = out.len() - chars.len();
    for item in &mut out[start..] {
        if !item.ch.is_control() {
            item.advance += style.tracking / 1000.0 * style.size;
        }
        if item.ch == PARAGRAPH_BREAK || item.ch == LINE_BREAK {
            item.advance = 0.0;
        }
    }
}

fn same_span(a: &Style, b: &Style) -> bool {
    a.font == b.font
        && (a.size - b.size).abs() < 1e-9
        && a.tracking == b.tracking
        && a.hscale == b.hscale
        && a.caps == b.caps
        && a.features == b.features
}

/// `styles` with synthetic superscript and subscript turned into size and baseline shift.
fn effective_styles(styles: &[Style]) -> Vec<Style> {
    styles
        .iter()
        .map(|s| {
            let mut s = s.clone();
            if s.synthetic_position != 0 {
                let dir = if s.synthetic_position == 1 { 1.0 } else { -1.0 };
                s.baseline_shift += dir * SYNTHETIC_SHIFT * s.size;
                s.size *= SYNTHETIC_SCALE;
                s.synthetic_position = 0;
            }
            s
        })
        .collect()
}

struct Line {
    start: usize,
    end: usize,
    first: bool,
    last_of_paragraph: bool,
}

/// Greedy word wrap of `items[start..end]` into lines no wider than `width`.
fn wrap(items: &[Item], start: usize, end: usize, width: impl Fn(bool) -> f64, first: bool, lines: &mut Vec<Line>) {
    let mut ls = start;
    loop {
        let is_first = first && ls == start;
        let avail = width(is_first);
        let (mut w, mut k, mut last_break) = (0.0, ls, None);
        while k < end {
            w += items[k].advance;
            if items[k].ch == ' ' {
                last_break = Some(k + 1);
            }
            if w > avail && k > ls && !items[k].ch.is_whitespace() {
                break;
            }
            k += 1;
        }
        let le = if k < end { last_break.filter(|&b| b > ls).unwrap_or(k.max(ls + 1)) } else { end };
        lines.push(Line { start: ls, end: le, first: is_first, last_of_paragraph: false });
        if le >= end {
            return;
        }
        ls = le;
    }
}

fn break_lines(tl: &TextLayer, items: &[Item], box_bounds: Option<[f64; 4]>) -> Vec<Line> {
    let n = items.len();
    let box_width = box_bounds.map(|b| b[2] - b[0]);
    let mut lines = vec![];
    let mut ps = 0;
    while ps < n {
        let pe = (ps..n).find(|&i| items[i].ch == PARAGRAPH_BREAK).unwrap_or(n);
        let para = &tl.paragraphs[ps];
        let mut s = ps;
        let mut first = true;
        loop {
            let e = (s..pe).find(|&i| items[i].ch == LINE_BREAK).unwrap_or(pe);
            let before = lines.len();
            match box_width {
                Some(bw) => {
                    let width = |first: bool| {
                        bw - para.start_indent - para.end_indent - if first { para.first_indent } else { 0.0 }
                    };
                    wrap(items, s, e, width, first, &mut lines);
                }
                None => lines.push(Line { start: s, end: e, first, last_of_paragraph: false }),
            }
            first = false;
            if e >= pe {
                if let Some(last) = lines[before..].last_mut() {
                    last.last_of_paragraph = true;
                }
                break;
            }
            s = e + 1;
        }
        ps = pe + 1;
    }
    lines
}

/// Lays out a type layer: shaping, line breaking, alignment and warp, all in text space.
///
/// Vertical text is laid out as horizontal lines in a frame turned 90 degrees (lines become
/// columns running right to left), with upright characters turned back.
pub(crate) fn layout(tl: &TextLayer, db: &FontDb) -> Layout {
    let mut styles = effective_styles(&tl.styles);
    // Missing fonts: the closest face, with synthetic bold or italic where it lacks them.
    let mut resolved: Vec<(String, Option<crate::fonts::Match>)> = vec![];
    let mut substitutions: Vec<(String, Option<String>)> = vec![];
    for s in &mut styles {
        let m = match resolved.iter().find(|(f, _)| *f == s.font) {
            Some((_, m)) => *m,
            None => {
                let m = db.resolve(&s.font);
                if !m.is_some_and(|m| m.exact) {
                    substitutions.push((s.font.clone(), m.map(|m| db.name(m.face).to_owned())));
                }
                resolved.push((s.font.clone(), m));
                m
            }
        };
        if let Some(m) = m {
            s.faux_bold |= m.synthetic_bold;
            s.faux_italic |= m.synthetic_italic;
        }
    }
    let tl = &TextLayer { styles, ..tl.clone() };
    let vertical = tl.vertical;
    // In the turned frame u runs down the column and v leftward across columns: (x, y) = (-v, u).
    let box_bounds = if vertical { tl.box_bounds.map(|b| [b[1], -b[2], b[3], -b[0]]) } else { tl.box_bounds };
    let place = move |u: f64, v: f64| if vertical { (-v, u) } else { (u, v) };
    let mut faces = Faces::new(db);
    let n = tl.chars.len();
    let mut items: Vec<Item> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && same_span(&tl.styles[j], &tl.styles[i]) && tl.chars[j - 1] != PARAGRAPH_BREAK {
            j += 1;
        }
        let style = &tl.styles[i];
        let primary = resolved.iter().find(|(f, _)| *f == style.font).and_then(|(_, m)| m.map(|m| m.face));
        let kerning: Vec<bool> = tl.styles[i..(j + 1).min(n)].iter().map(|s| s.kerning).collect();
        shape_span(&mut faces, &tl.chars[i..j], &kerning, style, primary, &mut items, i, vertical);
        i = j;
    }
    let lines = break_lines(tl, &items, box_bounds);
    if lines.is_empty() {
        return Layout { glyphs: vec![], substitutions };
    }

    let leading = |k: usize| {
        let s = &tl.styles[k];
        if s.auto_leading {
            tl.paragraphs[k].auto_leading * s.size
        } else {
            s.leading
        }
    };
    let ascent = |faces: &mut Faces, k: usize| {
        let s = &tl.styles[k];
        if vertical {
            return EM_ASCENT * s.size;
        }
        db.find(&s.font)
            .and_then(|f| faces.get(f))
            .map_or(s.size * FALLBACK_ASCENT, |f| f.ascender() as f64 / f.units_per_em() as f64 * s.size)
    };

    let mut glyphs = vec![];
    let mut y = 0.0;
    for (li, line) in lines.iter().enumerate() {
        let last = n - 1;
        let probe =
            if line.end > line.start { line.start..line.end } else { line.start.min(last)..line.start.min(last) + 1 };
        let para = &tl.paragraphs[line.start.min(last)];
        if li == 0 {
            if let Some(b) = box_bounds {
                y = b[1] + probe.clone().map(|k| ascent(&mut faces, k)).fold(0.0, f64::max);
            } else if vertical {
                // Point text: the first column is centered on the anchor.
                y = probe.clone().map(|k| EM_CENTER * tl.styles[k].size).fold(0.0, f64::max);
            }
        } else {
            y += probe.clone().map(leading).fold(0.0, f64::max);
            if line.first {
                y += para.space_before + tl.paragraphs[lines[li - 1].start.min(last)].space_after;
            }
        }
        let mut end = line.end;
        while end > line.start && items[end - 1].ch.is_whitespace() {
            end -= 1;
        }
        let width: f64 = items[line.start..end].iter().map(|it| it.advance).sum();
        let first_indent = if line.first { para.first_indent } else { 0.0 };
        let indent = para.start_indent + first_indent;
        let (x0, extra_per_space) = match box_bounds {
            None => (indent - width * para.justification.offset(), 0.0),
            Some(b) => {
                let free = b[2] - b[0] - para.start_indent - para.end_indent - first_indent - width;
                let spaces = items[line.start..end].iter().filter(|it| it.ch == ' ').count() as f64;
                if para.justification.justifies(line.last_of_paragraph) && spaces > 0.0 {
                    (b[0] + indent, free / spaces)
                } else {
                    (b[0] + indent + free * para.justification.offset(), 0.0)
                }
            }
        };
        let mut x = x0;
        for item in &items[line.start..line.end] {
            let style = &tl.styles[item.style];
            for g in &item.glyphs {
                let Some(face) = faces.get(g.font) else { continue };
                let caps_scale =
                    if style.caps == Caps::Small && item.ch.is_lowercase() { SMALL_CAPS_SCALE } else { 1.0 };
                let scale = style.size * caps_scale / face.units_per_em() as f64;
                let (ox, oy) = (x + g.dx, y - g.dy - style.baseline_shift);
                let (hs, vs) = (style.hscale, style.vscale);
                let skew = if style.faux_italic { FAUX_ITALIC_SKEW } else { 0.0 };
                // Upright glyphs in vertical text turn a quarter counterclockwise about their em box.
                let turn = (vertical && upright(item.ch))
                    .then(|| (x + item.advance / 2.0, y - style.baseline_shift - EM_CENTER * style.size));
                let corner = if g.corner { CORNER_SHIFT * style.size } else { 0.0 };
                let mut outline = Outline {
                    path: vec![],
                    f: |fx: f32, fy: f32| {
                        let (ux, uy) = (fx as f64 * scale * hs, fy as f64 * scale * vs);
                        let (u, v) = (ox + ux + skew * uy, oy - uy);
                        let (u, v) = match turn {
                            Some((cu, cv)) => (cu + (v - cv) - corner, cv - (u - cu) - corner),
                            None => (u, v),
                        };
                        place(u, v)
                    },
                };
                face.outline_glyph(GlyphId(g.id), &mut outline);
                if !outline.path.is_empty() {
                    let bold = if style.faux_bold { style.size * FAUX_BOLD_WIDTH } else { 0.0 };
                    glyphs.push(Glyph { path: outline.path, color: style.color, bold });
                }
            }
            if !item.ch.is_control() {
                let thickness = style.size * DECORATION_THICKNESS;
                for (on, offset) in [(style.underline, UNDERLINE_OFFSET), (style.strikethrough, STRIKETHROUGH_OFFSET)] {
                    if on {
                        let yy = y + style.size * offset;
                        let (x1, y1) = (x + item.advance, yy + thickness);
                        let path =
                            vec![Seg::Move(x, yy), Seg::Line(x1, yy), Seg::Line(x1, y1), Seg::Line(x, y1), Seg::Close];
                        glyphs.push(Glyph { path: path::map(&path, place), color: style.color, bold: 0.0 });
                    }
                }
            }
            x += item.advance + if item.ch == ' ' { extra_per_space } else { 0.0 };
        }
    }
    if !tl.warp.is_identity() {
        apply_warp(tl, &mut glyphs);
    }
    Layout { glyphs, substitutions }
}

/// The warp envelope spans the bounds Photoshop stored for the layer, else the glyph bounds.
fn warp_rect(tl: &TextLayer, glyphs: &[Glyph]) -> Option<[f64; 4]> {
    tl.bounds.filter(|b| b[2] > b[0] && b[3] > b[1]).or_else(|| path::bounds(glyphs.iter().map(|g| &g.path)))
}

fn apply_warp(tl: &TextLayer, glyphs: &mut [Glyph]) {
    let Some(rect) = warp_rect(tl, glyphs) else { return };
    if rect[2] <= rect[0] || rect[3] <= rect[1] {
        return;
    }
    let size = tl.styles.first().map_or(12.0, |s| s.size);
    let vertical = tl.warp.vertical;
    let (w, rect) = if vertical {
        (warp::Warp { vertical: false, ..tl.warp.clone() }, [rect[1], rect[0], rect[3], rect[2]])
    } else {
        (tl.warp.clone(), rect)
    };
    let env = warp::Envelope::new(&w, rect);
    for g in glyphs {
        let flat = path::flatten(&g.path, size / WARP_FLATTEN_DIVISOR);
        g.path = path::map(&flat, |x, y| {
            if vertical {
                let (a, b) = env.eval(y, x);
                (b, a)
            } else {
                env.eval(x, y)
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ch: char, advance: f64) -> Item {
        Item { ch, style: 0, advance, glyphs: vec![] }
    }

    fn items(s: &str) -> Vec<Item> {
        s.chars().map(|c| item(c, 10.0)).collect()
    }

    #[test]
    fn wraps_at_spaces() {
        let it = items("aaa bbb ccc");
        let mut lines = vec![];
        wrap(&it, 0, it.len(), |_| 75.0, true, &mut lines);
        let spans: Vec<_> = lines.iter().map(|l| (l.start, l.end)).collect();
        assert_eq!(spans, [(0, 8), (8, 11)]);
        assert!(lines[0].first && !lines[1].first);
    }

    #[test]
    fn breaks_long_words_anywhere() {
        let it = items("abcdefgh");
        let mut lines = vec![];
        wrap(&it, 0, it.len(), |_| 35.0, true, &mut lines);
        assert_eq!(lines.iter().map(|l| l.end - l.start).collect::<Vec<_>>(), [3, 3, 2]);
    }

    #[test]
    fn caps_mapping() {
        assert_eq!(apply_caps('a', Caps::All), ('A', 1.0));
        assert_eq!(apply_caps('a', Caps::Small), ('A', SMALL_CAPS_SCALE));
        assert_eq!(apply_caps('A', Caps::Small), ('A', 1.0));
        assert_eq!(apply_caps('a', Caps::Normal), ('a', 1.0));
    }
}
