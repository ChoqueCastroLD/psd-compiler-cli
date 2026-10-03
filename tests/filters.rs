mod common;

use common::{Layer, Psd, V};
use psd_compiler::{render, Document, EncodeOptions, FontDb, Format, Image, RenderOptions, Rendered};

const ID: &str = "11111111-2222-3333-4444-555555555555";
const QUAD: [(f64, f64); 4] = [(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)];

fn code(c: &[u8; 4]) -> V {
    V::Long(i32::from_be_bytes(*c))
}

fn filter(name: &str, id: &[u8; 4], opacity: f64, params: Option<V>) -> V {
    let mut items = vec![
        ("Nm  ", V::Text(name.into())),
        (
            "blendOptions",
            V::Obj("blendOptions", vec![("Opct", V::Unit("#Prc", opacity)), ("Md  ", V::Enum("BlnM", "Nrml"))]),
        ),
        ("enab", V::Bool(true)),
        ("filterID", code(id)),
    ];
    items.extend(params.map(|p| ("Fltr", p)));
    V::Obj("filterFX", items)
}

/// A white page with a 20x20 blue smart object (cached red) carrying `filters`.
fn page(enabled: bool, filters: Vec<V>, options: &RenderOptions) -> Rendered {
    let fx = V::Obj("filterFXStyle", vec![("enab", V::Bool(enabled)), ("filterFXList", V::List(filters))]);
    let mut psd = Psd::new(40, 40)
        .layer(Layer::solid("bg", 0, 0, 40, 40, [255, 255, 255, 255]))
        .layer(Layer::smart_with("so", ID, (4.0, 4.0), QUAD, [255, 0, 0, 255], vec![("filterFX", fx)]));
    let img = Image { width: 4, height: 4, data: [0, 0, 255, 255].repeat(16) };
    psd.linked.push((ID.into(), img.encode(Format::Png, &EncodeOptions::default()).unwrap()));
    render(&Document::parse(&psd.build()).unwrap(), &FontDb::new(), options)
}

fn rerender() -> RenderOptions {
    RenderOptions { render_smart_objects: true, ..Default::default() }
}

fn gaussian(radius: f64) -> V {
    filter("Gaussian Blur", b"GsnB", 100.0, Some(V::Obj("GsnB", vec![("Rds ", V::Unit("#Pxl", radius))])))
}

#[test]
fn gaussian_blur_spreads_past_the_edges() {
    let out = page(true, vec![gaussian(2.0)], &rerender());
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.image.pixel(20, 20), [0, 0, 255, 255]);
    let edge = out.image.pixel(9, 20);
    assert!(edge[0] > 100 && edge[0] < 220 && edge[2] == 255, "{edge:?}");
    assert_eq!(out.image.pixel(2, 20), [255, 255, 255, 255]);
}

#[test]
fn filters_run_in_order_and_fade_by_opacity() {
    let invert = filter("Invert", b"Invr", 50.0, None);
    let out = page(true, vec![invert], &rerender());
    let p = out.image.pixel(20, 20);
    assert!(p[..3].iter().all(|&v| v.abs_diff(128) <= 1), "{p:?}");
    // Blurring after the full invert blurs yellow, not blue.
    let out = page(true, vec![filter("Invert", b"Invr", 100.0, None), gaussian(2.0)], &rerender());
    let edge = out.image.pixel(9, 20);
    assert!(edge[0] == 255 && edge[2] > 100 && edge[2] < 250, "{edge:?}");
}

#[test]
fn offset_wraps_around_the_canvas() {
    let params = V::Obj("Ofst", vec![("Hrzn", V::Long(25)), ("Vrtc", V::Long(0)), ("Fl  ", V::Enum("FlMd", "Wrp "))]);
    let out = page(true, vec![filter("Offset", b"Ofst", 100.0, Some(params))], &rerender());
    assert_eq!(out.image.pixel(36, 20), [0, 0, 255, 255]);
    assert_eq!(out.image.pixel(2, 20), [0, 0, 255, 255]);
    assert_eq!(out.image.pixel(20, 20), [255, 255, 255, 255]);
}

#[test]
fn unsupported_filters_fall_back_to_the_cache() {
    let noise = filter("Add Noise", b"AdNs", 100.0, None);
    let out = page(true, vec![noise], &rerender());
    assert_eq!(out.image.pixel(20, 20), [255, 0, 0, 255]);
    assert!(out.warnings.iter().any(|w| w.message.contains("Add Noise")), "{:?}", out.warnings);
}

#[test]
fn disabled_filters_are_skipped() {
    let out = page(false, vec![filter("Invert", b"Invr", 100.0, None)], &rerender());
    assert_eq!(out.image.pixel(20, 20), [0, 0, 255, 255]);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}
