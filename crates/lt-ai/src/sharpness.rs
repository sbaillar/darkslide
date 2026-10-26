//! Sharpness: contrast-normalized Laplacian energy per tile, a focus region,
//! an optional subject region, group-relative comparison, and a motion-blur
//! detector.
//!
//! **Per-tile measure.** For each tile, `s = max(0, var(Laplacian) − floor) /
//! (var(luma) + eps)`: high-frequency energy relative to local contrast.
//! Dividing by local contrast makes it invariant to exposure and flicker (a
//! −2 EV frame scales both terms by the same factor), and `floor` is the
//! Laplacian variance measured on the frame's flattest tiles, so noise —
//! including the spatially correlated noise of JPEG previews — is removed
//! empirically rather than through a white-noise model.
//!
//! **Where to look.** A sharp background behind a soft subject is a missed
//! focus, not a sharp photo, and from one frame alone the two can't be told
//! apart. So, in order of preference:
//! 1. a **subject region** from the caller ([`SubjectRegion`]: Vision faces,
//!    saliency, or the camera's AF point) — measured on exactly those tiles;
//! 2. within a group, [`crate::focus::group_focus`] compares the **same
//!    tiles** across frames (see there for when it declines to judge);
//! 3. for a lone frame, the **focus region**: the sharpest `focus_fraction`
//!    of detailed tiles.
//!
//! **Motion blur.** Blur along a direction removes gradients along it. Over
//! the measured tiles we sum the unit-trace Sobel structure tensors: scene
//! edges point every which way and average out, a blur direction survives.
//! `directional_ratio` is the minor/major eigenvalue ratio (1 = isotropic).
//! Line-heavy scenes are anisotropic too, so the cull score only trusts it
//! relative to the group (see `cull`).

use lt_core::NormRect;
use serde::{Deserialize, Serialize};

use crate::LumaImage;
use crate::tiles::{TileGrid, TileStats};

/// Tunables for the tile pass and region selection.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SharpnessConfig {
    /// Tile edge in pixels (clamped to 4..=256). 48 px on a 1616×1080
    /// preview gives 34×23 tiles: small enough to isolate a subject.
    pub tile: usize,
    /// Fraction of detailed tiles forming the lone-frame focus region.
    pub focus_fraction: f32,
    /// Local luma variance (DN²) below which a tile counts as featureless.
    pub min_detail_var: f32,
    /// Added to the local variance in the per-tile ratio so near-flat tiles
    /// do not divide by ~0 (DN²).
    pub contrast_eps: f32,
}

impl Default for SharpnessConfig {
    fn default() -> Self {
        Self {
            tile: 48,
            focus_fraction: 0.08,
            min_detail_var: 16.0,
            contrast_eps: 4.0,
        }
    }
}

/// Where the subject is: the camera's AF point, a Vision face or saliency
/// rect. Normalized, display-upright coordinates. Build the list with
/// [`crate::focus::SubjectHints`], which applies the priority order.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SubjectRegion {
    pub rect: NormRect,
    /// Relative importance. Non-finite or non-positive weights are ignored.
    pub weight: f32,
}

impl SubjectRegion {
    /// A square-ish region of half-size `radius` (normalized) around the
    /// camera's AF point (`ImageMeta::af_point` from lt-raw), clamped to the
    /// frame.
    pub fn from_af_point(point: [f32; 2], radius: f32) -> SubjectRegion {
        let r = radius.abs();
        // `clamp` keeps NaN (an invalid point stays an invalid region).
        let (x0, y0) = (
            (point[0] - r).clamp(0.0, 1.0),
            (point[1] - r).clamp(0.0, 1.0),
        );
        let (x1, y1) = (
            (point[0] + r).clamp(0.0, 1.0),
            (point[1] + r).clamp(0.0, 1.0),
        );
        SubjectRegion {
            rect: NormRect {
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
            },
            weight: 1.0,
        }
    }

    fn is_valid(&self) -> bool {
        let r = self.rect;
        [r.x, r.y, r.w, r.h, self.weight]
            .iter()
            .all(|v| v.is_finite())
            && self.weight > 0.0
            && r.w > 0.0
            && r.h > 0.0
            && r.x < 1.0
            && r.y < 1.0
            && r.x + r.w > 0.0
            && r.y + r.h > 0.0
    }
}

/// Most subject regions measured per frame; any beyond (in order) are
/// ignored. Each costs a pass over its pixels, and a caller has no use for
/// more than a handful.
pub const MAX_SUBJECT_REGIONS: usize = 16;

/// Per-tile values kept for group comparison.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TileMap {
    pub cols: usize,
    pub rows: usize,
    /// Tile edge and frame size in pixels, to map regions onto tiles.
    pub tile: usize,
    pub width: usize,
    pub height: usize,
    /// Contrast-normalized, noise-corrected sharpness per tile.
    pub sharpness: Vec<f32>,
    /// Local contrast relative to the frame's most contrasty tile, 0..=1.
    pub detail: Vec<f32>,
    /// Unit-trace Sobel structure tensor per tile `[xx, yy, xy]`, so a group
    /// can measure blur direction on the *same* tiles in every frame.
    pub orientation: Vec<[f32; 3]>,
}

/// Sharpness measured on the caller's subject region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubjectSharpness {
    pub sharpness: f32,
    pub directional_ratio: f32,
    pub blur_angle_deg: f32,
    /// Per valid region (aligned with [`Sharpness::regions`]); `None` where
    /// the region was featureless. Lets a group compare region by region.
    pub per_region: Vec<Option<f32>>,
}

/// Sharpness metrics for one image. See the module docs.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Sharpness {
    /// Raw variance of the 4-neighbour Laplacian over the whole frame (DN²).
    pub laplacian_var: f32,
    /// Raw mean squared Sobel magnitude over the whole frame (DN²).
    pub tenengrad: f32,
    /// Contrast-normalized sharpness of the focus region.
    pub focus: f32,
    /// Minor/major eigenvalue ratio on the focus region.
    pub directional_ratio: f32,
    /// Direction of blur streaks, degrees `0..180`, 0 = horizontal.
    pub blur_angle_deg: f32,
    /// Normalized centre `(x, y)` of the focus region.
    pub focus_center: [f32; 2],
    /// Present when the caller supplied a subject region that covers detail.
    pub subject: Option<SubjectSharpness>,
    /// The valid subject regions this frame was given (kept so a group can
    /// lend them to frames where Vision found nothing).
    pub regions: Vec<SubjectRegion>,

    pub tiles: TileMap,
}

impl Sharpness {
    /// The headline value: the subject's sharpness when known, else the
    /// focus region's.
    pub fn value(&self) -> f32 {
        self.subject.as_ref().map_or(self.focus, |s| s.sharpness)
    }

    /// `(directional_ratio, blur_angle_deg)` where the subject is, so a
    /// panning shot (sharp subject, streaked background) is not motion blur.
    pub fn motion(&self) -> (f32, f32) {
        self.subject
            .as_ref()
            .map_or((self.directional_ratio, self.blur_angle_deg), |s| {
                (s.directional_ratio, s.blur_angle_deg)
            })
    }
}

/// Weighted sums over a set of tiles: mean sharpness, tensor, centre.
#[derive(Default)]
struct Acc {
    w: f64,
    s: f64,
    xx: f64,
    yy: f64,
    xy: f64,
    cx: f64,
    cy: f64,
}

impl Acc {
    fn add(&mut self, st: &TileStats, s: f32, w: f64, centre: (f64, f64)) {
        self.w += w;
        self.s += w * f64::from(s);
        let trace = (st.gxx + st.gyy) as f64;
        if trace > 0.0 {
            self.xx += w * st.gxx as f64 / trace;
            self.yy += w * st.gyy as f64 / trace;
            self.xy += w * st.gxy as f64 / trace;
        }
        self.cx += w * centre.0;
        self.cy += w * centre.1;
    }

    fn mean(&self) -> f32 {
        if self.w > 0.0 {
            (self.s / self.w) as f32
        } else {
            0.0
        }
    }
}

/// The pixel rectangles (`x0, y0, x1, y1`, exclusive end) of the valid
/// subject regions of a `width × height` frame, in order.
pub(crate) fn subject_rects(
    subject: &[SubjectRegion],
    width: usize,
    height: usize,
) -> Vec<(usize, usize, usize, usize)> {
    let (w, h) = (width as f64, height as f64);
    let px = |v: f32, size: f64| (f64::from(v) * size).round().clamp(0.0, size) as usize;
    subject
        .iter()
        .filter(|r| r.is_valid())
        .take(MAX_SUBJECT_REGIONS)
        .map(|r| {
            (
                px(r.rect.x, w),
                px(r.rect.y, h),
                px(r.rect.x + r.rect.w, w),
                px(r.rect.y + r.rect.h, h),
            )
        })
        .collect()
}

pub(crate) fn measure(
    img: &LumaImage,
    grid: &TileGrid,
    cfg: &SharpnessConfig,
    noise_floor: f32,
    subject: &[SubjectRegion],
) -> Sharpness {
    let (width, height) = (img.width(), img.height());
    let stats: Vec<TileStats> = subject_rects(subject, width, height)
        .into_iter()
        .map(|(x0, y0, x1, y1)| crate::tiles::region_stats(img, x0, y0, x1, y1))
        .collect();
    measure_with(grid, width, height, cfg, noise_floor, subject, &stats)
}

/// [`measure`] from precomputed stencil sums of the valid subject regions
/// (`subject_stats[i]` for the `i`-th rectangle of [`subject_rects`]), so
/// the GPU batch path runs the same code on sums it computed on the device.
pub(crate) fn measure_with(
    grid: &TileGrid,
    width: usize,
    height: usize,
    cfg: &SharpnessConfig,
    noise_floor: f32,
    subject: &[SubjectRegion],
    subject_stats: &[TileStats],
) -> Sharpness {
    let eps = f64::from(cfg.contrast_eps.max(1e-3));
    let floor = f64::from(noise_floor.max(0.0));
    let vars: Vec<f64> = grid.stats.iter().map(TileStats::luma_var).collect();
    let max_var = vars.iter().copied().fold(0.0, f64::max);
    let sharp: Vec<f32> = grid
        .stats
        .iter()
        .zip(&vars)
        .map(|(st, &v)| ((st.laplacian_var() - floor).max(0.0) / (v + eps)) as f32)
        .collect();
    let detail: Vec<f32> = vars
        .iter()
        .map(|&v| {
            if max_var > 0.0 {
                (v / max_var) as f32
            } else {
                0.0
            }
        })
        .collect();
    let centre = |i: usize| {
        let (x0, y0, x1, y1) = grid.bounds(i, width, height);
        (
            (x0 + x1) as f64 / 2.0 / width as f64,
            (y0 + y1) as f64 / 2.0 / height as f64,
        )
    };

    // Lone-frame focus region: the sharpest detailed tiles.
    let min_var = f64::from(cfg.min_detail_var);
    let mut cand: Vec<usize> = (0..sharp.len())
        .filter(|&i| grid.stats[i].n > 0 && vars[i] >= min_var)
        .collect();
    let frac = if cfg.focus_fraction.is_finite() {
        cfg.focus_fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let k = ((cand.len() as f32 * frac).ceil() as usize)
        .max(1)
        .min(cand.len());
    cand.sort_by(|&a, &b| sharp[b].total_cmp(&sharp[a]).then(a.cmp(&b)));
    let mut focus = Acc::default();
    for &i in &cand[..k] {
        focus.add(&grid.stats[i], sharp[i], 1.0, centre(i));
    }
    let (ratio, angle) = eigen_ratio(focus.xx, focus.yy, focus.xy);
    let focus_center = if focus.w > 0.0 {
        [(focus.cx / focus.w) as f32, (focus.cy / focus.w) as f32]
    } else {
        [0.5, 0.5]
    };

    let mut all = TileStats::default();
    grid.stats.iter().for_each(|s| all.add(s));
    Sharpness {
        laplacian_var: all.laplacian_var() as f32,
        tenengrad: all.tenengrad() as f32,
        focus: focus.mean(),
        directional_ratio: ratio,
        blur_angle_deg: angle,
        focus_center,
        subject: subject_sharpness(subject_stats, subject, floor, eps, min_var),
        regions: subject
            .iter()
            .copied()
            .filter(SubjectRegion::is_valid)
            .take(MAX_SUBJECT_REGIONS)
            .collect(),

        tiles: TileMap {
            cols: grid.cols,
            rows: grid.rows,
            tile: grid.tile,
            width,
            height,
            sharpness: sharp,
            detail,
            orientation: grid
                .stats
                .iter()
                .map(|s| {
                    let tr = (s.gxx + s.gyy) as f64;
                    if tr > 0.0 {
                        [
                            (s.gxx as f64 / tr) as f32,
                            (s.gyy as f64 / tr) as f32,
                            (s.gxy as f64 / tr) as f32,
                        ]
                    } else {
                        [0.0; 3]
                    }
                })
                .collect(),
        },
    }
}

/// Measured directly on each region's pixels (not on the coarse tiles, whose
/// sharpness is dominated by whatever sharp background they straddle), then
/// combined by region weight. `stats` holds each valid region's stencil sums.
fn subject_sharpness(
    stats: &[TileStats],
    regions: &[SubjectRegion],
    floor: f64,
    eps: f64,
    min_var: f64,
) -> Option<SubjectSharpness> {
    let mut acc = Acc::default();
    let mut per_region = Vec::new();
    for (r, st) in regions
        .iter()
        .filter(|r| r.is_valid())
        .take(MAX_SUBJECT_REGIONS)
        .zip(stats)
    {
        per_region.push(None);
        let var = st.luma_var();
        if st.n == 0 || var < min_var {
            continue; // off-frame, or featureless: says nothing about focus
        }
        let s = ((st.laplacian_var() - floor).max(0.0) / (var + eps)) as f32;
        acc.add(st, s, f64::from(r.weight), (0.0, 0.0));
        *per_region.last_mut().expect("pushed above") = Some(s);
    }
    (acc.w > 0.0).then(|| {
        let (directional_ratio, blur_angle_deg) = eigen_ratio(acc.xx, acc.yy, acc.xy);
        SubjectSharpness {
            sharpness: acc.mean(),
            directional_ratio,
            blur_angle_deg,
            per_region,
        }
    })
}

/// `(λmin/λmax, blur angle in degrees)` of a symmetric 2×2 tensor.
pub(crate) fn eigen_ratio(xx: f64, yy: f64, xy: f64) -> (f32, f32) {
    let tr = xx + yy;
    if tr <= 0.0 {
        return (1.0, 0.0);
    }
    let d = ((xx - yy).powi(2) + 4.0 * xy * xy).sqrt();
    let ratio = ((tr - d) / (tr + d)).clamp(0.0, 1.0);
    // Dominant gradient orientation; streaks run perpendicular to it.
    let grad = 0.5 * (2.0 * xy).atan2(xx - yy);
    let blur = (grad.to_degrees() + 90.0).rem_euclid(180.0);
    (ratio as f32, blur as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{blur, blur_region, img, scale, texture};
    use crate::tiles::tile_pass;
    use crate::{AnalysisConfig, analyze_with_subject};

    fn run(i: &LumaImage) -> Sharpness {
        crate::analyze(i, &AnalysisConfig::default()).sharpness
    }

    fn roi(x: f32, y: f32, w: f32, h: f32) -> SubjectRegion {
        SubjectRegion {
            rect: NormRect { x, y, w, h },
            weight: 1.0,
        }
    }

    #[test]
    fn eigen_ratio_cases() {
        assert_eq!(eigen_ratio(0.0, 0.0, 0.0), (1.0, 0.0));
        let (r, a) = eigen_ratio(1.0, 0.0, 0.0); // only x gradients: vertical streaks
        assert!(r.abs() < 1e-6 && (a - 90.0).abs() < 1e-4);
        let (r, a) = eigen_ratio(0.0, 1.0, 0.0);
        assert!(r.abs() < 1e-6 && a.abs() < 1e-4);
        let (r, _) = eigen_ratio(1.0, 1.0, 0.0);
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn blur_lowers_the_normalized_measure() {
        let t = texture(320, 240, 1);
        let v: Vec<f32> = [0, 1, 2, 4]
            .iter()
            .map(|&r| run(&blur(&t, r, r)).focus)
            .collect();
        assert!(v.windows(2).all(|w| w[0] > w[1]), "{v:?}");
    }

    #[test]
    fn exposure_changes_do_not_read_as_sharpness() {
        // A −2 EV frame (×0.25) or a flicker-brightened one keeps its score.
        let t = texture(320, 240, 2);
        let base = run(&t).focus;
        for k in [0.25, 0.5, 1.15] {
            let v = run(&scale(&t, k)).focus;
            assert!((v / base - 1.0).abs() < 0.2, "×{k}: {v} vs {base}");
        }
        assert!(run(&blur(&t, 1, 1)).focus < 0.6 * base);
    }

    #[test]
    fn panning_subject_is_isotropic_even_when_background_streaks() {
        let t = texture(480, 320, 4);
        let panned = blur_region(&t, 6, 0, Some([0, 0, 480, 100]));
        let panned = blur_region(&panned, 6, 0, Some([0, 220, 480, 320]));
        let r = [roi(0.0, 110.0 / 320.0, 1.0, 100.0 / 320.0)];
        let s = analyze_with_subject(&panned, &AnalysisConfig::default(), &r).sharpness;
        assert!(s.motion().0 > 0.5, "{s:?}");
        let smear = run(&blur(&t, 6, 0));
        assert!(smear.motion().0 < 0.15 && (smear.motion().1 < 15.0 || smear.motion().1 > 165.0));
    }

    #[test]
    fn invalid_or_featureless_subject_regions_are_ignored() {
        let t = texture(100, 100, 5);
        let cfg = AnalysisConfig::default();
        let bad = [
            SubjectRegion {
                rect: NormRect {
                    x: f32::NAN,
                    ..NormRect::FULL
                },
                weight: 1.0,
            },
            SubjectRegion {
                rect: NormRect::FULL,
                weight: 0.0,
            },
            SubjectRegion {
                rect: NormRect {
                    x: 2.0,
                    y: 2.0,
                    w: 0.5,
                    h: 0.5,
                },
                weight: 1.0,
            },
        ];
        assert_eq!(analyze_with_subject(&t, &cfg, &bad).sharpness.subject, None);
        let flat = img(100, 100, |_, _| 90);
        assert_eq!(
            analyze_with_subject(&flat, &cfg, &[roi(0.0, 0.0, 1.0, 1.0)])
                .sharpness
                .subject,
            None
        );
    }

    #[test]
    fn subject_regions_are_capped() {
        let t = texture(100, 100, 5);
        let many = vec![roi(0.0, 0.0, 1.0, 1.0); 100];
        let s = analyze_with_subject(&t, &AnalysisConfig::default(), &many).sharpness;
        assert_eq!(s.regions.len(), MAX_SUBJECT_REGIONS);
        let subject = s.subject.expect("the regions cover detail");
        assert_eq!(subject.per_region.len(), MAX_SUBJECT_REGIONS);
        assert_eq!(subject_rects(&many, 100, 100).len(), MAX_SUBJECT_REGIONS);
        // The same measure as the capped list given directly.
        let few = vec![roi(0.0, 0.0, 1.0, 1.0); MAX_SUBJECT_REGIONS];
        let f = analyze_with_subject(&t, &AnalysisConfig::default(), &few).sharpness;
        assert_eq!(f.subject.map(|x| x.sharpness), Some(subject.sharpness));
    }

    #[test]
    fn noise_floor_is_subtracted() {
        let t = texture(96, 96, 6);
        let g = tile_pass(&t, 48);
        let cfg = SharpnessConfig::default();
        let clean = measure(&t, &g, &cfg, 0.0, &[]).focus;
        let floored = measure(&t, &g, &cfg, 1e9, &[]).focus;
        assert!(clean > 0.0 && floored == 0.0);
    }

    #[test]
    fn degenerate_frames_are_neutral() {
        for im in [
            img(0, 0, |_, _| 0),
            img(1, 9, |_, _| 9),
            img(40, 30, |_, _| 0),
        ] {
            let s = run(&im);
            assert_eq!(
                (s.value(), s.directional_ratio, s.focus_center),
                (0.0, 1.0, [0.5, 0.5])
            );
        }
        let cfg = SharpnessConfig {
            focus_fraction: f32::NAN,
            ..SharpnessConfig::default()
        };
        let t = texture(96, 96, 7);
        let one = measure(&t, &tile_pass(&t, 48), &cfg, 0.0, &[]);
        assert!(one.focus > 0.0);
    }

    #[test]
    fn af_point_region_is_clamped_to_the_frame() {
        let r = SubjectRegion::from_af_point([0.95, 0.5], 0.1);
        assert!(
            (r.rect.x - 0.85).abs() < 1e-6 && (r.rect.w - 0.15).abs() < 1e-6,
            "{r:?}"
        );
        assert!(r.is_valid());
        assert!(!SubjectRegion::from_af_point([f32::NAN, 0.5], 0.1).is_valid());
    }
}
