//! Writes the merged image stored in a PSD: `cargo run --example stored IN.psd OUT.png`.
use psd_compiler::{Document, EncodeOptions, Format};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let doc = Document::parse(&std::fs::read(&args[1]).unwrap()).unwrap();
    let img = doc.stored_composite().expect("no stored composite");
    std::fs::write(&args[2], img.encode(Format::Png, &EncodeOptions::default()).unwrap()).unwrap();
}
