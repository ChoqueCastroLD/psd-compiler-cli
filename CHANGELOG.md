# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
