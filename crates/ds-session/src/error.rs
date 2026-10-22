use std::io;
use std::path::PathBuf;

/// Everything that can go wrong in the session store.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("index database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON error in {path}: {message}")]
    Json { path: PathBuf, message: String },
    /// A relative path that is absolute, escapes the session (`..`), or is otherwise unusable.
    #[error("invalid relative path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("{0} is not a Darkslide session")]
    NotASession(PathBuf),
    #[error("{0} already exists and is not empty")]
    AlreadyExists(PathBuf),
    #[error("original {0:?} is not on disk")]
    OriginalMissing(String),
    #[error("{0:?} is already in the session")]
    DuplicatePath(String),
    #[error("unknown {kind} id {id}")]
    NotFound { kind: &'static str, id: i64 },
    /// The sidecar was written by a newer build; editing it here could drop fields.
    #[error(
        "sidecar {path} has schema v{found}, this build supports v{supported}; it is read-only"
    )]
    SidecarTooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("sidecar {path} has schema v{found} and no migration path to v{supported}")]
    NoMigration {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("cannot delete the only variant of an image; move the image to Trash instead")]
    LastVariant,
    #[error("the session index has schema v{found}, this build supports v{supported}")]
    IndexTooNew { found: i64, supported: i64 },
    #[error("album {0} is a smart album; its members come from its filter")]
    SmartAlbum(i64),
    #[error("album {0} is a built-in smart folder and cannot be deleted or redefined")]
    BuiltinAlbum(i64),
    #[error("collection {0} is a smart collection; its photos come from its rules")]
    SmartCollection(i64),
    #[error("invalid name {0:?}")]
    InvalidName(String),
    #[error(
        "the macOS Trash refused the files, so nothing was deleted (a volume without a Trash, such as a network share, can't take them: move them off it in Finder first): {0}"
    )]
    Trash(String),
    #[error(
        "{0} is not allowed in a session over the user's own folder: its originals are never moved, renamed or deleted"
    )]
    ExternalOriginals(&'static str),
    #[error("{0} only applies to sessions over the user's own folder")]
    ExternalOnly(&'static str),
}

pub type Result<T, E = SessionError> = std::result::Result<T, E>;

/// Attach a path to an `io::Error`.
pub(crate) trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| SessionError::Io {
            path: path.into(),
            source,
        })
    }
}

pub(crate) fn json_err(path: impl Into<PathBuf>, e: &serde_json::Error) -> SessionError {
    SessionError::Json {
        path: path.into(),
        message: e.to_string(),
    }
}
