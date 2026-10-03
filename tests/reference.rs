//! Compares renders against the merged image Photoshop stored in real files.
//!
//! Point `PSDC_REFERENCE_DIR` at one or more directories of PSD/PSB files saved by Photoshop with
//! "Maximize compatibility" (separated by `:`), e.g. the psd-tools test files:
//!
//! ```text
//! PSDC_REFERENCE_DIR=psd-tools/tests/psd_files cargo test --release --test reference -- --nocapture
//! ```
//!
//! Each file is rendered from its layers, flattened onto white, and compared with the stored
//! composite. A file matches when the mean difference is at most 2 (of 255) and at most 1% of
//! pixels differ by more than 16. The test prints the result of every file and the match rate of
//! every feature, and fails when fewer than `PSDC_REFERENCE_MIN` (default 0.95) of the files match.
//! Files whose stored composite is a single color count toward the total but not toward the
//! feature rates, since they cannot show whether a feature renders right.
//! Without `PSDC_REFERENCE_DIR` it does nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use psd_compiler::{render, Document, FontDb, Image, RenderOptions};
use rayon::prelude::*;

const MAX_MEAN: f64 = 2.0;
const MAX_OFF_PERCENT: f64 = 1.0;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("psd") || x.eq_ignore_ascii_case("psb")) {
            out.push(p);
        }
    }
}

fn flat(img: &Image) -> Vec<f64> {
    img.data
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as f64 / 255.0;
            (0..3).map(move |c| p[c] as f64 * a + 255.0 * (1.0 - a))
        })
        .collect()
}

/// Mean difference and percentage of pixels off by more than 16 (largest channel difference).
fn compare(got: &Image, want: &Image) -> (f64, f64) {
    let (g, w) = (flat(got), flat(want));
    let n = (got.width * got.height).max(1) as f64;
    let (mut sum, mut off) = (0.0, 0usize);
    for (a, b) in g.chunks_exact(3).zip(w.chunks_exact(3)) {
        let d = (0..3).map(|c| (a[c] - b[c]).abs()).fold(0.0, f64::max);
        sum += d;
        off += (d > 16.0) as usize;
    }
    (sum / n, off as f64 * 100.0 / n)
}

enum Outcome {
    Compared { mean: f64, off: f64, uniform: bool, features: Vec<String> },
    Skipped(String),
}

fn check(path: &Path, fonts: &FontDb) -> Outcome {
    let doc = match std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|b| Document::parse(&b).map_err(|e| e.to_string()))
    {
        Ok(d) => d,
        Err(e) => return Outcome::Skipped(format!("unreadable: {e}")),
    };
    let Some(want) = doc.stored_composite() else {
        return Outcome::Skipped("no stored composite".into());
    };
    if doc.layers.is_empty() {
        return Outcome::Skipped("no layers".into());
    }
    let options = RenderOptions { keep_text_raster: true, ..Default::default() };
    let got = render(&doc, fonts, &options).image;
    let (mean, off) = compare(&got, &want);
    let uniform = want.data.chunks_exact(4).all(|p| p == &want.data[..4]);
    Outcome::Compared { mean, off, uniform, features: doc.features() }
}

#[test]
fn matches_photoshop_composites() {
    let Ok(dirs) = std::env::var("PSDC_REFERENCE_DIR") else {
        eprintln!("PSDC_REFERENCE_DIR not set; skipping");
        return;
    };
    let min: f64 = std::env::var("PSDC_REFERENCE_MIN").ok().and_then(|v| v.parse().ok()).unwrap_or(0.95);
    let mut files = Vec::new();
    for d in std::env::split_paths(&dirs) {
        walk(&d, &mut files);
    }
    files.sort();
    assert!(!files.is_empty(), "no PSD/PSB files under {dirs}");
    let fonts = FontDb::new();
    let results: Vec<_> = files.par_iter().map(|p| (p, check(p, &fonts))).collect();

    let mut per_feature: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let (mut compared, mut matched, mut uniforms) = (0, 0, 0);
    for (path, outcome) in &results {
        let name = path.display();
        match outcome {
            Outcome::Skipped(why) => println!("{:>16}  {name}  ({why})", "skip"),
            Outcome::Compared { mean, off, uniform, features } => {
                let ok = *mean <= MAX_MEAN && *off <= MAX_OFF_PERCENT;
                compared += 1;
                matched += ok as usize;
                let note = if !ok {
                    "  <<<"
                } else if *uniform {
                    "  (single color)"
                } else {
                    ""
                };
                println!("{mean:7.2} {off:6.2}%  {name}{note}");
                if !ok {
                    println!("{:>17} {}", "", features.join(", "));
                }
                if *uniform {
                    uniforms += 1;
                    continue;
                }
                for f in features {
                    let e = per_feature.entry(f).or_default();
                    e.0 += ok as usize;
                    e.1 += 1;
                }
            }
        }
    }
    println!("\n{:<40} {:>7} {:>6}", "feature", "match", "rate");
    for (f, (ok, n)) in &per_feature {
        println!("{f:<40} {:>7} {:>5.0}%", format!("{ok}/{n}"), *ok as f64 * 100.0 / *n as f64);
    }
    let rate = matched as f64 / compared.max(1) as f64;
    println!("\n{matched}/{compared} files match ({:.1}%); {uniforms} stored a single color", rate * 100.0);
    assert!(rate >= min, "{matched}/{compared} files match ({:.1}%), below {:.1}%", rate * 100.0, min * 100.0);
}
