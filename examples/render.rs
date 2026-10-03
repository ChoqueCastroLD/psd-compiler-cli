//! Renders a PSD with the library API.
//!
//! ```sh
//! cargo run --release --example render -- input.psd output.png
//! ```

use psd_compiler::{render, Document, FontDb, RenderOptions, DEFAULT_COMPRESSION};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: render <input.psd> <output.png>");
        std::process::exit(2);
    };

    let mut fonts = FontDb::new();
    fonts.add_dir("fonts");
    fonts.add_system_fonts();

    let doc = Document::open(&input)?;
    let out = render(&doc, &fonts, &RenderOptions::default());
    for w in &out.warnings {
        eprintln!("warning: {w}");
    }
    out.image.save_png(&output, DEFAULT_COMPRESSION)?;
    println!("{}x{} → {output}", doc.width, doc.height);
    Ok(())
}
