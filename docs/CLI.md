# `psdc` command-line reference

```text
psdc [OPTIONS] <INPUT>...
```

`psdc` compiles one or more PSD/PSB files to PNG, JPEG, WebP, TIFF or AVIF, or saves them back as PSD/PSB after text edits. When there are several inputs they are processed in parallel. Each file renders independently, so a broken file does not stop the batch.

## Arguments

| Argument | Description |
|---|---|
| `<INPUT>...` | One or more `.psd` or `.psb` files. Shell globs work: `psdc chapter/*.psd`. |

## Options

| Option | Default | Description |
|---|---|---|
| `-o, --output <PATH>` | `INPUT.png` next to each input | With **one** input this is the output file, unless `PATH` is an existing directory. With **several** inputs it is a directory, created if needed, and each output is named `STEM.EXT`. |
| `-F, --format <FORMAT>` | from `-o`, else `png` | `png`, `jpg`, `webp`, `tif`, `avif`, or `psd` to save the edited document (keeping the input's `.psd`/`.psb` extension). |
| `-Q, --quality <1-100>` | `90` | JPEG and AVIF quality. WebP is lossless. |
| `--background <RRGGBB>` | `ffffff` | Color that transparency is flattened onto for JPEG. |
| `--set-text <LAYER=TEXT>` | none | Replace the text of every type layer named `LAYER`; repeatable. `\n` starts a new paragraph. `Outer/Inner` edits layer `Inner` inside smart object `Outer`. Fails if no layer matches. |
| `--list-text` | off | Print the type layers of each input (index, name, text) instead of compiling. |
| `-f, --fonts <DIR>` | none | Font folder searched first. Repeatable; earlier folders win. |
| `--font-map <FROM=TO>` | none | Draw font `FROM` (the PostScript name in the PSD) with font `TO`; repeatable. Fails if `TO` isn't found. |
| `--no-system-fonts` | off | Skip the user and system font folders. Useful for reproducible builds. |
| `--keep-text` | off | Don't re-render type layers. Use the pixels Photoshop cached instead. |
| `--render-smart-objects` | off | Re-render smart objects from their embedded PSD/PSB, PNG or JPEG through the placement's perspective and warp. Smart objects edited with `--set-text` are always re-rendered. |
| `--text-masks <DIR>` | none | Also write each type layer's coverage as a grayscale PNG, `DIR/STEM.textNNN.png`. `NNN` is the layer's index, bottom to top. |
| `-c, --compression <0-9>` | `2` | PNG and TIFF deflate level. `0` is fastest; `9` gives the smallest files. On a 1284×1826 page, level 2 finishes about 2× faster end to end than level 9, with files about 7% larger. |
| `-j, --jobs <N>` | all cores | Size of the worker thread pool. |
| `--timings` | off | Print the parse, render and write time of every file. |
| `-q, --quiet` | off | Print only errors (no progress, no warnings). |
| `-h, --help` | | Show help. |
| `-V, --version` | | Show version. |

## Environment

| Variable | Description |
|---|---|
| `PSDC_FONTS` | Extra font folders, separated by `:` (`;` on Windows). Searched after `--fonts`. |
| `XDG_CACHE_HOME` | Where the font index cache lives (`$XDG_CACHE_HOME/psd-compiler/fonts.tsv`). Without it, `%LOCALAPPDATA%` (Windows) or `~/.cache` is used. |

## Font resolution

Fonts are matched by the PostScript name stored in the PSD, such as `Montserrat-Black`. Matching ignores case, spaces and punctuation, and falls back to the full name and then a family prefix. Folders are searched in this order:

1. every `--fonts DIR`, in the order given
2. every folder in `PSDC_FONTS`
3. `./fonts`, if it exists
4. the platform's font folders:
   - Linux: `~/.fonts`, `~/.local/share/fonts`, `$XDG_DATA_HOME/fonts`, `/usr/share/fonts`, `/usr/local/share/fonts`
   - macOS: `~/Library/Fonts`, `/Library/Fonts`, `/System/Library/Fonts`
   - Windows: `%LOCALAPPDATA%\Microsoft\Windows\Fonts`, `%WINDIR%\Fonts`

The first face found for a name wins. If a font is missing, the closest weight and slant of the same family is used, or else of a common sans, serif or monospace family chosen from the name. Synthetic bold and italic make up for a lighter or upright substitute, and a warning names the face used. Characters the face lacks fall back to a face that has the glyph (DejaVu Sans, Noto Sans, Liberation Sans or Arial when available). `--font-map` overrides all of this.

## Output

- Images are 8-bit. PNG, WebP and TIFF are written as **RGB** when every pixel is opaque, and as **RGBA** otherwise; JPEG is flattened onto `--background`.
- Colors are composited in the document's color space and converted to sRGB.
- PSD output copies the input and replaces only what changed: edited type layers and smart objects get new type data and pixels, embedded files are rewritten, and the merged image is re-rendered. Without edits the copy is byte-identical. Supported for 8- and 16-bit RGB and grayscale documents. `psdc` refuses to overwrite the input.

## Exit status

| Code | Meaning |
|---|---|
| `0` | Every input compiled. Warnings don't count as failures. |
| `1` | At least one input failed: unreadable, malformed, or output not writable. |
| `2` | Invalid command line. |

## Examples

```sh
# One file, output next to it
psdc page.psd

# Explicit output and a project font folder
psdc page.psd -o build/page.png -f ./fonts

# Whole chapter into a folder, maximum compression
psdc chapter/*.psd -o rendered/ -c 9

# Reproducible: only fonts that ship with the project
psdc page.psd --no-system-fonts -f vendor/fonts

# Translation workflow: render plus a mask of every text layer
psdc page.psd --text-masks masks/

# Translate and keep the PSD editable
psdc es.psd --set-text 'Title=Hello\nworld' --set-text 'Card/Name=Ana' -o en.psd

# A missing comic font drawn with another one, as a JPEG
psdc page.psd --font-map CCWildWords-Roman=Anton-Regular -o page.jpg -Q 85

# Compare against Photoshop's cached text
psdc page.psd --keep-text -o page.cached.png

# Profile a batch
psdc chapter/*.psd -o out/ --timings
```
