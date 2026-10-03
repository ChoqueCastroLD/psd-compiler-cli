mod common;

use common::{test_font, Layer, Psd, Text};
use psd_compiler::{render, Document, EncodeOptions, Format, Image, RenderOptions};

const ID: &str = "11111111-2222-3333-4444-555555555555";

fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let img = Image { width: w, height: h, data: rgba.repeat((w * h) as usize) };
    img.encode(Format::Png, &EncodeOptions::default()).unwrap()
}

fn white(w: u32, h: u32) -> Psd {
    Psd::new(w, h).layer(Layer::solid("bg", 0, 0, w as usize, h as usize, [255, 255, 255, 255]))
}

fn smart_on(quad: [(f64, f64); 4]) -> Document {
    let mut psd = white(40, 40).layer(Layer::smart("so", ID, (4.0, 4.0), quad, [255, 0, 0, 255]));
    psd.linked.push((ID.into(), png(4, 4, [0, 0, 255, 255])));
    Document::parse(&psd.build()).unwrap()
}

fn rerender() -> RenderOptions {
    RenderOptions { render_smart_objects: true, ..Default::default() }
}

#[test]
fn smart_objects_use_the_cache_by_default() {
    let doc = smart_on([(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)]);
    let out = render(&doc, &psd_compiler::FontDb::new(), &Default::default());
    assert_eq!(out.image.pixel(20, 20), [255, 0, 0, 255]);
}

#[test]
fn embedded_png_is_scaled_onto_its_quad() {
    let doc = smart_on([(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)]);
    let out = render(&doc, &psd_compiler::FontDb::new(), &rerender());
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    for (x, y) in [(12, 12), (20, 20), (28, 27)] {
        assert_eq!(out.image.pixel(x, y), [0, 0, 255, 255], "({x}, {y})");
    }
    for (x, y) in [(8, 20), (32, 20), (20, 8), (20, 32)] {
        assert_eq!(out.image.pixel(x, y), [255, 255, 255, 255], "({x}, {y})");
    }
}

#[test]
fn perspective_quad_follows_its_corners() {
    // A trapezoid: wide at the bottom, narrow at the top.
    let doc = smart_on([(16.0, 5.0), (24.0, 5.0), (35.0, 35.0), (5.0, 35.0)]);
    let out = render(&doc, &psd_compiler::FontDb::new(), &rerender());
    assert_eq!(out.image.pixel(20, 20), [0, 0, 255, 255]);
    assert_eq!(out.image.pixel(7, 33), [0, 0, 255, 255]);
    assert_eq!(out.image.pixel(8, 8), [255, 255, 255, 255]);
    assert_eq!(out.image.pixel(32, 8), [255, 255, 255, 255]);
}

#[test]
fn text_inside_a_smart_object_can_be_replaced() {
    let Some((db, font)) = test_font() else { return };
    let inner = Psd::new(100, 50)
        .layer(Layer::solid("bg", 0, 0, 100, 50, [255, 255, 255, 255]))
        .layer(Layer::text("Title", &Text::new("I", font, 40.0, [0.0; 3], 10.0, 40.0)));
    let mut psd = white(100, 50).layer(Layer::smart(
        "Card",
        ID,
        (100.0, 50.0),
        [(0.0, 0.0), (100.0, 0.0), (100.0, 50.0), (0.0, 50.0)],
        [255, 255, 255, 255],
    ));
    psd.linked.push((ID.into(), inner.build()));
    let mut doc = Document::parse(&psd.build()).unwrap();
    assert_eq!(doc.set_text("Card/Nope", "x").unwrap(), 0);
    assert_eq!(doc.set_text("Card/Title", "WWWW").unwrap(), 1);
    let out = render(&doc, &db, &Default::default());
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let ink = (0..100).filter(|&x| out.image.pixel(x, 30)[0] < 128).count();
    assert!(ink > 40, "only {ink} dark pixels across the new text");
}

#[test]
fn linked_files_are_found_next_to_the_document() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("assets");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("logo.png"), png(4, 4, [0, 0, 255, 255])).unwrap();
    let mut psd = white(40, 40).layer(Layer::smart(
        "so",
        ID,
        (4.0, 4.0),
        [(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)],
        [255, 0, 0, 255],
    ));
    psd.external.push((ID.into(), "assets/logo.png".into(), "file:///Macintosh%20HD/nowhere/logo.png".into()));
    let path = dir.path().join("page.psd");
    std::fs::write(&path, psd.build()).unwrap();

    let doc = Document::open(&path).unwrap();
    let out = render(&doc, &psd_compiler::FontDb::new(), &rerender());
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.image.pixel(20, 20), [0, 0, 255, 255]);

    // Without the folder the link can't be followed, so the cached pixels stay.
    let doc = Document::parse(&psd.build()).unwrap();
    let out = render(&doc, &psd_compiler::FontDb::new(), &rerender());
    assert_eq!(out.image.pixel(20, 20), [255, 0, 0, 255]);
    assert!(
        out.warnings.iter().any(|w| w.message.contains("linked file \"assets/logo.png\" not found")),
        "{:?}",
        out.warnings
    );
}
