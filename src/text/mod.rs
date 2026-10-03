//! Type layers: text engine data model, line layout, glyph outlines and warps.

pub(crate) mod edit;
pub(crate) mod layout;
pub(crate) mod path;
pub(crate) mod warp;

use crate::error::{OptionExt, Result};
use crate::psd::descriptor::{self, Descriptor};
use crate::psd::engine::{self, Node};
use crate::psd::reader::Reader;
use warp::Warp;

/// Anti-aliasing method of a type layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AntiAlias {
    None,
    Sharp,
    Crisp,
    Strong,
    Smooth,
}

/// Edge-coverage exponent that reproduces Photoshop's "Strong" anti-aliasing.
const STRONG_EXPONENT: f32 = 0.55;

impl AntiAlias {
    fn from_key(key: &str) -> AntiAlias {
        match key {
            "Anno" => AntiAlias::None,
            "AnSh" => AntiAlias::Sharp,
            "AnSt" => AntiAlias::Strong,
            "AnSm" => AntiAlias::Smooth,
            _ => AntiAlias::Crisp,
        }
    }

    /// Exponent applied to partial edge coverage; values below one make edges fuller.
    pub fn coverage_exponent(self) -> f32 {
        match self {
            AntiAlias::Strong => STRONG_EXPONENT,
            AntiAlias::Crisp | AntiAlias::Smooth => (1.0 + STRONG_EXPONENT) / 2.0,
            AntiAlias::None | AntiAlias::Sharp => 1.0,
        }
    }
}

/// Character style resolved from the style sheet chain.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Style {
    pub font: String,
    pub size: f64,
    pub leading: f64,
    pub auto_leading: bool,
    pub tracking: f64,
    pub hscale: f64,
    pub vscale: f64,
    pub faux_bold: bool,
    pub faux_italic: bool,
    pub kerning: bool,
    pub baseline_shift: f64,
    pub caps: Caps,
    pub underline: bool,
    pub strikethrough: bool,
    pub color: [f32; 4],
    /// OpenType features switched from their defaults, as `(tag, value)`.
    pub features: Vec<([u8; 4], u32)>,
    /// Synthetic superscript (1) or subscript (2).
    pub synthetic_position: u8,
}

/// OpenType features Photoshop exposes as character style flags: key, tag, default.
const FEATURE_FLAGS: [(&str, &[u8; 4], bool); 11] = [
    ("Ligatures", b"liga", true),
    ("Ligatures", b"clig", true),
    ("DLigatures", b"dlig", false),
    ("AltLigatures", b"hlig", false),
    ("ContextualLigatures", b"calt", true),
    ("OldStyle", b"onum", false),
    ("Fractions", b"frac", false),
    ("Ordinals", b"ordn", false),
    ("Swash", b"swsh", false),
    ("Titling", b"titl", false),
    ("StylisticAlternates", b"salt", false),
];

/// The features of style properties `m` that differ from shaping defaults.
fn features(m: &Props) -> Vec<([u8; 4], u32)> {
    let mut out: Vec<([u8; 4], u32)> = FEATURE_FLAGS
        .iter()
        .filter_map(|&(key, tag, default)| {
            let on = flag(m, key, default);
            (on != default).then_some((*tag, on as u32))
        })
        .collect();
    if flag(m, "Ornaments", false) {
        out.push((*b"ornm", 1));
    }
    let position = match num(m, "FontOTPosition", 0.0) as i64 {
        1 => Some(b"sups"),
        2 => Some(b"subs"),
        3 => Some(b"numr"),
        4 => Some(b"dnom"),
        _ => None,
    };
    out.extend(position.map(|t| (*t, 1)));
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caps {
    Normal,
    Small,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Justification {
    Left,
    Right,
    Center,
    JustifyLeft,
    JustifyRight,
    JustifyCenter,
    JustifyAll,
}

impl Justification {
    fn from_index(i: i64) -> Justification {
        match i {
            1 => Justification::Right,
            2 => Justification::Center,
            3 => Justification::JustifyLeft,
            4 => Justification::JustifyRight,
            5 => Justification::JustifyCenter,
            6 => Justification::JustifyAll,
            _ => Justification::Left,
        }
    }

    /// Fraction of the free space placed before the line: 0 left, 0.5 centered, 1 right.
    pub fn offset(self) -> f64 {
        match self {
            Justification::Right | Justification::JustifyRight => 1.0,
            Justification::Center | Justification::JustifyCenter => 0.5,
            _ => 0.0,
        }
    }

    pub fn justifies(self, last_line: bool) -> bool {
        match self {
            Justification::JustifyAll => true,
            Justification::JustifyLeft | Justification::JustifyRight | Justification::JustifyCenter => !last_line,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Paragraph {
    pub justification: Justification,
    pub first_indent: f64,
    pub start_indent: f64,
    pub end_indent: f64,
    pub space_before: f64,
    pub space_after: f64,
    pub auto_leading: f64,
}

impl Default for Paragraph {
    fn default() -> Self {
        Paragraph {
            justification: Justification::Left,
            first_indent: 0.0,
            start_indent: 0.0,
            end_indent: 0.0,
            space_before: 0.0,
            space_after: 0.0,
            auto_leading: 1.2,
        }
    }
}

/// A type layer: characters with per-character style and paragraph, placed by `transform`.
#[derive(Clone, Debug)]
pub(crate) struct TextLayer {
    pub chars: Vec<char>,
    pub styles: Vec<Style>,
    pub paragraphs: Vec<Paragraph>,
    pub transform: [f64; 6],
    pub box_bounds: Option<[f64; 4]>,
    pub warp: Warp,
    pub bounds: Option<[f64; 4]>,
    pub vertical: bool,
    pub anti_alias: AntiAlias,
}

type Props = Vec<(String, Node)>;

fn merge(base: &Node, over: Option<&Node>) -> Props {
    let mut m = base.entries().to_vec();
    for (k, v) in over.map(Node::entries).unwrap_or_default() {
        match m.iter_mut().find(|(a, _)| a == k) {
            Some(slot) => slot.1 = v.clone(),
            None => m.push((k.clone(), v.clone())),
        }
    }
    m
}

fn prop<'a>(m: &'a Props, key: &str) -> Option<&'a Node> {
    m.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn num(m: &Props, key: &str, default: f64) -> f64 {
    prop(m, key).and_then(Node::num).unwrap_or(default)
}

fn flag(m: &Props, key: &str, default: bool) -> bool {
    prop(m, key).and_then(Node::boolean).unwrap_or(default)
}

fn color(node: Option<&Node>) -> Option<[f32; 4]> {
    let v: Vec<f32> = node?.get("Values")?.array().iter().filter_map(Node::num).map(|x| x as f32).collect();
    match v[..] {
        [a, r, g, b] => Some([r, g, b, a]),
        [a, gray] => Some([gray, gray, gray, a]),
        _ => None,
    }
}

/// Maps each character to its run index, given run lengths.
fn expand_runs(lengths: &[Node], n: usize) -> Vec<usize> {
    let mut idx = Vec::with_capacity(n);
    for (i, len) in lengths.iter().enumerate() {
        idx.extend(std::iter::repeat_n(i, len.num().unwrap_or(0.0).max(0.0) as usize));
    }
    let last = idx.last().copied().unwrap_or(0);
    idx.resize(n, last);
    idx
}

fn rect_of(d: &Descriptor) -> [f64; 4] {
    let get = |k| d.num(k).unwrap_or(0.0);
    [get("Left"), get("Top "), get("Rght"), get("Btom")]
}

impl TextLayer {
    /// Parses a `TySh` block.
    pub fn parse(block: &[u8]) -> Result<TextLayer> {
        let mut r = Reader::new(block);
        r.u16()?;
        let mut transform = [0.0; 6];
        for v in &mut transform {
            *v = r.f64()?;
        }
        r.u16()?;
        r.u32()?;
        let text_desc = descriptor::read(&mut r)?;
        let warp_desc = (|| -> Result<Descriptor> {
            r.u16()?;
            r.u32()?;
            descriptor::read(&mut r)
        })()
        .unwrap_or_default();

        let root = engine::parse(text_desc.raw("EngineData").or_format("type layer without EngineData")?)?;
        let editor = root.get("EngineDict").or_format("EngineData without EngineDict")?;
        let resources = root
            .get("ResourceDict")
            .or_else(|| root.get("DocumentResources"))
            .or_format("EngineData without ResourceDict")?;
        let chars: Vec<char> = editor.path(&["Editor", "Text"]).and_then(Node::str).unwrap_or("").chars().collect();
        let (styles, paragraphs) = resolve_runs(editor, resources, chars.len());

        let shape = editor.path(&["Rendered", "Shapes", "Children"]).and_then(|c| c.array().first());
        let box_bounds = shape
            .filter(|s| s.get("ShapeType").and_then(Node::num) == Some(1.0))
            .and_then(|s| s.path(&["Cookie", "Photoshop", "BoxBounds"]))
            .and_then(|b| match b.array().iter().filter_map(Node::num).collect::<Vec<_>>()[..] {
                [l, t, r, b, ..] => Some([l, t, r, b]),
                _ => None,
            });
        let vertical = editor.path(&["Rendered", "Shapes", "WritingDirection"]).and_then(Node::num) == Some(2.0);
        let warp = Warp::from_descriptor(&warp_desc);
        Ok(TextLayer {
            chars,
            styles,
            paragraphs,
            transform,
            box_bounds,
            warp,
            bounds: text_desc.desc("bounds").map(rect_of),
            vertical,
            anti_alias: AntiAlias::from_key(text_desc.enumerated("AntA").unwrap_or("AnCr")),
        })
    }
}

/// Resolves style and paragraph runs into one `Style` and `Paragraph` per character.
fn resolve_runs(editor: &Node, resources: &Node, n: usize) -> (Vec<Style>, Vec<Paragraph>) {
    let fonts: Vec<&str> = resources
        .get("FontSet")
        .map(|f| f.array().iter().filter_map(|x| x.get("Name").and_then(Node::str)).collect())
        .unwrap_or_default();
    let sheets = resources.get("StyleSheetSet").map(Node::array).unwrap_or_default();
    let para_sheets = resources.get("ParagraphSheetSet").map(Node::array).unwrap_or_default();
    let normal_style = resources.get("TheNormalStyleSheet").and_then(Node::num).unwrap_or(0.0) as usize;
    let normal_para = resources.get("TheNormalParagraphSheet").and_then(Node::num).unwrap_or(0.0) as usize;
    let runs = |key: &str| {
        let run = editor.get(key);
        let items = run.and_then(|r| r.get("RunArray")).map(Node::array).unwrap_or_default();
        let lengths = run.and_then(|r| r.get("RunLengthArray")).map(Node::array).unwrap_or_default();
        (items, expand_runs(lengths, n))
    };

    let (para_runs, para_idx) = runs("ParagraphRun");
    let para_base = para_sheets.get(normal_para).and_then(|s| s.get("Properties")).unwrap_or(Node::empty());
    let mut para_defs: Vec<(Paragraph, usize)> = para_runs
        .iter()
        .map(|run| {
            let sheet = run.get("ParagraphSheet");
            let m = merge(para_base, sheet.and_then(|s| s.get("Properties")));
            let default_style =
                sheet.and_then(|s| s.get("DefaultStyleSheet")).and_then(Node::num).map_or(normal_style, |x| x as usize);
            let para = Paragraph {
                justification: Justification::from_index(num(&m, "Justification", 0.0) as i64),
                first_indent: num(&m, "FirstLineIndent", 0.0),
                start_indent: num(&m, "StartIndent", 0.0),
                end_indent: num(&m, "EndIndent", 0.0),
                space_before: num(&m, "SpaceBefore", 0.0),
                space_after: num(&m, "SpaceAfter", 0.0),
                auto_leading: num(&m, "AutoLeading", 1.2),
            };
            (para, default_style)
        })
        .collect();
    if para_defs.is_empty() {
        para_defs.push((Paragraph::default(), normal_style));
    }

    let (style_runs, style_idx) = runs("StyleRun");
    let mut styles = Vec::with_capacity(n);
    let mut paragraphs = Vec::with_capacity(n);
    for i in 0..n {
        let (para, sheet) = &para_defs[para_idx[i].min(para_defs.len() - 1)];
        paragraphs.push(para.clone());
        let base = sheets
            .get(*sheet)
            .or(sheets.get(normal_style))
            .and_then(|s| s.get("StyleSheetData"))
            .unwrap_or(Node::empty());
        let run = style_runs.get(style_idx[i]).and_then(|r| r.path(&["StyleSheet", "StyleSheetData"]));
        let m = merge(base, run);
        styles.push(Style {
            font: fonts.get(num(&m, "Font", 0.0) as usize).copied().unwrap_or_default().to_string(),
            size: num(&m, "FontSize", 12.0),
            leading: num(&m, "Leading", 0.0),
            auto_leading: flag(&m, "AutoLeading", true),
            tracking: num(&m, "Tracking", 0.0),
            hscale: num(&m, "HorizontalScale", 1.0),
            vscale: num(&m, "VerticalScale", 1.0),
            faux_bold: flag(&m, "FauxBold", false),
            faux_italic: flag(&m, "FauxItalic", false),
            kerning: flag(&m, "AutoKerning", true),
            baseline_shift: num(&m, "BaselineShift", 0.0),
            caps: match num(&m, "FontCaps", 0.0) as i64 {
                1 => Caps::Small,
                2 => Caps::All,
                _ => Caps::Normal,
            },
            underline: flag(&m, "Underline", false),
            strikethrough: flag(&m, "Strikethrough", false),
            color: color(prop(&m, "FillColor")).unwrap_or([0.0, 0.0, 0.0, 1.0]),
            features: features(&m),
            synthetic_position: num(&m, "FontBaseline", 0.0).clamp(0.0, 2.0) as u8,
        });
    }
    (styles, paragraphs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGINE: &[u8] = b"<< /EngineDict << /Editor << /Text (Hello!) >>
        /ParagraphRun << /RunArray [ << /ParagraphSheet << /Properties << /Justification 2 >> >> >> ]
                         /RunLengthArray [ 6 ] >>
        /StyleRun << /RunArray [
            << /StyleSheet << /StyleSheetData << /Font 1 /FontSize 30 /FillColor << /Values [ 1 1 0 0 ] >> >> >> >>
            << /StyleSheet << /StyleSheetData << /AutoKerning false /FontCaps 2 >> >> >> ]
          /RunLengthArray [ 3 3 ] >> >>
        /ResourceDict << /FontSet [ << /Name (Base) >> << /Name (Bold) >> ]
          /StyleSheetSet [ << /StyleSheetData << /Font 0 /FontSize 12 /Tracking 50 >> >> ] >> >>";

    #[test]
    fn resolves_runs_with_inheritance() {
        let root = engine::parse(ENGINE).unwrap();
        let (styles, paras) = resolve_runs(root.get("EngineDict").unwrap(), root.get("ResourceDict").unwrap(), 6);
        assert_eq!(styles.len(), 6);
        assert_eq!(styles[0].font, "Bold");
        assert_eq!(styles[0].size, 30.0);
        assert_eq!(styles[0].tracking, 50.0);
        assert_eq!(styles[0].color, [1.0, 0.0, 0.0, 1.0]);
        assert!(styles[0].kerning);
        assert_eq!(styles[4].font, "Base");
        assert!(!styles[4].kerning);
        assert_eq!(styles[4].caps, Caps::All);
        assert!(paras.iter().all(|p| p.justification == Justification::Center));
    }

    #[test]
    fn expands_runs_and_pads_with_last() {
        let lens = [Node::Number(2.0), Node::Number(1.0)];
        assert_eq!(expand_runs(&lens, 5), [0, 0, 1, 1, 1]);
        assert_eq!(expand_runs(&[], 2), [0, 0]);
    }

    #[test]
    fn justification_rules() {
        assert_eq!(Justification::from_index(1).offset(), 1.0);
        assert_eq!(Justification::Center.offset(), 0.5);
        assert!(Justification::JustifyLeft.justifies(false));
        assert!(!Justification::JustifyLeft.justifies(true));
        assert!(Justification::JustifyAll.justifies(true));
        assert!(!Justification::Left.justifies(false));
    }

    #[test]
    fn anti_alias_exponents() {
        assert_eq!(AntiAlias::from_key("AnSt").coverage_exponent(), STRONG_EXPONENT);
        assert_eq!(AntiAlias::from_key("AnSh").coverage_exponent(), 1.0);
        assert_eq!(AntiAlias::from_key("???"), AntiAlias::Crisp);
    }

    #[test]
    fn colors_from_argb_and_gray() {
        let argb = engine::parse(b"<< /Values [ 0.5 1 0.25 0 ] >>").unwrap();
        assert_eq!(color(Some(&argb)), Some([1.0, 0.25, 0.0, 0.5]));
        let gray = engine::parse(b"<< /Values [ 1 0.5 ] >>").unwrap();
        assert_eq!(color(Some(&gray)), Some([0.5, 0.5, 0.5, 1.0]));
    }
}
