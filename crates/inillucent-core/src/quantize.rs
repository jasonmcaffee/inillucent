//! int8 scalar quantization with rescoring.
//!
//! Qdrant's published results say int8 costs under 1% accuracy for a 4x memory
//! reduction, and that binary quantization needs high dimensionality: their
//! documented recall figures are for 1536 and 4096 dimensional models, and they
//! report material degradation below roughly 1000 dimensions. nomic is 768, so
//! binary is the wrong default here and int8 is the right one. The score card
//! reports the measured cost rather than repeating the vendor number.
//!
//! One scale per vector, symmetric. The vectors are already L2 normalized so no
//! component exceeds 1, and a per vector scale of `max(|component|) / 127` uses
//! the full int8 range without clipping.
//!
//! Invariant: **a quantized pass narrows the candidate set and never decides
//! the answer.** The int8 codes are a filter over which full vectors are worth
//! reading; the ranking that is returned is always computed from the f32
//! vectors, so the quantization costs speed and not correctness.

use crate::vectors::{Scorer, VectorSet};

/// One int8 code per component per vector, with a per-vector scale.
pub struct QuantizedSet {
    dims: usize,
    codes: Vec<i8>,
    scales: Vec<f32>,
}

impl QuantizedSet {
    /// Quantizes a whole vector set.
    ///
    /// @param vectors - the full-precision set
    pub fn from_vectors(vectors: &VectorSet) -> QuantizedSet {
        let mut set = QuantizedSet {
            dims: vectors.dims(),
            codes: Vec::new(),
            scales: Vec::new(),
        };
        set.encode_from(vectors, 0);
        set
    }

    /// Encode every vector from `first` onward, appending to the codes already
    /// here.
    ///
    /// A code is a function of its own vector and nothing else - one scale per
    /// vector, no shared codebook - so appending changes no existing code. That is
    /// what makes the quantized pass appendable at all.
    /// @param vectors - the full vector set, including the ones already encoded
    /// @param first - the ordinal to start at
    pub fn encode_from(&mut self, vectors: &VectorSet, first: usize) {
        let dims = self.dims;
        let n = vectors.len();
        self.codes.resize(n * dims, 0);
        self.scales.resize(n, 1.0);

        // **Read in blocks, because this pass runs on a set that may be in a file.**
        // The codes are derived rather than stored, so opening an index re-encodes
        // every vector - 601,862 of them on a real corpus. One positional read each
        // would be 601,862 reads; a block is one read per 2,730.
        let block = vectors.block_len();
        let mut buffer = vec![0f32; block * dims];
        let mut id = first;
        while id < n {
            let read = vectors.read_block(id as u32, &mut buffer);
            if read == 0 {
                break;
            }
            // `chunks_exact` rather than a pair of indexes: the buffer was
            // sized `block * dims`, so the windows are exactly the vectors
            // (task-1932, H9).
            for (nth, v) in buffer.chunks_exact(dims).take(read).enumerate() {
                let peak = v.iter().fold(0f32, |acc, x| acc.max(x.abs()));
                let scale = if peak > 0.0 { peak / 127.0 } else { 1.0 };
                let at = id.saturating_add(nth);
                if let Some(slot) = self.scales.get_mut(at) {
                    *slot = scale;
                }
                let base = at.saturating_mul(dims);
                for (d, x) in v.iter().enumerate() {
                    // round, then clamp, so a value exactly at the peak lands on 127
                    // rather than overflowing to -128.
                    let q = (x / scale).round().clamp(-127.0, 127.0);
                    if let Some(slot) = self.codes.get_mut(base.saturating_add(d)) {
                        *slot = q as i8;
                    }
                }
            }
            id += read;
        }
    }

    /// Returns how many vectors are quantized.
    pub fn len(&self) -> usize {
        self.scales.len()
    }

    /// Reports whether nothing is quantized.
    pub fn is_empty(&self) -> bool {
        self.scales.is_empty()
    }

    /// Returns how much memory the codes and scales occupy, which is what the
    /// score card reports against the full-precision set.
    pub fn bytes(&self) -> usize {
        self.codes.len() + self.scales.len() * 4
    }

    /// Returns one vector's codes, empty for an identifier this set does not
    /// hold.
    ///
    /// @param id - the chunk identifier
    #[inline]
    fn code(&self, id: u32) -> &[i8] {
        let s = (id as usize).saturating_mul(self.dims);
        self.codes
            .get(s..s.saturating_add(self.dims))
            .unwrap_or(&[])
    }

    /// Approximate similarity between a stored vector and a full precision query.
    ///
    /// The query stays in f32. Quantizing it too would add a second error term
    /// for no memory saving, since there is only ever one query vector resident.
    #[inline]
    pub fn similarity(&self, id: u32, query: &[f32]) -> f32 {
        let code = self.code(id);
        let Some(scale) = self.scales.get(id as usize).copied() else {
            return 0.0;
        };
        let mut s0 = 0f32;
        let mut s1 = 0f32;
        let mut s2 = 0f32;
        let mut s3 = 0f32;
        // The same four accumulator chains `distance::dot` uses, and the same
        // reason for `chunks_exact`: a fixed-width window has no bound to check,
        // which is what lets the compiler emit SIMD (task-1932, H9).
        for (c, q) in code.chunks_exact(4).zip(query.chunks_exact(4)) {
            let [c0, c1, c2, c3] = *<&[i8; 4]>::try_from(c).unwrap_or(&[0; 4]);
            let [q0, q1, q2, q3] = *<&[f32; 4]>::try_from(q).unwrap_or(&[0.0; 4]);
            s0 += f32::from(c0) * q0;
            s1 += f32::from(c1) * q1;
            s2 += f32::from(c2) * q2;
            s3 += f32::from(c3) * q3;
        }
        let mut acc = (s0 + s1) + (s2 + s3);
        for (c, q) in code
            .chunks_exact(4)
            .remainder()
            .iter()
            .zip(query.chunks_exact(4).remainder())
        {
            acc += f32::from(*c) * q;
        }
        acc * scale
    }

    #[inline]
    /// Returns the approximate distance between one code and a query.
    ///
    /// Approximate is the whole point: this narrows the candidate set, and the
    /// ranking that is returned is always recomputed from the full vectors.
    ///
    /// @param id - the chunk identifier
    /// @param query - the query vector
    pub fn distance(&self, id: u32, query: &[f32]) -> f32 {
        1.0 - self.similarity(id, query)
    }
}

impl Scorer for QuantizedSet {
    #[inline]
    fn distance(&self, id: u32, query: &[f32]) -> f32 {
        QuantizedSet::distance(self, id, query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance::normalize;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn random_set(n: usize, dims: usize) -> VectorSet {
        let mut rng = StdRng::seed_from_u64(7);
        let mut vs = VectorSet::new(dims);
        for _ in 0..n {
            let mut v: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
            normalize(&mut v);
            vs.push(&v);
        }
        vs
    }

    #[test]
    fn quantized_similarity_tracks_full_precision_similarity() {
        let vs = random_set(500, 768);
        let q = QuantizedSet::from_vectors(&vs);
        let query = vs.copy_of(0);
        let mut worst = 0f32;
        for id in 0..vs.len() as u32 {
            let exact = vs.similarity(id, &query);
            let approx = q.similarity(id, &query);
            worst = worst.max((exact - approx).abs());
        }
        assert!(worst < 0.01, "worst similarity error was {worst}");
    }

    #[test]
    fn memory_is_a_quarter_of_full_precision() {
        let vs = random_set(1000, 768);
        let q = QuantizedSet::from_vectors(&vs);
        let full = vs.heap_bytes();
        let ratio = full as f32 / q.bytes() as f32;
        assert!(ratio > 3.9, "compression ratio was only {ratio}");
    }

    #[test]
    fn the_top_ranked_vector_is_unchanged_for_its_own_query() {
        let vs = random_set(300, 768);
        let q = QuantizedSet::from_vectors(&vs);
        for id in [0u32, 42, 299] {
            let query = vs.copy_of(id);
            let best = (0..vs.len() as u32)
                .max_by(|a, b| {
                    q.similarity(*a, &query)
                        .partial_cmp(&q.similarity(*b, &query))
                        .unwrap()
                })
                .unwrap();
            assert_eq!(best, id);
        }
    }

    #[test]
    fn ranking_agrees_with_full_precision_on_the_top_ten() {
        let vs = random_set(2000, 768);
        let q = QuantizedSet::from_vectors(&vs);
        let query = vs.copy_of(11);

        let mut exact: Vec<u32> = (0..vs.len() as u32).collect();
        exact.sort_by(|a, b| {
            vs.distance(*a, &query)
                .partial_cmp(&vs.distance(*b, &query))
                .unwrap()
        });
        let mut approx: Vec<u32> = (0..vs.len() as u32).collect();
        approx.sort_by(|a, b| {
            q.distance(*a, &query)
                .partial_cmp(&q.distance(*b, &query))
                .unwrap()
        });

        let want: std::collections::HashSet<u32> = exact[..10].iter().copied().collect();
        let overlap = approx[..10].iter().filter(|c| want.contains(c)).count();
        assert!(
            overlap >= 9,
            "only {overlap} of the top 10 survived quantization"
        );
    }

    #[test]
    fn an_empty_set_quantizes_to_nothing() {
        let vs = VectorSet::new(768);
        let q = QuantizedSet::from_vectors(&vs);
        assert!(q.is_empty());
    }

    #[test]
    fn a_zero_vector_does_not_produce_nan() {
        let mut vs = VectorSet::new(8);
        vs.push(&[0.0; 8]);
        let q = QuantizedSet::from_vectors(&vs);
        assert!(q.similarity(0, &[1.0; 8]).is_finite());
    }
}
