//! Sony ARW specifics: levels/WB (plain raw-IFD tags or the encrypted SR2
//! block), the maker-note IFD, and the enciphered 0x9050 camera-info tag.

use crate::RawError;
use crate::source::{SliceSource, Source};
use crate::tiff::{Endian, Ifd, Tiff, tag};

/// Tags that may appear in the raw IFD (ARW ≥ 2.3.1) or the decrypted SR2 sub-IFD.
mod sr2 {
    pub const BLACK_LEVEL_1: u16 = 0x7300;
    pub const WB_GRBG: u16 = 0x7303;
    pub const BLACK_LEVEL_2: u16 = 0x7310;
    pub const WB_RGGB: u16 = 0x7313;
    pub const WHITE_LEVEL: u16 = 0x787F;
    /// DNG-style `WhiteLevel`, written by newer Sony bodies into the raw IFD.
    pub const DNG_WHITE_LEVEL: u16 = 0xC61D;
    // SR2Private IFD (pointed to by DNGPrivateData).
    pub const SUB_IFD_OFFSET: u16 = 0x7200;
    pub const SUB_IFD_LENGTH: u16 = 0x7201;
    pub const SUB_IFD_KEY: u16 = 0x7221;
}

/// Maker note tag holding enciphered camera info (shutter count, …).
const TAG_9050: u16 = 0x9050;
/// Enciphered shot info (release mode, shot number since power-up, burst index).
const TAG_9400: u16 = 0x9400;
/// `FocusLocation`: frame width, height, focus x, y (older bodies; fixed at the
/// frame center on the A9 regardless of the AF point actually used).
const TAG_FOCUS_LOCATION: u16 = 0x2027;
/// `FocalPlaneAFPoints` (A9 and later): `[?, used, areaW(u16), areaH(u16),
/// then up to 15 × (x u16, y u16)]` in the focal-plane AF grid.
const TAG_FOCAL_PLANE_AF: u16 = 0x202A;
/// Upper bound for the SR2 block; real files use a few KB.
const MAX_SR2_LEN: u64 = 1 << 20;

/// Sony levels and as-shot white balance.
///
/// `black` and `wb` are in sensor *color* order R, G1, G2, B (G1 shares rows
/// with R), independent of where the CFA pattern starts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Levels {
    pub black: Option<[u16; 4]>,
    pub white: Option<u16>,
    pub wb: Option<[f32; 4]>,
}

impl Levels {
    /// Read whichever of black/white/WB `ifd` carries; values already set win.
    fn fill_from(&mut self, src: &dyn Source, ifd: &Ifd) -> Result<(), RawError> {
        if self.black.is_none() {
            let v = match ifd.uints(src, sr2::BLACK_LEVEL_2)? {
                Some(v) => Some(v),
                None => ifd.uints(src, sr2::BLACK_LEVEL_1)?,
            };
            self.black = v.and_then(|v| four(&v)).map(|b| b.map(|x| x as u16));
        }
        if self.white.is_none() {
            let v = match ifd.uint(src, sr2::DNG_WHITE_LEVEL)? {
                Some(v) => Some(v),
                None => ifd.uint(src, sr2::WHITE_LEVEL)?,
            };
            self.white = v.map(|x| x as u16).filter(|&x| x > 0);
        }
        if self.wb.is_none() {
            // Stored as SSHORT; values are positive gains around 1024.
            if let Some(v) = ifd.uints(src, sr2::WB_RGGB)?.and_then(|v| four(&v)) {
                self.wb = Some(v.map(|x| f32::from(x as u16)));
            } else if let Some([g1, r, b, g2]) =
                ifd.uints(src, sr2::WB_GRBG)?.and_then(|v| four(&v))
            {
                self.wb = Some([r, g1, g2, b].map(|x| f32::from(x as u16)));
            }
        }
        Ok(())
    }

    fn complete(&self) -> bool {
        self.black.is_some() && self.white.is_some() && self.wb.is_some()
    }
}

/// Exactly four values, or one value replicated.
fn four(v: &[u32]) -> Option<[u32; 4]> {
    match v {
        [a, b, c, d, ..] => Some([*a, *b, *c, *d]),
        [a] => Some([*a; 4]),
        _ => None,
    }
}

/// Levels from the raw IFD, completed from the encrypted SR2 block when needed.
pub(crate) fn levels(src: &dyn Source, tiff: &Tiff, raw: &Ifd) -> Result<Levels, RawError> {
    let mut lv = Levels::default();
    lv.fill_from(src, raw)?;
    if !lv.complete()
        && let Some(sr2) = sr2_block(src, tiff)?
    {
        let s = SliceSource {
            data: &sr2.data,
            base: sr2.base,
        };
        let ifd = Ifd::parse(&s, sr2.base, Endian::Little)?;
        lv.fill_from(&s, &ifd)?;
    }
    Ok(lv)
}

struct Sr2Block {
    data: Vec<u8>,
    base: u64,
}

/// Locate and decrypt the SR2 sub-IFD via IFD0 `DNGPrivateData`.
fn sr2_block(src: &dyn Source, tiff: &Tiff) -> Result<Option<Sr2Block>, RawError> {
    let ifd0 = tiff.ifd0();
    let Some(e) = ifd0.get(tag::DNG_PRIVATE_DATA) else {
        return Ok(None);
    };
    let b = ifd0.bytes_prefix(src, e, 4)?;
    if b.len() < 4 {
        return Err(RawError::Malformed(
            "DNGPrivateData shorter than 4 bytes".into(),
        ));
    }
    // SR2Private is always little-endian, whatever the container says.
    let private = Ifd::parse(
        src,
        u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        Endian::Little,
    )?;
    let (Some(off), Some(len), Some(key)) = (
        private.uint(src, sr2::SUB_IFD_OFFSET)?,
        private.uint(src, sr2::SUB_IFD_LENGTH)?,
        private
            .get(sr2::SUB_IFD_KEY)
            .map(|e| private.bytes_prefix(src, e, 4))
            .transpose()?,
    ) else {
        return Ok(None);
    };
    if key.len() < 4 {
        return Err(RawError::Malformed("SR2 key shorter than 4 bytes".into()));
    }
    let key = u32::from_le_bytes([key[0], key[1], key[2], key[3]]);
    let len = u64::from(len).min(MAX_SR2_LEN);
    let enc = src.read(u64::from(off), len)?;
    Ok(Some(Sr2Block {
        data: decrypt(&enc, key),
        base: u64::from(off),
    }))
}

/// Sony's SR2 stream cipher (dcraw `sony_decrypt`). XOR with a keyed pad, so
/// it is its own inverse. Operates on whole little-endian 32-bit words;
/// trailing bytes (len % 4) are passed through.
pub(crate) fn decrypt(data: &[u8], key: u32) -> Vec<u8> {
    let mut pad = [0u32; 128];
    let mut k = key;
    for p in pad.iter_mut().take(4) {
        k = k.wrapping_mul(48_828_125).wrapping_add(1);
        *p = k;
    }
    pad[3] = (pad[3] << 1) | ((pad[0] ^ pad[2]) >> 31);
    for p in 4..127 {
        pad[p] = ((pad[p - 4] ^ pad[p - 2]) << 1) | ((pad[p - 3] ^ pad[p - 1]) >> 31);
    }
    for p in pad.iter_mut().take(127) {
        *p = u32::from_be(*p);
    }
    let mut out = data.to_vec();
    for (i, w) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let p = i + 127;
        pad[p & 127] = pad[(p + 1) & 127] ^ pad[(p + 65) & 127];
        *w = (u32::from_le_bytes(*w) ^ pad[p & 127]).to_le_bytes();
    }
    out
}

/// The Sony maker-note IFD. Offsets inside it are absolute file offsets.
/// Older bodies prefix it with a 12-byte `SONY DSC \0\0\0` header.
pub(crate) fn makernote(src: &dyn Source, exif: &Ifd) -> Result<Option<Ifd>, RawError> {
    let Some(e) = exif.get(tag::MAKER_NOTE) else {
        return Ok(None);
    };
    let off = u64::from(exif.endian.u32(e.field));
    let head = src.read(off, 12.min(src.end().saturating_sub(off)))?;
    let off = if head.starts_with(b"SONY") {
        off + 12
    } else {
        off
    };
    Ok(Ifd::parse(src, off, exif.endian).ok())
}

/// Sony substitution cipher for tags 0x9050/0x94xx: `c = b³ mod 249`.
pub(crate) fn decipher(data: &[u8]) -> Vec<u8> {
    const TABLE: [u8; 256] = {
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < 256 {
            if i >= 249 {
                t[i] = i as u8;
            } else {
                t[i * i * i % 249] = i as u8;
            }
            i += 1;
        }
        t
    };
    data.iter().map(|&b| TABLE[usize::from(b)]).collect()
}

/// Bodies using exiftool's `Tag9050b` layout (`ShutterCount` at 0x3A).
const TAG_9050B_MODELS: &[&str] = &[
    "ILCE-9",
    "ILCE-9M2",
    "ILCE-7M3",
    "ILCE-7RM3",
    "ILCE-7RM3A",
    "ILCE-7RM4",
    "ILCE-7RM4A",
    "ILCE-7SM2",
    "ILCE-7RM2",
    "ILCE-6300",
    "ILCE-6400",
    "ILCE-6500",
    "ILCE-6600",
    "ILCE-6100",
];

/// Mechanical shutter actuations (electronic-shutter frames are not counted).
pub(crate) fn shutter_count(
    src: &dyn Source,
    model: &str,
    mn: &Ifd,
) -> Result<Option<u32>, RawError> {
    if !TAG_9050B_MODELS.contains(&model) {
        return Ok(None);
    }
    let Some(e) = mn.get(TAG_9050) else {
        return Ok(None);
    };
    let raw = mn.bytes_prefix(src, e, 0x3E)?;
    let Some(b) = raw.get(0x3A..0x3E) else {
        return Ok(None);
    };
    let d = decipher(b);
    let n = u32::from_le_bytes([d[0], d[1], d[2], d[3]]) & 0x00FF_FFFF;
    Ok((n > 0).then_some(n))
}

/// Where the camera focused, normalized to the stored (sensor-orientation)
/// frame: `(x, y)` in `0..=1`.
///
/// Uses the focal-plane AF points actually used for the shot (tag 0x202A,
/// centroid of the used points) when present. Only bodies without that tag
/// fall back to `FocusLocation` (0x2027); the A9 writes the frame center
/// there whatever the AF mode, so it is ignored when 0x202A exists.
pub(crate) fn af_point(src: &dyn Source, mn: &Ifd) -> Option<(f32, f32)> {
    if let Some(e) = mn.get(TAG_FOCAL_PLANE_AF) {
        let b = mn.bytes_prefix(src, e, 6 + 15 * 4).ok()?;
        let u16_at = |o: usize| b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
        let used = usize::from(*b.get(1)?).min(15);
        let (aw, ah) = (u16_at(2)?, u16_at(4)?);
        if used == 0 || aw == 0 || ah == 0 {
            return None;
        }
        let pts: Vec<(u16, u16)> = (0..used)
            .map_while(|k| Some((u16_at(6 + 4 * k)?, u16_at(8 + 4 * k)?)))
            .collect();
        if pts.is_empty() {
            return None;
        }
        let n = pts.len() as f32;
        let cx = pts.iter().map(|p| f32::from(p.0)).sum::<f32>() / n;
        let cy = pts.iter().map(|p| f32::from(p.1)).sum::<f32>() / n;
        return Some((
            (cx / f32::from(aw)).clamp(0.0, 1.0),
            (cy / f32::from(ah)).clamp(0.0, 1.0),
        ));
    }
    let v = mn.uints(src, TAG_FOCUS_LOCATION).ok()??;
    let [w, h, x, y] = *v.get(..4)? else {
        return None;
    };
    if w == 0 || h == 0 || (x == 0 && y == 0) || x > w || y > h {
        return None;
    }
    Some((x as f32 / w as f32, y as f32 / h as f32))
}

/// Shot numbering from the enciphered `Tag9400` (exiftool `Tag9400c`
/// layout, selected by its first *enciphered* byte): `ShotNumberSincePowerUp`
/// (u32 at 0x0A) and the 1-based `SequenceImageNumber` within a burst
/// (u32 at 0x12, stored 0-based).
pub(crate) fn shot_numbers(src: &dyn Source, mn: &Ifd) -> (Option<u32>, Option<u32>) {
    let Some(e) = mn.get(TAG_9400) else {
        return (None, None);
    };
    let Ok(raw) = mn.bytes_prefix(src, e, 0x16) else {
        return (None, None);
    };
    if raw.len() < 0x16 || !matches!(raw[0], 0x23 | 0x24 | 0x26 | 0x28 | 0x31 | 0x32 | 0x33) {
        return (None, None);
    }
    let d = decipher(&raw);
    let at = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    let shot = Some(at(0x0A)).filter(|&v| v > 0);
    let seq = at(0x12).checked_add(1);
    (shot, seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiff::build::{Val, header, ifd};

    #[test]
    fn decrypt_is_an_involution_and_keeps_tail() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1030).collect();
        let enc = decrypt(&data, 0xDEAD_BEEF);
        assert_ne!(enc[..1028], data[..1028]);
        assert_eq!(enc[1028..], data[1028..]);
        assert_eq!(decrypt(&enc, 0xDEAD_BEEF), data);
        assert_ne!(decrypt(&enc, 1), data);
    }

    #[test]
    fn decipher_inverts_the_cube_cipher() {
        let plain: Vec<u8> = (0..=255u8).collect();
        let enc: Vec<u8> = plain
            .iter()
            .map(|&b| {
                if b >= 249 {
                    b
                } else {
                    ((u32::from(b).pow(3)) % 249) as u8
                }
            })
            .collect();
        // 0/1, 82-84, 165-167, 248 collide in the cipher; everything else round-trips.
        let dec = decipher(&enc);
        for (i, (&p, &d)) in plain.iter().zip(&dec).enumerate() {
            if ![0, 1, 82, 83, 84, 165, 166, 167, 248].contains(&i) {
                assert_eq!(p, d, "byte {i}");
            }
        }
    }

    #[test]
    fn four_handles_short_and_single() {
        assert_eq!(four(&[1, 2, 3, 4, 5]), Some([1, 2, 3, 4]));
        assert_eq!(four(&[7]), Some([7; 4]));
        assert_eq!(four(&[1, 2]), None);
    }

    #[test]
    fn levels_from_plain_raw_ifd_grbg_and_black1() {
        let e = Endian::Little;
        let mut f = header(e);
        f.extend(ifd(
            e,
            8,
            &[
                (sr2::BLACK_LEVEL_1, Val::Short(vec![128])),
                (sr2::WB_GRBG, Val::Short(vec![1024, 2000, 1500, 1030])),
                (sr2::WHITE_LEVEL, Val::Short(vec![4095])),
            ],
            0,
        ));
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        let lv = levels(&src, &t, t.ifd0()).unwrap();
        assert_eq!(lv.black, Some([128; 4]));
        assert_eq!(lv.white, Some(4095));
        assert_eq!(lv.wb, Some([2000.0, 1024.0, 1030.0, 1500.0]));
    }

    #[test]
    fn makernote_with_and_without_sony_header() {
        let e = Endian::Little;
        for prefixed in [false, true] {
            let mut f = header(e);
            let mn_at = 64u32;
            let exif = ifd(
                e,
                8,
                &[(
                    tag::MAKER_NOTE,
                    Val::Raw {
                        typ: 7,
                        count: 40,
                        bytes: mn_at.to_le_bytes().to_vec(),
                    },
                )],
                0,
            );
            f.extend(exif);
            f.resize(mn_at as usize, 0);
            let at = if prefixed {
                f.extend_from_slice(b"SONY DSC \0\0\0");
                mn_at + 12
            } else {
                mn_at
            };
            f.extend(ifd(e, at, &[(0x2000, Val::Short(vec![5]))], 0));
            let src = SliceSource::new(&f);
            let t = Tiff::parse(&src, false).unwrap();
            let mn = makernote(&src, t.ifd0()).unwrap().unwrap();
            assert_eq!(mn.uint(&src, 0x2000).unwrap(), Some(5));
        }
        // No maker note tag.
        let mut f = header(e);
        f.extend(ifd(e, 8, &[(tag::ORIENTATION, Val::Short(vec![1]))], 0));
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert!(makernote(&src, t.ifd0()).unwrap().is_none());
    }

    #[test]
    fn shutter_count_gated_by_model_and_length() {
        let e = Endian::Little;
        let mut plain = [0u8; 0x40];
        plain[0x3A..0x3E].copy_from_slice(&0x0100_0D5Au32.to_le_bytes()); // high byte masked off
        let enc: Vec<u8> = plain
            .iter()
            .map(|&b| {
                if b >= 249 {
                    b
                } else {
                    (u32::from(b).pow(3) % 249) as u8
                }
            })
            .collect();
        let mut f = header(e);
        f.extend(ifd(e, 8, &[(TAG_9050, Val::Undefined(enc.clone()))], 0));
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(
            shutter_count(&src, "ILCE-9", t.ifd0()).unwrap(),
            Some(0x0D5A)
        );
        assert_eq!(shutter_count(&src, "ILCE-1", t.ifd0()).unwrap(), None);
        // Too short.
        let mut f = header(e);
        f.extend(ifd(
            e,
            8,
            &[(TAG_9050, Val::Undefined(enc[..0x20].to_vec()))],
            0,
        ));
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(shutter_count(&src, "ILCE-9", t.ifd0()).unwrap(), None);
        // Missing tag.
        let mut f = header(e);
        f.extend(ifd(e, 8, &[(0x2000, Val::Short(vec![1]))], 0));
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(shutter_count(&src, "ILCE-9", t.ifd0()).unwrap(), None);
    }

    fn mn_with(entries: &[(u16, Val)]) -> (Vec<u8>, Ifd) {
        let e = Endian::Little;
        let mut f = header(e);
        f.extend(ifd(e, 8, entries, 0));
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        let i = t.ifd0().clone();
        (f, i)
    }

    #[test]
    fn af_point_sources() {
        // FocalPlaneAFPoints present but none used → no AF point, even with
        // FocusLocation present (the A9 always writes the center there).
        let mut af = vec![1u8, 0];
        for v in [640u16, 428] {
            af.extend_from_slice(&v.to_le_bytes());
        }
        af.resize(66, 0);
        let (f, mn) = mn_with(&[
            (TAG_FOCAL_PLANE_AF, Val::Undefined(af.clone())),
            (TAG_FOCUS_LOCATION, Val::Short(vec![6000, 4000, 3000, 2000])),
        ]);
        assert_eq!(af_point(&SliceSource::new(&f), &mn), None);
        // Zero area.
        af[1] = 1;
        af[2..6].fill(0);
        let (f, mn) = mn_with(&[(TAG_FOCAL_PLANE_AF, Val::Undefined(af))]);
        assert_eq!(af_point(&SliceSource::new(&f), &mn), None);
        // Claims 15 points but the tag holds only one: uses what is there.
        let mut short = vec![1u8, 15];
        for v in [100u16, 100, 25, 50] {
            short.extend_from_slice(&v.to_le_bytes());
        }
        let (f, mn) = mn_with(&[(TAG_FOCAL_PLANE_AF, Val::Undefined(short))]);
        assert_eq!(af_point(&SliceSource::new(&f), &mn), Some((0.25, 0.5)));
        // Claims points but has none.
        let mut none = vec![1u8, 3];
        for v in [100u16, 100] {
            none.extend_from_slice(&v.to_le_bytes());
        }
        let (f, mn) = mn_with(&[(TAG_FOCAL_PLANE_AF, Val::Undefined(none))]);
        assert_eq!(af_point(&SliceSource::new(&f), &mn), None);
        // Older bodies: FocusLocation.
        let (f, mn) = mn_with(&[(TAG_FOCUS_LOCATION, Val::Short(vec![6000, 4000, 1500, 3000]))]);
        assert_eq!(af_point(&SliceSource::new(&f), &mn), Some((0.25, 0.75)));
        for bad in [
            &[0u16, 4000, 1, 1][..],
            &[6000, 4000, 0, 0],
            &[6000, 4000, 7000, 1],
            &[6000, 4000],
        ] {
            let (f, mn) = mn_with(&[(TAG_FOCUS_LOCATION, Val::Short(bad.to_vec()))]);
            assert_eq!(af_point(&SliceSource::new(&f), &mn), None, "{bad:?}");
        }
        let (f, mn) = mn_with(&[(0x2000, Val::Short(vec![1]))]);
        assert_eq!(af_point(&SliceSource::new(&f), &mn), None);
    }

    #[test]
    fn shot_numbers_need_the_9400c_layout() {
        let mut plain = [0u8; 0x20];
        plain[0x0A..0x0E].copy_from_slice(&42u32.to_le_bytes());
        let enc: Vec<u8> = plain
            .iter()
            .map(|&b| {
                if b >= 249 {
                    b
                } else {
                    (u32::from(b).pow(3) % 249) as u8
                }
            })
            .collect();
        let with_magic = |m: u8| {
            let mut v = enc.clone();
            v[0] = m;
            v
        };
        let (f, mn) = mn_with(&[(TAG_9400, Val::Undefined(with_magic(0x26)))]);
        assert_eq!(
            shot_numbers(&SliceSource::new(&f), &mn),
            (Some(42), Some(1))
        );
        // Other layouts (9400a/b) are not decoded.
        let (f, mn) = mn_with(&[(TAG_9400, Val::Undefined(with_magic(0x0C)))]);
        assert_eq!(shot_numbers(&SliceSource::new(&f), &mn), (None, None));
        // Too short / absent.
        let (f, mn) = mn_with(&[(TAG_9400, Val::Undefined(with_magic(0x26)[..0x10].to_vec()))]);
        assert_eq!(shot_numbers(&SliceSource::new(&f), &mn), (None, None));
        let (f, mn) = mn_with(&[(0x2000, Val::Short(vec![1]))]);
        assert_eq!(shot_numbers(&SliceSource::new(&f), &mn), (None, None));
        // Value offset past EOF.
        let (f, mn) = mn_with(&[(
            TAG_9400,
            Val::Raw {
                typ: 7,
                count: 0x40,
                bytes: 0xFFFF_0000u32.to_le_bytes().to_vec(),
            },
        )]);
        assert_eq!(shot_numbers(&SliceSource::new(&f), &mn), (None, None));
    }
}
