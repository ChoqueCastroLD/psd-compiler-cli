//! Generates `docs/assets/demo.psd`, the document behind the README hero image.
//!
//! ```sh
//! cargo run --example demo
//! psdc docs/assets/demo.psd -o docs/assets/demo.png
//! ```
//!
//! The text layers reference Montserrat by name; install it or point `--fonts` at a folder containing it.

#[path = "../tests/common/mod.rs"]
mod common;

use common::{Compression, Effect, Layer, Psd, Text};

const W: usize = 1280;
const H: usize = 640;
const TAIL: [(f32, f32); 3] = [(420.0, 240.0), (540.0, 215.0), (600.0, 360.0)];

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn background(x: usize, y: usize) -> [u8; 4] {
    let t = (x as f32 / W as f32 * 0.7 + y as f32 / H as f32 * 0.3).clamp(0.0, 1.0);
    let base = if t < 0.5 {
        mix([30.0, 27.0, 75.0], [124.0, 58.0, 237.0], t * 2.0)
    } else {
        mix([124.0, 58.0, 237.0], [219.0, 39.0, 119.0], t * 2.0 - 1.0)
    };
    let cell = 16.0;
    let (cx, cy) = ((x as f32 % cell) - cell / 2.0, (y as f32 % cell) - cell / 2.0);
    let radius = 1.0 + 4.5 * (x as f32 / W as f32);
    let dot = (radius + 0.5 - (cx * cx + cy * cy).sqrt()).clamp(0.0, 1.0) * 0.12;
    let c = mix(base, [255.0; 3], dot);
    [c[0] as u8, c[1] as u8, c[2] as u8, 255]
}

fn bubble(cx: f32, cy: f32, rx: f32, ry: f32, tail: [(f32, f32); 3]) -> impl Fn(usize, usize) -> [u8; 4] {
    move |x, y| {
        let mut a: f32 = 0.0;
        for (sx, sy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
            let (px, py) = (x as f32 + sx, y as f32 + sy);
            let (dx, dy) = ((px - cx) / rx, (py - cy) / ry);
            if dx * dx + dy * dy <= 1.0 || in_triangle((px, py), tail) {
                a += 0.25;
            }
        }
        [255, 255, 255, (a * 255.0) as u8]
    }
}

fn in_triangle(p: (f32, f32), [a, b, c]: [(f32, f32); 3]) -> bool {
    let side = |p1: (f32, f32), p2: (f32, f32)| (p.0 - p2.0) * (p1.1 - p2.1) - (p1.0 - p2.0) * (p.1 - p2.1);
    let (d1, d2, d3) = (side(a, b), side(b, c), side(c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

fn main() -> std::io::Result<()> {
    let black = [0, 0, 0];
    let shadow = Effect::Shadow { size: 14.0, distance: 10.0, angle: 120.0, spread: 0.0, rgb: black, opacity: 55.0 };

    let title = Text::new("PSD COMPILER", "Montserrat-Black", 104.0, [250.0, 204.0, 21.0], 640.0, 165.0)
        .center()
        .warp("warpArc", 12.0, None);
    let speech = Text::new(
        "Photoshop text,\nre-rendered\nwithout Photoshop.",
        "Montserrat-ExtraBold",
        38.0,
        [17.0, 17.0, 17.0],
        900.0,
        330.0,
    )
    .center();
    let tagline =
        Text::new("Rust  ·  Headless  ·  PSD → PNG", "Montserrat-SemiBold", 34.0, [255.0, 255.0, 255.0], 60.0, 490.0);
    let pitch = Text::new(
        "Kerning, warps, strokes and shadows\nmatched against Photoshop.",
        "Montserrat-Medium",
        24.0,
        [233.0, 213.0, 255.0],
        60.0,
        370.0,
    );

    let psd = Psd::new(W as u32, H as u32)
        .layer(Layer::pixels("Background", 0, 0, W, H, background).compression(Compression::Zip))
        .layer(
            Layer::pixels("Bubble", 560, 215, 680, 380, bubble(340.0, 150.0, 300.0, 120.0, TAIL))
                .compression(Compression::Rle)
                .effects(&[Effect::Stroke { size: 6.0, rgb: black, position: "OutF" }, shadow]),
        )
        .layer(Layer::text("Speech", &speech))
        .layer(Layer::text("Pitch", &pitch))
        .layer(Layer::text("Tagline", &tagline))
        .layer(
            Layer::text("Title", &title).effects(&[Effect::Stroke { size: 9.0, rgb: black, position: "OutF" }, shadow]),
        );

    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/assets/demo.psd");
    std::fs::write(&out, psd.build())?;
    println!("wrote {}", out.display());
    Ok(())
}
