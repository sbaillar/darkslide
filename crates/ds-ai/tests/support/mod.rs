//! Dev-only helpers shared by integration tests and benches: pull the
//! embedded JPEG preview out of a Sony ARW (a TIFF container) and decode it to
//! luma, plus a synthetic stand-in scene for machines without samples.
#![allow(dead_code)]

use std::path::Path;

use ds_ai::LumaImage;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

fn rd16(b: &[u8], o: usize, le: bool) -> Option<u32> {
    let s: [u8; 2] = b.get(o..o + 2)?.try_into().ok()?;
    Some(u32::from(if le {
        u16::from_le_bytes(s)
    } else {
        u16::from_be_bytes(s)
    }))
}

fn rd32(b: &[u8], o: usize, le: bool) -> Option<u32> {
    let s: [u8; 4] = b.get(o..o + 4)?.try_into().ok()?;
    Some(if le {
        u32::from_le_bytes(s)
    } else {
        u32::from_be_bytes(s)
    })
}

/// Largest JPEG referenced by `JPEGInterchangeFormat`(0x201)/`Length`(0x202)
/// in the IFD chain. For the A9 that is the 1616×1080 preview in IFD0.
pub fn embedded_jpeg(path: &Path) -> Option<Vec<u8>> {
    let b = std::fs::read(path).ok()?;
    let le = match b.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let mut ifd = rd32(&b, 4, le)? as usize;
    let mut best: Option<(usize, usize)> = None;
    for _ in 0..8 {
        if ifd == 0 {
            break;
        }
        let n = rd16(&b, ifd, le)? as usize;
        let (mut off, mut len) = (None, None);
        for e in 0..n {
            let at = ifd + 2 + e * 12;
            match rd16(&b, at, le)? {
                0x201 => off = rd32(&b, at + 8, le),
                0x202 => len = rd32(&b, at + 8, le),
                _ => {}
            }
        }
        if let (Some(o), Some(l)) = (off, len)
            && best.is_none_or(|(_, bl)| l as usize > bl)
        {
            best = Some((o as usize, l as usize));
        }
        ifd = rd32(&b, ifd + 2 + n * 12, le)? as usize;
    }
    let (o, l) = best?;
    b.get(o..o + l).map(<[u8]>::to_vec)
}

pub fn decode_luma(jpeg: &[u8]) -> Option<LumaImage> {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Luma);
    let mut d = JpegDecoder::new_with_options(ZCursor::new(jpeg), opts);
    let px = d.decode().ok()?;
    let info = d.info()?;
    LumaImage::from_gray8(usize::from(info.width), usize::from(info.height), px).ok()
}

/// Luma of the embedded preview of every sample ARW (empty without samples).
pub fn sample_previews() -> Vec<(String, LumaImage)> {
    ds_testdata::samples_with_ext("arw")
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            Some((name, decode_luma(&embedded_jpeg(&p)?)?))
        })
        .collect()
}

/// Deterministic textured scene (edges, gradients, fine detail, mild noise),
/// a stand-in for a real preview in benches and tests.
pub fn synthetic_scene(w: usize, h: usize, seed: u64) -> LumaImage {
    let mut s = seed | 1;
    let data = (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let noise = (s >> 60) as f32 - 7.5;
            let grad = 40.0 + 120.0 * x / w as f32;
            let bars = if ((x / 7.0) as i32 + (y / 11.0) as i32) % 2 == 0 {
                50.0
            } else {
                0.0
            };
            let ring = ((x - w as f32 / 2.0).hypot(y - h as f32 / 2.0) / 5.0).sin() * 25.0;
            (grad + bars + ring + noise).clamp(0.0, 255.0) as u8
        })
        .collect();
    LumaImage::from_gray8(w, h, data).unwrap()
}

/// Separable box blur of radius `r` (horizontal and/or vertical).
pub fn box_blur(img: &LumaImage, rx: usize, ry: usize) -> LumaImage {
    let (w, h) = (img.width(), img.height());
    let src: Vec<f32> = img.pixels().iter().map(|&p| f32::from(p)).collect();
    let pass = |src: &[f32], horizontal: bool, r: usize| -> Vec<f32> {
        if r == 0 {
            return src.to_vec();
        }
        let mut out = vec![0.0; w * h];
        for y in 0..h {
            for x in 0..w {
                let (mut acc, mut n) = (0.0, 0.0);
                for d in -(r as isize)..=r as isize {
                    let (xx, yy) = if horizontal {
                        (x as isize + d, y as isize)
                    } else {
                        (x as isize, y as isize + d)
                    };
                    if xx >= 0 && yy >= 0 && (xx as usize) < w && (yy as usize) < h {
                        acc += src[yy as usize * w + xx as usize];
                        n += 1.0;
                    }
                }
                out[y * w + x] = acc / n;
            }
        }
        out
    };
    let out = pass(&pass(&src, true, rx), false, ry);
    LumaImage::from_gray8(w, h, out.iter().map(|&v| v.round() as u8).collect()).unwrap()
}
