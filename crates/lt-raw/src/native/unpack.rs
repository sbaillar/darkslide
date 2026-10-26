//! Uncompressed sample layouts: 8-bit, 16-bit words in either byte order,
//! and MSB-first bit packing with each row starting on a byte boundary
//! (TIFF 6.0 `FillOrder` 1, as DNG and most RAWs store 10/12/14-bit data).

use crate::RawError;
use crate::tiff::Endian;

/// How a row of uncompressed samples is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    U8,
    /// 16-bit words in the given byte order.
    U16(Endian),
    /// `n` bits per sample (1..=16), MSB first, rows byte-aligned.
    Msb(u32),
}

impl Layout {
    /// The layout TIFF/DNG uses for `bits` per sample in a file of `endian`.
    pub fn tiff(bits: u32, endian: Endian) -> Result<Self, RawError> {
        match bits {
            8 => Ok(Self::U8),
            16 => Ok(Self::U16(endian)),
            1..=15 => Ok(Self::Msb(bits)),
            _ => Err(RawError::Malformed(format!(
                "{bits} bits per sample is not supported"
            ))),
        }
    }

    /// Bytes holding a row of `samples`.
    pub fn row_bytes(self, samples: usize) -> usize {
        match self {
            Self::U8 => samples,
            Self::U16(_) => samples * 2,
            Self::Msb(n) => (samples * n as usize).div_ceil(8),
        }
    }

    /// Unpack one row: `src` holds at least [`Layout::row_bytes`] of `dst.len()`.
    pub fn unpack(self, src: &[u8], dst: &mut [u16]) {
        match self {
            Self::U8 => {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = u16::from(s);
                }
            }
            Self::U16(Endian::Little) => {
                for (d, s) in dst.iter_mut().zip(src.as_chunks::<2>().0) {
                    *d = u16::from_le_bytes(*s);
                }
            }
            Self::U16(Endian::Big) => {
                for (d, s) in dst.iter_mut().zip(src.as_chunks::<2>().0) {
                    *d = u16::from_be_bytes(*s);
                }
            }
            Self::Msb(12) => unpack12(src, dst),
            Self::Msb(14) => unpack14(src, dst),
            Self::Msb(n) => unpack_msb(src, dst, n),
        }
    }
}

/// Two samples per three bytes.
fn unpack12(src: &[u8], dst: &mut [u16]) {
    let pairs = dst.len() / 2;
    for (d, s) in dst
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<3>().0)
        .take(pairs)
    {
        let (a, b, c) = (u16::from(s[0]), u16::from(s[1]), u16::from(s[2]));
        d[0] = (a << 4) | (b >> 4);
        d[1] = ((b & 0x0F) << 8) | c;
    }
    if dst.len() % 2 == 1 {
        let tail = &mut dst[pairs * 2..];
        unpack_msb(src.get(pairs * 3..).unwrap_or_default(), tail, 12);
    }
}

/// Four samples per seven bytes.
fn unpack14(src: &[u8], dst: &mut [u16]) {
    let quads = dst.len() / 4;
    for (d, s) in dst
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<7>().0)
        .take(quads)
    {
        let mut b = [0u8; 8];
        b[..7].copy_from_slice(s);
        let v = u64::from_be_bytes(b) >> 8;
        d[0] = ((v >> 42) & 0x3FFF) as u16;
        d[1] = ((v >> 28) & 0x3FFF) as u16;
        d[2] = ((v >> 14) & 0x3FFF) as u16;
        d[3] = (v & 0x3FFF) as u16;
    }
    let rest = dst.len() % 4;
    if rest > 0 {
        let tail = &mut dst[quads * 4..];
        unpack_msb(src.get(quads * 7..).unwrap_or_default(), tail, 14);
    }
}

/// Any width 1..=16, MSB first.
fn unpack_msb(src: &[u8], dst: &mut [u16], n: u32) {
    let mut acc: u32 = 0;
    let mut have = 0u32;
    let mut bytes = src.iter();
    let mask = (1u32 << n) - 1;
    for d in dst {
        while have < n {
            acc = (acc << 8) | u32::from(*bytes.next().unwrap_or(&0));
            have += 8;
        }
        have -= n;
        *d = ((acc >> have) & mask) as u16;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// MSB-first packing of `samples` at `n` bits (the inverse of `unpack`).
    pub fn pack_msb(samples: &[u16], n: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut acc, mut have) = (0u64, 0u32);
        for &s in samples {
            acc = (acc << n) | u64::from(s);
            have += n;
            while have >= 8 {
                have -= 8;
                out.push((acc >> have) as u8);
            }
        }
        if have > 0 {
            out.push((acc << (8 - have)) as u8);
        }
        out
    }

    #[test]
    fn every_layout_round_trips() {
        for n in 1..=16u32 {
            for len in [1usize, 2, 3, 4, 5, 7, 8, 9, 31] {
                let v: Vec<u16> = (0..len as u32)
                    .map(|i| (i.wrapping_mul(2_654_435_761) >> 7) as u16 & ((1u32 << n) - 1) as u16)
                    .collect();
                let layout = Layout::tiff(n, Endian::Big).unwrap();
                let packed = match layout {
                    Layout::U8 => v.iter().map(|&x| x as u8).collect(),
                    Layout::U16(_) => v.iter().flat_map(|x| x.to_be_bytes()).collect(),
                    Layout::Msb(_) => pack_msb(&v, n),
                };
                assert_eq!(packed.len(), layout.row_bytes(len), "{n} bits x {len}");
                let mut out = vec![0u16; len];
                layout.unpack(&packed, &mut out);
                assert_eq!(out, v, "{n} bits x {len}");
            }
        }
        let le: Vec<u8> = [0x1234u16, 0xABCD]
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        let mut out = [0u16; 2];
        Layout::tiff(16, Endian::Little)
            .unwrap()
            .unpack(&le, &mut out);
        assert_eq!(out, [0x1234, 0xABCD]);
        assert!(Layout::tiff(0, Endian::Little).is_err());
        assert!(Layout::tiff(32, Endian::Little).is_err());
        // Short input reads as zeros rather than panicking.
        let mut out = [7u16; 3];
        Layout::Msb(10).unpack(&[0xFF], &mut out);
        assert_eq!(out, [0x3FC, 0, 0]);
    }
}
