use std::path::Path;

use rayon::prelude::*;

use crate::png::{self, ColorType};

/// Default PNG compression level: fast, with files close to level 6.
pub const DEFAULT_COMPRESSION: u8 = 2;

/// Default JPEG and AVIF quality.
pub const DEFAULT_QUALITY: u8 = 90;

/// Output file format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// PNG, lossless with alpha.
    Png,
    /// Baseline JPEG; transparency is flattened onto [`EncodeOptions::background`].
    Jpeg,
    /// Lossless WebP with alpha (at most 16383 pixels per side).
    WebP,
    /// TIFF with Deflate compression and unassociated alpha.
    Tiff,
    /// AVIF with alpha; needs the `avif` cargo feature.
    Avif,
}

impl Format {
    /// The format named by a file extension (`png`, `jpg`/`jpeg`, `webp`, `tif`/`tiff`, `avif`).
    pub fn from_extension(ext: &str) -> Option<Format> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "png" => Format::Png,
            "jpg" | "jpeg" => Format::Jpeg,
            "webp" => Format::WebP,
            "tif" | "tiff" => Format::Tiff,
            "avif" => Format::Avif,
            _ => return None,
        })
    }

    /// The format of `path`, from its extension.
    pub fn from_path(path: impl AsRef<Path>) -> Option<Format> {
        path.as_ref().extension().and_then(|e| e.to_str()).and_then(Format::from_extension)
    }

    /// Usual file extension.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::WebP => "webp",
            Format::Tiff => "tif",
            Format::Avif => "avif",
        }
    }
}

/// Encoder settings; each format uses the ones that apply to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeOptions {
    /// PNG and TIFF compression level, 0 (fastest) to 9 (smallest).
    pub compression: u8,
    /// JPEG and AVIF quality, 1 to 100.
    pub quality: u8,
    /// Color that transparent pixels are flattened onto for JPEG.
    pub background: [u8; 3],
}

impl Default for EncodeOptions {
    fn default() -> Self {
        EncodeOptions { compression: DEFAULT_COMPRESSION, quality: DEFAULT_QUALITY, background: [255; 3] }
    }
}

/// 8-bit RGBA image with straight (non-premultiplied) alpha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row-major RGBA bytes, `width * height * 4` long.
    pub data: Vec<u8>,
}

fn quantize(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

impl Image {
    pub(crate) fn from_premultiplied(width: u32, height: u32, px: &[f32]) -> Image {
        let mut data = vec![0u8; px.len()];
        let stride = (width as usize * 4).max(4);
        data.par_chunks_mut(stride).zip(px.par_chunks(stride)).for_each(|(dst, src)| {
            for (d, p) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                let a = p[3];
                if a > 0.0 {
                    d.copy_from_slice(&[quantize(p[0] / a), quantize(p[1] / a), quantize(p[2] / a), quantize(a)]);
                }
            }
        });
        Image { width, height, data }
    }

    /// RGBA of the pixel at `(x, y)`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]]
    }

    /// Whether every pixel is fully opaque.
    pub fn is_opaque(&self) -> bool {
        self.data.par_chunks(4 * 4096).all(|c| c.chunks_exact(4).all(|p| p[3] == 255))
    }

    /// Encodes as PNG with compression `level` (0-9). Opaque images are written as RGB.
    pub fn encode_png(&self, level: u8) -> Vec<u8> {
        if self.is_opaque() {
            let rgb: Vec<u8> = self.data.par_chunks(4).flat_map_iter(|p| [p[0], p[1], p[2]]).collect();
            png::encode(self.width, self.height, ColorType::Rgb, &rgb, level)
        } else {
            png::encode(self.width, self.height, ColorType::Rgba, &self.data, level)
        }
    }

    /// Writes a PNG file with compression `level` (0-9).
    pub fn save_png(&self, path: impl AsRef<Path>, level: u8) -> std::io::Result<()> {
        std::fs::write(path, self.encode_png(level))
    }

    /// RGB bytes with alpha flattened onto `bg`.
    fn flatten(&self, bg: [u8; 3]) -> Vec<u8> {
        self.data
            .par_chunks(4)
            .flat_map_iter(|p| {
                let a = p[3] as u32;
                [0, 1, 2].map(|c| ((p[c] as u32 * a + bg[c] as u32 * (255 - a) + 127) / 255) as u8)
            })
            .collect()
    }

    fn rgb_or_rgba(&self) -> (Vec<u8>, bool) {
        if self.is_opaque() {
            (self.data.par_chunks(4).flat_map_iter(|p| [p[0], p[1], p[2]]).collect(), false)
        } else {
            (self.data.clone(), true)
        }
    }

    /// Encodes in `format`.
    pub fn encode(&self, format: Format, options: &EncodeOptions) -> std::io::Result<Vec<u8>> {
        let (w, h) = (self.width, self.height);
        let quality = options.quality.clamp(1, 100);
        match format {
            Format::Png => Ok(self.encode_png(options.compression)),
            Format::Jpeg => {
                if w > u16::MAX as u32 || h > u16::MAX as u32 {
                    return Err(std::io::Error::other("JPEG images are limited to 65535 pixels per side"));
                }
                let mut out = vec![];
                jpeg_encoder::Encoder::new(&mut out, quality)
                    .encode(&self.flatten(options.background), w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
                    .map_err(std::io::Error::other)?;
                Ok(out)
            }
            Format::WebP => {
                if w > 16383 || h > 16383 {
                    return Err(std::io::Error::other("WebP images are limited to 16383 pixels per side"));
                }
                let (data, alpha) = self.rgb_or_rgba();
                let color = if alpha { image_webp::ColorType::Rgba8 } else { image_webp::ColorType::Rgb8 };
                let mut out = vec![];
                image_webp::WebPEncoder::new(&mut out).encode(&data, w, h, color).map_err(std::io::Error::other)?;
                Ok(out)
            }
            Format::Tiff => {
                let (data, alpha) = self.rgb_or_rgba();
                crate::tiff::encode(w, h, if alpha { 4 } else { 3 }, &data, options.compression)
            }
            #[cfg(feature = "avif")]
            Format::Avif => {
                let px: Vec<ravif::RGBA8> =
                    self.data.chunks_exact(4).map(|p| ravif::RGBA8::new(p[0], p[1], p[2], p[3])).collect();
                let img = ravif::Img::new(px.as_slice(), w as usize, h as usize);
                let encoded = ravif::Encoder::new()
                    .with_quality(quality as f32)
                    .with_speed(6)
                    .with_alpha_color_mode(ravif::AlphaColorMode::UnassociatedClean)
                    .encode_rgba(img)
                    .map_err(std::io::Error::other)?;
                Ok(encoded.avif_file)
            }
            #[cfg(not(feature = "avif"))]
            Format::Avif => Err(std::io::Error::other("AVIF output needs psd-compiler built with the `avif` feature")),
        }
    }

    /// Writes `path` in the format named by its extension (PNG when it has none).
    pub fn save(&self, path: impl AsRef<Path>, options: &EncodeOptions) -> std::io::Result<()> {
        let path = path.as_ref();
        let format = match path.extension() {
            None => Format::Png,
            Some(_) => Format::from_path(path).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown image format: {}", path.display()),
                )
            })?,
        };
        std::fs::write(path, self.encode(format, options)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremultiplies_and_quantizes() {
        let img = Image::from_premultiplied(2, 1, &[0.25, 0.0, 0.5, 0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(img.pixel(0, 0), [128, 0, 255, 128]);
        assert_eq!(img.pixel(1, 0), [0, 0, 0, 0]);
        assert!(!img.is_opaque());
    }

    fn checker() -> Image {
        let data = (0..64 * 48)
            .flat_map(|i| if (i % 64 / 8 + i / 64 / 8) % 2 == 0 { [255, 0, 0, 255] } else { [0, 0, 255, 128] })
            .collect();
        Image { width: 64, height: 48, data }
    }

    #[test]
    fn encodes_every_format() {
        let img = checker();
        let o = EncodeOptions::default();
        assert!(img.encode(Format::Jpeg, &o).unwrap().starts_with(&[0xFF, 0xD8]));
        let webp = img.encode(Format::WebP, &o).unwrap();
        assert!(webp.starts_with(b"RIFF") && &webp[8..12] == b"WEBP");
        let tiff = img.encode(Format::Tiff, &o).unwrap();
        assert!(tiff.starts_with(b"MM\0\x2a"));
        assert_eq!(Format::from_extension("JPEG"), Some(Format::Jpeg));
        assert_eq!(Format::from_path("a/b.tif"), Some(Format::Tiff));
        assert_eq!(Format::from_path("a/b.psd"), None);
    }

    #[test]
    fn opaque_images_encode_as_rgb() {
        let img = Image { width: 1, height: 1, data: vec![1, 2, 3, 255] };
        assert!(img.is_opaque());
        assert_eq!(img.encode_png(DEFAULT_COMPRESSION)[25], 2);
    }
}
