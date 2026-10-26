//! Apply a Lightroom source to a Luxtable session.
//!
//! Every field (rating, colour tag, pick, keywords, title, caption, copyright)
//! and every develop category (crop, tone, white balance, colour, detail,
//! effects, lens, masks) is merged three ways: Luxtable's current value,
//! Lightroom's value, and what the last import wrote ([`crate::state`]):
//!
//! | current = Lightroom | in sync, nothing to do |
//! |---|---|
//! | Lightroom = last import | Lightroom unchanged: Luxtable's value is kept (it may be an edit) |
//! | current = last import | Luxtable untouched since: Lightroom's new value is written |
//! | no record, Lightroom = default | Lightroom has nothing to say: left alone |
//! | no record, current = default | first import: written |
//! | otherwise | **conflict**: both changed; kept unless `overwrite_edits` |
//!
//! So a re-run updates instead of duplicating, and a Luxtable edit made
//! after an import is never replaced without the caller asking. A category
//! the caller leaves out is neither compared nor written nor recorded.
//! Photos are matched to the session's by [`crate::matching`]; virtual copies
//! become extra variants of the master's photo.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lt_core::{Adjustments, ColorTag, GeoPoint, IptcMeta, PickState, Rating};
use lt_session::{AlbumId, ImageId, ImageInfo, Session, VariantId};
use serde_json::Value;

use crate::error::{LrError, Result};
use crate::flavor::Flavor;
use crate::groups;
use crate::journal::{Change, Created, Journal, Place, Writer, diff};
use crate::map::{MapContext, Mapped};
use crate::matching::{By, match_photos};
use crate::model::{Category, Fidelity, LrCollection, LrLibrary, LrPhoto};
use crate::state::{self, Record, State};

/// Most photos with per-photo notes in a report.
const MAX_NOTES: usize = 200;
/// Per-photo failures spelled out in the warnings; the rest are counted.
const MAX_PHOTO_WARNINGS: usize = 200;

/// Extensions of files that are not RAW (Lightroom's temperature is not Kelvin for them).
const NOT_RAW: [&str; 9] = [
    "jpg", "jpeg", "png", "tif", "tiff", "heic", "heif", "webp", "psd",
];

#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Compute the report only; write nothing.
    pub dry_run: bool,
    /// Resolve conflicts in Lightroom's favour (the user confirmed it).
    pub overwrite_edits: bool,
    /// What to bring over; the rest is left alone.
    pub categories: BTreeSet<Category>,
    /// The application the library comes from.
    pub flavor: Flavor,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            overwrite_edits: false,
            categories: Category::ALL.into_iter().collect(),
            flavor: Flavor::LIGHTROOM,
        }
    }
}

/// A field both Luxtable and Lightroom changed since the last import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub variant: VariantId,
    /// The photo's path in the session.
    pub file: String,
    /// `rating`, `title`, `caption`, `copyright`, or a [`Category::key`].
    pub field: String,
}

/// Something not imported, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// A source photo with no Luxtable photo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFound {
    /// The path the source records.
    pub path: String,
    /// Its folder.
    pub folder: String,
}

/// A folder with source photos that are in no Luxtable photo of this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderCount {
    pub folder: String,
    pub photos: usize,
}

/// What an import found out about one photo, in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoNote {
    pub file: String,
    pub notes: Vec<String>,
}

/// What an import did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    pub dry_run: bool,
    /// The Lightroom version that wrote the source, when it says.
    pub written_by: Option<String>,
    /// Photos in the source (virtual copies included).
    pub photos: usize,
    /// Photos found in the Luxtable session (virtual copies not counted).
    pub matched: usize,
    pub matched_by_path: usize,
    pub matched_by_hash: usize,
    pub matched_by_name: usize,
    /// Paths of photos that could be several Luxtable photos (not imported).
    pub ambiguous: Vec<String>,
    pub not_found: Vec<NotFound>,
    /// `not_found` by folder, most photos first.
    pub not_found_folders: Vec<FolderCount>,
    /// Lightroom variants matched to (or created as) Luxtable variants.
    pub variants: usize,
    pub variants_created: usize,
    /// Per category: variants it was (would be) written to.
    pub changed: BTreeMap<Category, usize>,
    /// Mapped setting → (fidelity, variants that have it).
    pub mapped_keys: BTreeMap<String, (Fidelity, usize)>,
    /// Setting with no translation → (reason, variants that have it).
    pub dropped: BTreeMap<String, (String, usize)>,
    /// Variants with at least one dropped setting.
    pub with_dropped: usize,
    /// Colour labels with no Luxtable colour → photos.
    pub unmapped_labels: BTreeMap<String, usize>,
    /// Variants where nothing needed writing.
    pub in_sync: usize,
    /// Fields left at Luxtable's value because the source didn't change since the last import.
    pub kept_luxtable: usize,
    /// Source variants whose Luxtable variant the user deleted (not recreated).
    pub deleted_in_luxtable: usize,
    /// Conflicts left alone (all of them when not overwriting).
    pub conflicts: Vec<Conflict>,
    /// Conflicts resolved in Lightroom's favour (`overwrite_edits`).
    pub overwritten: usize,
    /// Variants whose sidecar is from a newer Luxtable (never written).
    pub read_only: usize,
    pub albums_created: usize,
    pub albums_updated: usize,
    /// Collections not imported, with the reason.
    pub collections_skipped: Vec<Skipped>,
    /// Per-photo notes (at most 200 photos).
    pub notes: Vec<PhotoNote>,
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
    pub iptc: Vec<(VariantId, IptcMeta)>,
    pub geotags: Vec<(VariantId, Option<GeoPoint>)>,
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
            .chain(self.iptc.iter_mut().map(|(v, _)| v))
            .chain(self.geotags.iter_mut().map(|(v, _)| v))
            .chain(self.adjustments.iter_mut().map(|(v, _)| v))
    }
}

/// One variant's current Luxtable state (defaults for one not created yet).
pub(crate) struct Current {
    pub rating: Rating,
    pub color_tag: ColorTag,
    pub pick: PickState,
    pub keywords: Vec<String>,
    pub iptc: IptcMeta,
    pub geotag: Option<GeoPoint>,
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
            iptc: IptcMeta::default(),
            geotag: None,
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
            iptc: info.iptc,
            geotag: info.geotag,
            adjustments: session.load_adjustments(v)?,
            read_only: info.read_only,
        })
    }
}

/// Merge one value; returns whether it is to be written.
fn merge<T: PartialEq + Clone>(
    run: &mut Run<'_>,
    (variant, file, field): (VariantId, &str, &str),
    (cur, new, default): (&T, &T, &T),
    last: &mut Option<T>,
) -> bool {
    match decide(cur, new, last.as_ref(), default) {
        Decision::InSync => {
            if last.is_some() || new != default {
                *last = Some(new.clone());
            }
            false
        }
        Decision::Keep => {
            run.report.kept_luxtable += 1;
            false
        }
        Decision::Skip => false,
        Decision::Write => {
            *last = Some(new.clone());
            true
        }
        Decision::Conflict if run.opts.overwrite_edits => {
            run.report.overwritten += 1;
            *last = Some(new.clone());
            true
        }
        Decision::Conflict => {
            run.report.conflicts.push(Conflict {
                variant,
                file: file.to_owned(),
                field: field.to_owned(),
            });
            run.note(
                file,
                format!("Changed in both: {field} kept as in Luxtable"),
            );
            false
        }
    }
}

/// Import `lib` into `session`. With `dry_run` nothing is written (not even
/// the state). `cancelled` is checked while planning, before anything is written.
pub fn import(
    session: &mut Session,
    lib: &LrLibrary,
    opts: &ImportOptions,
    cancelled: &dyn Fn() -> bool,
) -> Result<ImportReport> {
    let mut report = ImportReport {
        dry_run: opts.dry_run,
        written_by: lib.written_by.clone(),
        photos: lib.photos.len(),
        warnings: lib.warnings.clone(),
        ..ImportReport::default()
    };
    let flavor = opts.flavor;
    let mut state = state::load(session.root(), flavor.state_file).unwrap_or_else(|e| {
        report.warnings.push(format!(
            "{e} (starting over: Luxtable values that differ from {}'s count as edits)",
            flavor.app
        ));
        State::default()
    });
    let state_before = state.clone();
    let albums_by_name =
        forget_other_sources_albums(session, &flavor, lib, &mut state, &mut report);
    let images = session.images()?;
    let mut run = Run {
        session,
        lib,
        opts,
        report,
        state,
        writes: Writes::default(),
        resolved: HashMap::new(),
        pending_new: 0,
        journal: Journal::new(state_before),
        creations: Vec::new(),
    };
    run.plan(&images, cancelled)?;
    // Nothing is written before this point.
    let Run {
        session,
        mut state,
        mut report,
        mut writes,
        mut resolved,
        journal,
        creations,
        ..
    } = run;
    let mut journal = if opts.dry_run {
        Writer::dry()
    } else {
        apply_planned(
            session,
            flavor.journal_file,
            journal,
            creations,
            (&mut writes, &mut resolved, &mut state),
        )?
    };
    if opts.categories.contains(&Category::Collections) {
        import_collections(
            session,
            lib,
            (&mut state, &mut journal, &albums_by_name),
            &resolved,
            opts,
            &mut report,
        )?;
    }
    if !opts.dry_run {
        state.library.clone_from(&lib.id);
        state::save(session.root(), flavor.state_file, &mut state).map_err(|source| {
            LrError::Io {
                path: state::path(session.root(), flavor.state_file),
                source,
            }
        })?;
    }
    Ok(report)
}

/// Album keys are collection ids: meaningless for another source. When the
/// last import was from another catalog, the keys are dropped and the albums
/// an earlier import made are found by name.
fn forget_other_sources_albums(
    session: &Session,
    flavor: &Flavor,
    lib: &LrLibrary,
    state: &mut State,
    report: &mut ImportReport,
) -> HashMap<String, AlbumId> {
    let mut by_name = HashMap::new();
    if let (Some(was), Some(now)) = (&state.library, &lib.id)
        && was != now
    {
        report.warnings.push(format!(
            "This is a different {} than the one last imported; albums are matched by name",
            flavor.library
        ));
        for (_, id) in std::mem::take(&mut state.albums) {
            if let Ok(a) = session.album(AlbumId(id)) {
                by_name.insert(a.name.clone(), AlbumId(id));
            }
        }
    }
    by_name
}

/// Write a plan: the journal is saved before each change (or, for a variant
/// that only gets an id when made, right after), so a failure part-way can
/// still be reverted.
fn apply_planned(
    session: &mut Session,
    journal_file: &'static str,
    journal: Journal,
    creations: Vec<(i64, ImageId, Created)>,
    (writes, resolved, state): (&mut Writes, &mut HashMap<String, VariantId>, &mut State),
) -> Result<Writer> {
    let mut w = Writer::begin(session.root(), journal_file, journal)?;
    let mut real: HashMap<i64, VariantId> = HashMap::new();
    // The journal is written once before the variants are made and once
    // after, not per variant (that rewrites it whole each time). A failure
    // part-way saves it first, with every variant made so far as
    // `created_unwritten`, so it can still be reverted.
    let made = (|| -> Result<()> {
        for (placeholder, image, created) in creations {
            let v = session.new_variant(image)?;
            w.j.created.insert(v.0, created);
            real.insert(placeholder, v);
            let made = Created::of(&Current::of(session, v)?);
            w.j.created_unwritten.insert(v.0, made);
        }
        Ok(())
    })();
    if let Err(e) = made {
        let _ = w.save();
        return Err(e);
    }
    if !real.is_empty() {
        w.save()?;
    }
    let fix = |v: &mut VariantId| {
        if let Some(r) = real.get(&v.0) {
            *v = *r;
        }
    };
    writes.variants_mut().for_each(fix);
    resolved.values_mut().for_each(fix);
    for rec in state.variants.values_mut() {
        if let Some(r) = real.get(&rec.variant) {
            rec.variant = r.0;
        }
    }
    apply(session, writes)?;
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
    lib: &'a LrLibrary,
    opts: &'a ImportOptions,
    report: ImportReport,
    state: State,
    writes: Writes,
    /// Photo key → its Luxtable variant (for albums).
    resolved: HashMap<String, VariantId>,
    /// Ids handed to variants a dry run would create (negative, never real).
    pending_new: i64,
    /// What this run changes in existing variants, for a revert.
    journal: Journal,
    /// Variants to create: (placeholder id, image, what the import leaves in it).
    creations: Vec<(i64, ImageId, Created)>,
}

/// What a variant needs to merge: the Luxtable photo and the Lightroom values.
struct Target<'a> {
    vid: VariantId,
    created: bool,
    image: ImageId,
    file: &'a str,
}

impl Run<'_> {
    fn note(&mut self, file: &str, text: String) {
        let notes = &mut self.report.notes;
        if let Some(n) = notes.last_mut().filter(|n| n.file == file) {
            n.notes.push(text);
        } else if notes.len() < MAX_NOTES {
            notes.push(PhotoNote {
                file: file.to_owned(),
                notes: vec![text],
            });
        }
    }

    fn enabled(&self, c: Category) -> bool {
        self.opts.categories.contains(&c)
    }

    /// Plan every source photo (nothing is written).
    fn plan(&mut self, images: &[ImageInfo], cancelled: &dyn Fn() -> bool) -> Result<()> {
        let lib = self.lib;
        let m = match_photos(self.session, &lib.photos, images, cancelled)?;
        self.report.ambiguous = m.ambiguous;
        self.report.warnings.extend(m.warnings);
        let by_id: HashMap<ImageId, &ImageInfo> = images.iter().map(|i| (i.id, i)).collect();
        let mut copies: HashMap<&str, Vec<&LrPhoto>> = HashMap::new();
        let keys: BTreeSet<&str> = lib.photos.iter().map(|p| p.key.as_str()).collect();
        let mut orphans = 0usize;
        for p in &lib.photos {
            if let Some(master) = p.master.as_deref() {
                if keys.contains(master) {
                    copies.entry(master).or_default().push(p);
                } else {
                    orphans += 1;
                }
            }
        }
        if orphans > 0 {
            self.report.warnings.push(format!(
                "{orphans} virtual cop{} whose master is not in the source: not imported",
                if orphans == 1 { "y" } else { "ies" }
            ));
        }
        for list in copies.values_mut() {
            list.sort_by_key(|p| p.order);
        }
        let mut folders: BTreeMap<String, usize> = BTreeMap::new();
        let ambiguous: HashSet<String> = self.report.ambiguous.iter().cloned().collect();
        let (mut failed, mut shown) = (0usize, 0usize);
        for p in lib.photos.iter().filter(|p| p.master.is_none()) {
            if cancelled() {
                return Err(LrError::Cancelled);
            }
            let Some((id, by)) = m.found.get(&p.key).copied() else {
                if !ambiguous.contains(p.path.to_string_lossy().as_ref()) {
                    let folder = p
                        .folder
                        .as_deref()
                        .or_else(|| p.path.parent())
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    *folders.entry(folder.clone()).or_default() += 1;
                    self.report.not_found.push(NotFound {
                        path: p.path.to_string_lossy().into_owned(),
                        folder,
                    });
                }
                continue;
            };
            self.report.matched += 1;
            match by {
                By::Path => self.report.matched_by_path += 1,
                By::Hash => self.report.matched_by_hash += 1,
                By::Name => self.report.matched_by_name += 1,
            }
            let info = by_id[&id];
            let mut group = vec![p];
            group.extend(copies.get(p.key.as_str()).into_iter().flatten().copied());
            // One unreadable sidecar costs that photo, not the import.
            if let Err(e) = self.image(info, &group) {
                failed += 1;
                if shown < MAX_PHOTO_WARNINGS {
                    shown += 1;
                    self.report
                        .warnings
                        .push(format!("{}: {e} (not imported)", info.rel_path.as_str()));
                }
            }
        }
        if failed > shown {
            self.report
                .warnings
                .push(format!("and {} more photos not imported", failed - shown));
        }
        let mut by_count: Vec<FolderCount> = folders
            .into_iter()
            .map(|(folder, photos)| FolderCount { folder, photos })
            .collect();
        by_count.sort_by(|a, b| {
            b.photos
                .cmp(&a.photos)
                .then_with(|| a.folder.cmp(&b.folder))
        });
        self.report.not_found_folders = by_count;
        Ok(())
    }

    /// The master and its virtual copies against the Luxtable photo's variants.
    fn image(&mut self, info: &ImageInfo, group: &[&LrPhoto]) -> Result<()> {
        let lt_variants = self.session.variants(info.id)?;
        // Every read that can fail comes first: an unreadable sidecar leaves
        // the whole photo out, never half of it.
        let mut current: HashMap<VariantId, Current> = HashMap::new();
        for v in &lt_variants {
            current.insert(*v, Current::of(self.session, *v)?);
        }
        let first_read_only = lt_variants
            .first()
            .is_some_and(|v| current.get(v).is_some_and(|c| c.read_only));
        let mut claimed: Vec<VariantId> = group
            .iter()
            .filter_map(|p| self.state.variants.get(&p.key))
            .map(|r| VariantId(r.variant))
            .filter(|v| lt_variants.contains(v))
            .collect();
        let rel = info.rel_path.as_str();
        let file = rel.strip_prefix("Capture/").unwrap_or(rel).to_owned();
        let is_raw = !std::path::Path::new(rel)
            .extension()
            .is_some_and(|e| NOT_RAW.iter().any(|n| e.eq_ignore_ascii_case(n)));
        for photo in group {
            let record = self.state.variants.get(&photo.key).cloned();
            let target = match &record {
                Some(r) if lt_variants.contains(&VariantId(r.variant)) => {
                    Some(VariantId(r.variant))
                }
                Some(_) => {
                    self.report.deleted_in_luxtable += 1;
                    continue;
                }
                // Only the photo itself takes over a variant it has no record
                // of; a virtual copy is a new variant, never one the user made.
                None if photo.master.is_none() => lt_variants
                    .first()
                    .copied()
                    .filter(|v| !claimed.contains(v)),
                None => None,
            };
            let mapped = photo.develop.as_ref().map(|d| {
                (self.opts.flavor.map)(
                    d,
                    &MapContext {
                        meta: &info.meta,
                        is_raw,
                        lr_orientation: photo.orientation,
                    },
                )
            });
            let (vid, created, cur) = match target {
                Some(v) => (v, false, current.remove(&v).expect("loaded above")),
                None if first_read_only => {
                    self.count(&file, photo, mapped.as_ref());
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
            self.resolved.insert(photo.key.clone(), vid);
            self.count(&file, photo, mapped.as_ref());
            if cur.read_only {
                self.report.read_only += 1;
                continue;
            }
            let mut rec = record.unwrap_or_default();
            rec.variant = vid.0;
            rel.clone_into(&mut rec.rel);
            let t = Target {
                vid,
                created,
                image: info.id,
                file: &file,
            };
            self.variant(&mut rec, &t, &cur, photo, mapped.as_ref());
            if rec.is_blank() {
                self.state.variants.remove(&photo.key);
            } else {
                self.state.variants.insert(photo.key.clone(), rec);
            }
        }
        Ok(())
    }

    /// Tally what the photo's settings did and notes about it.
    fn count(&mut self, file: &str, photo: &LrPhoto, mapped: Option<&Mapped>) {
        if let Some(label) = &photo.unmapped_label {
            *self
                .report
                .unmapped_labels
                .entry(label.clone())
                .or_default() += 1;
        }
        let Some(m) = mapped else { return };
        let mut approximate: Vec<&str> = Vec::new();
        for (k, f) in &m.mapped {
            let e = self.report.mapped_keys.entry(k.clone()).or_insert((*f, 0));
            e.1 += 1;
            if *f == Fidelity::Approximate {
                approximate.push(k);
            }
        }
        for (k, reason) in &m.dropped {
            let e = self
                .report
                .dropped
                .entry(k.clone())
                .or_insert_with(|| (reason.clone(), 0));
            e.1 += 1;
        }
        self.report.with_dropped += usize::from(!m.dropped.is_empty());
        if !approximate.is_empty() {
            self.note(file, format!("Approximate: {}", approximate.join(", ")));
        }
        for (k, reason) in &m.dropped {
            self.note(file, format!("Not imported: {k} ({reason})"));
        }
    }

    /// Merge one variant's fields and categories, queueing the writes.
    #[allow(clippy::too_many_lines)] // one flat list of field merges, kept in one place
    fn variant(
        &mut self,
        rec: &mut Record,
        t: &Target<'_>,
        cur: &Current,
        photo: &LrPhoto,
        mapped: Option<&Mapped>,
    ) {
        let (vid, file) = (t.vid, t.file);
        let mut scratch = Writes::default();
        let mut ch = Change::default();
        let mut changed: Vec<Category> = Vec::new();
        if let (true, Some(r)) = (self.enabled(Category::Rating), photo.rating)
            && merge(
                self,
                (vid, file, "rating"),
                (&cur.rating, &r, &Rating::default()),
                &mut rec.rating,
            )
        {
            scratch.ratings.entry(r).or_default().push(vid);
            (ch.before.rating, ch.after.rating) = (Some(cur.rating), Some(r));
            changed.push(Category::Rating);
        }
        if let (true, Some(c)) = (self.enabled(Category::Label), photo.color_tag)
            && merge(
                self,
                (vid, file, Category::Label.key()),
                (&cur.color_tag, &c, &ColorTag::None),
                &mut rec.color_tag,
            )
        {
            scratch.tags.entry(c).or_default().push(vid);
            (ch.before.color_tag, ch.after.color_tag) = (Some(cur.color_tag), Some(c));
            changed.push(Category::Label);
        }
        if let (true, Some(p)) = (self.enabled(Category::Flag), photo.pick)
            && merge(
                self,
                (vid, file, Category::Flag.key()),
                (&cur.pick, &p, &PickState::Unflagged),
                &mut rec.pick,
            )
        {
            scratch.picks.entry(p).or_default().push(vid);
            (ch.before.pick, ch.after.pick) = (Some(cur.pick), Some(p));
            changed.push(Category::Flag);
        }
        if let (true, Some(k)) = (self.enabled(Category::Keywords), &photo.keywords) {
            let new = normalized(k);
            if merge(
                self,
                (vid, file, Category::Keywords.key()),
                (&normalized(&cur.keywords), &new, &Vec::new()),
                &mut rec.keywords,
            ) {
                ch.before.keywords = Some(cur.keywords.clone());
                ch.after.keywords = Some(new.clone());
                scratch.keywords.entry(new).or_default().push(vid);
                changed.push(Category::Keywords);
            }
        }
        let mut iptc = cur.iptc.clone();
        let mut iptc_written = false;
        if self.enabled(Category::Metadata) {
            let fields = [
                ("title", &photo.title, &cur.iptc.title, &mut rec.title),
                (
                    "caption",
                    &photo.caption,
                    &cur.iptc.caption,
                    &mut rec.caption,
                ),
                (
                    "copyright",
                    &photo.copyright,
                    &cur.iptc.copyright,
                    &mut rec.copyright,
                ),
            ];
            let mut news: Vec<(&str, String)> = Vec::new();
            for (name, new, now, last) in fields {
                let Some(new) = new else { continue };
                let new = new.trim().to_owned();
                if merge(
                    self,
                    (vid, file, name),
                    (&now.trim().to_owned(), &new, &String::new()),
                    last,
                ) {
                    news.push((name, new));
                }
            }
            for (name, new) in news {
                let (slot, before, after) = match name {
                    "title" => (&mut iptc.title, &mut ch.before.title, &mut ch.after.title),
                    "caption" => (
                        &mut iptc.caption,
                        &mut ch.before.caption,
                        &mut ch.after.caption,
                    ),
                    _ => (
                        &mut iptc.copyright,
                        &mut ch.before.copyright,
                        &mut ch.after.copyright,
                    ),
                };
                *before = Some(slot.clone());
                *after = Some(new.clone());
                *slot = new;
                iptc_written = true;
            }
            // Where it was taken belongs to the photo (every variant shares it).
            if let (None, Some(g)) = (&photo.master, photo.geotag) {
                let mut last = rec.geotag.map(Some);
                if merge(
                    self,
                    (vid, file, "location"),
                    (&cur.geotag, &Some(g), &None),
                    &mut last,
                ) {
                    ch.before.geotag = Some(Place { point: cur.geotag });
                    ch.after.geotag = Some(Place { point: Some(g) });
                    scratch.geotags.push((vid, Some(g)));
                }
                rec.geotag = last.flatten();
            }
            if iptc_written {
                scratch.iptc.push((vid, iptc.clone()));
            }
            if iptc_written || !scratch.geotags.is_empty() {
                changed.push(Category::Metadata);
            }
        }
        let mut after_adj = cur.adjustments.clone();
        if let Some(m) = mapped {
            for cat in Category::ALL.into_iter().filter(|c| c.is_develop()) {
                let key = cat.key();
                if !self.enabled(cat)
                    || !(m.categories.contains(&cat) || rec.groups.contains_key(key))
                {
                    continue;
                }
                // The mapped layers are all the import's, whatever their ids.
                let mapped_ids: Vec<u64> = m.adjustments.layers.iter().map(|l| l.id).collect();
                let new = groups::slice(cat, &m.adjustments, &mapped_ids);
                let now = groups::slice(cat, &cur.adjustments, &rec.layers);
                let default = groups::slice(cat, &Adjustments::default(), &[]);
                let mut last: Option<Value> = rec.groups.get(key).cloned();
                if merge(self, (vid, file, key), (&now, &new, &default), &mut last) {
                    let (a, ids) = groups::splice(cat, &after_adj, &new, &rec.layers);
                    after_adj = a;
                    if cat == Category::Masks {
                        rec.layers = ids;
                    }
                    changed.push(cat);
                }
                match last {
                    Some(v) => rec.groups.insert(key.to_owned(), v),
                    None => rec.groups.remove(key),
                };
            }
        }
        let adjusted = after_adj != cur.adjustments;
        if adjusted {
            ch.adjustments = diff(&cur.adjustments, &after_adj);
            scratch.adjustments.push((vid, after_adj.clone()));
        }
        for c in &changed {
            *self.report.changed.entry(*c).or_default() += 1;
        }
        self.report.in_sync += usize::from(changed.is_empty());
        if t.created {
            // Starts from defaults (not the copy of the first variant that a
            // new variant gets): every field is written.
            let a = &ch.after;
            let made = Created {
                rating: a.rating.unwrap_or_default(),
                color_tag: a.color_tag.unwrap_or_default(),
                pick: a.pick.unwrap_or_default(),
                keywords: a.keywords.clone().unwrap_or_default(),
                title: a.title.clone().unwrap_or_default(),
                caption: a.caption.clone().unwrap_or_default(),
                copyright: a.copyright.clone().unwrap_or_default(),
                adjustments: after_adj,
            };
            self.creations.push((vid.0, t.image, made.clone()));
            let w = &mut self.writes;
            w.ratings.entry(made.rating).or_default().push(vid);
            w.tags.entry(made.color_tag).or_default().push(vid);
            w.picks.entry(made.pick).or_default().push(vid);
            w.keywords.entry(made.keywords).or_default().push(vid);
            w.iptc.push((
                vid,
                IptcMeta {
                    title: made.title,
                    caption: made.caption,
                    copyright: made.copyright,
                    ..IptcMeta::default()
                },
            ));
            w.adjustments.push((vid, made.adjustments));
            w.geotags.extend(scratch.geotags);
        } else if !changed.is_empty() {
            let w = &mut self.writes;
            for (r, vs) in scratch.ratings {
                w.ratings.entry(r).or_default().extend(vs);
            }
            for (c, vs) in scratch.tags {
                w.tags.entry(c).or_default().extend(vs);
            }
            for (p, vs) in scratch.picks {
                w.picks.entry(p).or_default().extend(vs);
            }
            for (k, vs) in scratch.keywords {
                w.keywords.entry(k).or_default().extend(vs);
            }
            w.iptc.extend(scratch.iptc);
            w.geotags.extend(scratch.geotags);
            w.adjustments.extend(scratch.adjustments);
            self.journal.variants.insert(vid.0, ch);
        }
    }
}

fn normalized(k: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    k.iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && seen.insert(s))
        .map(str::to_owned)
        .collect()
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
    for (v, iptc) in &w.iptc {
        session.set_iptc(&[*v], iptc)?;
    }
    for (v, g) in &w.geotags {
        session.set_geotag(&[*v], *g)?;
    }
    if !w.adjustments.is_empty() {
        session.apply_batch(&w.adjustments)?;
    }
    Ok(())
}

fn album_name(c: &LrCollection, flavor: &Flavor) -> String {
    let n = c.display_name();
    let n = n.trim();
    if n.is_empty() {
        flavor.unnamed_album.to_owned()
    } else {
        n.to_owned()
    }
}

fn import_collections(
    session: &mut Session,
    lib: &LrLibrary,
    (state, journal, by_name): (&mut State, &mut Writer, &HashMap<String, AlbumId>),
    resolved: &HashMap<String, VariantId>,
    opts: &ImportOptions,
    report: &mut ImportReport,
) -> Result<()> {
    for coll in &lib.collections {
        let key = format!("col:{}", coll.id);
        let name = album_name(coll, &opts.flavor);
        let skip = match (&coll.unsupported, coll.smart) {
            (Some(why), _) => Some(why.clone()),
            (None, true) => Some(
                "a smart collection (a saved search): Luxtable can't read Lightroom's rules".into(),
            ),
            (None, false) => None,
        };
        if let Some(reason) = skip {
            report.collections_skipped.push(Skipped {
                name: coll.display_name(),
                reason,
            });
            continue;
        }
        let existing = state.albums.get(&key).map(|id| AlbumId(*id)).or_else(|| {
            let id = *by_name.get(&name)?;
            state.albums.insert(key.clone(), id.0);
            Some(id)
        });
        if let Some(id) = existing
            && session.album(id).is_err()
        {
            report.collections_skipped.push(Skipped {
                name: coll.display_name(),
                reason: "deleted in Luxtable; not recreated".into(),
            });
            continue;
        }
        let mut members: Vec<VariantId> = Vec::new();
        let mut seen: HashSet<VariantId> = HashSet::new();
        for k in &coll.members {
            if let Some(v) = resolved.get(k).filter(|v| seen.insert(**v)) {
                members.push(*v);
            }
        }
        if let Some(id) = existing {
            let have: HashSet<VariantId> = session
                .album_variants(id)
                .unwrap_or_default()
                .into_iter()
                .collect();
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
                let id = session.create_album(&name)?;
                journal.j.albums_created.push(id.0);
                journal.j.added_members(id, &members);
                journal.save()?;
                session.add_to_album(id, &members)?;
                state.albums.insert(key, id.0);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
