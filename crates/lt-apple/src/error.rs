use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum AppleError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: not an Aperture or iPhoto library: {message}")]
    NotALibrary { path: PathBuf, message: String },
    /// A library Luxtable recognises but can't read (Photos, Aperture 1 or 2).
    #[error("{message}")]
    Unsupported { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Db { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Xml { path: PathBuf, message: String },
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T, E = AppleError> = std::result::Result<T, E>;

pub(crate) trait IoAt<T> {
    fn at(self, path: &std::path::Path) -> Result<T>;
}

impl<T> IoAt<T> for std::io::Result<T> {
    fn at(self, path: &std::path::Path) -> Result<T> {
        self.map_err(|source| AppleError::Io {
            path: path.to_owned(),
            source,
        })
    }
}
