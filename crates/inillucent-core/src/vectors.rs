//! The vector set: N f32 per chunk, addressed by chunk identifier, plus the
//! optional int8 codes used for the quantized search pass.
//!
//! ## Two backings, and why the file one is the default
//!
//! A set of 601,862 chunks at 768 dimensions is **1.85 GB of f32**. Held on the
//! heap it is the single largest thing an index occupies, and it is resident for
//! as long as the process is - a cost paid by every process that opens the index,
//! whether or not it ever runs a semantic search.
//!
//! So a loaded set keeps its vectors **in the file they were read from** unless a
//! caller asks otherwise. Scoring reads them back through positional reads, which
//! the operating system serves out of its own page cache: the same bytes are
//! still in memory when they are being used, but they are in reclaimable cache
//! rather than in this process's heap, and a machine under pressure can take them
//! back. `IndexConfig::resident_vectors` turns the old behaviour back on for a
//! caller that would rather spend the memory.
//!
//! A set being **built** is always resident, because a build has just produced
//! the vectors and has nowhere else to put them. The file backing is what a
//! *load* produces.

use std::fs::File;
use std::io;

use crate::distance::{dot, normalize};

/// How many vectors one block of a streaming scan holds.
///
/// At 768 dimensions this is 8 MB a block, which is large enough that the read is
/// sequential work rather than syscall overhead and small enough that a scan
/// across every core holds a bounded amount at once.
const BLOCK_VECTORS: usize = 2_730;

/// What the graph traversal needs in order to compare a stored vector to a
/// query. Implemented over full precision vectors and over int8 codes, so the
/// same traversal code runs either way and the quantized pass is a real pass
/// rather than a relabelled one.
pub trait Scorer {
    fn distance(&self, id: u32, query: &[f32]) -> f32;
}

/// Where a set's vectors actually live.
enum Backing {
    /// One contiguous heap buffer. What a build produces, and what a load
    /// produces when the caller asked for resident vectors.
    Resident(Vec<f32>),
    /// The file the set was read from, positioned at the first vector, plus
    /// whatever has been appended since it was loaded.
    ///
    /// Read rather than mapped. A mapping would be one fewer copy, and it would
    /// also make the file undeletable while it is mapped, which the generation
    /// reclaim would then have to tolerate - and positional reads need nothing
    /// from the platform beyond what the standard library already offers.
    ///
    /// **The tail is why a filed index can still be appended to.** Nikaya adds
    /// chunks after every sync, and a set that refused an append would have to be
    /// read into memory at the first one, which is the cost this backing exists
    /// to avoid. Appended vectors are held on the heap until the index is saved,
    /// at which point they are written into the file and the next load has them
    /// filed like the rest. The heap holds what has arrived since the last save,
    /// not the corpus.
    Filed { file: File, offset: u64, count: usize, tail: Vec<f32> },
}

#[derive(Default)]
pub struct VectorSet {
    dims: usize,
    backing: Backing,
}

impl Default for Backing {
    fn default() -> Backing {
        Backing::Resident(Vec::new())
    }
}

/// Reads exactly `output.len()` bytes at `offset`, without moving a shared cursor.
///
/// The two platforms spell this differently and neither needs a crate for it. A
/// positional read is what lets several threads scan one file at once, which is
/// what the parallel exhaustive scan does.
///
/// @param file - the open file
/// @param offset - where to read from
/// @param output - where the bytes go
fn read_exact_at(file: &File, offset: u64, output: &mut [u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(output, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut at = offset;
        let mut written = 0usize;
        while written < output.len() {
            let read = file.seek_read(&mut output[written..], at)?;
            if read == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the vector file ended early"));
            }
            written += read;
            at += read as u64;
        }
        Ok(())
    }
}

/// Reads `count` vectors starting at ordinal `first` into `output`.
///
/// @param file - the vector file
/// @param offset - where the first vector of the whole set sits
/// @param dims - how wide a vector is
/// @param first - the ordinal to start at
/// @param output - a buffer of exactly `count * dims` floats
fn read_vectors(file: &File, offset: u64, dims: usize, first: u32, output: &mut [f32]) -> io::Result<()> {
    let at = offset + u64::from(first) * (dims as u64) * 4;
    read_exact_at(file, at, bytemuck::cast_slice_mut(output))
}

impl VectorSet {
    pub fn new(dims: usize) -> Self {
        VectorSet { dims, backing: Backing::Resident(Vec::new()) }
    }

    /// Wraps a file of already normalized vectors, without reading them.
    ///
    /// The bytes stay where they are. This is what a load produces by default,
    /// and it is why opening an index of this size costs megabytes rather than
    /// gigabytes.
    ///
    /// @param dims - how wide a vector is
    /// @param count - how many there are
    /// @param file - the file, open for reading
    /// @param offset - the byte offset of the first vector
    pub fn from_file(dims: usize, count: usize, file: File, offset: u64) -> Self {
        VectorSet { dims, backing: Backing::Filed { file, offset, count, tail: Vec::new() } }
    }

    /// Reports whether this set is holding its vectors on the heap.
    pub fn is_resident(&self) -> bool {
        matches!(self.backing, Backing::Resident(_))
    }

    /// How many bytes of heap this set occupies.
    ///
    /// Zero for a filed set, which is the point of one.
    pub fn heap_bytes(&self) -> usize {
        match &self.backing {
            Backing::Resident(data) => data.len() * 4,
            Backing::Filed { tail, .. } => tail.len() * 4,
        }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn len(&self) -> usize {
        match &self.backing {
            Backing::Resident(data) => data.len().checked_div(self.dims).unwrap_or(0),
            Backing::Filed { count, tail, .. } => {
                count + tail.len().checked_div(self.dims).unwrap_or(0)
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append a vector, normalizing it. Returns its ordinal, which callers keep
    /// aligned with chunk identifiers.
    ///
    /// A filed set appends onto its heap tail, which the next save folds into the file.
    pub fn push(&mut self, v: &[f32]) -> usize {
        assert_eq!(v.len(), self.dims, "vector width does not match the set");
        let dims = self.dims;
        match &mut self.backing {
            Backing::Resident(data) => {
                let id = data.len() / dims;
                let start = data.len();
                data.extend_from_slice(v);
                normalize(&mut data[start..]);
                id
            }
            Backing::Filed { count, tail, .. } => {
                let id = *count + tail.len() / dims;
                let start = tail.len();
                tail.extend_from_slice(v);
                normalize(&mut tail[start..]);
                id
            }
        }
    }

    /// Calls `visit` with one vector, however it is stored.
    ///
    /// **This is the accessor to write against.** `get` borrows out of the heap
    /// buffer and therefore only exists for a resident set; this one works either
    /// way, reading into a small stack of its own when the set is filed.
    ///
    /// @param id - the chunk ordinal
    /// @param visit - what to do with the vector
    #[inline]
    pub fn with<R>(&self, id: u32, visit: impl FnOnce(&[f32]) -> R) -> R {
        match &self.backing {
            Backing::Resident(data) => {
                let start = id as usize * self.dims;
                visit(&data[start..start + self.dims])
            }
            Backing::Filed { file, offset, count, tail } => {
                if (id as usize) >= *count {
                    let start = (id as usize - *count) * self.dims;
                    return visit(&tail[start..start + self.dims]);
                }
                let mut held = vec![0f32; self.dims];
                match read_vectors(file, *offset, self.dims, id, &mut held) {
                    Ok(()) => visit(&held),
                    // A vector file that cannot be read is a corrupt index, and a
                    // zero vector is at distance 1 from everything, so a failure
                    // here removes the chunk from the answer rather than
                    // producing a wrong one.
                    Err(_) => visit(&vec![0f32; self.dims]),
                }
            }
        }
    }

    /// Returns one vector out of a resident set, or nothing when the set is filed.
    ///
    /// **Nothing in this crate calls it outside its own tests.** It borrows out of
    /// the heap buffer, so it cannot answer for a filed set, and a version of it
    /// that panicked instead of returning `None` was a landmine: the int8 codes are
    /// derived at load rather than stored, so the first filed index that was opened
    /// re-encoded every vector through it and panicked. `with` and `copy_of` answer
    /// either way and are what the rest of the crate uses.
    ///
    /// @param id - the chunk ordinal
    #[inline]
    pub fn get(&self, id: u32) -> Option<&[f32]> {
        match &self.backing {
            Backing::Resident(data) => {
                let s = id as usize * self.dims;
                data.get(s..s + self.dims)
            }
            Backing::Filed { .. } => None,
        }
    }

    /// Returns one vector, copied, however the set is stored.
    ///
    /// @param id - the chunk ordinal
    pub fn copy_of(&self, id: u32) -> Vec<f32> {
        self.with(id, |v| v.to_vec())
    }

    #[inline]
    pub fn similarity(&self, id: u32, query: &[f32]) -> f32 {
        self.with(id, |v| dot(v, query))
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
        let left = self.copy_of(a);
        self.with(b, |right| 1.0 - dot(&left, right))
    }

    /// The whole buffer, for a resident set.
    ///
    /// `None` for a filed one, which has no buffer: a caller that wants to write
    /// the vectors out copies the file instead.
    pub fn raw(&self) -> Option<&[f32]> {
        match &self.backing {
            Backing::Resident(data) => Some(data),
            Backing::Filed { .. } => None,
        }
    }

    /// Writes every vector out, in ordinal order, however this set is stored.
    ///
    /// A resident set is one write of its buffer. A filed one is copied out of its
    /// file a block at a time and then followed by whatever was appended since it
    /// was loaded, which is what makes a saved index whole again.
    ///
    /// @param w - where the bytes go
    pub fn write_to(&self, w: &mut impl std::io::Write) -> io::Result<()> {
        match &self.backing {
            Backing::Resident(data) => w.write_all(bytemuck::cast_slice(data)),
            Backing::Filed { count, tail, .. } => {
                let mut block = vec![0f32; BLOCK_VECTORS * self.dims];
                let mut at = 0usize;
                while at < *count {
                    let read = self.read_block(at as u32, &mut block);
                    if read == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the vector file ended before the set did",
                        ));
                    }
                    w.write_all(bytemuck::cast_slice(&block[..read * self.dims]))?;
                    at += read;
                }
                w.write_all(bytemuck::cast_slice(tail))
            }
        }
    }

    /// Wrap an already normalized buffer, as read back from disk. Does not
    /// renormalize: the values were normalized when they were first inserted, and
    /// normalizing twice would be a second rounding step for no gain.
    pub fn from_raw(dims: usize, data: Vec<f32>) -> Self {
        assert!(dims > 0, "a vector set needs a positive width");
        assert_eq!(data.len() % dims, 0, "buffer length is not a multiple of the width");
        VectorSet { dims, backing: Backing::Resident(data) }
    }

    /// Reads this set into memory, so a caller that asked for resident vectors gets them.
    ///
    /// @param path - the file to read, when this set is filed
    pub fn make_resident(&mut self) -> io::Result<()> {
        let Backing::Filed { file, offset, count, tail } = &self.backing else {
            return Ok(());
        };
        let mut held = vec![0f32; count * self.dims + tail.len()];
        if *count > 0 {
            read_exact_at(file, *offset, bytemuck::cast_slice_mut(&mut held[..count * self.dims]))?;
        }
        held[count * self.dims..].copy_from_slice(tail);
        self.backing = Backing::Resident(held);
        Ok(())
    }

    /// How many vectors a streaming block holds.
    pub fn block_len(&self) -> usize {
        BLOCK_VECTORS
    }

    /// Reads one block of vectors into `output`, for a scan that walks the set in order.
    ///
    /// A filed set scored one vector at a time is one positional read per chunk,
    /// and an exhaustive scan over 601,862 of them is 601,862 of those. Reading
    /// 2,730 at a time makes the same scan sequential work.
    ///
    /// Returns how many vectors were read, which is short only at the end.
    ///
    /// @param first - the ordinal to start at
    /// @param output - a buffer of `block_len() * dims` floats
    pub fn read_block(&self, first: u32, output: &mut [f32]) -> usize {
        let remaining = self.len().saturating_sub(first as usize);
        let wanted = remaining.min(output.len() / self.dims);
        if wanted == 0 {
            return 0;
        }
        match &self.backing {
            Backing::Resident(data) => {
                let start = first as usize * self.dims;
                let end = start + wanted * self.dims;
                output[..wanted * self.dims].copy_from_slice(&data[start..end]);
                wanted
            }
            Backing::Filed { file, offset, count, tail } => {
                // A block never straddles the file and the tail: it is cut at the
                // boundary so each half is one contiguous copy.
                if (first as usize) >= *count {
                    let start = (first as usize - *count) * self.dims;
                    let take = wanted.min((tail.len() - start) / self.dims);
                    output[..take * self.dims].copy_from_slice(&tail[start..start + take * self.dims]);
                    return take;
                }
                let take = wanted.min(*count - first as usize);
                let slice = &mut output[..take * self.dims];
                match read_vectors(file, *offset, self.dims, first, slice) {
                    Ok(()) => take,
                    Err(_) => {
                        slice.fill(0.0);
                        take
                    }
                }
            }
        }
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
        assert_eq!(vs.copy_of(0), vec![1.0, 0.0, 0.0, 0.0]);
        assert_eq!(vs.copy_of(1), vec![0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn orthogonal_vectors_have_distance_one() {
        let mut vs = VectorSet::new(4);
        vs.push(&[1.0, 0.0, 0.0, 0.0]);
        assert!((vs.distance(0, &[0.0, 1.0, 0.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    /// A filed set answers exactly what the resident one it was written from does.
    ///
    /// **The claim is equality, not closeness.** Both backings hold the same
    /// little endian f32, so a distance computed either way is the same float, and
    /// a test with a tolerance in it would pass over a backing that had quietly
    /// rounded or reordered something.
    #[test]
    fn a_filed_set_answers_what_the_resident_one_answers() {
        let dims = 16usize;
        let count = 500usize;
        let mut resident = VectorSet::new(dims);
        for id in 0..count {
            let held: Vec<f32> = (0..dims).map(|d| ((id * 31 + d * 7) % 97) as f32 - 48.0).collect();
            resident.push(&held);
        }

        let directory = std::env::temp_dir().join(format!("inillucent-filed-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        let path = directory.join("vectors.raw");
        let raw = resident.raw().expect("a resident set has a buffer");
        std::fs::write(&path, bytemuck::cast_slice(raw)).expect("the vectors are written");
        let filed = VectorSet::from_file(dims, count, File::open(&path).expect("it opens"), 0);

        assert_eq!(filed.len(), resident.len());
        assert!(!filed.is_resident());
        assert_eq!(filed.heap_bytes(), 0);
        assert_eq!(resident.heap_bytes(), count * dims * 4);

        let query: Vec<f32> = (0..dims).map(|d| (d as f32) - 8.0).collect();
        for id in [0u32, 1, 17, 249, (count - 1) as u32] {
            assert_eq!(
                filed.distance(id, &query),
                resident.distance(id, &query),
                "vector {id} scores the same from the file as from the heap"
            );
            assert_eq!(filed.copy_of(id), resident.copy_of(id));
        }
        assert_eq!(filed.distance_between(3, 400), resident.distance_between(3, 400));

        // A block read produces the same floats as the buffer it was written from.
        let mut block = vec![0f32; filed.block_len() * dims];
        let read = filed.read_block(0, &mut block);
        assert_eq!(read, count, "the whole set fits in one block here");
        assert_eq!(&block[..count * dims], resident.raw().expect("resident"));

        // A block that starts past the middle is short and still correct.
        let read = filed.read_block(490, &mut block);
        assert_eq!(read, 10);
        assert_eq!(&block[..10 * dims], &resident.raw().expect("resident")[490 * dims..]);

        let mut promoted = VectorSet::from_file(dims, count, File::open(&path).expect("it opens"), 0);
        promoted.make_resident().expect("it reads");
        assert!(promoted.is_resident());
        assert_eq!(promoted.raw().expect("resident"), resident.raw().expect("resident"));

        let _ = std::fs::remove_file(&path);
    }
}
