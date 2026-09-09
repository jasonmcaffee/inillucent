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

use crate::vectors::{Scorer, VectorSet};

pub struct QuantizedSet {
    dims: usize,
    codes: Vec<i8>,
    scales: Vec<f32>,
}

impl QuantizedSet {
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
            for nth in 0..read {
                let v = &buffer[nth * dims..(nth + 1) * dims];
                let peak = v.iter().fold(0f32, |acc, x| acc.max(x.abs()));
                let scale = if peak > 0.0 { peak / 127.0 } else { 1.0 };
                self.scales[id + nth] = scale;
                let base = (id + nth) * dims;
                for (d, x) in v.iter().enumerate() {
                    // round, then clamp, so a value exactly at the peak lands on 127
                    // rather than overflowing to -128.
                    let q = (x / scale).round().clamp(-127.0, 127.0);
                    self.codes[base + d] = q as i8;
                }
            }
            id += read;
        }
    }

    pub fn len(&self) -> usize {
        self.scales.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scales.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.codes.len() + self.scales.len() * 4
    }

    #[inline]
    fn code(&self, id: u32) -> &[i8] {
        let s = id as usize * self.dims;
        &self.codes[s..s + self.dims]
    }

    /// Approximate similarity between a stored vector and a full precision query.
    ///
    /// The query stays in f32. Quantizing it too would add a second error term
    /// for no memory saving, since there is only ever one query vector resident.
    #[inline]
    pub fn similarity(&self, id: u32, query: &[f32]) -> f32 {
        let code = self.code(id);
        let scale = self.scales[id as usize];
        let mut s0 = 0f32;
        let mut s1 = 0f32;
        let mut s2 = 0f32;
        let mut s3 = 0f32;
        let chunks = self.dims / 4;
        for i in 0..chunks {
            let j = i * 4;
            s0 += code[j] as f32 * query[j];
            s1 += code[j + 1] as f32 * query[j + 1];
            s2 += code[j + 2] as f32 * query[j + 2];
            s3 += code[j + 3] as f32 * query[j + 3];
        }
        let mut acc = (s0 + s1) + (s2 + s3);
        for j in (chunks * 4)..self.dims {
            acc += code[j] as f32 * query[j];
        }
        acc * scale
    }

    #[inline]
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
        assert!(overlap >= 9, "only {overlap} of the top 10 survived quantization");
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
