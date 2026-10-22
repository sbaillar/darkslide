//! Standard output ICC profiles (sRGB, Display P3, and Rec.2020 for wide
//! intermediates), built deterministically.
//!
//! Both are compact ICC v4 matrix/TRC display profiles: D50-adapted colorants
//! (Bradford) derived from the standard primaries and D65 white, a `chad`
//! tag, and the sRGB parametric TRC (`para` type 3), which Display P3 shares.
//! Built from first principles instead of vendoring third-party profile
//! bytes; the bytes are cached so every export embeds the identical profile.

use std::sync::OnceLock;

use crate::recipe::OutputSpace;

/// Largest ICC profile a file embeds. libjpeg numbers a profile's APP2 chunks with one
/// byte (255 chunks of 65 519 bytes, about 16.7 MB), so a bigger one would corrupt a JPEG.
pub const MAX_EMBEDDED_BYTES: usize = 16_000_000;

/// PCS illuminant (D50) as ICC stores it.
pub const D50: [f64; 3] = [0.9642, 1.0, 0.8249];

pub(crate) type M3 = [[f64; 3]; 3];

pub(crate) fn mul(a: &M3, b: &M3) -> M3 {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

fn mul_v(a: &M3, v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| a[i][0] * v[0] + a[i][1] * v[1] + a[i][2] * v[2])
}

pub(crate) fn inv(m: &M3) -> M3 {
    let d = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let c =
        |r0: usize, c0: usize, r1: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    [
        [c(1, 1, 2, 2) / d, -c(0, 1, 2, 2) / d, c(0, 1, 1, 2) / d],
        [-c(1, 0, 2, 2) / d, c(0, 0, 2, 2) / d, -c(0, 0, 1, 2) / d],
        [c(1, 0, 2, 1) / d, -c(0, 0, 2, 1) / d, c(0, 0, 1, 1) / d],
    ]
}

fn xy_to_xyz(x: f64, y: f64) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Bradford chromatic adaptation from `src` white to `dst` white (XYZ).
fn bradford(src: [f64; 3], dst: [f64; 3]) -> M3 {
    const B: M3 = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let (s, d) = (mul_v(&B, src), mul_v(&B, dst));
    let diag = [
        [d[0] / s[0], 0.0, 0.0],
        [0.0, d[1] / s[1], 0.0],
        [0.0, 0.0, d[2] / s[2]],
    ];
    mul(&inv(&B), &mul(&diag, &B))
}

/// RGB→XYZ(D50) matrix for primaries `(x, y)` ×3 and a D65 white, plus the `chad` matrix.
pub fn colorants(primaries: [(f64, f64); 3]) -> (M3, M3) {
    let white = xy_to_xyz(0.3127, 0.3290);
    let p = primaries.map(|(x, y)| xy_to_xyz(x, y));
    let pm: M3 = [0, 1, 2].map(|i| [p[0][i], p[1][i], p[2][i]]);
    let s = mul_v(&inv(&pm), white);
    let rgb_to_xyz: M3 = [0, 1, 2].map(|i| [pm[i][0] * s[0], pm[i][1] * s[1], pm[i][2] * s[2]]);
    let chad = bradford(white, D50);
    (mul(&chad, &rgb_to_xyz), chad)
}

const SRGB_PRIMARIES: [(f64, f64); 3] = [(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)];
const P3_PRIMARIES: [(f64, f64); 3] = [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)];
pub(crate) const REC2020_PRIMARIES: [(f64, f64); 3] =
    [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)];

fn s15f16(v: f64) -> [u8; 4] {
    ((v * 65_536.0).round() as i32).to_be_bytes()
}

fn tag_mluc(text: &str) -> Vec<u8> {
    let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
    let mut t = b"mluc\0\0\0\0".to_vec();
    t.extend(1u32.to_be_bytes());
    t.extend(12u32.to_be_bytes());
    t.extend(b"enUS");
    t.extend((utf16.len() as u32).to_be_bytes());
    t.extend(28u32.to_be_bytes());
    t.extend(utf16);
    t
}

fn tag_xyz(v: [f64; 3]) -> Vec<u8> {
    let mut t = b"XYZ \0\0\0\0".to_vec();
    for c in v {
        t.extend(s15f16(c));
    }
    t
}

fn tag_sf32(m: &M3) -> Vec<u8> {
    let mut t = b"sf32\0\0\0\0".to_vec();
    m.iter().flatten().for_each(|c| t.extend(s15f16(*c)));
    t
}

fn tag_srgb_trc() -> Vec<u8> {
    let mut t = b"para\0\0\0\0".to_vec();
    t.extend(3u16.to_be_bytes());
    t.extend(0u16.to_be_bytes());
    for v in [2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.040_45] {
        t.extend(s15f16(v));
    }
    t
}

/// Build an ICC v4 RGB display profile.
pub fn build_profile(description: &str, primaries: [(f64, f64); 3]) -> Vec<u8> {
    let (m, chad) = colorants(primaries);
    let col = |j: usize| [m[0][j], m[1][j], m[2][j]];
    let trc = tag_srgb_trc();
    // (signature, data); identical data is written once and shared.
    let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"desc", tag_mluc(description)),
        (b"cprt", tag_mluc("No copyright, use freely")),
        (b"wtpt", tag_xyz(D50)),
        (b"chad", tag_sf32(&chad)),
        (b"rXYZ", tag_xyz(col(0))),
        (b"gXYZ", tag_xyz(col(1))),
        (b"bXYZ", tag_xyz(col(2))),
        (b"rTRC", trc.clone()),
        (b"gTRC", trc.clone()),
        (b"bTRC", trc),
    ];
    let mut table = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let data_start = 128 + 4 + 12 * tags.len();
    let mut written: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for (sig, bytes) in &tags {
        let (off, len) = if let Some((o, l, _)) = written.iter().find(|w| &w.2 == bytes) {
            (*o, *l)
        } else {
            let off = data_start + data.len();
            data.extend(bytes);
            while !data.len().is_multiple_of(4) {
                data.push(0);
            }
            written.push((off, bytes.len(), bytes.clone()));
            (off, bytes.len())
        };
        table.extend(*sig);
        table.extend((off as u32).to_be_bytes());
        table.extend((len as u32).to_be_bytes());
    }
    let total = data_start + data.len();
    let mut h = vec![0u8; 128];
    h[0..4].copy_from_slice(&(total as u32).to_be_bytes());
    h[8..12].copy_from_slice(&[4, 0x30, 0, 0]);
    h[12..16].copy_from_slice(b"mntr");
    h[16..20].copy_from_slice(b"RGB ");
    h[20..24].copy_from_slice(b"XYZ ");
    for (i, v) in [2026u16, 1, 1, 0, 0, 0].iter().enumerate() {
        h[24 + i * 2..26 + i * 2].copy_from_slice(&v.to_be_bytes());
    }
    h[36..40].copy_from_slice(b"acsp");
    h[40..44].copy_from_slice(b"APPL");
    for (i, c) in D50.iter().enumerate() {
        h[68 + i * 4..72 + i * 4].copy_from_slice(&s15f16(*c));
    }
    h[80..84].copy_from_slice(b"dslx");
    let mut out = h;
    out.extend((tags.len() as u32).to_be_bytes());
    out.extend(table);
    out.extend(data);
    out
}

/// Linear sRGB → linear Display P3 (both D65), for sRGB content composited into a
/// P3 output (watermarks).
pub fn srgb_to_p3() -> [[f32; 3]; 3] {
    srgb_to(P3_PRIMARIES)
}

/// Linear sRGB → linear Rec.2020 (an output-profile recipe's render).
pub fn srgb_to_rec2020() -> [[f32; 3]; 3] {
    srgb_to(REC2020_PRIMARIES)
}

fn srgb_to(primaries: [(f64, f64); 3]) -> [[f32; 3]; 3] {
    let (srgb, _) = colorants(SRGB_PRIMARIES);
    let (to, _) = colorants(primaries);
    mul(&inv(&to), &srgb).map(|r| r.map(|v| v as f32))
}

/// The embedded profile for an output space (cached, deterministic).
pub fn profile(space: OutputSpace) -> &'static [u8] {
    static SRGB: OnceLock<Vec<u8>> = OnceLock::new();
    static P3: OnceLock<Vec<u8>> = OnceLock::new();
    static REC2020: OnceLock<Vec<u8>> = OnceLock::new();
    match space {
        OutputSpace::Srgb => SRGB.get_or_init(|| build_profile("sRGB (built-in)", SRGB_PRIMARIES)),
        OutputSpace::DisplayP3 => P3.get_or_init(|| build_profile("Display P3", P3_PRIMARIES)),
        OutputSpace::Rec2020 => REC2020
            .get_or_init(|| build_profile("Rec. ITU-R BT.2020 (sRGB transfer)", REC2020_PRIMARIES)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn fixed(b: &[u8], at: usize) -> f64 {
        f64::from(i32::from_be_bytes(b[at..at + 4].try_into().unwrap())) / 65_536.0
    }

    /// Minimal structural ICC parser: returns (sig, offset, size) for every tag.
    fn parse(p: &[u8]) -> Vec<([u8; 4], usize, usize)> {
        assert_eq!(be32(p, 0) as usize, p.len(), "size field");
        assert_eq!(&p[36..40], b"acsp");
        assert_eq!(&p[12..16], b"mntr");
        assert_eq!(p[8], 4, "v4");
        let n = be32(p, 128) as usize;
        (0..n)
            .map(|i| {
                let e = 132 + i * 12;
                let (off, len) = (be32(p, e + 4) as usize, be32(p, e + 8) as usize);
                assert!(off % 4 == 0 && off + len <= p.len(), "tag in bounds");
                (p[e..e + 4].try_into().unwrap(), off, len)
            })
            .collect()
    }

    fn xyz_tag(p: &[u8], tags: &[([u8; 4], usize, usize)], sig: [u8; 4]) -> [f64; 3] {
        let (_, off, len) = tags.iter().find(|t| t.0 == sig).unwrap();
        assert_eq!((&p[*off..off + 4], *len), (&b"XYZ "[..], 20));
        [0, 1, 2].map(|i| fixed(p, off + 8 + i * 4))
    }

    #[test]
    fn profiles_are_structurally_valid() {
        for space in [
            OutputSpace::Srgb,
            OutputSpace::DisplayP3,
            OutputSpace::Rec2020,
        ] {
            let p = profile(space);
            let tags = parse(p);
            let sigs: Vec<_> = tags.iter().map(|t| t.0).collect();
            for s in [
                b"desc", b"cprt", b"wtpt", b"chad", b"rXYZ", b"gXYZ", b"bXYZ", b"rTRC", b"gTRC",
                b"bTRC",
            ] {
                assert!(
                    sigs.contains(s),
                    "{space:?} missing {}",
                    String::from_utf8_lossy(s)
                );
            }
            // Colorants sum to the D50 white.
            let sum = [b"rXYZ", b"gXYZ", b"bXYZ"]
                .iter()
                .map(|s| xyz_tag(p, &tags, **s))
                .fold([0.0; 3], |a, v| [a[0] + v[0], a[1] + v[1], a[2] + v[2]]);
            for (a, b) in sum.iter().zip(D50) {
                assert!((a - b).abs() < 2e-3, "{space:?} white {sum:?}");
            }
            // TRCs share one para block.
            let trc: Vec<_> = tags
                .iter()
                .filter(|t| t.0.ends_with(b"TRC"))
                .map(|t| t.1)
                .collect();
            assert!(trc.windows(2).all(|w| w[0] == w[1]));
            assert_eq!(&p[trc[0]..trc[0] + 4], b"para");
        }
        assert!(std::ptr::eq(
            profile(OutputSpace::Srgb),
            profile(OutputSpace::Srgb)
        ));
        assert_ne!(profile(OutputSpace::Srgb), profile(OutputSpace::DisplayP3));
    }

    #[test]
    fn colorants_match_published_values() {
        // sRGB D50 colorants as published in the ICC sRGB v4 profile.
        let (m, _) = colorants(SRGB_PRIMARIES);
        let expect = [
            [0.4361, 0.3851, 0.1431],
            [0.2225, 0.7169, 0.0606],
            [0.0139, 0.0971, 0.7141],
        ];
        // Display P3 D50 colorants (Apple's Display P3 profile).
        let (p3, _) = colorants(P3_PRIMARIES);
        let expect_p3 = [
            [0.5151, 0.2920, 0.1571],
            [0.2412, 0.6922, 0.0666],
            [-0.0011, 0.0419, 0.7841],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (m[i][j] - expect[i][j]).abs() < 1e-3,
                    "srgb {i},{j} = {}",
                    m[i][j]
                );
                assert!(
                    (p3[i][j] - expect_p3[i][j]).abs() < 1e-3,
                    "p3 {i},{j} = {}",
                    p3[i][j]
                );
            }
        }
    }

    #[test]
    fn description_is_utf16() {
        let p = profile(OutputSpace::DisplayP3);
        let tags = parse(p);
        let (_, off, len) = tags.iter().find(|t| &t.0 == b"desc").unwrap();
        let d = &p[*off..off + len];
        assert_eq!(&d[..4], b"mluc");
        let text: Vec<u16> = d[28..]
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&text).unwrap(), "Display P3");
    }

    #[test]
    fn srgb_to_p3_keeps_white_and_maps_red_inside() {
        let m = srgb_to_p3();
        for row in m {
            assert!(
                (row.iter().sum::<f32>() - 1.0).abs() < 1e-4,
                "white stays white"
            );
        }
        // sRGB red is inside P3: less than full P3 red, with a little green.
        assert!((m[0][0] - 0.8225).abs() < 1e-3, "{m:?}");
        assert!((m[1][0] - 0.0332).abs() < 1e-3, "{m:?}");
        assert!((m[2][0] - 0.0171).abs() < 1e-3, "{m:?}");
        // sRGB in Rec.2020 (BT.2087's matrix).
        let r = srgb_to_rec2020();
        assert!(
            r.iter()
                .all(|row| (row.iter().sum::<f32>() - 1.0).abs() < 1e-4)
        );
        assert!(
            (r[0][0] - 0.6274).abs() < 1e-3 && (r[1][1] - 0.9195).abs() < 1e-3,
            "{r:?}"
        );
    }
}
