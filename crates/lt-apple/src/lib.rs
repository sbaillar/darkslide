//! Aperture and iPhoto library import (see `docs/APPLE_FORMAT.md`).
//!
//! Everything here **reads** a library and never writes to it: its SQLite
//! databases are copied into a temporary directory and the copies are read,
//! and `AlbumData.xml` is read with plain `File::open`. Nothing inside the
//! library package is created, changed, moved or deleted.
//!
//! - [`read_library`]: an Aperture 3 / iPhoto 9.3+ library (`Library.apdb`)
//!   or an older iPhoto library (`AlbumData.xml`) → a [`lt_lr::LrLibrary`].
//!   A Photos library, or one from Aperture 1 or 2, is refused with a message
//!   that says so.
//! - [`FLAVOR`]: how the photos are applied to a Luxtable session, through
//!   lt-lr's import engine (matching, three-way merge, journal, revert), with
//!   this crate's mapping of the adjustments ([`map_adjustments`]) and its own
//!   import record and journal.

mod albumdata;
mod apdb;
mod archive;
mod detect;
mod error;
mod limits;
mod map;
mod sql;

use std::path::Path;

pub use detect::{Format, detect, is_inside_library, library_root};
pub use error::{AppleError, Result};
pub use map::map_adjustments;

use lt_lr::{Flavor, LrLibrary};

/// The import record's file under the session store's `.lt/settings/`.
pub const STATE_FILE: &str = "apple-import.json";
/// The revert journal's file under the session store's `.lt/settings/`.
pub const JOURNAL_FILE: &str = "apple-import-undo.json";

/// Apple libraries through lt-lr's import engine.
pub const FLAVOR: Flavor = Flavor {
    app: "the Aperture or iPhoto library",
    library: "Aperture or iPhoto library",
    state_file: STATE_FILE,
    journal_file: JOURNAL_FILE,
    unnamed_album: "Untitled album",
    map: map_adjustments,
};

/// A library, read.
#[derive(Debug, Clone, PartialEq)]
pub struct AppleLibrary {
    pub format: Format,
    pub lib: LrLibrary,
}

/// Where referenced originals' volumes are mounted.
const VOLUMES: &str = "/Volumes";

/// Read the library at `path` (the package, or the `AlbumData.xml` or
/// `Library.apdb` in it). `cancel` is polled throughout.
pub fn read_library(path: &Path, cancel: &(dyn Fn() -> bool + Sync)) -> Result<AppleLibrary> {
    read_library_with(path, Path::new(VOLUMES), cancel)
}

fn read_library_with(
    path: &Path,
    volumes: &Path,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Result<AppleLibrary> {
    let root = library_root(path);
    let format = detect(&root);
    let name = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let lib = match format {
        Format::Aperture | Format::IPhoto => apdb::read(&root, format, volumes, cancel)?,
        Format::IPhotoXml => albumdata::read(&root, cancel)?,
        Format::Photos => {
            return Err(AppleError::Unsupported {
                path: root,
                message: format!(
                    "\u{201c}{name}\u{201d} is a Photos library, which Luxtable doesn't read: \
                     only Aperture and iPhoto libraries are supported. In Photos, export the \
                     unmodified originals (File \u{25B8} Export) and open that folder instead."
                ),
            });
        }
        Format::OldAperture => {
            return Err(AppleError::Unsupported {
                path: root,
                message: format!(
                    "\u{201c}{name}\u{201d} was written by Aperture 1 or 2, which Luxtable \
                     can't read. Open it once in Aperture 3 to upgrade it, then choose it again."
                ),
            });
        }
        Format::Unknown => {
            return Err(AppleError::NotALibrary {
                path: root,
                message: "no Database/apdb/Library.apdb or AlbumData.xml in it".into(),
            });
        }
    };
    Ok(AppleLibrary { format, lib })
}

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod import_tests;
