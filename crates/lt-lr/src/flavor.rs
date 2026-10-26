//! Which application a library comes from. The import engine (matching,
//! three-way merge, journal and revert) is shared with other importers that
//! read their own libraries into an [`crate::LrLibrary`] (`lt-apple` reads
//! Aperture and iPhoto libraries); a flavor says how develop settings map, the
//! files the import keeps its record and journal in (separate per flavor, so
//! reverting one kind of import never takes back another), and the words the
//! report uses.

use crate::map::{MapContext, Mapped, map_develop};
use crate::model::Develop;

#[derive(Debug, Clone, Copy)]
pub struct Flavor {
    /// The application, as the report names it ("Lightroom").
    pub app: &'static str,
    /// What a library of it is called ("Lightroom catalog").
    pub library: &'static str,
    /// The import record's file under `.lt/settings/`.
    pub state_file: &'static str,
    /// The revert journal's file under `.lt/settings/`.
    pub journal_file: &'static str,
    /// The name of an album whose collection has none.
    pub unnamed_album: &'static str,
    /// One photo's develop settings onto Luxtable's.
    pub map: fn(&Develop, &MapContext) -> Mapped,
}

impl Flavor {
    pub const LIGHTROOM: Flavor = Flavor {
        app: "Lightroom",
        library: "Lightroom catalog",
        state_file: crate::state::STATE_FILE,
        journal_file: crate::journal::JOURNAL_FILE,
        unnamed_album: "Lightroom collection",
        map: map_develop,
    };
}
