//! Canon CR2: one lossless JPEG stream (T.81 process 14) in the last IFD of
//! the chain, cut into vertical slices (`CR2Slice`, 0xC640: `n`, width of
//! each of the first `n` slices, width of the last). The stream's samples,
//! in order, fill slice 0 top to bottom, then slice 1, and so on, whatever
//! the frame's own width, height and component count.
//!
//! From the maker note (an IFD in the Canon `MakerNote`): `SensorInfo`
//! (0x00E0) gives the crop borders (left, top, right, bottom, inclusive, at
//! indices 5..=8), and `ColorData` (0x4001) the as-shot white balance and
//! the levels. `ColorData`'s layout depends on its version; only layouts
//! checked against the corpus are read (see [`LAYOUTS`]), other files are
//! left to the fallback. sRAW/mRAW (subsampled YCbCr) files are too.

use ds_core::CfaPattern;
use rayon::prelude::*;

use super::{Sensor, ljpeg, orientation};
use crate::RawError;
use crate::container::Container;
use crate::source::Source;
use crate::tiff::{Ifd, tag};
use crate::{cameras, sony};

/// `CR2Slice`.
const SLICES: u16 = 0xC640;
/// Maker note tags.
const SENSOR_INFO: u16 = 0x00E0;
const COLOR_DATA: u16 = 0x4001;

/// Where a `ColorData` layout keeps what we read: its value count, then the
/// indices of `WB_RGGBLevelsAsShot` (4 values), the per-channel black
/// levels (4, by position in the 2×2 repeat) and the specular white level.
/// Each row was checked against a corpus file (docs/RAWDEC.md).
const LAYOUTS: &[(u32, usize, usize, usize)] = &[
    (1592, 63, 778, 783), // version 13: EOS 5D Mark IV
    (1602, 63, 778, 783), // version 15: EOS 6D Mark II
];

/// The IFD holding the lossless JPEG: the last of the chain with JPEG
/// compression whose data starts with SOI and contains a lossless frame.
fn raw_ifd<'c>(
    src: &dyn Source,
    c: &'c Container,
) -> Result<Option<(&'c Ifd, u64, u64)>, RawError> {
    for ifd in c.tiff.chain.iter().rev() {
        if ifd.uint(src, tag::COMPRESSION)? != Some(6) {
            continue;
        }
        let (Some(off), Some(len)) = (
            ifd.uint(src, tag::STRIP_OFFSETS)?,
            ifd.uint(src, tag::STRIP_BYTE_COUNTS)?,
        ) else {
            continue;
        };
        let (off, len) = (u64::from(off), u64::from(len));
        crate::source::check_range(off, len, 0, src.end())?;
        let head = src.read(off, len.min(4096))?;
        if head.starts_with(&[0xFF, 0xD8]) && head.windows(2).any(|w| w == [0xFF, 0xC3]) {
            return Ok(Some((ifd, off, len)));
        }
    }
    Ok(None)
}

/// As-shot WB (R, G, B), black by 2×2 position, and white level.
type Levels = ([f32; 3], [f32; 4], f32);

/// White balance and levels from `ColorData`, for the layouts we know.
fn color_data(src: &dyn Source, mn: &Ifd) -> Result<Option<Levels>, RawError> {
    let Some(e) = mn.get(COLOR_DATA) else {
        return Ok(None);
    };
    let Some(&(_, wb, black, white)) = LAYOUTS.iter().find(|l| l.0 == e.count) else {
        return Ok(None);
    };
    let Some(v) = mn.uints_n(src, COLOR_DATA, white as u64 + 1)? else {
        return Ok(None);
    };
    if v.len() <= white {
        return Ok(None);
    }
    let f = |i: usize| v[i] as f32;
    let g = f(wb + 1).midpoint(f(wb + 2));
    Ok(Some((
        [f(wb), g, f(wb + 3)],
        [f(black), f(black + 1), f(black + 2), f(black + 3)],
        f(white),
    )))
}

/// Slice widths, left to right: `n` × `w`, then `last` (one slice of the
/// whole width without the tag).
fn slices(src: &dyn Source, raw: &Ifd, total: usize) -> Result<Vec<usize>, RawError> {
    Ok(match raw.uints(src, SLICES)?.as_deref() {
        Some(&[n, w, last, ..]) if w > 0 && last > 0 && n < 64 => {
            let mut v = vec![w as usize; n as usize];
            v.push(last as usize);
            v
        }
        _ => vec![total],
    })
}

/// Decode a CR2; `None` for layouts we don't read.
pub(super) fn decode(src: &dyn Source, c: &Container) -> Result<Option<Sensor>, RawError> {
    let Some((raw, off, len)) = raw_ifd(src, c)? else {
        return Ok(None);
    };
    let Some(mn) = c
        .exif
        .as_ref()
        .map(|e| sony::makernote(src, e))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Some((wb, black, white)) = color_data(src, &mn)? else {
        return Ok(None);
    };
    let bytes = src.read(off, len)?;
    // sRAW/mRAW: three subsampled components (Y, Cb, Cr).
    if crate::preview::jpeg_frame(&bytes).is_some_and(|(_, _, nc)| nc == 3) {
        return Ok(None);
    }
    let lj = ljpeg::Ljpeg::parse(&bytes)?;
    let total = lj.frame.samples();
    let widths = slices(src, raw, lj.frame.row_len())?;
    let width: usize = widths.iter().sum();
    if width == 0 || !total.is_multiple_of(width) {
        return Err(RawError::Malformed(format!(
            "CR2 slices {widths:?} don't tile a frame of {total} samples"
        )));
    }
    let height = total / width;
    let data = if widths.len() == 1 {
        let mut data = vec![0u16; total];
        lj.decode_into(&mut data)?;
        data
    } else {
        let mut stream = vec![0u16; total];
        lj.decode_into(&mut stream)?;
        let mut data = vec![0u16; total];
        // Slice s starts after all earlier slices' columns, top to bottom.
        let starts: Vec<usize> = widths
            .iter()
            .scan(0, |acc, &w| {
                let s = *acc;
                *acc += w * height;
                Some(s)
            })
            .collect();
        data.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
            let mut x = 0;
            for (&w, &start) in widths.iter().zip(&starts) {
                row[x..x + w].copy_from_slice(&stream[start + y * w..][..w]);
                x += w;
            }
        });
        data
    };

    let make = c.make(src)?;
    let model = c.model(src)?;
    let cam = cameras::lookup(&make, &model, "");
    let crop = match mn.uints(src, SENSOR_INFO)?.as_deref() {
        Some(&[_, _, _, _, _, l, t, r, b, ..])
            if l < r && t < b && (r as usize) < width && (b as usize) < height =>
        {
            Some([
                l as usize,
                t as usize,
                (r - l + 1) as usize,
                (b - t + 1) as usize,
            ])
        }
        _ => cam.crop.and_then(|m| super::from_margins(m, width, height)),
    };
    Ok(Some(Sensor {
        make,
        model,
        width,
        height,
        cpp: 1,
        data,
        // Canon's sensors start with red; the camera table knows exceptions.
        cfa: cam.cfa.unwrap_or(CfaPattern::Rggb),
        black,
        white,
        crop,
        wb,
        matrix: None,
        orientation: orientation(src, c),
    }))
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
pub(crate) mod tests {
    use super::super::ljpeg::enc::{Opts, encode};
    use super::super::testfile::{Link, Node, build, sample};
    use super::*;
    use crate::source::SliceSource;
    use crate::tiff::Endian;
    use crate::tiff::build::Val;

    /// A CR2: IFD0 → Exif → Canon maker note; the raw stream in IFD3.
    /// `slices` as in `CR2Slice`; the stream has 4 components.
    pub fn cr2(
        w: usize,
        h: usize,
        slices: Option<[u16; 3]>,
        cd_count: usize,
    ) -> (Vec<u8>, Vec<u16>) {
        let img: Vec<u16> = (0..w * h)
            .map(|i| sample(i % w, i / w, 0, 1 << 14))
            .collect();
        let widths: Vec<usize> = match slices {
            Some([n, sw, last]) => {
                let mut v = vec![usize::from(sw); usize::from(n)];
                v.push(usize::from(last));
                v
            }
            None => vec![w],
        };
        // The stream: slice by slice, each top to bottom.
        let mut stream = Vec::new();
        let mut x0 = 0;
        for sw in widths {
            for y in 0..h {
                stream.extend_from_slice(&img[y * w + x0..y * w + x0 + sw]);
            }
            x0 += sw;
        }
        let lj = encode(&stream, w / 4, h, 4, &Opts::default());
        let mut cd = vec![0u16; cd_count];
        if cd_count > 783 {
            cd[63..67].copy_from_slice(&[2000, 1024, 1024, 1500]);
            cd[778..782].copy_from_slice(&[511, 512, 513, 514]);
            cd[783] = 15000;
        }
        let mut raw = vec![
            (tag::IMAGE_WIDTH, Val::Short(vec![w as u16])),
            (tag::IMAGE_LENGTH, Val::Short(vec![h as u16])),
            (tag::COMPRESSION, Val::Short(vec![6])),
        ];
        if let Some(s) = slices {
            raw.push((SLICES, Val::Short(s.to_vec())));
        }
        let small = |n: u16| {
            vec![
                (tag::IMAGE_WIDTH, Val::Short(vec![n])),
                (tag::COMPRESSION, Val::Short(vec![6])),
            ]
        };
        let f = build(
            Endian::Little,
            &[
                Node::new(
                    Link::Chain,
                    vec![
                        (tag::MAKE, Val::Ascii("Canon")),
                        (tag::MODEL, Val::Ascii("Canon EOS Test")),
                        (tag::ORIENTATION, Val::Short(vec![8])),
                    ],
                ),
                Node::new(Link::Exif(0), vec![(tag::ISO, Val::Short(vec![100]))]),
                Node::new(
                    Link::MakerNote(1),
                    vec![
                        (
                            SENSOR_INFO,
                            Val::Short(vec![
                                34,
                                w as u16,
                                h as u16,
                                1,
                                1,
                                2,
                                1,
                                w as u16 - 3,
                                h as u16 - 2,
                                0,
                            ]),
                        ),
                        (COLOR_DATA, Val::Short(cd)),
                    ],
                ),
                // A JPEG preview (no lossless frame) and an RGB thumbnail.
                Node::new(Link::Chain, small(160))
                    .with_blocks(vec![vec![0xFF, 0xD8, 0xFF, 0xC0]], false),
                Node::new(Link::Chain, vec![(tag::COMPRESSION, Val::Short(vec![1]))]),
                Node::new(Link::Chain, raw).with_blocks(vec![lj], false),
            ],
        );
        (f, img)
    }

    fn sensor(f: &[u8]) -> Option<Sensor> {
        let src = SliceSource::new(f);
        let c = Container::parse(&src).unwrap();
        decode(&src, &c).unwrap()
    }

    #[test]
    fn slices_reassemble_and_levels_come_from_color_data() {
        for slices in [Some([2, 16, 8]), Some([0, 40, 40]), None] {
            let (f, img) = cr2(40, 6, slices, 1592);
            let s = sensor(&f).unwrap();
            assert_eq!((s.width, s.height), (40, 6), "{slices:?}");
            assert_eq!(s.data, img, "{slices:?}");
            assert_eq!(s.black, [511.0, 512.0, 513.0, 514.0]);
            assert_eq!(s.white, 15000.0);
            assert_eq!(s.wb, [2000.0, 1024.0, 1500.0]);
            // SensorInfo borders 2..=37 x 1..=4.
            assert_eq!(s.crop, Some([2, 1, 36, 4]));
            assert_eq!(s.cfa, CfaPattern::Rggb);
            assert_eq!(s.orientation, ds_core::Orientation::Rotate270);
        }
    }

    #[test]
    fn unknown_color_data_and_odd_slices() {
        // A ColorData layout we haven't checked: the fallback's.
        let (f, _) = cr2(40, 6, None, 1500);
        assert!(sensor(&f).is_none());
        // Slices that don't tile the frame are damage.
        let (mut f, _) = cr2(40, 6, Some([1, 20, 20]), 1592);
        // Rewrite the tag to slices of 13: 26 doesn't divide 240 samples.
        let at = f
            .windows(6)
            .rposition(|w| w == [1, 0, 20, 0, 20, 0])
            .unwrap();
        f[at + 2] = 13;
        f[at + 4] = 13;
        let src = SliceSource::new(&f);
        let c = Container::parse(&src).unwrap();
        assert!(matches!(decode(&src, &c), Err(RawError::Malformed(_))));
    }

    #[test]
    fn no_maker_note_or_no_lossless_stream_is_not_ours() {
        let only_preview = build(
            Endian::Little,
            &[Node::new(
                Link::Chain,
                vec![
                    (tag::MAKE, Val::Ascii("Canon")),
                    (tag::COMPRESSION, Val::Short(vec![6])),
                ],
            )
            .with_blocks(vec![vec![0xFF, 0xD8, 0xFF, 0xC0]], false)],
        );
        assert!(sensor(&only_preview).is_none());
        // A lossless stream but no Exif / maker note.
        let lj = encode(&[1u16; 64], 4, 4, 4, &Opts::default());
        let bare = build(
            Endian::Little,
            &[Node::new(
                Link::Chain,
                vec![
                    (tag::MAKE, Val::Ascii("Canon")),
                    (tag::COMPRESSION, Val::Short(vec![6])),
                ],
            )
            .with_blocks(vec![lj], false)],
        );
        assert!(sensor(&bare).is_none());
    }
}
