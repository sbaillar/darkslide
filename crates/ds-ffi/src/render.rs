//! The synthetic test pattern, rendered like a real frame.
//!
//! It proves the pixel path end to end (Rust → caller-owned buffer → Metal
//! texture) before the real pipeline lands. The pattern is defined over a
//! *virtual full image* (the photo's pixel size), so any [`FrameInfo`] from
//! [`crate::frame::plan`] works: fit renders, zoomed renders, and 1:1 tiles
//! show exactly the pixels a real render of that region would. Colors are
//! defined in sRGB, converted to linear light, adjusted (exposure, warmth,
//! saturation), then written either as 8-bit sRGB (through LUTs) or as linear
//! Display P3 half floats (unclamped: exposure > 0 produces EDR values > 1).
//! Work proceeds in parallel row bands so it can report progress and stop
//! early on cancellation.

use std::sync::OnceLock;

use rayon::prelude::*;

use crate::error::DsError;
use crate::frame::{FrameFormat, FrameInfo};

/// Rows per progress/cancellation band.
pub(crate) const BAND_ROWS: u32 = 64;

/// Adjustments applied to the test pattern, in the same UI units as the real tools.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct TestPatternParams {
    /// Stops, -5..=5.
    #[uniffi(default = 0.0)]
    pub exposure_ev: f32,
    /// -1 (cool) ..= 1 (warm).
    #[uniffi(default = 0.0)]
    pub warmth: f32,
    /// -1 (grayscale) ..= 1 (double saturation).
    #[uniffi(default = 0.0)]
    pub saturation: f32,
}

impl Default for TestPatternParams {
    fn default() -> Self {
        Self {
            exposure_ev: 0.0,
            warmth: 0.0,
            saturation: 0.0,
        }
    }
}

impl TestPatternParams {
    pub(crate) fn validate(&self) -> Result<(), DsError> {
        let ok = |v: f32, lim: f32| v.is_finite() && (-lim..=lim).contains(&v);
        if !ok(self.exposure_ev, 5.0) {
            return Err(DsError::invalid(format!(
                "exposure_ev {} outside -5..=5",
                self.exposure_ev
            )));
        }
        if !ok(self.warmth, 1.0) || !ok(self.saturation, 1.0) {
            return Err(DsError::invalid(
                "warmth and saturation must be within -1..=1",
            ));
        }
        Ok(())
    }
}

/// The 24 `ColorChecker` Classic patches (sRGB D65, 8-bit), row-major 6×4.
const CHART: [[u8; 3]; 24] = [
    [115, 82, 68],
    [194, 150, 130],
    [98, 122, 157],
    [87, 108, 67],
    [133, 128, 177],
    [103, 189, 170],
    [214, 126, 44],
    [80, 91, 166],
    [193, 90, 99],
    [94, 60, 108],
    [157, 188, 64],
    [224, 163, 46],
    [56, 61, 150],
    [70, 148, 73],
    [175, 54, 60],
    [231, 199, 31],
    [187, 86, 149],
    [8, 133, 161],
    [243, 243, 242],
    [200, 200, 200],
    [160, 160, 160],
    [122, 122, 121],
    [85, 85, 85],
    [52, 52, 52],
];

const ENCODE_LUT_SIZE: usize = 4096;
/// Linear color between chart patches.
const SURROUND: [f32; 3] = [0.012, 0.012, 0.012];

fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// 8-bit sRGB → linear.
fn decode_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| std::array::from_fn(|i| srgb_to_linear(i as f32 / 255.0)))
}

/// Linear 0..=1 (quantized to 4096 steps) → 8-bit sRGB.
fn encode_lut() -> &'static [u8; ENCODE_LUT_SIZE] {
    static LUT: OnceLock<[u8; ENCODE_LUT_SIZE]> = OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|i| {
            (linear_to_srgb(i as f32 / (ENCODE_LUT_SIZE - 1) as f32) * 255.0).round() as u8
        })
    })
}

/// Continuous sRGB 0..=1 → linear through a 4096-entry table (no `powf` per pixel).
fn decode_fast(v: f32) -> f32 {
    static LUT: OnceLock<[f32; ENCODE_LUT_SIZE]> = OnceLock::new();
    let lut = LUT.get_or_init(|| {
        std::array::from_fn(|i| srgb_to_linear(i as f32 / (ENCODE_LUT_SIZE - 1) as f32))
    });
    lut[(v.clamp(0.0, 1.0) * (ENCODE_LUT_SIZE - 1) as f32 + 0.5) as usize]
}

fn encode(v: f32) -> u8 {
    let i = (v.clamp(0.0, 1.0) * (ENCODE_LUT_SIZE - 1) as f32 + 0.5) as usize;
    encode_lut()[i]
}

/// HSV (h in 0..1) → sRGB-encoded 0..1 RGB.
fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h6 = (h.fract() * 6.0).max(0.0);
    let i = h6.floor() as u32 % 6;
    let f = h6 - h6.floor();
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    match i {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// The unadjusted linear color at normalized position (fx, fy) of the pattern.
///
/// Layout, top to bottom: 45% hue sweep (fading to dark), 40% `ColorChecker`
/// chart on a dark surround, 15% gray step wedge (11 steps, 0..=100% sRGB).
fn base_color(fx: f32, fy: f32) -> [f32; 3] {
    if fy < 0.45 {
        let c = hsv(fx, 0.85, 1.0 - 0.7 * (fy / 0.45));
        return c.map(decode_fast);
    }
    if fy < 0.85 {
        // 6×4 grid with a margin; each cell shows its patch with a gap.
        let (cx, cy) = (fx * 6.0, (fy - 0.45) / 0.40 * 4.0);
        let (col, row) = ((cx as usize).min(5), (cy as usize).min(3));
        let (gx, gy) = (cx.fract(), cy.fract());
        if !(0.08..=0.92).contains(&gx) || !(0.08..=0.92).contains(&gy) {
            return SURROUND;
        }
        let lut = decode_lut();
        return CHART[row * 6 + col].map(|c| lut[usize::from(c)]);
    }
    let step = (fx * 11.0).floor().min(10.0) / 10.0;
    [srgb_to_linear(step); 3]
}

/// Apply exposure, warmth, and saturation in linear light.
fn adjust(c: [f32; 3], gain: f32, wb: [f32; 3], sat: f32) -> [f32; 3] {
    let [r, g, b] = [
        c[0] * gain * wb[0],
        c[1] * gain * wb[1],
        c[2] * gain * wb[2],
    ];
    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    [
        luma + (r - luma) * sat,
        luma + (g - luma) * sat,
        luma + (b - luma) * sat,
    ]
}

/// Linear sRGB → linear Display P3 (both D65), rows of the 3×3 matrix.
const SRGB_TO_P3: [[f32; 3]; 3] = [
    [0.822_462_1, 0.177_538, 0.0],
    [0.033_194_2, 0.966_805_8, 0.0],
    [0.017_082_7, 0.072_397_4, 0.910_519_9],
];

fn to_p3(c: [f32; 3]) -> [f32; 3] {
    SRGB_TO_P3.map(|r| r[0] * c[0] + r[1] * c[1] + r[2] * c[2])
}

/// IEEE 754 binary16 bits for `v`, round-to-nearest-even; overflow → ±inf,
/// NaN stays NaN, tiny values become subnormals or ±0.
pub(crate) fn f32_to_f16(v: f32) -> u16 {
    fn round(half: u32, rem: u32, halfway: u32) -> u32 {
        if rem > halfway || (rem == halfway && half & 1 == 1) {
            half + 1
        } else {
            half
        }
    }
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let mant = x & 0x007f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant == 0 { 0 } else { 0x0200 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = mant | 0x0080_0000;
        let shift = (14 - e) as u32;
        let half = round(m >> shift, m & ((1 << shift) - 1), 1 << (shift - 1));
        return sign | half as u16;
    }
    // A carry out of the mantissa correctly bumps the exponent (up to inf).
    let half = round(((e as u32) << 10) | (mant >> 13), mant & 0x1fff, 0x1000);
    sign | half as u16
}

/// Render the pattern into `dest` with the layout in `info`.
///
/// `cancelled` is polled before every band of [`BAND_ROWS`] rows; on
/// cancellation the function returns `Err(Cancelled)` having written only the
/// bands already done. `on_band(done, total)` runs after each band, in order,
/// on the calling thread.
pub(crate) fn render_pattern_into(
    info: &FrameInfo,
    params: TestPatternParams,
    dest: &mut [u8],
    cancelled: &dyn Fn() -> bool,
    on_band: &mut dyn FnMut(u64, u64),
) -> Result<(), DsError> {
    params.validate()?;
    let bpp = info.format.bytes_per_pixel() as usize;
    let row_bytes = info.bytes_per_row as usize;
    let needed = info.byte_len();
    if info.width == 0 || row_bytes < info.width as usize * bpp || (dest.len() as u64) < needed {
        return Err(DsError::invalid(format!(
            "destination {} bytes / {row_bytes} per row too small for {}x{} {:?}",
            dest.len(),
            info.width,
            info.height,
            info.format
        )));
    }
    let gain = params.exposure_ev.exp2();
    let wb = [1.0 + 0.25 * params.warmth, 1.0, 1.0 - 0.25 * params.warmth];
    let sat = 1.0 + params.saturation;
    let (fw, fh) = (info.full_width as f32, info.full_height as f32);
    let bands = u64::from(info.height.div_ceil(BAND_ROWS));
    let dest = &mut dest[..needed as usize];
    for (band_idx, band) in dest.chunks_mut(row_bytes * BAND_ROWS as usize).enumerate() {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        let y0 = info.origin_y + band_idx as u32 * BAND_ROWS;
        band.par_chunks_mut(row_bytes)
            .enumerate()
            .for_each(|(dy, row)| {
                let fy = ((y0 + dy as u32) as f32 + 0.5) / fh;
                let row = &mut row[..info.width as usize * bpp];
                let fx = |x: usize| ((info.origin_x + x as u32) as f32 + 0.5) / fw;
                match info.format {
                    FrameFormat::Rgba8Srgb => {
                        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                            let c = adjust(base_color(fx(x), fy), gain, wb, sat);
                            *px = [encode(c[0]), encode(c[1]), encode(c[2]), 255];
                        }
                    }
                    FrameFormat::Rgba16fLinearP3 => {
                        for (x, px) in row.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                            let c = to_p3(adjust(base_color(fx(x), fy), gain, wb, sat));
                            let h = [c[0], c[1], c[2], 1.0]
                                .map(|v| f32_to_f16(v.max(0.0)).to_le_bytes());
                            *px = [
                                h[0][0], h[0][1], h[1][0], h[1][1], h[2][0], h[2][1], h[3][0],
                                h[3][1],
                            ];
                        }
                    }
                }
            });
        on_band(band_idx as u64 + 1, bands);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::frame::{NormRect, RenderRequest, plan};

    /// A rendered frame in a Vec, for assertions.
    pub(crate) struct Img {
        pub info: FrameInfo,
        pub pixels: Vec<u8>,
    }

    impl Img {
        pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
            let i = (y * self.info.bytes_per_row + x * 4) as usize;
            [
                self.pixels[i],
                self.pixels[i + 1],
                self.pixels[i + 2],
                self.pixels[i + 3],
            ]
        }

        pub fn half(&self, x: u32, y: u32) -> [f32; 4] {
            let i = (y * self.info.bytes_per_row + x * 8) as usize;
            std::array::from_fn(|c| {
                f16_to_f32(u16::from_le_bytes([
                    self.pixels[i + 2 * c],
                    self.pixels[i + 2 * c + 1],
                ]))
            })
        }

        fn rgb_rows(&self) -> impl Iterator<Item = &[u8]> {
            self.pixels
                .chunks(self.info.bytes_per_row as usize)
                .map(|r| &r[..self.info.width as usize * 4])
        }
    }

    pub(crate) fn render_req(w: u32, h: u32, req: RenderRequest, p: TestPatternParams) -> Img {
        let info = plan(w, h, &req).unwrap();
        let mut pixels = vec![0u8; info.byte_len() as usize];
        render_pattern_into(&info, p, &mut pixels, &|| false, &mut |_, _| {}).unwrap();
        Img { info, pixels }
    }

    pub(crate) fn fit(edge: u32, format: FrameFormat) -> RenderRequest {
        RenderRequest {
            max_long_edge: edge,
            scale: None,
            region: None,
            format,
        }
    }

    fn render(w: u32, h: u32, p: TestPatternParams) -> Img {
        render_req(w, h, fit(w.max(h), FrameFormat::Rgba8Srgb), p)
    }

    /// Reference decoder for the tests.
    fn f16_to_f32(h: u16) -> f32 {
        let sign = if h & 0x8000 == 0 { 1.0 } else { -1.0 };
        let e = i32::from((h >> 10) & 0x1f);
        let m = f32::from(h & 0x3ff);
        match e {
            0 => sign * m * 2f32.powi(-24),
            31 if m == 0.0 => sign * f32::INFINITY,
            31 => f32::NAN,
            _ => sign * (1.0 + m / 1024.0) * 2f32.powi(e - 15),
        }
    }

    #[test]
    fn output_has_expected_shape_and_opaque_alpha() {
        let img = render(300, 200, TestPatternParams::default());
        assert_eq!((img.info.width, img.info.height), (300, 200));
        assert!(
            img.rgb_rows()
                .all(|r| r.as_chunks::<4>().0.iter().all(|p| p[3] == 255))
        );
    }

    #[test]
    fn identity_params_reproduce_chart_patches_exactly() {
        let (w, h) = (600, 1000);
        let img = render(w, h, TestPatternParams::default());
        let at = |col: f32, row: f32| {
            let x = ((col + 0.5) / 6.0 * w as f32) as u32;
            let y = ((0.45 + (row + 0.5) / 4.0 * 0.40) * h as f32) as u32;
            img.pixel(x, y)
        };
        assert_eq!(at(2.0, 2.0), [175, 54, 60, 255]);
        assert_eq!(at(0.0, 3.0), [243, 243, 242, 255]);
    }

    #[test]
    fn tile_matches_the_same_pixels_of_a_full_render() {
        let full = render(600, 400, TestPatternParams::default());
        let req = RenderRequest {
            scale: Some(1.0),
            region: Some(NormRect {
                x: 0.25,
                y: 0.5,
                w: 0.25,
                h: 0.25,
            }),
            ..fit(1, FrameFormat::Rgba8Srgb)
        };
        let tile = render_req(600, 400, req, TestPatternParams::default());
        assert_eq!((tile.info.origin_x, tile.info.origin_y), (150, 200));
        for (x, y) in [(0, 0), (37, 12), (149, 99)] {
            assert_eq!(tile.pixel(x, y), full.pixel(150 + x, 200 + y), "at {x},{y}");
        }
    }

    #[test]
    fn half_float_output_is_linear_p3_with_edr_headroom() {
        let (w, h) = (1100, 100);
        let lin = render_req(
            w,
            h,
            fit(w, FrameFormat::Rgba16fLinearP3),
            TestPatternParams::default(),
        );
        let white = lin.half(w - 5, h - 2);
        for c in white.iter().take(3) {
            assert!((c - 1.0).abs() < 2e-3, "white {white:?}");
        }
        assert!((white[3] - 1.0).abs() < f32::EPSILON);
        let black = lin.half(5, h - 2);
        assert!(black[..3].iter().all(|&v| v == 0.0), "{black:?}");
        let up = TestPatternParams {
            exposure_ev: 2.0,
            ..Default::default()
        };
        let bright = render_req(w, h, fit(w, FrameFormat::Rgba16fLinearP3), up);
        let edr = bright.half(w - 5, h - 2);
        assert!((edr[0] - 4.0).abs() < 1e-2, "EDR white = 4.0: {edr:?}");
        // Pure sRGB red maps inside P3: less red, some green/blue.
        let red = to_p3([1.0, 0.0, 0.0]);
        assert!(red[0] < 1.0 && red[1] > 0.0 && red[2] > 0.0);
    }

    #[test]
    fn f16_conversion_matches_reference() {
        let cases: [(f32, u16); 11] = [
            (0.0, 0x0000),
            (-0.0, 0x8000),
            (1.0, 0x3c00),
            (0.5, 0x3800),
            (-2.0, 0xc000),
            (65504.0, 0x7bff),
            (1e6, 0x7c00),
            (f32::INFINITY, 0x7c00),
            (5.960_464_5e-8, 0x0001),
            (1e-9, 0x0000),
            (6.103_515_6e-5, 0x0400),
        ];
        for (v, bits) in cases {
            assert_eq!(f32_to_f16(v), bits, "{v}");
        }
        assert_eq!(f32_to_f16(f32::NAN) & 0x7e00, 0x7e00);
        for i in 0..2000 {
            let v = (i as f32 - 1000.0) * 0.731;
            let back = f16_to_f32(f32_to_f16(v));
            assert!((back - v).abs() <= v.abs() * 1e-3 + 1e-4, "{v} -> {back}");
        }
        // Ties round to even.
        assert_eq!(f32_to_f16(1.0 + 2f32.powi(-11)), 0x3c00);
        assert_eq!(f32_to_f16(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
        assert!(f16_to_f32(0x7e00).is_nan());
        assert!(f16_to_f32(0xfc00).is_infinite());
    }

    #[test]
    fn gray_wedge_spans_black_to_white() {
        let (w, h) = (1100, 100);
        let img = render(w, h, TestPatternParams::default());
        let y = h - 2;
        assert_eq!(img.pixel(5, y), [0, 0, 0, 255]);
        assert_eq!(img.pixel(w - 5, y), [255, 255, 255, 255]);
        let mid = img.pixel(w / 2, y);
        assert!(
            mid[0] == mid[1] && mid[1] == mid[2],
            "wedge must be neutral: {mid:?}"
        );
        assert!(
            (120..=135).contains(&mid[0]),
            "50% step should be ~128: {mid:?}"
        );
    }

    #[test]
    fn exposure_brightens_and_darkens() {
        let sum = |i: &Img| i.rgb_rows().flatten().map(|&v| u64::from(v)).sum::<u64>();
        let base = render(64, 64, TestPatternParams::default());
        let up = render(
            64,
            64,
            TestPatternParams {
                exposure_ev: 1.0,
                ..Default::default()
            },
        );
        let down = render(
            64,
            64,
            TestPatternParams {
                exposure_ev: -1.0,
                ..Default::default()
            },
        );
        assert!(sum(&up) > sum(&base));
        assert!(sum(&down) < sum(&base));
    }

    #[test]
    fn warmth_shifts_red_blue_balance_on_neutrals() {
        let warm = render(
            1100,
            100,
            TestPatternParams {
                warmth: 1.0,
                ..Default::default()
            },
        );
        let cool = render(
            1100,
            100,
            TestPatternParams {
                warmth: -1.0,
                ..Default::default()
            },
        );
        let (pw, pc) = (warm.pixel(550, 98), cool.pixel(550, 98));
        assert!(pw[0] > pw[2], "warm: red > blue {pw:?}");
        assert!(pc[2] > pc[0], "cool: blue > red {pc:?}");
    }

    #[test]
    fn full_desaturation_makes_everything_neutral() {
        let img = render(
            120,
            120,
            TestPatternParams {
                saturation: -1.0,
                ..Default::default()
            },
        );
        for r in img.rgb_rows() {
            for p in r.as_chunks::<4>().0 {
                let (lo, hi) = (p[0].min(p[1]).min(p[2]), p[0].max(p[1]).max(p[2]));
                assert!(hi - lo <= 1, "not neutral: {p:?}");
            }
        }
    }

    #[test]
    fn rejects_bad_params_and_short_buffers() {
        let info = plan(10, 10, &fit(10, FrameFormat::Rgba8Srgb)).unwrap();
        let mut buf = vec![0u8; info.byte_len() as usize];
        let run = |p, buf: &mut [u8]| render_pattern_into(&info, p, buf, &|| false, &mut |_, _| {});
        let d = TestPatternParams::default();
        for bad in [
            TestPatternParams {
                exposure_ev: 6.0,
                ..d
            },
            TestPatternParams {
                warmth: f32::NAN,
                ..d
            },
            TestPatternParams {
                saturation: -1.5,
                ..d
            },
        ] {
            assert!(matches!(
                run(bad, &mut buf),
                Err(DsError::InvalidArgument { .. })
            ));
        }
        assert!(matches!(
            run(d, &mut buf[..10]),
            Err(DsError::InvalidArgument { .. })
        ));
        let narrow = FrameInfo {
            bytes_per_row: 8,
            ..info
        };
        let r = render_pattern_into(&narrow, d, &mut buf, &|| false, &mut |_, _| {});
        assert!(matches!(r, Err(DsError::InvalidArgument { .. })));
        assert!(run(d, &mut buf).is_ok());
    }

    #[test]
    fn reports_every_band_in_order() {
        let h = BAND_ROWS * 2 + 1;
        let info = plan(8, h, &fit(h, FrameFormat::Rgba8Srgb)).unwrap();
        let mut buf = vec![0u8; info.byte_len() as usize];
        let mut seen = Vec::new();
        render_pattern_into(
            &info,
            TestPatternParams::default(),
            &mut buf,
            &|| false,
            &mut |d, t| seen.push((d, t)),
        )
        .unwrap();
        assert_eq!(seen, vec![(1, 3), (2, 3), (3, 3)]);
    }

    #[test]
    fn cancellation_stops_between_bands_leaving_later_rows_untouched() {
        let h = BAND_ROWS * 4;
        let info = plan(8, h, &fit(h, FrameFormat::Rgba8Srgb)).unwrap();
        let mut buf = vec![0u8; info.byte_len() as usize];
        let calls = std::cell::Cell::new(0);
        let cancelled = || {
            calls.set(calls.get() + 1);
            calls.get() > 1
        };
        let r = render_pattern_into(
            &info,
            TestPatternParams::default(),
            &mut buf,
            &cancelled,
            &mut |_, _| {},
        );
        assert_eq!(r, Err(DsError::Cancelled));
        assert_eq!(calls.get(), 2);
        let band = (info.bytes_per_row * BAND_ROWS) as usize;
        assert!(buf[..band].iter().any(|&b| b != 0));
        assert!(
            buf[band..].iter().all(|&b| b == 0),
            "only the first band was written"
        );
    }

    #[test]
    fn transfer_functions_round_trip() {
        for i in 0..=255u8 {
            let lin = decode_lut()[usize::from(i)];
            assert_eq!(encode(lin), i, "round trip failed at {i}");
        }
        assert_eq!(encode(-1.0), 0);
        assert_eq!(encode(2.0), 255);
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            assert!((decode_fast(v) - srgb_to_linear(v)).abs() < 2e-3, "{v}");
        }
    }

    #[test]
    fn hsv_covers_all_sectors() {
        let expect = [
            (0.0, [1.0, 0.0, 0.0]),
            (1.0 / 6.0, [1.0, 1.0, 0.0]),
            (2.0 / 6.0, [0.0, 1.0, 0.0]),
            (3.0 / 6.0, [0.0, 1.0, 1.0]),
            (4.0 / 6.0, [0.0, 0.0, 1.0]),
            (5.0 / 6.0, [1.0, 0.0, 1.0]),
        ];
        for (h, rgb) in expect {
            let got = hsv(h + 1e-6, 1.0, 1.0);
            for c in 0..3 {
                assert!((got[c] - rgb[c]).abs() < 1e-3, "h={h} got {got:?}");
            }
        }
    }
}
