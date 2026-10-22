//! Small dense geometry: 3×3 matrices, homographies (normalized DLT), rotations from ray
//! correspondences (Horn's quaternion method), RANSAC, and a symmetric eigensolver.

/// Row-major 3×3 matrix.
pub type Mat3 = [[f64; 3]; 3];

pub(crate) const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub(crate) fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

pub(crate) fn transpose(a: &Mat3) -> Mat3 {
    std::array::from_fn(|i| std::array::from_fn(|j| a[j][i]))
}

pub(crate) fn mul_vec(a: &Mat3, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| a[i][0] * v[0] + a[i][1] * v[1] + a[i][2] * v[2])
}

pub(crate) fn det(a: &Mat3) -> f64 {
    a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
}

pub(crate) fn inverse(a: &Mat3) -> Option<Mat3> {
    let d = det(a);
    if !d.is_finite() || d.abs() < 1e-12 {
        return None;
    }
    let c =
        |r0: usize, r1: usize, c0: usize, c1: usize| a[r0][c0] * a[r1][c1] - a[r0][c1] * a[r1][c0];
    Some([
        [c(1, 2, 1, 2) / d, -c(0, 2, 1, 2) / d, c(0, 1, 1, 2) / d],
        [-c(1, 2, 0, 2) / d, c(0, 2, 0, 2) / d, -c(0, 1, 0, 2) / d],
        [c(1, 2, 0, 1) / d, -c(0, 2, 0, 1) / d, c(0, 1, 0, 1) / d],
    ])
}

/// `h · (x, y, 1)` dehomogenized; `None` at or behind the horizon.
#[inline]
pub(crate) fn project(h: &Mat3, x: f64, y: f64) -> Option<(f64, f64)> {
    let w = h[2][0] * x + h[2][1] * y + h[2][2];
    if w <= 1e-9 {
        return None;
    }
    Some((
        (h[0][0] * x + h[0][1] * y + h[0][2]) / w,
        (h[1][0] * x + h[1][1] * y + h[1][2]) / w,
    ))
}

pub(crate) fn normalize3(v: [f64; 3]) -> [f64; 3] {
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    v.map(|c| c / n)
}

pub(crate) fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub(crate) fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Rotation by `angle` (radians) about the unit `axis`.
#[cfg(test)]
pub(crate) fn axis_angle(axis: [f64; 3], angle: f64) -> Mat3 {
    let [x, y, z] = normalize3(axis);
    let (s, c) = angle.sin_cos();
    let t = 1.0 - c;
    [
        [t * x * x + c, t * x * y - s * z, t * x * z + s * y],
        [t * x * y + s * z, t * y * y + c, t * y * z - s * x],
        [t * x * z - s * y, t * y * z + s * x, t * z * z + c],
    ]
}

/// Angle of a rotation matrix (radians).
#[cfg(test)]
pub(crate) fn rotation_angle(r: &Mat3) -> f64 {
    ((r[0][0] + r[1][1] + r[2][2] - 1.0) / 2.0)
        .clamp(-1.0, 1.0)
        .acos()
}

/// Solve `a · x = b` (Gaussian elimination with partial pivoting).
fn solve<const N: usize>(mut a: [[f64; N]; N], mut b: [f64; N]) -> Option<[f64; N]> {
    for col in 0..N {
        let piv = (col..N).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        let pivot = a[col];
        for row in col + 1..N {
            let f = a[row][col] / pivot[col];
            for (v, p) in a[row][col..].iter_mut().zip(&pivot[col..]) {
                *v -= f * p;
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = [0.0; N];
    for row in (0..N).rev() {
        let s: f64 = (row + 1..N).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// Hartley normalization: centroid to the origin, mean distance √2.
fn normalizer(pts: impl Iterator<Item = (f64, f64)> + Clone) -> Mat3 {
    let n = pts.clone().count().max(1) as f64;
    let (cx, cy) = pts.clone().fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
    let (cx, cy) = (cx / n, cy / n);
    let d = pts.map(|p| (p.0 - cx).hypot(p.1 - cy)).sum::<f64>() / n;
    let s = if d > 1e-12 {
        std::f64::consts::SQRT_2 / d
    } else {
        1.0
    };
    [[s, 0.0, -s * cx], [0.0, s, -s * cy], [0.0, 0.0, 1.0]]
}

/// A point correspondence `src → dst`.
pub(crate) type Pair = ((f64, f64), (f64, f64));

/// Least-squares homography mapping `src` to `dst` (≥ 4 pairs; `h33 = 1`).
pub(crate) fn homography(pairs: &[Pair]) -> Option<Mat3> {
    if pairs.len() < 4 {
        return None;
    }
    let ts = normalizer(pairs.iter().map(|p| p.0));
    let td = normalizer(pairs.iter().map(|p| p.1));
    let mut ata = [[0.0f64; 8]; 8];
    let mut atb = [0.0f64; 8];
    for &((sx, sy), (dx, dy)) in pairs {
        let (x, y) = (ts[0][0] * sx + ts[0][2], ts[1][1] * sy + ts[1][2]);
        let (u, v) = (td[0][0] * dx + td[0][2], td[1][1] * dy + td[1][2]);
        let rows = [
            ([x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y], u),
            ([0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y], v),
        ];
        for (r, t) in rows {
            for i in 0..8 {
                atb[i] += r[i] * t;
                for j in 0..8 {
                    ata[i][j] += r[i] * r[j];
                }
            }
        }
    }
    let h = solve(ata, atb)?;
    let hn = [[h[0], h[1], h[2]], [h[3], h[4], h[5]], [h[6], h[7], 1.0]];
    let full = mul(&inverse(&td)?, &mul(&hn, &ts));
    let s = full[2][2];
    (s.abs() > 1e-12).then(|| full.map(|r| r.map(|v| v / s)))
}

/// Deterministic xorshift generator (RANSAC samples are reproducible).
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub(crate) fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    /// `k` distinct indices below `n` (`k ≤ n`).
    pub(crate) fn distinct<const K: usize>(&mut self, n: usize) -> [usize; K] {
        let mut out = [0usize; K];
        let mut i = 0;
        while i < K {
            let v = self.below(n);
            if !out[..i].contains(&v) {
                out[i] = v;
                i += 1;
            }
        }
        out
    }
}

/// RANSAC homography: the model with the most pairs within `thresh` px, refit on them.
/// Returns the model and its inlier indices.
pub(crate) fn ransac_homography(
    pairs: &[Pair],
    thresh: f64,
    iters: usize,
) -> Option<(Mat3, Vec<usize>)> {
    ransac_counted(pairs, thresh, iters).map(|(h, inl, _)| (h, inl))
}

/// Confidence that a 4-point sample from the best consensus is outlier-free.
const CONFIDENCE: f64 = 0.99;

/// Iterations after which an all-inlier sample has been drawn with `CONFIDENCE`, given
/// an inlier fraction `w`.
fn needed_iterations(w: f64) -> usize {
    let all_inlier = w.powi(4);
    if all_inlier >= 1.0 {
        return 1;
    }
    if all_inlier <= 0.0 {
        return usize::MAX;
    }
    ((1.0 - CONFIDENCE).ln() / (1.0 - all_inlier).ln()).ceil() as usize
}

/// [`ransac_homography`] that also reports the iterations run: at most `iters`, and
/// fewer once the best consensus makes a better all-inlier sample unlikely.
pub(crate) fn ransac_counted(
    pairs: &[Pair],
    thresh: f64,
    iters: usize,
) -> Option<(Mat3, Vec<usize>, usize)> {
    if pairs.len() < 4 {
        return None;
    }
    let inliers_of = |h: &Mat3| -> Vec<usize> {
        pairs
            .iter()
            .enumerate()
            .filter(|(_, ((sx, sy), (dx, dy)))| {
                project(h, *sx, *sy).is_some_and(|(x, y)| (x - dx).hypot(y - dy) < thresh)
            })
            .map(|(i, _)| i)
            .collect()
    };
    let mut rng = Rng::new(0x9e37_79b9_7f4a_7c15 ^ pairs.len() as u64);
    let mut best: Vec<usize> = Vec::new();
    let mut limit = iters;
    let mut run = 0;
    while run < limit {
        run += 1;
        let idx = rng.distinct::<4>(pairs.len());
        let sample: [Pair; 4] = idx.map(|i| pairs[i]);
        let Some(h) = homography(&sample) else {
            continue;
        };
        let inl = inliers_of(&h);
        if inl.len() > best.len() {
            best = inl;
            limit = limit.min(needed_iterations(best.len() as f64 / pairs.len() as f64));
        }
    }
    if best.len() < 4 {
        return None;
    }
    // Refit on the consensus twice (the set can grow once the fit improves).
    let mut h = homography(&best.iter().map(|&i| pairs[i]).collect::<Vec<_>>())?;
    for _ in 0..2 {
        let inl = inliers_of(&h);
        if inl.len() < 4 {
            break;
        }
        h = homography(&inl.iter().map(|&i| pairs[i]).collect::<Vec<_>>())?;
        best = inl;
    }
    Some((h, best, run))
}

/// Eigen-decomposition of a symmetric matrix (cyclic Jacobi). Returns eigenvalues and
/// eigenvectors as columns (`vecs[row][k]` is component `row` of vector `k`).
pub(crate) fn sym_eigen<const N: usize>(mut a: [[f64; N]; N]) -> ([f64; N], [[f64; N]; N]) {
    let mut v = [[0.0; N]; N];
    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for _ in 0..64 {
        let off: f64 = (0..N)
            .flat_map(|i| (0..N).filter(move |&j| j != i).map(move |j| (i, j)))
            .map(|(i, j)| a[i][j] * a[i][j])
            .sum();
        if off < 1e-22 {
            break;
        }
        for p in 0..N {
            for q in p + 1..N {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for row in &mut a {
                    let (akp, akq) = (row[p], row[q]);
                    row[p] = c * akp - s * akq;
                    row[q] = s * akp + c * akq;
                }
                let (rp, rq) = (a[p], a[q]);
                for k in 0..N {
                    a[p][k] = c * rp[k] - s * rq[k];
                    a[q][k] = s * rp[k] + c * rq[k];
                }
                for row in &mut v {
                    let (vp, vq) = (row[p], row[q]);
                    row[p] = c * vp - s * vq;
                    row[q] = s * vp + c * vq;
                }
            }
        }
    }
    (std::array::from_fn(|i| a[i][i]), v)
}

/// The rotation `R` minimizing `Σ |R·a − b|²` over unit-vector pairs (Horn 1987).
pub(crate) fn rotation_fit(pairs: &[([f64; 3], [f64; 3])]) -> Mat3 {
    let mut s = [[0.0f64; 3]; 3];
    for (a, b) in pairs {
        for i in 0..3 {
            for j in 0..3 {
                s[i][j] += a[i] * b[j];
            }
        }
    }
    let [[sxx, sxy, sxz], [syx, syy, syz], [szx, szy, szz]] = s;
    let n = [
        [sxx + syy + szz, syz - szy, szx - sxz, sxy - syx],
        [syz - szy, sxx - syy - szz, sxy + syx, szx + sxz],
        [szx - sxz, sxy + syx, -sxx + syy - szz, syz + szy],
        [sxy - syx, szx + sxz, syz + szy, -sxx - syy + szz],
    ];
    let (vals, vecs) = sym_eigen(n);
    let k = (0..4)
        .max_by(|&i, &j| vals[i].total_cmp(&vals[j]))
        .unwrap_or(0);
    let [w, x, y, z] = [vecs[0][k], vecs[1][k], vecs[2][k], vecs[3][k]];
    [
        [
            w * w + x * x - y * y - z * z,
            2.0 * (x * y - w * z),
            2.0 * (x * z + w * y),
        ],
        [
            2.0 * (x * y + w * z),
            w * w - x * x + y * y - z * z,
            2.0 * (y * z - w * x),
        ],
        [
            2.0 * (x * z - w * y),
            2.0 * (y * z + w * x),
            w * w - x * x - y * y + z * z,
        ],
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::cast_lossless)]

    use super::*;

    fn close(a: &Mat3, b: &Mat3, tol: f64) -> bool {
        (0..3).all(|i| (0..3).all(|j| (a[i][j] - b[i][j]).abs() < tol))
    }

    #[test]
    fn matrix_basics() {
        let a: Mat3 = [[2.0, 1.0, 0.0], [0.0, 3.0, 1.0], [1.0, 0.0, 1.0]];
        let ai = inverse(&a).unwrap();
        assert!(close(&mul(&a, &ai), &IDENTITY, 1e-12));
        assert!(inverse(&[[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 0.0, 1.0]]).is_none());
        assert_eq!(transpose(&a)[0][2], 1.0);
        assert_eq!(det(&IDENTITY), 1.0);
        assert_eq!(mul_vec(&a, [1.0, 1.0, 1.0]), [3.0, 4.0, 2.0]);
        assert_eq!(cross3([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0]);
        let r = axis_angle([0.0, 1.0, 0.0], 0.3);
        assert!((rotation_angle(&r) - 0.3).abs() < 1e-12);
        assert!(close(&mul(&r, &transpose(&r)), &IDENTITY, 1e-12));
        // Behind the horizon.
        assert!(
            project(
                &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]],
                2.0,
                0.0
            )
            .is_none()
        );
        assert!(solve([[0.0, 0.0], [0.0, 0.0]], [1.0, 1.0]).is_none());
    }

    #[test]
    fn homography_recovers_an_exact_projective_map() {
        let h: Mat3 = [[1.02, 0.03, 5.0], [-0.02, 0.98, -3.0], [1e-4, -5e-5, 1.0]];
        let pairs: Vec<Pair> = (0..20)
            .map(|i| {
                let (x, y) = ((i * 37 % 500) as f64, (i * 91 % 300) as f64);
                ((x, y), project(&h, x, y).unwrap())
            })
            .collect();
        assert!(close(&homography(&pairs).unwrap(), &h, 1e-8));
        assert!(homography(&pairs[..3]).is_none());
        // Collinear points are degenerate.
        let line: Vec<Pair> = (0..6)
            .map(|i| ((i as f64, i as f64), (i as f64, 0.0)))
            .collect();
        assert!(homography(&line).is_none());
    }

    #[test]
    fn ransac_ignores_outliers() {
        let h: Mat3 = [[1.0, 0.0, 12.5], [0.0, 1.0, -7.25], [0.0, 0.0, 1.0]];
        let mut pairs: Vec<Pair> = (0..60)
            .map(|i| {
                let (x, y) = ((i * 37 % 400) as f64, (i * 53 % 300) as f64);
                ((x, y), project(&h, x, y).unwrap())
            })
            .collect();
        for i in 0..30 {
            pairs.push((
                (i as f64 * 7.0, 100.0),
                (300.0 - i as f64 * 3.0, i as f64 * 11.0),
            ));
        }
        let (got, inl) = ransac_homography(&pairs, 1.0, 300).unwrap();
        assert!(close(&got, &h, 1e-6));
        assert_eq!(inl.len(), 60);
        assert!(ransac_homography(&pairs[..3], 1.0, 10).is_none());
    }

    #[test]
    fn ransac_stops_early_on_a_clean_consensus() {
        let h: Mat3 = [[1.0, 0.0, 12.5], [0.0, 1.0, -7.25], [0.0, 0.0, 1.0]];
        let pairs: Vec<Pair> = (0..200)
            .map(|i| {
                let (x, y) = ((i * 37 % 400) as f64, (i * 53 % 300) as f64);
                ((x, y), project(&h, x, y).unwrap())
            })
            .collect();
        let (got, inl, run) = ransac_counted(&pairs, 1.0, 1000).unwrap();
        assert!(close(&got, &h, 1e-6));
        assert_eq!(inl.len(), 200);
        // All inliers: one all-inlier sample is enough.
        assert!(run <= 2, "{run} iterations");
        // Mostly outliers keep sampling, up to the cap.
        let mut noisy = pairs[..100].to_vec();
        noisy.extend((0..100).map(|i| ((i as f64 * 3.0, 5.0), (400.0 - i as f64, i as f64 * 2.0))));
        let (_, inl, run) = ransac_counted(&noisy, 1.0, 1000).unwrap();
        assert_eq!(inl.len(), 100);
        assert!(run > 20, "{run} iterations");
        assert_eq!(needed_iterations(0.0), usize::MAX);
        assert_eq!(needed_iterations(1.0), 1);
        assert_eq!(needed_iterations(0.5), 72);
    }

    #[test]
    fn eigen_and_rotation_fit() {
        let (vals, vecs) = sym_eigen([[2.0, 1.0], [1.0, 2.0]]);
        let mut v = vals;
        v.sort_by(f64::total_cmp);
        assert!((v[0] - 1.0).abs() < 1e-12 && (v[1] - 3.0).abs() < 1e-12);
        assert!((vecs[0][0].abs() - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-9);

        let r = mul(
            &axis_angle([0.0, 1.0, 0.0], 0.4),
            &axis_angle([1.0, 0.0, 0.2], -0.1),
        );
        let rays: Vec<[f64; 3]> = (0..30)
            .map(|i| normalize3([(i % 7) as f64 * 0.1 - 0.3, (i % 5) as f64 * 0.1 - 0.2, 1.0]))
            .collect();
        let pairs: Vec<([f64; 3], [f64; 3])> = rays.iter().map(|a| (*a, mul_vec(&r, *a))).collect();
        assert!(close(&rotation_fit(&pairs), &r, 1e-9));
    }

    #[test]
    fn rng_is_deterministic_and_distinct() {
        let mut a = Rng::new(0);
        let mut b = Rng::new(0);
        let x = a.distinct::<4>(5);
        assert_eq!(x, b.distinct::<4>(5));
        let mut s = x.to_vec();
        s.sort_unstable();
        s.dedup();
        assert_eq!(s.len(), 4);
        assert!(x.iter().all(|&v| v < 5));
    }
}
