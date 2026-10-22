//! The bundled plug-ins this build trusts: their file hashes, compiled in by `build.rs`
//! from `plugins/builtin`. A folder in the app's `Resources/Plugins` loads only when its
//! files are exactly these, so no signing key is needed for Darkslide's own plug-ins.

use std::collections::BTreeMap;

use crate::trust::FileHashes;

include!(concat!(env!("OUT_DIR"), "/builtin_hashes.rs"));

/// Bundled folder name (`darkslide-looks.dsplugin`) → its files' hashes.
#[must_use]
pub fn embedded() -> BTreeMap<String, FileHashes> {
    BUILTIN_HASHES
        .iter()
        .map(|(folder, files)| {
            (
                (*folder).to_owned(),
                files
                    .iter()
                    .map(|(p, h)| ((*p).to_owned(), (*h).to_owned()))
                    .collect(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_looks_pack_is_compiled_in() {
        let e = super::embedded();
        let looks = &e["darkslide-looks.dsplugin"];
        assert!(
            looks.contains_key("plugin.toml")
                && looks.contains_key("looks.json")
                && looks.contains_key("styles.json")
        );
    }
}
