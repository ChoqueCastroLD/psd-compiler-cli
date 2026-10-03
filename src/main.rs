use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use psd_compiler::{png, render, Document, EncodeOptions, FontDb, Format, RenderOptions};
use rayon::prelude::*;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const FONTS_ENV: &str = "PSDC_FONTS";

/// Compile PSD/PSB files to PNG, JPEG, WebP, TIFF or AVIF, re-rendering text layers with real fonts.
#[derive(Parser, Debug)]
#[command(name = "psdc", version, about, after_help = AFTER_HELP)]
struct Cli {
    /// PSD or PSB files to compile.
    #[arg(required = true, value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    /// Output file (one input) or directory (several inputs). Defaults to INPUT with the format's extension.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Output format: png, jpg, webp, tif or avif. Defaults to the output file's extension, else png.
    #[arg(short = 'F', long, value_name = "FORMAT", value_parser = parse_format)]
    format: Option<Format>,

    /// JPEG and AVIF quality, 1 to 100.
    #[arg(short = 'Q', long, default_value_t = psd_compiler::DEFAULT_QUALITY, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,

    /// Color that transparency is flattened onto for JPEG, as RRGGBB.
    #[arg(long, value_name = "RRGGBB", default_value = "ffffff", value_parser = parse_color)]
    background: [u8; 3],

    /// Replace the text of the type layers named LAYER before rendering; repeatable. `\n` in TEXT
    /// starts a new paragraph.
    #[arg(long = "set-text", value_name = "LAYER=TEXT", value_parser = parse_set_text)]
    set_text: Vec<(String, String)>,

    /// List the type layers of each input (index, name and text) instead of compiling.
    #[arg(long)]
    list_text: bool,

    /// Font folder to search first; repeatable.
    #[arg(short, long = "fonts", value_name = "DIR")]
    fonts: Vec<PathBuf>,

    /// Only use fonts from --fonts, PSDC_FONTS and ./fonts.
    #[arg(long)]
    no_system_fonts: bool,

    /// Keep Photoshop's cached text pixels instead of re-rendering type layers.
    #[arg(long)]
    keep_text: bool,

    /// Write the coverage of every type layer to DIR as STEM.textNNN.png.
    #[arg(long, value_name = "DIR")]
    text_masks: Option<PathBuf>,

    /// PNG and TIFF compression level, 0 (fastest) to 9 (smallest).
    #[arg(short, long, default_value_t = psd_compiler::DEFAULT_COMPRESSION, value_parser = clap::value_parser!(u8).range(0..=9))]
    compression: u8,

    /// Worker threads (default: all cores).
    #[arg(short, long, value_name = "N")]
    jobs: Option<usize>,

    /// Print timings for each file.
    #[arg(long)]
    timings: bool,

    /// Only print errors.
    #[arg(short, long)]
    quiet: bool,
}

const AFTER_HELP: &str = "\
Fonts are looked up by PostScript name, in this order:
  1. --fonts DIR (in the order given)
  2. PSDC_FONTS (folders separated by ':' or ';' on Windows)
  3. ./fonts, if it exists
  4. user and system font folders (skipped with --no-system-fonts)

Examples:
  psdc page.psd                      write page.png next to page.psd
  psdc page.psd -o out.png -f fonts  use ./fonts for missing typefaces
  psdc chapter/*.psd -o rendered/    compile a batch in parallel
  psdc page.psd -o page.jpg -Q 85    write a JPEG
  psdc page.psd --list-text          show the type layers
  psdc page.psd --set-text 'Title=Hello\\nworld' -o hello.png";

fn parse_format(s: &str) -> Result<Format, String> {
    Format::from_extension(s).ok_or_else(|| format!("unknown format {s:?} (png, jpg, webp, tif, avif)"))
}

fn parse_color(s: &str) -> Result<[u8; 3], String> {
    let hex = s.trim_start_matches('#');
    let v = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6).ok_or_else(|| format!("{s:?} is not RRGGBB"))?;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn parse_set_text(s: &str) -> Result<(String, String), String> {
    let (layer, text) = s.split_once('=').ok_or("expected LAYER=TEXT")?;
    Ok((layer.to_string(), text.replace("\\n", "\n")))
}

fn font_db(cli: &Cli) -> FontDb {
    let mut db = FontDb::default_cache_path().map(FontDb::with_cache).unwrap_or_default();
    for dir in &cli.fonts {
        db.add_dir(dir);
    }
    if let Some(paths) = std::env::var_os(FONTS_ENV) {
        for dir in std::env::split_paths(&paths) {
            db.add_dir(dir);
        }
    }
    let local = Path::new("fonts");
    if local.is_dir() && !cli.fonts.iter().any(|f| f == local) {
        db.add_dir(local);
    }
    if !cli.no_system_fonts {
        db.add_system_fonts();
    }
    let _ = db.save_cache();
    db
}

/// The output path and format for `input`.
fn output_for(cli: &Cli, input: &Path) -> Result<(PathBuf, Format)> {
    let single = cli.output.as_ref().filter(|out| cli.inputs.len() == 1 && !out.is_dir());
    let format = match (cli.format, single) {
        (Some(f), _) => f,
        (None, Some(out)) if out.extension().is_some() => Format::from_path(out)
            .with_context(|| format!("unknown image format for {} (use png, jpg, webp, tif or avif)", out.display()))?,
        _ => Format::Png,
    };
    let name = || PathBuf::from(input.file_name().unwrap_or_default()).with_extension(format.extension());
    let path = match (&cli.output, single) {
        (_, Some(out)) => out.clone(),
        (Some(dir), None) => dir.join(name()),
        (None, _) => input.with_extension(format.extension()),
    };
    Ok((path, format))
}

fn list_text(doc: &Document, input: &Path) {
    let mut out = format!("{}\n", input.display());
    for (i, l) in doc.layers.iter().enumerate() {
        if let Some(text) = l.text() {
            out += &format!("  {i:4}  {:?}  {:?}\n", l.name, text.replace('\r', "\n"));
        }
    }
    print!("{out}");
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn compile(cli: &Cli, fonts: &FontDb, input: &Path) -> Result<()> {
    let (output, format) = output_for(cli, input)?;
    let start = Instant::now();
    let mut doc = Document::open(input).with_context(|| format!("cannot read {}", input.display()))?;
    if cli.list_text {
        list_text(&doc, input);
        return Ok(());
    }
    for (layer, text) in &cli.set_text {
        if doc.set_text(layer, text).with_context(|| format!("cannot set the text of {layer:?}"))? == 0 {
            bail!("{}: no type layer named {layer:?}", input.display());
        }
    }
    let parsed = ms(start);
    let options = RenderOptions { keep_text_raster: cli.keep_text, text_masks: cli.text_masks.is_some() };
    let rendered = render(&doc, fonts, &options);
    let drawn = ms(start);
    let encode = EncodeOptions { compression: cli.compression, quality: cli.quality, background: cli.background };
    let bytes = rendered.image.encode(format, &encode).with_context(|| format!("cannot encode {}", output.display()))?;
    std::fs::write(&output, bytes).with_context(|| format!("cannot write {}", output.display()))?;
    if let Some(dir) = &cli.text_masks {
        let stem = input.file_stem().unwrap_or_default().to_string_lossy();
        for m in &rendered.text_masks {
            let path = dir.join(format!("{stem}.text{:03}.png", m.layer));
            let data = png::encode(
                doc.width,
                doc.height,
                png::ColorType::Gray,
                &m.to_canvas(doc.width, doc.height),
                cli.compression,
            );
            std::fs::write(&path, data).with_context(|| format!("cannot write {}", path.display()))?;
        }
    }
    if !cli.quiet {
        for w in &rendered.warnings {
            eprintln!("warning: {}: {w}", input.display());
        }
        if cli.timings {
            eprintln!(
                "{} -> {} ({}x{}; parse {parsed:.0} ms, render {:.0} ms, write {:.0} ms)",
                input.display(),
                output.display(),
                doc.width,
                doc.height,
                drawn - parsed,
                ms(start) - drawn
            );
        } else {
            eprintln!("{} -> {}", input.display(), output.display());
        }
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<usize> {
    if let Some(n) = cli.jobs {
        rayon::ThreadPoolBuilder::new().num_threads(n).build_global().context("cannot configure threads")?;
    }
    if let Some(out) = &cli.output {
        if cli.inputs.len() > 1 {
            if out.exists() && !out.is_dir() {
                bail!("{} is not a directory; several inputs need an output directory", out.display());
            }
            std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
        }
    }
    if let Some(dir) = &cli.text_masks {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let start = Instant::now();
    let fonts = font_db(cli);
    if cli.timings && !cli.quiet {
        eprintln!("fonts: {} faces in {:.0} ms", fonts.len(), ms(start));
    }
    let failures: Vec<String> =
        cli.inputs.par_iter().filter_map(|input| compile(cli, &fonts, input).err().map(|e| format!("{e:#}"))).collect();
    for f in &failures {
        eprintln!("error: {f}");
    }
    if cli.inputs.len() > 1 && !cli.quiet {
        eprintln!(
            "{} compiled, {} failed in {:.2} s",
            cli.inputs.len() - failures.len(),
            failures.len(),
            ms(start) / 1e3
        );
    }
    Ok(failures.len())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
