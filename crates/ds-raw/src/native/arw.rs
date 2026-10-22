//! Sony ARW beyond the uncompressed fast path (`crate::decode::sony_plan`):
//!
//! - compressed ARW 2 ("cRAW", TIFF compression 32767, one byte per pixel):
//!   each row is a run of 16-byte little-endian blocks, each holding 16
//!   same-colour pixels (even or odd columns of 32): an 11-bit maximum and
//!   minimum, the 4-bit positions of both, and 14 7-bit offsets from the
//!   minimum, scaled up by the smallest shift that covers the block's range.
//!   The 11-bit values `v` go through the `SonyToneCurve` (0x7010): linear
//!   from 0 to 4095 over `k = 2v` with slopes 1, 2, 4, 8 and 16 between the
//!   breakpoints `0x7010[i] / 4`. Layout and curve were worked out from the
//!   corpus files against rawler's output (docs/RAWDEC.md).
//! - lossless compressed ARW (TIFF compression 7): lossless JPEG tiles,
//!   decoded like DNG tiles.
//!
//! Levels and white balance come from the raw IFD and the SR2 block
//! ([`crate::sony::levels`]), the crop from `DefaultCrop*`, the colour
//! matrix from the camera table.

use ds_core::CfaPattern;
use rayon::prelude::*;

use super::{Blocks, Sensor, ljpeg, ljpeg_block, orientation};
use crate::RawError;
use crate::container::{Container, crop_rect};
use crate::decode::cfa_from_tag;
use crate::sony;
use crate::source::Source;
use crate::tiff::{Ifd, tag};

/// TIFF compression of cRAW.
const COMPRESSION_CRAW: u32 = 32767;
const COMPRESSION_LJPEG: u32 = 7;
/// `SonyToneCurve`, in the raw IFD.
const TONE_CURVE: u16 = 0x7010;
/// Rows per parallel task (~200 KB of cRAW data on a 33 MP sensor).
const BAND_ROWS: usize = 32;

/// The 11-bit cRAW value → output lookup built from `SonyToneCurve`.
pub(crate) fn curve(points: [u32; 4]) -> Result<Vec<u16>, RawError> {
    let mut knots = [0u32, 0, 0, 0, 0, 4095];
    for (k, p) in knots[1..5].iter_mut().zip(points) {
        *k = (p >> 2) & 0xFFF;
    }
    if knots.windows(2).any(|w| w[0] > w[1]) {
        return Err(RawError::Malformed(format!(
            "SonyToneCurve {points:?} is not increasing"
        )));
    }
    // y over k = 0..=4095, slope 2^i between knots i and i+1.
    let mut y = vec![0u32; 4096];
    for (i, w) in knots.windows(2).enumerate() {
        for k in w[0] + 1..=w[1] {
            y[k as usize] = y[k as usize - 1] + (1 << i);
        }
    }
    Ok((0..2048).map(|v| y[v * 2] as u16).collect())
}

/// Decode one cRAW row (`out.len()` pixels, a multiple of 32).
fn craw_row(src: &[u8], out: &mut [u16], lut: &[u16]) {
    for (blk, px) in src
        .as_chunks::<32>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<32>().0)
    {
        for (par, b) in blk.as_chunks::<16>().0.iter().enumerate() {
            let v = u128::from_le_bytes(*b);
            let max = (v & 0x7FF) as u32;
            let min = ((v >> 11) & 0x7FF) as u32;
            let imax = ((v >> 22) & 0xF) as usize;
            let imin = ((v >> 26) & 0xF) as usize;
            let mut sh = 0;
            while sh < 4 && 0x80 << sh <= max.saturating_sub(min) {
                sh += 1;
            }
            let mut bits = v >> 30;
            for i in 0..16 {
                let val = if i == imax {
                    max
                } else if i == imin {
                    min
                } else {
                    let d = (bits & 0x7F) as u32;
                    bits >>= 7;
                    ((d << sh) + min).min(0x7FF)
                };
                px[i * 2 + par] = lut[val as usize];
            }
        }
    }
}

/// Black levels in colour order R, G1, G2, B → by position in the 2×2 repeat.
fn positional(black: [u16; 4], cfa: CfaPattern) -> [f32; 4] {
    let mut out = [0.0; 4];
    for (slot, o) in out.iter_mut().enumerate() {
        let (x, y) = ((slot % 2) as u32, (slot / 2) as u32);
        let idx = match cfa.color_at(x, y) {
            Some(0) => 0,
            Some(2) => 3,
            _ if cfa.color_at(0, y) == Some(0) || cfa.color_at(1, y) == Some(0) => 1,
            _ => 2,
        };
        *o = f32::from(black[idx]);
    }
    out
}

/// cRAW strips, decoded in parallel row bands.
fn craw(
    src: &dyn Source,
    raw: &Ifd,
    w: usize,
    h: usize,
    lut: &[u16],
) -> Result<Option<Vec<u16>>, RawError> {
    let (Some(off), Some(count)) = (
        raw.uint(src, tag::STRIP_OFFSETS)?,
        raw.uint(src, tag::STRIP_BYTE_COUNTS)?,
    ) else {
        return Ok(None);
    };
    if !w.is_multiple_of(32) || count as usize != w * h {
        return Ok(None); // another ARW 2 layout
    }
    let off = u64::from(off);
    crate::source::check_range(off, (w * h) as u64, 0, src.end())?;
    let mut data = vec![0u16; w * h];
    data.par_chunks_mut(w * BAND_ROWS)
        .enumerate()
        .try_for_each(|(band, out)| {
            let bytes = src.read(off + (band * BAND_ROWS * w) as u64, out.len() as u64)?;
            for (s, d) in bytes.chunks_exact(w).zip(out.chunks_exact_mut(w)) {
                craw_row(s, d, lut);
            }
            Ok::<(), RawError>(())
        })?;
    Ok(Some(data))
}

/// Lossless compressed ARW: lossless JPEG tiles whose frames hold exactly
/// one tile in raster order (checked on the first tile; anything else is
/// left to the fallback).
fn lossless(src: &dyn Source, raw: &Ifd, w: usize, h: usize) -> Result<Option<Vec<u16>>, RawError> {
    let blocks = Blocks::of(src, raw, w, h)?;
    let (off, len) = blocks.spans[0];
    let first = src.read(off, len)?;
    if ljpeg::Ljpeg::parse(&first)?.frame.samples() != blocks.bw * blocks.bh {
        return Ok(None);
    }
    let mut data = vec![0u16; w * h];
    blocks.decode(src, &mut data, w, 1, ljpeg_block)?;
    Ok(Some(data))
}

/// Decode a compressed ARW; `None` for layouts we don't read.
pub(super) fn decode(src: &dyn Source, c: &Container) -> Result<Option<Sensor>, RawError> {
    let Some(raw) = c.raw_ifd(src)? else {
        return Ok(None);
    };
    let u = |t| raw.uint(src, t);
    let (Some(w), Some(h), Some(cfa)) = (
        u(tag::IMAGE_WIDTH)?,
        u(tag::IMAGE_LENGTH)?,
        raw.uints(src, tag::CFA_PATTERN)?
            .as_deref()
            .and_then(cfa_from_tag),
    ) else {
        return Ok(None);
    };
    let (w, h) = (w as usize, h as usize);
    let data = match u(tag::COMPRESSION)? {
        Some(COMPRESSION_CRAW) => {
            let Some(&[a, b, cc, d, ..]) = raw.uints(src, TONE_CURVE)?.as_deref() else {
                return Ok(None);
            };
            craw(src, raw, w, h, &curve([a, b, cc, d])?)?
        }
        Some(COMPRESSION_LJPEG) => lossless(src, raw, w, h)?,
        _ => None,
    };
    let Some(data) = data else {
        return Ok(None);
    };
    let lv = sony::levels(src, &c.tiff, raw)?;
    let (Some(black), Some(white)) = (lv.black, lv.white) else {
        return Ok(None);
    };
    let wb = lv
        .wb
        .map_or([f32::NAN; 3], |[r, g1, g2, b]| [r, g1.midpoint(g2), b]);
    let [cx, cy, cw, ch] = crop_rect(src, raw, w as u32, h as u32)?.map(|v| v as usize);
    Ok(Some(Sensor {
        make: c.make(src)?,
        model: c.model(src)?,
        width: w,
        height: h,
        cpp: 1,
        data,
        cfa,
        black: positional(black, cfa),
        white: f32::from(white),
        crop: Some([cx, cy, cw, ch]),
        wb,
        matrix: None,
        orientation: orientation(src, c),
    }))
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::super::ljpeg::enc::{Opts, encode as ljpeg_encode};
    use super::super::testfile::{Link, Node, build, sample};
    use super::*;
    use crate::source::SliceSource;
    use crate::tiff::Endian;
    use crate::tiff::build::Val;

    /// The A7 III's `SonyToneCurve`.
    const A7M3_CURVE: [u32; 4] = [8000, 10400, 12900, 14100];

    #[test]
    fn tone_curve_matches_the_corpus_observations() {
        let lut = curve(A7M3_CURVE).unwrap();
        // Values read off rawler's output for the A7 III corpus file (the
        // centre of its dithering), one per curve segment.
        for (v, y) in [
            (0, 0),
            (400, 800),
            (1000, 2000),
            (1100, 2400),
            (1300, 3200),
            (1400, 4000),
            (1700, 7100),
            (1900, 12500),
            (2000, 15700),
        ] {
            assert_eq!(lut[v], y, "v = {v}");
        }
        assert_eq!(lut[2047], 8100 + 569 * 16);
        assert!(curve([9000, 8000, 12900, 14100]).is_err());
        // All zero: one slope-16 segment.
        assert_eq!(curve([0; 4]).unwrap()[10], 16 * 20);
    }

    /// One 16-byte cRAW block (the layout `craw_row` reads).
    fn block(max: u32, min: u32, imax: u32, imin: u32, deltas: &[u32; 14]) -> [u8; 16] {
        let mut v = u128::from(max) | u128::from(min) << 11;
        v |= u128::from(imax) << 22 | u128::from(imin) << 26;
        for (i, &d) in deltas.iter().enumerate() {
            v |= u128::from(d) << (30 + 7 * i);
        }
        v.to_le_bytes()
    }

    /// Two blocks (even and odd columns) for 32 pixels, with the 11-bit
    /// values each pixel should decode to.
    fn pair(seed: u32) -> ([u8; 32], [u32; 32]) {
        let mut bytes = [0u8; 32];
        let mut want = [0u32; 32];
        for par in 0..2u32 {
            // Ranges that need shifts 0..=4, and one value that clamps.
            let min = 100 + seed * 37 % 900;
            let sh = (seed + par) % 5;
            let max = (min + (0x7F << sh)).min(0x7FF);
            let (imax, imin) = ((seed + 3 * par) % 16, (seed + 7 + par) % 16);
            let imin = if imin == imax { (imax + 1) % 16 } else { imin };
            let mut deltas = [0u32; 14];
            let mut k = 0;
            for i in 0..16 {
                let val = if i == imax {
                    max
                } else if i == imin {
                    min
                } else {
                    let d = (i * 9 + seed) % 128;
                    deltas[k] = d;
                    k += 1;
                    ((d << sh) + min).min(0x7FF)
                };
                want[(i * 2 + par) as usize] = val;
            }
            bytes[par as usize * 16..][..16].copy_from_slice(&block(max, min, imax, imin, &deltas));
        }
        (bytes, want)
    }

    #[test]
    fn craw_blocks_decode_every_shift() {
        let lut: Vec<u16> = (0..2048).collect();
        for seed in 0..40 {
            let (bytes, want) = pair(seed);
            let mut out = [0u16; 32];
            craw_row(&bytes, &mut out, &lut);
            let got: Vec<u32> = out.iter().map(|&v| u32::from(v)).collect();
            assert_eq!(got, want, "seed {seed}");
        }
        // A corrupt block (min above max) still decodes, without panicking.
        let b = block(10, 2000, 0, 1, &[127; 14]);
        let mut out = [0u16; 32];
        craw_row(&[b, b].concat(), &mut out, &lut);
        assert_eq!(out[0], 10);
        assert_eq!(out[4], 0x7FF);
    }

    #[test]
    fn positional_black_follows_the_cfa() {
        let b = [1, 2, 3, 4]; // R, G1, G2, B
        assert_eq!(positional(b, CfaPattern::Rggb), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(positional(b, CfaPattern::Gbrg), [3.0, 4.0, 1.0, 2.0]);
        assert_eq!(positional(b, CfaPattern::Bggr), [4.0, 3.0, 2.0, 1.0]);
    }

    /// A Sony file with the raw IFD as a `SubIFD`.
    fn arw(raw: Vec<(u16, Val)>, blocks: Vec<Vec<u8>>, tiled: bool) -> Vec<u8> {
        build(
            Endian::Little,
            &[
                Node::new(
                    Link::Chain,
                    vec![
                        (tag::MAKE, Val::Ascii("SONY")),
                        (tag::MODEL, Val::Ascii("ILCE-7M3")),
                        (tag::ORIENTATION, Val::Short(vec![3])),
                    ],
                ),
                Node::new(Link::Sub(0), raw).with_blocks(blocks, tiled),
            ],
        )
    }

    fn raw_tags(w: u32, h: u32, compression: u16) -> Vec<(u16, Val)> {
        vec![
            (tag::IMAGE_WIDTH, Val::Long(vec![w])),
            (tag::IMAGE_LENGTH, Val::Long(vec![h])),
            (tag::COMPRESSION, Val::Short(vec![compression])),
            (tag::PHOTOMETRIC, Val::Short(vec![32803])),
            (tag::CFA_PATTERN, Val::Byte(vec![0, 1, 1, 2])),
            (
                TONE_CURVE,
                Val::Short(A7M3_CURVE.map(|v| v as u16).to_vec()),
            ),
            (0x7310, Val::Short(vec![512, 510, 514, 516])),
            (0x7313, Val::Short(vec![2048, 1024, 1024, 1536])),
            (tag::WHITE_LEVEL, Val::Short(vec![16383])),
            (tag::DEFAULT_CROP_ORIGIN, Val::Long(vec![2, 1])),
            (tag::DEFAULT_CROP_SIZE, Val::Long(vec![60, 2])),
        ]
    }

    fn sensor(f: &[u8]) -> Option<Sensor> {
        let src = SliceSource::new(f);
        let c = Container::parse(&src).unwrap();
        decode(&src, &c).unwrap()
    }

    #[test]
    fn craw_file_decodes_with_sony_levels() {
        let (w, h) = (64usize, 4usize);
        let mut strip = Vec::new();
        let mut want = Vec::new();
        let lut = curve(A7M3_CURVE).unwrap();
        for y in 0..h {
            for half in 0..w / 32 {
                let (b, v) = pair((y * 2 + half) as u32);
                strip.extend_from_slice(&b);
                want.extend(v.iter().map(|&v| lut[v as usize]));
            }
        }
        let f = arw(raw_tags(64, 4, 32767), vec![strip], false);
        let s = sensor(&f).unwrap();
        assert_eq!((s.width, s.height, s.cpp), (64, 4, 1));
        assert_eq!(s.data, want);
        assert_eq!(s.black, [512.0, 510.0, 514.0, 516.0]);
        assert_eq!(s.white, 16383.0);
        assert_eq!(s.crop, Some([2, 1, 60, 2]));
        assert_eq!(s.wb, [2048.0, 1024.0, 1536.0]);
        assert_eq!(s.orientation, ds_core::Orientation::Rotate180);
        assert!(s.matrix.is_none(), "colour comes from the camera table");
    }

    #[test]
    fn lossless_tiles_decode_when_frames_match_tiles() {
        let (w, h, tw, th) = (40usize, 20usize, 32usize, 16usize);
        let all: Vec<u16> = (0..w * h)
            .map(|i| sample(i % w, i / w, 0, 1 << 14))
            .collect();
        let tiles = |ncomp: usize, fw: usize, fh: usize| -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            for ty in 0..h.div_ceil(th) {
                for tx in 0..w.div_ceil(tw) {
                    let mut t = vec![0u16; tw * th];
                    for y in 0..th.min(h - ty * th) {
                        for x in 0..tw.min(w - tx * tw) {
                            t[y * tw + x] = all[(ty * th + y) * w + tx * tw + x];
                        }
                    }
                    out.push(ljpeg_encode(
                        &t[..fw * ncomp * fh],
                        fw,
                        fh,
                        ncomp,
                        &Opts::default(),
                    ));
                }
            }
            out
        };
        let mut tags = raw_tags(w as u32, h as u32, 7);
        tags.push((tag::TILE_WIDTH, Val::Long(vec![tw as u32])));
        tags.push((tag::TILE_LENGTH, Val::Long(vec![th as u32])));
        let s = sensor(&arw(tags.clone(), tiles(2, tw / 2, th), true)).unwrap();
        assert_eq!(s.data, all);
        // Frames of another size than the tile: not ours to guess.
        assert!(sensor(&arw(tags, tiles(2, tw / 2, th / 2), true)).is_none());
    }

    #[test]
    fn other_layouts_are_left_to_the_fallback() {
        let strip = vec![0u8; 64 * 4];
        let without = |t: u16| -> Vec<(u16, Val)> {
            raw_tags(64, 4, 32767)
                .into_iter()
                .filter(|(k, _)| *k != t)
                .collect()
        };
        for tags in [
            without(TONE_CURVE),
            without(0x7310),           // no black level
            without(tag::CFA_PATTERN), // no CFA
            raw_tags(64, 4, 1),        // uncompressed, but not the fast-path layout
            raw_tags(48, 4, 32767),    // not whole blocks
        ] {
            assert!(sensor(&arw(tags, vec![strip.clone()], false)).is_none());
        }
        // Wrong strip size for one byte per pixel.
        assert!(sensor(&arw(raw_tags(64, 4, 32767), vec![vec![0; 100]], false)).is_none());
        // A decreasing tone curve is damage.
        let mut bad = without(TONE_CURVE);
        bad.push((TONE_CURVE, Val::Short(vec![9000, 8000, 7000, 6000])));
        let f = arw(bad, vec![strip], false);
        let src = SliceSource::new(&f);
        assert!(decode(&src, &Container::parse(&src).unwrap()).is_err());
    }
}
