//! Moving files to the macOS Trash, behind a trait so tests never touch it.

use std::path::PathBuf;

/// Something that can move files to a recoverable trash.
pub trait Trasher {
    fn trash(&self, paths: &[PathBuf]) -> Result<(), String>;
}

/// The user's real Trash, through `NSFileManager.trashItem` (no Automation
/// permission, unlike asking Finder). It fails instead of deleting on a volume
/// without a Trash (Finder would delete such files outright).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTrash;

impl Trasher for SystemTrash {
    fn trash(&self, paths: &[PathBuf]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut ctx = trash::TrashContext::default();
        #[cfg(target_os = "macos")]
        {
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            ctx.set_delete_method(DeleteMethod::NsFileManager);
        }
        ctx.delete_all(paths).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_trash_with_nothing_is_a_no_op() {
        assert_eq!(SystemTrash.trash(&[]), Ok(()));
    }

    #[test]
    fn system_trash_refuses_a_missing_file() {
        let d = tempfile::tempdir().unwrap();
        assert!(SystemTrash.trash(&[d.path().join("nope.ARW")]).is_err());
    }
}
