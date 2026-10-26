//! Adjustment edits, batch copy/apply, and undo/redo.

use std::collections::HashMap;

use lt_core::clipboard::Clipboard;
use lt_core::{AdjustGroup, Adjustments, PasteOptions, PasteTarget, PasteWarning};

use crate::error::Result;
use crate::preset::Preset;
use crate::session::{Session, UndoState, VariantId};
use crate::sidecar::ImageMetaLite;
use crate::undo::{Dir, GroupId};

/// Outcome of a batch copy/apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchApplied {
    /// The undo step, or `None` if nothing changed.
    pub group: Option<GroupId>,
    /// Targets where a relative white-balance paste had to fall back to
    /// absolute because an as-shot white balance was unknown.
    pub wb_absolute_fallback: Vec<VariantId>,
}

/// Coalescing key for an adjustments edit: which tool groups it touches.
/// Dragging one slider keeps the key; switching tools starts a new step.
fn groups_key(a: &Adjustments, b: &Adjustments) -> u64 {
    AdjustGroup::changed(a, b)
        .into_iter()
        .filter_map(|g| AdjustGroup::ALL.iter().position(|x| *x == g))
        .fold(0, |k, i| k | 1 << i)
}

impl Session {
    /// Save a variant's adjustments (atomic sidecar write + index update).
    /// Recorded for undo; rapid successive saves touching the same tools (a
    /// slider drag) coalesce into one step.
    pub fn save_adjustments(&mut self, v: VariantId, adj: &Adjustments) -> Result<()> {
        let now = (self.clock)();
        let mut key = 0;
        let (written, err) = self.modify_collect(&[v], |_, sc| {
            key = groups_key(&sc.adjustments, adj);
            let changed = sc.adjustments != *adj;
            sc.adjustments.clone_from(adj);
            changed
        })?;
        for (id, before) in &written {
            self.undo.record(id.0, before, now, Some(key));
        }
        err.map_or(Ok(()), Err)
    }

    /// Set different adjustments on many variants as a single undo step.
    /// Returns the group, or `None` if nothing changed.
    pub fn apply_batch(&mut self, edits: &[(VariantId, Adjustments)]) -> Result<Option<GroupId>> {
        let map: HashMap<VariantId, &Adjustments> = edits.iter().map(|(v, a)| (*v, a)).collect();
        let ids: Vec<VariantId> = edits.iter().map(|(v, _)| *v).collect();
        self.grouped(&ids, |v, _, _| map[&v].clone())
    }

    /// A clipboard holding `v`'s adjustments, with its oriented size and
    /// as-shot white balance (for aspect-aware crop and relative WB paste).
    pub fn copy_adjustments(&self, v: VariantId) -> Result<Clipboard> {
        let loc = self.locate(v)?;
        let sc = self.read_sidecar(&loc.sidecar)?.sidecar;
        let mut clip = Clipboard::new(
            sc.adjustments,
            Adjustments::default(),
            sc.meta.oriented_size(),
        );
        clip.source_as_shot = sc.meta.as_shot_wb;
        Ok(clip)
    }

    /// Copy & Apply: paste `clip` onto every target (aspect-aware crop uses each
    /// target's oriented size, relative WB its as-shot WB). One undo step.
    pub fn apply_clipboard(
        &mut self,
        clip: &Clipboard,
        targets: &[VariantId],
        opts: &PasteOptions,
    ) -> Result<BatchApplied> {
        let mut fallback = Vec::new();
        let group = self.grouped(targets, |v, cur, meta| {
            let t = PasteTarget {
                size: meta.oriented_size(),
                as_shot: meta.as_shot_wb,
            };
            let pasted = clip.apply_to(cur, &t, opts);
            if pasted
                .warnings
                .contains(&PasteWarning::WhiteBalanceAbsoluteFallback)
            {
                fallback.push(v);
            }
            pasted.adjustments
        })?;
        Ok(BatchApplied {
            group,
            wb_absolute_fallback: fallback,
        })
    }

    /// Apply a preset/style to every target. One undo step.
    pub fn apply_preset(
        &mut self,
        preset: &Preset,
        targets: &[VariantId],
    ) -> Result<Option<GroupId>> {
        self.grouped(targets, |_, cur, meta| {
            preset.apply(cur, meta.oriented_size())
        })
    }

    /// Rewrite each target's adjustments with `f(variant, current, metadata)`
    /// and record every changed variant under one new undo group.
    fn grouped(
        &mut self,
        ids: &[VariantId],
        mut f: impl FnMut(VariantId, &Adjustments, &ImageMetaLite) -> Adjustments,
    ) -> Result<Option<GroupId>> {
        let (written, err) = self.modify_collect(ids, |v, sc| {
            let next = f(v, &sc.adjustments, &sc.meta);
            let changed = next != sc.adjustments;
            sc.adjustments = next;
            changed
        })?;
        let group = (!written.is_empty()).then(|| self.undo.new_group());
        if let Some(g) = group {
            for (id, before) in &written {
                self.undo.record_group(g, id.0, before);
            }
        }
        err.map_or(Ok(group), Err)
    }

    /// Undo the variant's last step (an edit, rating, tag, pick, or keyword
    /// change). If that step was a batch, every variant in the batch (not
    /// edited since) is reverted together. Returns whether anything was undone.
    pub fn undo(&mut self, v: VariantId) -> Result<bool> {
        let plan = self.undo.peek(v.0, Dir::Undo);
        self.run_plan(plan, Dir::Undo)
    }

    pub fn redo(&mut self, v: VariantId) -> Result<bool> {
        let plan = self.undo.peek(v.0, Dir::Redo);
        self.run_plan(plan, Dir::Redo)
    }

    /// Undo a whole batch step by its group id (e.g. from the browser's ⌘Z).
    pub fn undo_group(&mut self, g: GroupId) -> Result<bool> {
        let plan = self.undo.peek_group(g, Dir::Undo);
        self.run_plan(plan, Dir::Undo)
    }

    pub fn redo_group(&mut self, g: GroupId) -> Result<bool> {
        let plan = self.undo.peek_group(g, Dir::Redo);
        self.run_plan(plan, Dir::Redo)
    }

    pub fn undo_depth(&self, v: VariantId) -> usize {
        self.undo.depth(v.0, Dir::Undo)
    }

    pub fn redo_depth(&self, v: VariantId) -> usize {
        self.undo.depth(v.0, Dir::Redo)
    }

    fn run_plan(&mut self, plan: Vec<(i64, UndoState)>, dir: Dir) -> Result<bool> {
        if plan.is_empty() {
            return Ok(false);
        }
        let ids: Vec<VariantId> = plan.iter().map(|(v, _)| VariantId(*v)).collect();
        let mut targets: HashMap<i64, UndoState> = plan.into_iter().collect();
        let (written, err) = self.modify_collect(&ids, |v, sc| {
            if let Some(t) = targets.remove(&v.0) {
                t.restore(sc);
            }
            // Always write so the history pops even if the state already matches.
            true
        })?;
        self.undo.commit(
            dir,
            written
                .into_iter()
                .map(|(v, before)| (v.0, before))
                .collect(),
        );
        err.map_or(Ok(true), Err)
    }
}
