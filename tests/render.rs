mod common;

use common::{Compression, Effect, Layer, Psd};
use psd_compiler::{render, Document, FontDb, Image, RenderOptions};

fn draw(psd: Psd) -> Image {
    let doc = Document::parse(&psd.build()).unwrap();
    render(&doc, &FontDb::new(), &RenderOptions::default()).image
}

fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tol)
}

fn white(w: u32, h: u32) -> Psd {
    Psd::new(w, h).layer(Layer::solid("bg", 0, 0, w as usize, h as usize, [255, 255, 255, 255]))
}

#[test]
fn composites_layers_in_order() {
    let img = draw(white(20, 20).layer(Layer::solid("red", 5, 5, 10, 10, [255, 0, 0, 255])));
    assert_eq!(img.pixel(0, 0), [255, 255, 255, 255]);
    assert_eq!(img.pixel(10, 10), [255, 0, 0, 255]);
    assert!(img.is_opaque());
}

#[test]
fn compressions_are_equivalent() {
    let pattern = |x: usize, y: usize| [(x * 13) as u8, (y * 7) as u8, ((x ^ y) * 5) as u8, 255];
    let images: Vec<Image> = [Compression::Raw, Compression::Rle, Compression::Zip, Compression::ZipPredicted]
        .into_iter()
        .map(|c| draw(Psd::new(33, 17).layer(Layer::pixels("p", 0, 0, 33, 17, pattern).compression(c))))
        .collect();
    for img in &images[1..] {
        assert_eq!(img.data, images[0].data);
    }
    assert_eq!(images[0].pixel(5, 3), pattern(5, 3));
}

#[test]
fn hidden_layers_are_skipped() {
    let img = draw(white(10, 10).layer(Layer::solid("x", 0, 0, 10, 10, [0, 0, 0, 255]).hidden()));
    assert_eq!(img.pixel(5, 5), [255, 255, 255, 255]);
}

#[test]
fn opacity_and_fill() {
    let half = draw(white(4, 4).layer(Layer::solid("x", 0, 0, 4, 4, [0, 0, 0, 255]).opacity(128)));
    assert!(close(half.pixel(1, 1), [127, 127, 127, 255], 1));
    let fill = draw(white(4, 4).layer(Layer::solid("x", 0, 0, 4, 4, [0, 0, 0, 255]).fill(0)));
    assert_eq!(fill.pixel(1, 1), [255, 255, 255, 255]);
}

#[test]
fn multiply_blend() {
    let img = draw(
        Psd::new(4, 4)
            .layer(Layer::solid("bg", 0, 0, 4, 4, [200, 100, 50, 255]))
            .layer(Layer::solid("m", 0, 0, 4, 4, [128, 128, 128, 255]).blend(b"mul ")),
    );
    assert!(close(img.pixel(0, 0), [100, 50, 25, 255], 1));
}

#[test]
fn clipping_masks_to_base() {
    let img = draw(
        white(20, 10)
            .layer(Layer::solid("base", 0, 0, 10, 10, [0, 0, 255, 255]))
            .layer(Layer::solid("clip", 0, 0, 20, 10, [0, 255, 0, 255]).clipped()),
    );
    assert_eq!(img.pixel(5, 5), [0, 255, 0, 255]);
    assert_eq!(img.pixel(15, 5), [255, 255, 255, 255]);
}

#[test]
fn layer_mask_hides_pixels() {
    let layer = Layer::solid("m", 0, 0, 10, 10, [0, 0, 0, 255]).mask([0, 0, 10, 5], 0, |_, _| 255);
    let img = draw(white(10, 10).layer(layer));
    assert_eq!(img.pixel(2, 5), [0, 0, 0, 255]);
    assert_eq!(img.pixel(7, 5), [255, 255, 255, 255]);
}

#[test]
fn group_opacity_applies_to_children() {
    let img = draw(
        white(6, 6)
            .layer(Layer::group_end())
            .layer(Layer::solid("a", 0, 0, 6, 6, [0, 0, 0, 255]))
            .layer(Layer::group("g").opacity(128)),
    );
    assert!(close(img.pixel(3, 3), [127, 127, 127, 255], 1));
}

#[test]
fn transparent_canvas_keeps_alpha() {
    let img = draw(Psd::new(8, 8).layer(Layer::solid("a", 0, 0, 4, 8, [10, 20, 30, 255])));
    assert_eq!(img.pixel(1, 1), [10, 20, 30, 255]);
    assert_eq!(img.pixel(6, 1)[3], 0);
    assert!(!img.is_opaque());
}

#[test]
fn stroke_surrounds_shape() {
    let layer = Layer::solid("s", 10, 10, 10, 10, [255, 0, 0, 255]).effects(&[Effect::Stroke {
        size: 3.0,
        rgb: [0, 0, 255],
        position: "OutF",
    }]);
    let img = draw(white(30, 30).layer(layer));
    assert_eq!(img.pixel(15, 15), [255, 0, 0, 255]);
    assert_eq!(img.pixel(8, 15), [0, 0, 255, 255]);
    assert_eq!(img.pixel(3, 15), [255, 255, 255, 255]);
}

#[test]
fn drop_shadow_is_offset_and_soft() {
    let layer = Layer::solid("s", 10, 10, 10, 10, [255, 255, 255, 255]).effects(&[Effect::Shadow {
        size: 4.0,
        distance: 6.0,
        angle: 90.0,
        spread: 0.0,
        rgb: [0, 0, 0],
        opacity: 100.0,
    }]);
    let img = draw(white(40, 40).layer(layer));
    assert_eq!(img.pixel(15, 15), [255, 255, 255, 255]);
    assert!(img.pixel(15, 23)[0] < 60, "{:?}", img.pixel(15, 23));
    assert_eq!(img.pixel(15, 5), [255, 255, 255, 255]);
    let edge = img.pixel(15, 27)[0];
    assert!(edge > 60 && edge < 255, "{edge}");
}

#[test]
fn color_overlay_recolors() {
    let layer = Layer::solid("s", 0, 0, 5, 5, [255, 0, 0, 255]).effects(&[Effect::Overlay { rgb: [0, 255, 0] }]);
    assert_eq!(draw(white(5, 5).layer(layer)).pixel(2, 2), [0, 255, 0, 255]);
}

#[test]
fn empty_document_falls_back_to_composite() {
    let mut psd = Psd::new(3, 3);
    psd.composite = Some([12, 34, 56]);
    assert_eq!(draw(psd).pixel(1, 1), [12, 34, 56, 255]);
}

#[test]
fn png_roundtrip() {
    let img =
        draw(Psd::new(16, 9).layer(Layer::pixels("p", 0, 0, 16, 9, |x, y| [x as u8 * 16, y as u8 * 28, 99, 200])));
    let png = img.encode_png(6);
    let decoder = png::Decoder::new(&png[..]);
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!((info.width, info.height), (16, 9));
    assert_eq!(&buf[..info.buffer_size()], &img.data[..]);
}

#[test]
fn restricted_channels_keep_the_backdrop() {
    // Advanced blending with only R and G checked: the blue channel (index 2) is left alone.
    let gray = Psd::new(4, 4).layer(Layer::solid("bg", 0, 0, 4, 4, [128, 128, 128, 255]));
    let img =
        draw(gray.layer(Layer::solid("red", 0, 0, 4, 4, [255, 0, 0, 255]).block(b"brst", 2u32.to_be_bytes().to_vec())));
    assert_eq!(img.pixel(1, 1), [255, 0, 128, 255]);
}

#[test]
fn interior_effects_blend_over_the_blended_layer() {
    // Photoshop paints a color overlay onto the result of the layer over its backdrop: in Normal
    // mode it hides the layer's Darken entirely.
    let gray = Psd::new(4, 4).layer(Layer::solid("bg", 0, 0, 4, 4, [128, 128, 128, 255]));
    let layer = Layer::solid("l", 0, 0, 4, 4, [242, 85, 85, 255]).blend(b"dark");
    let img = draw(gray.layer(layer.effects(&[Effect::Overlay { rgb: [65, 217, 217] }])));
    assert!(close(img.pixel(1, 1), [65, 217, 217, 255], 1), "{:?}", img.pixel(1, 1));
}

#[test]
fn faded_modes_show_plainly_over_transparency() {
    // Matches Photoshop: a Linear Light layer at 20% fill blends its faded color where the
    // backdrop is covered and shows at 20% where it is transparent.
    let img = draw(
        Psd::new(2, 2)
            .layer(Layer::solid("blue", 0, 0, 2, 2, [0, 0, 255, 255]).opacity(153))
            .layer(Layer::solid("green", 0, 0, 2, 2, [0, 255, 0, 255]).opacity(102).blend(b"lLit"))
            .layer(Layer::solid("red", 0, 0, 2, 2, [255, 0, 0, 255]).fill(51).blend(b"lLit")),
    );
    let (got, want) = (img.pixel(1, 1), [63, 78, 66, 206]);
    assert!(got.iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= 2), "{got:?}");
}

#[test]
fn blend_if_hides_by_own_and_underlying_values() {
    const FULL: [u8; 4] = [0, 0, 255, 255];
    // Underlay: left half black, right half white. A gray ramp on top.
    let under = Layer::pixels("under", 0, 0, 40, 10, |x, _| if x < 20 { [0, 0, 0, 255] } else { [255; 4] });
    let ramp = |x: usize, _: usize| {
        let v = (x * 255 / 39) as u8;
        [v, v, v, 255]
    };
    // This layer's darks (gray below 128) are hidden.
    let img = draw(white(40, 10).layer(under.clone()).layer(Layer::pixels("ramp", 0, 0, 40, 10, ramp).blend_if(&[
        ([128, 128, 255, 255], FULL),
        (FULL, FULL),
        (FULL, FULL),
        (FULL, FULL),
    ])));
    assert_eq!(img.pixel(5, 5), [0, 0, 0, 255]);
    assert_eq!(img.pixel(30, 5), ramp(30, 0));
    // Shown only over underlying lights; a split slider fades in between.
    let img = draw(
        white(40, 10)
            .layer(under)
            .layer(Layer::pixels("ramp", 0, 0, 40, 10, ramp).blend_if(&[(FULL, [100, 200, 255, 255])])),
    );
    assert_eq!(img.pixel(30, 5), ramp(30, 0));
    assert_eq!(img.pixel(10, 5), [0, 0, 0, 255]);
}

#[test]
fn pass_through_fill_mixes_blended_and_apart() {
    // Fit to psd-tools' passthrough_fill_blendmode.psd: in a pass-through group at fill f, a
    // Lighter Color layer shows by f where it is lighter and by f² where the backdrop is.
    let group = |child: [u8; 4]| {
        draw(
            Psd::new(2, 2)
                .layer(Layer::solid("gray", 0, 0, 2, 2, [128, 128, 128, 255]))
                .layer(Layer::group_end())
                .layer(Layer::solid("child", 0, 0, 2, 2, child).blend(b"lgCl"))
                .layer(Layer::group("g").blend(b"pass").fill(120)),
        )
        .pixel(1, 1)
    };
    assert!(close(group([200, 200, 200, 255]), [162, 162, 162, 255], 2), "{:?}", group([200, 200, 200, 255]));
    assert!(close(group([50, 50, 50, 255]), [111, 111, 111, 255], 2), "{:?}", group([50, 50, 50, 255]));
}
