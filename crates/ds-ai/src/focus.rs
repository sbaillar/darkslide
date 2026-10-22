//! Where the subject is, and whether a group can tell which frames missed
//! focus.
//!
//! **Subject priority** ([`SubjectHints::regions`]): the camera's AF point
//! (the photographer's intent) > the face under the AF point > the largest
//! prominent face > the most salient region.
//!
//! **Group verdict** ([`group_focus`]). Principle: *wrongly rejecting a good
//! frame is worse than keeping a bad one*, so a frame is only judged soft
//! against its group when that judgement is corroborated:
//!
//! 1. **Subject regions** (any frame has one, borrowed by frames that don't):
//!    the subject decides (on each frame's own pixels when every frame has
//!    a region, else on the same tiles under the region), with the
//!    whole-frame tile vote as a small tie-breaker. A region that is soft
//!    in *every* frame (a background face in the bokeh) is dropped.
//! 2. **Tile consensus** without a subject: per tile, each frame is compared
//!    with the group median, over the tiles that are sharp in most frames.
//!    Trusted only when at least three frames agree on where the sharp tiles
//!    are (mean Jaccard overlap with the consensus ≥ 0.5).
//! 3. Otherwise the group is **focus-uncertain**: frames are ranked by their
//!    own lone-frame measure and only a frame that is sharp *nowhere* (on no
//!    real share of tiles as sharp as the best frame there) is judged soft;
//!    the others stay within reach of the best. Two frames — one with
//!    a sharp subject on bokeh, one with the focus on the background — are
//!    indistinguishable without a subject region, and neither is rejected.

use ds_core::NormRect;
use serde::{Deserialize, Serialize};

use crate::sharpness::{Sharpness, SubjectRegion};
use crate::stats::median;

/// Half-size of the AF-point region, normalized (≈ 100 px on a 1616 px preview).
pub const AF_RADIUS: f32 = 0.06;
/// A face must cover this fraction of the frame to be the subject by itself.
pub const PROMINENT_FACE_AREA: f32 = 0.01;
/// Relative sharpness given to a frame that is sharp nowhere.
pub const NOWHERE_SHARP: f32 = 0.3;
/// A frame is sharp *somewhere* when at least this share of the detailed
/// tiles is within half of the group's best on that tile.
const SHARP_SHARE: f32 = 0.05;
/// In an uncertain group, frames sharp somewhere keep at least this relative
/// sharpness: ranked, never rejected on sharpness.
const UNCERTAIN_FLOOR: f32 = 0.65;

/// Raw subject cues for one frame, as the Swift side and ds-raw provide them.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SubjectHints {
    /// Camera AF point (normalized `x, y`), from ds-raw `ImageMeta::af_point`.
    pub af_point: Option<[f32; 2]>,
    /// Vision face rectangles.
    pub faces: Vec<NormRect>,
    /// Vision attention-saliency rectangles, most salient first.
    pub saliency: Vec<NormRect>,
}

fn usable(r: &NormRect) -> bool {
    [r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite()) && r.w > 0.0 && r.h > 0.0
}

fn contains(r: &NormRect, p: [f32; 2]) -> bool {
    (r.x..=r.x + r.w).contains(&p[0]) && (r.y..=r.y + r.h).contains(&p[1])
}

impl SubjectHints {
    /// The subject regions to measure, by priority (see the module docs).
    pub fn regions(&self) -> Vec<SubjectRegion> {
        let region = |rect: NormRect| SubjectRegion { rect, weight: 1.0 };
        if let Some(p) = self.af_point.filter(|p| p.iter().all(|v| v.is_finite())) {
            let mut v = vec![SubjectRegion::from_af_point(p, AF_RADIUS)];
            v.extend(
                self.faces
                    .iter()
                    .filter(|f| usable(f) && contains(f, p))
                    .take(1)
                    .map(|&f| region(f)),
            );
            return v;
        }
        let face = self
            .faces
            .iter()
            .filter(|f| usable(f) && f.w * f.h >= PROMINENT_FACE_AREA)
            .max_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)));
        match face.or_else(|| self.saliency.iter().find(|s| usable(s))) {
            Some(&r) => vec![region(r)],
            None => Vec::new(),
        }
    }
}

/// The group's verdict on each frame's sharpness.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GroupFocus {
    /// Sharpness relative to the group's best, `0..=1` (best = 1).
    pub relative: Vec<f32>,
    /// Whether a "soft" verdict on this frame is corroborated (may be a hard
    /// failure). See the module docs.
    pub confident: Vec<bool>,
    /// No subject region and no tile consensus: ranked by lone-frame measure.
    pub uncertain: bool,
    /// Subject regions took part in the verdict.
    pub used_subject: bool,
    /// Each frame's own (lone-frame) sharpness relative to the group's best,
    /// whatever the verdict: evidence that does not depend on *where* focus
    /// is (motion blur) uses it.
    pub own: Vec<f32>,
    /// `[directional ratio, blur angle]` per frame, measured on the same tiles
    /// in every frame (the subject's when known), so content differences
    /// between frames — a subject moving across a line-heavy background — do
    /// not read as motion blur.
    pub motion: Vec<[f32; 2]>,
}

/// Keeps log-ratios finite where a tile's sharpness is ~0.
const DELTA: f32 = 0.01;
/// A region whose sharpness stays under this in *every* frame is never where
/// focus was (a face in the bokeh). Contrast-normalized units: A9 subjects
/// in focus measure 0.05–1.1, the same under a 9×9 blur ~0.01.
const ROI_SOFT: f32 = 0.015;
/// Weight of the whole-frame tile vote next to the subject vote: a
/// tie-breaker only (a panning shot's streaked background must not count).
const TILE_TIEBREAK: f32 = 0.05;
/// Tiles featureless in the typical frame (sky, wall) cannot show focus.
const MIN_DETAIL: f32 = 0.02;

fn finite_pos(v: f32) -> f32 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

fn ratio_to_best(vals: &[f32]) -> Vec<f32> {
    let max = vals.iter().copied().fold(0.0f32, f32::max);
    vals.iter()
        .map(|&v| if max > 0.0 { v / max } else { 0.0 })
        .collect()
}

fn from_logs(logs: &[f32]) -> Vec<f32> {
    let best = logs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    logs.iter().map(|&l| (l - best).exp()).collect()
}

/// Squared overlap fraction of tile `t` with the regions, times their weight:
/// interior tiles dominate, tiles straddling the region edge barely count.
fn region_weight(tiles: &crate::sharpness::TileMap, t: usize, regions: &[SubjectRegion]) -> f32 {
    let (tx, ty) = (t % tiles.cols, t / tiles.cols);
    let (w, h) = (tiles.width as f32, tiles.height as f32);
    let x0 = (tx * tiles.tile) as f32 / w;
    let y0 = (ty * tiles.tile) as f32 / h;
    let x1 = (((tx + 1) * tiles.tile).min(tiles.width)) as f32 / w;
    let y1 = (((ty + 1) * tiles.tile).min(tiles.height)) as f32 / h;
    let area = (x1 - x0) * (y1 - y0);
    regions
        .iter()
        .map(|r| {
            let ox = ((r.rect.x + r.rect.w).min(x1) - r.rect.x.max(x0)).max(0.0);
            let oy = ((r.rect.y + r.rect.h).min(y1) - r.rect.y.max(y0)).max(0.0);
            let f = if area > 0.0 { ox * oy / area } else { 0.0 };
            r.weight * f * f
        })
        .sum()
}

/// Directional ratio and angle of `frame` over weighted tiles.
fn motion_on(frame: &Sharpness, tiles: &[(usize, f32)]) -> [f32; 2] {
    let (mut xx, mut yy, mut xy) = (0.0f64, 0.0f64, 0.0f64);
    for &(t, w) in tiles {
        if let Some(o) = frame.tiles.orientation.get(t) {
            xx += f64::from(w * o[0]);
            yy += f64::from(w * o[1]);
            xy += f64::from(w * o[2]);
        }
    }
    let (r, a) = crate::sharpness::eigen_ratio(xx, yy, xy);
    [r, a]
}

/// Tiles with weights.
type WeightedTiles = Vec<(usize, f32)>;

/// Outcome of comparing subject regions on each frame's own pixels.
enum PixelVote {
    /// Not every frame measured the same regions.
    NotApplicable,
    /// Every region is soft in every frame.
    AllSoft,
    Logs(Vec<Option<f32>>),
}

/// Frames sharing one tile grid, with per-tile group statistics.
struct Grid<'a> {
    frames: &'a [&'a Sharpness],
    tiles: usize,
    /// Tiles with detail in the typical frame (sky and walls cannot show focus).
    eligible: Vec<usize>,
    /// Median sharpness per tile across frames.
    med: Vec<f32>,
}

impl<'a> Grid<'a> {
    fn new(frames: &'a [&'a Sharpness], tiles: usize) -> Self {
        let mut g = Grid {
            frames,
            tiles,
            eligible: Vec::new(),
            med: vec![0.0; tiles],
        };
        g.eligible = (0..tiles)
            .filter(|&t| {
                median(frames.iter().map(|f| f.tiles.detail[t]).collect())
                    .is_some_and(|m| m >= MIN_DETAIL)
            })
            .collect();
        for k in 0..g.eligible.len() {
            let t = g.eligible[k];
            g.med[t] = median((0..frames.len()).map(|i| g.s(i, t)).collect()).unwrap_or(0.0);
        }
        g
    }

    fn n(&self) -> usize {
        self.frames.len()
    }

    fn s(&self, i: usize, t: usize) -> f32 {
        finite_pos(self.frames[i].tiles.sharpness[t])
    }

    fn log_ratio(&self, i: usize, t: usize) -> f32 {
        ((self.s(i, t) + DELTA) / (self.med[t] + DELTA))
            .ln()
            .clamp(-4.0, 4.0)
    }

    /// Whole-frame vote over the tiles sharp in most frames: `(tiles, log per frame)`.
    fn tile_vote(&self) -> (Vec<usize>, Vec<f32>) {
        let mut voted = self.eligible.clone();
        voted.sort_by(|&a, &b| self.med[b].total_cmp(&self.med[a]).then(a.cmp(&b)));
        let cut = voted.first().map_or(0.0, |&t| self.med[t] * 0.25);
        voted.truncate(voted.len().div_ceil(4));
        voted.retain(|&t| self.med[t] >= cut && self.med[t] > 0.0);
        let logs = (0..self.n())
            .map(|i| {
                if voted.is_empty() {
                    0.0
                } else {
                    voted.iter().map(|&t| self.log_ratio(i, t)).sum::<f32>() / voted.len() as f32
                }
            })
            .collect();
        (voted, logs)
    }

    /// Subject vote: per frame its log score (if any) and the tiles used.
    /// `None` when no frame has a region, or the regions are soft everywhere.
    fn subject_vote(&self) -> Option<(Vec<Option<f32>>, Vec<WeightedTiles>)> {
        let n = self.n();
        // Frames without regions borrow the nearest frame's.
        let lender = |i: usize| {
            (0..n)
                .filter(|&j| !self.frames[j].regions.is_empty())
                .min_by_key(|&j| (j.abs_diff(i), j))
                .map(|j| self.frames[j].regions.as_slice())
        };
        let mut logs: Vec<Option<f32>> = vec![None; n];
        let mut used: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
        let mut soft = true;
        for i in 0..n {
            let Some(regions) = lender(i) else { continue };
            // Tiles mostly inside the region; all overlapping ones for a small region.
            let weights: Vec<(usize, f32)> = self
                .eligible
                .iter()
                .map(|&t| (t, region_weight(&self.frames[i].tiles, t, regions)))
                .filter(|w| w.1 > 0.0)
                .collect();
            let inner: Vec<(usize, f32)> =
                weights.iter().copied().filter(|w| w.1 >= 0.25).collect();
            used[i] = if inner.is_empty() { weights } else { inner };
            let wsum: f32 = used[i].iter().map(|w| w.1).sum();
            if wsum > 0.0 {
                logs[i] = Some(
                    used[i]
                        .iter()
                        .map(|&(t, w)| w * self.log_ratio(i, t))
                        .sum::<f32>()
                        / wsum,
                );
                // Sharp somewhere under the subject in at least one frame?
                soft &=
                    used[i].iter().map(|&(t, w)| w * self.s(i, t)).sum::<f32>() / wsum < ROI_SOFT;
            }
        }
        match self.subject_pixels() {
            PixelVote::NotApplicable => {}
            PixelVote::AllSoft => soft = true,
            PixelVote::Logs(pixel) => (soft, logs) = (false, pixel),
        }
        (logs.iter().any(Option::is_some) && !soft).then_some((logs, used))
    }

    /// Every frame measured its own regions on its pixels — the most precise
    /// comparison there is. Region by region (a sharp background face must
    /// not outvote the soft subject), skipping regions soft in every frame.
    fn subject_pixels(&self) -> PixelVote {
        let per: Vec<&[Option<f32>]> = self
            .frames
            .iter()
            .filter_map(|f| f.subject.as_ref().map(|s| &s.per_region[..]))
            .collect();
        let k = per.first().map_or(0, |p| p.len());
        if per.len() != self.n() || per.iter().any(|p| p.len() != k) {
            return PixelVote::NotApplicable;
        }
        let usable: Vec<usize> = (0..k)
            .filter(|&r| per.iter().all(|p| p[r].is_some()))
            .filter(|&r| per.iter().any(|p| p[r].unwrap_or(0.0) >= ROI_SOFT))
            .collect();
        if usable.is_empty() {
            return PixelVote::AllSoft;
        }
        let v = |i: usize, r: usize| finite_pos(per[i][r].unwrap_or(0.0));
        let logs = (0..self.n())
            .map(|i| {
                let sum: f32 = usable
                    .iter()
                    .map(|&r| {
                        let m = median((0..self.n()).map(|j| v(j, r)).collect()).unwrap_or(0.0);
                        ((v(i, r) + DELTA) / (m + DELTA)).ln().clamp(-4.0, 4.0)
                    })
                    .sum();
                Some(sum / usable.len() as f32)
            })
            .collect();
        PixelVote::Logs(logs)
    }

    /// Frames sharp *somewhere*: on a real share of the tiles as sharp as the
    /// best frame there. Counted only where sharpness means something — the
    /// frame clearly beats the softest frame (a subject window), or the tile
    /// is generally sharp; on a tile soft in every frame everybody is "as
    /// sharp as the best", which proves nothing.
    fn sharp_somewhere(&self) -> Vec<bool> {
        let n = self.n();
        let best: Vec<f32> = (0..self.tiles)
            .map(|t| (0..n).map(|i| self.s(i, t)).fold(0.0f32, f32::max))
            .collect();
        let worst: Vec<f32> = (0..self.tiles)
            .map(|t| (0..n).map(|i| self.s(i, t)).fold(f32::INFINITY, f32::min))
            .collect();
        let mut bests: Vec<f32> = self.eligible.iter().map(|&t| best[t]).collect();
        bests.sort_by(f32::total_cmp);
        let sharp_level = bests.get(bests.len() * 9 / 10).copied().unwrap_or(0.0) * 0.5;
        (0..n)
            .map(|i| {
                let hits = self
                    .eligible
                    .iter()
                    .filter(|&&t| {
                        let v = self.s(i, t);
                        v > 0.0
                            && v >= 0.5 * best[t]
                            && (v >= 2.0 * worst[t] + DELTA || v >= sharp_level)
                    })
                    .count();
                !self.eligible.is_empty() && hits as f32 >= SHARP_SHARE * self.eligible.len() as f32
            })
            .collect()
    }

    /// Do at least three frames — and two thirds of the voters — agree on
    /// where the sharp tiles are?
    fn consensus(&self, voters: &[usize], focus: &[f32]) -> bool {
        let sets: Vec<Vec<bool>> = voters
            .iter()
            .map(|&i| {
                let mut v = vec![false; self.tiles];
                for &t in &self.eligible {
                    v[t] = focus[i] > 0.0 && self.s(i, t) >= 0.5 * focus[i];
                }
                v
            })
            .collect();
        let majority: Vec<bool> = (0..self.tiles)
            .map(|t| 2 * sets.iter().filter(|s| s[t]).count() > sets.len())
            .collect();
        let jaccard = |set: &[bool]| {
            let inter = (0..self.tiles).filter(|&t| set[t] && majority[t]).count();
            let union = (0..self.tiles).filter(|&t| set[t] || majority[t]).count();
            if union == 0 {
                0.0
            } else {
                inter as f32 / union as f32
            }
        };
        let agreeing = sets.iter().filter(|s| jaccard(s) >= 0.5).count();
        let mean = sets.iter().map(|s| jaccard(s)).sum::<f32>() / sets.len().max(1) as f32;
        agreeing >= 3 && 3 * agreeing >= 2 * sets.len() && mean >= 0.5
    }
}

/// Nothing to compare tile-for-tile: subject values if all frames have one.
fn by_values(frames: &[&Sharpness]) -> GroupFocus {
    let n = frames.len();
    let all_subject = frames.iter().all(|f| f.subject.is_some());
    let vals: Vec<f32> = frames.iter().map(|f| finite_pos(f.value())).collect();
    let relative = ratio_to_best(&vals);
    let confident = relative
        .iter()
        .map(|&r| n < 2 || all_subject || r < NOWHERE_SHARP)
        .collect();
    GroupFocus {
        own: relative.clone(),
        relative,
        confident,
        uncertain: n >= 2 && !all_subject,
        used_subject: all_subject,
        motion: frames.iter().map(|f| f.motion().into()).collect(),
    }
}

/// Judge a group's frames against each other. See the module docs.
pub fn group_focus(frames: &[&Sharpness]) -> GroupFocus {
    let n = frames.len();
    let (cols, rows) = frames
        .first()
        .map_or((0, 0), |f| (f.tiles.cols, f.tiles.rows));
    let tiles = cols * rows;
    let same_grid = frames.iter().all(|f| {
        (f.tiles.cols, f.tiles.rows, f.tiles.width, f.tiles.height)
            == (cols, rows, frames[0].tiles.width, frames[0].tiles.height)
            && f.tiles.sharpness.len() == tiles
            && f.tiles.detail.len() == tiles
    });
    if n < 2 || !same_grid || tiles == 0 {
        return by_values(frames);
    }
    let focus: Vec<f32> = frames.iter().map(|f| finite_pos(f.focus)).collect();
    let lone = ratio_to_best(&focus);
    let grid = Grid::new(frames, tiles);
    let (voted, tile_log) = grid.tile_vote();

    if let Some((roi_log, roi_tiles)) = grid.subject_vote() {
        // The subject decides; the whole-frame vote only breaks near-ties.
        let logs: Vec<f32> = (0..n)
            .map(|i| roi_log[i].map_or(tile_log[i], |r| r + TILE_TIEBREAK * tile_log[i]))
            .collect();
        return GroupFocus {
            relative: from_logs(&logs),
            confident: vec![true; n],
            uncertain: false,
            used_subject: true,
            own: lone,
            motion: (0..n)
                .map(|i| motion_on(frames[i], &roi_tiles[i]))
                .collect(),
        };
    }

    // No usable subject: blur direction on the commonly sharp tiles, and
    // trust the tile vote only with a real consensus.
    let common: Vec<(usize, f32)> = if voted.is_empty() {
        &grid.eligible
    } else {
        &voted
    }
    .iter()
    .map(|&t| (t, 1.0))
    .collect();
    let motion: Vec<[f32; 2]> = frames.iter().map(|f| motion_on(f, &common)).collect();
    let somewhere = grid.sharp_somewhere();
    let voters: Vec<usize> = (0..n).filter(|&i| somewhere[i]).collect();
    if grid.consensus(&voters, &focus) {
        return GroupFocus {
            relative: from_logs(&tile_log),
            confident: vec![true; n],
            uncertain: false,
            used_subject: false,
            own: lone,
            motion,
        };
    }
    // Uncertain: rank by each frame's own measure; a frame sharp somewhere
    // stays within reach of the best (never rejected on sharpness), a frame
    // sharp nowhere is soft.
    let relative = (0..n)
        .map(|i| {
            if somewhere[i] {
                UNCERTAIN_FLOOR + (1.0 - UNCERTAIN_FLOOR) * lone[i]
            } else {
                lone[i].min(NOWHERE_SHARP)
            }
        })
        .collect();
    let confident = somewhere.iter().map(|&s| !s).collect();
    GroupFocus {
        relative,
        confident,
        uncertain: true,
        used_subject: false,
        own: lone,
        motion,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{blur, img, texture};
    use crate::{AnalysisConfig, LumaImage, analyze, analyze_with_subject};

    const W: usize = 480;
    const H: usize = 320;
    const SUBJ: [usize; 4] = [160, 100, 320, 220];

    fn inside(x: usize, y: usize) -> bool {
        (SUBJ[0]..SUBJ[2]).contains(&x) && (SUBJ[1]..SUBJ[3]).contains(&y)
    }

    fn subject_rect() -> NormRect {
        NormRect {
            x: 160.0 / 480.0,
            y: 100.0 / 320.0,
            w: 160.0 / 480.0,
            h: 120.0 / 320.0,
        }
    }

    /// (held: subject sharp on bokeh, missed: subject soft, background sharp)
    fn pair(seed: u64) -> (LumaImage, LumaImage) {
        let t = texture(W, H, seed);
        let soft = blur(&t, 3, 3);
        let mix = |subject_sharp: bool| {
            img(W, H, |x, y| {
                let sharp = inside(x, y) == subject_sharp;
                if sharp {
                    t.pixels()[y * W + x]
                } else {
                    soft.pixels()[y * W + x]
                }
            })
        };
        (mix(true), mix(false))
    }

    fn run(i: &LumaImage, regions: &[SubjectRegion]) -> Sharpness {
        analyze_with_subject(i, &AnalysisConfig::default(), regions).sharpness
    }

    fn roi() -> [SubjectRegion; 1] {
        [SubjectRegion {
            rect: subject_rect(),
            weight: 1.0,
        }]
    }

    #[test]
    fn subject_region_decides_missed_focus() {
        let (held, missed) = pair(3);
        let (h, m) = (run(&held, &roi()), run(&missed, &roi()));
        assert!(
            h.value() > 3.0 * m.value(),
            "{} vs {}",
            h.value(),
            m.value()
        );
        let g = group_focus(&[&m, &h]);
        assert!(
            g.used_subject && !g.uncertain && g.relative[1] == 1.0 && g.relative[0] < 0.4,
            "{g:?}"
        );
        // Majority missed, subject known: the held frame still wins.
        let m2 = run(&blur(&missed, 0, 0), &roi());
        let g = group_focus(&[&h, &m, &m2]);
        assert!(
            g.relative[0] == 1.0 && g.relative[1] < 0.4 && g.relative[2] < 0.4,
            "{g:?}"
        );
    }

    #[test]
    fn frames_without_a_region_borrow_their_neighbours() {
        // Vision lost the face in the missed frame: it must not win by default.
        let (held, missed) = pair(4);
        let g = group_focus(&[
            &run(&held, &roi()),
            &run(&missed, &[]),
            &run(&missed, &roi()),
        ]);
        assert!(
            g.relative[0] == 1.0 && g.relative[1] < 0.75 && g.relative[2] < 0.75,
            "{g:?}"
        );
    }

    #[test]
    fn a_region_soft_in_every_frame_is_dropped() {
        // A "face" in the bokeh (top-left, soft in all frames) must not decide.
        // Real bokeh: large, smooth blobs (contrast without fine detail).
        let t = texture(W, H, 5);
        let held = img(W, H, |x, y| {
            if inside(x, y) {
                t.pixels()[y * W + x]
            } else {
                (120.0 + 60.0 * (x as f32 / 30.0).sin() * (y as f32 / 30.0).cos()) as u8
            }
        });
        let bg = [SubjectRegion {
            rect: NormRect {
                x: 0.0,
                y: 0.0,
                w: 0.2,
                h: 0.2,
            },
            weight: 1.0,
        }];
        let (a, b) = (run(&held, &bg), run(&blur(&held, 0, 0), &bg));
        assert!(a.value() < ROI_SOFT, "{}", a.value());
        assert!(!group_focus(&[&a, &b]).used_subject);
    }

    #[test]
    fn without_a_subject_disagreement_is_uncertain_and_nobody_is_condemned() {
        // Held vs missed, no region → symmetric; neither is judged soft.
        let (held, missed) = pair(6);
        let (h, m) = (run(&held, &[]), run(&missed, &[]));
        let g = group_focus(&[&h, &m]);
        assert!(g.uncertain && g.confident.iter().all(|c| !c), "{g:?}");
        assert!(g.relative.iter().all(|&r| r >= NOWHERE_SHARP), "{g:?}");
        // Three frames, two agreeing, still not enough corroboration.
        let g = group_focus(&[&h, &m, &m.clone()]);
        assert!(g.uncertain && !g.confident[0], "{g:?}");
        // A frame sharp nowhere is soft regardless.
        let mush = run(&blur(&held, 4, 4), &[]);
        let g = group_focus(&[&h, &m, &mush]);
        assert!(
            g.confident[2] && g.relative[2] < NOWHERE_SHARP && !g.confident[0],
            "{g:?}"
        );
    }

    #[test]
    fn a_real_consensus_is_trusted() {
        // Four frames hold focus on the subject, one slipped to the background.
        let (held, missed) = pair(7);
        let frames = [
            run(&held, &[]),
            run(&held, &[]),
            run(&missed, &[]),
            run(&held, &[]),
            run(&held, &[]),
        ];
        let refs: Vec<&Sharpness> = frames.iter().collect();
        let g = group_focus(&refs);
        assert!(!g.uncertain && g.confident.iter().all(|&c| c), "{g:?}");
        assert!(g.relative[2] < 0.5 && g.relative[0] > 0.9, "{g:?}");
    }

    #[test]
    fn fallbacks() {
        assert_eq!(group_focus(&[]).relative, Vec::<f32>::new());
        let a = analyze(&texture(96, 96, 8), &AnalysisConfig::default()).sharpness;
        let g = group_focus(&[&a]);
        assert_eq!((g.relative, g.confident), (vec![1.0], vec![true]));
        // Different sizes, no subject: plain ratio, uncertain.
        let b = analyze(&blur(&texture(64, 64, 8), 3, 3), &AnalysisConfig::default()).sharpness;
        let g = group_focus(&[&a, &b]);
        assert!(
            g.uncertain && g.relative[0] == 1.0 && g.relative[1] < NOWHERE_SHARP && g.confident[1],
            "{g:?}"
        );
        // Featureless frames: no information.
        let f = analyze(&img(96, 96, |_, _| 50), &AnalysisConfig::default()).sharpness;
        assert_eq!(group_focus(&[&f, &f.clone()]).relative, vec![0.0, 0.0]);
    }

    #[test]
    fn hint_priority() {
        let face = NormRect {
            x: 0.4,
            y: 0.3,
            w: 0.2,
            h: 0.2,
        };
        let far_face = NormRect {
            x: 0.0,
            y: 0.0,
            w: 0.3,
            h: 0.3,
        };
        let sal = NormRect {
            x: 0.1,
            y: 0.6,
            w: 0.2,
            h: 0.2,
        };
        // AF point first, plus the face under it (not the bigger one elsewhere).
        let h = SubjectHints {
            af_point: Some([0.5, 0.4]),
            faces: vec![far_face, face],
            saliency: vec![sal],
        };
        let r = h.regions();
        assert_eq!(r.len(), 2);
        assert!(
            (r[0].rect.x - 0.44).abs() < 1e-6 && r[1].rect == face,
            "{r:?}"
        );
        // No AF point: the largest prominent face.
        let h = SubjectHints {
            af_point: None,
            ..h
        };
        assert_eq!(h.regions()[0].rect, far_face);
        // Tiny faces only: saliency.
        let tiny = NormRect {
            x: 0.5,
            y: 0.5,
            w: 0.05,
            h: 0.05,
        };
        let h = SubjectHints {
            af_point: Some([f32::NAN, 0.0]),
            faces: vec![tiny],
            saliency: vec![sal],
        };
        assert_eq!(h.regions()[0].rect, sal);
        assert_eq!(SubjectHints::default().regions().len(), 0);
    }
}
