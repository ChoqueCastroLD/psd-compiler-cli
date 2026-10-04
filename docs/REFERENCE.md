# Reference test

`tests/reference.rs` renders real Photoshop files from their layers and compares the result with the
merged image Photoshop stored in them ("Maximize compatibility"). Both images are flattened onto
white. A file matches when the mean difference is at most 2 (of 255) and at most 1% of pixels differ
by more than 16 in any channel. The test prints every file, the features each failing file uses, and
the match rate of every feature.

```sh
PSDC_REFERENCE_DIR=dir1:dir2 cargo test --release --test reference -- --nocapture
```

`PSDC_REFERENCE_MIN` (default `0.95`) is the match rate below which the test fails. Files without a
stored composite, without layers, or whose stored composite is an all-black placeholder (no version
info block, as in the contents of some smart objects) are skipped. Files whose stored composite is a
single color (121 of the 734, e.g. an adjustment over an empty canvas) count toward the total but not
toward the feature rates, since they cannot show whether a feature renders right.

Some features are only covered by files that combine many of them: Photo Filter, Selective Color,
Channel Mixer, Vibrance and Color Lookup appear in psd-tools only in `fill_adjustments.psd`, and in
ag-psd only under a final Threshold or Gradient Map that flattens their effect, so their rates say
little about them.

CI runs it on the test files of [psd-tools](https://github.com/psd-tools/psd-tools),
[ag-psd](https://github.com/Agamnentzar/ag-psd), [webtoon/psd](https://github.com/webtoon/psd),
[psd.rb](https://github.com/layervault/psd.rb), [chinedufn/psd](https://github.com/chinedufn/psd),
[PhotoshopAPI](https://github.com/EmilDohne/PhotoshopAPI), [psd_sdk](https://github.com/MolecularMatters/psd_sdk)
[psd.js](https://github.com/meltingice/psd.js), [Krita](https://invent.kde.org/graphics/krita) (its PSD import tests) and
[Aspose.PSD for .NET](https://github.com/aspose-psd/Aspose.PSD-for-.NET) (its example files),
[oov/psd](https://github.com/oov/psd) (its test data) and [Artal](https://github.com/EvineDev/Artal) (its test cases), each pinned to a commit. Files ag-psd wrote itself (`test/write`, `expected.psd`) are left out: they
carry no Photoshop composite, and neither are webtoon/psd's deliberately broken files or the Aspose
examples written back by Aspose (names with Changed, Added, Edited, Merged, Flattened or `_out`,
plus `CropTest.psd`, whose composite carries Aspose's evaluation watermark, and
`ImageWithTextLayer.psd`, whose text layer Aspose wrote, and `HasFont.psd` with its copy
`asposeImage02.psd`, whose composite text is set differently from their own text layers), nor oov/psd's files saved by other
painting apps (no Photoshop version info). None of the
files are committed here.

## Results

| Suite | Match |
|---|---|
| psd-tools `tests/psd_files` | 270 / 274 (98.5%) |
| ag-psd `test` | 74 / 76 (97.4%) |
| webtoon/psd, psd.rb, chinedufn/psd | 64 / 65 (98.5%) |
| PhotoshopAPI, psd_sdk, psd.js | 76 / 76 |
| Krita `plugins/impex/psd/tests/data` | 20 / 20 |
| Aspose.PSD `Examples/Data/PSD` | 192 / 197 (97.5%) |
| oov/psd `testdata` | 22 / 22 |
| Artal `tests/cases` | 4 / 4 |
| All | 722 / 734 (98.4%) |

### Misses

| File | Why |
|---|---|
| `blend-modes/dissolve.psd` | Dissolve uses Photoshop's random pattern; ours is a different one with the same density. |
| `effect-stroke-gradient.psd` | A noise gradient. Photoshop generates its colors from a stored seed and roughness with an unpublished algorithm; we draw a ramp between the color limits. |
| `fill_adjustments.psd` (also in webtoon/psd as `fillAdjustments.psd`) | Eleven adjustment layers in a row. Every step up to Hue/Saturation matches its own single-adjustment test files. Photoshop's last two steps (Color Balance, then a Photo Filter with the Lab color (67, 32, 120)) act almost like one affine color map, but no Color Balance and Photo Filter model we tried fits it, even with every weight free (mean 13.0). |
| `third-party-psds/cactus_top.psd` | Written by a third-party tool: its merged image has an opaque black first column that its layers don't have. |
| ag-psd `read/effects`, `read-write/effects` | A noise gradient, as above. |
| Aspose `artboard2.psd` | Dissolve groups, as above. |
| Aspose `StrokeNoise.psd` | A noise gradient, as above. |
| Aspose `White 3D Text Effect.psd` | Hundreds of stacked extrusion layers with bevels and satins: the noise and faces match, but everything comes out about 2.5 levels brighter through the Brightness/Contrast, Color Balance, Vibrance and Curves stack (mean 4.0, 0.8% of pixels). |
| Aspose `ColorBalance.psd` | Extreme sliders on a 10×10 swatch: Photoshop's red patch keeps less than its HSL lightness under Preserve Luminosity (mean 2.11, just over the limit). No common lightness (HSL, Rec. 601 or 709 luma, L\*, linear light) is kept on it. |
| Aspose `PhotoFilterAdjustmentLayer.psd` | A Photo Filter with a Lab color (88, −79, −118) far outside RGB (mean 4.0, 0.01% of pixels): the model below lands close but not within 2. |

Closing the color-adjustment misses needs Photoshop's own renders of a color chart under grids of Color Balance, Photo Filter and Brightness/Contrast settings. Photopea is not a substitute: its Color Balance differs from Photoshop's.

## Calibrated models

Measured against the stored composites:

- **Vector shapes** cover each pixel by the exact area inside the path (tiny-skia's anti-aliasing
  steps in quarters along near-horizontal edges, which Photoshop's does not).
- **Gradients**: smooth stops use Hermite ramps between stops and honor midpoints, with
  Catmull-Rom tangents over the stop index (half the step at the ends), so unevenly spaced stops
  keep per-step slopes. The
  interpolation method (`gs99` in gradient fills and overlays, the method tag of version 3 gradient
  maps) picks the space: "Classic" runs in encoded RGB, "Linear" in linear light, "Perceptual"
  in Oklab, "Smooth" in Oklab without the classic smoothness; Lab documents interpolate in Lab. A linear gradient spans the line through the box center as far
  as the box clips it: half-length `min(w/|cos|, h/|sin|) · scale / 2`.
- **Strokes** measure a chamfer distance from pixel centers: steps to the 16 nearest neighbors at
  their true lengths (1, √2, √5), so they are exact along the axes, diagonals and knight moves and
  up to 2.7% long in between (large strokes around round shapes come out slightly faceted). A pixel
  costs `1 − alpha` outward and `alpha` inward. The inside band replaces the layer's pixels;
  outside strokes knock out the drop shadow under the layer. Gradient strokes span the stroke's
  outer edge. Several strokes on one layer are listed top first; each covers the ones below it,
  so a multiplying stroke multiplies the backdrop rather than the stroke under it. Other
  multiple effects are listed top first too.
- **Shape strokes** (vector stroke) are drawn above the layer's overlays and clipped layers, below
  its layer style strokes, keeping the shape's alpha.
- **Shadows and glows**: Gaussian with sigma `0.45 · (size − spread)`, its taps closer than
  `size − spread` (a size of 5 blurs over 4 pixels each way). Offsets are whole pixels, halves
  rounded away from zero (5 at 120° moves 3 across and 4 down).
- **Bevels**: smooth bevels are lit from the blurred shape; Smooth Emboss and Pillow Emboss blur over
  half their size with 0.6 of the lift, and a pillow is lit per side of its fold. Chisel bevels
  (hard and soft) rise 0.35 of their depth, so their sides barely shade. A stroke emboss is an
  inner bevel of the layer with its first stroke, painted only on the stroke. The gloss contour
  maps the lit shade before it is split into highlight and shadow around the flat level. Soften
  blurs the highlight and shadow, not the height, so it fades a steep bevel's shading at the
  shape's edge (White 3D Text Effect's extrude undersides). A texture adds the pattern's
  luminance to the height (light is high, Invert flips it), scaled by the texture depth but not
  by the bevel's; its strength comes from the one sample file (smart_object_file_no_warp).
- **Strokes on shape layers** follow the stored pixels, with Photoshop's own anti-aliasing (on a
  45° edge it is sharper than exact area coverage); they follow the path only where the pixels lost
  the shape.
- **Neutral-color modes** (Color/Linear Dodge, Burn, Difference, Vivid/Linear Light...): effects and
  fill opacity fade the color toward the mode's neutral color rather than lowering alpha. Over a
  transparent backdrop the layer shows plainly at its fill opacity; over a covered one the faded
  color blends at full strength.
- **Interior effects over blend modes**: overlays, inner shadows and glows, satin and inner bevels
  are blended onto the layer after it was blended with the backdrop, inside its shape, rather than
  onto the layer alone. The same holds for a Normal layer with fill below 100% whose interior
  effects use another mode (a fill-0 layer with a Color Dodge bevel lightens the backdrop).
- **Brightness/Contrast** (non-legacy): brightness first, then contrast. Brightness moves each
  level `b` steps along a fixed field (so +b and −b undo each other and keep black and white);
  contrast is a spline through (55, 55 − 0.27c) and (200, 200 + 0.27c), pivoting on middle gray.
  Grayscale and CMYK documents use the same curve on their stored values.
- **Color Balance** midtones bend each channel by a gamma of `2^(−m/100)`, each output depending
  only on its own channel. Shadows and highlights then shift the channel by the slider times a
  weight of its value that depends on the slider's sign: raising the shadows peaks around a
  third of the way up, lowering them fades out from black by three quarters; raising the
  highlights grows steadily toward white, lowering them barely acts. Pure colors (channels at 0
  or 255) stay put. Preserve Luminosity restores the HSL lightness. Fit together to Aspose's
  Mixer_ipad_Hand_W_crash (colored pixels), ColorBalance (extreme sliders) and White 3D Text
  Effect (whose grays, once Brightness/Contrast and Curves are undone, show the balance alone).
- **Photo Filter** multiplies by the filter color at half the stated density, then restores the
  Rec. 601 luminosity if asked. A Lab color outside RGB keeps its out-of-range channels (Lab
  (88, −79, −118) acts as (−1.11, 1.04, 1.76)). Fit to Aspose's PhotoFilterAdjustmentLayer (100%)
  and AllAdjustments (25%), both Lab colors; the half density is sharp in both.
- **Vibrance**: both sliders scale chroma about the luminance (0.32, 0.62, 0.06) in gamma 2.4
  light, Saturation by `1 + s`, Vibrance by `1 + v · (1 − S) / 3` where S is the HSB saturation.
  Fit to the two Aspose.PSD files that hold a Vibrance layer alone; skin tones get no special case.
- **Selective Color**: each range moves a channel by `((−1 − c) · k − c)` (c its cyan, magenta or
  yellow slider, k its black) of the channel's ink (relative) or of the whole range (absolute),
  kept within the channel's room, weighted by how much the pixel belongs to the range (after
  [pkh.me's reverse engineering](https://blog.pkh.me/p/22-understanding-selective-coloring-in-adobe-photoshop.html)).
- **Outer glows** never show through a faded fill: under the layer only the part outside the shape
  remains, whatever the fill opacity (Aspose's FillOpacitySample).
- **Clipping masks**: clipped layers paint over the base's color as if it were opaque; the base's
  alpha then limits the group. With "Blend Clipped Layers as Group" off, the clipped layers blend
  with their own modes onto what is below the base, within its pixels and at its opacity, and hide
  the base where they cover it.
- **Channel Mixer** in CMYK mixes ink, not the stored (inverted) values; each plate mixes the
  color plates or black from itself.
- **CMYK merged images with transparency** are matted onto white in ink (ink × alpha), so the
  reference test divides the inks by alpha before converting.
- **Color Lookup** CUBE tables name their loops outer to inner: `bgrOrder` runs red fastest,
  `rgbOrder` blue fastest.
- **Patterns** shrunk below their size average the texels each pixel covers rather than sample one. They
  also sit a quarter pixel further along both axes than enlarged ones (fit to a 7% overlay in
  PhotoshopAPI's smart_object_file_no_warp and an 81% one in ag-psd's pattern test).
- **Text** blends with gamma 1.53 (Photoshop's "Blend Text Colors Using Gamma"): coverage mixes
  `B^γ` and `S^γ`, so antialiased edges look heavier than a plain alpha blend.
- **Effect blend modes** may be stored with long names (`colorBurn`, `softLight`...) in recent files.
- **Pass-through groups with fill below 100%**: the group's content is what it adds passing
  through mixed with what it shows apart, by the fill; the fill then fades it. Adjustment layers
  inside reach only the group's own content.
- **Channel restrictions** keep the backdrop's values in unchecked channels.
- **16-bit Lab**: a/b channels run 256 per unit around 32768; L uses the full range.
- **Photo filter** colors are stored in RGB, HSB, CMYK, Lab (L/100, a and b as signed /100) or gray
  (1 − v/10000); the filter multiplies at its density, then restores luminosity when asked.
