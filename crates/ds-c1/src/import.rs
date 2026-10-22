//! Apply a C1 session to the Darkslide session of the same folder.
//!
//! Every field (rating, colour tag, pick, keywords, mapped adjustments) is
//! merged three ways: Darkslide's current value, C1's value, and what the
//! last import wrote ([`crate::state`]):
//!
//! | current = C1 | in sync, nothing to do |
//! |---|---|
//! | C1 = last import | C1 unchanged: Darkslide's value is kept (it may be an edit) |
//! | current = last import | Darkslide untouched since: C1's new value is written |
//! | no record, C1 = default | C1 has nothing to say: left alone |
//! | no record, current = default | first import: written |
//! | otherwise | **conflict**: both changed; kept unless `overwrite_edits` |
//!
//! So a re-run updates instead of duplicating, and a Darkslide edit made
//! after an import is never replaced without the caller asking.
//! C1's Selects folder maps to *picked* (C1 has no pick/reject flag). Images
//! in C1's Trash are not part of the Darkslide session (its in-place walk
//! skips that folder) and are only counted.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use ds_core::{Adjustments, ColorTag, PickState, Rating};
use ds_session::{AlbumId, Filter, ImageId, ImageInfo, Session, VariantId, builtin_smart_folders};

use crate::criteria::Criteria;
use crate::error::{C1Error, Result};
use crate::journal::{Change, Created, Journal, Writer, diff};
use crate::map::{Mapped, WbConverter, map_variant};
use crate::model::{C1Collection, C1Folder, C1Image, C1Session, CollectionKind};
use crate::state::{self, Record, State, Tools};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// Compute the report only; write nothing.
    pub dry_run: bool,
    /// Resolve conflicts in C1's favour (the user confirmed it).
    pub overwrite_edits: bool,
}

/// A merged field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Rating,
    ColorTag,
    Pick,
    Keywords,
    Adjustments,
}

/// A field both Darkslide and C1 changed since the last import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub variant: VariantId,
    /// The image path relative to the C1 session folder.
    pub file: String,
    pub field: Field,
}

/// Something not imported, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// What an import did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    pub dry_run: bool,
    /// C1 images inside the session folder, outside its Trash and Output.
    pub images: usize,
    /// Of those, found in the Darkslide session.
    pub matched: usize,
    /// C1 variants matched to (or created as) Darkslide variants.
    pub variants: usize,
    pub variants_created: usize,
    /// C1 images with no Darkslide photo (paths relative to the C1 folder).
    pub not_found: Vec<String>,
    /// C1 images in folders outside the session folder.
    pub outside_folder: usize,
    pub in_trash: usize,
    pub in_output: usize,
    pub in_selects: usize,
    /// Variants whose rating / colour tag / pick / keywords were (would be) written.
    pub rated: usize,
    pub tagged: usize,
    pub picked: usize,
    pub keyworded: usize,
    /// Variants whose mapped adjustments were (would be) written.
    pub edits_mapped: usize,
    /// Variants with any edit in C1's adjustment layer.
    pub edited_in_c1: usize,
    /// Variants with at least one edit that has no translation.
    pub with_unmapped: usize,
    /// Mapped edit key → variants that have it.
    pub mapped_keys: BTreeMap<String, usize>,
    /// Unmapped edit key → (reason, variants that have it).
    pub unmapped: BTreeMap<String, (String, usize)>,
    /// Variants where nothing needed writing.
    pub in_sync: usize,
    /// Fields left at Darkslide's value because C1's didn't change since the last import.
    pub kept_darkslide: usize,
    /// C1 variants whose Darkslide variant the user deleted (not recreated).
    pub deleted_in_darkslide: usize,
    /// Conflicts left alone (all of them when not overwriting).
    pub conflicts: Vec<Conflict>,
    /// Conflicts resolved in C1's favour (`overwrite_edits`).
    pub overwritten: usize,
    /// Variants whose sidecar is from a newer Darkslide (never written).
    pub read_only: usize,
    pub albums_created: usize,
    pub albums_updated: usize,
    /// Smart albums and albums not imported, with the reason.
    pub collections_skipped: Vec<Skipped>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    InSync,
    Keep,
    Skip,
    Write,
    Conflict,
}

fn decide<T: PartialEq>(cur: &T, new: &T, last: Option<&T>, default: &T) -> Decision {
    if cur == new {
        Decision::InSync
    } else if let Some(last) = last {
        if new == last {
            Decision::Keep
        } else if cur == last {
            Decision::Write
        } else {
            Decision::Conflict
        }
    } else if new == default {
        Decision::Skip
    } else if cur == default {
        Decision::Write
    } else {
        Decision::Conflict
    }
}

/// The writes of one import, grouped so each value is one batch.
#[derive(Default)]
pub(crate) struct Writes {
    pub ratings: BTreeMap<Rating, Vec<VariantId>>,
    pub tags: HashMap<ColorTag, Vec<VariantId>>,
    pub picks: HashMap<PickState, Vec<VariantId>>,
    pub keywords: BTreeMap<Vec<String>, Vec<VariantId>>,
    pub adjustments: Vec<(VariantId, Adjustments)>,
}

impl Writes {
    fn variants_mut(&mut self) -> impl Iterator<Item = &mut VariantId> {
        self.ratings
            .values_mut()
            .chain(self.tags.values_mut())
            .chain(self.picks.values_mut())
            .chain(self.keywords.values_mut())
            .flatten()
            .chain(self.adjustments.iter_mut().map(|(v, _)| v))
    }
}

/// One variant's current Darkslide state (defaults for one not created yet).
pub(crate) struct Current {
    pub rating: Rating,
    pub color_tag: ColorTag,
    pub pick: PickState,
    pub keywords: Vec<String>,
    pub adjustments: Adjustments,
    read_only: bool,
}

impl Current {
    fn fresh() -> Self {
        Self {
            rating: Rating::default(),
            color_tag: ColorTag::None,
            pick: PickState::Unflagged,
            keywords: Vec::new(),
            adjustments: Adjustments::default(),
            read_only: false,
        }
    }

    pub fn of(session: &Session, v: VariantId) -> Result<Self> {
        let info = session.variant(v)?;
        Ok(Self {
            rating: info.rating,
            color_tag: info.color_tag,
            pick: info.pick,
            keywords: info.keywords,
            adjustments: session.load_adjustments(v)?,
            read_only: info.read_only,
        })
    }
}

/// Merge one field; returns whether it is to be written.
fn merge<T: PartialEq + Clone + Default>(
    report: &mut ImportReport,
    opts: ImportOptions,
    (variant, file, field): (VariantId, &str, Field),
    cur: &T,
    new: &T,
    last: &mut Option<T>,
) -> bool {
    match decide(cur, new, last.as_ref(), &T::default()) {
        Decision::InSync => {
            if last.is_some() || *new != T::default() {
                *last = Some(new.clone());
            }
            false
        }
        Decision::Keep => {
            report.kept_darkslide += 1;
            false
        }
        Decision::Skip => false,
        Decision::Write => {
            *last = Some(new.clone());
            true
        }
        Decision::Conflict if opts.overwrite_edits => {
            report.overwritten += 1;
            *last = Some(new.clone());
            true
        }
        Decision::Conflict => {
            report.conflicts.push(Conflict {
                variant,
                file: file.to_owned(),
                field,
            });
            false
        }
    }
}

/// Import `c1` into `session`, which must be the in-place session of
/// `c1.folder`. With `dry_run` nothing is written (not even the state).
pub fn import(
    session: &mut Session,
    c1: &C1Session,
    wb: &dyn WbConverter,
    opts: ImportOptions,
) -> Result<ImportReport> {
    import_cancellable(session, c1, wb, opts, &|| false)
}

/// [`import`], checking `cancelled` while planning (before anything is written).
pub fn import_cancellable(
    session: &mut Session,
    c1: &C1Session,
    wb: &dyn WbConverter,
    opts: ImportOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<ImportReport> {
    let same = |a: &Path, b: &Path| {
        a.canonicalize()
            .ok()
            .zip(b.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
    };
    if !session.external_root().is_some_and(|r| same(r, &c1.folder)) {
        return Err(C1Error::WrongSession(c1.folder.clone()));
    }
    let mut report = ImportReport {
        dry_run: opts.dry_run,
        warnings: c1.warnings.clone(),
        ..ImportReport::default()
    };
    let mut state = state::load(session.root()).unwrap_or_else(|e| {
        report.warnings.push(format!(
            "{e} (starting over: Darkslide values that differ from C1's count as edits)"
        ));
        State::default()
    });
    let state_before = state.clone();
    let albums_by_name = forget_other_sessions_albums(session, c1, &mut state, &mut report);
    let ds_images: HashMap<String, ImageInfo> = session
        .images()?
        .into_iter()
        .filter_map(|i| {
            let rel = i.rel_path.as_str().strip_prefix("Capture/")?.to_owned();
            Some((rel, i))
        })
        .collect();
    let mut run = Run {
        session,
        c1,
        wb,
        opts,
        report,
        state,
        writes: Writes::default(),
        resolved: HashMap::new(),
        by_uuid: HashMap::new(),
        pending_new: 0,
        journal: Journal::new(state_before),
        creations: Vec::new(),
    };
    run.plan(&ds_images, cancelled)?;
    // Nothing is written before this point.
    let Run {
        session,
        mut state,
        mut report,
        mut writes,
        mut resolved,
        mut by_uuid,
        journal,
        creations,
        ..
    } = run;
    let mut journal = if opts.dry_run {
        Writer::dry()
    } else {
        let mut planned = Planned {
            writes: &mut writes,
            resolved: &mut resolved,
            by_uuid: &mut by_uuid,
            state: &mut state,
        };
        apply_planned(session, journal, creations, &mut planned)?
    };
    import_collections(
        session,
        c1,
        (&mut state, &mut journal, &albums_by_name),
        &resolved,
        &by_uuid,
        opts,
        &mut report,
    )?;
    if !opts.dry_run {
        state.c1_session.clone_from(&c1.uuid);
        state::save(session.root(), &mut state).map_err(|source| C1Error::Io {
            path: state::path(session.root()),
            source,
        })?;
    }
    Ok(report)
}

/// Album keys are C1 row ids: meaningless for another C1 database. When the
/// folder's database is another session than the one last imported, the keys
/// are dropped and the albums an earlier import made are found by name.
fn forget_other_sessions_albums(
    session: &Session,
    c1: &C1Session,
    state: &mut State,
    report: &mut ImportReport,
) -> HashMap<String, AlbumId> {
    let mut by_name = HashMap::new();
    if let (Some(was), Some(now)) = (&state.c1_session, &c1.uuid)
        && was != now
    {
        report.warnings.push(
            "This folder's Capture One database is a different Capture One session than the one \
             last imported; albums are matched by name"
                .to_owned(),
        );
        for (_, id) in std::mem::take(&mut state.albums) {
            if let Ok(a) = session.album(AlbumId(id)) {
                by_name.insert(a.name.clone(), AlbumId(id));
            }
        }
    }
    by_name
}

/// What the plan refers to by variant id (placeholders until created).
struct Planned<'a> {
    writes: &'a mut Writes,
    resolved: &'a mut HashMap<String, Vec<VariantId>>,
    by_uuid: &'a mut HashMap<String, VariantId>,
    state: &'a mut State,
}

/// Write a plan: the journal is saved before each change (or, for a variant
/// that only gets an id when made, right after), so a failure part-way can
/// still be reverted.
fn apply_planned(
    session: &mut Session,
    journal: Journal,
    creations: Vec<(i64, ImageId, Created)>,
    p: &mut Planned<'_>,
) -> Result<Writer> {
    let mut w = Writer::begin(session.root(), journal)?;
    let mut real: HashMap<i64, VariantId> = HashMap::new();
    for (placeholder, image, created) in creations {
        let v = session.new_variant(image)?;
        w.j.created.insert(v.0, created);
        let made = Created::of(&Current::of(session, v)?);
        w.j.created_unwritten.insert(v.0, made);
        w.save()?;
        real.insert(placeholder, v);
    }
    let fix = |v: &mut VariantId| {
        if let Some(r) = real.get(&v.0) {
            *v = *r;
        }
    };
    p.writes.variants_mut().for_each(fix);
    p.resolved.values_mut().flatten().for_each(fix);
    p.by_uuid.values_mut().for_each(fix);
    for rec in p.state.variants.values_mut() {
        if let Some(r) = real.get(&rec.variant) {
            rec.variant = r.0;
        }
    }
    apply(session, p.writes)?;
    for v in real.values() {
        w.j.created_unwritten.remove(&v.0);
    }
    if !real.is_empty() {
        w.save()?;
    }
    Ok(w)
}

/// One import in progress.
struct Run<'a> {
    session: &'a mut Session,
    c1: &'a C1Session,
    wb: &'a dyn WbConverter,
    opts: ImportOptions,
    report: ImportReport,
    state: State,
    writes: Writes,
    /// C1 image path → its Darkslide variants in C1's order (for albums).
    resolved: HashMap<String, Vec<VariantId>>,
    by_uuid: HashMap<String, VariantId>,
    /// Ids handed to variants a dry run would create (negative, never real).
    pending_new: i64,
    /// What this run changes in existing variants, for a revert.
    journal: Journal,
    /// Variants to create: (placeholder id, image, what the import leaves in it).
    creations: Vec<(i64, ImageId, Created)>,
}

impl Run<'_> {
    /// Plan every C1 image (nothing is written).
    fn plan(
        &mut self,
        ds_images: &HashMap<String, ImageInfo>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        for image in &self.c1.images {
            if cancelled() {
                return Err(C1Error::Cancelled);
            }
            let (Some(rel), Some(folder)) = (&image.rel, image.folder()) else {
                self.report.outside_folder += 1;
                continue;
            };
            match folder {
                C1Folder::Trash => self.report.in_trash += 1,
                C1Folder::Output => self.report.in_output += 1,
                C1Folder::Selects | C1Folder::Capture => {
                    self.report.in_selects += usize::from(folder == C1Folder::Selects);
                    self.report.images += 1;
                    match ds_images.get(rel) {
                        // One unreadable sidecar costs that photo, not the folder.
                        Some(info) => {
                            if let Err(e) = self.image(image, rel, folder, info) {
                                self.report
                                    .warnings
                                    .push(format!("{rel}: {e} (not imported)"));
                            }
                        }
                        None => self.report.not_found.push(rel.clone()),
                    }
                }
            }
        }
        Ok(())
    }

    fn image(
        &mut self,
        image: &C1Image,
        rel: &str,
        folder: C1Folder,
        info: &ImageInfo,
    ) -> Result<()> {
        self.report.matched += 1;
        let ds_variants = self.session.variants(info.id)?;
        // Every read that can fail comes first: an unreadable sidecar leaves
        // the whole photo out, never half of it.
        let mut current: HashMap<VariantId, Current> = HashMap::new();
        for v in &ds_variants {
            current.insert(*v, Current::of(self.session, *v)?);
        }
        // A new variant copies the first one's sidecar, which a newer build's can't.
        let first_read_only = ds_variants
            .first()
            .is_some_and(|v| current.get(v).is_some_and(|c| c.read_only));
        let mut claimed: Vec<VariantId> = image
            .variants
            .iter()
            .filter_map(|v| self.state.variants.get(&v.uuid))
            .map(|r| VariantId(r.variant))
            .filter(|v| ds_variants.contains(v))
            .collect();
        for cv in &image.variants {
            let record = self.state.variants.get(&cv.uuid).cloned();
            let target = match &record {
                Some(r) if ds_variants.contains(&VariantId(r.variant)) => {
                    Some(VariantId(r.variant))
                }
                Some(_) => {
                    self.report.deleted_in_darkslide += 1;
                    continue;
                }
                None => ds_variants
                    .get(cv.position)
                    .copied()
                    .filter(|v| !claimed.contains(v)),
            };
            let original = self.c1.folder.join(rel);
            let mapped = map_variant(
                &cv.settings,
                &original,
                &info.meta,
                self.wb,
                &self.c1.keyword_library,
            );
            let (vid, created, cur) = match target {
                Some(v) => (v, false, current.remove(&v).expect("loaded above")),
                None if first_read_only => {
                    count_edits(&mut self.report, &mapped);
                    self.report.read_only += 1;
                    continue;
                }
                // Created when the import is applied (a placeholder id until then).
                None => {
                    self.pending_new -= 1;
                    (VariantId(self.pending_new), true, Current::fresh())
                }
            };
            claimed.push(vid);
            self.report.variants += 1;
            self.report.variants_created += usize::from(created);
            self.resolved.entry(rel.to_owned()).or_default().push(vid);
            self.by_uuid.insert(cv.uuid.clone(), vid);
            count_edits(&mut self.report, &mapped);
            if cur.read_only {
                self.report.read_only += 1;
                continue;
            }
            let mut rec = record.unwrap_or_default();
            rec.variant = vid.0;
            rel.clone_into(&mut rec.rel);
            let pick = if folder == C1Folder::Selects {
                PickState::Picked
            } else {
                PickState::Unflagged
            };
            if created {
                // Starts from defaults (not the copy of the first variant
                // that a new variant gets): every field is written.
                let made = Created {
                    rating: mapped.rating,
                    color_tag: mapped.color_tag,
                    pick,
                    keywords: normalized(&mapped.keywords),
                    adjustments: mapped.apply(&Adjustments::default()),
                };
                self.creations.push((vid.0, info.id, made.clone()));
                let w = &mut self.writes;
                w.ratings.entry(made.rating).or_default().push(vid);
                w.tags.entry(made.color_tag).or_default().push(vid);
                w.picks.entry(made.pick).or_default().push(vid);
                w.keywords.entry(made.keywords).or_default().push(vid);
                w.adjustments.push((vid, made.adjustments));
            }
            self.variant(&mut rec, (vid, rel), &cur, &mapped, pick, created);
            if rec
                == (Record {
                    variant: rec.variant,
                    rel: rec.rel.clone(),
                    ..Record::default()
                })
            {
                self.state.variants.remove(&cv.uuid);
            } else {
                self.state.variants.insert(cv.uuid.clone(), rec);
            }
        }
        Ok(())
    }

    /// Merge one variant's fields, queueing the writes.
    fn variant(
        &mut self,
        rec: &mut Record,
        (vid, rel): (VariantId, &str),
        cur: &Current,
        mapped: &Mapped,
        pick: PickState,
        created: bool,
    ) {
        let (report, opts) = (&mut self.report, self.opts);
        // A created variant's writes are queued with its creation.
        let mut scratch = Writes::default();
        let w = if created {
            &mut scratch
        } else {
            &mut self.writes
        };
        let key = |field| (vid, rel, field);
        let mut any = false;
        let mut ch = Change::default();
        if merge(
            report,
            opts,
            key(Field::Rating),
            &cur.rating,
            &mapped.rating,
            &mut rec.rating,
        ) {
            w.ratings.entry(mapped.rating).or_default().push(vid);
            (ch.before.rating, ch.after.rating) = (Some(cur.rating), Some(mapped.rating));
            report.rated += 1;
            any = true;
        }
        if merge(
            report,
            opts,
            key(Field::ColorTag),
            &cur.color_tag,
            &mapped.color_tag,
            &mut rec.color_tag,
        ) {
            w.tags.entry(mapped.color_tag).or_default().push(vid);
            (ch.before.color_tag, ch.after.color_tag) =
                (Some(cur.color_tag), Some(mapped.color_tag));
            report.tagged += 1;
            any = true;
        }
        if merge(
            report,
            opts,
            key(Field::Pick),
            &cur.pick,
            &pick,
            &mut rec.pick,
        ) {
            w.picks.entry(pick).or_default().push(vid);
            (ch.before.pick, ch.after.pick) = (Some(cur.pick), Some(pick));
            report.picked += 1;
            any = true;
        }
        let keywords = normalized(&mapped.keywords);
        if merge(
            report,
            opts,
            key(Field::Keywords),
            &normalized(&cur.keywords),
            &keywords,
            &mut rec.keywords,
        ) {
            ch.before.keywords = Some(cur.keywords.clone());
            ch.after.keywords = Some(keywords.clone());
            w.keywords.entry(keywords).or_default().push(vid);
            report.keyworded += 1;
            any = true;
        }
        let tools = Tools::of(&mapped.apply(&Adjustments::default()));
        if merge(
            report,
            opts,
            key(Field::Adjustments),
            &Tools::of(&cur.adjustments),
            &tools,
            &mut rec.tools,
        ) {
            let after = mapped.apply(&cur.adjustments);
            ch.adjustments = diff(&cur.adjustments, &after);
            w.adjustments.push((vid, after));
            report.edits_mapped += 1;
            any = true;
        }
        report.in_sync += usize::from(!any);
        if any && !created {
            self.journal.variants.insert(vid.0, ch);
        }
    }
}

fn normalized(k: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in k.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !out.iter().any(|o| o == s) {
            out.push(s.to_owned());
        }
    }
    out
}

fn count_edits(report: &mut ImportReport, m: &Mapped) {
    for k in &m.mapped {
        *report.mapped_keys.entry(k.clone()).or_default() += 1;
    }
    for (k, reason) in &m.unmapped {
        let e = report
            .unmapped
            .entry(k.clone())
            .or_insert_with(|| ((*reason).to_owned(), 0));
        e.1 += 1;
    }
    report.edited_in_c1 += usize::from(!m.mapped.is_empty() || !m.unmapped.is_empty());
    report.with_unmapped += usize::from(!m.unmapped.is_empty());
}

pub(crate) fn apply(session: &mut Session, w: &Writes) -> Result<()> {
    for (r, vs) in &w.ratings {
        session.set_rating(vs, *r)?;
    }
    for (t, vs) in &w.tags {
        session.set_color_tag(vs, *t)?;
    }
    for (p, vs) in &w.picks {
        session.set_pick(vs, *p)?;
    }
    for (k, vs) in &w.keywords {
        session.set_keywords(vs, k)?;
    }
    if !w.adjustments.is_empty() {
        session.apply_batch(&w.adjustments)?;
    }
    Ok(())
}

/// A smart album's search as a Darkslide filter, or why it has none.
fn translate(c: &Criteria) -> std::result::Result<Filter, String> {
    let conds = c
        .conjunction()
        .ok_or_else(|| "its search uses OR groups or parts Darkslide can't read".to_owned())?;
    let mut f = Filter::default();
    for cond in conds {
        match (cond.key.as_str(), cond.operator) {
            ("IB_I_BASIC_RATING", 0) => {
                let r = cond
                    .criterion
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .and_then(Rating::new)
                    .ok_or_else(|| format!("rating {:?} is not 0–5", cond.criterion))?;
                // Darkslide's "unrated" is `max_rating = 0` alone.
                f.min_rating = (r.stars() > 0).then_some(r);
                f.max_rating = Some(r);
            }
            (key, op) => {
                return Err(format!(
                    "condition {key} (operator {op}) has no translation"
                ));
            }
        }
    }
    Ok(f)
}

fn import_collections(
    session: &mut Session,
    c1: &C1Session,
    (state, journal, by_name): (&mut State, &mut Writer, &HashMap<String, AlbumId>),
    resolved: &HashMap<String, Vec<VariantId>>,
    by_uuid: &HashMap<String, VariantId>,
    opts: ImportOptions,
    report: &mut ImportReport,
) -> Result<()> {
    let builtins = builtin_smart_folders(&session.tags_in_use()?);
    for coll in &c1.collections {
        let key = format!("pk:{}", coll.id);
        let name = coll.display_name();
        let skip = |report: &mut ImportReport, reason: String| {
            report.collections_skipped.push(Skipped {
                name: name.clone(),
                reason,
            });
        };
        let existing = state.albums.get(&key).map(|id| AlbumId(*id)).or_else(|| {
            let id = *by_name.get(&album_name(&name))?;
            state.albums.insert(key.clone(), id.0);
            Some(id)
        });
        if let Some(id) = existing
            && session.album(id).is_err()
        {
            skip(report, "deleted in Darkslide; not recreated".into());
            continue;
        }
        match coll.kind {
            CollectionKind::Album => {
                let members = album_members(coll, resolved, by_uuid);
                if let Some(id) = existing {
                    let have = session.album_variants(id).unwrap_or_default();
                    let missing: Vec<VariantId> =
                        members.into_iter().filter(|v| !have.contains(v)).collect();
                    if !missing.is_empty() {
                        report.albums_updated += 1;
                        if !opts.dry_run {
                            journal.j.added_members(id, &missing);
                            journal.save()?;
                            session.add_to_album(id, &missing)?;
                        }
                    }
                } else {
                    report.albums_created += 1;
                    if !opts.dry_run {
                        let id = session.create_album(&album_name(&name))?;
                        journal.j.albums_created.push(id.0);
                        journal.j.added_members(id, &members);
                        journal.save()?;
                        session.add_to_album(id, &members)?;
                        state.albums.insert(key, id.0);
                    }
                }
            }
            CollectionKind::Smart => {
                // Only "rating is N" translates, and Darkslide has those built in.
                let reason = match coll.criteria.as_ref().map(translate) {
                    None => "matches every image; Darkslide lists them all already".to_owned(),
                    Some(Err(why)) => why,
                    Some(Ok(filter)) => builtins.iter().find(|(_, _, f)| *f == filter).map_or_else(
                        || "no Darkslide equivalent".to_owned(),
                        |(_, builtin, _)| {
                            format!(
                                "already in Darkslide as the built-in \u{201c}{builtin}\u{201d}"
                            )
                        },
                    ),
                };
                skip(report, reason);
            }
        }
    }
    Ok(())
}

/// A valid Darkslide album name (C1 allows names Darkslide doesn't: blank).
fn album_name(name: &str) -> String {
    let n = name.trim();
    if n.is_empty() {
        "Capture One album".to_owned()
    } else {
        n.to_owned()
    }
}

fn album_members(
    coll: &C1Collection,
    resolved: &HashMap<String, Vec<VariantId>>,
    by_uuid: &HashMap<String, VariantId>,
) -> Vec<VariantId> {
    let mut out = Vec::new();
    for (rel, variant) in &coll.members {
        let v = match variant {
            Some(u) => by_uuid.get(u).copied(),
            None => resolved.get(rel).and_then(|v| v.first().copied()),
        };
        // A dry run's would-be variants (negative ids) count too.
        if let Some(v) = v.filter(|v| !out.contains(v)) {
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
