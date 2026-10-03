mod common;

use common::{test_font, Effect, Layer, Psd, Run, Text};
use psd_compiler::{render, Document, Image, RenderOptions};

fn ink(img: &Image) -> Vec<(u32, u32)> {
    let mut v = vec![];
    for y in 0..img.height {
        for x in 0..img.width {
            if img.pixel(x, y)[0] < 128 {
                v.push((x, y));
            }
        }
    }
    v
}

fn extent(px: &[(u32, u32)]) -> (u32, u32, u32, u32) {
    let xs = px.iter().map(|p| p.0);
    let ys = px.iter().map(|p| p.1);
    (xs.clone().min().unwrap(), ys.clone().min().unwrap(), xs.max().unwrap(), ys.max().unwrap())
}

macro_rules! font_or_skip {
    () => {
        match test_font() {
            Some(f) => f,
            None => {
                eprintln!("skipped: no DejaVu/Liberation/Arial font installed");
                return;
            }
        }
    };
}

fn white(w: u32, h: u32) -> Psd {
    Psd::new(w, h).layer(Layer::solid("bg", 0, 0, w as usize, h as usize, [255, 255, 255, 255]))
}

#[test]
fn renders_point_text_at_baseline() {
    let (db, font) = font_or_skip!();
    let text = Text::new("HELLO", font, 40.0, [0.0; 3], 20.0, 60.0);
    let doc = Document::parse(&white(300, 100).layer(Layer::text("t", &text)).build()).unwrap();
    let out = render(&doc, &db, &RenderOptions::default());
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let (x0, y0, x1, y1) = extent(&ink(&out.image));
    assert!((18..=26).contains(&x0), "left {x0}");
    assert!((57..=61).contains(&y1), "baseline {y1}");
    assert!((25..=35).contains(&y0), "cap top {y0}");
    assert!(x1 > 120 && x1 < 200, "right {x1}");
}

#[test]
fn center_justification_centers_on_anchor() {
    let (db, font) = font_or_skip!();
    let text = Text::new("Centered", font, 30.0, [0.0; 3], 150.0, 50.0).center();
    let doc = Document::parse(&white(300, 80).layer(Layer::text("t", &text)).build()).unwrap();
    let (x0, _, x1, _) = extent(&ink(&render(&doc, &db, &Default::default()).image));
    let mid = (x0 + x1) as i32 / 2;
    assert!((mid - 150).abs() <= 3, "{x0}..{x1}");
}

#[test]
fn paragraph_text_wraps_inside_box() {
    let (db, font) = font_or_skip!();
    let text = Text::new("one two three four five six seven", font, 20.0, [0.0; 3], 10.0, 10.0)
        .boxed([0.0, 0.0, 120.0, 200.0]);
    let doc = Document::parse(&white(200, 240).layer(Layer::text("t", &text)).build()).unwrap();
    let (x0, y0, x1, y1) = extent(&ink(&render(&doc, &db, &Default::default()).image));
    assert!(x0 >= 10 && x1 <= 132, "{x0}..{x1}");
    assert!(y0 >= 10 && y1 - y0 > 60, "{y0}..{y1}");
}

#[test]
fn style_runs_change_color() {
    let (db, font) = font_or_skip!();
    let text = Text::runs(
        vec![Run::new("RRR", font, 40.0, [255.0, 0.0, 0.0]), Run::new("BBB", font, 40.0, [0.0, 0.0, 255.0])],
        10.0,
        60.0,
    );
    let doc = Document::parse(&white(300, 90).layer(Layer::text("t", &text)).build()).unwrap();
    let img = render(&doc, &db, &Default::default()).image;
    let (mut red, mut blue) = (0, 0);
    for y in 0..img.height {
        for x in 0..img.width {
            match img.pixel(x, y) {
                [r, g, b, _] if r > 200 && g < 50 && b < 50 => red += 1,
                [r, g, b, _] if b > 200 && g < 50 && r < 50 => blue += 1,
                _ => {}
            }
        }
    }
    assert!(red > 100 && blue > 100, "red {red} blue {blue}");
}

#[test]
fn stroke_effect_on_text() {
    let (db, font) = font_or_skip!();
    let text = Text::new("O", font, 60.0, [255.0, 255.0, 255.0], 20.0, 80.0);
    let layer = Layer::text("t", &text).effects(&[Effect::Stroke { size: 4.0, rgb: [0, 0, 0], position: "OutF" }]);
    let doc = Document::parse(&white(120, 110).layer(layer).build()).unwrap();
    let img = render(&doc, &db, &Default::default()).image;
    assert!(ink(&img).len() > 400);
}

#[test]
fn warp_bends_baseline() {
    let (db, font) = font_or_skip!();
    let flat = Text::new("HHHHHHHH", font, 30.0, [0.0; 3], 20.0, 100.0);
    let arched = flat.clone().warp("warpArc", 50.0, None);
    let measure = |t: &Text| {
        let doc = Document::parse(&white(320, 200).layer(Layer::text("t", t)).build()).unwrap();
        let px = ink(&render(&doc, &db, &Default::default()).image);
        let (x0, _, x1, _) = extent(&px);
        let bottom = |x: u32| px.iter().filter(|p| p.0.abs_diff(x) <= 8).map(|p| p.1).max().unwrap_or(0) as i32;
        bottom(x0 + 8) - bottom((x0 + x1) / 2)
    };
    assert!(measure(&flat).abs() <= 2);
    assert!(measure(&arched) > 8, "arc should lift the middle");
}

#[test]
fn missing_font_warns() {
    let text = Text::new("Hi", "NoSuchFont-Regular", 20.0, [0.0; 3], 5.0, 20.0);
    let doc = Document::parse(&white(30, 30).layer(Layer::text("t", &text)).build()).unwrap();
    let out = render(&doc, &psd_compiler::FontDb::new(), &Default::default());
    assert!(out.warnings.iter().any(|w| w.message.contains("NoSuchFont-Regular")), "{:?}", out.warnings);
}

#[test]
fn keep_text_uses_cached_pixels() {
    let text = Text::new("Hi", "NoSuchFont-Regular", 20.0, [0.0; 3], 5.0, 20.0);
    let layer = Layer::text("t", &text).with_pixels(4, 4, 10, 10, [9, 9, 9, 255]);
    let doc = Document::parse(&white(30, 30).layer(layer).build()).unwrap();
    let opts = RenderOptions { keep_text_raster: true, ..Default::default() };
    let out = render(&doc, &psd_compiler::FontDb::new(), &opts);
    assert_eq!(out.image.pixel(8, 8), [9, 9, 9, 255]);
    assert_eq!(out.image.pixel(20, 20), [255, 255, 255, 255]);
}

#[test]
fn text_blends_with_gamma() {
    // Measured from Photoshop: red text at 40% coverage over white leaves green at 184, not 153.
    let text = Text::new("Hi", "NoSuchFont-Regular", 20.0, [1.0, 0.0, 0.0], 5.0, 20.0);
    let layer = Layer::text("t", &text).with_pixels(4, 4, 10, 10, [255, 0, 0, 102]);
    let doc = Document::parse(&white(30, 30).layer(layer).build()).unwrap();
    let opts = RenderOptions { keep_text_raster: true, ..Default::default() };
    let p = render(&doc, &psd_compiler::FontDb::new(), &opts).image.pixel(8, 8);
    assert!(p[0] == 255 && p[1].abs_diff(184) <= 3, "{p:?}");
}

#[test]
fn unreadable_text_uses_cached_pixels() {
    let layer = Layer::solid("t", 4, 4, 10, 10, [9, 9, 9, 255]).block(b"TySh", vec![0, 1, 2]);
    let doc = Document::parse(&white(30, 30).layer(layer).build()).unwrap();
    let out = render(&doc, &psd_compiler::FontDb::new(), &Default::default());
    assert!(out.warnings.iter().any(|w| w.message.contains("cached pixels")), "{:?}", out.warnings);
    assert_eq!(out.image.pixel(8, 8), [9, 9, 9, 255]);
}

#[test]
fn huge_text_renders_in_bands_without_seams() {
    let (db, font) = font_or_skip!();
    // A glyph far taller than the canvas, so it is clipped and drawn across several bands.
    let text = Text::new("I", font, 6000.0, [0.0; 3], 600.0, 3500.0);
    let doc = Document::parse(&white(3000, 3000).layer(Layer::text("t", &text)).build()).unwrap();
    let out = render(&doc, &db, &RenderOptions::default());
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let x = (600..3000).find(|&x| out.image.pixel(x, 1500)[0] == 0).expect("stem");
    assert!((0..3000).all(|y| out.image.pixel(x + 50, y)[0] == 0), "gap in the stem");
}

#[test]
fn text_masks_cover_glyphs() {
    let (db, font) = font_or_skip!();
    let text = Text::new("Mask", font, 30.0, [0.0; 3], 10.0, 40.0);
    let doc = Document::parse(&white(160, 60).layer(Layer::text("caption", &text)).build()).unwrap();
    let opts = RenderOptions { text_masks: true, ..Default::default() };
    let out = render(&doc, &db, &opts);
    assert_eq!(out.text_masks.len(), 1);
    let m = &out.text_masks[0];
    assert_eq!(m.name, "caption");
    let full = m.to_canvas(160, 60);
    for (x, y) in ink(&out.image) {
        assert!(full[(y * 160 + x) as usize] > 0, "ink at {x},{y} outside mask");
    }
}

#[test]
fn set_text_replaces_and_keeps_styles() {
    let text = Text::runs(
        vec![Run::new("RRR", "Base", 40.0, [255.0, 0.0, 0.0]), Run::new("BBB", "Base", 40.0, [0.0, 0.0, 255.0])],
        10.0,
        50.0,
    );
    let mut doc = Document::parse(&white(300, 80).layer(Layer::text("t", &text)).build()).unwrap();
    assert_eq!(doc.set_text("t", "Hola\nmundo").unwrap(), 1);
    assert_eq!(doc.layers[1].text().as_deref(), Some("Hola\rmundo"));
    assert_eq!(doc.set_text("missing", "x").unwrap(), 0);
    assert!(doc.layers[0].set_text("x").is_err());
}

#[test]
fn set_text_renders_new_text() {
    let (db, font) = font_or_skip!();
    let text = Text::new("I", font, 40.0, [0.0; 3], 20.0, 60.0);
    let mut doc = Document::parse(&white(400, 100).layer(Layer::text("t", &text)).build()).unwrap();
    let before = extent(&ink(&render(&doc, &db, &Default::default()).image));
    doc.layers[1].set_text("WIDE WORDS").unwrap();
    let out = render(&doc, &db, &Default::default());
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let after = extent(&ink(&out.image));
    assert!(after.2 > before.2 + 100, "{before:?} -> {after:?}");
}

#[test]
fn vertical_text_runs_down_a_column() {
    let (db, font) = font_or_skip!();
    let text = Text::new("HELLO", font, 30.0, [0.0; 3], 100.0, 20.0).vertical();
    let doc = Document::parse(&white(200, 220).layer(Layer::text("t", &text)).build()).unwrap();
    let (x0, y0, x1, y1) = extent(&ink(&render(&doc, &db, &Default::default()).image));
    assert!(y1 - y0 > 3 * (x1 - x0), "{x0},{y0}..{x1},{y1}");
    assert!(x0 >= 80 && x1 <= 120, "column centered on the anchor: {x0}..{x1}");
    assert!((18..=30).contains(&y0), "starts at the anchor: {y0}");
}

#[test]
fn synthetic_superscript_is_smaller_and_raised() {
    let (db, font) = font_or_skip!();
    let plain = Text::new("H", font, 40.0, [0.0; 3], 20.0, 60.0);
    let sup = Text::new("H", font, 40.0, [0.0; 3], 20.0, 60.0).style("/FontBaseline 1");
    let ext = |t: &Text| {
        let doc = Document::parse(&white(100, 100).layer(Layer::text("t", t)).build()).unwrap();
        extent(&ink(&render(&doc, &db, &Default::default()).image))
    };
    let (a, b) = (ext(&plain), ext(&sup));
    assert!(b.3 < a.3 - 8, "raised: {a:?} {b:?}");
    assert!(b.3 - b.1 < (a.3 - a.1) * 3 / 4, "smaller: {a:?} {b:?}");
}

#[test]
#[ignore = "writes /tmp/vertical-ja.png for a visual check"]
fn vertical_japanese_preview() {
    let mut db = psd_compiler::FontDb::new();
    db.add_system_fonts();
    let text = Text::new("縦書き「テスト」ABC。\nふたつめ", "WenQuanYiZenHei", 28.0, [0.0; 3], 150.0, 20.0).vertical();
    let doc = Document::parse(&white(200, 360).layer(Layer::text("t", &text)).build()).unwrap();
    let out = render(&doc, &db, &Default::default());
    out.image.save_png("/tmp/vertical-ja.png", 2).unwrap();
}
