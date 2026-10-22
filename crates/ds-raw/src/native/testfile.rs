//! A TIFF file builder for the native decoders' tests: IFDs linked as a
//! chain, `SubIFDs` or the Exif IFD, each with optional strips or tiles,
//! laid out automatically (IFDs first, then the data blocks).

use crate::tiff::Endian;
use crate::tiff::build::{Val, header, ifd};
use crate::tiff::tag;

/// How an IFD hangs off another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Next in IFD0's chain (the first node is IFD0 itself).
    Chain,
    /// Listed in `SubIFDs` of node `n`.
    Sub(usize),
    /// The Exif IFD of node `n`.
    Exif(usize),
    /// A maker note IFD in node `n`'s `MakerNote` (offsets absolute).
    MakerNote(usize),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub link: Link,
    pub entries: Vec<(u16, Val)>,
    /// Strips (or tiles) written after the IFDs; their offset and byte-count
    /// tags are added.
    pub blocks: Vec<Vec<u8>>,
    pub tiled: bool,
}

impl Node {
    pub fn new(link: Link, entries: Vec<(u16, Val)>) -> Self {
        Self {
            link,
            entries,
            blocks: Vec::new(),
            tiled: false,
        }
    }

    pub fn with_blocks(mut self, blocks: Vec<Vec<u8>>, tiled: bool) -> Self {
        self.blocks = blocks;
        self.tiled = tiled;
        self
    }
}

/// Serialize `nodes` (node 0 is IFD0) into a TIFF file.
pub fn build(e: Endian, nodes: &[Node]) -> Vec<u8> {
    // Pass 1: sizes with placeholder offsets; pass 2: real offsets.
    let entries_at = |pos: &[u32], data: &[Vec<u32>]| -> Vec<Vec<(u16, Val)>> {
        nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let mut v = n.entries.clone();
                if !n.blocks.is_empty() {
                    let (ot, lt) = if n.tiled {
                        (tag::TILE_OFFSETS, tag::TILE_BYTE_COUNTS)
                    } else {
                        (tag::STRIP_OFFSETS, tag::STRIP_BYTE_COUNTS)
                    };
                    v.push((ot, Val::Long(data[i].clone())));
                    v.push((
                        lt,
                        Val::Long(n.blocks.iter().map(|b| b.len() as u32).collect()),
                    ));
                }
                let subs: Vec<u32> = nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.link == Link::Sub(i))
                    .map(|(j, _)| pos[j])
                    .collect();
                if !subs.is_empty() {
                    v.push((tag::SUB_IFDS, Val::Long(subs)));
                }
                if let Some((j, _)) = nodes
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.link == Link::Exif(i))
                {
                    v.push((tag::EXIF_IFD, Val::Long(vec![pos[j]])));
                }
                if let Some((j, _)) = nodes
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.link == Link::MakerNote(i))
                {
                    let bytes = match e {
                        Endian::Little => pos[j].to_le_bytes(),
                        Endian::Big => pos[j].to_be_bytes(),
                    };
                    // UNDEFINED, sized past 4 bytes so the field is an offset.
                    v.push((
                        tag::MAKER_NOTE,
                        Val::Raw {
                            typ: 7,
                            count: 64,
                            bytes: bytes.to_vec(),
                        },
                    ));
                }
                v
            })
            .collect()
    };
    let next_of = |i: usize, pos: &[u32]| -> u32 {
        if i != 0 && nodes[i].link != Link::Chain {
            return 0;
        }
        nodes
            .iter()
            .enumerate()
            .skip(i + 1)
            .find(|(_, c)| c.link == Link::Chain)
            .map_or(0, |(j, _)| pos[j])
    };
    let zero_pos = vec![0u32; nodes.len()];
    let zero_data: Vec<Vec<u32>> = nodes.iter().map(|n| vec![0; n.blocks.len()]).collect();
    let sized = entries_at(&zero_pos, &zero_data);
    let mut pos = Vec::new();
    let mut at = 8u32;
    for v in &sized {
        pos.push(at);
        at += ifd(e, at, v, 0).len() as u32;
        at = (at + 3) & !3;
    }
    let mut data = Vec::new();
    for n in nodes {
        let mut offs = Vec::new();
        for b in &n.blocks {
            offs.push(at);
            at += b.len() as u32;
            at = (at + 1) & !1;
        }
        data.push(offs);
    }
    let real = entries_at(&pos, &data);
    let mut f = header(e);
    for (i, v) in real.iter().enumerate() {
        f.resize(pos[i] as usize, 0);
        f.extend(ifd(e, pos[i], v, next_of(i, &pos)));
    }
    for (n, offs) in nodes.iter().zip(&data) {
        for (b, &o) in n.blocks.iter().zip(offs) {
            f.resize(o as usize, 0);
            f.extend_from_slice(b);
        }
    }
    f
}

/// A position-unique test sample below `max`.
pub fn sample(x: usize, y: usize, c: usize, max: u32) -> u16 {
    ((x as u32 * 73 + y as u32 * 151 + c as u32 * 997 + (x * y) as u32 % 61) % max) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::Container;
    use crate::source::SliceSource;

    #[test]
    fn builder_links_chain_subs_and_exif() {
        let f = build(
            Endian::Big,
            &[
                Node::new(Link::Chain, vec![(tag::MAKE, Val::Ascii("X"))]),
                Node::new(Link::Sub(0), vec![(tag::IMAGE_WIDTH, Val::Long(vec![4]))])
                    .with_blocks(vec![vec![1, 2, 3], vec![4]], false),
                Node::new(Link::Exif(0), vec![(tag::ISO, Val::Short(vec![100]))]),
                Node::new(Link::Chain, vec![(tag::IMAGE_WIDTH, Val::Long(vec![2]))]),
            ],
        );
        let src = SliceSource::new(&f);
        let c = Container::parse(&src).unwrap();
        assert_eq!(c.tiff.chain.len(), 2);
        assert_eq!(c.subs.len(), 1);
        assert_eq!(
            c.exif.as_ref().unwrap().uint(&src, tag::ISO).unwrap(),
            Some(100)
        );
        let offs = c.subs[0].uints(&src, tag::STRIP_OFFSETS).unwrap().unwrap();
        assert_eq!(f[offs[0] as usize..offs[0] as usize + 3], [1, 2, 3]);
        assert_eq!(f[offs[1] as usize], 4);
        assert!(sample(3, 4, 1, 100) < 100);
    }
}
