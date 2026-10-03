mod common;

use common::{Compression, Layer, Psd, Text};
use psd_compiler::{BlendMode, ColorMode, Document, LayerKind};

#[test]
fn reads_header_and_layers() {
    let data = Psd::new(40, 30)
        .layer(Layer::solid("bg", 0, 0, 40, 30, [255, 255, 255, 255]))
        .layer(Layer::solid("box", 5, 6, 10, 8, [255, 0, 0, 255]).opacity(128).blend(b"mul "))
        .build();
    let doc = Document::parse(&data).unwrap();
    assert_eq!((doc.width, doc.height, doc.depth), (40, 30, 8));
    assert_eq!(doc.color_mode, ColorMode::Rgb);
    assert_eq!(doc.layers.len(), 2);
    let l = &doc.layers[1];
    assert_eq!(l.name, "box");
    assert_eq!((l.bounds.left, l.bounds.top, l.bounds.width(), l.bounds.height()), (5, 6, 10, 8));
    assert_eq!(l.opacity, 128);
    assert_eq!(l.blend_mode, BlendMode::Multiply);
    assert_eq!(l.kind, LayerKind::Pixel);
}

#[test]
fn unicode_names_and_flags() {
    let data =
        Psd::new(8, 8).layer(Layer::solid("señal ✓", 0, 0, 8, 8, [0, 0, 0, 255]).hidden().clipped().fill(51)).build();
    let l = &Document::parse(&data).unwrap().layers[0];
    assert_eq!(l.name, "señal ✓");
    assert!(l.hidden);
    assert!(l.clipping);
    assert_eq!(l.fill_opacity, 51);
}

#[test]
fn groups_and_text_kinds() {
    let text = Text::new("Hi (there) \\o/", "DejaVuSans", 12.0, [0.0; 3], 2.0, 10.0);
    let data = Psd::new(20, 20)
        .layer(Layer::group_end())
        .layer(Layer::text("caption", &text))
        .layer(Layer::group("folder"))
        .build();
    let doc = Document::parse(&data).unwrap();
    let kinds: Vec<_> = doc.layers.iter().map(|l| l.kind).collect();
    assert_eq!(kinds, [LayerKind::GroupEnd, LayerKind::Text, LayerKind::Group]);
    assert_eq!(doc.layers[1].text().as_deref(), Some("Hi (there) \\o/"));
    assert_eq!(doc.layers[2].blend_mode, BlendMode::PassThrough);
}

#[test]
fn psb_and_16_bit() {
    for (psb, depth) in [(true, 8), (false, 16), (true, 16)] {
        for c in [Compression::Raw, Compression::Rle, Compression::Zip, Compression::ZipPredicted] {
            let mut psd = Psd::new(9, 7)
                .layer(Layer::pixels("g", 0, 0, 9, 7, |x, y| [(x * 20) as u8, (y * 30) as u8, 7, 255]).compression(c));
            psd.psb = psb;
            psd.depth = depth;
            let doc = Document::parse(&psd.build()).unwrap_or_else(|e| panic!("{psb} {depth} {c:?}: {e}"));
            assert_eq!(doc.depth, depth);
            let img = psd_compiler::render(&doc, &psd_compiler::FontDb::new(), &Default::default()).image;
            assert_eq!(img.pixel(4, 3), [80, 90, 7, 255], "{psb} {depth} {c:?}");
        }
    }
}

#[test]
fn rejects_garbage() {
    assert!(Document::parse(b"").is_err());
    assert!(Document::parse(b"GIF89a....").is_err());
    let data = Psd::new(16, 16).layer(Layer::solid("a", 0, 0, 16, 16, [1, 2, 3, 255])).build();
    for cut in [10, 26, 40, data.len() / 2] {
        assert!(Document::parse(&data[..cut]).is_err(), "truncated at {cut}");
    }
    let mut bad = data.clone();
    bad[4..6].copy_from_slice(&7u16.to_be_bytes());
    assert!(Document::parse(&bad).is_err());
}

#[test]
fn rejects_empty_canvas() {
    assert!(Document::parse(&Psd::new(0, 10).build()).is_err());
}

#[test]
fn open_reports_missing_file() {
    let err = Document::open("/definitely/not/here.psd").unwrap_err();
    assert!(matches!(err, psd_compiler::Error::Io(_)));
}
