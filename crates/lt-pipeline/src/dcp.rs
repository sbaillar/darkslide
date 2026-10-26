//! Adobe camera profiles (`.dcp`): a TIFF-structured file (magic `IIRC`/`MMCR`) whose
//! first IFD carries the DNG colour tags. Parsed here into plain data; how a profile is
//! applied lives in [`crate::profile`].
//!
//! Supported tags (DNG 1.4 profiles; the DNG 1.6 third illuminant is ignored):
//! `UniqueCameraModel`, `ProfileName`, `CalibrationIlluminant1/2`, `ColorMatrix1/2`,
//! `ForwardMatrix1/2`, `ProfileHueSatMapDims` + `ProfileHueSatMapData1/2` +
//! `ProfileHueSatMapEncoding`, `ProfileLookTableDims` + `ProfileLookTableData` +
//! `ProfileLookTableEncoding`, `ProfileToneCurve` and `BaselineExposureOffset`. Files
//! over [`MAX_FILE_BYTES`] and tables over [`MAX_TABLE`] entries are refused.

use crate::color::{self, Mat3, mat_inv, mat_vec};
use crate::profile::{HsvMap, blend3};

const UNIQUE_CAMERA_MODEL: u16 = 50708;
const COLOR_MATRIX: [u16; 2] = [50721, 50722];
const CALIBRATION_ILLUMINANT: [u16; 2] = [50778, 50779];
const PROFILE_NAME: u16 = 50936;
const HUE_SAT_DIMS: u16 = 50937;
const HUE_SAT_DATA: [u16; 2] = [50938, 50939];
const TONE_CURVE: u16 = 50940;
const FORWARD_MATRIX: [u16; 2] = [50964, 50965];
const LOOK_DIMS: u16 = 50981;
const LOOK_DATA: u16 = 50982;
const HUE_SAT_ENCODING: u16 = 51107;
const LOOK_ENCODING: u16 = 51108;
const BASELINE_EXPOSURE_OFFSET: u16 = 51109;

/// Largest HSV table accepted (entries): Adobe's are at most 90 × 30 × 32.
pub const MAX_TABLE: usize = 90 * 30 * 32;

/// Largest `.dcp` file accepted (bytes). Adobe's are well under 2 MB; two maximal
/// tables of `f32` triples are ~2 MB.
pub const MAX_FILE_BYTES: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DcpError {
    #[error("not a DNG camera profile (bad header)")]
    NotDcp,
    #[error("truncated or malformed profile: {0}")]
    Malformed(&'static str),
    #[error("profile has no colour matrix")]
    NoMatrix,
    #[error("file too large for a camera profile ({0} bytes)")]
    TooLarge(u64),
}

/// A parsed camera profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Dcp {
    pub name: String,
    pub camera: Option<String>,
    /// EXIF light-source codes of the two calibrations (0 = unknown).
    pub illuminants: [u16; 2],
    /// XYZ → camera, per calibration (the second may be absent).
    pub color_matrix: [Option<Mat3>; 2],
    /// White-balanced camera → XYZ D50, per calibration.
    pub forward_matrix: [Option<Mat3>; 2],
    /// Hue/saturation/value maps per calibration (applied before exposure).
    pub hue_sat: [Option<HsvMap>; 2],
    /// The look table (applied after exposure).
    pub look: Option<HsvMap>,
    /// Linear → linear tone curve points.
    pub tone_curve: Option<Vec<[f32; 2]>>,
    /// Stops added to the baseline exposure.
    pub baseline_offset: f32,
}

/// Correlated colour temperature (K) of an EXIF light source code, as the DNG SDK maps
/// them; unknown codes read as D50-ish daylight.
pub fn illuminant_temp(code: u16) -> f32 {
    match code {
        17 | 3 => 2856.0, // Standard light A, tungsten
        16 => 2940.0,     // Warm white fluorescent
        24 => 3200.0,     // ISO studio tungsten
        15 => 3450.0,     // White fluorescent
        2 | 14 => 4150.0, // Fluorescent, cool white fluorescent
        18 => 4874.0,     // Standard light B
        13 => 5000.0,     // Day white fluorescent
        23 => 5003.0,     // D50
        20 => 5503.0,     // D55
        12 => 6430.0,     // Daylight fluorescent
        10 => 6500.0,     // Cloudy
        21 => 6504.0,     // D65
        19 => 6774.0,     // Standard light C
        11 => 7500.0,     // Shade
        22 => 7504.0,     // D75
        _ => 5500.0,      // Daylight, flash, fine weather, unknown
    }
}

#[derive(Clone, Copy)]
struct Reader<'a> {
    b: &'a [u8],
    le: bool,
}

impl Reader<'_> {
    fn bytes<const N: usize>(&self, at: usize) -> Result<[u8; N], DcpError> {
        self.b
            .get(at..at.checked_add(N).ok_or(DcpError::Malformed("offset"))?)
            .and_then(|s| s.try_into().ok())
            .ok_or(DcpError::Malformed("offset past end"))
    }
    fn u16(&self, at: usize) -> Result<u16, DcpError> {
        let v = self.bytes::<2>(at)?;
        Ok(if self.le {
            u16::from_le_bytes(v)
        } else {
            u16::from_be_bytes(v)
        })
    }
    fn u32(&self, at: usize) -> Result<u32, DcpError> {
        let v = self.bytes::<4>(at)?;
        Ok(if self.le {
            u32::from_le_bytes(v)
        } else {
            u32::from_be_bytes(v)
        })
    }
}

/// One IFD entry: tag, type, count and where its values start.
#[derive(Debug, Clone, Copy)]
struct Entry {
    typ: u16,
    count: usize,
    at: usize,
}

fn type_size(typ: u16) -> Option<usize> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

impl Entry {
    /// Numeric values as f64 (integers, rationals, floats).
    fn numbers(&self, r: Reader<'_>) -> Result<Vec<f64>, DcpError> {
        let size = type_size(self.typ).ok_or(DcpError::Malformed("tag type"))?;
        (0..self.count)
            .map(|i| {
                let at = self.at + i * size;
                Ok(match self.typ {
                    1 | 7 => f64::from(r.bytes::<1>(at)?[0]),
                    6 => f64::from(r.bytes::<1>(at)?[0] as i8),
                    3 => f64::from(r.u16(at)?),
                    8 => f64::from(r.u16(at)? as i16),
                    4 => f64::from(r.u32(at)?),
                    9 => f64::from(r.u32(at)? as i32),
                    11 => f64::from(f32::from_bits(r.u32(at)?)),
                    5 | 10 => {
                        let (n, d) = (r.u32(at)?, r.u32(at + 4)?);
                        let (n, d) = if self.typ == 10 {
                            (f64::from(n as i32), f64::from(d as i32))
                        } else {
                            (f64::from(n), f64::from(d))
                        };
                        if d == 0.0 { 0.0 } else { n / d }
                    }
                    12 => {
                        let hi = u64::from(r.u32(at)?);
                        let lo = u64::from(r.u32(at + 4)?);
                        f64::from_bits(if r.le {
                            (lo << 32) | hi
                        } else {
                            (hi << 32) | lo
                        })
                    }
                    _ => return Err(DcpError::Malformed("tag type")),
                })
            })
            .collect()
    }

    fn text(&self, r: Reader<'_>) -> Result<String, DcpError> {
        let s =
            r.b.get(self.at..self.at + self.count)
                .ok_or(DcpError::Malformed("string past end"))?;
        let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        Ok(String::from_utf8_lossy(&s[..end]).trim().to_owned())
    }
}

impl Dcp {
    /// Parse a `.dcp` file's bytes.
    pub fn parse(b: &[u8]) -> Result<Self, DcpError> {
        if b.len() as u64 > MAX_FILE_BYTES {
            return Err(DcpError::TooLarge(b.len() as u64));
        }
        let le = match b.get(..4) {
            Some(b"IIRC") => true,
            Some(b"MMCR") => false,
            _ => return Err(DcpError::NotDcp),
        };
        let r = Reader { b, le };
        let ifd = r.u32(4)? as usize;
        let n = usize::from(r.u16(ifd)?);
        let mut tags = std::collections::BTreeMap::new();
        for i in 0..n {
            let e = ifd + 2 + 12 * i;
            let (tag, typ, count) = (r.u16(e)?, r.u16(e + 2)?, r.u32(e + 4)? as usize);
            let Some(size) = type_size(typ) else {
                continue; // unknown types are skipped, as TIFF readers do
            };
            let len = size
                .checked_mul(count)
                .ok_or(DcpError::Malformed("tag size"))?;
            let at = if len <= 4 {
                e + 8
            } else {
                r.u32(e + 8)? as usize
            };
            if at.checked_add(len).is_none_or(|end| end > b.len()) {
                return Err(DcpError::Malformed("tag data past end"));
            }
            tags.insert(tag, Entry { typ, count, at });
        }
        let nums = |t: u16| tags.get(&t).map(|e| e.numbers(r)).transpose();
        let matrix = |t: u16| -> Result<Option<Mat3>, DcpError> {
            match nums(t)? {
                None => Ok(None),
                Some(v) if v.len() == 9 => Ok(Some(
                    [0, 1, 2].map(|i| [0, 1, 2].map(|j| v[i * 3 + j] as f32)),
                )),
                Some(_) => Err(DcpError::Malformed("matrix is not 3×3")),
            }
        };
        let dims = |t: u16| -> Result<Option<[usize; 3]>, DcpError> {
            match nums(t)? {
                None => Ok(None),
                Some(v) if v.len() == 2 || v.len() == 3 => {
                    let d = [v[0], v[1], v.get(2).copied().unwrap_or(1.0)];
                    if d.iter().any(|x| *x < 1.0 || *x > 4096.0) {
                        return Err(DcpError::Malformed("table dimensions"));
                    }
                    Ok(Some(d.map(|x| x as usize)))
                }
                Some(_) => Err(DcpError::Malformed("table dimensions")),
            }
        };
        let table = |dims: Option<[usize; 3]>, t: u16, enc: u16| {
            let (Some(d), Some(data)) = (dims, nums(t)?) else {
                return Ok(None);
            };
            if d[0] * d[1] * d[2] > MAX_TABLE {
                return Err(DcpError::Malformed("table too large"));
            }
            let srgb = nums(enc)?.and_then(|v| v.first().copied()) == Some(1.0);
            let entries: Vec<[f32; 3]> = data
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| c.map(|v| v as f32))
                .collect();
            HsvMap::new(d, entries, srgb)
                .map(Some)
                .ok_or(DcpError::Malformed(
                    "table size does not match its dimensions",
                ))
        };
        let hs_dims = dims(HUE_SAT_DIMS)?;
        let look_dims = dims(LOOK_DIMS)?;
        let color_matrix = [matrix(COLOR_MATRIX[0])?, matrix(COLOR_MATRIX[1])?];
        if color_matrix[0].is_none() {
            return Err(DcpError::NoMatrix);
        }
        let tone_curve = match nums(TONE_CURVE)? {
            None => None,
            Some(v) if v.len() >= 4 && v.len() % 2 == 0 => Some(
                v.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| p.map(|x| x as f32))
                    .collect(),
            ),
            Some(_) => return Err(DcpError::Malformed("tone curve")),
        };
        let illum = |t: u16| -> Result<u16, DcpError> {
            Ok(nums(t)?.and_then(|v| v.first().copied()).unwrap_or(0.0) as u16)
        };
        let text = |t: u16| tags.get(&t).map(|e| e.text(r)).transpose();
        Ok(Self {
            name: text(PROFILE_NAME)?
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Camera Profile".into()),
            camera: text(UNIQUE_CAMERA_MODEL)?,
            illuminants: [
                illum(CALIBRATION_ILLUMINANT[0])?,
                illum(CALIBRATION_ILLUMINANT[1])?,
            ],
            color_matrix,
            forward_matrix: [matrix(FORWARD_MATRIX[0])?, matrix(FORWARD_MATRIX[1])?],
            hue_sat: [
                table(hs_dims, HUE_SAT_DATA[0], HUE_SAT_ENCODING)?,
                table(hs_dims, HUE_SAT_DATA[1], HUE_SAT_ENCODING)?,
            ],
            look: table(look_dims, LOOK_DATA, LOOK_ENCODING)?,
            tone_curve,
            baseline_offset: nums(BASELINE_EXPOSURE_OFFSET)?
                .and_then(|v| v.first().copied())
                .filter(|v| v.is_finite())
                .unwrap_or(0.0)
                .clamp(-5.0, 5.0) as f32,
        })
    }

    /// Weight of calibration 1 for a white balance of `temp_k` (DNG: linear in inverse
    /// temperature between the two calibrations' temperatures, clamped).
    pub fn weight1(&self, temp_k: f32) -> f32 {
        if self.color_matrix[1].is_none() {
            return 1.0;
        }
        let (t1, t2) = (
            illuminant_temp(self.illuminants[0]),
            illuminant_temp(self.illuminants[1]),
        );
        if (t1 - t2).abs() < 1.0 {
            return 1.0;
        }
        let inv = 1.0 / temp_k.max(1000.0);
        ((inv - 1.0 / t2) / (1.0 / t1 - 1.0 / t2)).clamp(0.0, 1.0)
    }

    /// Weight of calibration 1 for the white balance `mults` (camera multipliers,
    /// G = 1), found the way the DNG SDK does: the camera neutral `1 ⊘ mults` is taken
    /// to XYZ through the profile's own colour matrices blended at the current weight,
    /// its temperature gives the next weight, until it settles (starting from D50).
    pub fn weight_for_neutral(&self, mults: [f32; 3]) -> f32 {
        let Some(cm1) = &self.color_matrix[0] else {
            return 1.0;
        };
        if self.color_matrix[1].is_none() {
            return 1.0;
        }
        let neutral = mults.map(|m| {
            if m > 0.0 && m.is_finite() {
                1.0 / m
            } else {
                1.0
            }
        });
        let mut w = self.weight1(5003.0);
        for _ in 0..30 {
            let Some(to_xyz) = mat_inv(&blend3(cm1, self.color_matrix[1].as_ref(), w)) else {
                break;
            };
            let xyz = mat_vec(&to_xyz, neutral);
            let sum = xyz[0] + xyz[1] + xyz[2];
            if !(sum.is_finite() && sum > 1e-9) {
                break;
            }
            let xy = [f64::from(xyz[0] / sum), f64::from(xyz[1] / sum)];
            let next = self.weight1(color::xy_to_temp_tint(xy).0);
            let done = (next - w).abs() < 1e-5;
            w = next;
            if done {
                break;
            }
        }
        w
    }
}

/// Writes synthetic `.dcp` files for tests (no Adobe files in the repository).
#[doc(hidden)]
pub mod synth {
    /// A tag value to write.
    #[derive(Debug, Clone)]
    pub enum Val {
        Short(Vec<u16>),
        Long(Vec<u32>),
        SRational(Vec<f32>),
        Float(Vec<f32>),
        Ascii(String),
    }

    /// A complete two-illuminant profile named `name` for the A9: colour and forward
    /// matrices, a 2.5-D hue/sat map per calibration (different ones), a 3-D look table
    /// indexed on sRGB-encoded value and an S-shaped tone curve.
    pub fn example(name: &str) -> Vec<u8> {
        let hsm = |k: f32| -> Vec<f32> {
            (0..18 * 6)
                .flat_map(|i| {
                    let (h, s) = ((i / 6) as f32, (i % 6) as f32 / 5.0);
                    [
                        k * s * (h * 0.7).sin() * 8.0,
                        1.0 + 0.15 * s * (h * 0.35).cos(),
                        1.0 - 0.05 * s,
                    ]
                })
                .collect()
        };
        let look: Vec<f32> = (0..5 * 12 * 4)
            .flat_map(|i| {
                let (v, h, s) = (
                    (i / 48) as f32 / 4.0,
                    ((i / 4) % 12) as f32,
                    (i % 4) as f32 / 3.0,
                );
                [
                    s * 6.0 * (h * 0.5).cos(),
                    1.0 + 0.1 * s * (1.0 - v),
                    1.0 + 0.04 * (v - 0.5) * s,
                ]
            })
            .collect();
        let cm_a = [0.7, -0.15, -0.05, -0.45, 1.25, 0.2, -0.1, 0.2, 0.75];
        let cm_d = [0.75, -0.2, -0.05, -0.4, 1.2, 0.2, -0.05, 0.15, 0.6];
        let fm_a = [0.62, 0.28, 0.0842, 0.27, 0.78, -0.05, 0.01, 0.04, 0.7752];
        let fm_d = [0.6, 0.3, 0.0642, 0.25, 0.8, -0.05, 0.0, 0.05, 0.7752];
        let tone: Vec<f32> = (0..=32)
            .flat_map(|i| {
                let x = i as f32 / 32.0;
                [x, x * x * (3.0 - 2.0 * x) * 0.5 + x * 0.5]
            })
            .collect();
        build(
            &[
                (50708, Val::Ascii("Sony ILCE-9".into())),
                (50936, Val::Ascii(name.into())),
                (50778, Val::Short(vec![17])),
                (50779, Val::Short(vec![21])),
                (50721, Val::SRational(cm_a.to_vec())),
                (50722, Val::SRational(cm_d.to_vec())),
                (50964, Val::SRational(fm_a.to_vec())),
                (50965, Val::SRational(fm_d.to_vec())),
                (50937, Val::Long(vec![18, 6, 1])),
                (50938, Val::Float(hsm(1.0))),
                (50939, Val::Float(hsm(-0.5))),
                (50981, Val::Long(vec![12, 4, 5])),
                (50982, Val::Float(look)),
                (51108, Val::Long(vec![1])),
                (50940, Val::Float(tone)),
            ],
            false,
        )
    }

    /// A profile file with `tags` (little-endian unless `big_endian`).
    pub fn build(tags: &[(u16, Val)], big_endian: bool) -> Vec<u8> {
        let w16 = |v: u16| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let w32 = |v: u32| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut out = Vec::new();
        out.extend_from_slice(if big_endian { b"MMCR" } else { b"IIRC" });
        out.extend_from_slice(&w32(8));
        let mut tags = tags.to_vec();
        tags.sort_by_key(|t| t.0);
        let ifd_len = 2 + 12 * tags.len() + 4;
        let mut data: Vec<u8> = Vec::new();
        let mut entries = Vec::new();
        for (tag, v) in &tags {
            let (typ, count, bytes): (u16, usize, Vec<u8>) = match v {
                Val::Short(s) => (3, s.len(), s.iter().flat_map(|x| w16(*x)).collect()),
                Val::Long(s) => (4, s.len(), s.iter().flat_map(|x| w32(*x)).collect()),
                Val::Float(s) => (
                    11,
                    s.len(),
                    s.iter().flat_map(|x| w32(x.to_bits())).collect(),
                ),
                Val::SRational(s) => (
                    10,
                    s.len(),
                    s.iter()
                        .flat_map(|x| {
                            let n = (x * 10_000.0).round() as i32;
                            [w32(n as u32), w32(10_000)].concat()
                        })
                        .collect(),
                ),
                Val::Ascii(s) => {
                    let mut b = s.as_bytes().to_vec();
                    b.push(0);
                    (2, b.len(), b)
                }
            };
            let mut field = [0u8; 4];
            if bytes.len() <= 4 {
                field[..bytes.len()].copy_from_slice(&bytes);
            } else {
                let off = (8 + ifd_len + data.len()) as u32;
                field = w32(off);
                data.extend_from_slice(&bytes);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
            }
            entries.push((*tag, typ, count as u32, field));
        }
        out.extend_from_slice(&w16(entries.len() as u16));
        for (tag, typ, count, field) in entries {
            out.extend_from_slice(&w16(tag));
            out.extend_from_slice(&w16(typ));
            out.extend_from_slice(&w32(count));
            out.extend_from_slice(&field);
        }
        out.extend_from_slice(&w32(0));
        out.extend_from_slice(&data);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::synth::{Val, build};
    use super::*;

    const CM: [f32; 9] = [0.7, -0.2, -0.05, -0.4, 1.2, 0.2, -0.05, 0.15, 0.6];

    fn full(big: bool) -> Vec<u8> {
        let hs: Vec<f32> = (0..4 * 3)
            .flat_map(|i| [i as f32, 1.0 + 0.01 * i as f32, 1.0])
            .collect();
        let look: Vec<f32> = (0..2 * 2 * 2).flat_map(|_| [5.0, 1.1, 0.9]).collect();
        build(
            &[
                (UNIQUE_CAMERA_MODEL, Val::Ascii("Sony ILCE-9".into())),
                (PROFILE_NAME, Val::Ascii("Test Standard".into())),
                (CALIBRATION_ILLUMINANT[0], Val::Short(vec![17])),
                (CALIBRATION_ILLUMINANT[1], Val::Short(vec![21])),
                (COLOR_MATRIX[0], Val::SRational(CM.to_vec())),
                (
                    COLOR_MATRIX[1],
                    Val::SRational(CM.map(|v| v * 0.9).to_vec()),
                ),
                (
                    FORWARD_MATRIX[0],
                    Val::SRational(vec![0.6, 0.3, 0.06, 0.25, 0.8, -0.05, 0.0, 0.05, 0.77]),
                ),
                (HUE_SAT_DIMS, Val::Long(vec![4, 3, 1])),
                (HUE_SAT_DATA[0], Val::Float(hs.clone())),
                (HUE_SAT_DATA[1], Val::Float(hs)),
                (LOOK_DIMS, Val::Long(vec![2, 2, 2])),
                (LOOK_DATA, Val::Float(look)),
                (LOOK_ENCODING, Val::Long(vec![1])),
                (TONE_CURVE, Val::Float(vec![0.0, 0.0, 0.5, 0.6, 1.0, 1.0])),
                (BASELINE_EXPOSURE_OFFSET, Val::SRational(vec![0.25])),
                (50941, Val::Long(vec![0])), // embed policy: ignored
            ],
            big,
        )
    }

    #[test]
    fn parses_every_supported_tag_in_both_byte_orders() {
        for big in [false, true] {
            let d = Dcp::parse(&full(big)).unwrap();
            assert_eq!(d.name, "Test Standard");
            assert_eq!(d.camera.as_deref(), Some("Sony ILCE-9"));
            assert_eq!(d.illuminants, [17, 21]);
            let cm = d.color_matrix[0].unwrap();
            assert!((cm[0][0] - 0.7).abs() < 1e-4 && (cm[2][2] - 0.6).abs() < 1e-4);
            assert!((d.color_matrix[1].unwrap()[0][0] - 0.63).abs() < 1e-4);
            assert!(d.forward_matrix[0].is_some() && d.forward_matrix[1].is_none());
            let hs = d.hue_sat[0].as_ref().unwrap();
            assert_eq!(hs.dims(), [4, 3, 1]);
            assert!(!hs.srgb_value());
            assert_eq!(d.hue_sat[0], d.hue_sat[1]);
            let lk = d.look.as_ref().unwrap();
            assert_eq!(lk.dims(), [2, 2, 2]);
            assert!(lk.srgb_value());
            assert_eq!(d.tone_curve.as_ref().unwrap().len(), 3);
            assert!((d.baseline_offset - 0.25).abs() < 1e-4);
        }
    }

    #[test]
    fn example_profile_parses() {
        let d = Dcp::parse(&super::synth::example("Example")).unwrap();
        assert_eq!(d.name, "Example");
        assert_eq!(d.look.as_ref().unwrap().dims(), [12, 4, 5]);
        assert_ne!(d.hue_sat[0], d.hue_sat[1]);
        assert!(d.forward_matrix.iter().all(Option::is_some));
        assert_eq!(d.tone_curve.as_ref().unwrap().len(), 33);
    }

    #[test]
    fn minimal_profile_and_defaults() {
        let b = build(&[(COLOR_MATRIX[0], Val::SRational(CM.to_vec()))], false);
        let d = Dcp::parse(&b).unwrap();
        assert_eq!(d.name, "Camera Profile");
        assert_eq!(d.camera, None);
        assert_eq!(d.illuminants, [0, 0]);
        assert!(d.hue_sat.iter().all(Option::is_none) && d.look.is_none());
        assert_eq!(d.tone_curve, None);
        assert_eq!(d.baseline_offset, 0.0);
        // One calibration: always weight 1.
        assert_eq!(d.weight1(3000.0), 1.0);
    }

    #[test]
    fn weight_interpolates_in_inverse_temperature() {
        let d = Dcp::parse(&full(false)).unwrap();
        assert_eq!(d.weight1(2000.0), 1.0);
        assert_eq!(d.weight1(2856.0), 1.0);
        assert!(d.weight1(6504.0).abs() < 1e-6);
        assert_eq!(d.weight1(10_000.0), 0.0);
        // Mired midpoint of A and D65 weighs one half.
        let mid = 2.0 / (1.0 / 2856.0 + 1.0 / 6504.0);
        assert!((d.weight1(mid) - 0.5).abs() < 1e-3);
        // Same temperature twice: the first calibration.
        let mut same = d.clone();
        same.illuminants = [21, 21];
        assert_eq!(same.weight1(4000.0), 1.0);
    }

    /// Camera multipliers (G = 1) that neutralize the white `temp_k` under `d`'s own
    /// colour matrices blended at `w`.
    fn mults_for(d: &Dcp, temp_k: f32, w: f32) -> [f32; 3] {
        let [x, y] = color::temp_tint_to_xy(temp_k, 0.0);
        let xyz = [(x / y) as f32, 1.0, ((1.0 - x - y) / y) as f32];
        let cm = blend3(
            d.color_matrix[0].as_ref().unwrap(),
            d.color_matrix[1].as_ref(),
            w,
        );
        let n = mat_vec(&cm, xyz);
        [n[1] / n[0], 1.0, n[1] / n[2]]
    }

    #[test]
    fn weight_follows_the_profiles_own_matrices() {
        let mut d = Dcp::parse(&full(false)).unwrap();
        // Distinct calibrations so the neutral depends on the weight.
        d.color_matrix[1] = Some([[0.8, -0.25, -0.02], [-0.35, 1.15, 0.22], [-0.02, 0.1, 0.5]]);
        for t in [2856.0, 3500.0, 4500.0, 5500.0, 6504.0, 9000.0] {
            let want = d.weight1(t);
            let got = d.weight_for_neutral(mults_for(&d, t, want));
            assert!((got - want).abs() < 2e-3, "{t} K: {got} vs {want}");
        }
        // One calibration, or a neutral that can't be placed: the first.
        let mut one = d.clone();
        one.color_matrix[1] = None;
        assert_eq!(one.weight_for_neutral([2.0, 1.0, 1.5]), 1.0);
        let mut flat = d.clone();
        flat.color_matrix = [Some([[0.0; 3]; 3]), Some([[0.0; 3]; 3])];
        assert_eq!(
            flat.weight_for_neutral([2.0, 1.0, 1.5]),
            flat.weight1(5003.0)
        );
        let mut neg = d.clone();
        neg.color_matrix = [Some([[-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, -1.0]]); 2];
        assert_eq!(neg.weight_for_neutral([2.0, 1.0, 1.5]), neg.weight1(5003.0));
        // Bad multipliers read as 1.
        assert!((0.0..=1.0).contains(&d.weight_for_neutral([0.0, 1.0, f32::NAN])));
    }

    #[test]
    fn size_limits() {
        assert!(matches!(
            Dcp::parse(&vec![0u8; MAX_FILE_BYTES as usize + 1]),
            Err(DcpError::TooLarge(_))
        ));
        let cm = (COLOR_MATRIX[0], Val::SRational(CM.to_vec()));
        let table = |d: [u32; 3]| {
            let n = (d[0] * d[1] * d[2]) as usize;
            build(
                &[
                    cm.clone(),
                    (HUE_SAT_DIMS, Val::Long(d.to_vec())),
                    (HUE_SAT_DATA[0], Val::Float([0.0, 1.0, 1.0].repeat(n))),
                ],
                false,
            )
        };
        assert!(Dcp::parse(&table([90, 30, 32])).is_ok(), "Adobe's largest");
        assert_eq!(
            Dcp::parse(&table([90, 30, 33])),
            Err(DcpError::Malformed("table too large"))
        );
    }

    #[test]
    fn illuminant_codes() {
        assert_eq!(illuminant_temp(17), 2856.0);
        assert_eq!(illuminant_temp(21), 6504.0);
        assert_eq!(illuminant_temp(23), 5003.0);
        assert_eq!(illuminant_temp(16), 2940.0);
        assert_eq!(illuminant_temp(255), 5500.0);
        for c in [2, 10, 11, 12, 13, 14, 15, 18, 19, 20, 22, 24] {
            assert!((2000.0..8000.0).contains(&illuminant_temp(c)));
        }
    }

    #[test]
    fn rejects_bad_files() {
        assert_eq!(Dcp::parse(b"II*\0\x08\0\0\0"), Err(DcpError::NotDcp));
        assert_eq!(Dcp::parse(b""), Err(DcpError::NotDcp));
        assert!(matches!(
            Dcp::parse(b"IIRC\xff\0\0\0"),
            Err(DcpError::Malformed(_))
        ));
        // No colour matrix.
        let b = build(&[(PROFILE_NAME, Val::Ascii("x".into()))], false);
        assert_eq!(Dcp::parse(&b), Err(DcpError::NoMatrix));
        // A matrix of the wrong size.
        let b = build(&[(COLOR_MATRIX[0], Val::SRational(vec![1.0; 6]))], false);
        assert!(matches!(Dcp::parse(&b), Err(DcpError::Malformed(_))));
        // Table data not matching its dimensions.
        let cm = (COLOR_MATRIX[0], Val::SRational(CM.to_vec()));
        let b = build(
            &[
                cm.clone(),
                (HUE_SAT_DIMS, Val::Long(vec![4, 3, 1])),
                (HUE_SAT_DATA[0], Val::Float(vec![0.0; 9])),
            ],
            false,
        );
        assert!(matches!(Dcp::parse(&b), Err(DcpError::Malformed(_))));
        // Silly dimensions.
        let b = build(
            &[cm.clone(), (HUE_SAT_DIMS, Val::Long(vec![0, 3, 1]))],
            false,
        );
        assert!(matches!(Dcp::parse(&b), Err(DcpError::Malformed(_))));
        let b = build(&[cm.clone(), (HUE_SAT_DIMS, Val::Long(vec![4]))], false);
        assert!(matches!(Dcp::parse(&b), Err(DcpError::Malformed(_))));
        let b = build(
            &[
                cm.clone(),
                (HUE_SAT_DIMS, Val::Long(vec![4000, 4000, 4000])),
            ],
            false,
        );
        let big = build(
            &[
                cm.clone(),
                (HUE_SAT_DIMS, Val::Long(vec![4000, 4000, 4000])),
                (HUE_SAT_DATA[0], Val::Float(vec![0.0; 3])),
            ],
            false,
        );
        assert!(Dcp::parse(&b).is_ok(), "dims without data are ignored");
        assert!(matches!(Dcp::parse(&big), Err(DcpError::Malformed(_))));
        // Odd tone curve.
        let b = build(&[cm, (TONE_CURVE, Val::Float(vec![0.0, 0.0, 1.0]))], false);
        assert!(matches!(Dcp::parse(&b), Err(DcpError::Malformed(_))));
        // Data offsets past the end.
        let mut b = full(false);
        b.truncate(b.len() - 10);
        assert!(matches!(Dcp::parse(&b), Err(DcpError::Malformed(_))));
    }

    #[test]
    fn reads_every_numeric_type() {
        // Byte, signed byte, signed short, signed long and double values.
        let r = Reader {
            b: &[
                7, 0xfe, 0xff, 0xff, 0xfd, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f,
            ],
            le: true,
        };
        let e = |typ, count, at| Entry { typ, count, at };
        assert_eq!(e(1, 1, 0).numbers(r).unwrap(), [7.0]);
        assert_eq!(e(6, 1, 1).numbers(r).unwrap(), [-2.0]);
        assert_eq!(e(8, 1, 1).numbers(r).unwrap(), [-2.0]);
        assert_eq!(e(9, 1, 4).numbers(r).unwrap(), [-3.0]);
        assert_eq!(e(12, 1, 8).numbers(r).unwrap(), [1.5]);
        assert_eq!(e(4, 1, 4).numbers(r).unwrap(), [4_294_967_293.0]);
        let rb = Reader {
            b: &[0x3f, 0xf8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0],
            le: false,
        };
        assert_eq!(e(12, 1, 0).numbers(rb).unwrap(), [1.5]);
        // Unsigned rational with a zero denominator reads 0.
        assert_eq!(e(5, 1, 8).numbers(rb).unwrap(), [0.0]);
        assert!(e(13, 1, 0).numbers(r).is_err());
        assert!(e(4, 1, 14).numbers(r).is_err());
        assert!(e(2, 40, 0).text(r).is_err());
    }
}
