//! Replacing the text of a type layer: the `TySh` block is rewritten with new characters and
//! style runs fitted to them, so it renders and saves like text typed in Photoshop.

use crate::error::{OptionExt, Result};
use crate::psd::descriptor;
use crate::psd::engine::{self, Node};
use crate::psd::reader::Reader;

/// Text as Photoshop stores it: `\r` between paragraphs and one at the end.
fn normalize(text: &str) -> String {
    let mut t = text.replace("\r\n", "\r").replace('\n', "\r");
    if !t.ends_with('\r') {
        t.push('\r');
    }
    t
}

/// Paragraph spans of `units` (UTF-16 code units), each ending with its `\r`.
fn paragraphs(units: &[u16]) -> Vec<(usize, usize)> {
    let mut spans = vec![];
    let mut start = 0;
    for (i, &u) in units.iter().enumerate() {
        if u == u16::from(b'\r') {
            spans.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < units.len() {
        spans.push((start, units.len()));
    }
    spans
}

/// Run index of every unit, from a run length array, padded with the last run.
fn run_of_each(lengths: &[Node], n: usize) -> Vec<usize> {
    let mut idx = vec![];
    for (i, len) in lengths.iter().enumerate() {
        idx.extend(std::iter::repeat_n(i, len.num().unwrap_or(0.0).max(0.0) as usize));
    }
    let last = idx.last().copied().unwrap_or(0);
    idx.resize(n, last);
    idx
}

/// Collapses per-unit run indices into `(run, length)` pairs.
fn runs(per_unit: impl IntoIterator<Item = usize>) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = vec![];
    for r in per_unit {
        match out.last_mut() {
            Some((last, n)) if *last == r => *n += 1,
            _ => out.push((r, 1)),
        }
    }
    out
}

/// Rewrites a run (`ParagraphRun` or `StyleRun`) with `(old run index, length)` pairs.
fn set_runs(run: &mut Node, new: &[(usize, usize)]) {
    let items = run.get("RunArray").map(|a| a.array().to_vec()).unwrap_or_default();
    if items.is_empty() {
        return;
    }
    let array = new.iter().map(|&(i, _)| items[i.min(items.len() - 1)].clone()).collect();
    let lengths = new.iter().map(|&(_, n)| Node::Integer(n as i64)).collect();
    if let Some(a) = run.get_mut("RunArray") {
        *a = Node::Array(array);
    }
    if let Some(l) = run.get_mut("RunLengthArray") {
        *l = Node::Array(lengths);
    }
}

/// Fits the paragraph and style runs of `editor` from its old text to `new` (both normalized).
/// New paragraph `i` takes the paragraph settings of old paragraph `i` (or the last one) and the
/// character style used most in it.
fn refit(editor: &mut Node, old: &[u16], new: &[u16]) {
    let (old_paras, new_paras) = (paragraphs(old), paragraphs(new));
    let lengths = |key: &str, editor: &Node| {
        editor.path(&[key, "RunLengthArray"]).map(|a| a.array().to_vec()).unwrap_or_default()
    };
    let para_of = run_of_each(&lengths("ParagraphRun", editor), old.len());
    let style_of = run_of_each(&lengths("StyleRun", editor), old.len());
    let mut para_units = vec![];
    let mut style_units = vec![];
    for (i, &(s, e)) in new_paras.iter().enumerate() {
        let (os, oe) = old_paras.get(i.min(old_paras.len().saturating_sub(1))).copied().unwrap_or((0, 0));
        let para = para_of.get(os).copied().unwrap_or(0);
        let mut counts: Vec<(usize, usize)> = vec![];
        for &st in &style_of[os.min(style_of.len())..oe.min(style_of.len())] {
            match counts.iter_mut().find(|(r, _)| *r == st) {
                Some(c) => c.1 += 1,
                None => counts.push((st, 1)),
            }
        }
        let style = counts.iter().max_by_key(|c| c.1).map_or_else(|| style_of.last().copied().unwrap_or(0), |c| c.0);
        para_units.extend(std::iter::repeat_n(para, e - s));
        style_units.extend(std::iter::repeat_n(style, e - s));
    }
    if let Some(run) = editor.get_mut("ParagraphRun") {
        set_runs(run, &runs(para_units));
    }
    if let Some(run) = editor.get_mut("StyleRun") {
        set_runs(run, &runs(style_units));
    }
}

/// Returns `block` (a `TySh` block) with its text replaced by `text`.
pub(crate) fn replace_text(block: &[u8], text: &str) -> Result<Vec<u8>> {
    let mut r = Reader::new(block);
    r.skip(2 + 6 * 8 + 2 + 4)?;
    let head = r.pos;
    let desc = descriptor::read(&mut Reader::at(block, head, false))?;
    let mut root = engine::parse(desc.raw("EngineData").or_format("type layer without EngineData")?)?;
    let new = normalize(text);
    let editor = root.get_mut("EngineDict").or_format("EngineData without EngineDict")?;
    let old: Vec<u16> = editor.path(&["Editor", "Text"]).and_then(Node::str).unwrap_or("").encode_utf16().collect();
    let units: Vec<u16> = new.encode_utf16().collect();
    refit(editor, &old, &units);
    if let Some(t) = editor.path_mut(&["Editor", "Text"]) {
        *t = Node::String(new.clone());
    }
    let engine_data = engine::write(&root);
    let spliced = descriptor::splice(&mut r, |k| match k {
        "Txt " => Some(descriptor::text_item(new.trim_end_matches('\r'))),
        "EngineData" => Some(descriptor::raw_item(&engine_data)),
        _ => None,
    })?;
    let mut out = block[..head].to_vec();
    out.extend_from_slice(&spliced);
    out.extend_from_slice(&block[r.pos..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn normalizes_paragraphs() {
        assert_eq!(normalize("a\nb\r\nc"), "a\rb\rc\r");
        assert_eq!(normalize("x\r"), "x\r");
    }

    #[test]
    fn refits_runs_per_paragraph() {
        let mut editor = engine::parse(
            b"<< /ParagraphRun << /RunArray [ << /P 0 >> << /P 1 >> ] /RunLengthArray [ 4 3 ] >>
                 /StyleRun << /RunArray [ << /S 0 >> << /S 1 >> << /S 2 >> ] /RunLengthArray [ 1 3 3 ] >> >>",
        )
        .unwrap();
        refit(&mut editor, &utf16("abc\rde\r"), &utf16("x\ryyyy\rzz\r"));
        let lens = |k: &str| {
            editor.path(&[k, "RunLengthArray"]).unwrap().array().iter().map(|n| n.num().unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(lens("ParagraphRun"), [2.0, 8.0]);
        assert_eq!(lens("StyleRun"), [2.0, 8.0]);
        let styles: Vec<f64> = editor
            .path(&["StyleRun", "RunArray"])
            .unwrap()
            .array()
            .iter()
            .map(|n| n.get("S").unwrap().num().unwrap())
            .collect();
        assert_eq!(styles, [1.0, 2.0]);
    }
}
