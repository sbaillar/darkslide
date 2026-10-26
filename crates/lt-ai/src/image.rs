//! The one input type every analysis takes: an 8-bit luma plane.
//!
//! Callers hand in whatever they have (the embedded JPEG preview decoded to
//! RGBA, a gray thumbnail, a float render) and convert once. Analysis never
//! allocates per pixel and never sees NaNs: the float constructor sanitizes.

use crate::AiError;

/// Row-major 8-bit luma, display-referred (sRGB-encoded, as in a JPEG preview).
#[derive(Clone, PartialEq, Eq)]
pub struct LumaImage {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl std::fmt::Debug for LumaImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LumaImage({}x{})", self.width, self.height)
    }
}

fn check_len(width: usize, height: usize, channels: usize, len: usize) -> Result<usize, AiError> {
    let px = width.checked_mul(height).ok_or(AiError::BadBuffer {
        width,
        height,
        channels,
        len,
    })?;
    if px.checked_mul(channels) != Some(len) {
        return Err(AiError::BadBuffer {
            width,
            height,
            channels,
            len,
        });
    }
    Ok(px)
}

impl LumaImage {
    /// Wrap an existing 8-bit luma plane (`len == width * height`).
    pub fn from_gray8(width: usize, height: usize, data: Vec<u8>) -> Result<Self, AiError> {
        check_len(width, height, 1, data.len())?;
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// Convert interleaved RGBA8 (alpha ignored) to luma with Rec.709 weights
    /// in 8.8 fixed point (54/183/19, sums to 256).
    pub fn from_rgba8(width: usize, height: usize, rgba: &[u8]) -> Result<Self, AiError> {
        Self::from_interleaved::<4>(width, height, rgba)
    }

    /// Convert interleaved RGB8 to luma (same weights as [`Self::from_rgba8`]).
    pub fn from_rgb8(width: usize, height: usize, rgb: &[u8]) -> Result<Self, AiError> {
        Self::from_interleaved::<3>(width, height, rgb)
    }

    fn from_interleaved<const N: usize>(
        width: usize,
        height: usize,
        src: &[u8],
    ) -> Result<Self, AiError> {
        check_len(width, height, N, src.len())?;
        // Fixed-size chunks let LLVM vectorize the weighted sum.
        let data = src
            .as_chunks::<N>()
            .0
            .iter()
            .map(|p| {
                let y = 54 * u32::from(p[0]) + 183 * u32::from(p[1]) + 19 * u32::from(p[2]) + 128;
                (y >> 8) as u8
            })
            .collect();
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// From float luma in `0..=1`. Values are clamped; NaN becomes 0.
    pub fn from_f32(width: usize, height: usize, luma: &[f32]) -> Result<Self, AiError> {
        let px = check_len(width, height, 1, luma.len())?;
        let mut data = Vec::with_capacity(px);
        // `f32 as u8` saturates and maps NaN to 0, which is exactly the policy.
        data.extend(luma.iter().map(|&v| (v * 255.0 + 0.5) as u8));
        Ok(Self {
            width,
            height,
            data,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.data
    }

    /// True for a zero-area image; analyses return neutral results for it.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(crate) fn row(&self, y: usize) -> &[u8] {
        &self.data[y * self.width..(y + 1) * self.width]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_weights_map_primaries_and_extremes() {
        let px = [
            255, 255, 255, 0, // white
            0, 0, 0, 255, // black
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
        ];
        let img = LumaImage::from_rgba8(5, 1, &px).unwrap();
        assert_eq!(img.pixels(), &[255, 0, 54, 182, 19]);
        let rgb = LumaImage::from_rgb8(1, 1, &[255, 255, 255]).unwrap();
        assert_eq!(rgb.pixels(), &[255]);
    }

    #[test]
    fn float_input_is_sanitized() {
        let img = LumaImage::from_f32(5, 1, &[f32::NAN, -1.0, 2.0, 0.5, f32::INFINITY]).unwrap();
        assert_eq!(img.pixels(), &[0, 0, 255, 128, 255]);
    }

    #[test]
    fn bad_lengths_are_rejected() {
        assert!(matches!(
            LumaImage::from_gray8(4, 4, vec![0; 15]),
            Err(AiError::BadBuffer {
                width: 4,
                height: 4,
                channels: 1,
                len: 15
            })
        ));
        assert!(LumaImage::from_rgba8(2, 2, &[0; 15]).is_err());
        assert!(LumaImage::from_f32(usize::MAX, 2, &[]).is_err());
        let empty = LumaImage::from_gray8(0, 7, Vec::new()).unwrap();
        assert!(empty.is_empty());
        assert_eq!((empty.width(), empty.height()), (0, 7));
    }
}
