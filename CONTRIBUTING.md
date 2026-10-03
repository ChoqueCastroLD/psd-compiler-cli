# Contributing

Thanks for helping make PSD Compiler better. Bug reports with a sample file are the most valuable contribution of all.

## Dev loop

```sh
cargo build                     # debug build
cargo test                      # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo run --release -- page.psd --timings
```

Some text tests need a common system font: DejaVu Sans, Liberation Sans or Arial. If none is installed they print `skipped` and pass. On Debian/Ubuntu, run `sudo apt install fonts-dejavu-core`.

## Project layout

```text
src/
  psd/        parsing: reader, channels, descriptors, EngineData
  text/       type engine: runs, layout, warps, outlines
  render/     layers, effects, distance fields, canvas, compositing
  fonts.rs    font discovery
  png.rs      parallel PNG encoder
  main.rs     the psdc CLI
tests/
  common/     PSD builder: writes test documents in code
  parse.rs    format coverage
  render.rs   compositing and effects
  text.rs     type layers (needs a system font)
  cli.rs      end-to-end runs of the psdc binary
examples/
  demo.rs     generates docs/assets/demo.psd
  render.rs   minimal library usage
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the pieces fit.

## Writing tests

Never commit real PSDs or font files. Build documents in code with `tests/common`:

```rust
mod common;
use common::{Effect, Layer, Psd, Text};

let psd = Psd::new(200, 100)
    .layer(Layer::solid("bg", 0, 0, 200, 100, [255, 255, 255, 255]))
    .layer(Layer::text("title", &Text::new("Hi", "DejaVuSans-Bold", 40.0, [0.0; 3], 10.0, 60.0))
        .effects(&[Effect::Stroke { size: 3.0, rgb: [255, 0, 0], position: "OutF" }]));
let doc = psd_compiler::Document::parse(&psd.build()).unwrap();
```

The builder supports PSB, 16-bit, every channel compression, groups, clipping, masks, blend modes, type layers with runs, boxes and warps, and the supported effects.

## Changing rendering behavior

Rendering rules come from measurements against Photoshop, not guesses. If you change one:

1. Make a minimal PSD in Photoshop that isolates the behavior.
2. Compare Photoshop's raster with `psdc`'s, using IoU of coverage for text and per-pixel difference for compositing.
3. Put the numbers in the PR description, and in [docs/TEXT_ENGINE.md](docs/TEXT_ENGINE.md) when they change a documented rule.
4. Add a regression test with the builder.

## Code style

- `cargo fmt` (120 columns) and clippy with no warnings.
- Errors go through `psd_compiler::Error`. Never panic on input data.
- Prefer clear names to comments. Comment only what the code cannot say: the *why*, a spec quirk, a measured constant.
- New public items need doc comments (`#![warn(missing_docs)]`).

## Pull requests

- Keep each PR focused on one change.
- CI runs fmt, clippy and the tests on Linux, macOS and Windows.
- Add a line to [CHANGELOG.md](CHANGELOG.md) under *Unreleased*.
