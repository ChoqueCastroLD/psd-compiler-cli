# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Color modes: indexed, Lab, duotone and multichannel.
- Adjustment layers: levels, curves, brightness/contrast, hue/saturation, color balance, vibrance, exposure, selective color, channel mixer, gradient map, photo filter, invert, posterize, threshold and black & white.
- Fill layers, vector masks, knockout.
- All layer effects: inner shadow, inner glow, satin, bevel and emboss, gradient and pattern overlays, and gradient and pattern strokes.
- Vertical text, OpenType features from character styles, synthetic super/subscript.
- Custom and quilt warps.
- Smart objects re-rendered from embedded PSD/PSB, PNG or JPEG (`--render-smart-objects`).
- Text replacement: `Document::set_text`, `--set-text` (also inside smart objects) and `--list-text`.
- JPEG, WebP, TIFF and AVIF output (`-F`, `-Q`, `--background`).
- Saving the edited document as PSD/PSB: `Document::to_psd`, `-o out.psd`.
- Closest-style font substitution with synthetic bold/italic, `FontDb::alias` and `--font-map`.

### Changed

- Type layers are clipped to the canvas and drawn in bands, so there is no size limit.
- Text that can't be parsed falls back to the cached pixels.

### Fixed

- Embedded smart object files were missed in some documents (global block padding).

## [0.1.0] - 2026-10-03

First public release.

### Added

- PSD and PSB parsing at 1/8/16/32 bits, in RGB, grayscale, bitmap and CMYK, with raw, RLE, ZIP and ZIP-predicted channels.
- Compositing with all 27 blend modes, groups (including pass-through), clipping masks, layer masks, opacity and fill opacity.
- Type layer re-rendering:
  - style and paragraph runs, Photoshop pair-kerning rules, tracking, leading, scaling, baseline shift, caps, faux styles and decorations;
  - point and box text with every justification mode;
  - calibrated anti-aliasing.
- All 15 warp presets, with horizontal and vertical distortion.
- Layer effects: stroke, drop shadow, outer glow and color overlay.
- Font discovery from `--fonts`, `PSDC_FONTS`, `./fonts` and the system font folders, with an on-disk index cache.
- `psdc` CLI: parallel batches, text masks, compression level and timings.
- Parallel PNG encoder.
- A library API: `Document`, `FontDb`, `render` and `Image`.
