//! Per-variant bounded undo/redo with slider coalescing and cross-variant groups.
//!
//! Each variant has its own stack of prior states (adjustments plus the
//! organizational fields, so ⌘Z after a rating change undoes the rating). A
//! batch operation (copy/apply to many variants) tags its entries with one
//! [`GroupId`]; undoing or redoing any member undoes/redoes every member whose
//! top entry still belongs to that group, so the batch is a single step.
//!
//! Only edits with the same *coalesce key* (e.g. the same set of adjustment
//! groups, i.e. one slider being dragged) within the window merge into one step.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Identifies one batch step across variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoConfig {
    /// Steps kept per variant; older ones are dropped.
    pub max_steps: usize,
    /// Edits with the same key closer together than this merge into one step.
    pub coalesce: Duration,
}

impl Default for UndoConfig {
    fn default() -> Self {
        Self {
            max_steps: 200,
            coalesce: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone)]
struct Entry<S> {
    state: S,
    group: Option<GroupId>,
}

#[derive(Debug)]
struct History<S> {
    undo: VecDeque<Entry<S>>,
    redo: Vec<Entry<S>>,
    /// Time and key of the last coalescible edit; cleared by groups, undo,
    /// redo, and non-coalescible edits.
    last_edit: Option<(Instant, u64)>,
}

impl<S> Default for History<S> {
    fn default() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            last_edit: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dir {
    Undo,
    Redo,
}

#[derive(Debug)]
pub(crate) struct UndoManager<S> {
    cfg: UndoConfig,
    histories: HashMap<i64, History<S>>,
    next_group: u64,
}

impl<S: Clone> UndoManager<S> {
    pub fn new(cfg: UndoConfig) -> Self {
        Self {
            cfg,
            histories: HashMap::new(),
            next_group: 0,
        }
    }

    fn push(&mut self, v: i64, entry: Entry<S>) -> &mut History<S> {
        let max = self.cfg.max_steps.max(1);
        let h = self.histories.entry(v).or_default();
        h.redo.clear();
        h.undo.push_back(entry);
        while h.undo.len() > max {
            h.undo.pop_front();
        }
        h
    }

    /// Record that `v` is about to change away from `before` (a single-variant
    /// edit). `key` identifies what is being edited; `None` never coalesces.
    pub fn record(&mut self, v: i64, before: &S, now: Instant, key: Option<u64>) {
        let window = self.cfg.coalesce;
        let h = self.histories.entry(v).or_default();
        let coalesce = h.redo.is_empty()
            && h.undo.back().is_some_and(|e| e.group.is_none())
            && key.is_some_and(|k| {
                h.last_edit
                    .is_some_and(|(t, lk)| lk == k && now.saturating_duration_since(t) < window)
            });
        if coalesce {
            h.last_edit = key.map(|k| (now, k));
            return;
        }
        self.push(
            v,
            Entry {
                state: before.clone(),
                group: None,
            },
        )
        .last_edit = key.map(|k| (now, k));
    }

    pub fn new_group(&mut self) -> GroupId {
        self.next_group += 1;
        GroupId(self.next_group)
    }

    /// Record one member of a batch step.
    pub fn record_group(&mut self, g: GroupId, v: i64, before: &S) {
        self.push(
            v,
            Entry {
                state: before.clone(),
                group: Some(g),
            },
        )
        .last_edit = None;
    }

    fn stack(h: &History<S>, dir: Dir) -> Option<&Entry<S>> {
        match dir {
            Dir::Undo => h.undo.back(),
            Dir::Redo => h.redo.last(),
        }
    }

    /// States to restore for undoing/redoing `v`'s top step: just `v`, or every
    /// variant whose top entry is in the same group. Nothing is popped yet.
    pub fn peek(&self, v: i64, dir: Dir) -> Vec<(i64, S)> {
        let Some(top) = self.histories.get(&v).and_then(|h| Self::stack(h, dir)) else {
            return Vec::new();
        };
        match top.group {
            None => vec![(v, top.state.clone())],
            Some(g) => self.peek_group(g, dir),
        }
    }

    pub fn peek_group(&self, g: GroupId, dir: Dir) -> Vec<(i64, S)> {
        let mut out: Vec<(i64, S)> = self
            .histories
            .iter()
            .filter_map(|(id, h)| {
                Self::stack(h, dir)
                    .filter(|e| e.group == Some(g))
                    .map(|e| (*id, e.state.clone()))
            })
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// Pop the peeked entries and push `current` states onto the opposite stack.
    pub fn commit(&mut self, dir: Dir, currents: Vec<(i64, S)>) {
        let max = self.cfg.max_steps.max(1);
        for (v, current) in currents {
            let Some(h) = self.histories.get_mut(&v) else {
                continue;
            };
            let popped = match dir {
                Dir::Undo => h.undo.pop_back(),
                Dir::Redo => h.redo.pop(),
            };
            let Some(popped) = popped else { continue };
            let entry = Entry {
                state: current,
                group: popped.group,
            };
            match dir {
                Dir::Undo => h.redo.push(entry),
                Dir::Redo => {
                    h.undo.push_back(entry);
                    while h.undo.len() > max {
                        h.undo.pop_front();
                    }
                }
            }
            h.last_edit = None;
        }
    }

    pub fn depth(&self, v: i64, dir: Dir) -> usize {
        self.histories.get(&v).map_or(0, |h| match dir {
            Dir::Undo => h.undo.len(),
            Dir::Redo => h.redo.len(),
        })
    }

    pub fn forget(&mut self, v: i64) {
        self.histories.remove(&v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: Option<u64> = Some(1);

    #[test]
    fn coalesces_within_window_only() {
        let mut m = UndoManager::new(UndoConfig::default());
        let t0 = Instant::now();
        m.record(1, &0, t0, K);
        m.record(1, &1, t0 + Duration::from_millis(100), K);
        m.record(1, &2, t0 + Duration::from_millis(599), K);
        assert_eq!(m.depth(1, Dir::Undo), 1, "a continuous drag is one step");
        m.record(1, &3, t0 + Duration::from_millis(1100), K);
        assert_eq!(m.depth(1, Dir::Undo), 2);
        assert_eq!(m.peek(1, Dir::Undo), vec![(1, 3)]);
    }

    #[test]
    fn different_keys_or_no_key_never_coalesce() {
        let mut m = UndoManager::new(UndoConfig::default());
        let t0 = Instant::now();
        m.record(1, &0, t0, Some(1));
        m.record(1, &1, t0 + Duration::from_millis(10), Some(2));
        assert_eq!(m.depth(1, Dir::Undo), 2, "exposure drag then WB click");
        m.record(1, &2, t0 + Duration::from_millis(20), None);
        m.record(1, &3, t0 + Duration::from_millis(30), None);
        assert_eq!(
            m.depth(1, Dir::Undo),
            4,
            "ratings are always their own step"
        );
        m.record(1, &4, t0 + Duration::from_millis(40), Some(2));
        m.record(1, &5, t0 + Duration::from_millis(50), Some(2));
        assert_eq!(m.depth(1, Dir::Undo), 5);
    }

    #[test]
    fn bounded() {
        let mut m = UndoManager::new(UndoConfig {
            max_steps: 3,
            coalesce: Duration::ZERO,
        });
        let t0 = Instant::now();
        for i in 0..10 {
            m.record(1, &i, t0 + Duration::from_secs(i), K);
        }
        assert_eq!(m.depth(1, Dir::Undo), 3);
        assert_eq!(m.peek(1, Dir::Undo)[0].1, 9);
    }

    #[test]
    fn undo_redo_cycle_and_new_edit_clears_redo() {
        let mut m = UndoManager::new(UndoConfig::default());
        let t0 = Instant::now();
        m.record(1, &0, t0, K);
        let p = m.peek(1, Dir::Undo);
        m.commit(Dir::Undo, vec![(1, 10)]);
        assert_eq!(p, vec![(1, 0)]);
        assert_eq!(m.depth(1, Dir::Redo), 1);
        assert_eq!(m.peek(1, Dir::Redo), vec![(1, 10)]);
        m.commit(Dir::Redo, vec![(1, 0)]);
        assert_eq!(m.depth(1, Dir::Undo), 1);
        m.commit(Dir::Undo, vec![(1, 10)]);
        // A new edit right after undo must not coalesce and must drop redo.
        m.record(1, &0, t0 + Duration::from_millis(1), K);
        assert_eq!(m.depth(1, Dir::Redo), 0);
        assert_eq!(m.depth(1, Dir::Undo), 1);
        assert_eq!(m.peek(2, Dir::Undo), [] as [(i64, i32); 0]);
        m.commit(Dir::Undo, vec![(2, 0)]); // unknown variant ignored
        m.forget(1);
        assert_eq!(m.depth(1, Dir::Undo), 0);
    }

    #[test]
    fn groups_span_variants_and_skip_members_edited_since() {
        let mut m = UndoManager::new(UndoConfig::default());
        let g = m.new_group();
        for v in 1..=3 {
            m.record_group(g, v, &v);
        }
        // Variant 3 edited after the batch: its top is no longer the group's.
        m.record(3, &9, Instant::now(), K);
        let plan = m.peek(1, Dir::Undo);
        assert_eq!(plan, vec![(1, 1), (2, 2)]);
        assert_eq!(m.peek_group(g, Dir::Undo), plan);
        m.commit(Dir::Undo, vec![(1, 5), (2, 5)]);
        assert_eq!(m.peek(2, Dir::Redo), vec![(1, 5), (2, 5)]);
        // A slider edit right after a group does not coalesce into it.
        let g2 = m.new_group();
        assert_ne!(g, g2);
        m.record_group(g2, 4, &0);
        m.record(4, &1, Instant::now(), K);
        assert_eq!(m.depth(4, Dir::Undo), 2);
    }

    #[test]
    fn redo_respects_bound() {
        let mut m = UndoManager::new(UndoConfig {
            max_steps: 1,
            coalesce: Duration::ZERO,
        });
        let t = Instant::now();
        m.record(1, &0, t, K);
        m.commit(Dir::Undo, vec![(1, 1)]);
        m.record(1, &2, t, K);
        assert_eq!(m.depth(1, Dir::Undo), 1);
        m.histories.get_mut(&1).unwrap().redo.push(Entry {
            state: 3,
            group: None,
        });
        m.commit(Dir::Redo, vec![(1, 4)]);
        assert_eq!(m.depth(1, Dir::Undo), 1);
        assert_eq!(m.peek(1, Dir::Undo), vec![(1, 4)]);
    }
}
