use std::path::Path;

use rayon::prelude::*;

use crate::png::{self, ColorType};

/// Default PNG compression level: fast, with files close to level 6.
pub const DEFAULT_COMPRESSION: u8 = 2;

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

    #[test]
    fn opaque_images_encode_as_rgb() {
        let img = Image { width: 1, height: 1, data: vec![1, 2, 3, 255] };
        assert!(img.is_opaque());
        assert_eq!(img.encode_png(DEFAULT_COMPRESSION)[25], 2);
    }
}
