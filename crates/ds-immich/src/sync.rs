//! One sync run: which Immich albums should hold which archived photos, the
//! diff against what the albums hold, and the bookkeeping that keeps Darkslide
//! to its own changes.
//!
//! Darkslide only ever removes album members it added itself (recorded per
//! album and per source in [`SyncState`]): photos someone added by hand, and
//! members the album already had before Darkslide managed it, are never
//! touched.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ImmichError;
use crate::client::{Album, BULK_LIMIT, BulkOutcome, Client};
use crate::mapping::{immich_components, immich_under};

/// Shown when the API key may not start a library scan.
pub const SCAN_NOTE: &str = "Only an Immich admin's API key can start a library scan; \
     Immich will pick up newly archived photos on its own schedule.";

/// What Darkslide did in one Immich album.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumRecord {
    pub id: String,
    /// Source key → ids of the assets Darkslide added for that source.
    pub added: BTreeMap<String, BTreeSet<String>>,
}

/// Persisted between runs (the caller saves it after every run, even a failed
/// one: it is updated as each change succeeds).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    /// The normalized server URL the ids belong to.
    pub server: String,
    /// Album name → record.
    pub albums: BTreeMap<String, AlbumRecord>,
}

impl SyncState {
    /// The state saved at `path`; a missing or unreadable (corrupt) file is a
    /// fresh state (nothing is ever removed then, only possibly re-added).
    pub fn load(path: &Path) -> Result<Self, ImmichError> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ImmichError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Write atomically, creating the parent folder.
    pub fn save(&self, path: &Path) -> Result<(), ImmichError> {
        let bytes = serde_json::to_vec_pretty(self).expect("sync state serializes");
        ds_fsutil::write_atomic(path, &bytes).map_err(|source| ImmichError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Forget everything when the ids belong to another server.
    pub fn reset_if_server_changed(&mut self, server: &str) {
        if self.server != server {
            *self = Self {
                server: server.to_owned(),
                albums: BTreeMap::new(),
            };
        }
    }

    fn set_ours(&mut self, album: &str, source: &str, ours: &BTreeSet<String>) {
        if let Some(r) = self.albums.get_mut(album) {
            if ours.is_empty() {
                r.added.remove(source);
            } else {
                r.added.insert(source.to_owned(), ours.clone());
            }
        }
    }
}

/// The Immich paths (`originalPath`) that should be in an album, from one
/// source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub album: String,
    pub paths: Vec<String>,
}

/// One Darkslide session as archived. A source passed to [`sync`] speaks for
/// all of its photos: albums it filled before but no longer lists lose the
/// photos it added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Stable id of the session.
    pub key: String,
    /// Immich path of the session's archived folder.
    pub scope: String,
    pub wanted: Vec<Wanted>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOptions {
    /// Ask Immich to rescan the libraries holding the sources first.
    pub scan: bool,
    /// Description of albums Darkslide creates.
    pub album_description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlbumReport {
    pub name: String,
    pub id: String,
    pub created: bool,
    pub added: usize,
    pub removed: usize,
    /// Wanted photos Immich hasn't indexed yet.
    pub missing: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Libraries asked to rescan.
    pub scanned: usize,
    /// Why no scan was started, when the key may not.
    pub scan_note: Option<String>,
    pub albums: Vec<AlbumReport>,
    /// Wanted photos Immich hasn't indexed yet (a later sync adds them).
    pub missing: usize,
    /// What failed; the rest of the run went on.
    pub failures: Vec<String>,
}

/// The changes one source asks of one album.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Wanted assets not in the album yet (sorted).
    pub add: Vec<String>,
    /// Assets Darkslide added that are no longer wanted (sorted).
    pub remove: Vec<String>,
    /// Wanted paths with no asset yet (not scanned yet).
    pub missing: Vec<String>,
    /// Assets Darkslide added that stay.
    pub keep: BTreeSet<String>,
}

/// The diff for one source and album. `assets_by_path` maps the source's
/// indexed paths to asset ids, `members` is the album now, `ours` what this
/// source added before. Only `ours` is ever removed, and only assets whose
/// path is known (an asset that vanished from the source's folder in Immich
/// is left alone); a wanted asset that already was a member is not claimed.
pub fn plan<S: BuildHasher, T: BuildHasher>(
    wanted_paths: &[String],
    assets_by_path: &HashMap<String, String, S>,
    members: &HashSet<String, T>,
    ours: &BTreeSet<String>,
) -> Plan {
    let mut wanted = BTreeSet::new();
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    for p in wanted_paths {
        match assets_by_path.get(p) {
            Some(id) => {
                wanted.insert(id.clone());
            }
            None if seen.insert(p) => missing.push(p.clone()),
            None => {}
        }
    }
    let known: HashSet<&String> = assets_by_path.values().collect();
    let add = wanted
        .iter()
        .filter(|id| !members.contains(*id))
        .cloned()
        .collect();
    let (remove, keep): (BTreeSet<String>, BTreeSet<String>) = ours
        .iter()
        .filter(|id| members.contains(*id))
        .cloned()
        .partition(|id| known.contains(id) && !wanted.contains(id));
    Plan {
        add,
        remove: remove.into_iter().collect(),
        missing,
        keep,
    }
}

/// Stops the whole run: every later request would fail the same way.
fn fatal(e: &ImmichError) -> bool {
    matches!(
        e,
        ImmichError::Auth(_) | ImmichError::Network(_) | ImmichError::Cancelled
    )
}

/// `Err` when fatal, else a failure line.
fn note(report: &mut SyncReport, e: ImmichError, what: &str) -> Result<(), ImmichError> {
    if fatal(&e) {
        return Err(e);
    }
    report.failures.push(format!("{what}: {e}"));
    Ok(())
}

fn scan(client: &Client, sources: &[Source], report: &mut SyncReport) -> Result<(), ImmichError> {
    let libraries = match client.libraries() {
        Ok(l) => l,
        Err(ImmichError::Forbidden(_)) => {
            report.scan_note = Some(SCAN_NOTE.into());
            return Ok(());
        }
        Err(e) => return note(report, e, "Could not list the Immich libraries"),
    };
    for lib in libraries {
        let holds_a_source = lib
            .import_paths
            .iter()
            .any(|ip| sources.iter().any(|s| immich_under(&s.scope, ip)));
        if !holds_a_source {
            continue;
        }
        match client.scan_library(&lib.id) {
            Ok(()) => report.scanned += 1,
            Err(ImmichError::Forbidden(_)) => report.scan_note = Some(SCAN_NOTE.into()),
            Err(e) => note(report, e, &format!("Could not scan library “{}”", lib.name))?,
        }
    }
    Ok(())
}

/// The album's id: the recorded one while it exists, else an album with that
/// exact name (adopted with nothing recorded as ours), else a new one.
fn resolve(
    client: &Client,
    state: &mut SyncState,
    on_server: &[Album],
    name: &str,
    description: &str,
) -> Result<(String, bool), ImmichError> {
    if let Some(r) = state.albums.get(name)
        && on_server.iter().any(|a| a.id == r.id)
    {
        return Ok((r.id.clone(), false));
    }
    state.albums.remove(name);
    let (id, created) = match on_server.iter().find(|a| a.name == name) {
        Some(a) => (a.id.clone(), false),
        None => (client.create_album(name, description)?.id, true),
    };
    state.albums.insert(
        name.to_owned(),
        AlbumRecord {
            id: id.clone(),
            added: BTreeMap::new(),
        },
    );
    Ok((id, created))
}

struct Run {
    id: String,
    members: HashSet<String>,
    report: AlbumReport,
}

/// Find, adopt or create every album in `names` and read its members.
/// Albums not in `wanted` are only emptied of our photos: never recreated.
fn open_albums(
    client: &Client,
    state: &mut SyncState,
    names: &BTreeSet<String>,
    wanted: &BTreeSet<&str>,
    description: &str,
    report: &mut SyncReport,
) -> Result<BTreeMap<String, Run>, ImmichError> {
    let on_server = client.albums()?;
    let mut runs = BTreeMap::new();
    for name in names {
        let (id, created) = if wanted.contains(name.as_str()) {
            match resolve(client, state, &on_server, name, description) {
                Ok(r) => r,
                Err(e) => {
                    note(report, e, &format!("Album “{name}”"))?;
                    continue;
                }
            }
        } else {
            match state.albums.get(name) {
                Some(r) if on_server.iter().any(|a| a.id == r.id) => (r.id.clone(), false),
                _ => {
                    state.albums.remove(name);
                    continue;
                }
            }
        };
        let members = match client.album_asset_ids(&id) {
            Ok(m) => m.into_iter().collect(),
            Err(e) => {
                note(report, e, &format!("Album “{name}”"))?;
                continue;
            }
        };
        let album = AlbumReport {
            name: name.clone(),
            id: id.clone(),
            created,
            ..AlbumReport::default()
        };
        runs.insert(
            name.clone(),
            Run {
                id,
                members,
                report: album,
            },
        );
    }
    Ok(runs)
}

/// Each source's indexed assets, exact path → id (`None`: unknown this run).
fn index_sources(
    client: &Client,
    sources: &[Source],
    report: &mut SyncReport,
) -> Result<Vec<Option<HashMap<String, String>>>, ImmichError> {
    let mut assets = Vec::new();
    for s in sources {
        let scope = s.scope.trim();
        if immich_components(scope).is_empty() {
            report
                .failures
                .push(format!("Session {}: no archive folder to look in", s.key));
            assets.push(None);
            continue;
        }
        match client.assets_under(scope) {
            Ok(list) => assets.push(Some(
                list.into_iter().map(|a| (a.original_path, a.id)).collect(),
            )),
            Err(e) => {
                note(report, e, &format!("Session {}", s.key))?;
                assets.push(None);
            }
        }
    }
    Ok(assets)
}

/// One source's part in one album.
struct One<'a> {
    key: &'a str,
    paths: &'a [String],
    assets: &'a HashMap<String, String>,
    /// Wanted in this album by any source: never removed.
    protected: &'a HashSet<String>,
}

/// Plan and apply one source's changes to one album, recording what is ours
/// after every request.
fn apply(
    client: &Client,
    state: &mut SyncState,
    run: &mut Run,
    one: &One,
    report: &mut SyncReport,
) -> Result<(), ImmichError> {
    let name = run.report.name.clone();
    let ours = state
        .albums
        .get(&name)
        .and_then(|r| r.added.get(one.key))
        .cloned()
        .unwrap_or_default();
    let p = plan(one.paths, one.assets, &run.members, &ours);
    let (blocked, remove): (Vec<String>, Vec<String>) = p
        .remove
        .into_iter()
        .partition(|id| one.protected.contains(id));
    let mut now: BTreeSet<String> = p
        .keep
        .into_iter()
        .chain(blocked)
        .chain(remove.iter().cloned())
        .collect();
    state.set_ours(&name, one.key, &now);
    run.report.missing += p.missing.len();
    report.missing += p.missing.len();

    for chunk in p.add.chunks(BULK_LIMIT) {
        match client.add_to_album(&run.id, chunk) {
            Ok(o) => {
                run.report.added += o.done.len();
                run.members
                    .extend(o.done.iter().chain(&o.unchanged).cloned());
                now.extend(o.done.iter().cloned());
                state.set_ours(&name, one.key, &now);
                failed(report, &name, "added", &o);
            }
            Err(e) => note(report, e, &format!("Album “{name}”: adding photos"))?,
        }
    }
    for chunk in remove.chunks(BULK_LIMIT) {
        match client.remove_from_album(&run.id, chunk) {
            Ok(o) => {
                run.report.removed += o.done.len();
                for id in o.done.iter().chain(&o.unchanged) {
                    run.members.remove(id);
                    now.remove(id);
                }
                state.set_ours(&name, one.key, &now);
                failed(report, &name, "removed", &o);
            }
            Err(e) => note(report, e, &format!("Album “{name}”: removing photos"))?,
        }
    }
    Ok(())
}

/// Bring the albums the sources want up to date. Fatal errors (API key,
/// network, cancel) end the run with `Err`; anything else is reported in
/// [`SyncReport::failures`] and the run goes on. `state` is updated as each
/// change succeeds, so it is worth saving after an `Err` too.
pub fn sync(
    client: &Client,
    state: &mut SyncState,
    sources: &[Source],
    opts: &SyncOptions,
) -> Result<SyncReport, ImmichError> {
    state.reset_if_server_changed(client.server());
    let mut report = SyncReport::default();
    if opts.scan {
        scan(client, sources, &mut report)?;
    }

    // Per source: album → wanted paths, including albums it filled before
    // and no longer wants (emptied of its photos).
    let mut per_source: Vec<BTreeMap<String, Vec<String>>> = Vec::new();
    for s in sources {
        let mut albums: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for w in &s.wanted {
            albums
                .entry(w.album.clone())
                .or_default()
                .extend(w.paths.iter().cloned());
        }
        for (name, r) in &state.albums {
            if r.added.contains_key(&s.key) {
                albums.entry(name.clone()).or_default();
            }
        }
        per_source.push(albums);
    }
    let wanted_names: BTreeSet<&str> = sources
        .iter()
        .flat_map(|s| s.wanted.iter().map(|w| w.album.as_str()))
        .collect();
    let all_names: BTreeSet<String> = per_source.iter().flat_map(|a| a.keys().cloned()).collect();

    let mut runs = open_albums(
        client,
        state,
        &all_names,
        &wanted_names,
        &opts.album_description,
        &mut report,
    )?;
    let assets = index_sources(client, sources, &mut report)?;

    // Assets any source wants in an album are never taken out of it.
    let mut protected: HashMap<&str, HashSet<String>> = HashMap::new();
    for (albums, map) in per_source.iter().zip(&assets) {
        let Some(map) = map else { continue };
        for (name, paths) in albums {
            let p = protected.entry(name.as_str()).or_default();
            p.extend(paths.iter().filter_map(|path| map.get(path).cloned()));
        }
    }

    for ((s, albums), map) in sources.iter().zip(&per_source).zip(&assets) {
        let Some(map) = map else { continue };
        for (name, paths) in albums {
            if let Some(run) = runs.get_mut(name) {
                let one = One {
                    key: &s.key,
                    paths,
                    assets: map,
                    protected: &protected[name.as_str()],
                };
                apply(client, state, run, &one, &mut report)?;
            }
        }
    }
    state
        .albums
        .retain(|name, r| !r.added.is_empty() || wanted_names.contains(name.as_str()));
    report.albums = runs.into_values().map(|r| r.report).collect();
    Ok(report)
}

fn failed(report: &mut SyncReport, album: &str, verb: &str, o: &BulkOutcome) {
    if o.failed.is_empty() {
        return;
    }
    let reasons: BTreeSet<&str> = o.failed.iter().map(|(_, r)| r.as_str()).collect();
    report.failures.push(format!(
        "Album “{album}”: {} photo(s) could not be {verb} ({})",
        o.failed.len(),
        reasons.into_iter().collect::<Vec<_>>().join(", ")
    ));
}

#[cfg(test)]
mod tests;
