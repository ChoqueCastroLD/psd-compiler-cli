//! # psd-compiler
//!
//! Compile Photoshop documents (PSD and PSB) to flat images without Photoshop.
//!
//! Type layers are not copied from the raster Photoshop cached in the file: they are re-rendered
//! from their text engine data with real fonts, so edited text (for example a translation written
//! with a PSD library) comes out exactly as Photoshop would draw it.
//!
//! ```no_run
//! use psd_compiler::{render, Document, FontDb, RenderOptions};
//!
//! let doc = Document::open("page.psd")?;
//! let mut fonts = FontDb::new();
//! fonts.add_dir("fonts");
//! fonts.add_system_fonts();
//!
//! let out = render(&doc, &fonts, &RenderOptions::default());
//! for w in &out.warnings {
//!     eprintln!("warning: {w}");
//! }
//! out.image.save_png("page.png", psd_compiler::DEFAULT_COMPRESSION)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![warn(missing_docs)]

mod blend;
mod color;
mod error;
mod fonts;
mod image;
pub mod png;
mod psd;
mod render;
mod text;

pub use blend::BlendMode;
pub use error::{Error, Result};
pub use fonts::FontDb;
pub use image::{Image, DEFAULT_COMPRESSION};
pub use psd::{ColorMode, Document, Layer, LayerKind, Rect};
pub use render::{render, RenderOptions, Rendered, TextMask, Warning};
