//! The one error type crossing the FFI. Swift sees it as `DsError: Error`.
//!
//! Variants carry a human-readable `message` so the app can show it directly;
//! the variant itself tells the app *what to do* (retry, show a picker, ignore).

/// Errors returned by every fallible exported call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum DsError {
    /// The caller passed something out of range (e.g. a zero-sized render).
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },
    /// The operation was cancelled (Swift `Task` cancelled or engine shut down).
    #[error("cancelled")]
    Cancelled,
    /// A file or volume operation failed.
    #[error("I/O error: {message}")]
    Io { message: String },
    /// The file is readable but not something we can handle (not a RAW, an
    /// unsupported camera, no embedded preview). Skip it; retrying won't help.
    #[error("unsupported: {message}")]
    Unsupported { message: String },
    /// The folder of an open-in-place session was renamed or moved while open
    /// (`message` is its old path). Reopen it at its new location and retry.
    #[error("folder moved: {message}")]
    FolderMoved { message: String },
    /// A bug, a worker panic, or an unexpected internal state; worth a bug report.
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl DsError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for DsError {
    fn from(e: std::io::Error) -> Self {
        Self::Io {
            message: e.to_string(),
        }
    }
}

/// What the app says about a RAW file that is damaged or incomplete (a card
/// copy cut short, a failing card): the fix is copying it from the card again.
pub const DAMAGED_RAW: &str =
    "This RAW file is damaged or incomplete — copy it from the card again.";

impl From<lt_raw::RawError> for DsError {
    fn from(e: lt_raw::RawError) -> Self {
        use lt_raw::RawError as R;
        let message = e.to_string();
        match e {
            // final-journeys M2: a damaged or cut-short file is the file's fault, not
            // an app bug; say what to do instead of raw offsets.
            R::Truncated(_) | R::Malformed(_) | R::Decode(_) => Self::Io {
                message: DAMAGED_RAW.to_owned(),
            },
            R::Io(_) => Self::Io { message },
            R::NotRaw(_) | R::Unsupported { .. } | R::NoPreview => Self::Unsupported { message },
            R::Invalid(_) => Self::Internal { message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_human_readable() {
        assert_eq!(
            DsError::invalid("width is 0").to_string(),
            "invalid argument: width is 0"
        );
        assert_eq!(DsError::Cancelled.to_string(), "cancelled");
        assert_eq!(
            DsError::Internal {
                message: "x".into()
            }
            .to_string(),
            "internal error: x"
        );
    }

    #[test]
    fn io_errors_convert() {
        let e: DsError = std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into();
        assert_eq!(
            e,
            DsError::Io {
                message: "gone".into()
            }
        );
    }

    #[test]
    fn raw_errors_map_by_what_the_app_should_do() {
        use lt_raw::RawError as R;
        let io: DsError = R::Io(std::io::Error::other("disk")).into();
        assert!(matches!(io, DsError::Io { .. }));
        assert!(matches!(
            DsError::from(R::Truncated("x".into())),
            DsError::Io { .. }
        ));
        assert!(matches!(
            DsError::from(R::Malformed("x".into())),
            DsError::Io { .. }
        ));
        assert!(matches!(
            DsError::from(R::NotRaw("x".into())),
            DsError::Unsupported { .. }
        ));
        assert!(matches!(
            DsError::from(R::NoPreview),
            DsError::Unsupported { .. }
        ));
        let u = R::Unsupported {
            make: "A".into(),
            model: "B".into(),
            reason: "c".into(),
        };
        assert!(matches!(DsError::from(u), DsError::Unsupported { .. }));
        assert_eq!(
            DsError::from(R::Decode("x".into())),
            DsError::Io {
                message: DAMAGED_RAW.into()
            }
        );
        assert_eq!(
            DsError::from(R::Truncated(
                "range 1286144+48674304 outside 0..20000000".into()
            )),
            DsError::Io {
                message: DAMAGED_RAW.into()
            },
            "no raw offsets in the UI"
        );
        assert_eq!(
            DsError::Unsupported {
                message: "m".into()
            }
            .to_string(),
            "unsupported: m"
        );
    }
}
