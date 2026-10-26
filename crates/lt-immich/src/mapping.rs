//! The archive folder as this Mac sees it (`/Volumes/photos/…`) ↔ the path the
//! Immich server indexes it under (`/mnt/photos/…`), and detecting that pair.
//!
//! Matching is by whole path components, never by string prefix:
//! `/mnt/photos2` is not under `/mnt/photos`.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Client, ImmichError};

/// One folder seen from both sides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathMapping {
    /// The folder as Immich sees it (`/`-separated, absolute).
    pub immich_root: String,
    /// The same folder on this Mac.
    pub local_root: PathBuf,
}

/// How a mapping was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// A file known to be archived was found in Immich.
    Asset,
    /// A library import path is named like a local folder.
    LibraryPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub mapping: PathMapping,
    pub how: Detection,
}

/// The components of an Immich path.
pub(crate) fn immich_components(path: &str) -> Vec<&str> {
    path.split('/').filter(|c| !c.is_empty()).collect()
}

/// `path` is `root` or below it, component-wise.
pub(crate) fn immich_under(path: &str, root: &str) -> bool {
    immich_components(path).starts_with(&immich_components(root))
}

/// The named components of a local path; `None` with `..` or a name that
/// isn't UTF-8.
fn local_names(path: &Path) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n.to_str()?),
            Component::ParentDir => return None,
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    Some(out)
}

fn join_immich(root: &str, parts: &[&str]) -> String {
    let mut out = String::new();
    for c in immich_components(root).iter().chain(parts) {
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// How many trailing components `a` and `b` share.
fn common_suffix(a: &[&str], b: &[&str]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

impl PathMapping {
    /// The Immich path of a local file or folder under `local_root`.
    pub fn to_immich(&self, local: &Path) -> Option<String> {
        let rest = local.strip_prefix(&self.local_root).ok()?;
        Some(join_immich(&self.immich_root, &local_names(rest)?))
    }

    /// The local path of an Immich path under `immich_root`; `None` outside
    /// it or with `.`/`..` components.
    pub fn to_local(&self, immich: &str) -> Option<PathBuf> {
        let path = immich_components(immich);
        let rest = path.strip_prefix(immich_components(&self.immich_root).as_slice())?;
        if rest.iter().any(|c| *c == ".." || *c == ".") {
            return None;
        }
        Some(rest.iter().fold(self.local_root.clone(), |p, c| p.join(c)))
    }

    /// The mapping implied by one file seen from both sides: the longest
    /// common run of trailing components is the shared part, which must hold
    /// at least the file and its folder; what precedes it are the two roots.
    pub fn from_pair(local_file: &Path, immich_file: &str) -> Option<Self> {
        let local = local_names(local_file)?;
        let immich = immich_components(immich_file);
        if immich.iter().any(|c| *c == ".." || *c == ".") {
            return None;
        }
        let k = common_suffix(&local, &immich);
        if k < 2 {
            return None;
        }
        Some(Self {
            immich_root: join_immich("", &immich[..immich.len() - k]),
            local_root: local_file.ancestors().nth(k)?.to_path_buf(),
        })
    }
}

/// Pair an import path with `local_root` or one of its ancestors that has the
/// same folder name (the deepest local match wins): import path `/mnt/photos`
/// and local root `/Volumes/photos/Luxtable` give `/mnt/photos` ↔
/// `/Volumes/photos`.
pub fn guess_from_import_paths(import_paths: &[String], local_root: &Path) -> Option<PathMapping> {
    for dir in local_root.ancestors() {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(ip) = import_paths
            .iter()
            .find(|ip| immich_components(ip).last() == Some(&name))
        {
            return Some(PathMapping {
                immich_root: join_immich(ip, &[]),
                local_root: dir.to_path_buf(),
            });
        }
    }
    None
}

/// Find how Immich sees `local_root` (the archive folder): from a file known
/// to be archived there and already indexed, else from the libraries' import
/// paths (admin keys only; without one this is `Ok(None)`). A mapping that
/// doesn't cover `local_root` is ignored.
pub fn detect(
    client: &Client,
    known_local_file: Option<&Path>,
    local_root: &Path,
) -> Result<Option<Detected>, ImmichError> {
    let covers = |m: &PathMapping| local_root.starts_with(&m.local_root);
    if let Some(file) = known_local_file
        && let Some(name) = file.file_name().and_then(|n| n.to_str())
        && let Some(local) = local_names(file)
    {
        let mut best: Option<(usize, String)> = None;
        for a in client.find_by_file_name(name)? {
            let k = common_suffix(&local, &immich_components(&a.original_path));
            if k >= 2 && best.as_ref().is_none_or(|(b, _)| k > *b) {
                best = Some((k, a.original_path));
            }
        }
        if let Some(mapping) = best
            .and_then(|(_, p)| PathMapping::from_pair(file, &p))
            .filter(covers)
        {
            return Ok(Some(Detected {
                mapping,
                how: Detection::Asset,
            }));
        }
    }
    let libraries = match client.libraries() {
        Ok(l) => l,
        Err(ImmichError::Forbidden(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let paths: Vec<String> = libraries.into_iter().flat_map(|l| l.import_paths).collect();
    Ok(guess_from_import_paths(&paths, local_root)
        .filter(covers)
        .map(|mapping| Detected {
            mapping,
            how: Detection::LibraryPath,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{API_KEY, MockImmich, SERVER};

    fn m() -> PathMapping {
        PathMapping {
            immich_root: "/mnt/photos/".into(),
            local_root: PathBuf::from("/Volumes/photos"),
        }
    }

    #[test]
    fn maps_both_ways() {
        let m = m();
        let local = Path::new("/Volumes/photos/Luxtable/Trip/IMG_1.ARW");
        let immich = m.to_immich(local).unwrap();
        assert_eq!(immich, "/mnt/photos/Luxtable/Trip/IMG_1.ARW");
        assert_eq!(m.to_local(&immich).unwrap(), local);
        assert_eq!(
            m.to_immich(Path::new("/Volumes/photos")).unwrap(),
            "/mnt/photos"
        );
        assert_eq!(
            m.to_local("/mnt/photos/").unwrap(),
            Path::new("/Volumes/photos")
        );
        // Component-wise, not string prefixes.
        assert_eq!(m.to_immich(Path::new("/Volumes/photos2/x.ARW")), None);
        assert_eq!(m.to_local("/mnt/photos2/x.ARW"), None);
        assert_eq!(m.to_local("/elsewhere/x.ARW"), None);
        // Never walk out of the root.
        assert_eq!(m.to_local("/mnt/photos/../etc/passwd"), None);
        assert_eq!(m.to_local("/mnt/photos/./x"), None);
        assert_eq!(m.to_immich(Path::new("/Volumes/photos/a/../../b")), None);
        let root = PathMapping {
            immich_root: "/".into(),
            local_root: PathBuf::from("/Volumes/nas"),
        };
        assert_eq!(root.to_immich(Path::new("/Volumes/nas")).unwrap(), "/");
        assert_eq!(
            root.to_immich(Path::new("/Volumes/nas/a/b.jpg")).unwrap(),
            "/a/b.jpg"
        );
        assert_eq!(
            root.to_local("/a/b.jpg").unwrap(),
            Path::new("/Volumes/nas/a/b.jpg")
        );
    }

    #[test]
    fn mapping_from_one_file_seen_twice() {
        let m = PathMapping::from_pair(
            Path::new("/Volumes/photos/Luxtable/Trip/IMG_1.ARW"),
            "/mnt/photos/Luxtable/Trip/IMG_1.ARW",
        )
        .unwrap();
        assert_eq!(m.immich_root, "/mnt");
        assert_eq!(m.local_root, Path::new("/Volumes"));
        let m = PathMapping::from_pair(
            Path::new("/Volumes/nas/Trip/IMG_1.ARW"),
            "/data/archive/Trip/IMG_1.ARW",
        )
        .unwrap();
        assert_eq!(m.immich_root, "/data/archive");
        assert_eq!(m.local_root, Path::new("/Volumes/nas"));
        assert_eq!(
            m.to_immich(Path::new("/Volumes/nas/Other/a.ARW")).unwrap(),
            "/data/archive/Other/a.ARW"
        );
        // Only the file name in common: too weak to trust.
        assert_eq!(
            PathMapping::from_pair(Path::new("/Volumes/nas/A/IMG_1.ARW"), "/data/B/IMG_1.ARW"),
            None
        );
        assert_eq!(
            PathMapping::from_pair(Path::new("/Volumes/nas/A/x.ARW"), "/data/../A/x.ARW"),
            None
        );
        assert_eq!(
            PathMapping::from_pair(Path::new("/Volumes/../A/x.ARW"), "/data/A/x.ARW"),
            None
        );
        // Everything in common: both roots are `/`.
        let same = PathMapping::from_pair(Path::new("/a/b.ARW"), "/a/b.ARW").unwrap();
        assert_eq!(same.immich_root, "/");
        assert_eq!(same.local_root, Path::new("/"));
    }

    #[test]
    fn guesses_from_import_path_names() {
        let local = Path::new("/Volumes/photos/Luxtable");
        let g =
            guess_from_import_paths(&["/srv/other".into(), "/mnt/photos/".into()], local).unwrap();
        assert_eq!(g.immich_root, "/mnt/photos");
        assert_eq!(g.local_root, Path::new("/Volumes/photos"));
        // The deepest local folder wins.
        let g = guess_from_import_paths(
            &["/mnt/photos".into(), "/mnt/photos/Luxtable".into()],
            local,
        )
        .unwrap();
        assert_eq!(g.immich_root, "/mnt/photos/Luxtable");
        assert_eq!(g.local_root, local);
        assert_eq!(guess_from_import_paths(&["/srv/other".into()], local), None);
        assert_eq!(guess_from_import_paths(&["/".into()], local), None);
        assert!(immich_under("/mnt/photos/a", "/mnt/photos/"));
        assert!(immich_under("/mnt/photos", "/mnt/photos"));
        assert!(!immich_under("/mnt/photos2/a", "/mnt/photos"));
        assert!(immich_under("/anything", "/"));
    }

    #[test]
    fn detects_from_a_known_asset() {
        let mock = MockImmich::new();
        mock.add_asset("/data/old/Trip/IMG_1.ARW");
        mock.add_asset("/mnt/photos/Luxtable/Trip/IMG_1.ARW");
        mock.add_asset("/mnt/photos/Other/IMG_1.ARW");
        let c = Client::new(&mock, SERVER, API_KEY).unwrap();
        let d = detect(
            &c,
            Some(Path::new("/Volumes/photos/Luxtable/Trip/IMG_1.ARW")),
            Path::new("/Volumes/photos/Luxtable"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(d.how, Detection::Asset);
        assert_eq!(d.mapping.immich_root, "/mnt");
        assert_eq!(d.mapping.local_root, Path::new("/Volumes"));
        assert!(mock.log().iter().all(|(_, p)| p != "/libraries"));
    }

    #[test]
    fn falls_back_to_library_paths() {
        let mock = MockImmich::new();
        mock.add_library("NAS", &["/mnt/photos"]);
        let c = Client::new(&mock, SERVER, API_KEY).unwrap();
        let local_root = Path::new("/Volumes/photos/Luxtable");
        // The known file isn't indexed yet.
        let d = detect(&c, Some(&local_root.join("Trip/IMG_9.ARW")), local_root)
            .unwrap()
            .unwrap();
        assert_eq!(d.how, Detection::LibraryPath);
        assert_eq!(d.mapping.immich_root, "/mnt/photos");
        assert_eq!(d.mapping.local_root, Path::new("/Volumes/photos"));
        // A match that doesn't cover the archive folder is ignored.
        mock.add_asset("/x/Elsewhere/IMG_2.ARW");
        let d = detect(
            &c,
            Some(Path::new("/Volumes/other/Elsewhere/IMG_2.ARW")),
            local_root,
        )
        .unwrap()
        .unwrap();
        assert_eq!(d.how, Detection::LibraryPath);
        // Nothing named alike.
        assert_eq!(
            detect(&c, None, Path::new("/Volumes/nas/Archive")).unwrap(),
            None
        );
    }

    #[test]
    fn non_admin_keys_detect_nothing_without_a_known_file() {
        let mock = MockImmich::new();
        mock.add_library("NAS", &["/mnt/photos"]);
        mock.state().is_admin = false;
        let c = Client::new(&mock, SERVER, API_KEY).unwrap();
        assert_eq!(
            detect(&c, None, Path::new("/Volumes/photos")).unwrap(),
            None
        );
        mock.state().offline = true;
        assert!(
            detect(&c, None, Path::new("/Volumes/photos"))
                .unwrap_err()
                .is_offline()
        );
        assert!(
            detect(
                &c,
                Some(Path::new("/Volumes/photos/a/b.ARW")),
                Path::new("/Volumes/photos")
            )
            .unwrap_err()
            .is_offline()
        );
    }
}
