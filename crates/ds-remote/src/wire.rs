//! The link protocol: request/response messages over one TLS connection.
//!
//! A frame is a big-endian `u32` header length, the header as JSON, then
//! `body` bytes (file data; zero for most messages). The client sends
//! [`Request::Hello`] first; the server answers with the version it speaks.
//! Hashes are BLAKE3, as 64 hex digits.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::merge::MergeOutcome;

/// Bumped on any incompatible change; the server refuses other versions.
pub const PROTOCOL_VERSION: u32 = 2;
/// Transfer chunk size (as `ds-import`'s copy).
pub const CHUNK: usize = 4 << 20;
/// Largest header (the JSON part of a frame) either side accepts or sends.
pub const MAX_HEADER: usize = 1 << 20;

/// A file of one photo.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileRef {
    /// The original, by its session-relative path (`Capture/…`).
    Original { rel: String },
    /// The photo's embedded preview JPEG.
    Preview { rel: String },
    /// The sidecar of variant `ordinal` (1 = the first).
    Sidecar { rel: String, ordinal: u32 },
}

impl FileRef {
    pub fn rel(&self) -> &str {
        match self {
            Self::Original { rel } | Self::Preview { rel } | Self::Sidecar { rel, .. } => rel,
        }
    }
}

/// Which pairing secret the client has: the 6-digit code or the QR token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Secret {
    Code,
    Token,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u32,
        name: String,
    },
    /// SPAKE2 message A (hex), for the secret the user typed or scanned.
    PairStart {
        secret: Secret,
        spake: String,
    },
    /// The client's key confirmation (hex).
    PairFinish {
        confirm: String,
    },
    ListSessions,
    /// One page of the session's photos, sorted by path: those after `after`
    /// (the previous page's `next`), at most `limit` (0: the server's default).
    ListPhotos {
        session: String,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: u32,
    },
    Stat {
        session: String,
        file: FileRef,
    },
    /// `len` bytes at `offset` of `file`, which must still have `hash`.
    Read {
        session: String,
        file: FileRef,
        hash: String,
        offset: u64,
        len: u32,
    },
    /// How much of a pushed original the server already has.
    PushStatus {
        session: String,
        name: String,
        size: u64,
        hash: String,
    },
    /// Body: bytes of the original at `offset` (must equal what the server has).
    PushChunk {
        session: String,
        name: String,
        size: u64,
        hash: String,
        offset: u64,
    },
    /// An edited sidecar: `based_on` is the hash of the sidecar it was edited
    /// from, and `base` that sidecar's exact text when the client still has
    /// it (the Mac checks it hashes to `based_on` before trusting either).
    PushSidecar {
        session: String,
        rel: String,
        ordinal: u32,
        based_on: Option<String>,
        base: Option<String>,
        sidecar: Value,
        /// When it was edited, ms since the Unix epoch.
        modified_ms: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantEntry {
    pub ordinal: u32,
    pub variant_id: i64,
    pub name: String,
    pub sidecar_hash: String,
    pub modified_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoInfo {
    pub rel: String,
    pub size: u64,
    /// `None` until the Mac has hashed it (folders opened in place hash lazily).
    pub hash: Option<String>,
    pub missing: bool,
    pub variants: Vec<VariantEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Version,
    Unpaired,
    ReadOnly,
    NotFound,
    /// The file changed since the client asked for its hash.
    Changed,
    HashMismatch,
    BadRequest,
    PairingClosed,
    PairingFailed,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    Welcome {
        version: u32,
        name: String,
        paired: bool,
        write: bool,
    },
    PairChallenge {
        spake: String,
    },
    Paired {
        confirm: String,
        name: String,
    },
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    /// `next`: pass it as `after` for the following page; `None` on the last.
    Photos {
        photos: Vec<PhotoInfo>,
        #[serde(default)]
        next: Option<String>,
    },
    File {
        size: u64,
        hash: String,
    },
    /// Body: the bytes read.
    Data,
    PushState {
        have: u64,
    },
    Pushed {
        rel: String,
        duplicate: bool,
    },
    /// Body: the sidecar as it now is on the Mac.
    SidecarMerged {
        outcome: MergeOutcome,
        hash: String,
        /// The variant the other edit was kept as (conflicts).
        kept_ordinal: Option<u32>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Write one frame.
pub fn send<T: Serialize>(w: &mut impl Write, msg: &T, body: &[u8]) -> io::Result<()> {
    #[derive(Serialize)]
    struct Frame<'a, T> {
        #[serde(flatten)]
        msg: &'a T,
        body: u64,
    }
    let header = serde_json::to_vec(&Frame {
        msg,
        body: body.len() as u64,
    })
    .map_err(io::Error::other)?;
    if header.len() > MAX_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("header of {} bytes", header.len()),
        ));
    }
    let mut out = Vec::with_capacity(4 + header.len() + body.len());
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(body);
    w.write_all(&out)?;
    w.flush()
}

/// Read one frame: the message and its body. `UnexpectedEof` on a clean close.
pub fn recv<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<(T, Vec<u8>)> {
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_HEADER {
        return Err(bad(format!("header of {len} bytes")));
    }
    let mut header = vec![0u8; len];
    r.read_exact(&mut header)?;
    let mut v: Value = serde_json::from_slice(&header).map_err(|e| bad(e.to_string()))?;
    let body = v
        .as_object_mut()
        .and_then(|o| o.remove("body"))
        .and_then(|b| b.as_u64())
        .ok_or_else(|| bad("frame without a body length".into()))?;
    if body > CHUNK as u64 {
        return Err(bad(format!("body of {body} bytes")));
    }
    let msg = serde_json::from_value(v).map_err(|e| bad(e.to_string()))?;
    let mut data = vec![0u8; body as usize];
    r.read_exact(&mut data)?;
    Ok((msg, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_with_bodies() {
        let mut buf = Vec::new();
        let req = Request::Read {
            session: "s".into(),
            file: FileRef::Sidecar {
                rel: "Capture/a.arw".into(),
                ordinal: 2,
            },
            hash: "h".into(),
            offset: 7,
            len: 3,
        };
        send(&mut buf, &req, b"xyz").unwrap();
        send(&mut buf, &Request::ListSessions, &[]).unwrap();
        let mut r = &buf[..];
        let (got, body): (Request, _) = recv(&mut r).unwrap();
        assert_eq!((got, body), (req, b"xyz".to_vec()));
        let (got, body): (Request, _) = recv(&mut r).unwrap();
        assert_eq!((got, body.len()), (Request::ListSessions, 0));
        let e = recv::<Request>(&mut r).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            FileRef::Preview {
                rel: "Capture/x".into()
            }
            .rel(),
            "Capture/x"
        );
    }

    #[test]
    fn send_refuses_an_oversize_header() {
        let big = Response::Photos {
            photos: vec![
                PhotoInfo {
                    rel: "Capture/x".repeat(40),
                    size: 1,
                    hash: None,
                    missing: false,
                    variants: Vec::new(),
                };
                4000
            ],
            next: None,
        };
        let mut buf = Vec::new();
        let e = send(&mut buf, &big, &[]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("header of"), "{e}");
        assert!(buf.is_empty(), "nothing is written for a refused frame");
    }

    #[test]
    fn list_photos_paging_fields_default_for_a_bare_request() {
        let h = br#"{"op":"list_photos","session":"s","body":0}"#;
        let mut frame = (h.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(h);
        let (req, _) = recv::<Request>(&mut &frame[..]).unwrap();
        assert!(matches!(
            req,
            Request::ListPhotos {
                after: None,
                limit: 0,
                ..
            }
        ));
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let frame = |header: &[u8]| {
            let mut v = (header.len() as u32).to_be_bytes().to_vec();
            v.extend_from_slice(header);
            v
        };
        for (bytes, why) in [
            (frame(b"{\"op\":\"list_sessions\"}"), "body length"),
            (frame(b"not json"), "expected"),
            (frame(b"{\"op\":\"nope\",\"body\":0}"), "unknown variant"),
            (
                frame(format!("{{\"op\":\"list_sessions\",\"body\":{}}}", CHUNK + 1).as_bytes()),
                "body of",
            ),
            ((MAX_HEADER as u32 + 1).to_be_bytes().to_vec(), "header of"),
        ] {
            let e = recv::<Request>(&mut &bytes[..]).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
            assert!(e.to_string().contains(why), "{e} vs {why}");
        }
    }
}
