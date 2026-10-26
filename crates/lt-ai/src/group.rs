//! Burst and near-duplicate grouping into "moments".
//!
//! Frames are split per camera (`GroupItem::camera`), then ordered by
//! capture time, then `seq` (the file or shot number — needed because A9
//! ARWs only record whole seconds), then input index; frames without a time
//! go last. Each frame is then assigned, in order:
//!
//! 1. **Burst**: to the group of the most similar earlier frame (same camera)
//!    whose capture time is within `burst_gap_ms` — accounting for the
//!    timestamp resolution, so frames stamped 12:00:01 and 12:00:02 can still
//!    be 50 ms apart — and that looks alike (`hash ≤ burst_max_hash_distance`
//!    or Vision distance `≤ vision_max_distance`);
//! 2. **Near-duplicate**: a frame that does *not* continue a burst (a retake
//!    seconds later) joins the recent group whose anchor (first frame) is
//!    very similar (`near_dup_max_hash_distance` or Vision) within
//!    `near_dup_window_ms`;
//! 3. otherwise it starts a new group.
//!
//! A frame only joins a group while it stays close to the group's anchor
//! (`moment_max_hash_distance`) and within `moment_max_span_ms` of it. A long
//! burst that drifts (a runner crossing the frame) therefore splits into
//! several moments — one pick each — instead of chaining into one group whose
//! first and last frames share nothing. Look-back is bounded (64 frames /
//! 64 moments) so 20 000 frames sharing one timestamp stay linear.

use serde::{Deserialize, Serialize};

use crate::PerceptualHash;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GroupConfig {
    /// Max gap between neighbouring frames of one burst. The A9 shoots 20 fps
    /// (50 ms); 250 ms still chains 5 fps bursts.
    pub burst_gap_ms: i64,
    /// Timestamp granularity. `None` detects it: 1 000 ms for a whole-second
    /// stamp (ARW without sub-seconds) in a stream that is ≥ 90 % whole
    /// seconds, else 1 ms.
    pub timestamp_resolution_ms: Option<i64>,
    /// Look-alike check between neighbouring burst frames (0..=1). A 30 px
    /// pan of an A9 preview measures 0.05–0.11; unrelated A9 shots 0.23–0.55.
    pub burst_max_hash_distance: f32,
    /// Max distance from a group's anchor frame; beyond it a new moment starts.
    pub moment_max_hash_distance: f32,
    /// Max time from a group's anchor frame (after resolution slack).
    pub moment_max_span_ms: i64,
    /// Time window for near-duplicate links; `None` = any time apart.
    pub near_dup_window_ms: Option<i64>,
    /// Tight similarity for near-duplicates outside a burst.
    pub near_dup_max_hash_distance: f32,
    /// Max Vision feature-print distance counted as "same scene".
    pub vision_max_distance: f32,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            burst_gap_ms: 250,
            timestamp_resolution_ms: None,
            burst_max_hash_distance: 0.35,
            moment_max_hash_distance: 0.30,
            moment_max_span_ms: 3_000,
            near_dup_window_ms: Some(60_000),
            near_dup_max_hash_distance: 0.12,
            vision_max_distance: 0.35,
        }
    }
}

/// One image to group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GroupItem {
    /// Capture time in ms (any epoch).
    pub capture_ms: Option<i64>,
    /// Orders frames that share a timestamp: lt-raw's `shot_number` /
    /// `sequence_image_number`, or `lt_raw::filename_sequence` (`DSC07062`
    /// → 7062). Counter wraps (9999 → 0001) and power-cycle resets inside
    /// one timestamp are undone. Without it, input order is used — pass it:
    /// A9 ARWs stamp whole seconds, so 20 frames share every timestamp.
    pub seq: Option<u64>,
    /// Camera identity (e.g. a hash of the body serial). Frames from
    /// different bodies never share a group.
    pub camera: Option<u64>,
    pub hash: PerceptualHash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    Single,
    /// Contains at least one burst link.
    Burst,
    /// Linked only by visual similarity.
    NearDuplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub kind: GroupKind,
    /// Input indices in capture order; the first is the anchor.
    pub members: Vec<usize>,
}

/// Optional Vision feature-print distance for a pair of input indices
/// (always called with `a < b`). `None` or a non-finite value = unknown.
pub type VisionDistance<'a> = &'a dyn Fn(usize, usize) -> Option<f32>;

struct Ctx<'a> {
    items: &'a [GroupItem],
    cfg: &'a GroupConfig,
    vision: Option<VisionDistance<'a>>,
}

impl Ctx<'_> {
    fn hash_d(&self, a: usize, b: usize) -> f32 {
        self.items[a].hash.distance(&self.items[b].hash)
    }

    /// Hash within `thr`, or Vision says same scene.
    fn close(&self, a: usize, b: usize, thr: f32) -> bool {
        self.hash_d(a, b) <= thr
            || self
                .vision
                .and_then(|f| f(a.min(b), a.max(b)))
                .is_some_and(|d| d.is_finite() && d <= self.cfg.vision_max_distance)
    }
}

struct Moment {
    anchor: usize,
    members: Vec<usize>,
    burst: bool,
}

/// How far back (in capture order) a frame looks for its burst predecessor,
/// and how many recent moments it may join as a near-duplicate. Bounds the
/// work when thousands of frames share a timestamp (or have none).
const LOOKBACK: usize = 64;

/// A jump this large between shot/file numbers that share a timestamp is a
/// counter wrap (9999 → 0001) or a reset after a power cycle: the high block
/// was shot first.
const SEQ_WRAP_GAP: u64 = 1000;

/// Order equal-timestamp runs by `seq`, undoing counter wraps.
fn unwrap_seq_ties(order: &mut [usize], items: &[GroupItem]) {
    let same = |a: usize, b: usize| {
        (items[a].camera, items[a].capture_ms) == (items[b].camera, items[b].capture_ms)
    };
    let mut start = 0;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len() && same(order[start], order[end]) {
            end += 1;
        }
        // `order` is sorted by seq within the run, `None` first.
        let run = &mut order[start..end];
        let first_seq = run
            .iter()
            .position(|&i| items[i].seq.is_some())
            .unwrap_or(run.len());
        let seqd = &mut run[first_seq..];
        let gaps = seqd.windows(2).enumerate().map(|(k, w)| {
            (
                items[w[1]].seq.unwrap_or(0) - items[w[0]].seq.unwrap_or(0),
                k,
            )
        });
        if let Some((gap, k)) = gaps.max()
            && gap > SEQ_WRAP_GAP
        {
            seqd.rotate_left(k + 1);
        }
        start = end;
    }
}

/// Group images into moments. Every input index appears in exactly one
/// group; groups are ordered by their anchor's capture order.
pub fn group_images(
    items: &[GroupItem],
    cfg: &GroupConfig,
    vision: Option<VisionDistance<'_>>,
) -> Vec<Group> {
    let ctx = Ctx { items, cfg, vision };
    let key = |i: usize| {
        let it = &items[i];
        (it.capture_ms.is_none(), it.capture_ms, it.seq, i)
    };
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| (items[i].camera, key(i)));
    unwrap_seq_ties(&mut order, items);
    let mut rank = vec![0usize; items.len()];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r;
    }

    let mut moments: Vec<Moment> = Vec::new();
    for stream in order.chunk_by(|&a, &b| items[a].camera == items[b].camera) {
        group_stream(&ctx, stream, &mut moments);
    }
    // Chronological across cameras; capture order breaks ties.
    moments.sort_by_key(|m| {
        (
            items[m.anchor].capture_ms.is_none(),
            items[m.anchor].capture_ms,
            rank[m.anchor],
        )
    });
    moments
        .into_iter()
        .map(|m| Group {
            kind: match (m.members.len(), m.burst) {
                (1, _) => GroupKind::Single,
                (_, true) => GroupKind::Burst,
                _ => GroupKind::NearDuplicate,
            },
            members: m.members,
        })
        .collect()
}

fn group_stream(ctx: &Ctx<'_>, stream: &[usize], out: &mut Vec<Moment>) {
    let cfg = ctx.cfg;
    let items = ctx.items;
    // A whole-second stamp may hide up to 999 ms (ARW has no sub-seconds).
    // Decided per item — so one phone photo with milliseconds does not
    // change how the camera's frames are read — but only in a stream that is
    // mostly whole seconds: 1 in 20 frames of a 20 fps millisecond stream
    // lands on a whole second by chance.
    let whole = |i: usize| items[i].capture_ms.is_some_and(|t| t.rem_euclid(1000) == 0);
    let timed = stream
        .iter()
        .filter(|&&i| items[i].capture_ms.is_some())
        .count();
    let whole_stream = 10 * stream.iter().filter(|&&i| whole(i)).count() >= 9 * timed;
    let res = |i: usize| {
        cfg.timestamp_resolution_ms
            .unwrap_or(if whole_stream && whole(i) { 1000 } else { 1 })
            .max(1)
    };
    // Elapsed time between two frames, minus what their resolution may hide.
    let gap = |a: usize, b: usize| match (items[a].capture_ms, items[b].capture_ms) {
        (Some(ta), Some(tb)) => Some(
            tb.saturating_sub(ta)
                .saturating_sub(res(a).max(res(b)) - 1)
                .max(0),
        ),
        _ => None,
    };
    let first = out.len();
    let mut moment_of = vec![usize::MAX; stream.len()];
    for (pos, &j) in stream.iter().enumerate() {
        // 1. Burst: most similar recent frame within the burst gap (ties:
        //    the most recent).
        let mut best: Option<(f32, usize)> = None;
        for p in (pos.saturating_sub(LOOKBACK)..pos).rev() {
            let i = stream[p];
            // Not `break`: a nearer frame with a millisecond stamp can be
            // further away than an older whole-second one, whose stamp may
            // hide up to 999 ms.
            match gap(i, j) {
                Some(g) if g <= cfg.burst_gap_ms => {}
                _ => continue,
            }
            let d = ctx.hash_d(i, j);
            if ctx.close(i, j, cfg.burst_max_hash_distance) && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, p));
            }
        }
        let burst_home = best.map(|(_, p)| moment_of[p]).filter(|&m| {
            let anchor = out[m].anchor;
            gap(anchor, j).unwrap_or(0) <= cfg.moment_max_span_ms
                && ctx.close(anchor, j, cfg.moment_max_hash_distance)
        });

        // 2. Near-duplicate: the closest recent anchor within the window (ties:
        //    the most recent). A frame that continues a burst but outgrew its
        //    moment starts a new moment instead, so the span limit holds for
        //    long bursts (a static 15 s burst is several moments).
        let continues_burst = best.is_some();
        let near_home = || {
            (first..out.len())
                .rev()
                .take(LOOKBACK)
                .take_while(|&m| match (gap(out[m].anchor, j), cfg.near_dup_window_ms) {
                    (Some(g), Some(w)) => g <= w,
                    _ => true,
                })
                .filter(|&m| ctx.close(out[m].anchor, j, cfg.near_dup_max_hash_distance))
                .min_by(|&a, &b| {
                    ctx.hash_d(out[a].anchor, j)
                        .total_cmp(&ctx.hash_d(out[b].anchor, j))
                        .then(b.cmp(&a))
                })
        };

        let m = if let Some(m) = burst_home {
            out[m].burst = true;
            m
        } else if let Some(m) = (!continues_burst).then(near_home).flatten() {
            m
        } else {
            out.push(Moment {
                anchor: j,
                members: Vec::new(),
                burst: false,
            });
            out.len() - 1
        };
        out[m].members.push(j);
        moment_of[pos] = m;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(bits: u32) -> PerceptualHash {
        let dhash = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        PerceptualHash { dhash, phash: 0 }
    }

    fn item(t: Option<i64>, bits: u32) -> GroupItem {
        GroupItem {
            capture_ms: t,
            hash: h(bits),
            ..GroupItem::default()
        }
    }

    fn members(g: &[Group]) -> Vec<Vec<usize>> {
        g.iter().map(|g| g.members.clone()).collect()
    }

    fn run(items: &[GroupItem]) -> Vec<Group> {
        group_images(items, &GroupConfig::default(), None)
    }

    #[test]
    fn burst_splits_on_gap_and_scene_change() {
        let items = [
            item(Some(0), 0),
            item(Some(50), 5),
            item(Some(100), 10),
            item(Some(2100), 64),
            item(Some(2150), 60),
        ];
        let g = run(&items);
        assert_eq!(members(&g), vec![vec![0, 1, 2], vec![3, 4]]);
        assert!(g.iter().all(|g| g.kind == GroupKind::Burst));
    }

    #[test]
    fn whole_second_timestamps_keep_a_burst_together() {
        // 60 frames at 20 fps, ARW stamps only whole seconds; seq orders them.
        let items: Vec<GroupItem> = (0..60u64)
            .map(|k| GroupItem {
                capture_ms: Some(1_000 * (k as i64 / 20)),
                seq: Some(7000 + k),
                hash: h(0),
                camera: None,
            })
            .rev() // input order must not matter
            .collect();
        let g = run(&items);
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].members[0], 59, "anchor is the lowest shot number");
        // A real pause (stamps three seconds apart) still splits.
        let paused = [item(Some(0), 0), item(Some(3_000), 0)];
        assert_eq!(
            group_images(
                &paused,
                &GroupConfig {
                    near_dup_window_ms: Some(0),
                    ..GroupConfig::default()
                },
                None
            )
            .len(),
            2
        );
    }

    #[test]
    fn two_bodies_group_separately() {
        // Same scene, same instants, two cameras: one group per body.
        let items: Vec<GroupItem> = (0..20)
            .map(|k| GroupItem {
                capture_ms: Some(k / 2 * 50),
                camera: Some((k % 2) as u64),
                hash: h(0),
                seq: None,
            })
            .collect();
        let g = run(&items);
        assert_eq!(
            members(&g),
            vec![
                (0..20).step_by(2).collect::<Vec<_>>(),
                (1..20).step_by(2).collect()
            ]
        );
        // Without camera keys, unrelated interleaved scenes still separate by hash.
        let mixed: Vec<_> = (0..10)
            .map(|k| item(Some(k * 25), if k % 2 == 0 { 0 } else { 64 }))
            .collect();
        assert_eq!(
            members(&run(&mixed)),
            vec![vec![0, 2, 4, 6, 8], vec![1, 3, 5, 7, 9]]
        );
    }

    #[test]
    fn drifting_burst_splits_into_moments() {
        // 200 frames, each 1 bit from the previous → frame 0 vs 150 far apart.
        let items: Vec<GroupItem> = (0..200)
            .map(|k| item(Some(k * 50), k as u32 * 64 / 200))
            .collect();
        let cfg = GroupConfig {
            moment_max_span_ms: i64::MAX,
            ..GroupConfig::default()
        };
        let g = group_images(&items, &cfg, None);
        assert!(g.len() >= 2, "{}", g.len());
        for grp in &g {
            let a = items[grp.members[0]].hash;
            assert!(
                grp.members
                    .iter()
                    .all(|&m| a.distance(&items[m].hash) <= 0.30)
            );
            assert!(grp.members.len() == 1 || grp.kind == GroupKind::Burst);
        }
        // Time cap: a static 10 s burst also splits into ≤ 3 s moments.
        let still: Vec<_> = (0..200).map(|k| item(Some(k * 50), 0)).collect();
        let cfg = GroupConfig {
            near_dup_window_ms: Some(0),
            ..GroupConfig::default()
        };
        assert_eq!(group_images(&still, &cfg, None).len(), 4);
    }

    #[test]
    fn ties_are_ordered_by_seq_then_index() {
        let items = [
            item(Some(100), 0),
            item(Some(0), 0),
            item(Some(50), 0),
            item(Some(50), 0),
        ];
        assert_eq!(members(&run(&items)), vec![vec![1, 2, 3, 0]]);
        let mut seqd = items;
        seqd[2].seq = Some(9);
        seqd[3].seq = Some(8);
        assert_eq!(members(&run(&seqd)), vec![vec![1, 3, 2, 0]]);
    }

    #[test]
    fn simultaneous_frames_of_different_scenes_stay_apart() {
        let g = run(&[item(Some(0), 0), item(Some(0), 64)]);
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|g| g.kind == GroupKind::Single));
    }

    #[test]
    fn near_duplicates_by_hash_and_by_vision() {
        let cfg = GroupConfig::default();
        let items = [
            item(Some(0), 0),
            item(Some(5_000), 3),
            item(Some(10_000), 20),
        ];
        let g = group_images(&items, &cfg, None);
        assert_eq!(members(&g), vec![vec![0, 1], vec![2]]);
        assert_eq!(g[0].kind, GroupKind::NearDuplicate);

        let calls = std::cell::Cell::new(0);
        let vision = |a: usize, b: usize| {
            calls.set(calls.get() + 1);
            assert!(a < b);
            ((a, b) == (0, 2)).then_some(0.1)
        };
        let g = group_images(&items, &cfg, Some(&vision));
        assert_eq!(members(&g), vec![vec![0, 1, 2]]);
        assert!(calls.get() >= 1);
        let nan = |_: usize, _: usize| Some(f32::NAN);
        assert_eq!(group_images(&items, &cfg, Some(&nan)).len(), 2);
    }

    #[test]
    fn window_limits_and_unknown_times() {
        let mut cfg = GroupConfig::default();
        let far = [item(Some(0), 0), item(Some(120_000), 0)];
        assert_eq!(group_images(&far, &cfg, None).len(), 2);
        cfg.near_dup_window_ms = None;
        cfg.moment_max_span_ms = i64::MAX;
        assert_eq!(group_images(&far, &cfg, None).len(), 1);

        // Timeless frames can only be near-duplicates; they sort last.
        let items = [item(None, 0), item(Some(0), 64), item(None, 2)];
        let g = run(&items);
        assert_eq!(members(&g), vec![vec![1], vec![0, 2]]);
        assert_eq!(g[1].kind, GroupKind::NearDuplicate);
    }

    #[test]
    fn empty_and_extreme_timestamps() {
        assert_eq!(run(&[]).len(), 0);
        assert_eq!(
            run(&[item(Some(i64::MIN), 0), item(Some(i64::MAX), 0)]).len(),
            2
        );
    }

    fn it(t: i64, seq: Option<u64>, bits: u32) -> GroupItem {
        GroupItem {
            capture_ms: Some(t),
            seq,
            camera: None,
            hash: h(bits),
        }
    }

    fn no_near_dup() -> GroupConfig {
        GroupConfig {
            near_dup_window_ms: Some(0),
            ..GroupConfig::default()
        }
    }

    #[test]
    fn one_millisecond_photo_does_not_break_a_whole_second_burst() {
        // 40 ARW frames (whole seconds) + one phone photo.
        let mut items: Vec<_> = (0..40u64)
            .map(|k| it(10_000 + 1000 * (k as i64 / 20), Some(k), 0))
            .collect();
        items.push(it(500_123, None, 40));
        let g = group_images(&items, &no_near_dup(), None);
        assert_eq!(
            g.iter().map(|g| g.members.len()).collect::<Vec<_>>(),
            vec![40, 1]
        );
        // A millisecond stream is not read as whole seconds just because every
        // 20th frame lands on one: a 10 s static burst is ≤ 3 s moments.
        let still: Vec<_> = (0..200).map(|k| item(Some(k * 50), 0)).collect();
        assert_eq!(group_images(&still, &no_near_dup(), None).len(), 4);
    }

    #[test]
    fn burst_look_back_skips_a_far_frame_instead_of_stopping() {
        // 19 whole-second ARW frames, a phone photo 100 ms later, then a frame
        // whose nearest predecessor (the phone photo) is 1.1 s away but whose
        // look-alike whole-second frames may be only 201 ms away.
        let mut items: Vec<_> = (0..19u64).map(|k| it(10_000, Some(k), 0)).collect();
        items.push(it(10_100, None, 40));
        items.push(it(11_200, None, 0));
        let g = group_images(&items, &no_near_dup(), None);
        assert_eq!(
            g.iter().map(|g| g.members.len()).collect::<Vec<_>>(),
            vec![20, 1]
        );
        assert!(g[0].members.contains(&20) && g[0].kind == GroupKind::Burst);
    }

    #[test]
    fn long_static_burst_is_several_moments_even_with_near_duplicates_on() {
        // 300 frames, 15 s, identical content.
        let items: Vec<_> = (0..300).map(|k| it(k * 50, Some(k as u64), 0)).collect();
        let sizes: Vec<usize> = run(&items).iter().map(|g| g.members.len()).collect();
        assert_eq!(sizes, vec![61, 61, 61, 61, 56]);
        // A genuine retake seconds later (not a burst continuation) still joins.
        let retake = [it(0, None, 0), it(5_000, None, 2)];
        assert_eq!(run(&retake).len(), 1);
    }

    #[test]
    fn counter_wraps_and_resets_inside_one_second_keep_shot_order() {
        // File numbers 9990..9999 then 0..9 in the same second.
        let mut items: Vec<_> = (0..10u64)
            .map(|k| it(7000, Some(9990 + k), 2 * k as u32))
            .collect();
        items.extend((0..10u64).map(|k| it(7000, Some(k), 20 + 2 * k as u32)));
        let g = run(&items);
        assert_eq!(g[0].members[..3], [0, 1, 2], "{g:?}");
        let order: Vec<usize> = g.iter().flat_map(|g| g.members.clone()).collect();
        assert_eq!(order, (0..20).collect::<Vec<_>>());
        // Power-cycle reset (9990.. → 1..) behaves the same.
        let mut items: Vec<_> = (0..10u64)
            .map(|k| it(5000, Some(9990 + k), k as u32))
            .collect();
        items.extend((0..10u64).map(|k| it(5000, Some(1 + k), 10 + k as u32)));
        assert_eq!(run(&items)[0].members, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn identical_timestamps_are_ordered_by_filename_sequence() {
        // Shuffled input, one timestamp; the caller passes
        // `lt_raw::filename_sequence` as `seq` and the true order comes back.
        let perm: Vec<u64> = (0..20).map(|k| (k * 7) % 20).collect();
        let items: Vec<_> = perm
            .iter()
            .map(|&k| it(1000, Some(7000 + k), 2 * k as u32))
            .collect();
        let g = run(&items);
        let frames: Vec<u64> = g
            .iter()
            .flat_map(|g| g.members.iter().map(|&i| perm[i]))
            .collect();
        assert_eq!(frames, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn many_frames_sharing_or_lacking_a_timestamp_stay_linear() {
        // Was 2.9 s for 20 000 identical stamps.
        for t in [Some(1_000), None] {
            let items: Vec<_> = (0..20_000u64)
                .map(|k| GroupItem {
                    capture_ms: t,
                    seq: None,
                    camera: None,
                    hash: PerceptualHash {
                        dhash: k.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                        phash: k,
                    },
                })
                .collect();
            let start = std::time::Instant::now();
            let g = run(&items);
            assert_eq!(g.iter().map(|g| g.members.len()).sum::<usize>(), 20_000);
            assert!(start.elapsed().as_secs_f32() < 2.0, "{:?}", start.elapsed());
        }
    }

    #[test]
    fn near_duplicate_ties_prefer_the_most_recent_moment() {
        // A retake exactly as close (1 bit) to two earlier, distinct
        // moments joins the more recent one.
        let hash = |dhash| PerceptualHash { dhash, phash: 0 };
        let items = [
            GroupItem {
                capture_ms: Some(0),
                hash: hash(0b01),
                ..GroupItem::default()
            },
            GroupItem {
                capture_ms: Some(5_000),
                hash: hash(0b10),
                ..GroupItem::default()
            },
            GroupItem {
                capture_ms: Some(9_000),
                hash: hash(0b00),
                ..GroupItem::default()
            },
        ];
        // Only 1-bit differences count as near-duplicates (0b01 vs 0b10 is 2).
        let cfg = GroupConfig {
            near_dup_max_hash_distance: 1.5 / 128.0,
            ..GroupConfig::default()
        };
        let g = group_images(&items, &cfg, None);
        assert_eq!(
            g.iter().map(|g| g.members.clone()).collect::<Vec<_>>(),
            vec![vec![0], vec![1, 2]]
        );
    }
}
