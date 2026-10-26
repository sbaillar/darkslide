use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum LrError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: not a Lightroom catalog: {message}")]
    NotACatalog { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Db { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Xmp { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Preset { path: PathBuf, message: String },
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Session(#[from] lt_session::SessionError),
}

pub type Result<T, E = LrError> = std::result::Result<T, E>;

pub(crate) trait IoAt<T> {
    fn at(self, path: &std::path::Path) -> Result<T>;
}

impl<T> IoAt<T> for std::io::Result<T> {
    fn at(self, path: &std::path::Path) -> Result<T> {
        self.map_err(|source| LrError::Io {
            path: path.to_owned(),
            source,
        })
    }
}
