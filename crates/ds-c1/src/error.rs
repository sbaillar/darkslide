use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum C1Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: not a Capture One session (no .cosessiondb)")]
    NotASession(PathBuf),
    #[error("{path}: {message}")]
    Db { path: PathBuf, message: String },
    #[error("{path}: invalid settings file: {message}")]
    Cos { path: PathBuf, message: String },
    #[error("the Darkslide session is not the in-place session of {0}")]
    WrongSession(PathBuf),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Session(#[from] ds_session::SessionError),
}

pub type Result<T, E = C1Error> = std::result::Result<T, E>;

pub(crate) trait IoAt<T> {
    fn at(self, path: &std::path::Path) -> Result<T>;
}

impl<T> IoAt<T> for std::io::Result<T> {
    fn at(self, path: &std::path::Path) -> Result<T> {
        self.map_err(|source| C1Error::Io {
            path: path.to_owned(),
            source,
        })
    }
}
