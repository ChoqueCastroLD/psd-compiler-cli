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
- Color lookup adjustments: CUBE, 3DL and LOOK tables, abstract and device-link ICC profiles.
- Duotone documents shown in their inks, from Photoshop's preview table or the ink curves.
- Smart objects linked to files outside the document, found next to it or at their absolute path; `Document::open` and `Document::set_base_dir`.
- Smart filters on re-rendered smart objects: blurs, sharpening, unsharp mask, high pass, median, maximum, minimum, offset, custom, mosaic, invert, solarize, average, curves and brightness/contrast, with their blending options.
- Writing CMYK, Lab, duotone, indexed and 32-bit documents.
- Reference test against the composites Photoshop stored in real files, with a match rate per feature (`PSDC_REFERENCE_DIR`); CI runs it on the test files of psd-tools, ag-psd, webtoon/psd, psd.rb, chinedufn/psd, PhotoshopAPI, psd_sdk, psd.js and Krita (502/511 match; see docs/REFERENCE.md).
- Stroke emboss bevels.
- Non-legacy brightness/contrast as Photoshop runs it: brightness, then contrast.
- Color lookup tables read in the order the layer names; color balance midtones as a per-channel gamma.
- Vibrance as Photoshop runs it: chroma scaled about the luminance in linear light, less for saturated colors.
- Patterns shrunk below their size average the texels each pixel covers.
- Strokes measured with Photoshop's chamfer distance, faceted like its large strokes.
- Several strokes, shadows and overlays per layer in Photoshop's order (listed top first), upper strokes covering lower ones.
- Gradient interpolation methods: Linear (linear light), Perceptual (Oklab) and Smooth, in gradient fills, overlays, strokes and gradient maps.
- Effect contours, glow range and jitter, Blend If, channel restrictions, "Blend Interior Effects as Group", layer style pattern origin (`fxrp`), CMYK plates and 32-bit linear compositing.

### Changed

- Pass-through groups with lowered fill mix their passed-through and isolated results, as Photoshop does.
- Vector masks and shape strokes are rasterized with exact-area coverage.
- Text blends with gamma 1.53, like Photoshop's "Blend Text Colors Using Gamma".
- Interior effects on layers with blend modes are blended over the blended layer, as Photoshop does.
- Type layers are clipped to the canvas and drawn in bands, so there is no size limit.
- Text that can't be parsed falls back to the cached pixels.

### Fixed

- Embedded smart object files were missed in some documents (global block padding).
- Effects and fills calibrated against Photoshop composites:
  - Gradients use Hermite ramps for smooth stops, honor stop midpoints, interpolate in Lab when asked, and linear gradients span the center line as far as the box clips it.
  - Strokes measure from pixel centers; their inner band replaces the layer's pixels; outside strokes knock out drop shadows the layer conceals; stroke gradients span the stroke's outer edge.
  - A shape's vector stroke is drawn above its overlays and clipped layers, below its layer style strokes.
  - Effects in Color/Linear Dodge, Burn, Difference and Vivid/Linear Light fade toward the mode's neutral color instead of losing alpha (fill opacity too).
  - Smooth bevels are lit from the blurred shape; smooth emboss and pillow emboss blur over half their size, and a pillow is lit per side of its fold; chisel bevels rise 0.35 of their depth.
  - Pass-through groups with fill opacity mix what passes through with what they show apart, by the fill; 16-bit posterize uses 16-bit levels.
- 16-bit Lab a/b channels are read and written at 256 per unit around 32768.

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
