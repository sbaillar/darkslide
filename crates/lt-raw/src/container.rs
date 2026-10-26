//! A parsed TIFF-based RAW container: the IFDs every entry point needs.
//!
//! The IFD graph is walked once with one global seen-set of offsets (across
//! the IFD0 chain, `SubIFDs` and the Exif IFD) and at most [`MAX_IFDS`] IFDs
//! in total, so crafted graphs (cycles, fan-out, shared children) cost a
//! bounded amount of work. When the walk hits a limit, [`Container::overflow`]
//! is set and the rawler fallback refuses the file rather than letting
//! rawler walk the same graph without limits.

use crate::RawError;
use crate::source::Source;
use crate::tiff::{Ifd, MAX_ARRAY, Tiff, tag};

/// Total IFDs parsed per file (chain + children). Real ARW: 4.
pub(crate) const MAX_IFDS: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct Container {
    pub tiff: Tiff,
    /// `SubIFDs` of every IFD in the IFD0 chain.
    pub subs: Vec<Ifd>,
    pub exif: Option<Ifd>,
    /// The GPS IFD (IFD0 `GPSInfo`), when the camera recorded a position.
    pub gps: Option<Ifd>,
    /// The IFD graph exceeded [`MAX_IFDS`] / [`MAX_ARRAY`] and was cut short.
    pub overflow: bool,
}

struct Walk<'a> {
    src: &'a dyn Source,
    seen: Vec<u64>,
    overflow: bool,
}

impl Walk<'_> {
    /// Parse the IFD at `off` unless already seen or over budget.
    fn visit(&mut self, off: u32, endian: crate::tiff::Endian) -> Option<Ifd> {
        let off = u64::from(off);
        if off == 0 || self.seen.contains(&off) {
            return None;
        }
        if self.seen.len() >= MAX_IFDS {
            self.overflow = true;
            return None;
        }
        self.seen.push(off);
        Ifd::parse(self.src, off, endian).ok()
    }

    /// Offsets listed by `tag` in `parent` (bounded), flagging oversized lists.
    fn offsets(&mut self, parent: &Ifd, tag: u16) -> Vec<u32> {
        if parent
            .get(tag)
            .is_some_and(|e| u64::from(e.count) > MAX_ARRAY)
        {
            self.overflow = true;
        }
        parent
            .uints(self.src, tag)
            .ok()
            .flatten()
            .unwrap_or_default()
    }
}

impl Container {
    /// Classic TIFF (`II*\0` / `MM\0*`) only.
    pub fn parse(src: &dyn Source) -> Result<Self, RawError> {
        Self::parse_with(src, false)
    }

    /// With `variants`, also ORF/RW2 signatures (used for size guarding only).
    pub fn parse_with(src: &dyn Source, variants: bool) -> Result<Self, RawError> {
        let tiff = Tiff::parse(src, variants)?;
        let mut w = Walk {
            src,
            seen: tiff.chain.iter().map(|i| i.off).collect(),
            overflow: false,
        };
        let mut subs = Vec::new();
        for ifd in &tiff.chain {
            for off in w.offsets(ifd, tag::SUB_IFDS) {
                subs.extend(w.visit(off, tiff.endian));
            }
        }
        let exif = w
            .offsets(tiff.ifd0(), tag::EXIF_IFD)
            .into_iter()
            .find_map(|off| w.visit(off, tiff.endian));
        let gps = w
            .offsets(tiff.ifd0(), tag::GPS_IFD)
            .into_iter()
            .find_map(|off| w.visit(off, tiff.endian));
        Ok(Self {
            tiff,
            subs,
            exif,
            gps,
            overflow: w.overflow,
        })
    }

    pub fn ifd0(&self) -> &Ifd {
        self.tiff.ifd0()
    }

    /// `Make`, trimmed. Empty string when absent.
    pub fn make(&self, src: &dyn Source) -> Result<String, RawError> {
        Ok(self.ifd0().ascii(src, tag::MAKE)?.unwrap_or_default())
    }

    pub fn model(&self, src: &dyn Source) -> Result<String, RawError> {
        Ok(self.ifd0().ascii(src, tag::MODEL)?.unwrap_or_default())
    }

    pub fn is_sony(&self, src: &dyn Source) -> Result<bool, RawError> {
        Ok(self.make(src)?.eq_ignore_ascii_case("SONY"))
    }

    /// All IFDs: the IFD0 chain, the `SubIFDs`, then the Exif IFD.
    pub fn all(&self) -> impl Iterator<Item = &Ifd> {
        self.tiff.chain.iter().chain(&self.subs).chain(&self.exif)
    }

    /// Sum of the declared out-of-line value sizes over every parsed IFD:
    /// roughly what a decoder that loads all tag values eagerly would copy.
    pub fn declared_value_bytes(&self) -> u64 {
        self.all()
            .flat_map(|i| &i.entries)
            .filter_map(crate::tiff::Entry::byte_len)
            .filter(|&n| n > 4)
            .fold(0u64, u64::saturating_add)
    }

    /// The full-resolution sensor-data IFD: the largest non-JPEG,
    /// non-reduced-resolution image with strips or tiles.
    pub fn raw_ifd(&self, src: &dyn Source) -> Result<Option<&Ifd>, RawError> {
        let mut best: Option<(u64, &Ifd)> = None;
        for ifd in self.tiff.chain.iter().chain(&self.subs) {
            if ifd.get(tag::STRIP_OFFSETS).is_none() && ifd.get(tag::TILE_OFFSETS).is_none() {
                continue;
            }
            if ifd.uint(src, tag::COMPRESSION)? == Some(6)
                || ifd.uint(src, tag::NEW_SUBFILE_TYPE)?.unwrap_or(0) & 1 == 1
            {
                continue;
            }
            let (Some(w), Some(h)) = (
                ifd.uint(src, tag::IMAGE_WIDTH)?,
                ifd.uint(src, tag::IMAGE_LENGTH)?,
            ) else {
                continue;
            };
            let area = u64::from(w) * u64::from(h);
            if best.is_none_or(|(a, _)| area > a) {
                best = Some((area, ifd));
            }
        }
        Ok(best.map(|(_, i)| i))
    }
}

/// Recommended crop inside the raw IFD as `(x, y, w, h)`: `DefaultCropOrigin/Size`
/// when present and in bounds, else the full raw image.
pub(crate) fn crop_rect(
    src: &dyn Source,
    raw: &Ifd,
    raw_w: u32,
    raw_h: u32,
) -> Result<[u32; 4], RawError> {
    let origin = raw.rationals(src, tag::DEFAULT_CROP_ORIGIN)?;
    let size = raw.rationals(src, tag::DEFAULT_CROP_SIZE)?;
    if let (Some([ox, oy, ..]), Some([sw, sh, ..])) = (origin.as_deref(), size.as_deref()) {
        let v = [ox, oy, sw, sh].map(|r| r.to_f64().unwrap_or(-1.0));
        if v.iter().all(|x| (0.0..=f64::from(u32::MAX)).contains(x)) {
            let [x, y, w, h] = v.map(|x| x.round() as u32);
            if w >= 2
                && h >= 2
                && x.checked_add(w).is_some_and(|r| r <= raw_w)
                && y.checked_add(h).is_some_and(|b| b <= raw_h)
            {
                return Ok([x, y, w, h]);
            }
        }
    }
    Ok([0, 0, raw_w, raw_h])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SliceSource;
    use crate::tiff::Endian;
    use crate::tiff::build::{Val, header, ifd};

    fn one_ifd(entries: &[(u16, Val)]) -> Vec<u8> {
        let mut f = header(Endian::Little);
        f.extend(ifd(Endian::Little, 8, entries, 0));
        f
    }

    #[test]
    fn crop_rect_validates_bounds() {
        let ok = one_ifd(&[
            (tag::DEFAULT_CROP_ORIGIN, Val::Long(vec![12, 12])),
            (
                tag::DEFAULT_CROP_SIZE,
                Val::Rational(vec![(6000, 1), (4000, 1)]),
            ),
        ]);
        let src = SliceSource::new(&ok);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(
            crop_rect(&src, t.ifd0(), 6048, 4024).unwrap(),
            [12, 12, 6000, 4000]
        );
        // Crop exceeding the raw bounds falls back to the full frame.
        assert_eq!(
            crop_rect(&src, t.ifd0(), 6000, 4000).unwrap(),
            [0, 0, 6000, 4000]
        );
        // Negative / zero-denominator values fall back too.
        let bad = one_ifd(&[
            (
                tag::DEFAULT_CROP_ORIGIN,
                Val::SRational(vec![(-4, 1), (0, 1)]),
            ),
            (
                tag::DEFAULT_CROP_SIZE,
                Val::Rational(vec![(10, 0), (10, 1)]),
            ),
        ]);
        let src = SliceSource::new(&bad);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(
            crop_rect(&src, t.ifd0(), 100, 100).unwrap(),
            [0, 0, 100, 100]
        );
        // Absent tags.
        let none = one_ifd(&[(tag::ORIENTATION, Val::Short(vec![1]))]);
        let src = SliceSource::new(&none);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(crop_rect(&src, t.ifd0(), 8, 6).unwrap(), [0, 0, 8, 6]);
    }

    #[test]
    fn raw_ifd_skips_jpeg_and_reduced_resolution() {
        // IFD0 = JPEG preview; no SubIFDs; nothing qualifies.
        let f = one_ifd(&[
            (tag::COMPRESSION, Val::Short(vec![6])),
            (tag::STRIP_OFFSETS, Val::Long(vec![0])),
            (tag::IMAGE_WIDTH, Val::Long(vec![100])),
            (tag::IMAGE_LENGTH, Val::Long(vec![100])),
        ]);
        let src = SliceSource::new(&f);
        let c = Container::parse(&src).unwrap();
        assert!(c.raw_ifd(&src).unwrap().is_none());
        assert!(c.exif.is_none());
        assert_eq!(c.make(&src).unwrap(), "");
        assert!(!c.is_sony(&src).unwrap());
        // Reduced-resolution (NewSubfileType bit 0) is skipped; missing dims skipped.
        let f = one_ifd(&[
            (tag::NEW_SUBFILE_TYPE, Val::Long(vec![1])),
            (tag::STRIP_OFFSETS, Val::Long(vec![0])),
            (tag::IMAGE_WIDTH, Val::Long(vec![100])),
            (tag::IMAGE_LENGTH, Val::Long(vec![100])),
        ]);
        let src = SliceSource::new(&f);
        assert!(
            Container::parse(&src)
                .unwrap()
                .raw_ifd(&src)
                .unwrap()
                .is_none()
        );
        let f = one_ifd(&[
            (tag::TILE_OFFSETS, Val::Long(vec![0])),
            (tag::IMAGE_WIDTH, Val::Long(vec![100])),
        ]);
        let src = SliceSource::new(&f);
        assert!(
            Container::parse(&src)
                .unwrap()
                .raw_ifd(&src)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn children_skip_bad_offsets() {
        let mut f = header(Endian::Little);
        let child_at = 8 + 2 + 12 + 4 + 8; // IFD0 table + 2 LONGs out of line
        let ifd0 = ifd(
            Endian::Little,
            8,
            &[(tag::SUB_IFDS, Val::Long(vec![child_at, 0xFFFF_0000]))],
            0,
        );
        assert_eq!(ifd0.len() as u32, child_at - 8);
        f.extend(ifd0);
        f.extend(ifd(
            Endian::Little,
            child_at,
            &[(tag::COMPRESSION, Val::Short(vec![1]))],
            0,
        ));
        let src = SliceSource::new(&f);
        let c = Container::parse(&src).unwrap();
        assert_eq!(c.subs.len(), 1);
        assert_eq!(c.subs[0].uint(&src, tag::COMPRESSION).unwrap(), Some(1));
        assert!(c.exif.is_none());
        assert!(!c.overflow);
    }

    /// IFD0 whose `SubIFDs` list its own offset, a duplicate child, and a huge
    /// fan-out: every IFD is parsed at most once and the total is capped.
    #[test]
    fn graph_walk_is_global_and_capped() {
        let e = Endian::Little;
        // 100 distinct child IFDs, each 18 bytes, after IFD0.
        let n = 100u32;
        let ifd0_len = 2 + 12 + 4 + 4 * (n + 2);
        let first_child = 8 + ifd0_len;
        let mut offs: Vec<u32> = vec![8, first_child]; // self + duplicate of child 0
        offs.extend((0..n).map(|k| first_child + 18 * k));
        let mut f = header(e);
        let ifd0 = ifd(e, 8, &[(tag::SUB_IFDS, Val::Long(offs))], 0);
        assert_eq!(ifd0.len() as u32, ifd0_len);
        f.extend(ifd0);
        for k in 0..n {
            f.extend(ifd(
                e,
                first_child + 18 * k,
                &[(tag::ORIENTATION, Val::Short(vec![1]))],
                0,
            ));
        }
        let src = SliceSource::new(&f);
        let c = Container::parse(&src).unwrap();
        assert!(c.overflow);
        assert_eq!(c.all().count(), MAX_IFDS);
        let mut offs: Vec<u64> = c.all().map(|i| i.off).collect();
        offs.sort_unstable();
        offs.dedup();
        assert_eq!(offs.len(), MAX_IFDS, "no IFD parsed twice");
        assert!(c.declared_value_bytes() >= 4 * u64::from(n));

        // A SubIFDs entry declaring more than MAX_ARRAY offsets is flagged too.
        let mut f = header(e);
        f.extend(ifd(
            e,
            8,
            &[(
                tag::SUB_IFDS,
                Val::Raw {
                    typ: 4,
                    count: 100_000,
                    bytes: 8u32.to_le_bytes().to_vec(),
                },
            )],
            0,
        ));
        f.resize(2048, 0);
        let src = SliceSource::new(&f);
        assert!(Container::parse(&src).unwrap().overflow);
    }
}
