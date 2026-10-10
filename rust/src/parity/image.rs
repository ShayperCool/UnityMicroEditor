//! PNG screenshots decoded for pixel measurements.
use std::io::Cursor;

/// A decoded PNG: 8- or 16-bit samples, gray or RGB, with or without alpha.
pub(super) struct Image {
    pub width: u32,
    pub height: u32,
    samples: usize,
    wide: bool,
    data: Vec<u8>,
}

impl Image {
    /// Decodes a PNG, refusing one above `max_pixels` before its pixels are allocated.
    pub fn decode(data: &[u8], max_pixels: Option<u64>) -> Result<Self, String> {
        let mut decoder = png::Decoder::new(Cursor::new(data));
        // Palettes, low bit depths and tRNS become plain 8-bit samples; 16 bits stay.
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
        let (width, height) = (reader.info().width, reader.info().height);
        if width == 0 || height == 0 {
            return Err("PNG has no pixels".into());
        }
        if max_pixels.is_some_and(|limit| u64::from(width) * u64::from(height) > limit) {
            return Err(format!(
                "PNG exceeds {} megapixels",
                max_pixels.unwrap_or_default() / 1_000_000
            ));
        }
        let (color, depth) = reader.output_color_type();
        let samples = match color {
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Indexed => return Err("PNG palette was not expanded".into()),
        };
        let wide = match depth {
            png::BitDepth::Eight => false,
            png::BitDepth::Sixteen => true,
            _ => return Err("PNG bit depth was not expanded".into()),
        };
        let size = reader
            .output_buffer_size()
            .ok_or("PNG is too large for this platform")?;
        let mut pixels = vec![0; size];
        reader
            .next_frame(&mut pixels)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            width,
            height,
            samples,
            wide,
            data: pixels,
        })
    }

    /// An opaque or translucent 8-bit RGBA image, row by row.
    #[cfg(test)]
    pub fn from_rgba8(width: u32, height: u32, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), width as usize * height as usize * 4);
        Self {
            width,
            height,
            samples: 4,
            wide: false,
            data,
        }
    }

    /// One pixel as 16-bit red, green, blue and alpha, with the colors multiplied by alpha:
    /// a fully transparent pixel is the same whatever color is stored under it.
    pub fn rgba(&self, x: u32, y: u32) -> [u32; 4] {
        let width = if self.wide { 2 } else { 1 };
        let start = (y as usize * self.width as usize + x as usize) * self.samples * width;
        let sample = |index: usize| -> u32 {
            let at = start + index * width;
            if self.wide {
                u32::from(u16::from_be_bytes([self.data[at], self.data[at + 1]]))
            } else {
                u32::from(self.data[at]) * 0x101
            }
        };
        let [red, green, blue, alpha] = match self.samples {
            1 => [sample(0), sample(0), sample(0), 0xffff],
            2 => [sample(0), sample(0), sample(0), sample(1)],
            3 => [sample(0), sample(1), sample(2), 0xffff],
            _ => [sample(0), sample(1), sample(2), sample(3)],
        };
        let scale = |color: u32| color * alpha / 0xffff;
        [scale(red), scale(green), scale(blue), alpha]
    }
}
