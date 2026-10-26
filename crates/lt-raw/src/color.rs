//! Camera color matrix handling.
//!
//! # Convention for [`lt_core::RawImage::cam_to_xyz`]
//!
//! Let `M` be the camera's Adobe `ColorMatrix` for D65 (DNG `ColorMatrix2`,
//! XYZ → camera native RGB), and `w = (0.95047, 1.0, 1.08883)` the XYZ of the
//! D65 white point (Y = 1). Each row of `M` is divided by its response to
//! that white:
//!
//! ```text
//! M'[i][j] = M[i][j] / (M[i] · w)      so that   M' · w = (1, 1, 1)
//! cam_to_xyz = inverse(M')              so that   cam_to_xyz · (1, 1, 1) = w
//! ```
//!
//! That is: **after white balance, a neutral pixel (R = G = B) maps to the D65
//! white chromaticity**, with Y equal to its (white-balanced) green value.
//! The pipeline therefore does, per pixel,
//!
//! ```text
//! cam  = (raw - black[c]) / (white - black[c])        // 0..1
//! cam *= wb                                           // wb_as_shot, or from Kelvin/tint
//! xyz  = cam_to_xyz · cam                             // D65-relative XYZ
//! ```
//!
//! and never applies an extra D65 → working-space adaptation for the WB
//! itself: that is what the multipliers already did. The D65 "daylight"
//! multipliers implied by the matrix are `1 / (M · w)`, normalized to G = 1
//! ([`daylight_multipliers`]).
//!
//! If a camera has no D65 matrix, the one for another illuminant (lowest
//! EXIF `LightSource` code first) goes through the same D65 normalization:
//! the dcraw-style single-matrix approximation, accurate near that
//! illuminant and increasingly off away from it. `DecodeInfo::matrix_illuminant`
//! reports which one was used (21 = D65).

/// XYZ of the CIE D65 white point, Y = 1.
pub const D65_WHITE: [f64; 3] = [0.950_47, 1.0, 1.088_83];

type M3 = [[f64; 3]; 3];

fn mul_vec(m: &M3, v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

/// Inverse of a 3×3 matrix, `None` if (numerically) singular.
pub(crate) fn invert(m: &M3) -> Option<M3> {
    let c =
        |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    let cof = [
        [c(1, 2, 1, 2), -c(1, 2, 0, 2), c(1, 2, 0, 1)],
        [-c(0, 2, 1, 2), c(0, 2, 0, 2), -c(0, 2, 0, 1)],
        [c(0, 1, 1, 2), -c(0, 1, 0, 2), c(0, 1, 0, 1)],
    ];
    let det = m[0][0] * cof[0][0] + m[0][1] * cof[0][1] + m[0][2] * cof[0][2];
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    // inverse = adjugate / det, adjugate = cofactorᵀ
    Some([0, 1, 2].map(|i| [0, 1, 2].map(|j| cof[j][i] / det)))
}

/// `M · w` for a row-major 9-element `ColorMatrix` (XYZ → camera).
/// Also returns the matrix itself as `f64`.
fn white_response(xyz_to_cam: &[f32; 9]) -> (M3, [f64; 3]) {
    let m: M3 = [0, 1, 2].map(|i| [0, 1, 2].map(|j| f64::from(xyz_to_cam[i * 3 + j])));
    let resp = mul_vec(&m, D65_WHITE);
    (m, resp)
}

/// `cam_to_xyz` from an Adobe `ColorMatrix` (XYZ → camera, D65), following
/// the module-level convention. `None` if the matrix is degenerate (a camera
/// channel does not respond positively to white, or it is singular).
pub fn cam_to_xyz(xyz_to_cam: &[f32; 9]) -> Option<[[f32; 3]; 3]> {
    let (m, resp) = white_response(xyz_to_cam);
    if resp.iter().any(|r| !r.is_finite() || *r <= 1e-6) {
        return None;
    }
    let norm: M3 = [0, 1, 2].map(|i| m[i].map(|v| v / resp[i]));
    let inv = invert(&norm)?;
    Some(inv.map(|row| row.map(|v| v as f32)))
}

/// White-balance multipliers (R, G, B; G = 1) that neutralize D65 daylight
/// for this matrix: `1 / (M · w)`, normalized. `None` for degenerate matrices.
pub fn daylight_multipliers(xyz_to_cam: &[f32; 9]) -> Option<[f32; 3]> {
    let (_, resp) = white_response(xyz_to_cam);
    if resp.iter().any(|r| !r.is_finite() || *r <= 1e-6) {
        return None;
    }
    Some([resp[1] / resp[0], 1.0, resp[1] / resp[2]].map(|v| v as f32))
}

/// Normalize RGGB-ish gains to (R, G, B) with G = 1, where G is the mean of the
/// two greens. `None` unless all gains are finite and positive.
pub(crate) fn normalize_wb(r: f32, g1: f32, g2: f32, b: f32) -> Option<[f32; 3]> {
    let g = if g2.is_finite() && g2 > 0.0 {
        g1.midpoint(g2)
    } else {
        g1
    };
    let out = [r / g, 1.0, b / g];
    (g.is_finite() && g > 0.0 && out.iter().all(|v| v.is_finite() && *v > 0.0)).then_some(out)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests compare exactly-representable values
mod tests {
    use super::*;

    /// Adobe `ColorMatrix2` for the Sony ILCE-9 (as in rawler's camera DB).
    pub const A9: [f32; 9] = [
        0.6389, -0.1703, -0.0378, -0.4562, 1.2265, 0.2587, -0.067, 0.1489, 0.655,
    ];

    #[test]
    fn neutral_maps_to_d65_white() {
        let c = cam_to_xyz(&A9).unwrap();
        for (i, row) in c.iter().enumerate() {
            let sum: f32 = row.iter().sum();
            assert!(
                (f64::from(sum) - D65_WHITE[i]).abs() < 1e-5,
                "row {i}: {sum}"
            );
        }
    }

    #[test]
    fn roundtrip_with_normalized_forward_matrix() {
        let c = cam_to_xyz(&A9).unwrap();
        let cd: M3 = c.map(|r| r.map(f64::from));
        let back = invert(&cd).unwrap();
        // back · w must be (1,1,1): the row-normalized forward matrix.
        let v = mul_vec(&back, D65_WHITE);
        for x in v {
            assert!((x - 1.0).abs() < 1e-5);
        }
        // Row ratios of `back` equal those of the original matrix.
        for i in 0..3 {
            let k = back[i][1] / f64::from(A9[i * 3 + 1]);
            for j in 0..3 {
                assert!((back[i][j] - k * f64::from(A9[i * 3 + j])).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn daylight_multipliers_are_plausible_for_a9() {
        let [r, g, b] = daylight_multipliers(&A9).unwrap();
        assert_eq!(g, 1.0);
        assert!((2.0..3.5).contains(&r), "{r}");
        assert!((1.1..2.0).contains(&b), "{b}");
    }

    #[test]
    fn degenerate_matrices_rejected() {
        assert!(cam_to_xyz(&[0.0; 9]).is_none());
        assert!(daylight_multipliers(&[0.0; 9]).is_none());
        // Positive white response but singular (two identical rows).
        let s = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        assert!(cam_to_xyz(&s).is_none());
        assert!(invert(&[[f64::NAN; 3]; 3]).is_none());
        // Identity is its own inverse.
        let id = invert(&[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]).unwrap();
        assert_eq!(id, [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    }

    #[test]
    fn wb_normalization() {
        assert_eq!(
            normalize_wb(2568.0, 1024.0, 1024.0, 1620.0).unwrap(),
            [2.507_812_5, 1.0, 1.582_031_3]
        );
        assert_eq!(
            normalize_wb(2.0, 1.0, f32::NAN, 1.5).unwrap(),
            [2.0, 1.0, 1.5]
        );
        assert_eq!(normalize_wb(2.0, 1.0, 3.0, 4.0).unwrap(), [1.0, 1.0, 2.0]);
        assert!(normalize_wb(0.0, 1.0, 1.0, 1.0).is_none());
        assert!(normalize_wb(1.0, 0.0, 0.0, 1.0).is_none());
        assert!(normalize_wb(f32::NAN, 1.0, 1.0, 1.0).is_none());
    }
}
