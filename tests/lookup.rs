mod common;

use common::{descriptor, Layer, Psd, V};
use psd_compiler::{render, Document, FontDb};

fn lookup(items: &[(&str, V)]) -> Vec<u8> {
    let mut b = vec![0, 1, 0, 0, 0, 16];
    b.extend(descriptor("null", items));
    b
}

fn page_with(lut: Vec<u8>) -> psd_compiler::Image {
    let psd = Psd::new(4, 1)
        .layer(Layer::pixels("bg", 0, 0, 4, 1, |x, _| [(x * 80) as u8, 100, 200, 255]))
        .layer(Layer::solid("Color Lookup 1", 0, 0, 0, 0, [0; 4]).block(b"clrL", lut));
    render(&Document::parse(&psd.build()).unwrap(), &FontDb::new(), &Default::default()).image
}

#[test]
fn cube_lookup_inverts() {
    let mut cube = String::from("LUT_3D_SIZE 2\n");
    for i in 0..8 {
        let (r, g, b) = (i & 1, i >> 1 & 1, i >> 2 & 1);
        cube += &format!("{} {} {}\n", 1 - r, 1 - g, 1 - b);
    }
    let out = page_with(lookup(&[
        ("lookupType", V::Enum("colorLookupType", "3DLUT")),
        ("LUTFormat", V::Enum("LUTFormatType", "LUTFormatCUBE")),
        ("LUT3DFileData", V::Raw(cube.into_bytes())),
    ]));
    for x in 0..4 {
        let p = out.pixel(x, 0);
        let want = [255 - x as u8 * 80, 155, 55];
        assert!(p[..3].iter().zip(&want).all(|(a, b)| a.abs_diff(*b) <= 1), "{x}: {p:?}");
    }
}

#[test]
fn empty_lookup_changes_nothing() {
    let out = page_with(lookup(&[]));
    assert_eq!(out.pixel(1, 0), [80, 100, 200, 255]);
}
