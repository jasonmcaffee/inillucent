//! Cosine similarity over L2 normalized vectors, which reduces to a dot product.
//!
//! Vectors are normalized once at insert time, so nothing here divides or takes a
//! square root. The four independent accumulators matter more than they look: a
//! single accumulator serializes the whole loop on the latency of floating point
//! addition, because each add depends on the previous one. Four chains let the
//! CPU keep several adds in flight and let LLVM emit SIMD.
//!
//! Invariant: **one metric decides both how a vector is stored and how two of
//! them are compared.** A set normalized on the way in and compared with a
//! measure that assumes raw magnitudes gives wrong neighbours quietly, so the
//! `Metric` travels with the vectors rather than being chosen at the call
//! site.

/// Dot product of two equal length slices.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut s0 = 0.0f32;
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    let mut s3 = 0.0f32;

    // **`chunks_exact` rather than an index, and it is the same four chains
    // (task-1932, H9).** The comment that was here said "bounds checks are
    // hoisted by the slice reborrow below", which is a claim about what LLVM
    // does rather than anything the code guarantees - and this crate now denies
    // `indexing_slicing`. `chunks_exact` states the same fact in a form the
    // compiler has to honour: a fixed-width window with no bound to check at
    // all, which is what lets it emit SIMD. `zip` also makes the two lengths
    // agreeing something the loop enforces rather than something the caller is
    // trusted for.
    for (x, y) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let [x0, x1, x2, x3] = *<&[f32; 4]>::try_from(x).unwrap_or(&[0.0; 4]);
        let [y0, y1, y2, y3] = *<&[f32; 4]>::try_from(y).unwrap_or(&[0.0; 4]);
        s0 += x0 * y0;
        s1 += x1 * y1;
        s2 += x2 * y2;
        s3 += x3 * y3;
    }

    let mut acc = (s0 + s1) + (s2 + s3);
    // The components a multiple of four does not reach, at most three of them.
    for (x, y) in a
        .chunks_exact(4)
        .remainder()
        .iter()
        .zip(b.chunks_exact(4).remainder())
    {
        acc += x * y;
    }
    acc
}

/// Cosine distance for normalized vectors: `1 - cosine similarity`.
///
/// Kept in the same range and orientation as pgvector's `<=>` operator so the
/// harness can compare distances between the two engines directly.
#[inline]
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    1.0 - dot(a, b)
}

/// L2 normalize in place. A zero vector is left alone rather than producing NaN;
/// pgvector likewise refuses to index a zero vector for cosine distance.
pub fn normalize(v: &mut [f32]) {
    let norm = dot(v, v).sqrt();
    if norm > 0.0 {
        let inv = 1.0 / norm;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
}

/// Truncate to `dims` and renormalize, the Matryoshka operation. The model is
/// trained so a prefix of the embedding is itself a usable embedding, but the
/// prefix is no longer unit length, so it has to be normalized again.
pub fn truncate_normalized(v: &[f32], dims: usize) -> Vec<f32> {
    let mut out = v.get(..dims.min(v.len())).unwrap_or(v).to_vec();
    normalize(&mut out);
    out
}

/// Squared Euclidean distance.
///
/// No square root. A top-k ordered by distance is the same top-k whether or
/// not the epsilon is applied - `sqrt` is monotone increasing over the
/// non-negative reals a squared distance always is - so the root is one
/// operation this build does not pay for on every comparison a graph walk
/// makes. `vector_distance_l2` still takes it, once, on the handful of rows a
/// query actually projects; the index never does, because the index never
/// returns a distance, only an order.
/// @param a - one vector
/// @param b - the other, the same width
pub fn squared_euclidean(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

/// Which distance a vector structure minimises.
///
/// **Cosine and L2 want opposite things done to a vector before it is
/// stored.** Cosine is one minus the dot product of two *unit* vectors, so
/// every stored vector is normalized once at insert and the comparison is a
/// dot product for the rest of the structure's life. L2 measures the raw
/// distance between two points, and normalizing either one first would move
/// it to a point at a fixed distance from the origin - throwing away the
/// magnitude L2 exists to compare. So a structure has to know, at insert time
/// and not only at query time, which of the two it was declared to minimise;
/// that is why this is threaded through as a field on `VectorSet` and
/// `IndexConfig` rather than decided only where a query is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Metric {
    /// One minus the cosine similarity of two unit vectors.
    #[default]
    Cosine,
    /// Euclidean distance, compared as its square (see [`squared_euclidean`]).
    L2,
}

impl Metric {
    /// Returns the distance between two vectors under this metric.
    /// @param a - one vector
    /// @param b - the other, the same width
    pub fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            Metric::Cosine => cosine_distance(a, b),
            Metric::L2 => squared_euclidean(a, b),
        }
    }

    /// Whether a vector must be normalized before this metric can compare it.
    ///
    /// Cosine needs it; L2 is what breaks if a vector gets it, because the
    /// magnitude L2 measures is exactly what normalizing discards.
    pub fn normalizes(self) -> bool {
        matches!(self, Metric::Cosine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_matches_naive_including_ragged_tail() {
        // 771 is deliberately not a multiple of 4, so the tail loop runs.
        for n in [1usize, 3, 4, 7, 768, 771] {
            let a: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin()).collect();
            let b: Vec<f32> = (0..n).map(|i| (i as f32 * 0.11).cos()).collect();
            let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
            let got = dot(&a, &b);
            assert!((naive - got).abs() < 1e-4, "n={n} naive={naive} got={got}");
        }
    }

    #[test]
    fn normalized_vector_has_unit_length() {
        let mut v: Vec<f32> = (0..768).map(|i| (i as f32 * 0.017).sin()).collect();
        normalize(&mut v);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn identical_vectors_have_zero_cosine_distance() {
        let mut v: Vec<f32> = (0..768).map(|i| (i as f32).cos()).collect();
        normalize(&mut v);
        assert!(cosine_distance(&v, &v).abs() < 1e-5);
    }

    #[test]
    fn zero_vector_survives_normalization() {
        let mut v = vec![0.0f32; 16];
        normalize(&mut v);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn truncation_renormalizes() {
        let mut v: Vec<f32> = (0..768).map(|i| (i as f32 * 0.03).sin()).collect();
        normalize(&mut v);
        for dims in [64usize, 128, 256, 512, 768] {
            let t = truncate_normalized(&v, dims);
            assert_eq!(t.len(), dims);
            assert!((dot(&t, &t) - 1.0).abs() < 1e-5, "dims={dims}");
        }
    }

    #[test]
    fn squared_euclidean_matches_the_naive_sum_of_squares() {
        for n in [1usize, 3, 4, 7, 768] {
            let a: Vec<f32> = (0..n).map(|i| (i as f32 * 0.37).sin()).collect();
            let b: Vec<f32> = (0..n).map(|i| (i as f32 * 0.11).cos()).collect();
            let naive: f32 = a.iter().zip(&b).map(|(x, y)| (x - y) * (x - y)).sum();
            let got = squared_euclidean(&a, &b);
            assert!((naive - got).abs() < 1e-4, "n={n} naive={naive} got={got}");
        }
    }

    /// The whole argument for skipping the square root: it cannot change which
    /// of two candidates is nearer, only the number that says so.
    #[test]
    fn squared_euclidean_orders_the_same_as_true_euclidean() {
        let query = [1.0f32, 0.0, 0.0, 0.0];
        let near = [0.9f32, 0.1, 0.0, 0.0];
        let far = [2.0f32, 0.0, 0.0, 0.0];
        let squared_near = squared_euclidean(&near, &query);
        let squared_far = squared_euclidean(&far, &query);
        let true_near = squared_near.sqrt();
        let true_far = squared_far.sqrt();
        assert!(squared_near < squared_far);
        assert!(true_near < true_far);
    }

    #[test]
    fn cosine_normalizes_and_l2_does_not() {
        assert!(Metric::Cosine.normalizes());
        assert!(!Metric::L2.normalizes());
        assert_eq!(Metric::default(), Metric::Cosine);
    }

    /// The disagreement the whole ticket is about: a vector that is perfectly
    /// aligned with the query but far from it in space is nearest by cosine and
    /// farthest by L2, over the same two candidates.
    #[test]
    fn cosine_and_l2_can_disagree_about_which_candidate_is_nearer() {
        let query = [1.0f32, 0.0, 0.0, 0.0];
        // Perfectly aligned with the query, but twice as long.
        let aligned_but_far = [2.0f32, 0.0, 0.0, 0.0];
        // Slightly off-axis, but close to the query in raw position.
        let close_but_off_axis = [0.9f32, 0.1, 0.0, 0.0];

        let mut aligned_unit = aligned_but_far;
        normalize(&mut aligned_unit);
        let mut off_axis_unit = close_but_off_axis;
        normalize(&mut off_axis_unit);
        assert!(
            Metric::Cosine.distance(&aligned_unit, &query)
                < Metric::Cosine.distance(&off_axis_unit, &query),
            "cosine should prefer the aligned vector"
        );
        assert!(
            Metric::L2.distance(&aligned_but_far, &query)
                > Metric::L2.distance(&close_but_off_axis, &query),
            "L2 should prefer the vector that is actually close"
        );
    }
}
