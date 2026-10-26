//! Error type for everything in `lt-raw`.

/// Why a RAW file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum RawError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not a RAW container we recognize (bad magic, not TIFF-based, …).
    #[error("not a RAW file: {0}")]
    NotRaw(String),
    /// A header value points past the end of the file (truncated or corrupt).
    #[error("file truncated or offset out of range: {0}")]
    Truncated(String),
    /// Structurally invalid data (bad counts, types, dimensions).
    #[error("malformed RAW file: {0}")]
    Malformed(String),
    /// A valid RAW from a camera/mode we cannot decode. Its message starts with
    /// "unsupported camera " or "unsupported file: ", which the app relies on to
    /// say "can't develop this file yet" (`OpenFailures.isUnsupportedFormat`).
    #[error("unsupported {}: {reason}", subject(make, model))]
    Unsupported {
        make: String,
        model: String,
        reason: String,
    },
    /// No embedded preview JPEG was found.
    #[error("no embedded preview found")]
    NoPreview,
    /// The rawler fallback decoder failed (or panicked).
    #[error("decoder failed: {0}")]
    Decode(String),
    /// Decoded image failed [`lt_core::RawImage::validate`].
    #[error(transparent)]
    Invalid(#[from] lt_core::raw::RawError),
}

/// "camera MAKE MODEL", or "file" when the camera isn't known.
fn subject(make: &str, model: &str) -> String {
    let name = format!("{make} {model}");
    match name.trim() {
        "" => "file".into(),
        n => format!("camera {n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_names_the_camera_when_known() {
        let e = |make: &str, model: &str| RawError::Unsupported {
            make: make.into(),
            model: model.into(),
            reason: "why".into(),
        };
        assert_eq!(
            e("FUJIFILM", "X-T4").to_string(),
            "unsupported camera FUJIFILM X-T4: why"
        );
        assert_eq!(e("", "X-T4").to_string(), "unsupported camera X-T4: why");
        assert_eq!(e("", "").to_string(), "unsupported file: why");
    }
}
