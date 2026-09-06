//! The vector set: one contiguous f32 buffer addressed by chunk identifier,
//! plus the optional int8 codes used for the quantized search pass.

use crate::distance::{dot, normalize};

/// What the graph traversal needs in order to compare a stored vector to a
/// query. Implemented over full precision vectors and over int8 codes, so the
/// same traversal code runs either way and the quantized pass is a real pass
/// rather than a relabelled one.
pub trait Scorer {
    fn distance(&self, id: u32, query: &[f32]) -> f32;
}

#[derive(Default)]
pub struct VectorSet {
    dims: usize,
    data: Vec<f32>,
}

impl VectorSet {
    pub fn new(dims: usize) -> Self {
        VectorSet {
            dims,
            data: Vec::new(),
        }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        if self.dims == 0 {
            0
        } else {
            self.data.len() / self.dims
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append a vector, normalizing it. Returns its ordinal, which callers keep
    /// aligned with chunk identifiers.
    pub fn push(&mut self, v: &[f32]) -> usize {
        assert_eq!(v.len(), self.dims, "vector width does not match the set");
        let id = self.len();
        let start = self.data.len();
        self.data.extend_from_slice(v);
        normalize(&mut self.data[start..]);
        id
    }

    #[inline]
    pub fn get(&self, id: u32) -> &[f32] {
        let s = id as usize * self.dims;
        &self.data[s..s + self.dims]
    }

    #[inline]
    pub fn similarity(&self, id: u32, query: &[f32]) -> f32 {
        dot(self.get(id), query)
    }

    /// Distance in the same orientation as pgvector's `<=>`: smaller is nearer.
    #[inline]
    pub fn distance(&self, id: u32, query: &[f32]) -> f32 {
        1.0 - self.similarity(id, query)
    }

    /// Cosine distance between two stored vectors, for the times a caller needs
    /// to know how similar two results are to each other rather than to a query.
    /// Diversity selection is the one that does.
    /// @param a - a chunk ordinal
    /// @param b - another chunk ordinal
    #[inline]
    pub fn distance_between(&self, a: u32, b: u32) -> f32 {
        1.0 - dot(self.get(a), self.get(b))
    }

    pub fn raw(&self) -> &[f32] {
        &self.data
    }

    /// Wrap an already normalized buffer, as read back from disk. Does not
    /// renormalize: the values were normalized when they were first inserted, and
    /// normalizing twice would be a second rounding step for no gain.
    pub fn from_raw(dims: usize, data: Vec<f32>) -> Self {
        assert!(dims > 0, "a vector set needs a positive width");
        assert_eq!(
            data.len() % dims,
            0,
            "buffer length is not a multiple of the width"
        );
        VectorSet { dims, data }
    }
}

impl Scorer for VectorSet {
    #[inline]
    fn distance(&self, id: u32, query: &[f32]) -> f32 {
        VectorSet::distance(self, id, query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_normalizes_and_indexes_by_ordinal() {
        let mut vs = VectorSet::new(4);
        assert_eq!(vs.push(&[2.0, 0.0, 0.0, 0.0]), 0);
        assert_eq!(vs.push(&[0.0, 3.0, 0.0, 0.0]), 1);
        assert_eq!(vs.len(), 2);
        assert_eq!(vs.get(0), &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(vs.get(1), &[0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn orthogonal_vectors_have_distance_one() {
        let mut vs = VectorSet::new(4);
        vs.push(&[1.0, 0.0, 0.0, 0.0]);
        assert!((vs.distance(0, &[0.0, 1.0, 0.0, 0.0]) - 1.0).abs() < 1e-6);
    }
}
