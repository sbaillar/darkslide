//! Lossless JPEG (ITU-T T.81 Annex H, process 14), as RAW files use it: one
//! `SOF3` frame, Huffman-coded prediction differences, predictors 1–7, any
//! number of 1×1-sampled components interleaved in one scan, an optional
//! point transform and restart intervals.
//!
//! Decoding is table driven: one lookup on the next [`LUT_BITS`] bits yields
//! the code length and, when the difference bits also fit, the decoded
//! difference itself, so most samples cost one load and one shift. Longer
//! codes take the canonical slow path (T.81 F.2.2.3). Rows are decoded into
//! caller buffers; nothing is allocated per sample.

use crate::RawError;

/// Bits looked up at once (a 16K-entry table, 64 KB: L1/L2 resident).
const LUT_BITS: u32 = 14;
const LUT_SIZE: usize = 1 << LUT_BITS;
/// Table entry flag: the difference is complete (`len + ssss <= LUT_BITS`).
const FULL: u32 = 1 << 10;
/// Table entry flag: the code is longer than [`LUT_BITS`].
const SLOW: u32 = 1 << 11;
/// How far past the end of the entropy-coded data a decode may read before the
/// stream counts as truncated (the reader pads with zeros up to here).
const MAX_OVERRUN_BITS: u64 = 64;

fn bad(msg: impl Into<String>) -> RawError {
    RawError::Malformed(format!("lossless JPEG: {}", msg.into()))
}

/// Read position in an unstuffed segment (kept in registers while a line
/// decodes: see [`Bits`]).
#[derive(Debug, Clone, Copy, Default)]
struct State {
    /// Bytes of the segment loaded into `buf` so far.
    pos: usize,
    /// Left-aligned bits; `nbits` of them are valid.
    buf: u64,
    nbits: u32,
    /// An invalid Huffman code was read (checked once per line).
    bad_code: bool,
}

/// True when any byte of `v` is 0xFF.
#[inline]
fn has_ff(v: u64) -> bool {
    let x = !v;
    (x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080) != 0
}

/// Index of the first 0xFF in `d`, eight bytes at a time.
fn find_ff(d: &[u8]) -> Option<usize> {
    let mut i = 0;
    for w in d.as_chunks::<8>().0 {
        if has_ff(u64::from_ne_bytes(*w)) {
            break;
        }
        i += 8;
    }
    d[i..].iter().position(|&b| b == 0xFF).map(|k| i + k)
}

/// JPEG entropy-coded data, one segment (the data up to the next marker)
/// at a time, copied with its `FF 00` stuffing removed so that reads are
/// plain 8-byte loads. Reads past the end of a segment yield zeros and are
/// counted.
#[derive(Debug)]
pub(crate) struct JpegBits<'a> {
    src: &'a [u8],
    /// Where the current segment ends in `src` (at a marker or the end).
    src_pos: usize,
    /// The current segment, unstuffed, plus 8 zero bytes.
    clean: Vec<u8>,
    /// Real bytes in `clean`.
    len: usize,
    st: State,
}

impl<'a> JpegBits<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        let mut b = Self {
            src,
            src_pos: 0,
            clean: Vec::with_capacity(src.len() + 8),
            len: 0,
            st: State::default(),
        };
        b.load_segment();
        b
    }

    /// Unstuff `src` from `src_pos` up to the next marker.
    fn load_segment(&mut self) {
        self.clean.clear();
        let d = self.src;
        let mut i = self.src_pos;
        while let Some(k) = find_ff(&d[i..]) {
            self.clean.extend_from_slice(&d[i..i + k]);
            i += k;
            if d.get(i + 1) != Some(&0) {
                break; // a marker (or a lone FF at the end)
            }
            self.clean.push(0xFF);
            i += 2;
        }
        if !d[i..].starts_with(&[0xFF]) {
            self.clean.extend_from_slice(&d[i..]);
            i = d.len();
        }
        self.src_pos = i;
        self.len = self.clean.len();
        self.clean.extend_from_slice(&[0; 8]);
        self.st = State::default();
    }

    /// A reader at the current position; hand it back with [`Self::store`].
    fn reader(&self) -> Bits<'_> {
        Bits {
            data: &self.clean,
            len: self.len,
            st: self.st,
        }
    }

    fn store(&mut self, st: State) {
        self.st = st;
    }

    /// Bits consumed beyond the real data of the segment.
    fn overrun(&self) -> u64 {
        (self.st.pos as u64 * 8)
            .saturating_sub(u64::from(self.st.nbits))
            .saturating_sub(self.len as u64 * 8)
    }

    /// Skip the restart marker `RSTn` that must end this segment, and load
    /// the next one.
    fn restart(&mut self, n: u8) -> Result<(), RawError> {
        if self.overrun() > MAX_OVERRUN_BITS {
            return Err(RawError::Truncated(
                "lossless JPEG interval ends early".into(),
            ));
        }
        let d = self.src;
        // Fill bytes (0xFF) may precede the marker (T.81 B.1.1.2).
        while d.get(self.src_pos) == Some(&0xFF) && d.get(self.src_pos + 1) == Some(&0xFF) {
            self.src_pos += 1;
        }
        if d.get(self.src_pos..self.src_pos + 2) != Some(&[0xFF, 0xD0 + (n & 7)]) {
            return Err(bad(format!("missing RST{} marker", n & 7)));
        }
        self.src_pos += 2;
        self.load_segment();
        Ok(())
    }
}

/// MSB-first bit reader over an unstuffed segment. `Copy`, so a decoding
/// loop keeps it in registers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bits<'s> {
    /// The segment plus 8 zero bytes.
    data: &'s [u8],
    len: usize,
    st: State,
}

impl<'s> Bits<'s> {
    /// A reader over plain (unstuffed) MSB-first data: `padded` is the data
    /// followed by 8 zero bytes.
    pub fn over(padded: &'s [u8]) -> Self {
        Self {
            data: padded,
            len: padded.len().saturating_sub(8),
            st: State::default(),
        }
    }

    /// Bits read beyond the real data.
    pub fn overrun(&self) -> u64 {
        (self.st.pos as u64 * 8)
            .saturating_sub(u64::from(self.st.nbits))
            .saturating_sub(self.len as u64 * 8)
    }

    /// An invalid Huffman code was read.
    pub fn bad_code(&self) -> bool {
        self.st.bad_code
    }

    /// Make sure at least 32 bits (a code and its difference) are buffered.
    #[inline]
    pub fn need32(&mut self) {
        if self.st.nbits < 32 {
            self.refill();
        }
    }

    /// Top up to 56..=63 buffered bits with one 8-byte load.
    #[inline]
    pub fn refill(&mut self) {
        let p = self.st.pos.min(self.len);
        let mut w = [0u8; 8];
        w.copy_from_slice(&self.data[p..p + 8]);
        self.st.buf |= u64::from_be_bytes(w) >> self.st.nbits;
        self.st.pos = self
            .st
            .pos
            .wrapping_add(((63 - self.st.nbits) >> 3) as usize);
        self.st.nbits |= 0x38; // 56..=63: the bytes just loaded
    }

    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        (self.st.buf >> (64 - n)) as u32
    }

    #[inline]
    pub fn consume(&mut self, n: u32) {
        self.st.buf <<= n;
        self.st.nbits = self.st.nbits.wrapping_sub(n);
    }

    /// `n` (1..=16) bits; the buffer must hold them.
    #[inline]
    pub fn bits(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.consume(n);
        v
    }
}

/// One Huffman table (DC class: a symbol is the bit count of a difference).
#[derive(Debug, Clone)]
pub(crate) struct Huffman {
    lut: Box<[u32; LUT_SIZE]>,
    /// Canonical decoding for codes longer than [`LUT_BITS`]: per length,
    /// the largest code (or -1) and the index of its first symbol.
    maxcode: [i32; 17],
    valptr: [i32; 17],
    mincode: [i32; 17],
    values: Vec<u8>,
}

/// The difference coded by `bits` (`ssss` bits), T.81 F.12 `EXTEND`.
#[inline]
fn extend(bits: u32, ssss: u32) -> i32 {
    if ssss == 16 {
        // No extra bits; 32768 is congruent to -32768 modulo 2^16.
        return -32768;
    }
    let v = bits as i32;
    if ssss == 0 {
        0
    } else if v < 1 << (ssss - 1) {
        v - (1 << ssss) + 1
    } else {
        v
    }
}

impl Huffman {
    /// Build from a `DHT` specification: code counts per length 1..=16 and
    /// the symbols in code order. Symbols are difference sizes (0..=16).
    pub fn new(counts: &[u8; 16], values: &[u8]) -> Result<Self, RawError> {
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        if total != values.len() || total == 0 || values.iter().any(|&v| v > 16) {
            return Err(bad("invalid Huffman table"));
        }
        let Ok(mut lut) = <Box<[u32; LUT_SIZE]>>::try_from(vec![0u32; LUT_SIZE].into_boxed_slice())
        else {
            unreachable!("the vector has LUT_SIZE entries");
        };
        let mut maxcode = [-1i32; 17];
        let mut valptr = [0i32; 17];
        let mut mincode = [0i32; 17];
        let mut code: u32 = 0;
        let mut k = 0usize;
        for len in 1..=16u32 {
            let n = usize::from(counts[len as usize - 1]);
            if n > 0 {
                valptr[len as usize] = k as i32;
                mincode[len as usize] = code as i32;
            }
            for _ in 0..n {
                if code >= 1 << len {
                    return Err(bad("Huffman table overflows its code space"));
                }
                let ssss = u32::from(values[k]);
                if len <= LUT_BITS {
                    let free = LUT_BITS - len;
                    let base = (code << free) as usize;
                    for (j, slot) in lut[base..base + (1 << free)].iter_mut().enumerate() {
                        *slot = if ssss == 16 || ssss <= free {
                            let extra = if ssss == 16 || ssss == 0 {
                                0
                            } else {
                                (j as u32 >> (free - ssss)) & ((1 << ssss) - 1)
                            };
                            let used = if ssss == 16 { len } else { len + ssss };
                            let diff = extend(extra, ssss) as i16 as u16;
                            (u32::from(diff) << 16) | FULL | used
                        } else {
                            (ssss << 5) | len
                        };
                    }
                } else {
                    let base = (code >> (len - LUT_BITS)) as usize;
                    lut[base] = SLOW;
                }
                code += 1;
                k += 1;
            }
            if n > 0 {
                maxcode[len as usize] = code as i32 - 1;
            }
            code <<= 1;
        }
        Ok(Self {
            lut,
            maxcode,
            valptr,
            mincode,
            values: values.to_vec(),
        })
    }

    /// Decode one difference. The reader must hold at least 32 bits. An
    /// invalid code sets `bad_code` and decodes as 0.
    #[inline]
    pub fn diff(&self, br: &mut Bits<'_>) -> i32 {
        self.diff_with(&self.lut, br)
    }

    /// [`Self::diff`] with the lookup table passed in, so a decoding loop
    /// keeps the table's address in a register.
    #[inline]
    fn diff_with(&self, lut: &[u32; LUT_SIZE], br: &mut Bits<'_>) -> i32 {
        let e = lut[(br.st.buf >> (64 - LUT_BITS)) as usize];
        if e & FULL != 0 {
            br.consume(e & 0x1F);
            return i32::from((e >> 16) as u16 as i16);
        }
        // By value, so the reader never has to leave registers on the hot path.
        let (v, r) = self.partial(*br, e);
        *br = r;
        v
    }

    /// Codes whose difference bits don't fit the table, and long codes.
    #[inline(never)]
    fn partial<'s>(&self, mut br: Bits<'s>, e: u32) -> (i32, Bits<'s>) {
        if e != 0 && e & SLOW == 0 {
            br.consume(e & 0x1F);
            let ssss = (e >> 5) & 0x1F;
            return (extend(br.bits(ssss), ssss), br);
        }
        for len in LUT_BITS + 1..=16 {
            let code = br.peek(len) as i32;
            if code <= self.maxcode[len as usize] {
                br.consume(len);
                let idx = self.valptr[len as usize] + code - self.mincode[len as usize];
                let v = match u32::from(self.values[idx as usize]) {
                    0 => 0,
                    16 => -32768,
                    ssss => extend(br.bits(ssss), ssss),
                };
                return (v, br);
            }
        }
        br.st.bad_code = true;
        (0, br)
    }
}

/// `SOF3` frame geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame {
    /// Samples per line, per component (`X`).
    pub width: usize,
    /// Lines (`Y`).
    pub height: usize,
    /// Interleaved components (`Nf`).
    pub ncomp: usize,
    /// Sample precision in bits (`P`).
    pub precision: u8,
}

impl Frame {
    /// Samples in one decoded line: `width × ncomp`.
    pub fn row_len(&self) -> usize {
        self.width * self.ncomp
    }

    pub fn samples(&self) -> usize {
        self.row_len() * self.height
    }
}

/// A parsed lossless JPEG stream, ready to decode.
#[derive(Debug, Clone)]
pub(crate) struct Ljpeg<'a> {
    pub frame: Frame,
    /// Huffman table of each component, in scan (= frame) order.
    tables: Vec<Huffman>,
    predictor: u8,
    point_transform: u8,
    /// Restart interval in lines (0 = none).
    restart_lines: usize,
    scan: &'a [u8],
}

fn be16(d: &[u8], at: usize) -> Result<usize, RawError> {
    d.get(at..at + 2)
        .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
        .ok_or_else(|| RawError::Truncated("lossless JPEG header".into()))
}

/// After the last line: no invalid code, and no read far past the data.
fn finish(br: &JpegBits<'_>) -> Result<(), RawError> {
    if br.st.bad_code {
        return Err(bad("invalid Huffman code"));
    }
    if br.overrun() > MAX_OVERRUN_BITS {
        return Err(RawError::Truncated("lossless JPEG data ends early".into()));
    }
    Ok(())
}

/// `SOF3`: the frame and its component ids (all 1×1 sampled).
fn parse_sof(seg: &[u8]) -> Result<(Frame, Vec<u8>), RawError> {
    let (Some(&p), Ok(y), Ok(x), Some(&nf)) = (seg.first(), be16(seg, 1), be16(seg, 3), seg.get(5))
    else {
        return Err(bad("short SOF3"));
    };
    let nf = usize::from(nf);
    if !(2..=16).contains(&p) || x == 0 || y == 0 || !(1..=4).contains(&nf) {
        return Err(bad(format!("unsupported frame {x}x{y}x{nf} P={p}")));
    }
    let comps = seg
        .get(6..6 + 3 * nf)
        .ok_or_else(|| bad("short SOF3 component list"))?;
    let mut ids = Vec::with_capacity(nf);
    for c in comps.as_chunks::<3>().0 {
        if c[1] != 0x11 {
            return Err(bad("subsampled components are not supported"));
        }
        ids.push(c[0]);
    }
    let frame = Frame {
        width: x,
        height: y,
        ncomp: nf,
        precision: p,
    };
    Ok((frame, ids))
}

/// `DHT`: one or more DC tables (ids 0..=3).
fn parse_dht(seg: &[u8], dht: &mut [Option<Huffman>; 4]) -> Result<(), RawError> {
    let mut q = 0;
    while q < seg.len() {
        let tc = seg[q];
        let counts: &[u8; 16] = seg
            .get(q + 1..q + 17)
            .and_then(|c| c.try_into().ok())
            .ok_or_else(|| bad("short DHT"))?;
        let n: usize = counts.iter().map(|&c| usize::from(c)).sum();
        let vals = seg
            .get(q + 17..q + 17 + n)
            .ok_or_else(|| bad("short DHT"))?;
        if tc >> 4 != 0 || tc & 0x0F > 3 {
            return Err(bad(format!("Huffman table class/id {tc:#x}")));
        }
        dht[usize::from(tc & 3)] = Some(Huffman::new(counts, vals)?);
        q += 17 + n;
    }
    Ok(())
}

/// What a `SOS` header selects.
struct Scan {
    tables: Vec<Huffman>,
    predictor: u8,
    point_transform: u8,
}

fn parse_sos(
    seg: &[u8],
    f: &Frame,
    ids: &[u8],
    dht: &[Option<Huffman>; 4],
) -> Result<Scan, RawError> {
    let ns = usize::from(*seg.first().ok_or_else(|| bad("short SOS"))?);
    let sel = seg.get(1..1 + 2 * ns).ok_or_else(|| bad("short SOS"))?;
    let tail = seg
        .get(1 + 2 * ns..4 + 2 * ns)
        .ok_or_else(|| bad("short SOS"))?;
    if ns != f.ncomp {
        return Err(bad("scans with a subset of components are not supported"));
    }
    let mut tables = Vec::with_capacity(ns);
    for (k, s) in sel.as_chunks::<2>().0.iter().enumerate() {
        if ids.get(k) != Some(&s[0]) {
            return Err(bad("scan components out of frame order"));
        }
        let t = dht[usize::from(s[1] >> 4) & 3]
            .clone()
            .ok_or_else(|| bad("scan uses an undefined Huffman table"))?;
        tables.push(t);
    }
    let (predictor, point_transform) = (tail[0], tail[2] & 0x0F);
    if !(1..=7).contains(&predictor) || point_transform >= f.precision {
        return Err(bad(format!(
            "predictor {predictor}, point transform {point_transform}"
        )));
    }
    Ok(Scan {
        tables,
        predictor,
        point_transform,
    })
}

impl<'a> Ljpeg<'a> {
    /// Parse the markers from `SOI` up to the start of the scan data.
    pub fn parse(data: &'a [u8]) -> Result<Self, RawError> {
        if data.get(..2) != Some(&[0xFF, 0xD8]) {
            return Err(bad("no SOI marker"));
        }
        let mut pos = 2;
        let mut frame: Option<(Frame, Vec<u8>)> = None;
        let mut dht: [Option<Huffman>; 4] = [None, None, None, None];
        let mut restart = 0usize;
        loop {
            // Markers may be preceded by any number of 0xFF fill bytes.
            while data.get(pos) == Some(&0xFF) && data.get(pos + 1) == Some(&0xFF) {
                pos += 1;
            }
            let (Some(&0xFF), Some(&m)) = (data.get(pos), data.get(pos + 1)) else {
                return Err(bad("expected a marker"));
            };
            let len = be16(data, pos + 2)?;
            let seg = data
                .get(pos + 4..pos + 2 + len)
                .filter(|_| len >= 2)
                .ok_or_else(|| RawError::Truncated("lossless JPEG segment".into()))?;
            match m {
                0xC3 => frame = Some(parse_sof(seg)?),
                0xC0..=0xCF if m != 0xC4 && m != 0xC8 && m != 0xCC => {
                    return Err(bad(format!("SOF{} is not lossless Huffman", m - 0xC0)));
                }
                0xC4 => parse_dht(seg, &mut dht)?,
                0xDD => restart = be16(seg, 0)?,
                0xDA => {
                    let (f, ids) = frame.ok_or_else(|| bad("SOS before SOF3"))?;
                    let scan = parse_sos(seg, &f, &ids, &dht)?;
                    let restart_lines = if restart == 0 {
                        0
                    } else if restart.is_multiple_of(f.width) {
                        restart / f.width
                    } else {
                        return Err(bad(format!(
                            "restart interval {restart} is not whole lines of {}",
                            f.width
                        )));
                    };
                    return Ok(Self {
                        frame: f,
                        tables: scan.tables,
                        predictor: scan.predictor,
                        point_transform: scan.point_transform,
                        restart_lines,
                        scan: &data[pos + 2 + len..],
                    });
                }
                0xD8 | 0xD9 => return Err(bad("no scan")),
                _ => {} // APPn, COM, DQT, … carry nothing we need
            }
            pos += 2 + len;
        }
    }

    /// Decode the whole frame into `out` (exactly [`Frame::samples`] long,
    /// row-major, components interleaved).
    pub fn decode_into(&self, out: &mut [u16]) -> Result<(), RawError> {
        let row = self.frame.row_len();
        if out.len() != self.frame.samples() {
            return Err(bad("output size does not match the frame"));
        }
        let mut br = JpegBits::new(self.scan);
        for y in 0..self.frame.height {
            let (done, rest) = out.split_at_mut(y * row);
            let first = self.start_line(&mut br, y)?;
            let prev = if first {
                None
            } else {
                Some(&done[(y - 1) * row..])
            };
            self.line(&mut br, prev, &mut rest[..row])?;
        }
        finish(&br)
    }

    /// Decode line by line into a two-line ring, handing each finished line
    /// (`width × ncomp` samples) and its index to `sink`.
    pub fn decode_rows(
        &self,
        mut sink: impl FnMut(usize, &[u16]) -> Result<(), RawError>,
    ) -> Result<(), RawError> {
        let row = self.frame.row_len();
        let mut ring = vec![0u16; 2 * row];
        let mut br = JpegBits::new(self.scan);
        for y in 0..self.frame.height {
            let (a, b) = ring.split_at_mut(row);
            let (prev, cur) = if y % 2 == 0 { (&*b, a) } else { (&*a, b) };
            let first = self.start_line(&mut br, y)?;
            self.line(&mut br, if first { None } else { Some(prev) }, cur)?;
            sink(y, cur)?;
        }
        finish(&br)
    }

    /// Handle a restart before line `y`; true when the line starts an interval.
    fn start_line(&self, br: &mut JpegBits<'_>, y: usize) -> Result<bool, RawError> {
        if y == 0 {
            return Ok(true);
        }
        if self.restart_lines > 0 && y.is_multiple_of(self.restart_lines) {
            br.restart(((y / self.restart_lines - 1) & 7) as u8)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Decode one line. `prev` is the line above, `None` on the first line
    /// of the image or of a restart interval (1-D prediction, T.81 H.1.2.1).
    fn line(
        &self,
        br: &mut JpegBits<'_>,
        prev: Option<&[u16]>,
        cur: &mut [u16],
    ) -> Result<(), RawError> {
        let mut r = br.reader();
        let left = prev.is_none_or(|_| self.predictor == 1);
        match (left, self.frame.ncomp, prev) {
            (true, 1, _) => self.line_left::<1>(&mut r, prev, cur),
            (true, 2, _) => self.line_left::<2>(&mut r, prev, cur),
            (true, 3, _) => self.line_left::<3>(&mut r, prev, cur),
            (true, _, _) => self.line_left::<4>(&mut r, prev, cur),
            (false, _, Some(p)) => self.line_2d(&mut r, p, cur),
            (false, _, None) => unreachable!("2-D prediction needs a line above"),
        }
        let st = r.st;
        br.store(st);
        let bad_code = st.bad_code;
        if bad_code {
            return Err(bad("invalid Huffman code"));
        }
        Ok(())
    }

    /// Predictor 1 (left), or the first line of an interval: the first
    /// column predicts from the line above (or `2^(P-Pt-1)` without one).
    #[inline]
    fn line_left<const N: usize>(&self, br: &mut Bits<'_>, prev: Option<&[u16]>, cur: &mut [u16]) {
        let pt = u32::from(self.point_transform);
        let Ok(tables) = <&[Huffman; N]>::try_from(&self.tables[..N]) else {
            unreachable!("one table per component (checked at parse)");
        };
        let mut left = [0i32; N];
        for c in 0..N {
            left[c] = match prev {
                Some(p) => i32::from(p[c] >> pt),
                None => 1 << (u32::from(self.frame.precision) - pt - 1),
            };
        }
        let luts: [&[u32; LUT_SIZE]; N] = std::array::from_fn(|c| &*tables[c].lut);
        for mcu in cur.as_chunks_mut::<N>().0 {
            for c in 0..N {
                br.need32();
                let v = left[c].wrapping_add(tables[c].diff_with(luts[c], br)) & 0xFFFF;
                left[c] = v;
                mcu[c] = v.wrapping_shl(pt) as u16;
            }
        }
    }

    /// Predictors 2–7 (T.81 Table H.1) on a line with one above it.
    fn line_2d(&self, br: &mut Bits<'_>, prev: &[u16], cur: &mut [u16]) {
        let n = self.frame.ncomp;
        let pt = u32::from(self.point_transform);
        let sel = self.predictor;
        for c in 0..n {
            br.need32();
            let v = (i32::from(prev[c] >> pt) + self.tables[c].diff(br)) & 0xFFFF;
            cur[c] = (v << pt) as u16;
        }
        for i in n..cur.len() {
            let c = i % n;
            let ra = i32::from(cur[i - n] >> pt);
            let rb = i32::from(prev[i] >> pt);
            let rc = i32::from(prev[i - n] >> pt);
            let px = match sel {
                1 => ra,
                2 => rb,
                3 => rc,
                4 => ra + rb - rc,
                5 => ra + ((rb - rc) >> 1),
                6 => rb + ((ra - rc) >> 1),
                _ => (ra + rb) >> 1,
            };
            br.need32();
            let v = (px + self.tables[c].diff(br)) & 0xFFFF;
            cur[i] = (v << pt) as u16;
        }
    }
}

#[cfg(test)]
pub(crate) mod enc {
    //! A small lossless JPEG encoder for tests (any predictor, components,
    //! restart interval and Huffman table).

    /// Code lengths per symbol (difference size 0..=16) in a valid table that
    /// includes codes longer than `LUT_BITS` (so the slow path is tested).
    pub const LONG_TABLE: ([u8; 16], [u8; 17]) = (
        [0, 1, 4, 2, 3, 1, 1, 1, 1, 1, 0, 0, 0, 0, 1, 1],
        [6, 4, 8, 5, 7, 3, 9, 0, 10, 2, 1, 12, 11, 13, 14, 15, 16],
    );

    #[derive(Debug, Clone)]
    pub struct Opts {
        pub precision: u8,
        pub predictor: u8,
        pub point_transform: u8,
        /// Restart interval in MCUs (0 = none).
        pub restart: u16,
        pub table: ([u8; 16], [u8; 17]),
        /// Separate (identical) tables per component, numbered 0..n.
        pub table_per_comp: bool,
    }

    impl Default for Opts {
        fn default() -> Self {
            Self {
                precision: 14,
                predictor: 1,
                point_transform: 0,
                restart: 0,
                table: LONG_TABLE,
                table_per_comp: true,
            }
        }
    }

    struct Bits {
        out: Vec<u8>,
        acc: u64,
        n: u32,
    }

    impl Bits {
        fn put(&mut self, v: u32, len: u32) {
            if len == 0 {
                return;
            }
            self.acc = (self.acc << len) | u64::from(v & ((1u32 << len) - 1));
            self.n += len;
            while self.n >= 8 {
                let b = (self.acc >> (self.n - 8)) as u8;
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0);
                }
                self.n -= 8;
            }
        }
        fn flush(&mut self) {
            if self.n > 0 {
                let pad = 8 - self.n;
                self.put((1 << pad) - 1, pad);
            }
        }
    }

    fn codes(t: &([u8; 16], [u8; 17])) -> [(u32, u32); 17] {
        let mut out = [(0u32, 0u32); 17];
        let (mut code, mut k) = (0u32, 0usize);
        for len in 1..=16u32 {
            for _ in 0..t.0[len as usize - 1] {
                out[usize::from(t.1[k])] = (code, len);
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        out
    }

    fn predict(sel: u8, ra: i32, rb: i32, rc: i32) -> i32 {
        match sel {
            1 => ra,
            2 => rb,
            3 => rc,
            4 => ra + rb - rc,
            5 => ra + ((rb - rc) >> 1),
            6 => rb + ((ra - rc) >> 1),
            _ => (ra + rb) >> 1,
        }
    }

    /// Encode `w × h` MCUs of `ncomp` interleaved samples (`data` row-major).
    #[allow(clippy::too_many_lines)]
    pub fn encode(data: &[u16], w: usize, h: usize, ncomp: usize, o: &Opts) -> Vec<u8> {
        assert_eq!(data.len(), w * h * ncomp);
        let row = w * ncomp;
        let pt = u32::from(o.point_transform);
        let mut f = vec![0xFF, 0xD8];
        // DHT: one table per component (or one shared).
        let ntab = if o.table_per_comp { ncomp } else { 1 };
        for t in 0..ntab {
            let n: usize = o.table.0.iter().map(|&c| usize::from(c)).sum();
            f.extend_from_slice(&[0xFF, 0xC4]);
            f.extend_from_slice(&((2 + 17 + n) as u16).to_be_bytes());
            f.push(t as u8);
            f.extend_from_slice(&o.table.0);
            f.extend_from_slice(&o.table.1[..n]);
        }
        f.extend_from_slice(&[0xFF, 0xC3]);
        f.extend_from_slice(&((8 + 3 * ncomp) as u16).to_be_bytes());
        f.push(o.precision);
        f.extend_from_slice(&(h as u16).to_be_bytes());
        f.extend_from_slice(&(w as u16).to_be_bytes());
        f.push(ncomp as u8);
        for c in 0..ncomp {
            f.extend_from_slice(&[c as u8 + 1, 0x11, 0]);
        }
        if o.restart > 0 {
            f.extend_from_slice(&[0xFF, 0xDD, 0, 4]);
            f.extend_from_slice(&o.restart.to_be_bytes());
        }
        f.extend_from_slice(&[0xFF, 0xDA]);
        f.extend_from_slice(&((6 + 2 * ncomp) as u16).to_be_bytes());
        f.push(ncomp as u8);
        for c in 0..ncomp {
            let t = if o.table_per_comp { c } else { 0 };
            f.extend_from_slice(&[c as u8 + 1, (t as u8) << 4]);
        }
        f.extend_from_slice(&[o.predictor, 0, o.point_transform]);
        let code = codes(&o.table);
        let mut bits = Bits {
            out: Vec::new(),
            acc: 0,
            n: 0,
        };
        let restart_lines = if o.restart == 0 {
            0
        } else {
            usize::from(o.restart) / w
        };
        let mut rst = 0u8;
        let v = |i: usize| i32::from(data[i] >> pt);
        for y in 0..h {
            let first = y == 0 || (restart_lines > 0 && y % restart_lines == 0);
            if y > 0 && first {
                bits.flush();
                bits.out.extend_from_slice(&[0xFF, 0xD0 + (rst & 7)]);
                rst = rst.wrapping_add(1);
            }
            for i in 0..row {
                let x = i / ncomp;
                let pred = if first {
                    if x == 0 {
                        1 << (u32::from(o.precision) - pt - 1)
                    } else {
                        v(y * row + i - ncomp)
                    }
                } else if x == 0 {
                    v((y - 1) * row + i)
                } else {
                    predict(
                        o.predictor,
                        v(y * row + i - ncomp),
                        v((y - 1) * row + i),
                        v((y - 1) * row + i - ncomp),
                    )
                };
                let diff = (v(y * row + i) - pred).rem_euclid(65536);
                let diff = if diff >= 32768 { diff - 65536 } else { diff };
                let (ssss, extra) = if diff == -32768 {
                    (16, 0)
                } else {
                    let s = 32 - diff.unsigned_abs().leading_zeros();
                    let e = if diff < 0 { diff - 1 } else { diff };
                    (s, (e as u32) & ((1u32 << s) - 1))
                };
                let (cd, len) = code[ssss as usize];
                assert!(len > 0, "symbol {ssss} missing from the table");
                bits.put(cd, len);
                if ssss != 16 {
                    bits.put(extra, ssss);
                }
            }
        }
        bits.flush();
        f.extend(bits.out);
        f.extend_from_slice(&[0xFF, 0xD9]);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::enc::{LONG_TABLE, Opts, encode};
    use super::*;

    /// Deterministic noise with occasional big jumps (every difference size).
    fn samples(n: usize, bits: u8, seed: u32) -> Vec<u16> {
        let mut s = seed.wrapping_mul(2_654_435_761) | 1;
        let max = (1u32 << bits) - 1;
        let mut v = max / 2;
        (0..n)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                v = if i % 97 == 0 {
                    s & max
                } else {
                    (v as i32 + (s % 64) as i32 - 32).clamp(0, max as i32) as u32
                };
                v as u16
            })
            .collect()
    }

    fn round_trip(w: usize, h: usize, ncomp: usize, o: &Opts) {
        let data = samples(w * h * ncomp, o.precision, (w * 31 + h + ncomp) as u32);
        let data: Vec<u16> = data
            .iter()
            .map(|&v| (v >> o.point_transform) << o.point_transform)
            .collect();
        let jpg = encode(&data, w, h, ncomp, o);
        let lj = Ljpeg::parse(&jpg).unwrap();
        assert_eq!(
            lj.frame,
            Frame {
                width: w,
                height: h,
                ncomp,
                precision: o.precision
            }
        );
        let mut out = vec![0u16; lj.frame.samples()];
        lj.decode_into(&mut out).unwrap();
        assert_eq!(out, data, "{o:?} {w}x{h}x{ncomp}");
        let mut rows = vec![0u16; data.len()];
        lj.decode_rows(|y, r| {
            rows[y * r.len()..(y + 1) * r.len()].copy_from_slice(r);
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, data);
    }

    #[test]
    fn every_predictor_and_component_count_round_trips() {
        for predictor in 1..=7 {
            for ncomp in 1..=4 {
                round_trip(
                    19,
                    7,
                    ncomp,
                    &Opts {
                        predictor,
                        ..Opts::default()
                    },
                );
            }
        }
    }

    #[test]
    fn precisions_point_transform_and_shared_tables() {
        for precision in [8, 12, 16] {
            round_trip(
                33,
                5,
                2,
                &Opts {
                    precision,
                    table_per_comp: false,
                    ..Opts::default()
                },
            );
        }
        round_trip(
            17,
            6,
            2,
            &Opts {
                point_transform: 2,
                predictor: 6,
                ..Opts::default()
            },
        );
    }

    #[test]
    fn restart_intervals() {
        for predictor in [1, 4] {
            round_trip(
                16,
                9,
                2,
                &Opts {
                    restart: 32, // two lines
                    predictor,
                    ..Opts::default()
                },
            );
        }
        // Not whole lines: refused.
        let data = vec![100u16; 16 * 4];
        let jpg = encode(
            &data,
            16,
            4,
            1,
            &Opts {
                restart: 0,
                ..Opts::default()
            },
        );
        let mut bad_dri = jpg[..2].to_vec();
        bad_dri.extend_from_slice(&[0xFF, 0xDD, 0, 4, 0, 5]);
        bad_dri.extend_from_slice(&jpg[2..]);
        assert!(Ljpeg::parse(&bad_dri).is_err());
        // A missing RST marker is an error.
        let jpg = encode(
            &data,
            16,
            4,
            1,
            &Opts {
                restart: 16,
                ..Opts::default()
            },
        );
        let at = jpg.windows(2).rposition(|w| w == [0xFF, 0xD2]).unwrap();
        let mut broken = jpg.clone();
        broken[at + 1] = 0xD5;
        let lj = Ljpeg::parse(&broken).unwrap();
        assert!(lj.decode_into(&mut [0; 64]).is_err());
    }

    #[test]
    fn extremes_use_size_16_and_slow_codes() {
        // Alternating 0 / 65535 needs difference size 16 (and -32768 wrap).
        let data: Vec<u16> = (0..64)
            .map(|i| if i % 2 == 0 { 0 } else { 65535 })
            .collect();
        let o = Opts {
            precision: 16,
            ..Opts::default()
        };
        let jpg = encode(&data, 32, 2, 1, &o);
        let lj = Ljpeg::parse(&jpg).unwrap();
        let mut out = vec![0; 64];
        lj.decode_into(&mut out).unwrap();
        assert_eq!(out, data);
        // Exactly 32768 apart: the -32768 / +32768 congruence.
        let data: Vec<u16> = (0..16)
            .map(|i| if i % 2 == 0 { 100 } else { 32868 })
            .collect();
        let jpg = encode(&data, 16, 1, 1, &o);
        let mut out = vec![0; 16];
        Ljpeg::parse(&jpg).unwrap().decode_into(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn huffman_tables_are_validated() {
        let (c, v) = LONG_TABLE;
        assert!(Huffman::new(&c, &v[..16]).is_err()); // count mismatch
        assert!(Huffman::new(&[0; 16], &[]).is_err());
        let mut bigsym = v;
        bigsym[0] = 17;
        assert!(Huffman::new(&c, &bigsym).is_err());
        // Three 1-bit codes overflow the code space.
        let mut over = [0u8; 16];
        over[0] = 3;
        assert!(Huffman::new(&over, &[0, 1, 2]).is_err());
        // A code nothing was assigned to (all ones) is an invalid code.
        let mut one = [0u8; 16];
        one[0] = 1;
        let t = Huffman::new(&one, &[0]).unwrap();
        let data = [0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00];
        let jb = JpegBits::new(&data);
        let mut br = jb.reader();
        br.refill();
        assert_eq!(t.diff(&mut br), 0);
        assert!(br.st.bad_code);
    }

    #[test]
    fn bit_reader_unstuffs_and_pads_at_markers() {
        let data = [0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD9, 0x56];
        let mut jb = JpegBits::new(&data);
        let mut br = jb.reader();
        br.refill();
        assert_eq!(br.bits(8), 0x12);
        assert_eq!(br.bits(8), 0xFF);
        assert_eq!(br.bits(8), 0x34);
        assert_eq!(br.bits(16), 0); // the marker stops the data
        let st = br.st;
        jb.store(st);
        assert!(jb.overrun() > 0);
        // Fast refill (no 0xFF in the next 8 bytes).
        let data: Vec<u8> = (1..=16).collect();
        let mut jb = JpegBits::new(&data);
        let mut br = jb.reader();
        br.refill();
        assert_eq!(br.bits(16), 0x0102);
        br.need32();
        assert_eq!(br.bits(16), 0x0304);
        let st = br.st;
        jb.store(st);
        assert_eq!(jb.overrun(), 0);
    }

    #[test]
    fn malformed_headers_are_errors() {
        let data = samples(8 * 4, 12, 3);
        let good = encode(
            &data,
            8,
            4,
            1,
            &Opts {
                precision: 12,
                ..Opts::default()
            },
        );
        assert!(Ljpeg::parse(&good).is_ok());
        assert!(Ljpeg::parse(b"").is_err());
        assert!(Ljpeg::parse(&[0xFF, 0xD8, 0xFF, 0xD9]).is_err());
        assert!(Ljpeg::parse(&[0xFF, 0xD8, 0x00]).is_err());
        assert!(Ljpeg::parse(&good[..good.len().min(40)]).is_err());
        let sof = good.windows(2).position(|w| w == [0xFF, 0xC3]).unwrap();
        let sos = good.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        let patch = |at: usize, v: u8| {
            let mut f = good.clone();
            f[at] = v;
            Ljpeg::parse(&f).err()
        };
        assert!(patch(sof + 1, 0xC0).is_some()); // baseline, not lossless
        assert!(patch(sof + 4, 1).is_some()); // precision 1
        assert!(patch(sof + 9, 5).is_some()); // 5 components
        assert!(patch(sof + 11, 0x21).is_some()); // subsampled
        assert!(patch(sos + 6, 0x10).is_some()); // undefined table 1
        assert!(patch(sos + 7, 0).is_some()); // predictor 0
        assert!(patch(sos + 7, 8).is_some()); // predictor 8
        assert!(patch(sos + 9, 12).is_some()); // point transform >= P
        assert!(patch(sos + 5, 9).is_some()); // unknown component id
        assert!(patch(sos + 7, 1).is_none()); // predictor 1 still fine
        assert!(patch(sos + 4, 2).is_some()); // a scan with fewer components
        // The DHT before SOF: class 1 (AC) and id 4 are refused.
        assert!(patch(6, 0x10).is_some());
        assert!(patch(6, 0x04).is_some());
        // SOS before SOF3.
        let mut f = good[..2].to_vec();
        f.extend_from_slice(&good[sos..]);
        assert!(Ljpeg::parse(&f).is_err());
        // Wrong output size.
        let lj = Ljpeg::parse(&good).unwrap();
        assert!(lj.decode_into(&mut [0; 3]).is_err());
    }

    #[test]
    fn truncated_scan_is_an_error_not_garbage() {
        let data = samples(64 * 32, 14, 9);
        let jpg = encode(&data, 64, 32, 1, &Opts::default());
        let cut = &jpg[..jpg.len() / 2];
        let lj = Ljpeg::parse(cut).unwrap();
        assert!(matches!(
            lj.decode_into(&mut vec![0; 64 * 32]),
            Err(RawError::Truncated(_))
        ));
    }
}
