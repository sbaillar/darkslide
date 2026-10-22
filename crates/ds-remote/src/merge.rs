//! Sidecar merge rules for edits pushed from another device.
//!
//! The pushed sidecar names the sidecar it was edited from (`based_on`, a
//! BLAKE3 of the Mac's file as the client pulled it):
//! - **Fast-forward:** the Mac's sidecar is still that one: take the push.
//! - **Field merge:** otherwise, each field is merged three ways against the
//!   base (when the client sent it): a field only one side changed takes that
//!   side's value; keywords merge as sets (additions and removals of both
//!   sides). A field both sides changed takes the newer write.
//! - **Conflict:** if both sides changed the adjustments differently, the
//!   newer edit wins on the photo's variant and the other edit is kept as a
//!   new variant: nothing is dropped.
//!
//! Without the base every differing field counts as changed on both sides.

use ds_core::{Adjustments, ColorTag, PickState, Rating};
use ds_session::Sidecar;
use serde::{Deserialize, Serialize};

/// What a device may change on a photo's variant.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Edit {
    pub adjustments: Adjustments,
    pub rating: Rating,
    pub color_tag: ColorTag,
    pub pick: PickState,
    pub keywords: Vec<String>,
}

impl Edit {
    pub fn of(sc: &Sidecar) -> Self {
        Self {
            adjustments: sc.adjustments.clone(),
            rating: sc.rating,
            color_tag: sc.color_tag,
            pick: sc.pick,
            keywords: sc.keywords.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeOutcome {
    /// The push matched the Mac's sidecar.
    Unchanged,
    FastForward,
    /// Both sides changed; merged field by field without losing an edit.
    Merged,
    /// Both changed the adjustments; the older edit was kept as a variant.
    Conflict,
}

/// The adjustments that lost a conflict, to keep as a new variant.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub adjustments: Adjustments,
    /// They were the pushed (remote) edit.
    pub remote: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub edit: Edit,
    pub kept: Option<Kept>,
    pub outcome: MergeOutcome,
}

/// Merge `remote` into `local`. `fast_forward`: the push was based on
/// exactly `local`. `remote_newer`: the push was edited after `local` was
/// last written.
pub fn merge(
    base: Option<&Edit>,
    local: &Edit,
    remote: &Edit,
    fast_forward: bool,
    remote_newer: bool,
) -> Merged {
    if local == remote {
        return Merged {
            edit: local.clone(),
            kept: None,
            outcome: MergeOutcome::Unchanged,
        };
    }
    if fast_forward {
        return Merged {
            edit: remote.clone(),
            kept: None,
            outcome: MergeOutcome::FastForward,
        };
    }
    let n = remote_newer;
    let adjustments = three_way(
        base.map(|b| &b.adjustments),
        &local.adjustments,
        &remote.adjustments,
        n,
    )
    .clone();
    let both_changed_adj = local.adjustments != remote.adjustments
        && base.is_none_or(|b| {
            b.adjustments != local.adjustments && b.adjustments != remote.adjustments
        });
    let kept = both_changed_adj.then(|| Kept {
        adjustments: if remote_newer {
            local.adjustments.clone()
        } else {
            remote.adjustments.clone()
        },
        remote: !remote_newer,
    });
    let edit = Edit {
        adjustments,
        rating: *three_way(base.map(|b| &b.rating), &local.rating, &remote.rating, n),
        color_tag: *three_way(
            base.map(|b| &b.color_tag),
            &local.color_tag,
            &remote.color_tag,
            n,
        ),
        pick: *three_way(base.map(|b| &b.pick), &local.pick, &remote.pick, n),
        keywords: merge_keywords(
            base.map(|b| &b.keywords[..]),
            &local.keywords,
            &remote.keywords,
        ),
    };
    Merged {
        edit,
        outcome: if kept.is_some() {
            MergeOutcome::Conflict
        } else {
            MergeOutcome::Merged
        },
        kept,
    }
}

fn three_way<'a, T: PartialEq>(base: Option<&T>, l: &'a T, r: &'a T, remote_newer: bool) -> &'a T {
    if l == r || base == Some(r) {
        l
    } else if base == Some(l) || remote_newer {
        r
    } else {
        l
    }
}

/// Local keywords, minus those the remote removed from the base, plus those
/// it added (in the remote's order). Without a base: the union.
fn merge_keywords(base: Option<&[String]>, local: &[String], remote: &[String]) -> Vec<String> {
    let base = base.unwrap_or_default();
    let removed = |k: &String| base.contains(k) && !remote.contains(k);
    let mut out: Vec<String> = local.iter().filter(|k| !removed(k)).cloned().collect();
    for k in remote {
        if !base.contains(k) && !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn adj(exposure: f32) -> Adjustments {
        let mut a = Adjustments::default();
        a.exposure.exposure_ev = exposure;
        a
    }

    fn edit(exp: f32, rating: u8, kw: &[&str]) -> Edit {
        Edit {
            adjustments: adj(exp),
            rating: Rating::new(rating).unwrap(),
            keywords: kw.iter().map(|s| (*s).to_owned()).collect(),
            ..Edit::default()
        }
    }

    #[test]
    fn identical_and_fast_forward() {
        let l = edit(0.5, 1, &["a"]);
        assert_eq!(
            merge(None, &l, &l, false, true).outcome,
            MergeOutcome::Unchanged
        );
        let r = edit(1.0, 3, &[]);
        let m = merge(Some(&l), &l, &r, true, false);
        assert_eq!(
            (m.outcome, &m.edit, m.kept),
            (MergeOutcome::FastForward, &r, None)
        );
    }

    #[test]
    fn one_sided_changes_merge_field_by_field() {
        let base = edit(0.0, 1, &["trip", "day1"]);
        // Mac rated it, iPad edited the exposure and the keywords.
        let local = edit(0.0, 4, &["trip", "day1"]);
        let remote = edit(0.7, 1, &["trip", "best"]);
        let m = merge(Some(&base), &local, &remote, false, false);
        assert_eq!(m.outcome, MergeOutcome::Merged);
        assert_eq!(m.edit.adjustments, adj(0.7));
        assert_eq!(m.edit.rating, Rating::new(4).unwrap());
        assert_eq!(m.edit.keywords, ["trip", "best"]);
        assert_eq!(m.kept, None);
    }

    #[test]
    fn both_changed_adjustments_newer_wins_and_the_other_is_kept() {
        let base = edit(0.0, 0, &[]);
        let local = edit(0.3, 2, &["mac"]);
        let remote = edit(0.9, 5, &["ipad"]);
        let m = merge(Some(&base), &local, &remote, false, true);
        assert_eq!(m.outcome, MergeOutcome::Conflict);
        assert_eq!(m.edit.adjustments, adj(0.9));
        assert_eq!(
            m.edit.rating,
            Rating::new(5).unwrap(),
            "both rated: newer wins"
        );
        assert_eq!(m.edit.keywords, ["mac", "ipad"]);
        assert_eq!(
            m.kept,
            Some(Kept {
                adjustments: adj(0.3),
                remote: false
            })
        );
        let m = merge(Some(&base), &local, &remote, false, false);
        assert_eq!(m.edit.adjustments, adj(0.3));
        assert_eq!(m.edit.rating, Rating::new(2).unwrap());
        assert_eq!(
            m.kept.unwrap(),
            Kept {
                adjustments: adj(0.9),
                remote: true
            }
        );
    }

    #[test]
    fn without_a_base_differences_are_conflicts_and_keywords_union() {
        let local = edit(0.3, 2, &["a"]);
        let remote = edit(0.3, 4, &["b"]);
        let m = merge(None, &local, &remote, false, true);
        assert_eq!(
            m.outcome,
            MergeOutcome::Merged,
            "same adjustments: no variant"
        );
        assert_eq!(m.edit.rating, Rating::new(4).unwrap());
        assert_eq!(m.edit.keywords, ["a", "b"]);
        let m = merge(None, &local, &edit(0.1, 2, &["a"]), false, false);
        assert_eq!(m.outcome, MergeOutcome::Conflict);
        assert_eq!(m.edit.adjustments, adj(0.3));
    }

    fn arb_edit() -> impl Strategy<Value = Edit> {
        let words = prop::sample::subsequence(vec!["a", "b", "c", "d"], 0..=4);
        (0..3u8, 0..=5u8, 0..3u8, words).prop_map(|(e, r, c, kw)| Edit {
            adjustments: adj(f32::from(e) * 0.5),
            rating: Rating::new(r).unwrap(),
            color_tag: [ColorTag::None, ColorTag::Red, ColorTag::Green][c as usize],
            keywords: kw.into_iter().map(str::to_owned).collect(),
            ..Edit::default()
        })
    }

    proptest! {
        #[test]
        fn no_edit_is_ever_lost(base in arb_edit(), local in arb_edit(), remote in arb_edit(),
                                has_base: bool, newer: bool) {
            let b = has_base.then_some(&base);
            let m = merge(b, &local, &remote, false, newer);
            // The result's adjustments are one side's, and the other side's
            // survive as a kept variant whenever both sides changed them.
            let won = &m.edit.adjustments;
            prop_assert!(won == &local.adjustments || won == &remote.adjustments);
            for side in [&local.adjustments, &remote.adjustments] {
                let changed = b.is_none_or(|b| &b.adjustments != side);
                if changed && side != won {
                    prop_assert_eq!(m.kept.as_ref().map(|k| &k.adjustments), Some(side));
                }
            }
            // Keywords both sides have are kept; keywords one side added appear.
            for k in &local.keywords {
                if remote.contains_kw(k) { prop_assert!(m.edit.keywords.contains(k)); }
            }
            for k in &remote.keywords {
                if b.is_none_or(|b| !b.keywords.contains(k)) {
                    prop_assert!(m.edit.keywords.contains(k));
                }
            }
            // Merging is idempotent: merging the result again changes nothing.
            prop_assert_eq!(merge(b, &m.edit, &m.edit, false, newer).edit, m.edit.clone());
            // A fast-forward always takes the push.
            prop_assert_eq!(merge(b, &local, &remote, true, newer).edit, remote.clone());
        }
    }

    impl Edit {
        fn contains_kw(&self, k: &String) -> bool {
            self.keywords.contains(k)
        }
    }
}
