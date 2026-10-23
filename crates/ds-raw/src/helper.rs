//! The rawler helper: `darkslide-rawler` (crate `ds-rawler`), a separate program that
//! links the LGPL-2.1 `rawler` crate so that Darkslide's own binaries don't
//! (`docs/RAWDEC.md`). A build without the `rawler` feature hands it what the native
//! code doesn't cover: decodes and metadata of other formats, per-camera data (colour
//! matrices, crops, white levels) and linear DNG writing. Without a helper those
//! return [`RawError::Unsupported`], as before.
//!
//! One process per request, so a rawler crash or runaway allocation ends the helper,
//! never the app. The command and its small arguments go on the command line; the
//! one large input (the RAW file, or a DNG's pixels) is the helper's stdin, given as a
//! file, never a pipe: the RAW's own open descriptor (the bytes Darkslide opened and
//! checked, not whatever is at the path now) or an unlinked temporary file. Writing
//! to a pipe would raise `SIGPIPE` in the app if the helper quit early. The helper
//! reads stdin from offset 0 with positional reads, so a shared descriptor's offset
//! doesn't matter. The answer on stdout is a status byte (0 ok, 1 error) and
//! length-prefixed parts (`u64` little-endian length, then the bytes).
//!
//! The server side ([`serve`]) lives here too (with the `rawler` feature), so both
//! ends of the protocol are tested together; `ds-rawler`'s `main` only moves bytes.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, RwLock};

use ds_core::{CfaPattern, Orientation, RawImage, Size};
use serde::{Deserialize, Serialize};

use crate::meta::ImageMeta;
use crate::{DecodeInfo, DecodePath, RawError};

/// Version of the exchange below. A helper that answers another one is not used.
pub const PROTOCOL: u32 = 1;

/// Environment variable naming the helper, when [`set_rawler_helper`] wasn't called.
pub const HELPER_ENV: &str = "DS_RAWLER_HELPER";

/// The end of every "needs rawler" message of a build without rawler and no helper;
/// the app looks for "rawler helper" to offer installing it.
pub const NOT_INSTALLED: &str = "needs the rawler helper, which isn't installed";

/// Largest answer read from a helper (a decode is at most a few hundred MB).
const MAX_ANSWER: u64 = 4 << 30;

static CONFIGURED: RwLock<Option<PathBuf>> = RwLock::new(None);
/// The helper last checked with [`check`] and its answer, so it is asked once.
static CHECKED: Mutex<Option<(PathBuf, Result<HelperInfo, String>)>> = Mutex::new(None);

/// Use the helper at `path` (`None`: none, unless [`HELPER_ENV`] names one).
pub fn set_rawler_helper(path: Option<PathBuf>) {
    *CONFIGURED
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    crate::cameras::clear_helper_cache();
}

/// The helper in use: the one set with [`set_rawler_helper`], else [`HELPER_ENV`].
pub fn rawler_helper() -> Option<PathBuf> {
    CONFIGURED
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .or_else(|| std::env::var_os(HELPER_ENV).map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
}

/// What a working helper says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperInfo {
    pub protocol: u32,
    /// The rawler version it links.
    pub rawler: String,
}

/// Ask the helper at `path` who it is; `Unsupported` when it doesn't run or speaks
/// another protocol. The answer is remembered for that path.
pub fn check(path: &Path) -> Result<HelperInfo, RawError> {
    let mut checked = CHECKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((p, r)) = checked.as_ref()
        && p == path
    {
        return r.clone().map_err(|e| not_available(&e));
    }
    let r = info(&Process(path.to_owned()))
        .map_err(|e| e.to_string())
        .and_then(|i| {
            if i.protocol == PROTOCOL {
                Ok(i)
            } else {
                Err(format!(
                    "it speaks protocol {}, this Darkslide needs {PROTOCOL}",
                    i.protocol
                ))
            }
        });
    *checked = Some((path.to_owned(), r.clone()));
    r.map_err(|e| not_available(&e))
}

fn not_available(why: &str) -> RawError {
    RawError::Unsupported {
        make: String::new(),
        model: String::new(),
        reason: format!("the rawler helper can't be used: {why}"),
    }
}

/// The configured helper, once it has answered [`check`]; `None` without one.
#[cfg_attr(feature = "rawler", allow(dead_code))] // the client side of builds without rawler
pub(crate) fn client() -> Result<Option<Process>, RawError> {
    let Some(path) = rawler_helper() else {
        return Ok(None);
    };
    check(&path)?;
    Ok(Some(Process(path)))
}

/// The input a request gives the helper as its stdin.
pub(crate) enum Input {
    None,
    File(File),
}

impl Input {
    /// `parts` written one after the other to an unlinked temporary file.
    pub(crate) fn bytes(parts: &[&[u8]]) -> Result<Self, RawError> {
        let f = tempfile::tempfile()?;
        let mut off = 0u64;
        for p in parts {
            f.write_all_at(p, off)?;
            off += p.len() as u64;
        }
        Ok(Self::File(f))
    }

    /// A file of `len` zero bytes but for the first and last `window` bytes, which
    /// `read` fills (`read(offset, buf)`). The zeros are a hole: nothing is written.
    pub(crate) fn window(
        len: u64,
        window: u64,
        read: impl Fn(u64, &mut [u8]) -> Result<(), RawError>,
    ) -> Result<Self, RawError> {
        let f = tempfile::tempfile()?;
        f.set_len(len)?;
        let mut buf = vec![0u8; window.min(len) as usize];
        for off in [0, len.saturating_sub(window)] {
            read(off, &mut buf)?;
            f.write_all_at(&buf, off)?;
        }
        Ok(Self::File(f))
    }

    fn stdio(self) -> Stdio {
        match self {
            Self::None => Stdio::null(),
            Self::File(f) => Stdio::from(f),
        }
    }
}

/// Something that answers requests: the helper process, or (in tests) [`serve`].
pub(crate) trait Transport {
    /// Run `args` with `input` as stdin; the raw answer.
    fn call(&self, args: &[&str], input: Input) -> Result<Vec<u8>, RawError>;
}

/// The helper program at a path.
pub(crate) struct Process(pub(crate) PathBuf);

impl Transport for Process {
    fn call(&self, args: &[&str], input: Input) -> Result<Vec<u8>, RawError> {
        let mut child = Command::new(&self.0)
            .args(args)
            .stdin(input.stdio())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| not_available(&format!("{}: {e}", self.0.display())))?;
        let mut out = Vec::new();
        let read = child
            .stdout
            .take()
            .map(|s| s.take(MAX_ANSWER).read_to_end(&mut out));
        let status = child.wait()?;
        if let Some(Err(e)) = read {
            return Err(e.into());
        }
        if out.is_empty() {
            return Err(RawError::Decode(format!(
                "the rawler helper gave no answer ({status})"
            )));
        }
        Ok(out)
    }
}

/// [`HelperInfo`] of the helper behind `t`.
pub(crate) fn info(t: &dyn Transport) -> Result<HelperInfo, RawError> {
    json(one(&answer(&t.call(&["info"], Input::None)?)?)?)
}

/// Decode the RAW in `file` (its whole contents) with rawler.
pub(crate) fn decode(t: &dyn Transport, file: Input) -> Result<(RawImage, DecodeInfo), RawError> {
    let raw = t.call(&["decode"], file)?;
    let parts = answer(&raw)?;
    let [head, data] = parts.as_slice() else {
        return Err(bad_answer("decode wants 2 parts"));
    };
    let head: ImageHead = json(head)?;
    let img = head.image(samples(data)?)?;
    let info = DecodeInfo {
        path: DecodePath::Rawler,
        matrix_illuminant: head.matrix_illuminant,
    };
    Ok((img, info))
}

/// rawler's metadata of the RAW in `file`.
pub(crate) fn metadata(t: &dyn Transport, file: Input) -> Result<ImageMeta, RawError> {
    json(one(&answer(&t.call(&["metadata"], file)?)?)?)
}

/// rawler's camera database entry (see [`crate::cameras`]).
#[cfg_attr(feature = "rawler", allow(dead_code))] // the client side of builds without rawler
pub(crate) fn camera(
    t: &dyn Transport,
    make: &str,
    model: &str,
    mode: &str,
) -> Result<crate::cameras::Camera, RawError> {
    json(one(&answer(
        &t.call(&["camera", make, model, mode], Input::None)?,
    )?)?)
}

/// What [`write_dng`] sends besides the pixels.
#[derive(Debug, Serialize, Deserialize)]
struct DngRequest {
    image: ImageHead,
    make: String,
    model: String,
    software: String,
    exif_from: Option<PathBuf>,
    preview: Option<(u32, u32)>,
}

/// A linear DNG of `img` written by rawler's writer: the file's bytes.
#[cfg_attr(feature = "rawler", allow(dead_code))] // the client side of builds without rawler
pub(crate) fn write_dng(
    t: &dyn Transport,
    img: &RawImage,
    meta: &crate::DngMeta<'_>,
) -> Result<Vec<u8>, RawError> {
    let req = DngRequest {
        image: ImageHead::of(img, 0),
        make: meta.make.into(),
        model: meta.model.into(),
        software: meta.software.into(),
        exif_from: meta.exif_from.map(Path::to_path_buf),
        preview: meta.preview.map(|(_, w, h)| (w, h)),
    };
    let req = to_json(&req)?;
    let data = le_bytes(&img.data);
    let jpeg = meta.preview.map_or(&[][..], |(j, _, _)| j);
    let len = |p: &[u8]| (p.len() as u64).to_le_bytes();
    let (lr, ld, lj) = (len(&req), len(&data), len(jpeg));
    let input = Input::bytes(&[&lr, &req, &ld, &data, &lj, jpeg])?;
    let raw = t.call(&["write-dng"], input)?;
    Ok(one(&answer(&raw)?)?.to_vec())
}

/// Everything of a [`RawImage`] but its samples, plus the matrix's illuminant.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImageHead {
    width: u32,
    height: u32,
    cfa: u8,
    black_level: [f32; 4],
    white_level: f32,
    wb_as_shot: [f32; 3],
    wb_daylight: [f32; 3],
    cam_to_xyz: [[f32; 3]; 3],
    orientation: Orientation,
    default_crop: Option<[u32; 4]>,
    matrix_illuminant: u16,
}

impl ImageHead {
    fn of(img: &RawImage, matrix_illuminant: u16) -> Self {
        Self {
            width: img.size.width,
            height: img.size.height,
            cfa: cfa_code(img.cfa),
            black_level: img.black_level,
            white_level: img.white_level,
            wb_as_shot: img.wb_as_shot,
            wb_daylight: img.wb_daylight,
            cam_to_xyz: img.cam_to_xyz,
            orientation: img.orientation,
            default_crop: img.default_crop,
            matrix_illuminant,
        }
    }

    fn image(&self, data: Vec<u16>) -> Result<RawImage, RawError> {
        let img = RawImage {
            size: Size::new(self.width, self.height),
            cfa: cfa_of(self.cfa).ok_or_else(|| bad_answer("unknown CFA"))?,
            data,
            black_level: self.black_level,
            white_level: self.white_level,
            wb_as_shot: self.wb_as_shot,
            wb_daylight: self.wb_daylight,
            cam_to_xyz: self.cam_to_xyz,
            orientation: self.orientation,
            default_crop: self.default_crop,
        };
        img.validate()?;
        Ok(img)
    }
}

pub(crate) fn cfa_code(c: CfaPattern) -> u8 {
    match c {
        CfaPattern::Rggb => 0,
        CfaPattern::Bggr => 1,
        CfaPattern::Grbg => 2,
        CfaPattern::Gbrg => 3,
        CfaPattern::LinearRgb => 4,
    }
}

pub(crate) fn cfa_of(v: u8) -> Option<CfaPattern> {
    Some(match v {
        0 => CfaPattern::Rggb,
        1 => CfaPattern::Bggr,
        2 => CfaPattern::Grbg,
        3 => CfaPattern::Gbrg,
        4 => CfaPattern::LinearRgb,
        _ => return None,
    })
}

/// A [`RawError`] as it crosses the process boundary.
#[derive(Debug, Serialize, Deserialize)]
struct WireError {
    kind: String,
    make: String,
    model: String,
    message: String,
}

impl WireError {
    #[cfg(feature = "rawler")]
    fn of(e: &RawError) -> Self {
        let (kind, message) = match e {
            RawError::Io(e) => ("io", e.to_string()),
            RawError::NotRaw(s) => ("not-raw", s.clone()),
            RawError::Truncated(s) => ("truncated", s.clone()),
            RawError::Malformed(s) => ("malformed", s.clone()),
            RawError::Unsupported { reason, .. } => ("unsupported", reason.clone()),
            RawError::NoPreview => ("no-preview", String::new()),
            RawError::Decode(s) => ("decode", s.clone()),
            RawError::Invalid(e) => ("malformed", e.to_string()),
        };
        let (make, model) = match e {
            RawError::Unsupported { make, model, .. } => (make.clone(), model.clone()),
            _ => (String::new(), String::new()),
        };
        Self {
            kind: kind.into(),
            make,
            model,
            message,
        }
    }

    fn error(self) -> RawError {
        match self.kind.as_str() {
            "io" => RawError::Io(std::io::Error::other(self.message)),
            "not-raw" => RawError::NotRaw(self.message),
            "truncated" => RawError::Truncated(self.message),
            "malformed" => RawError::Malformed(self.message),
            "unsupported" => RawError::Unsupported {
                make: self.make,
                model: self.model,
                reason: self.message,
            },
            "no-preview" => RawError::NoPreview,
            _ => RawError::Decode(self.message),
        }
    }
}

fn bad_answer(why: &str) -> RawError {
    RawError::Decode(format!("the rawler helper's answer is malformed: {why}"))
}

#[cfg(feature = "rawler")]
fn frame(out: &mut Vec<u8>, part: &[u8]) {
    out.extend_from_slice(&(part.len() as u64).to_le_bytes());
    out.extend_from_slice(part);
}

/// The parts of a framed message.
fn unframe(mut b: &[u8]) -> Result<Vec<&[u8]>, RawError> {
    let mut parts = Vec::new();
    while !b.is_empty() {
        let (len, rest) = b
            .split_first_chunk::<8>()
            .ok_or_else(|| bad_answer("short length"))?;
        let len = usize::try_from(u64::from_le_bytes(*len))
            .ok()
            .filter(|&n| n <= rest.len())
            .ok_or_else(|| bad_answer("part runs past the end"))?;
        parts.push(&rest[..len]);
        b = &rest[len..];
    }
    Ok(parts)
}

/// The parts of an answer, or the error it carries.
fn answer(b: &[u8]) -> Result<Vec<&[u8]>, RawError> {
    match b.split_first() {
        Some((0, rest)) => unframe(rest),
        Some((1, rest)) => Err(json::<WireError>(one(&unframe(rest)?)?)?.error()),
        _ => Err(bad_answer("unknown status")),
    }
}

fn one<'a>(parts: &[&'a [u8]]) -> Result<&'a [u8], RawError> {
    match parts {
        [p] => Ok(p),
        _ => Err(bad_answer("wants 1 part")),
    }
}

fn to_json<T: Serialize>(v: &T) -> Result<Vec<u8>, RawError> {
    serde_json::to_vec(v).map_err(|e| RawError::Decode(e.to_string()))
}

fn json<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T, RawError> {
    serde_json::from_slice(b).map_err(|e| bad_answer(&e.to_string()))
}

fn le_bytes(data: &[u16]) -> Vec<u8> {
    data.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn samples(b: &[u8]) -> Result<Vec<u16>, RawError> {
    if !b.len().is_multiple_of(2) {
        return Err(bad_answer("odd sample bytes"));
    }
    Ok(b.as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect())
}

/// Read a helper's stdin: positionally from offset 0 when it is a file, else to its end.
pub fn read_input(mut stdin: File) -> std::io::Result<Vec<u8>> {
    let meta = stdin.metadata()?;
    if !meta.is_file() {
        let mut v = Vec::new();
        stdin.read_to_end(&mut v)?;
        return Ok(v);
    }
    let mut v = vec![0u8; usize::try_from(meta.len()).map_err(std::io::Error::other)?];
    stdin.read_exact_at(&mut v, 0)?;
    Ok(v)
}

/// The helper's side: answer `args` (command first) given stdin `input`.
#[cfg(feature = "rawler")]
pub fn serve(args: &[String], input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match serve_parts(args, input) {
        Ok(parts) => {
            out.push(0);
            for p in parts {
                frame(&mut out, &p);
            }
        }
        Err(e) => {
            out.push(1);
            let e = serde_json::to_vec(&WireError::of(&e)).unwrap_or_default();
            frame(&mut out, &e);
        }
    }
    out
}

#[cfg(feature = "rawler")]
fn serve_parts(args: &[String], input: &[u8]) -> Result<Vec<Vec<u8>>, RawError> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["info"] => Ok(vec![to_json(&HelperInfo {
            protocol: PROTOCOL,
            rawler: RAWLER_VERSION.into(),
        })?]),
        ["decode"] => {
            let (img, info) = crate::RawFile::from_bytes(input)?.decode_rawler()?;
            let head = ImageHead::of(&img, info.matrix_illuminant);
            Ok(vec![to_json(&head)?, le_bytes(&img.data)])
        }
        ["metadata"] => Ok(vec![to_json(
            &crate::RawFile::from_bytes(input)?.metadata()?,
        )?]),
        ["camera", make, model, mode] => Ok(vec![to_json(&crate::cameras::db(make, model, mode))?]),
        ["write-dng"] => {
            let parts = unframe(input)?;
            let [req, data, jpeg] = parts.as_slice() else {
                return Err(RawError::Malformed("write-dng wants 3 parts".into()));
            };
            let req: DngRequest = serde_json::from_slice(req)
                .map_err(|e| RawError::Malformed(format!("write-dng request: {e}")))?;
            let img = req.image.image(samples(data)?)?;
            let meta = crate::DngMeta {
                make: &req.make,
                model: &req.model,
                exif_from: req.exif_from.as_deref(),
                preview: req.preview.map(|(w, h)| (*jpeg, w, h)),
                software: &req.software,
            };
            let mut out = std::io::Cursor::new(Vec::new());
            crate::write_linear_dng(&mut out, img, &meta)?;
            Ok(vec![out.into_inner()])
        }
        _ => Err(RawError::Malformed(format!("unknown request {args:?}"))),
    }
}

/// The rawler version this crate builds against (its `Cargo.toml` pin).
#[cfg(feature = "rawler")]
const RAWLER_VERSION: &str = "0.8.0";

/// `Option<CfaPattern>` as its [`cfa_code`], for the camera data in answers.
pub(crate) mod cfa_opt {
    use ds_core::CfaPattern;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)] // serde's `with` signature
    pub fn serialize<S: Serializer>(c: &Option<CfaPattern>, s: S) -> Result<S::Ok, S::Error> {
        c.map(super::cfa_code).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<CfaPattern>, D::Error> {
        Ok(Option::<u8>::deserialize(d)?.and_then(super::cfa_of))
    }
}

#[cfg(all(test, feature = "rawler"))]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;
    use crate::preview::tests::fake_jpeg;
    use crate::synth::{Synth, write_tmp};
    use std::io::Cursor;

    /// [`serve`] in this process: both ends of the protocol without a helper binary.
    struct InProcess;

    impl Transport for InProcess {
        fn call(&self, args: &[&str], input: Input) -> Result<Vec<u8>, RawError> {
            let bytes = match input {
                Input::None => Vec::new(),
                Input::File(f) => read_input(f)?,
            };
            let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            Ok(serve(&args, &bytes))
        }
    }

    /// An answer made by hand.
    struct Canned(Vec<u8>);

    impl Transport for Canned {
        fn call(&self, _: &[&str], _: Input) -> Result<Vec<u8>, RawError> {
            Ok(self.0.clone())
        }
    }

    fn ok(parts: &[&[u8]]) -> Canned {
        let mut b = vec![0];
        for p in parts {
            frame(&mut b, p);
        }
        Canned(b)
    }

    fn synth_file() -> (tempfile::TempDir, PathBuf) {
        // Not Sony's fast path (unknown compression would fail): the plain layout rawler
        // decodes, as `rawler_fallback_agrees_with_fast_path` does.
        write_tmp(&Synth::default().build(), "h.ARW")
    }

    #[test]
    fn info_names_the_protocol_and_the_rawler_pin() {
        let i = info(&InProcess).unwrap();
        assert_eq!(i.protocol, PROTOCOL);
        let manifest = include_str!("../Cargo.toml");
        assert!(
            manifest.contains(&format!("rawler = {{ version = \"{}\"", i.rawler)),
            "RAWLER_VERSION {} isn't the Cargo.toml pin",
            i.rawler
        );
    }

    /// A decode through the protocol is the in-process rawler decode, field for field,
    /// from the file's own descriptor and from bytes.
    #[test]
    fn decode_crosses_the_protocol_unchanged() {
        let (_d, p) = synth_file();
        let raw = crate::open(&p).unwrap();
        let (want, want_info) = raw.decode_rawler().unwrap();
        for (img, info) in [
            raw.helper_decode(&InProcess).unwrap(),
            crate::RawFile::from_bytes(&std::fs::read(&p).unwrap())
                .unwrap()
                .helper_decode(&InProcess)
                .unwrap(),
        ] {
            assert_eq!(info, want_info);
            assert_eq!(info.path, DecodePath::Rawler);
            assert_eq!(img.size, want.size);
            assert_eq!(img.cfa, want.cfa);
            assert_eq!(img.data, want.data);
            assert_eq!(img.black_level, want.black_level);
            assert_eq!(img.white_level, want.white_level);
            assert_eq!(img.wb_as_shot, want.wb_as_shot);
            assert_eq!(img.wb_daylight, want.wb_daylight);
            assert_eq!(img.cam_to_xyz, want.cam_to_xyz);
            assert_eq!(img.orientation, want.orientation);
            assert_eq!(img.default_crop, want.default_crop);
        }
    }

    #[test]
    fn metadata_and_camera_data_cross_the_protocol() {
        let (_d, p) = synth_file();
        let raw = crate::open(&p).unwrap();
        assert_eq!(
            raw.helper_metadata(&InProcess).unwrap(),
            raw.metadata().unwrap()
        );
        let cam = camera(&InProcess, "SONY", "ILCE-9", "").unwrap();
        assert_eq!(cam, crate::cameras::db("SONY", "ILCE-9", ""));
        assert!(cam.matrix.is_some());
        let d850 = camera(&InProcess, "NIKON CORPORATION", "NIKON D850", "14bit").unwrap();
        assert_eq!(d850.cfa, Some(CfaPattern::Rggb));
        assert_eq!(d850.white, Some(15520));
        assert_eq!(
            camera(&InProcess, "NOPE", "NOPE-1", "").unwrap(),
            crate::cameras::Camera::default()
        );
    }

    /// Big files: the helper reads both ends first (a hole between them).
    #[test]
    fn metadata_of_a_big_file_reads_its_ends() {
        let mut bytes = Synth::default().build();
        let n = bytes.len();
        bytes.resize((METADATA_WINDOW_TEST * 2 + 4096) as usize, 0);
        let raw = crate::RawFile::from_bytes(&bytes).unwrap();
        let w = Input::window(bytes.len() as u64, METADATA_WINDOW_TEST, |off, buf| {
            raw.src().read_into(off, buf)
        })
        .unwrap();
        let Input::File(f) = w else { panic!() };
        let back = read_input(f).unwrap();
        assert_eq!(back.len(), bytes.len());
        assert_eq!(back[..n], bytes[..n]);
        assert!(
            back[METADATA_WINDOW_TEST as usize..]
                .iter()
                .all(|&b| b == 0)
        );
        // The windows parse on their own (the TIFF header is at the start).
        assert_eq!(
            metadata(&InProcess, Input::bytes(&[&back]).unwrap())
                .unwrap()
                .model,
            "ILCE-9"
        );
        // Files no bigger than two windows: a single window covers them.
        let small = Input::window(10, 8, |off, buf| {
            buf.fill(u8::try_from(off).unwrap() + 1);
            Ok(())
        })
        .unwrap();
        let Input::File(f) = small else { panic!() };
        assert_eq!(read_input(f).unwrap(), [1, 1, 3, 3, 3, 3, 3, 3, 3, 3]);
    }

    const METADATA_WINDOW_TEST: u64 = 64 << 10;

    /// The helper's DNG is byte for byte the in-process writer's.
    #[test]
    fn write_dng_crosses_the_protocol_unchanged() {
        let img = crate::dng::tests::linear(40, 30);
        let jpeg = fake_jpeg(16, 12, 3);
        let (_d, src) = synth_file();
        let meta = crate::DngMeta {
            make: "SONY",
            model: "ILCE-9",
            exif_from: Some(&src),
            preview: Some((&jpeg, 16, 12)),
            software: "Darkslide",
        };
        let mut want = Cursor::new(Vec::new());
        crate::write_linear_dng(&mut want, img.clone(), &meta).unwrap();
        assert_eq!(
            write_dng(&InProcess, &img, &meta).unwrap(),
            want.into_inner()
        );
        let bare = crate::DngMeta {
            make: "X",
            model: "Y",
            ..crate::DngMeta::default()
        };
        let back = crate::decode_bytes(&write_dng(&InProcess, &img, &bare).unwrap()).unwrap();
        assert_eq!(back.data, img.data);
        // The writer's own refusal comes back as the same error.
        let mut mosaic = img;
        mosaic.cfa = CfaPattern::Rggb;
        mosaic.data.truncate(40 * 30);
        assert!(matches!(
            write_dng(&InProcess, &mosaic, &bare),
            Err(RawError::Malformed(_))
        ));
    }

    #[test]
    fn errors_cross_the_protocol_with_their_kind() {
        // Unsupported keeps the camera.
        let s = Synth {
            model: "NOPE-1",
            compression: 99,
            ..Synth::default()
        };
        let (_d, p) = write_tmp(&s.build(), "n.ARW");
        let e = crate::open(&p).unwrap().helper_decode(&InProcess);
        assert!(matches!(
            e,
            Err(RawError::Unsupported { .. } | RawError::Decode(_))
        ));
        let not_raw = decode(&InProcess, Input::bytes(&[b"hello"]).unwrap());
        assert!(matches!(not_raw, Err(RawError::NotRaw(_))), "{not_raw:?}");
        let unknown = InProcess.call(&["nope"], Input::None).unwrap();
        assert!(matches!(answer(&unknown), Err(RawError::Malformed(m)) if m.contains("nope")));
        let bad_dng = InProcess
            .call(&["write-dng"], Input::bytes(&[b"x"]).unwrap())
            .unwrap();
        assert!(answer(&bad_dng).is_err());
        let mut parts = Vec::new();
        for p in [&b"{"[..], b"", b""] {
            frame(&mut parts, p);
        }
        let bad_req = InProcess
            .call(&["write-dng"], Input::bytes(&[&parts]).unwrap())
            .unwrap();
        assert!(matches!(answer(&bad_req), Err(RawError::Malformed(m)) if m.contains("request")));

        let all = [
            RawError::Io(std::io::Error::other("disk")),
            RawError::NotRaw("n".into()),
            RawError::Truncated("t".into()),
            RawError::Malformed("m".into()),
            RawError::Unsupported {
                make: "K".into(),
                model: "M".into(),
                reason: "r".into(),
            },
            RawError::NoPreview,
            RawError::Decode("d".into()),
            RawError::Invalid(ds_core::raw::RawError::Invalid("i".into())),
        ];
        for e in all {
            let back = WireError::of(&e).error();
            let same_kind = std::mem::discriminant(&back) == std::mem::discriminant(&e)
                || matches!((&e, &back), (RawError::Invalid(_), RawError::Malformed(_)));
            assert!(same_kind, "{e:?} came back as {back:?}");
            if !matches!(e, RawError::Invalid(_)) {
                assert_eq!(back.to_string(), e.to_string());
            }
        }
        let odd = WireError {
            kind: "future".into(),
            make: String::new(),
            model: String::new(),
            message: "x".into(),
        };
        assert!(matches!(odd.error(), RawError::Decode(m) if m == "x"));
    }

    #[test]
    fn malformed_answers_are_errors_not_panics() {
        let malformed =
            |t: &Canned| matches!(info(t), Err(RawError::Decode(m)) if m.contains("malformed"));
        assert!(malformed(&Canned(vec![])));
        assert!(malformed(&Canned(vec![7])));
        assert!(malformed(&Canned(vec![0, 1, 2])));
        let mut overlong = vec![0];
        overlong.extend(100u64.to_le_bytes());
        overlong.extend(b"short");
        assert!(malformed(&Canned(overlong)));
        assert!(malformed(&ok(&[b"{}", b"{}"])));
        assert!(malformed(&ok(&[b"not json"])));
        assert!(malformed(&ok(&[])));
        // A decode answer: wrong part count, odd sample bytes, an unknown CFA, an
        // image that doesn't validate.
        let head = |cfa: u8, w: u32| {
            let img = crate::dng::tests::linear(2, 2);
            let mut h = ImageHead::of(&img, 21);
            h.cfa = cfa;
            h.width = w;
            serde_json::to_vec(&h).unwrap()
        };
        let dec = |t: Canned| decode(&t, Input::None);
        let px = [0u8; 24];
        assert!(dec(ok(&[&head(4, 2), &px])).is_ok());
        assert!(matches!(dec(ok(&[&head(4, 2)])), Err(RawError::Decode(_))));
        assert!(matches!(
            dec(ok(&[&head(4, 2), &px[..23]])),
            Err(RawError::Decode(_))
        ));
        assert!(matches!(
            dec(ok(&[&head(9, 2), &px])),
            Err(RawError::Decode(_))
        ));
        assert!(matches!(
            dec(ok(&[&head(4, 3), &px])),
            Err(RawError::Invalid(_))
        ));
        // An error answer whose error doesn't parse.
        let mut e = vec![1];
        frame(&mut e, b"?");
        assert!(malformed(&Canned(e)));
        for c in [
            CfaPattern::Rggb,
            CfaPattern::Bggr,
            CfaPattern::Grbg,
            CfaPattern::Gbrg,
            CfaPattern::LinearRgb,
        ] {
            assert_eq!(cfa_of(cfa_code(c)), Some(c));
        }
        assert_eq!(cfa_of(5), None);
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// The process transport, with stand-in helpers written as shell scripts.
    #[test]
    fn helper_processes_are_checked_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let e = check(&missing).unwrap_err();
        assert!(e.to_string().contains("rawler helper can't be used"), "{e}");
        // Asked once: the answer is remembered for that path.
        assert_eq!(check(&missing).unwrap_err().to_string(), e.to_string());

        let silent = script(dir.path(), "silent", "exit 3");
        let e = check(&silent).unwrap_err().to_string();
        assert!(e.contains("no answer"), "{e}");

        // `{"protocol":99,"rawler":"x"}` is 28 bytes: status 0, length 28 (octal 034).
        let other = script(
            dir.path(),
            "other",
            r#"printf '\000\034\000\000\000\000\000\000\000{"protocol":99,"rawler":"x"}'"#,
        );
        let e = check(&other).unwrap_err().to_string();
        assert!(e.contains("protocol 99"), "{e}");
        let good = script(
            dir.path(),
            "good",
            r#"printf '\000\033\000\000\000\000\000\000\000{"protocol":1,"rawler":"x"}'"#,
        );
        assert_eq!(check(&good).unwrap().rawler, "x");
        // stdin is the file given, readable from the start.
        let echo = script(dir.path(), "echo", "cat");
        let out = Process(echo)
            .call(&[], Input::bytes(&[b"ab", b"cd"]).unwrap())
            .unwrap();
        assert_eq!(out, b"abcd");
    }

    #[test]
    fn the_configured_helper_wins_over_the_environment() {
        let p = PathBuf::from("/nonexistent/darkslide-rawler");
        set_rawler_helper(Some(p.clone()));
        assert_eq!(rawler_helper(), Some(p));
        assert!(
            client().is_err(),
            "a helper that doesn't run is an error, not none"
        );
        set_rawler_helper(None);
        assert_eq!(
            rawler_helper(),
            std::env::var_os(HELPER_ENV).map(PathBuf::from)
        );
    }

    #[test]
    fn pipes_are_read_to_their_end() {
        let mut child = Command::new("/bin/echo")
            .arg("piped")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let f = File::from(std::os::fd::OwnedFd::from(out));
        assert_eq!(read_input(f).unwrap(), b"piped\n");
        child.wait().unwrap();
    }
}
