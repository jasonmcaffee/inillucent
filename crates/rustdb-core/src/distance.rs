//! Cosine similarity over L2 normalized vectors, which reduces to a dot product.
//!
//! Vectors are normalized once at insert time, so nothing here divides or takes a
//! square root. The four independent accumulators matter more than they look: a
//! single accumulator serializes the whole loop on the latency of floating point
//! addition, because each add depends on the previous one. Four chains let the
//! CPU keep several adds in flight and let LLVM emit SIMD.

/// Dot product of two equal length slices.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let n = a.len();
    let chunks = n / 4;

    let mut s0 = 0.0f32;
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    let mut s3 = 0.0f32;

    for i in 0..chunks {
        let j = i * 4;
        // Bounds checks are hoisted by the slice reborrow below.
        let (x, y) = (&a[j..j + 4], &b[j..j + 4]);
        s0 += x[0] * y[0];
        s1 += x[1] * y[1];
        s2 += x[2] * y[2];
        s3 += x[3] * y[3];
    }

    let mut acc = (s0 + s1) + (s2 + s3);
    for i in (chunks * 4)..n {
        acc += a[i] * b[i];
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
    let mut out = v[..dims.min(v.len())].to_vec();
    normalize(&mut out);
    out
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
            assert!(
                (naive - got).abs() < 1e-4,
                "n={n} naive={naive} got={got}"
            );
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
}
