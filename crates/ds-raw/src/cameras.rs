//! Per-camera data a RAW file doesn't carry itself: the colour matrix, and
//! for some formats the sensor margins and white level.
//!
//! DNGs carry all of it. For the other formats it comes from, in order:
//!
//! 1. [`TABLE`], Darkslide's own data. Every row names where its numbers
//!    come from; only sources that may be redistributed under Apache-2.0
//!    are allowed (today: matrices a camera wrote into its own DNG in the
//!    CC0 compatibility corpus). See `docs/RAWDEC.md` for the policy and the
//!    open question of sourcing more.
//! 2. With the `rawler` feature, rawler's camera database, read through its
//!    public API (the data stays in the LGPL crate; nothing is copied here).
//!
//! A camera found in neither gets [`RawError::Unsupported`](crate::RawError)
//! from the native decoders.

#[cfg(feature = "rawler")]
use crate::decode::D65_LIGHT_SOURCE;

/// One row of Darkslide's own camera table.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Row {
    /// EXIF `Make` and `Model`, trimmed (matched case-insensitively).
    pub make: &'static str,
    pub model: &'static str,
    /// Adobe-style `ColorMatrix` (XYZ → camera, row-major).
    pub matrix: [f32; 9],
    /// EXIF `LightSource` code the matrix is calibrated for (21 = D65).
    pub illuminant: u16,
    /// Margins `[left, top, right, bottom]` of the recommended crop.
    pub crop: Option<[usize; 4]>,
    /// Where the numbers come from (kept with the data; see `docs/RAWDEC.md`).
    #[allow(dead_code)] // documentation, read by humans and the table test
    pub provenance: &'static str,
}

/// Darkslide's camera table. Add a row only with a redistributable source.
pub(crate) const TABLE: &[Row] = &[Row {
    make: "RICOH IMAGING COMPANY, LTD.",
    model: "PENTAX K-3 Mark III",
    // ColorMatrix2 / 65536 as the camera wrote it into its own DNG.
    matrix: [
        46019.0 / 65536.0,
        -10631.0 / 65536.0,
        -5830.0 / 65536.0,
        -30238.0 / 65536.0,
        83413.0 / 65536.0,
        13536.0 / 65536.0,
        -4194.0 / 65536.0,
        9364.0 / 65536.0,
        37261.0 / 65536.0,
    ],
    illuminant: 21,
    // ActiveArea origin (26, 34) + DefaultCropOrigin (28, 24), DefaultCropSize
    // 6192 × 4128, in the 6304 × 4224 frame.
    crop: Some([54, 58, 58, 38]),
    provenance: "in-camera DNG, compat corpus pentax-k-3-mark-iii.dng (CC0): \
                 ColorMatrix2, CalibrationIlluminant2 = D65; ActiveArea, DefaultCrop",
}];

/// What is known about a camera beyond its file.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Camera {
    /// `ColorMatrix` (XYZ → camera) and its EXIF `LightSource` code.
    pub matrix: Option<([f32; 9], u16)>,
    /// Margins `[left, top, right, bottom]` of the area with image data.
    pub active: Option<[usize; 4]>,
    /// Margins `[left, top, right, bottom]` of the recommended crop.
    pub crop: Option<[usize; 4]>,
    pub white: Option<u32>,
    /// Masked (optically black) rows and columns, as `(start, count)`
    /// ranges measured from the frame edge: top rows / left columns.
    pub black_rows: Option<(usize, usize)>,
    pub black_cols: Option<(usize, usize)>,
    /// The 2×2 CFA at the frame's top-left pixel.
    pub cfa: Option<ds_core::CfaPattern>,
}

fn own_row(make: &str, model: &str) -> Option<&'static Row> {
    TABLE
        .iter()
        .find(|r| r.make.eq_ignore_ascii_case(make) && r.model.eq_ignore_ascii_case(model))
}

/// Everything known about `make`/`model` (EXIF values, trimmed); `mode`
/// picks a variant entry where the camera DB has one (`"12bit"`, `"14bit"`).
pub(crate) fn lookup(make: &str, model: &str, mode: &str) -> Camera {
    let mut cam = db(make, model, mode);
    if let Some(r) = own_row(make, model) {
        cam.matrix = Some((r.matrix, r.illuminant));
        cam.crop = r.crop.or(cam.crop);
    }
    cam
}

/// The colour matrix alone (see [`lookup`]).
pub(crate) fn matrix(make: &str, model: &str) -> Option<([f32; 9], u16)> {
    lookup(make, model, "").matrix
}

#[cfg(not(feature = "rawler"))]
fn db(_: &str, _: &str, _: &str) -> Camera {
    Camera::default()
}

/// rawler's entry for the camera: the `mode` variant when there is one,
/// else the plain entry. The make is matched as written, then upper-cased
/// (rawler keys Sony as `SONY`).
#[cfg(feature = "rawler")]
fn db(make: &str, model: &str, mode: &str) -> Camera {
    let cams = rawler::global_loader().get_cameras();
    let get =
        |make: &str, mode: &str| cams.get(&(make.to_string(), model.to_string(), mode.into()));
    let upper = make.to_ascii_uppercase();
    let Some(c) = get(make, mode)
        .or_else(|| get(&upper, mode))
        .or_else(|| get(make, ""))
        .or_else(|| get(&upper, ""))
    else {
        return Camera::default();
    };
    let margins = |m: Option<[usize; 4]>| m.filter(|m| m.iter().any(|&v| v > 0));
    Camera {
        matrix: matrix_from_map(&c.color_matrix),
        active: margins(c.active_area),
        crop: margins(c.crop_area),
        white: c.whitelevel.as_ref().and_then(|w| w.first().copied()),
        black_rows: c.blackareah,
        black_cols: c.blackareav,
        cfa: (c.cfa.width == 2 && c.cfa.height == 2)
            .then(|| {
                let p: Vec<u32> = [(0, 0), (0, 1), (1, 0), (1, 1)]
                    .iter()
                    .map(|&(r, col)| c.cfa.color_at(r, col) as u32)
                    .collect();
                crate::decode::cfa_from_tag(&p)
            })
            .flatten(),
    }
}

/// Prefer the D65 matrix, else the first available one by illuminant order
/// (`None` if there is none). The second value is its EXIF `LightSource` code.
///
/// A non-D65 matrix is still normalized to D65 white (see [`crate::color`]):
/// the same approximation dcraw makes for cameras with a single matrix. It
/// is logged and reported through [`crate::DecodeInfo::matrix_illuminant`].
#[cfg(feature = "rawler")]
pub(crate) fn matrix_from_map(
    m: &std::collections::HashMap<rawler::imgop::xyz::Illuminant, Vec<f32>>,
) -> Option<([f32; 9], u16)> {
    use rawler::imgop::xyz::Illuminant;
    let (ill, v) = if let Some(v) = m.get(&Illuminant::D65) {
        (Illuminant::D65, v)
    } else {
        let k = *m.keys().min()?;
        (k, &m[&k])
    };
    let arr: [f32; 9] = v.get(..9).map(|s| std::array::from_fn(|i| s[i]))?;
    let code = u16::from(ill);
    if code != D65_LIGHT_SOURCE {
        tracing::warn!(illuminant = ?ill, "no D65 color matrix; using {ill:?} with D65 normalization");
    }
    Some((arr, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_rows_are_found_case_insensitively_and_carry_provenance() {
        for r in TABLE {
            assert_ne!(r.provenance, "");
            let m = matrix(&r.make.to_lowercase(), &r.model.to_uppercase()).unwrap();
            assert_eq!(m, (r.matrix, r.illuminant));
            assert!(crate::color::cam_to_xyz(&r.matrix).is_some(), "{}", r.model);
        }
        assert!(own_row("NOPE", "NOPE-1").is_none());
        assert!(matrix("NOPE", "NOPE-1").is_none());
    }

    #[cfg(feature = "rawler")]
    #[test]
    fn a9_matrix_is_in_rawler_db() {
        let (m, ill) = matrix("SONY", "ILCE-9").unwrap();
        assert!((m[0] - 0.6389).abs() < 1e-6);
        assert_eq!(ill, D65_LIGHT_SOURCE);
        // Make case does not matter (consistent with Container::is_sony).
        assert_eq!(matrix("Sony", "ILCE-9").unwrap().0, m);
        // Variant entries, with the plain one as the fallback.
        let d850 = lookup("NIKON CORPORATION", "NIKON D850", "14bit");
        assert_eq!(d850.white, Some(15520));
        assert_eq!(d850.crop, Some([16, 8, 16, 8]));
        assert_eq!(d850.active, None, "all-zero margins are no margins");
        assert_eq!(d850.cfa, Some(ds_core::CfaPattern::Rggb));
        let plain = lookup("NIKON CORPORATION", "NIKON D850", "no-such-mode");
        assert_eq!(plain.white, None);
        assert!(plain.matrix.is_some());
        assert_eq!(lookup("NOPE", "NOPE-1", ""), Camera::default());
    }

    #[cfg(feature = "rawler")]
    #[test]
    fn matrix_map_prefers_d65_then_any() {
        use rawler::imgop::xyz::Illuminant;
        let mut m = std::collections::HashMap::new();
        assert!(matrix_from_map(&m).is_none());
        m.insert(Illuminant::D55, vec![3.0; 9]);
        m.insert(Illuminant::A, vec![2.0; 9]);
        // Lowest illuminant code wins deterministically: A (17) before D55 (20).
        assert_eq!(matrix_from_map(&m), Some(([2.0; 9], 17)));
        m.insert(Illuminant::D65, vec![1.0; 12]);
        assert_eq!(matrix_from_map(&m), Some(([1.0; 9], D65_LIGHT_SOURCE)));
        m.insert(Illuminant::D65, vec![1.0; 3]);
        assert!(matrix_from_map(&m).is_none());
    }
}
