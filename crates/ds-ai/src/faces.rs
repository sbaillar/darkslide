//! Face clustering for the People view.
//!
//! Faces arrive as embedding vectors from the app (Apple Vision feature
//! prints of aligned face crops). [`cluster`] groups them by **average
//! linkage** on cosine similarity: two groups merge while the mean similarity
//! between their members is at least [`ClusterConfig::threshold`]. Average
//! linkage keeps one look-alike pair from chaining strangers together (single
//! linkage) without needing every pair to agree (complete linkage).
//!
//! What the user said is a constraint, not a hint:
//! - **must-link** (the same person: faces tagged with one name, a merge)
//!   start in one group;
//! - **cannot-link** (different people, a split, a rejected suggestion)
//!   never end in one group: their similarity is −∞, and average linkage
//!   carries −∞ to every group that contains them.
//!
//! The algorithm is the nearest-neighbour chain (O(n²) time for n groups) over
//! the upper triangle of the similarity matrix, built in parallel; a group
//! whose best neighbour is below the threshold is final. Past
//! [`MAX_BLOCK`] groups the matrix would not fit in memory, so they are
//! merged in blocks first (see [`cluster`]).
//!
//! [`suggest`] compares a group with the known people (mean similarity to
//! each person's confirmed faces), for "Is this Alex?".

use ds_core::embedding;
use rayon::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterConfig {
    /// Minimum mean cosine similarity for two groups to merge.
    pub threshold: f32,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self { threshold: 0.6 }
    }
}

/// User constraints between faces (indices into the vectors).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Links {
    pub must: Vec<(usize, usize)>,
    pub cannot: Vec<(usize, usize)>,
}

/// Union-find over face indices.
struct Sets(Vec<usize>);

impl Sets {
    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] != x {
            self.0[x] = self.0[self.0[x]];
            x = self.0[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a.max(b)] = a.min(b);
        }
    }
}

/// Faces joined by must-links: the groups (faces ascending) and each face's group.
fn initial_groups(n: usize, links: &Links) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut sets = Sets((0..n).collect());
    for &(a, b) in &links.must {
        if a < n && b < n {
            sets.union(a, b);
        }
    }
    let mut members: Vec<Vec<usize>> = Vec::new();
    let mut slot = vec![usize::MAX; n];
    for i in 0..n {
        let r = sets.find(i);
        if slot[r] == usize::MAX {
            slot[r] = members.len();
            members.push(Vec::new());
        }
        members[slot[r]].push(i);
        slot[i] = slot[r];
    }
    (members, slot)
}

/// Most groups compared at once. Their similarities take `k (k − 1) / 2`
/// floats (72 MB at this size); more groups than this are merged block by
/// block first.
const MAX_BLOCK: usize = 6000;

/// A cluster under construction: its faces and the sum of their unit vectors
/// (`None` when a face has no usable vector, which keeps the group apart).
struct Group {
    members: Vec<usize>,
    sum: Option<Vec<f32>>,
}

/// Mean pairwise similarity of k groups, (ΣA · ΣB) / (|A| |B|) of their unit
/// vectors, kept for `a < b` only; −∞ on the diagonal and for a group with an
/// unusable vector.
struct Sim {
    k: usize,
    upper: Vec<f32>,
}

impl Sim {
    fn index(&self, a: usize, b: usize) -> usize {
        let (i, j) = (a.min(b), a.max(b));
        i * (2 * self.k - i - 1) / 2 + (j - i - 1)
    }

    fn get(&self, a: usize, b: usize) -> f32 {
        if a == b {
            f32::NEG_INFINITY
        } else {
            self.upper[self.index(a, b)]
        }
    }

    fn set(&mut self, a: usize, b: usize, v: f32) {
        let at = self.index(a, b);
        self.upper[at] = v;
    }

    fn of(groups: &[Group]) -> Self {
        let k = groups.len();
        let mut upper = vec![0.0f32; k * k.saturating_sub(1) / 2];
        // One slice per row i, holding the entries for j > i.
        let mut rows: Vec<&mut [f32]> = Vec::with_capacity(k);
        let mut rest = upper.as_mut_slice();
        for i in 0..k {
            let (row, tail) = rest.split_at_mut(k - i - 1);
            rows.push(row);
            rest = tail;
        }
        rows.into_par_iter().enumerate().for_each(|(i, row)| {
            for (slot, j) in row.iter_mut().zip(i + 1..) {
                *slot = match (&groups[i].sum, &groups[j].sum) {
                    (Some(a), Some(b)) => {
                        embedding::dot(a, b)
                            / (groups[i].members.len() * groups[j].members.len()) as f32
                    }
                    _ => f32::NEG_INFINITY,
                };
            }
        });
        Self { k, upper }
    }
}

/// The groups of the faces joined by must-links, each with its summed unit
/// vector.
fn start_groups(vectors: &[Vec<f32>], links: &Links) -> Vec<Group> {
    let (members, _) = initial_groups(vectors.len(), links);
    let units: Vec<Option<Vec<f32>>> = vectors.iter().map(|v| embedding::normalized(v)).collect();
    let dim = majority_dim(units.iter().flatten().map(Vec::len)).unwrap_or(0);
    members
        .into_iter()
        .map(|members| Group {
            sum: sum_of(&members, &units, dim),
            members,
        })
        .collect()
}

/// The most common vector length (the first seen wins a tie): one odd face
/// from another model must not set the dimension everyone else is judged by.
fn majority_dim(lens: impl Iterator<Item = usize>) -> Option<usize> {
    let mut counts: Vec<(usize, usize)> = Vec::new();
    for len in lens {
        match counts.iter_mut().find(|(l, _)| *l == len) {
            Some(c) => c.1 += 1,
            None => counts.push((len, 1)),
        }
    }
    counts
        .iter()
        .fold(None, |best: Option<(usize, usize)>, &(l, n)| match best {
            Some((_, m)) if m >= n => best,
            _ => Some((l, n)),
        })
        .map(|(l, _)| l)
}

/// The sum of the unit vectors of `members`; `None` if one is unusable.
fn sum_of(members: &[usize], units: &[Option<Vec<f32>>], dim: usize) -> Option<Vec<f32>> {
    let mut s = vec![0.0f32; dim];
    for &i in members {
        let u = units[i].as_ref().filter(|u| u.len() == dim)?;
        for (a, b) in s.iter_mut().zip(u) {
            *a += b;
        }
    }
    Some(s)
}

/// Average-linkage merging of `groups` (nearest-neighbour chain). `cannot`
/// lists pairs of indices into `groups` that must stay apart.
fn merge_block(mut groups: Vec<Group>, cannot: &[(usize, usize)], threshold: f32) -> Vec<Group> {
    let k = groups.len();
    let mut sim = Sim::of(&groups);
    for &(a, b) in cannot {
        sim.set(a, b, f32::NEG_INFINITY);
    }
    let mut size: Vec<usize> = groups.iter().map(|g| g.members.len()).collect();
    // Active groups that may still merge.
    let mut open = vec![true; k];
    let mut chain: Vec<usize> = Vec::new();
    loop {
        if chain.is_empty() {
            match open.iter().position(|o| *o) {
                Some(a) => chain.push(a),
                None => break,
            }
        }
        let a = *chain.last().expect("non-empty");
        let prev = chain.len().checked_sub(2).map(|i| chain[i]);
        // Nearest open neighbour; the chain's previous group wins ties.
        let mut best: Option<(usize, f32)> = prev.map(|p| (p, sim.get(a, p)));
        for c in (0..k).filter(|&c| c != a && open[c]) {
            let s = sim.get(a, c);
            if best.is_none_or(|(_, b)| s > b) {
                best = Some((c, s));
            }
        }
        match best {
            Some((b, s)) if s >= threshold => {
                if Some(b) == prev {
                    chain.truncate(chain.len() - 2);
                    // Merge b into a (Lance–Williams, average linkage).
                    let (na, nb) = (size[a] as f32, size[b] as f32);
                    for c in (0..k).filter(|&c| c != a && c != b && open[c]) {
                        sim.set(a, c, (na * sim.get(a, c) + nb * sim.get(b, c)) / (na + nb));
                    }
                    size[a] += size[b];
                    open[b] = false;
                    let moved = std::mem::take(&mut groups[b].members);
                    groups[a].members.extend(moved);
                    let other = groups[b].sum.take();
                    groups[a].sum = match (groups[a].sum.take(), other) {
                        (Some(mut x), Some(y)) => {
                            for (p, q) in x.iter_mut().zip(&y) {
                                *p += q;
                            }
                            Some(x)
                        }
                        _ => None,
                    };
                } else {
                    chain.push(b);
                }
            }
            // Nothing close enough: final. Earlier links pointed at it; recompute.
            _ => {
                open[a] = false;
                chain.clear();
            }
        }
    }
    groups.retain(|g| !g.members.is_empty());
    groups
}

/// Group `vectors` (any length, normalized here; a vector that can't be is
/// left alone in its own group). Groups come back largest first, then by their
/// first face; each lists its faces in ascending order. Must-links win over
/// conflicting cannot-links.
///
/// Up to [`MAX_BLOCK`] starting groups are clustered exactly; more are first
/// merged in blocks of that size (groups that clear the threshold inside a
/// block), then the survivors are clustered again, so memory stays bounded.
pub fn cluster(vectors: &[Vec<f32>], links: &Links, cfg: ClusterConfig) -> Vec<Vec<usize>> {
    cluster_blocks(vectors, links, cfg, MAX_BLOCK)
}

fn cluster_blocks(
    vectors: &[Vec<f32>],
    links: &Links,
    cfg: ClusterConfig,
    block: usize,
) -> Vec<Vec<usize>> {
    let n = vectors.len();
    let mut groups = start_groups(vectors, links);
    loop {
        let before = groups.len();
        let mut owner = vec![0usize; n];
        for (g, group) in groups.iter().enumerate() {
            for &i in &group.members {
                owner[i] = g;
            }
        }
        let mut merged = Vec::new();
        let mut start = 0;
        let mut rest = groups;
        while !rest.is_empty() {
            let take = rest.len().min(block);
            let tail = rest.split_off(take);
            let inside = start..start + take;
            let cannot: Vec<(usize, usize)> = links
                .cannot
                .iter()
                .filter(|&&(a, b)| a < n && b < n)
                .map(|&(a, b)| (owner[a], owner[b]))
                .filter(|(ga, gb)| ga != gb && inside.contains(ga) && inside.contains(gb))
                .map(|(ga, gb)| (ga - start, gb - start))
                .collect();
            merged.extend(merge_block(rest, &cannot, cfg.threshold));
            rest = tail;
            start += take;
        }
        groups = merged;
        if before <= block || groups.len() == before {
            break;
        }
    }
    let mut out: Vec<Vec<usize>> = groups.into_iter().map(|g| g.members).collect();
    for m in &mut out {
        m.sort_unstable();
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    out
}

/// A person's confirmed faces, summarized: the sum of their unit vectors and
/// how many there are (mean similarity to them = `dot(face, sum) / count`).
#[derive(Debug, Clone, PartialEq)]
pub struct Exemplars {
    pub sum: Vec<f32>,
    pub count: usize,
}

impl Exemplars {
    pub fn of(vectors: &[Vec<f32>]) -> Option<Self> {
        let units: Vec<Vec<f32>> = vectors
            .iter()
            .filter_map(|v| embedding::normalized(v))
            .collect();
        let dim = majority_dim(units.iter().map(Vec::len))?;
        let mut sum = vec![0.0f32; dim];
        let mut count = 0;
        for u in units.iter().filter(|u| u.len() == dim) {
            for (a, b) in sum.iter_mut().zip(u) {
                *a += b;
            }
            count += 1;
        }
        Some(Self { sum, count })
    }

    /// Mean similarity between `other`'s faces and these.
    pub fn similarity(&self, other: &Exemplars) -> f32 {
        if self.count == 0 || other.count == 0 || self.sum.len() != other.sum.len() {
            return f32::NEG_INFINITY;
        }
        embedding::dot(&self.sum, &other.sum) / (self.count * other.count) as f32
    }
}

/// The known person (index into `people`) whose faces `group` resembles most,
/// on average, if at least `threshold`; ties go to the first.
pub fn suggest(group: &Exemplars, people: &[Exemplars], threshold: f32) -> Option<(usize, f32)> {
    people
        .iter()
        .enumerate()
        .map(|(i, p)| (i, group.similarity(p)))
        .filter(|(_, s)| *s >= threshold)
        .fold(None, |best: Option<(usize, f32)>, (i, s)| match best {
            Some((_, b)) if b >= s => best,
            _ => Some((i, s)),
        })
}

/// Clustering quality against true labels: pairwise precision (pairs put
/// together that are the same person), recall (same-person pairs put
/// together), F1, and purity (share of faces in their group's majority person).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterQuality {
    pub precision: f32,
    pub recall: f32,
    pub f1: f32,
    pub purity: f32,
    pub groups: usize,
    pub people: usize,
}

pub fn quality<L: PartialEq + Clone>(groups: &[Vec<usize>], labels: &[L]) -> ClusterQuality {
    let mut assigned = vec![usize::MAX; labels.len()];
    for (g, m) in groups.iter().enumerate() {
        for &i in m {
            if i < labels.len() {
                assigned[i] = g;
            }
        }
    }
    let (mut tp, mut fp, mut fn_) = (0u64, 0u64, 0u64);
    for i in 0..labels.len() {
        for j in i + 1..labels.len() {
            let same = labels[i] == labels[j];
            let together = assigned[i] == assigned[j] && assigned[i] != usize::MAX;
            match (same, together) {
                (true, true) => tp += 1,
                (false, true) => fp += 1,
                (true, false) => fn_ += 1,
                (false, false) => {}
            }
        }
    }
    let ratio = |a: u64, b: u64| {
        if a + b == 0 {
            1.0
        } else {
            a as f32 / (a + b) as f32
        }
    };
    let (precision, recall) = (ratio(tp, fp), ratio(tp, fn_));
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    let mut majority = 0usize;
    for m in groups {
        let mut counts: Vec<(L, usize)> = Vec::new();
        for &i in m.iter().filter(|&&i| i < labels.len()) {
            match counts.iter_mut().find(|(l, _)| *l == labels[i]) {
                Some(c) => c.1 += 1,
                None => counts.push((labels[i].clone(), 1)),
            }
        }
        majority += counts.iter().map(|c| c.1).max().unwrap_or(0);
    }
    let mut people: Vec<&L> = Vec::new();
    for l in labels {
        if !people.contains(&l) {
            people.push(l);
        }
    }
    ClusterQuality {
        precision,
        recall,
        f1,
        purity: if labels.is_empty() {
            1.0
        } else {
            majority as f32 / labels.len() as f32
        },
        groups: groups.len(),
        people: people.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit-ish vector near axis `p` (a person), jittered by `j`.
    fn face(p: usize, j: f32) -> Vec<f32> {
        let mut v = vec![0.05f32; 8];
        v[p] = 1.0;
        v[(p + 1) % 8] += j;
        v
    }

    fn cfg() -> ClusterConfig {
        ClusterConfig::default()
    }

    #[test]
    fn separates_people_and_keeps_order() {
        let faces = vec![
            face(0, 0.0),
            face(1, 0.1),
            face(0, 0.2),
            face(2, 0.0),
            face(1, 0.0),
            face(0, 0.1),
        ];
        let g = cluster(&faces, &Links::default(), cfg());
        assert_eq!(g, [vec![0, 2, 5], vec![1, 4], vec![3]]);
        let q = quality(&g, &[0, 1, 0, 2, 1, 0]);
        assert_eq!(
            (q.precision, q.recall, q.purity, q.groups, q.people),
            (1.0, 1.0, 1.0, 3, 3)
        );
        assert_eq!(cluster(&[], &Links::default(), cfg()).len(), 0);
        assert_eq!(
            cluster(&[face(0, 0.0)], &Links::default(), cfg()),
            [vec![0]]
        );
    }

    #[test]
    fn average_linkage_does_not_chain_strangers() {
        // a ~ b ~ c in a line: a-b and b-c are close, a-c far. Single linkage
        // would join all three; average linkage joins one pair only.
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.80, 0.60, 0.0];
        let c = vec![0.28, 0.96, 0.0];
        let g = cluster(
            &[a, b, c],
            &Links::default(),
            ClusterConfig { threshold: 0.75 },
        );
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|m| m.len() <= 2));
    }

    #[test]
    fn constraints_win() {
        let faces = vec![face(0, 0.0), face(0, 0.1), face(3, 0.0), face(0, 0.05)];
        // Cannot-link 0 and 1: they (and whoever joins either) stay apart.
        let links = Links {
            must: vec![],
            cannot: vec![(0, 1)],
        };
        let g = cluster(&faces, &links, cfg());
        assert!(
            g.iter().all(|m| !(m.contains(&0) && m.contains(&1))),
            "{g:?}"
        );
        assert_eq!(g.iter().find(|m| m.contains(&3)).unwrap().len(), 2);
        // Must-link a stranger in: one group regardless of looks.
        let links = Links {
            must: vec![(0, 2), (2, 2), (9, 0)],
            cannot: vec![(0, 2), (7, 1)],
        };
        let g = cluster(&faces, &links, cfg());
        assert!(g.iter().any(|m| m.contains(&0) && m.contains(&2)), "{g:?}");
        // Unusable vectors stay alone.
        let g = cluster(
            &[face(0, 0.0), vec![0.0; 8], face(0, 0.0), vec![1.0; 3]],
            &Links::default(),
            cfg(),
        );
        assert_eq!(g, [vec![0, 2], vec![1], vec![3]]);
        let must = Links {
            must: vec![(0, 1)],
            cannot: vec![],
        };
        assert_eq!(cluster(&[face(0, 0.0), vec![]], &must, cfg()), [vec![0, 1]]);
    }

    #[test]
    fn threshold_controls_merging() {
        let faces = vec![face(0, 0.0), face(0, 0.9)];
        assert_eq!(
            cluster(&faces, &Links::default(), ClusterConfig { threshold: 0.99 }).len(),
            2
        );
        assert_eq!(
            cluster(&faces, &Links::default(), ClusterConfig { threshold: 0.5 }).len(),
            1
        );
    }

    #[test]
    fn many_faces_cluster_exactly() {
        // 6 people × 40 faces with jitter, shuffled deterministically.
        let mut faces = Vec::new();
        let mut labels = Vec::new();
        for i in 0..240usize {
            let p = (i * 7) % 6;
            faces.push(face(p, ((i * 13) % 10) as f32 * 0.03));
            labels.push(p);
        }
        let g = cluster(&faces, &Links::default(), cfg());
        let q = quality(&g, &labels);
        assert_eq!((q.groups, q.f1), (6, 1.0), "{q:?}");
    }

    #[test]
    fn blocks_give_the_same_groups_on_clear_data() {
        let mut faces = Vec::new();
        let mut labels = Vec::new();
        for i in 0..240usize {
            let p = (i * 7) % 6;
            faces.push(face(p, ((i * 13) % 10) as f32 * 0.03));
            labels.push(p);
        }
        let exact = cluster(&faces, &Links::default(), cfg());
        for block in [7, 50, 100] {
            let g = cluster_blocks(&faces, &Links::default(), cfg(), block);
            assert_eq!(g, exact, "block {block}");
        }
    }

    #[test]
    fn blocks_keep_cannot_links_and_must_links() {
        let faces: Vec<Vec<f32>> = (0..40).map(|i| face(i % 2, 0.01 * i as f32)).collect();
        // Faces 0 and 2 look alike but are different people; 1 and 3 are
        // told to be one person. Blocks of 6 put them in different blocks.
        let links = Links {
            must: vec![(1, 3)],
            cannot: vec![(0, 2)],
        };
        let g = cluster_blocks(&faces, &links, cfg(), 6);
        assert!(
            g.iter().all(|m| !(m.contains(&0) && m.contains(&2))),
            "{g:?}"
        );
        assert!(g.iter().any(|m| m.contains(&1) && m.contains(&3)));
        // Every face lands in exactly one group.
        let mut all: Vec<usize> = g.concat();
        all.sort_unstable();
        assert_eq!(all, (0..40).collect::<Vec<_>>());
        // Unusable vectors are never merged, in or across blocks.
        let blanks = vec![vec![0.0; 8]; 30];
        assert_eq!(
            cluster_blocks(&blanks, &Links::default(), cfg(), 4).len(),
            30
        );
    }

    #[test]
    fn similarities_are_stored_as_an_upper_triangle() {
        let faces: Vec<Vec<f32>> = (0..5).map(|i| face(i, 0.0)).collect();
        let groups = start_groups(&faces, &Links::default());
        let sim = Sim::of(&groups);
        assert_eq!(sim.upper.len(), 10);
        assert_eq!(sim.get(2, 2), f32::NEG_INFINITY);
        assert_eq!(sim.get(1, 3), sim.get(3, 1));
        assert!(sim.get(0, 1) < 0.2);
        let mut seen = std::collections::HashSet::new();
        for a in 0..5 {
            for b in a + 1..5 {
                assert!(seen.insert(sim.index(a, b)));
            }
        }
        assert_eq!(Sim::of(&[]).upper.len(), 0);
    }

    #[test]
    fn the_majority_dimension_wins_not_the_first_face() {
        // The first face comes from another model (3 numbers); the rest have 8.
        let mut faces = vec![vec![1.0, 0.0, 0.0]];
        faces.extend([face(0, 0.0), face(0, 0.1), face(0, 0.05), face(1, 0.0)]);
        let g = cluster(&faces, &Links::default(), cfg());
        assert_eq!(g, [vec![1, 2, 3], vec![0], vec![4]]);
        let e = Exemplars::of(&faces).unwrap();
        assert_eq!((e.sum.len(), e.count), (8, 4));
        assert_eq!(majority_dim([2, 3, 3, 2, 3].into_iter()), Some(3));
        assert_eq!(majority_dim([2, 3].into_iter()), Some(2));
        assert_eq!(majority_dim(std::iter::empty()), None);
    }

    #[test]
    fn suggestions() {
        let alex = Exemplars::of(&[face(0, 0.0), face(0, 0.1)]).unwrap();
        let sam = Exemplars::of(&[face(1, 0.0)]).unwrap();
        let group = Exemplars::of(&[face(0, 0.05)]).unwrap();
        let (i, s) = suggest(&group, &[sam.clone(), alex.clone()], 0.6).unwrap();
        assert_eq!(i, 1);
        assert!(s > 0.9);
        assert_eq!(suggest(&group, std::slice::from_ref(&sam), 0.6), None);
        assert_eq!(suggest(&group, &[], 0.6), None);
        // Equal similarity: the first wins.
        assert_eq!(
            suggest(&group, &[alex.clone(), alex.clone()], 0.0)
                .unwrap()
                .0,
            0
        );
        assert!(Exemplars::of(&[]).is_none());
        assert!(Exemplars::of(&[vec![0.0; 4]]).is_none());
        let mixed = Exemplars::of(&[vec![1.0, 0.0], vec![1.0, 0.0, 0.0]]).unwrap();
        assert_eq!(mixed.count, 1);
        let other_dim = Exemplars::of(&[vec![1.0, 0.0, 0.0]]).unwrap();
        assert_eq!(mixed.similarity(&other_dim), f32::NEG_INFINITY);
        let empty = Exemplars {
            sum: vec![0.0; 2],
            count: 0,
        };
        assert_eq!(empty.similarity(&mixed), f32::NEG_INFINITY);
    }

    /// Clustering quality of a labeled face set (`label<TAB>v1,v2,…` per line,
    /// written by the app's `faceEvalHarness`), over a threshold sweep. Skipped
    /// unless `DS_FACE_EVAL_TSV` names the file.
    #[test]
    fn eval_harness() {
        let Some(path) = std::env::var_os("DS_FACE_EVAL_TSV") else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap();
        let (labels, vectors): (Vec<&str>, Vec<Vec<f32>>) = text
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(l, v)| (l, v.split(',').filter_map(|x| x.parse().ok()).collect()))
            .unzip();
        for t in [0.6, 0.65, 0.7, 0.75, 0.8, 0.85] {
            let g = cluster(&vectors, &Links::default(), ClusterConfig { threshold: t });
            let q = quality(&g, &labels);
            println!(
                "t={t:.2} faces={} people={} groups={} P={:.3} R={:.3} F1={:.3} purity={:.3}",
                labels.len(),
                q.people,
                q.groups,
                q.precision,
                q.recall,
                q.f1,
                q.purity
            );
        }
    }

    #[test]
    fn quality_metrics() {
        // Two people split into three groups, one face misplaced.
        let groups = vec![vec![0, 1], vec![2], vec![3, 4, 5]];
        let labels = ["a", "a", "a", "b", "b", "a"];
        let q = quality(&groups, &labels);
        // Pairs together: (0,1) ok; (3,4) ok, (3,5) and (4,5) wrong → 2/4.
        assert!((q.precision - 0.5).abs() < 1e-6, "{q:?}");
        // Same-person pairs: a has 4 faces (6 pairs), b 2 (1): 2 of 7 together.
        assert!((q.recall - 2.0 / 7.0).abs() < 1e-6);
        assert!((q.purity - 5.0 / 6.0).abs() < 1e-6);
        assert_eq!((q.groups, q.people), (3, 2));
        let none: [u8; 0] = [];
        let q = quality(&[], &none);
        assert_eq!((q.precision, q.recall, q.purity), (1.0, 1.0, 1.0));
        // Everything apart: no pairs together, recall 0, F1 0.
        let q = quality(&[vec![0], vec![1]], &["a", "a"]);
        assert_eq!((q.recall, q.f1), (0.0, 0.0));
        // Faces in no group count as apart; out-of-range indices are ignored.
        let q = quality(&[vec![0, 7]], &["a", "a"]);
        assert_eq!(q.recall, 0.0);
    }
}
