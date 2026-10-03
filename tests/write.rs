mod common;

use common::{test_font, Layer, Psd, Text};
use psd_compiler::{render, Document, FontDb, Image, RenderOptions};
use std::process::Command;

const ID: &str = "11111111-2222-3333-4444-555555555555";

fn page(font: &str, psb: bool) -> Psd {
    let mut psd = Psd::new(120, 60)
        .layer(Layer::solid("bg", 0, 0, 120, 60, [255, 255, 255, 255]))
        .layer(Layer::text("Title", &Text::new("I", font, 40.0, [0.0; 3], 10.0, 45.0)))
        .layer(Layer::solid("dot", 100, 5, 10, 10, [200, 0, 0, 255]));
    psd.psb = psb;
    psd
}

fn cached() -> RenderOptions {
    RenderOptions { keep_text_raster: true, ..Default::default() }
}

fn diff(a: &Image, b: &Image) -> u8 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    a.data.iter().zip(&b.data).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// Decodes the RLE composite image of an 8-bit PSD (not PSB) as interleaved channels.
fn composite(file: &[u8]) -> Vec<u8> {
    let be32 = |p: usize| u32::from_be_bytes(file[p..p + 4].try_into().unwrap()) as usize;
    let channels = u16::from_be_bytes([file[12], file[13]]) as usize;
    let (h, w) = (be32(14), be32(18));
    let mut p = 26;
    for _ in 0..3 {
        p += 4 + be32(p);
    }
    assert_eq!(&file[p..p + 2], &[0, 1], "composite is not RLE");
    let counts: Vec<usize> =
        (0..channels * h).map(|i| u16::from_be_bytes([file[p + 2 + 2 * i], file[p + 3 + 2 * i]]) as usize).collect();
    p += 2 + 2 * channels * h;
    let mut planes = vec![];
    for n in counts {
        let (mut row, mut i) = (vec![], p);
        while i < p + n {
            let c = file[i] as i8;
            if c >= 0 {
                row.extend_from_slice(&file[i + 1..i + 2 + c as usize]);
                i += 2 + c as usize;
            } else if c != -128 {
                row.extend(std::iter::repeat_n(file[i + 1], 1 - c as isize as usize));
                i += 2;
            } else {
                i += 1;
            }
        }
        assert_eq!(row.len(), w);
        planes.extend(row);
        p += n;
    }
    (0..w * h).flat_map(|i| (0..channels).map(move |c| (c, i))).map(|(c, i)| planes[c * w * h + i]).collect()
}

#[test]
fn unedited_document_is_copied_byte_for_byte() {
    let original = Psd::new(8, 8).layer(Layer::solid("a", 0, 0, 8, 8, [1, 2, 3, 255])).build();
    let doc = Document::parse(&original).unwrap();
    let (out, warnings) = doc.to_psd(&original, &FontDb::new(), &Default::default()).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(out, original);
}

#[test]
fn edited_text_is_written_with_new_pixels() {
    let Some((db, font)) = test_font() else { return };
    for psb in [false, true] {
        let original = page(font, psb).build();
        let mut doc = Document::parse(&original).unwrap();
        assert_eq!(doc.set_text("Title", "WWWW").unwrap(), 1);
        let expected = render(&doc, &db, &Default::default()).image;
        let (out, warnings) = doc.to_psd(&original, &db, &Default::default()).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");

        let written = Document::parse(&out).unwrap();
        assert_eq!(written.layers.len(), 3);
        assert_eq!(written.layers[1].text().as_deref(), Some("WWWW"));
        let b = written.layers[1].bounds;
        assert!(b.right - b.left > 60, "text layer is only {} px wide", b.right - b.left);
        assert_eq!(written.layers[2].bounds, doc.layers[2].bounds);
        // The stored pixels match a fresh render of the new text.
        assert!(diff(&render(&written, &FontDb::new(), &cached()).image, &expected) <= 1, "psb: {psb}");
        if !psb {
            let rgb = composite(&out);
            let px: Vec<u8> = expected.data.chunks(4).flat_map(|p| p[..3].to_vec()).collect();
            assert!(rgb.iter().zip(&px).all(|(a, b)| a.abs_diff(*b) <= 1));
        }
    }
}

#[test]
fn text_inside_a_smart_object_is_written_to_the_embedded_file() {
    let Some((db, font)) = test_font() else { return };
    let inner = Psd::new(100, 50)
        .layer(Layer::solid("bg", 0, 0, 100, 50, [255, 255, 255, 255]))
        .layer(Layer::text("Title", &Text::new("I", font, 40.0, [0.0; 3], 10.0, 40.0)));
    let mut psd = Psd::new(100, 50).layer(Layer::smart(
        "Card",
        ID,
        (100.0, 50.0),
        [(0.0, 0.0), (100.0, 0.0), (100.0, 50.0), (0.0, 50.0)],
        [255, 255, 255, 255],
    ));
    psd.linked.push((ID.into(), inner.build()));
    let original = psd.build();
    let mut doc = Document::parse(&original).unwrap();
    assert_eq!(doc.set_text("Card/Title", "WWWW").unwrap(), 1);
    let (out, warnings) = doc.to_psd(&original, &db, &Default::default()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");

    let written = Document::parse(&out).unwrap();
    let ink = |img: &Image| (0..100).filter(|&x| img.pixel(x, 30)[0] < 128).count();
    // Both the layer's cached pixels and the embedded file carry the new text.
    assert!(ink(&render(&written, &FontDb::new(), &cached()).image) > 40);
    let rerender = RenderOptions { render_smart_objects: true, keep_text_raster: true, ..Default::default() };
    assert!(ink(&render(&written, &FontDb::new(), &rerender).image) > 40);
}

#[test]
fn cli_writes_psd_and_keeps_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("page.psd");
    let original = Psd::new(8, 8).layer(Layer::solid("a", 0, 0, 8, 8, [1, 2, 3, 255])).build();
    std::fs::write(&input, &original).unwrap();
    let psdc = || Command::new(env!("CARGO_BIN_EXE_psdc"));

    let out = dir.path().join("copy.psd");
    assert!(psdc().arg(&input).arg("-o").arg(&out).args(["-q", "--no-system-fonts"]).status().unwrap().success());
    assert_eq!(std::fs::read(&out).unwrap(), original);

    let same = psdc().arg(&input).arg("-o").arg(&input).args(["-q", "--no-system-fonts"]).output().unwrap();
    assert!(!same.status.success());
    assert!(String::from_utf8_lossy(&same.stderr).contains("refusing to overwrite"));
    assert_eq!(std::fs::read(&input).unwrap(), original);
}

#[test]
fn edited_text_is_written_in_every_color_mode() {
    let Some((db, font)) = test_font() else { return };
    // Planar: 256 reds, greens, then blues.
    let gray_palette: Vec<u8> = (0..3).flat_map(|_| 0..=255u8).collect();
    // Header mode, depth, color mode data, white background channels and the tolerance.
    type Case = (&'static str, u16, u16, Vec<u8>, [u8; 4], u8);
    let cases: [Case; 5] = [
        ("CMYK", 4, 8, vec![], [255; 4], 2),
        ("Lab", 9, 16, vec![], [255, 128, 128, 255], 3),
        // Composited in linear light, where the 8-bit edge alpha rounds to 2 levels after encoding.
        ("gray 32-bit", 1, 32, vec![], [255; 4], 2),
        ("duotone", 8, 8, vec![], [255; 4], 1),
        ("indexed", 2, 8, gray_palette, [255; 4], 1),
    ];
    for (name, mode, depth, color_data, white, tolerance) in cases {
        let mut psd = Psd::new(120, 60)
            .layer(Layer::solid("bg", 0, 0, 120, 60, white).channel(3, vec![255; 120 * 60]))
            .layer(Layer::text("Title", &Text::new("I", font, 40.0, [0.0; 3], 10.0, 45.0)).channel(3, vec![]));
        (psd.mode, psd.depth, psd.color_data) = (mode, depth, color_data);
        let original = psd.build();
        let mut doc = Document::parse(&original).unwrap();
        doc.set_text("Title", "WWWW").unwrap();
        let expected = render(&doc, &db, &Default::default()).image;
        let (out, _) = doc.to_psd(&original, &db, &Default::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
        let written = Document::parse(&out).unwrap();
        let got = render(&written, &FontDb::new(), &cached()).image;
        assert!(diff(&got, &expected) <= tolerance, "{name}: off by {}", diff(&got, &expected));
        let ink = (0..120).filter(|&x| got.pixel(x, 30)[0] < 128).count();
        assert!(ink > 40, "{name}: only {ink} dark pixels");
    }
}

#[test]
fn bitmap_documents_cannot_be_written() {
    let Some((db, font)) = test_font() else { return };
    let mut psd = Psd::new(8, 8).layer(Layer::text("Title", &Text::new("I", font, 8.0, [0.0; 3], 1.0, 7.0)));
    psd.mode = 7;
    let original = psd.build();
    let mut doc = Document::parse(&original).unwrap();
    doc.set_text("Title", "W").unwrap();
    let err = doc.to_psd(&original, &db, &Default::default()).unwrap_err();
    assert!(err.to_string().contains("not supported"), "{err}");
}
