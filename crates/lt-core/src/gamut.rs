//! Soft gamut compression, shared by the develop pipeline's output transform and
//! exports to an ICC profile (`lt-export`), so both bring colours in the same way.

/// Soft gamut compression (ACES reference gamut compression, simplified to one
/// threshold). Pulls out-of-gamut (negative) components toward the achromatic axis
/// smoothly instead of hard clipping, so saturated gradients don't band.
/// Distances below `THR` are untouched; `LIM` maps onto the gamut boundary.
#[inline]
pub fn compress(rgb: [f32; 3]) -> [f32; 3] {
    let ach = rgb[0].max(rgb[1]).max(rgb[2]);
    if ach <= 1e-10 {
        return [rgb[0].max(0.0), rgb[1].max(0.0), rgb[2].max(0.0)];
    }
    let mut out = rgb;
    for c in &mut out {
        let d = (ach - *c) / ach;
        if d > THR {
            let x = d - THR;
            let cd = THR + x / (1.0 + x / S);
            *c = ach - cd * ach;
        }
    }
    [out[0].max(0.0), out[1].max(0.0), out[2].max(0.0)]
}

/// Distance from the achromatic axis below which colors are untouched.
pub const THR: f32 = 0.8;
/// Distance that lands exactly on the gamut boundary after compression.
pub const LIM: f32 = 1.25;
/// Curve scale so that `f(LIM) = 1`: `s = b·a / (a − b)`, `a = LIM − THR`, `b = 1 − THR`.
pub const S: f32 = (1.0 - THR) * (LIM - THR) / ((LIM - THR) - (1.0 - THR));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_gamut_is_untouched_and_the_limit_lands_on_the_boundary() {
        let same = compress([0.5, 0.4, 0.3]);
        assert!(
            same.iter()
                .zip([0.5, 0.4, 0.3])
                .all(|(a, b)| (a - b).abs() < 1e-7),
            "{same:?}"
        );
        // A component at distance LIM from the achromatic axis lands on 0.
        let o = compress([1.0, 0.5, 1.0 - LIM]);
        assert!(o[2].abs() < 1e-5 && (o[1] - 0.5).abs() < 1e-6, "{o:?}");
        assert!(compress([-1.0, -2.0, -3.0]).iter().all(|v| v.abs() < 1e-7));
    }
}
