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
