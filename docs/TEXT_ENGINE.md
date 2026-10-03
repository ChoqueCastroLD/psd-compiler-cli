# The text engine

Re-rendering Photoshop text so it matches Photoshop's raster takes much more than "draw the string with the font". This page records the rules PSD Compiler follows and the measurements behind them.

All numbers come from rendering controlled test documents in Photoshop and comparing glyph coverage (IoU) with the output of `psdc`.

## Where text lives in a PSD

A type layer carries a `TySh` tagged block:

```text
TySh
 ├─ transform           6 × f64 (xx xy yx yy tx ty), maps text space to the canvas
 ├─ text descriptor     Action descriptor
 │   ├─ Txt             plain text
 │   ├─ AntA            anti-aliasing (Anno, AnSh, AnCr, AnSt, AnSm)
 │   ├─ bounds          text-space bounds (also the warp rectangle)
 │   └─ EngineData      raw bytes, PostScript-like syntax
 │       ├─ EngineDict.Editor.Text           the characters ('\r' between paragraphs)
 │       ├─ EngineDict.StyleRun              run lengths + style sheets
 │       ├─ EngineDict.ParagraphRun          run lengths + paragraph sheets
 │       ├─ EngineDict.Rendered.Shapes       point vs box text, BoxBounds
 │       └─ ResourceDict                     FontSet, normal style/paragraph sheets
 └─ warp descriptor     warpStyle, warpValue, warpPerspective, warpPerspectiveOther, warpRotate
```

Styles resolve in this order: **normal style sheet → paragraph's default style sheet → run's style sheet.** Paragraph properties resolve the same way. Each character ends up with a complete `Style` and `Paragraph`.

## Layout

- **Shaping.** Characters with the same style are shaped together with rustybuzz using their font. Characters the font cannot draw go to a fallback face.
- **Pair kerning.** Each character has an `AutoKerning` flag, and **character *i*'s flag governs the pair (*i*−1, *i*).** The engine shapes each span twice, with and without the `kern` feature, and picks the advance per glyph based on the flag of the character after it. Getting this wrong shifts whole words by a pixel or two, which is very visible in comic lettering.
- **Tracking** adds `tracking / 1000 · size` after every character.
- **Leading.** Auto leading is `paragraph.AutoLeading · size` (1.2 by default). Line spacing uses the largest leading on the line.
- **First baseline.** For box text it sits at `box.top + max ascent` of the first line. Point text has its baseline at y = 0 of text space.
- **Wrapping** is greedy and breaks at spaces. Words longer than the line are split by character. Trailing spaces never count toward a line's width.
- **Justification.** Left, center and right shift by 0, ½ and 1 of the free space. The justify modes spread the free space over the spaces of every line, except the last line of a paragraph, which aligns according to the mode (*justify-all* includes it).
- **Indents and spacing.** First-line indent, start and end indent, and space before and after paragraphs.
- **Faux styles.**
  - Small caps scale lowercase letters to 75%.
  - Faux italic skews by 0.2.
  - Faux bold strokes the outline at 4% of the size.
  - Underline and strikethrough are 5% of the size thick.

## Rasterization and anti-aliasing

Glyphs fill with tiny-skia at the layer transform. Photoshop's anti-aliasing modes differ only in how partial edge coverage is mapped. The best fit was a power curve:

| `AntA` | Mode | Coverage exponent |
|---|---|---|
| `Anno` | None | aliased (no anti-aliasing) |
| `AnSh` | Sharp | 1.0 (linear coverage) |
| `AnCr`, `AnSm` | Crisp / Smooth | **0.775** |
| `AnSt` | Strong | **0.55** |

An exponent below 1 makes edges fuller. Strong at 0.55 matches Photoshop's heavier look on thin strokes.

## Warps

Photoshop warps are **bicubic Bézier patches**: 4×4 control points over the warp rectangle. The rectangle is the layer's stored `bounds` when present, and the glyph bounds otherwise.

- **Arc family** (Arc, Arc Lower, Arc Upper, Arch, Bulge, Shell Lower, Shell Upper). Control points are placed on circular arcs whose sweep is proportional to bend.
- **Table presets** (Flag, Wave, Fish, Rise, Fisheye, Inflate, Squeeze). Control-point offsets are linear in bend. The exact offsets at bend +50% are stored as rational tables in units of the rectangle's size, for example Flag = (0, ∓1) on the inner columns. Negative bends negate the table, except **Squeeze, which has its own negative table.**
- **Twist** rotates and scales the four inner control points about the center by `bend · 90°`.
- **Distortion.** Horizontal distortion is applied **first**: column *i* scales about the midpoint of its end points by `1 + h(2i/3 − 1)`. Vertical distortion comes second: the top and bottom rows scale about their chord midpoints by `1 − v` and `1 + v`, and inner rows interpolate.
- **Vertical** orientation swaps axes.

Outlines are flattened adaptively before warping, so straight segments bend smoothly. Against reference renders, all presets reach IoU 0.94–0.98. The linear presets score about 0.97.

## Effects

Effects run on a **4× supersampled** inside/outside grid derived from anti-aliased coverage. Exact Euclidean distance transforms are computed on it (Felzenszwalb & Huttenlocher), then downsampled back to coverage.

| Effect | Model |
|---|---|
| Stroke | Outside: distance to shape ≤ size. Inside: distance to complement ≤ size. Center: ±size/2. |
| Drop shadow | Offset by distance at angle (global or local). The shape grows by `spread · size`, then a Gaussian blur with **σ = 0.45 · (size − spread)**, truncated at `size − spread`. |
| Outer glow | Same blur model around the shape with no offset. |
| Color overlay | Replaces the layer color, using the effect's blend mode and opacity. |

`Scl ` (effect scale) multiplies all sizes. Layer fill opacity affects the content but not the effects, as in Photoshop.

## Known differences

- **Vertical text** is drawn horizontally (with a warning).
- OpenType features other than kerning (ligature options, stylistic sets) use rustybuzz defaults.
- Font versions matter. A different build of the "same" font changes metrics, and that is the most common cause of mismatches.
