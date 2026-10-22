//! Image and text embeddings for natural-language search (MobileCLIP): unit
//! normalization, compact f16 storage, and brute-force cosine ranking.
//!
//! Vectors are stored normalized, so cosine similarity is a dot product. A
//! query scores every stored row of an [`EmbeddingMatrix`] (contiguous `f32`,
//! eight independent accumulators per row so the loop vectorizes to NEON):
//! 20 000 × 512 is about 10 M multiply-adds, a few milliseconds on one core.

use half::f16;
use half::slice::{HalfBitsSliceExt, HalfFloatSliceExt};

/// Largest accepted embedding length (`MobileCLIP` uses 512).
pub const MAX_DIM: usize = 4096;

/// `v` scaled to unit length; `None` when it is empty, longer than [`MAX_DIM`],
/// not finite, or zero.
pub fn normalized(v: &[f32]) -> Option<Vec<f32>> {
    if v.is_empty() || v.len() > MAX_DIM || v.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let norm = dot(v, v).sqrt();
    if !norm.is_normal() {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

/// Little-endian IEEE half floats (2 bytes per component).
pub fn encode_f16(v: &[f32]) -> Vec<u8> {
    v.iter()
        .flat_map(|x| f16::from_f32(*x).to_le_bytes())
        .collect()
}

/// Inverse of [`encode_f16`]; `None` for an odd or empty byte count.
pub fn decode_f16(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = vec![0f32; bytes.len() / 2];
    decode_f16_into(bytes, &mut out);
    Some(out)
}

/// Decode little-endian halves into `out` (same count), with the hardware
/// conversion where the CPU has one (NEON `fcvtl` on Apple silicon).
fn decode_f16_into(bytes: &[u8], out: &mut [f32]) {
    let bits: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    bits.reinterpret_cast::<f16>().convert_to_f32_slice(out);
}

/// Dot product over the common length of `a` and `b`.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut acc = [0f32; 8];
    let (ca, ra) = a.as_chunks::<8>();
    let (cb, rb) = b.as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    acc.iter().sum::<f32>() + tail
}

/// Rows of equal-length vectors, stored contiguously.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EmbeddingMatrix {
    dim: usize,
    data: Vec<f32>,
}

impl EmbeddingMatrix {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            data: Vec::new(),
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.dim).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// A matrix of f16 rows ([`encode_f16`]) with the most common length (rows
    /// of another length, e.g. damaged or from another model, can't be compared
    /// and are left out), and the tags of the kept rows in row order. Decodes
    /// straight into the matrix (one small scratch buffer per row).
    pub fn from_f16_rows<T>(rows: Vec<(T, Vec<u8>)>) -> (Self, Vec<T>) {
        let mut counts = std::collections::HashMap::new();
        for (_, b) in &rows {
            if !b.is_empty() && b.len().is_multiple_of(2) {
                *counts.entry(b.len()).or_insert(0usize) += 1;
            }
        }
        let bytes = counts
            .into_iter()
            .max_by_key(|&(len, n)| (n, len))
            .map_or(0, |(len, _)| len);
        let mut m = Self::new(bytes / 2);
        let mut tags = Vec::with_capacity(rows.len());
        if bytes == 0 {
            return (m, tags);
        }
        m.data.reserve(rows.len() * m.dim);
        for (t, b) in rows {
            if b.len() == bytes {
                let at = m.data.len();
                m.data.resize(at + m.dim, 0.0);
                decode_f16_into(&b, &mut m.data[at..]);
                tags.push(t);
            }
        }
        (m, tags)
    }

    /// Append a row; refused (false) when its length isn't `dim`.
    pub fn push(&mut self, row: &[f32]) -> bool {
        if self.dim == 0 || row.len() != self.dim {
            return false;
        }
        self.data.extend_from_slice(row);
        true
    }

    pub fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.dim..(i + 1) * self.dim]
    }

    /// Dot product of `query` with every row (empty when the lengths differ).
    pub fn scores(&self, query: &[f32]) -> Vec<f32> {
        if query.len() != self.dim || self.dim == 0 {
            return Vec::new();
        }
        self.data
            .chunks_exact(self.dim)
            .map(|r| dot(r, query))
            .collect()
    }
}

/// Which scores count as results.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankOptions {
    /// At most this many results.
    pub limit: usize,
    /// Absolute floor (cosine similarity).
    pub min_score: f32,
    /// Results must also score within this much of the best one, so a query
    /// that matches a few photos well doesn't trail off into the rest.
    pub window: f32,
}

impl Default for RankOptions {
    fn default() -> Self {
        Self {
            limit: 500,
            min_score: f32::NEG_INFINITY,
            window: f32::INFINITY,
        }
    }
}

/// The best `(index, score)` pairs passing `opts`, highest first (ties by index).
pub fn rank(
    scores: impl IntoIterator<Item = (usize, f32)>,
    opts: RankOptions,
) -> Vec<(usize, f32)> {
    let mut hits: Vec<(usize, f32)> = scores
        .into_iter()
        .filter(|(_, s)| s.is_finite() && *s >= opts.min_score)
        .collect();
    let order = |a: &(usize, f32), b: &(usize, f32)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
    if opts.limit < hits.len() {
        if opts.limit == 0 {
            return Vec::new();
        }
        hits.select_nth_unstable_by(opts.limit - 1, order);
        hits.truncate(opts.limit);
    }
    hits.sort_unstable_by(order);
    if let Some(best) = hits.first().map(|h| h.1) {
        hits.retain(|h| h.1 >= best - opts.window);
    }
    hits
}

/// Zero-shot classification: softmax over `logit_scale × cosine` of `image`
/// against each label row (CLIP uses a scale of 100). Probabilities in row order.
pub fn zero_shot(image: &[f32], labels: &EmbeddingMatrix, logit_scale: f32) -> Vec<f32> {
    let logits: Vec<f32> = labels
        .scores(image)
        .into_iter()
        .map(|s| s * logit_scale)
        .collect();
    let Some(max) = logits.iter().copied().reduce(f32::max) else {
        return Vec::new();
    };
    let exp: Vec<f32> = logits.iter().map(|l| (l - max).exp()).collect();
    let sum: f32 = exp.iter().sum();
    exp.into_iter().map(|e| e / sum).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn normalizes_and_rejects_degenerate_vectors() {
        let v = normalized(&[3.0, 4.0]).unwrap();
        assert!(close(v[0], 0.6, 1e-6) && close(v[1], 0.8, 1e-6));
        assert!(close(dot(&v, &v), 1.0, 1e-6));
        assert_eq!(normalized(&[]), None);
        assert_eq!(normalized(&[0.0, 0.0]), None);
        assert_eq!(normalized(&[1.0, f32::NAN]), None);
        assert_eq!(normalized(&[f32::INFINITY]), None);
        assert_eq!(normalized(&vec![1.0; MAX_DIM + 1]), None);
        assert!(normalized(&vec![1.0; MAX_DIM]).is_some());
    }

    #[test]
    fn f16_round_trip_keeps_unit_vectors_close() {
        let raw: Vec<f32> = (0..512)
            .map(|i| ((i * 37 % 101) as f32 - 50.0) / 7.0)
            .collect();
        let v = normalized(&raw).unwrap();
        let bytes = encode_f16(&v);
        assert_eq!(bytes.len(), 1024);
        let back = decode_f16(&bytes).unwrap();
        assert_eq!(back.len(), 512);
        let worst = v
            .iter()
            .zip(&back)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(worst < 1e-3, "max error {worst}");
        assert!(close(dot(&v, &back), 1.0, 1e-3));
        assert_eq!(decode_f16(&[]), None);
        assert_eq!(decode_f16(&[1, 2, 3]), None);
    }

    #[test]
    fn dot_handles_tails_and_mismatched_lengths() {
        let a: Vec<f32> = (1..=11).map(|i| i as f32).collect();
        let b = vec![1.0; 11];
        assert!(close(dot(&a, &b), 66.0, 1e-4));
        assert!(close(dot(&a, &b[..3]), 6.0, 1e-6));
        assert!(dot(&[], &a).abs() < f32::EPSILON);
    }

    #[test]
    fn matrix_scores_every_row_and_refuses_wrong_lengths() {
        let mut m = EmbeddingMatrix::new(2);
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.push(&[1.0, 0.0]));
        assert!(m.push(&[0.0, 1.0]));
        assert!(!m.push(&[1.0]));
        assert_eq!((m.len(), m.dim()), (2, 2));
        assert_eq!(m.row(1), &[0.0, 1.0]);
        assert_eq!(m.scores(&[0.5, 2.0]), vec![0.5, 2.0]);
        assert_eq!(m.scores(&[1.0]).len(), 0);
        let mut z = EmbeddingMatrix::default();
        assert!(!z.push(&[]));
        assert_eq!(z.len(), 0);
        assert_eq!(z.scores(&[]).len(), 0);
    }

    #[test]
    fn from_f16_rows_keeps_the_most_common_length() {
        let rows = vec![
            ("short", encode_f16(&[1.0])),
            ("a", encode_f16(&[1.0, 0.0])),
            ("odd", vec![0, 0, 0]),
            ("b", encode_f16(&[0.0, 1.0])),
            ("empty", vec![]),
        ];
        let (m, tags) = EmbeddingMatrix::from_f16_rows(rows);
        assert_eq!((m.dim(), m.len()), (2, 2));
        assert_eq!(tags, vec!["a", "b"]);
        assert_eq!(m.row(1), &[0.0, 1.0]);
        let (m, tags) = EmbeddingMatrix::from_f16_rows(vec![((), vec![1u8])]);
        assert!(m.is_empty() && tags.is_empty());
        let (m, tags) = EmbeddingMatrix::from_f16_rows(Vec::<((), Vec<u8>)>::new());
        assert!(m.is_empty() && tags.is_empty());
    }

    #[test]
    fn rank_orders_limits_and_thresholds() {
        let s = vec![
            (0, 0.1),
            (1, 0.30),
            (2, 0.25),
            (3, f32::NAN),
            (4, 0.30),
            (5, 0.2),
        ];
        let all = rank(s.clone(), RankOptions::default());
        assert_eq!(
            all.iter().map(|h| h.0).collect::<Vec<_>>(),
            vec![1, 4, 2, 5, 0],
            "highest first, ties by index, NaN dropped"
        );
        let top2 = rank(
            s.clone(),
            RankOptions {
                limit: 2,
                ..Default::default()
            },
        );
        assert_eq!(top2, vec![(1, 0.30), (4, 0.30)]);
        let floor = rank(
            s.clone(),
            RankOptions {
                min_score: 0.2,
                ..Default::default()
            },
        );
        assert_eq!(floor.len(), 4);
        let window = rank(
            s.clone(),
            RankOptions {
                window: 0.06,
                ..Default::default()
            },
        );
        assert_eq!(
            window.iter().map(|h| h.0).collect::<Vec<_>>(),
            vec![1, 4, 2]
        );
        assert_eq!(
            rank(
                s,
                RankOptions {
                    limit: 0,
                    ..Default::default()
                }
            )
            .len(),
            0
        );
        assert_eq!(rank(Vec::new(), RankOptions::default()).len(), 0);
    }

    #[test]
    fn zero_shot_softmax_prefers_the_closest_label() {
        let mut labels = EmbeddingMatrix::new(2);
        labels.push(&[1.0, 0.0]);
        labels.push(&[0.0, 1.0]);
        let p = zero_shot(&normalized(&[0.9, 0.1]).unwrap(), &labels, 100.0);
        assert_eq!(p.len(), 2);
        assert!(close(p.iter().sum::<f32>(), 1.0, 1e-5));
        assert!(p[0] > 0.99, "{p:?}");
        assert_eq!(
            zero_shot(&[1.0, 0.0], &EmbeddingMatrix::new(2), 100.0).len(),
            0
        );
    }
}
