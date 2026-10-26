//! In-memory image buffers handed to the exporter by the injected renderer.

use lt_core::Size;
use rayon::prelude::*;

use crate::ExportError;

/// Interleaved 16-bit RGB, row-major, output-encoded (gamma) in the recipe's space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image16 {
    pub size: Size,
    pub data: Vec<u16>,
}

/// What a renderer may return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendered {
    /// Interleaved RGB, 3 × u16 per pixel.
    Rgb16 { size: Size, data: Vec<u16> },
    /// Interleaved RGBA, 4 × u8 per pixel (alpha ignored).
    Rgba8 { size: Size, data: Vec<u8> },
}

#[inline]
fn to8(v: u16) -> u8 {
    ((u32::from(v) * 255 + 32_767) / 65_535) as u8
}

fn expected(size: Size, channels: u64) -> Result<usize, ExportError> {
    u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .and_then(|p| p.checked_mul(channels))
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n > 0)
        .ok_or_else(|| ExportError::InvalidImage(format!("bad size {size:?}")))
}

impl Image16 {
    pub fn new(size: Size, data: Vec<u16>) -> Result<Self, ExportError> {
        let n = expected(size, 3)?;
        if data.len() != n {
            return Err(ExportError::InvalidImage(format!(
                "rgb16 buffer has {} samples, {size:?} needs {n}",
                data.len()
            )));
        }
        Ok(Self { size, data })
    }

    /// Validate a renderer result and convert 8-bit input losslessly (v × 257).
    pub fn from_rendered(r: Rendered) -> Result<Self, ExportError> {
        match r {
            Rendered::Rgb16 { size, data } => Self::new(size, data),
            Rendered::Rgba8 { size, data } => {
                let n = expected(size, 4)?;
                if data.len() != n {
                    return Err(ExportError::InvalidImage(format!(
                        "rgba8 buffer has {} bytes, {size:?} needs {n}",
                        data.len()
                    )));
                }
                let mut out = vec![0u16; n / 4 * 3];
                out.par_chunks_mut(3 * 4096)
                    .zip(data.par_chunks(4 * 4096))
                    .for_each(|(o, i)| {
                        for (o, i) in o
                            .as_chunks_mut::<3>()
                            .0
                            .iter_mut()
                            .zip(i.as_chunks::<4>().0)
                        {
                            o[0] = u16::from(i[0]) * 257;
                            o[1] = u16::from(i[1]) * 257;
                            o[2] = u16::from(i[2]) * 257;
                        }
                    });
                Ok(Self { size, data: out })
            }
        }
    }

    /// Rows `rows` as 8-bit RGB into `out` (resized to fit): lets encoders
    /// stream in strips instead of holding a full 8-bit copy.
    pub fn rgb8_rows(&self, rows: std::ops::Range<usize>, out: &mut Vec<u8>) {
        let stride = self.size.width as usize * 3;
        let src = &self.data[rows.start * stride..rows.end * stride];
        out.resize(src.len(), 0);
        for (o, &v) in out.iter_mut().zip(src) {
            *o = to8(v);
        }
    }

    /// Rows `rows` as big-endian 16-bit bytes (PNG sample order) into `out`.
    pub fn be16_rows(&self, rows: std::ops::Range<usize>, out: &mut Vec<u8>) {
        let stride = self.size.width as usize * 3;
        out.clear();
        out.extend(
            self.data[rows.start * stride..rows.end * stride]
                .iter()
                .flat_map(|v| v.to_be_bytes()),
        );
    }

    /// Round to 8-bit RGB (exact inverse of × 257 for 8-bit sources).
    pub fn to_rgb8(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.data.len()];
        out.par_chunks_mut(1 << 16)
            .zip(self.data.par_chunks(1 << 16))
            .for_each(|(o, i)| {
                for (o, &v) in o.iter_mut().zip(i) {
                    *o = to8(v);
                }
            });
        out
    }
}

/// What an encoder reads, in row strips: a 16-bit image, or an 8-bit RGBA render
/// (alpha ignored) that JPEG and PNG8 encode directly, without a 16-bit copy.
#[derive(Debug, Clone, Copy)]
pub enum Pixels<'a> {
    Rgb16(&'a Image16),
    Rgba8 { size: Size, data: &'a [u8] },
}

impl<'a> Pixels<'a> {
    /// An 8-bit RGBA render, validated like [`Image16::from_rendered`].
    pub fn rgba8(size: Size, data: &'a [u8]) -> Result<Self, ExportError> {
        let n = expected(size, 4)?;
        if data.len() != n {
            return Err(ExportError::InvalidImage(format!(
                "rgba8 buffer has {} bytes, {size:?} needs {n}",
                data.len()
            )));
        }
        Ok(Self::Rgba8 { size, data })
    }

    pub fn size(&self) -> Size {
        match self {
            Self::Rgb16(img) => img.size,
            Self::Rgba8 { size, .. } => *size,
        }
    }

    /// Rows `rows` as 8-bit RGB into `out` (see [`Image16::rgb8_rows`]).
    pub fn rgb8_rows(&self, rows: std::ops::Range<usize>, out: &mut Vec<u8>) {
        match self {
            Self::Rgb16(img) => img.rgb8_rows(rows, out),
            Self::Rgba8 { size, data } => {
                let stride = size.width as usize * 4;
                let src = &data[rows.start * stride..rows.end * stride];
                out.resize(src.len() / 4 * 3, 0);
                for (o, p) in out
                    .as_chunks_mut::<3>()
                    .0
                    .iter_mut()
                    .zip(src.as_chunks::<4>().0)
                {
                    *o = [p[0], p[1], p[2]];
                }
            }
        }
    }

    /// As a 16-bit image: borrowed, or converted from 8 bits (v × 257).
    pub fn to_image16(&self) -> Result<std::borrow::Cow<'a, Image16>, ExportError> {
        match *self {
            Self::Rgb16(img) => Ok(std::borrow::Cow::Borrowed(img)),
            Self::Rgba8 { size, data } => Image16::from_rendered(Rendered::Rgba8 {
                size,
                data: data.to_vec(),
            })
            .map(std::borrow::Cow::Owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba8_pixels_stream_rgb_rows_and_convert() {
        let size = Size::new(2, 2);
        let data: Vec<u8> = (0..16).map(|i| i as u8 * 10).collect();
        let px = Pixels::rgba8(size, &data).unwrap();
        assert_eq!(px.size(), size);
        let mut rows = Vec::new();
        px.rgb8_rows(1..2, &mut rows);
        assert_eq!(rows, [80, 90, 100, 120, 130, 140]);
        let img = px.to_image16().unwrap();
        assert_eq!(img.data[3], 40 * 257);
        let mut again = Vec::new();
        Pixels::Rgb16(&img).rgb8_rows(1..2, &mut again);
        assert_eq!(
            again, rows,
            "16-bit rows of an 8-bit render are the same bytes"
        );
        assert!(Pixels::rgba8(size, &data[..15]).is_err());
        assert!(Pixels::rgba8(Size::new(0, 2), &[]).is_err());
    }

    #[test]
    fn rgba8_roundtrips_through_16_bit() {
        let size = Size::new(3, 2);
        let data: Vec<u8> = (0..24).map(|i| (i * 11) as u8).collect();
        let img = Image16::from_rendered(Rendered::Rgba8 {
            size,
            data: data.clone(),
        })
        .unwrap();
        assert_eq!(img.data.len(), 18);
        assert_eq!(img.data[0], 0);
        assert_eq!(img.data[3], u16::from(data[4]) * 257);
        let back = img.to_rgb8();
        let expect: Vec<u8> = data.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        assert_eq!(back, expect);
    }

    #[test]
    fn strip_converters_match_whole_image() {
        let data: Vec<u16> = (0..4 * 3 * 3).map(|i| (i * 1999) as u16).collect();
        let img = Image16::new(Size::new(4, 3), data.clone()).unwrap();
        let mut strip = Vec::new();
        img.rgb8_rows(1..3, &mut strip);
        assert_eq!(strip, img.to_rgb8()[12..]);
        img.be16_rows(0..1, &mut strip);
        assert_eq!(strip.len(), 24);
        assert_eq!(u16::from_be_bytes([strip[2], strip[3]]), data[1]);
    }

    #[test]
    fn to_rgb8_rounds() {
        let img = Image16::new(Size::new(1, 1), vec![0, 65_535, 32_896]).unwrap();
        assert_eq!(img.to_rgb8(), vec![0, 255, 128]);
    }

    #[test]
    fn length_and_size_checks() {
        let e = Image16::new(Size::new(2, 2), vec![0; 11]).unwrap_err();
        assert!(e.to_string().contains("needs 12"), "{e}");
        assert!(Image16::new(Size::new(0, 2), vec![]).is_err());
        let r = Rendered::Rgba8 {
            size: Size::new(2, 2),
            data: vec![0; 15],
        };
        assert!(Image16::from_rendered(r).is_err());
        let r = Rendered::Rgba8 {
            size: Size::new(u32::MAX, u32::MAX),
            data: vec![],
        };
        assert!(Image16::from_rendered(r).is_err());
        let r = Rendered::Rgb16 {
            size: Size::new(1, 1),
            data: vec![1, 2, 3],
        };
        assert_eq!(Image16::from_rendered(r).unwrap().data, vec![1, 2, 3]);
    }
}
