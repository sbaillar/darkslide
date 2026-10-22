//! Camera profiles for the picker: Darkslide's built-in looks plus imported Adobe
//! `.dcp` files. Imported files live in a folder the app owns (one `<blake3>.dcp` per
//! profile, written atomically); the app registers them at launch with
//! [`load_camera_profiles`] so renders can resolve `Profile::Dcp { hash, .. }`.

use std::path::Path;

use ds_core::adjust::Profile;
use ds_pipeline::profile::{self, DcpInfo};

use crate::error::DsError;
use crate::jobs::guard;

/// One row of the profile picker.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CameraProfileEntry {
    /// `standard`, `neutral`, `vivid` or `dcp` (the sidecar's `profile.kind`).
    pub kind: String,
    /// BLAKE3 hex of the `.dcp` file (imported profiles only).
    pub hash: Option<String>,
    pub name: String,
    /// The camera the profile was made for (imported profiles only).
    pub camera: Option<String>,
}

fn builtin_entry(p: &Profile) -> CameraProfileEntry {
    let kind = match p {
        Profile::Standard => "standard",
        Profile::Neutral => "neutral",
        Profile::Vivid => "vivid",
        Profile::Dcp { .. } => "dcp",
        Profile::Other(v) => v.get("kind").and_then(|k| k.as_str()).unwrap_or("other"),
    };
    CameraProfileEntry {
        kind: kind.into(),
        hash: None,
        name: profile::display_name(p),
        camera: None,
    }
}

fn dcp_entry(i: DcpInfo) -> CameraProfileEntry {
    CameraProfileEntry {
        kind: "dcp".into(),
        hash: Some(i.hash),
        name: i.name,
        camera: i.camera,
    }
}

fn io(e: &std::io::Error, what: &Path) -> DsError {
    DsError::Io {
        message: format!("{}: {e}", what.display()),
    }
}

fn unsupported(e: &ds_pipeline::dcp::DcpError, what: &Path) -> DsError {
    DsError::Unsupported {
        message: format!("{}: {e}", what.display()),
    }
}

/// A profile file's bytes, refusing anything larger than a camera profile can be
/// (before reading it).
fn read_capped(path: &Path) -> Result<Vec<u8>, DsError> {
    let len = std::fs::metadata(path).map_err(|e| io(&e, path))?.len();
    if len > ds_pipeline::dcp::MAX_FILE_BYTES {
        return Err(unsupported(
            &ds_pipeline::dcp::DcpError::TooLarge(len),
            path,
        ));
    }
    std::fs::read(path).map_err(|e| io(&e, path))
}

/// The built-in profiles (Standard first: the default) then every registered import,
/// by name.
#[uniffi::export]
pub fn camera_profiles() -> Result<Vec<CameraProfileEntry>, DsError> {
    guard(|| {
        let mut v: Vec<_> = profile::BUILT_IN.iter().map(builtin_entry).collect();
        v.extend(profile::registered_dcps().into_iter().map(dcp_entry));
        Ok(v)
    })
}

/// Copy the `.dcp` at `path` into `dir` as `<hash>.dcp` (atomically; an identical
/// file already there is kept) and register it.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn import_camera_profile(path: String, dir: String) -> Result<CameraProfileEntry, DsError> {
    guard(|| {
        let src = Path::new(&path);
        let bytes = read_capped(src)?;
        ds_pipeline::dcp::Dcp::parse(&bytes).map_err(|e| unsupported(&e, src))?;
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir).map_err(|e| io(&e, dir))?;
        let dest = dir.join(format!("{}.dcp", blake3::hash(&bytes).to_hex()));
        if !dest.exists() {
            ds_fsutil::write_atomic(&dest, &bytes).map_err(|e| io(&e, &dest))?;
        }
        // Registered only once it is stored, so a failed import leaves nothing behind.
        let info = profile::register_dcp(&bytes).map_err(|e| unsupported(&e, src))?;
        Ok(dcp_entry(info))
    })
}

/// Register every `.dcp` in `dir` (a missing folder is empty; unreadable or invalid
/// files are skipped). Returns the profiles registered.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn load_camera_profiles(dir: String) -> Result<Vec<CameraProfileEntry>, DsError> {
    guard(|| {
        let dir = Path::new(&dir);
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&e, dir)),
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dcp")))
            .collect();
        paths.sort();
        Ok(paths
            .iter()
            .filter_map(|p| read_capped(p).ok())
            .filter_map(|b| profile::register_dcp(&b).ok())
            .map(dcp_entry)
            .collect())
    })
}

/// Whether an imported profile (by hash) is registered; renders of a missing one fall
/// back to Darkslide Standard.
#[uniffi::export]
pub fn camera_profile_available(hash: String) -> bool {
    profile::is_available(&Profile::Dcp {
        hash,
        name: String::new(),
    })
}

/// Whether the imported profile `hash` is registered and made for `camera` (the photo's
/// make and model as the app shows it, e.g. "Sony ILCE-9"; case, punctuation and the
/// make are ignored). A profile for another camera renders as Standard.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn camera_profile_fits(hash: String, camera: String) -> bool {
    profile::fits_camera(&hash, &camera)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_built_ins_then_imports() {
        let v = camera_profiles().unwrap();
        let kinds: Vec<_> = v.iter().take(3).map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["standard", "neutral", "vivid"]);
        assert_eq!(v[0].name, "Darkslide Standard");
        assert!(
            v[..3]
                .iter()
                .all(|e| e.hash.is_none() && e.camera.is_none())
        );
        assert_eq!(
            builtin_entry(&Profile::Dcp {
                hash: "h".into(),
                name: "n".into()
            })
            .kind,
            "dcp"
        );
        let other = |v| builtin_entry(&Profile::Other(v)).kind;
        assert_eq!(other(serde_json::json!({"kind": "film"})), "film");
        assert_eq!(other(serde_json::json!({"kind": 3})), "other");
    }

    #[test]
    fn import_copies_registers_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("Sony ILCE-9 Camera Standard.dcp");
        let bytes = ds_pipeline::dcp::synth::example("FFI Import");
        std::fs::write(&src, &bytes).unwrap();
        let lib = tmp.path().join("Profiles");
        let lib_s = lib.to_string_lossy().to_string();
        let e = import_camera_profile(src.to_string_lossy().into(), lib_s.clone()).unwrap();
        assert_eq!(e.kind, "dcp");
        assert_eq!(e.name, "FFI Import");
        assert_eq!(e.camera.as_deref(), Some("Sony ILCE-9"));
        let hash = e.hash.clone().unwrap();
        assert_eq!(
            std::fs::read(lib.join(format!("{hash}.dcp"))).unwrap(),
            bytes
        );
        assert!(camera_profile_available(hash.clone()));
        assert!(camera_profile_fits(hash.clone(), "Sony ILCE-9".into()));
        assert!(camera_profile_fits(hash.clone(), "SONY ILCE-9".into()));
        assert!(!camera_profile_fits(hash.clone(), "Nikon D850".into()));
        assert!(!camera_profile_fits("0".repeat(64), "Sony ILCE-9".into()));
        assert!(camera_profiles().unwrap().contains(&e));
        // Importing again keeps the one file.
        assert_eq!(
            import_camera_profile(src.to_string_lossy().into(), lib_s.clone()).unwrap(),
            e
        );
        assert_eq!(std::fs::read_dir(&lib).unwrap().count(), 1);
        // A relaunch: unregistered, then loaded from the folder (junk skipped).
        assert!(ds_pipeline::profile::unregister_dcp(&hash));
        assert!(!camera_profile_available(hash.clone()));
        std::fs::write(lib.join("junk.dcp"), b"not a profile").unwrap();
        std::fs::write(lib.join("notes.txt"), b"x").unwrap();
        let loaded = load_camera_profiles(lib_s).unwrap();
        assert_eq!(loaded, vec![e]);
        assert!(camera_profile_available(hash));
    }

    #[test]
    fn errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_string_lossy().to_string();
        // Missing source, invalid profile, missing library folder, a file as the folder.
        assert!(matches!(
            import_camera_profile(
                tmp.path().join("nope.dcp").to_string_lossy().into(),
                dir.clone()
            ),
            Err(DsError::Io { .. })
        ));
        let bad = tmp.path().join("bad.dcp");
        std::fs::write(&bad, b"IIRC").unwrap();
        assert!(matches!(
            import_camera_profile(bad.to_string_lossy().into(), dir.clone()),
            Err(DsError::Unsupported { .. })
        ));
        assert_eq!(
            load_camera_profiles(tmp.path().join("none").to_string_lossy().into()).unwrap(),
            vec![]
        );
        assert!(matches!(
            load_camera_profiles(bad.to_string_lossy().into()),
            Err(DsError::Io { .. })
        ));
        let good = tmp.path().join("good.dcp");
        std::fs::write(&good, ds_pipeline::dcp::synth::example("Blocked")).unwrap();
        assert!(matches!(
            import_camera_profile(good.to_string_lossy().into(), bad.to_string_lossy().into()),
            Err(DsError::Io { .. })
        ));
        // Oversized files are refused before they are read, and skipped on load.
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let huge = lib.join("huge.dcp");
        let f = std::fs::File::create(&huge).unwrap();
        f.set_len(ds_pipeline::dcp::MAX_FILE_BYTES + 1).unwrap();
        assert!(matches!(
            import_camera_profile(huge.to_string_lossy().into(), dir),
            Err(DsError::Unsupported { .. })
        ));
        assert_eq!(
            load_camera_profiles(lib.to_string_lossy().into()).unwrap(),
            vec![]
        );
    }
}
