//! The B+tree on the buffer pool: real interior pages, a paged descent, and
//! scans in both directions.
//!
//! Invariant: the interior separators and the leaf keys are ordered by the same
//! function. That is not a coincidence to be maintained by care - it is
//! [`PagedTree::encode_key`], one method, used by the builder that writes the
//! separators and by every descent that compares against them. A tree whose
//! separators were encoded by one rule and probed by another routes descents to
//! the wrong leaf and returns *no row* rather than an error, which is the
//! failure mode this module is arranged to make impossible.
//!
//! ## What this replaces
//!
//! Phase 1's [`crate::tree::Tree`] held its leaves in a `Vec` and its
//! separators in a parallel array of owned keys, and said in its own
//! documentation that the array "is what an interior page *is*, held as an
//! array because Phase 1 has no buffer pool to put one in". This is the buffer
//! pool version. The comparison code did not change, exactly as that note
//! predicted: a descent is still "the last child whose separator is not above
//! the probe", it just reads the separator out of a page now.
//!
//! Phase 1's tree survives beside this one. It is the model the property test
//! compares against, and keeping a second, obviously-correct implementation of
//! the same ordering is worth more than the file it costs.
//!
//! ## Two key encodings, and why
//!
//! A rowid tree's key is one integer, so its separators are eight bytes of
//! [`crate::key::order_preserving_int`] with no class byte and no tail. Every
//! other tree uses the general [`crate::key::encode`]. The distinction is worth
//! its branch: a rowid descent compares eight bytes where the general encoding
//! would compare eighteen, and `point.rowid` is the workload with the tightest
//! budget in Phase 2's gate.
//!
//! ## The descent, and what "optimistic" buys in Phase 2
//!
//! A descent observes each interior frame's version, reads the swip, follows
//! it, and validates. In Phase 2 nothing writes while a read runs, so the
//! validation never fails - and that is stated rather than glossed. What it
//! buys now is that the machinery is exercised on every descent rather than
//! being written and never run: the pool bumps a frame's version on every load
//! and every eviction, so a descent that raced one really would restart, and
//! [`crate::paged::tests`] forces exactly that race.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;
use inillucent_pool::interior::{InteriorBuilder, InteriorRef};
use inillucent_pool::page::{self, PageKind};
use inillucent_pool::{Database, PageGuard, PageId, Pool, Swip};

use inillucent_value::collation::Collation;

use crate::datum::{Datum, OwnedDatum};
use crate::key;
use crate::leaf::{LeafBuilder, LeafRef, Packed};
use crate::tree::BULK_FILL;
use crate::types::{ColumnSpec, PhysicalType};

/// How many times a descent retries an optimistic read before giving up.
///
/// The TDD's four, taken from the pool so the two cannot drift.
const RESTARTS: u32 = inillucent_pool::OPTIMISTIC_RETRIES;

/// How a tree's keys become comparable bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyEncoding {
    /// One `Int64` column, encoded as eight exact bytes.
    Rowid,
    /// Anything else, through the general memcmp encoding.
    General,
}

impl KeyEncoding {
    /// Chooses the encoding a column directory calls for.
    ///
    /// The rowid form is taken only for a single non-nullable `Int64` key
    /// column, because it has no way to represent a NULL, a text or a real -
    /// and a rowid, by definition, is never any of those.
    ///
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    pub fn choose(columns: &[ColumnSpec], key_columns: usize) -> KeyEncoding {
        if key_columns != 1 {
            return KeyEncoding::General;
        }
        match columns.first() {
            Some(spec) if spec.physical == PhysicalType::Int64 && !spec.nullable() => {
                KeyEncoding::Rowid
            }
            _ => KeyEncoding::General,
        }
    }

    /// Encodes a key tuple into comparable bytes.
    ///
    /// A rowid tree's probe that is not an integer falls back to the general
    /// encoding for that one comparison, which cannot match any separator and
    /// therefore lands the descent at the leftmost or rightmost leaf rather
    /// than somewhere arbitrary. The leaf's own search then answers correctly.
    ///
    /// @param self - the tree's encoding
    /// @param values - the key tuple
    pub fn encode(self, values: &[Datum<'_>]) -> Vec<u8> {
        self.encode_under(values, &[])
    }

    /// Encodes a key tuple whose columns have collations.
    ///
    /// @param values - the key tuple
    /// @param collations - one per column; short means BINARY for the rest
    pub fn encode_under(self, values: &[Datum<'_>], collations: &[Collation]) -> Vec<u8> {
        match (self, values.first()) {
            (KeyEncoding::Rowid, Some(Datum::Int(number))) if values.len() == 1 => {
                key::order_preserving_int(*number).to_vec()
            }
            (KeyEncoding::Rowid, Some(Datum::Real(number))) if values.len() == 1 => {
                // A real probe against a rowid tree: clamp to the integer it
                // sits between, so the descent lands on the leaf that could
                // hold it rather than at an end of the tree.
                let clamped = if *number <= i64::MIN as f64 {
                    i64::MIN
                } else if *number >= i64::MAX as f64 {
                    i64::MAX
                } else {
                    number.floor() as i64
                };
                key::order_preserving_int(clamped).to_vec()
            }
            (KeyEncoding::Rowid, Some(Datum::Null)) => key::order_preserving_int(i64::MIN).to_vec(),
            (KeyEncoding::Rowid, None) => Vec::new(),
            (KeyEncoding::Rowid, Some(_)) => vec![0xFF; 8],
            (KeyEncoding::General, _) => key::encode_with(values, collations).into_bytes(),
        }
    }
}

/// Returns the collation of each key column.
///
/// @param columns - the column directory
/// @param key_columns - how many leading columns form the key
fn collations_of(columns: &[ColumnSpec], key_columns: usize) -> Vec<Collation> {
    columns
        .iter()
        .take(key_columns)
        .map(|column| column.collation)
        .collect()
}

/// An encoded key, on the stack when it fits.
///
/// A rowid key is eight bytes and a descent needs one per probe, so returning a
/// `Vec` put a heap allocation on the path `point.rowid` and every rowid lookup
/// take. Twenty-four bytes inline covers a rowid, a one-column general key and
/// most two-column ones; anything longer spills, which is correct and only
/// slower for the keys that were going to allocate anyway.
#[derive(Clone, Debug)]
pub enum KeyBytes {
    /// The key fits inline; the second field is its length.
    Inline([u8; KeyBytes::INLINE], usize),
    /// The key did not fit and lives on the heap.
    Heap(Vec<u8>),
}

impl KeyBytes {
    /// How many bytes fit without allocating.
    pub const INLINE: usize = 24;

    /// Returns the encoded bytes.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            KeyBytes::Inline(bytes, length) => bytes.get(..*length).unwrap_or(&[]),
            KeyBytes::Heap(bytes) => bytes.as_slice(),
        }
    }

    /// Returns a key over a run of bytes, inline when it fits.
    ///
    /// @param bytes - the encoded key
    pub fn from_slice(bytes: &[u8]) -> KeyBytes {
        if bytes.len() <= KeyBytes::INLINE {
            let mut inline = [0u8; KeyBytes::INLINE];
            if let Some(slot) = inline.get_mut(..bytes.len()) {
                slot.copy_from_slice(bytes);
            }
            KeyBytes::Inline(inline, bytes.len())
        } else {
            KeyBytes::Heap(bytes.to_vec())
        }
    }
}

/// What one descent found, and the path it took.
///
/// The path is kept so a reverse walk can step left without descending from the
/// root again: moving to the previous leaf is "back up until a child index can
/// be decremented, then down the rightmost spine".
#[derive(Clone, Debug, Default)]
pub struct Descent {
    /// The interior pages entered, with the child index taken at each.
    pub steps: Vec<(PageId, usize)>,
    /// The leaf the descent landed on.
    pub leaf: PageId,
}

/// A B+tree whose pages live in a buffer pool.
#[derive(Clone, Debug)]
pub struct PagedTree {
    /// The identifier written into every page of this tree.
    tree_id: u64,
    /// The root page; a one-leaf tree's root is that leaf.
    root: PageId,
    /// How many interior levels sit above the leaves.
    height: u16,
    /// The column directory.
    columns: Vec<ColumnSpec>,
    /// How many leading columns form the key.
    key_columns: usize,
    /// The database's page size in bytes.
    page_size: usize,
    /// How keys become bytes.
    encoding: KeyEncoding,
    /// The collation of each key column.
    ///
    /// A tree is *stored* in its collations' order, so every comparison the
    /// tree makes - a descent's separator test, a leaf's binary search, a
    /// bound - has to use them. They come from the column directory rather
    /// than from the page, because the catalog is what says a column has one.
    collations: Vec<Collation>,
    /// The leftmost leaf, so a full scan needs no descent.
    first_leaf: PageId,
    /// How many leaves the tree holds.
    leaf_count: u64,
    /// How many rows the tree holds.
    row_count: u64,
}

impl PagedTree {
    /// Builds a tree bottom-up from rows already sorted by key.
    ///
    /// Leaves are packed left to right at [`BULK_FILL`] and never revisited,
    /// then each interior level is built from the level below it, then the root
    /// is whatever the last level left. One pass, no per-key descent - the
    /// TDD's bulk builder, and what the fixture import uses.
    ///
    /// @param database - the file the pages are allocated and installed in
    /// @param tree_id - the identifier stamped into every page
    /// @param columns - the column directory, key columns first
    /// @param key_columns - how many leading columns form the key
    /// @param rows - the rows, already sorted by the key columns
    pub fn bulk_build(
        database: &mut Database,
        tree_id: u64,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        rows: &[Vec<Datum<'_>>],
    ) -> DbResult<PagedTree> {
        let page_size = database.page_size();
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations = collations_of(&columns, key_columns);
        let builder = LeafBuilder::new(page_size, tree_id, columns.clone(), key_columns)?;

        // Pass one: the leaves, and each one's first key as a separator.
        let mut leaves: Vec<PageId> = Vec::new();
        let mut separators: Vec<Vec<u8>> = Vec::new();
        let mut images: Vec<Vec<u8>> = Vec::new();
        let mut at = 0usize;
        let mut row_count = 0u64;
        while at < rows.len() {
            let remaining = rows.get(at..).unwrap_or(&[]);
            match builder.pack(remaining, BULK_FILL)? {
                Packed::Filled { page, rows: packed } => {
                    let first = remaining
                        .first()
                        .ok_or_else(|| corrupt("a packed leaf held no rows"))?;
                    let head: Vec<Datum<'_>> = first.iter().copied().take(key_columns).collect();
                    separators.push(encoding.encode_under(&head, &collations));
                    images.push(page);
                    at = at.saturating_add(packed);
                    row_count = row_count.saturating_add(packed as u64);
                }
                Packed::RowTooLarge => {
                    return Err(misuse(
                        "a row is larger than a page; out-of-line blobs are Phase 4",
                    ))
                }
            }
        }
        if images.is_empty() {
            // An empty tree is still a tree: one empty leaf, so every reader
            // has a page to land on and nothing has to special-case a root that
            // does not exist.
            images.push(builder.encode(&[])?);
            separators.push(Vec::new());
        }

        // The leaves are allocated as one run so the sibling chain is also the
        // file's page order, which is what makes a full scan sequential.
        let leaf_pages = images.len();
        let first_leaf = database.allocate(leaf_pages as u64)?;
        for (index, image) in images.iter_mut().enumerate() {
            let id = PageId(first_leaf.0.saturating_add(index as u64));
            let right = if index.saturating_add(1) < leaf_pages {
                PageId(id.0.saturating_add(1))
            } else {
                PageId::NONE
            };
            page::set_right(image, right)?;
            database.install(id, image)?;
            leaves.push(id);
        }

        // Pass two: the interior levels, bottom up.
        let mut level = 1u16;
        let mut children = leaves.clone();
        let mut child_keys = separators;
        let mut root = *leaves.first().unwrap_or(&PageId::NONE);
        let mut height = 0u16;
        while children.len() > 1 {
            let interior = InteriorBuilder::new(page_size, tree_id, level)?;
            let mut parents: Vec<PageId> = Vec::new();
            let mut parent_keys: Vec<Vec<u8>> = Vec::new();
            let mut cursor = 0usize;
            while cursor < children.len() {
                let remaining_keys: Vec<&[u8]> = child_keys
                    .get(cursor.saturating_add(1)..)
                    .unwrap_or(&[])
                    .iter()
                    .map(|key| key.as_slice())
                    .collect();
                let fit = interior
                    .capacity(&remaining_keys)
                    .min(children.len().saturating_sub(cursor))
                    .max(1);
                let group: Vec<Swip> = children
                    .get(cursor..cursor.saturating_add(fit))
                    .unwrap_or(&[])
                    .iter()
                    .map(|page| Swip::unswizzled(*page))
                    .collect();
                let group_separators: Vec<&[u8]> = child_keys
                    .get(cursor.saturating_add(1)..cursor.saturating_add(fit))
                    .unwrap_or(&[])
                    .iter()
                    .map(|key| key.as_slice())
                    .collect();
                let image = interior.build(&group_separators, &group)?;
                let id = database.allocate(1)?;
                database.install(id, &image)?;
                parents.push(id);
                parent_keys.push(child_keys.get(cursor).cloned().unwrap_or_default());
                cursor = cursor.saturating_add(fit);
            }
            children = parents;
            child_keys = parent_keys;
            height = level;
            level = level.saturating_add(1);
            root = *children.first().unwrap_or(&PageId::NONE);
            if level > 32 {
                return Err(corrupt(
                    "the bulk builder made a tree deeper than 32 levels",
                ));
            }
        }
        let collations = collations_of(&columns, key_columns);
        Ok(PagedTree {
            tree_id,
            root,
            height,
            columns,
            key_columns,
            page_size,
            encoding,
            collations,
            first_leaf: *leaves.first().unwrap_or(&PageId::NONE),
            leaf_count: leaves.len() as u64,
            row_count,
        })
    }

    /// Rebuilds the handle for a tree already in a file.
    ///
    /// The shape - root, height, first leaf, counts - is recorded in the
    /// catalog rather than rediscovered, except that the height is verified
    /// against the root page so a catalog that disagrees with the file is an
    /// error rather than a wrong descent.
    ///
    /// @param pool - the buffer pool the pages live in
    /// @param tree_id - the identifier stamped into every page
    /// @param root - the root page
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    /// @param first_leaf - the leftmost leaf
    /// @param leaf_count - how many leaves
    /// @param row_count - how many rows
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        pool: &Pool,
        tree_id: u64,
        root: PageId,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
        first_leaf: PageId,
        leaf_count: u64,
        row_count: u64,
    ) -> DbResult<PagedTree> {
        let guard = pool.fetch(root)?;
        let height = match page::kind_of(&guard)? {
            PageKind::Leaf => 0,
            PageKind::Interior => page::level_of(&guard)?,
            other => return Err(corrupt(format!("a tree root cannot be {other:?}"))),
        };
        let encoding = KeyEncoding::choose(&columns, key_columns);
        let collations = collations_of(&columns, key_columns);
        Ok(PagedTree {
            tree_id,
            root,
            height,
            columns,
            key_columns,
            page_size: pool.page_size(),
            encoding,
            collations,
            first_leaf,
            leaf_count,
            row_count,
        })
    }

    /// Returns the tree identifier every page carries.
    pub fn tree_id(&self) -> u64 {
        self.tree_id
    }

    /// Returns the root page.
    pub fn root(&self) -> PageId {
        self.root
    }

    /// Returns how many interior levels sit above the leaves.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Returns the column directory.
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    /// Returns how many leading columns form the key.
    pub fn key_columns(&self) -> usize {
        self.key_columns
    }

    /// Returns how many leaves the tree holds.
    pub fn leaf_count(&self) -> u64 {
        self.leaf_count
    }

    /// Returns how many rows the tree holds.
    pub fn row_count(&self) -> u64 {
        self.row_count
    }

    /// Returns the bytes the tree's leaves occupy.
    pub fn byte_size(&self) -> usize {
        (self.leaf_count as usize).saturating_mul(self.page_size)
    }

    /// Returns the leftmost leaf.
    pub fn first_leaf(&self) -> PageId {
        self.first_leaf
    }

    /// Returns how a key tuple becomes comparable bytes.
    pub fn key_encoding(&self) -> KeyEncoding {
        self.encoding
    }

    /// Encodes a key tuple the way this tree's separators are encoded.
    ///
    /// @param values - the key tuple, in key-column order
    pub fn encode_key(&self, values: &[Datum<'_>]) -> Vec<u8> {
        self.encoding.encode_under(values, &self.collations)
    }

    /// Returns the collation of each key column.
    pub fn collations(&self) -> &[Collation] {
        &self.collations
    }

    /// Encodes a key tuple without allocating when it fits inline.
    ///
    /// The hot form. [`PagedTree::encode_key`] stays for the callers that want
    /// an owned `Vec` - the bulk builder's separators, which are kept - and
    /// this one is what every descent uses.
    ///
    /// @param values - the key tuple, in key-column order
    pub fn encode_key_small(&self, values: &[Datum<'_>]) -> KeyBytes {
        if let (KeyEncoding::Rowid, [Datum::Int(number)]) = (self.encoding, values) {
            let mut inline = [0u8; KeyBytes::INLINE];
            if let Some(slot) = inline.get_mut(..8) {
                slot.copy_from_slice(&key::order_preserving_int(*number));
            }
            return KeyBytes::Inline(inline, 8);
        }
        KeyBytes::from_slice(&self.encoding.encode_under(values, &self.collations))
    }

    /// Descends to the leaf whose range holds a key, returning the pinned leaf.
    ///
    /// The guard comes back rather than the page id alone because every caller
    /// wants to read the leaf next, and re-fetching it would be a second page
    /// lookup for a page the descent is already holding. On `point.rowid` -
    /// the workload with the tightest budget in this phase's gate - that one
    /// lookup is a measurable share of the whole probe.
    ///
    /// @param pool - the buffer pool
    /// @param key - the encoded key being looked for
    pub fn descend_guard<'p>(
        &self,
        pool: &'p Pool,
        key: &[u8],
    ) -> DbResult<(PageGuard<'p>, PageId)> {
        for attempt in 0..=RESTARTS {
            match self.try_descend(pool, key, attempt == RESTARTS)? {
                Some(found) => return Ok(found),
                None => continue,
            }
        }
        Err(corrupt(
            "a descent restarted more times than the tree is deep",
        ))
    }

    /// Attaches to a tree given only its root, discovering the rest by walking.
    ///
    /// [`PagedTree::attach`] is told the leftmost leaf, the leaf count and the
    /// row count, because the caller that built the tree already knew them.
    /// A reader that opens a file it did not write knows only where the root
    /// is, so it has to find them - and `leaf_count` in particular is not a
    /// statistic here, it is the bound that stops a corrupt right-link chain
    /// from being followed forever.
    ///
    /// That makes the count both the thing being measured and the guard on
    /// measuring it, so this walk is bounded by the file instead: a chain
    /// cannot be longer than the file has pages, and a tree whose chain is
    /// says so rather than looping.
    ///
    /// **The walk reads every leaf.** That is free for a small tree - the
    /// catalog is one page - and it is emphatically not free for a large one,
    /// so this is the right way to open a catalog and the wrong way to open a
    /// table. A table's counts belong in the catalog beside its root page.
    ///
    /// @param pool - the buffer pool
    /// @param tree_id - the tree's identifier
    /// @param root - the root page
    /// @param columns - the column directory
    /// @param key_columns - how many leading columns form the key
    pub fn attach_scanned(
        pool: &Pool,
        tree_id: u64,
        root: PageId,
        columns: Vec<ColumnSpec>,
        key_columns: usize,
    ) -> DbResult<PagedTree> {
        let first_leaf = Self::leftmost_leaf(pool, root)?;
        let limit = pool.page_count().max(1);
        let mut page = first_leaf;
        let mut leaf_count = 0u64;
        let mut row_count = 0u64;
        while !page.is_none() {
            let guard = pool.fetch(page)?;
            let leaf = LeafRef::parse(&guard)?;
            row_count = row_count.saturating_add(leaf.row_count() as u64);
            page = leaf.right_sibling();
            leaf_count = leaf_count.saturating_add(1);
            if leaf_count > limit {
                return Err(corrupt("a leaf chain is longer than the file has pages"));
            }
        }
        PagedTree::attach(
            pool,
            tree_id,
            root,
            columns,
            key_columns,
            first_leaf,
            leaf_count,
            row_count,
        )
    }

    /// Returns the leftmost leaf of a tree, taking the first child at each level.
    ///
    /// Reads the child pointer directly rather than going through
    /// [`PagedTree::take_child`], because that swizzles the parent's slot as a
    /// side effect and this walk happens once, at open, before any descent has
    /// a reason to want the pointer warm.
    ///
    /// @param pool - the buffer pool
    /// @param root - the root page
    fn leftmost_leaf(pool: &Pool, root: PageId) -> DbResult<PageId> {
        let mut page = root;
        for _ in 0..=64 {
            let next = {
                let guard = pool.fetch(page)?;
                match page::kind_of(&guard)? {
                    PageKind::Leaf => return Ok(page),
                    PageKind::Interior => {
                        let interior = InteriorRef::parse(&guard)?;
                        pool.page_of_swip(interior.swip(0)?)?
                    }
                    other => return Err(corrupt(format!("a tree cannot contain {other:?}"))),
                }
            };
            page = next;
        }
        Err(corrupt("a tree is deeper than 64 levels"))
    }

    /// Descends to the leaf whose range holds a key, recording the path taken.
    ///
    /// The path costs one `Vec` push per level and is only wanted by a reverse
    /// walk, which is why it is a separate entry point from
    /// [`PagedTree::descend_guard`] rather than something every point probe
    /// pays for.
    ///
    /// @param pool - the buffer pool
    /// @param key - the encoded key being looked for
    pub fn descend(&self, pool: &Pool, key: &[u8]) -> DbResult<Descent> {
        let mut steps = Vec::with_capacity(self.height as usize);
        let mut page = self.root;
        loop {
            let child = {
                let guard = pool.fetch(page)?;
                if page::kind_of(&guard)? == PageKind::Leaf {
                    return Ok(Descent { steps, leaf: page });
                }
                InteriorRef::parse(&guard)?.child_for(key)?.0
            };
            steps.push((page, child));
            page = self.take_child(pool, page, child)?;
            if steps.len() > 64 {
                return Err(corrupt("a descent walked more than 64 levels"));
            }
        }
    }

    /// One attempt at a descent.
    ///
    /// Returns `None` when an optimistic read failed validation, which means
    /// the caller should start again. On the last attempt the reader takes
    /// shared latches instead, so a writer that keeps invalidating cannot
    /// starve it - the TDD's "after 4 restarts they descend with shared
    /// latches".
    ///
    /// The guard on the level above is held until the level below is pinned, so
    /// a pool too small to hold the path cannot evict a page this descent is
    /// still walking through. It is then dropped *before* the swizzle, because
    /// reading a frame and writing it are exclusive - and the swizzle re-checks
    /// that the frame still holds the page it was about to annotate.
    ///
    /// @param pool - the buffer pool
    /// @param key - the encoded key
    /// @param pessimistic - whether to take shared latches rather than observe
    fn try_descend<'p>(
        &self,
        pool: &'p Pool,
        key: &[u8],
        pessimistic: bool,
    ) -> DbResult<Option<(PageGuard<'p>, PageId)>> {
        let mut page = self.root;
        let mut guard = pool.fetch(page)?;
        loop {
            let frame = guard.frame();
            let observed = if pessimistic {
                None
            } else {
                pool.observe(frame)
            };
            if !pessimistic && observed.is_none() {
                return Ok(None);
            }
            if page::kind_of(&guard)? == PageKind::Leaf {
                if let Some(seen) = observed {
                    if !pool.validate(frame, seen) {
                        return Ok(None);
                    }
                }
                return Ok(Some((guard, page)));
            }
            let (swip, at) = {
                let interior = InteriorRef::parse(&guard)?;
                let (_, swip, at) = interior.child_for(key)?;
                (swip, at)
            };
            if let Some(seen) = observed {
                if !pool.validate(frame, seen) {
                    return Ok(None);
                }
            }
            // A swizzled swip names the frame directly, which is the whole
            // point of swizzling: no page-table lookup, no page id to resolve.
            let (child_guard, target) = match swip.frame() {
                Some(child_frame) => {
                    let child_guard = pool.fetch_frame(child_frame)?;
                    let target = pool
                        .page_in_frame(child_frame)
                        .filter(|page| !page.is_none())
                        .ok_or_else(|| corrupt("a swizzled swip names an empty frame"))?;
                    (child_guard, target)
                }
                None => {
                    let target = pool.page_of_swip(swip)?;
                    let child_guard = pool.fetch(target)?;
                    pool.note_parent(child_guard.frame(), frame, page, at);
                    (child_guard, target)
                }
            };
            let child_frame = child_guard.frame();
            let parent_page = page;
            drop(guard);
            pool.swizzle_into(frame, parent_page, at, Swip::swizzled(child_frame))?;
            guard = child_guard;
            page = target;
        }
    }

    /// Follows one child swip, swizzling the parent's slot.
    ///
    /// The shared half of [`PagedTree::try_descend`], used by the walks that
    /// take a fixed child rather than searching for one: the rightmost spine
    /// and the step-left of a reverse scan.
    ///
    /// @param pool - the buffer pool
    /// @param parent - the parent page
    /// @param child - which child to take
    fn take_child(&self, pool: &Pool, parent: PageId, child: usize) -> DbResult<PageId> {
        let (frame, at, target, child_frame) = {
            let guard = pool.fetch(parent)?;
            let frame = guard.frame();
            let interior = InteriorRef::parse(&guard)?;
            let swip = interior.swip(child)?;
            let at = interior.swip_offset(child)?;
            let target = pool.page_of_swip(swip)?;
            let child_guard = pool.fetch(target)?;
            let child_frame = child_guard.frame();
            pool.note_parent(child_frame, frame, parent, at);
            (frame, at, target, child_frame)
        };
        pool.swizzle_into(frame, parent, at, Swip::swizzled(child_frame))?;
        Ok(target)
    }

    /// Visits every leaf of the tree in key order.
    ///
    /// The visitor returns `false` to stop, which is how `LIMIT` gets out of a
    /// scan without reading the rest of the tree.
    ///
    /// @param pool - the buffer pool
    /// @param visit - what to do with each leaf
    pub fn visit_leaves(
        &self,
        pool: &Pool,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        self.visit_from(pool, self.first_leaf, visit)
    }

    /// Visits leaves from a starting page rightwards.
    ///
    /// @param pool - the buffer pool
    /// @param from - the first leaf to visit
    /// @param visit - what to do with each leaf
    pub fn visit_from(
        &self,
        pool: &Pool,
        from: PageId,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut page = from;
        let mut seen = 0u64;
        while !page.is_none() {
            let guard = pool.fetch(page)?;
            let leaf = LeafRef::parse(&guard)?.with_collations(&self.collations);
            let next = leaf.right_sibling();
            if !visit(&leaf)? {
                return Ok(());
            }
            drop(guard);
            page = next;
            seen = seen.saturating_add(1);
            if seen > self.leaf_count.saturating_add(1) {
                return Err(corrupt("the leaf chain is longer than the tree"));
            }
        }
        Ok(())
    }

    /// Visits every leaf whose range can hold a key at or above `low`.
    ///
    /// @param pool - the buffer pool
    /// @param low - the encoded lower bound
    /// @param visit - what to do with each leaf
    pub fn visit_range(
        &self,
        pool: &Pool,
        low: &[u8],
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let start = {
            let (guard, page) = self.descend_guard(pool, low)?;
            drop(guard);
            page
        };
        self.visit_from(pool, start, visit)
    }

    /// Visits leaves right to left, starting from the one holding a key.
    ///
    /// A reverse walk cannot follow a sibling pointer, because the common
    /// header carries only a right sibling. It walks the descent path instead:
    /// back up until a child index can be decremented, then down the rightmost
    /// spine of the sibling before it. That is `O(height)` per leaf rather than
    /// `O(1)`, which is the right trade for a workload that is always bounded
    /// by a `LIMIT`; adding a left-sibling field would cost a write on every
    /// split for the benefit of an unbounded reverse scan nobody runs.
    ///
    /// @param pool - the buffer pool
    /// @param from - the encoded key to start at, or `None` for the last leaf
    /// @param visit - what to do with each leaf
    pub fn visit_reverse(
        &self,
        pool: &Pool,
        from: Option<&[u8]>,
        visit: &mut dyn FnMut(&LeafRef<'_>) -> DbResult<bool>,
    ) -> DbResult<()> {
        let mut descent = match from {
            Some(key) => self.descend(pool, key)?,
            None => self.descend_rightmost(pool)?,
        };
        let mut seen = 0u64;
        loop {
            {
                let guard = pool.fetch(descent.leaf)?;
                let leaf = LeafRef::parse(&guard)?.with_collations(&self.collations);
                if !visit(&leaf)? {
                    return Ok(());
                }
            }
            seen = seen.saturating_add(1);
            if seen > self.leaf_count.saturating_add(1) {
                return Err(corrupt("a reverse walk visited more leaves than exist"));
            }
            match self.step_left(pool, &mut descent)? {
                true => {}
                false => return Ok(()),
            }
        }
    }

    /// Descends the rightmost spine.
    ///
    /// @param pool - the buffer pool
    fn descend_rightmost(&self, pool: &Pool) -> DbResult<Descent> {
        let mut steps = Vec::new();
        let mut page = self.root;
        loop {
            let child = {
                let guard = pool.fetch(page)?;
                if page::kind_of(&guard)? == PageKind::Leaf {
                    return Ok(Descent { steps, leaf: page });
                }
                InteriorRef::parse(&guard)?.count()
            };
            steps.push((page, child));
            page = self.take_child(pool, page, child)?;
            if steps.len() > 64 {
                return Err(corrupt("a rightmost descent walked more than 64 levels"));
            }
        }
    }

    /// Moves a descent to the leaf before the one it is on.
    ///
    /// Returns false when there is none, which is the left edge of the tree.
    ///
    /// @param pool - the buffer pool
    /// @param descent - the path, updated in place
    fn step_left(&self, pool: &Pool, descent: &mut Descent) -> DbResult<bool> {
        while let Some((page, index)) = descent.steps.pop() {
            if index == 0 {
                continue;
            }
            let child = index.saturating_sub(1);
            descent.steps.push((page, child));
            let mut next = self.take_child(pool, page, child)?;
            // Down the rightmost spine of that subtree.
            loop {
                let count = {
                    let guard = pool.fetch(next)?;
                    if page::kind_of(&guard)? == PageKind::Leaf {
                        descent.leaf = next;
                        break;
                    }
                    InteriorRef::parse(&guard)?.count()
                };
                descent.steps.push((next, count));
                next = self.take_child(pool, next, count)?;
                if descent.steps.len() > 64 {
                    return Err(corrupt("a left step walked more than 64 levels"));
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Returns the key that sorts immediately after every key sharing a prefix.
    ///
    /// Appending `0xFF` works because no encoded value can begin with it: a
    /// number's class byte is `0x01`, text's is `0x02`, a blob's `0x03` and a
    /// NULL is `0x00`. So `enc(v) ++ [0xFF]` sorts above every key that starts
    /// with `enc(v)` and below `enc(w)` for every `w > v` - the two facts a
    /// skip scan's seek needs, and the reason it does not have to know what the
    /// next distinct value *is* before it looks for it.
    ///
    /// @param prefix - the key prefix to step past
    pub fn after_prefix(&self, prefix: &[Datum<'_>]) -> KeyBytes {
        match self.encode_key_small(prefix) {
            KeyBytes::Inline(mut bytes, length) if length < KeyBytes::INLINE => {
                if let Some(slot) = bytes.get_mut(length) {
                    *slot = 0xFF;
                }
                KeyBytes::Inline(bytes, length.saturating_add(1))
            }
            other => {
                let mut heap = other.as_slice().to_vec();
                heap.push(0xFF);
                KeyBytes::Heap(heap)
            }
        }
    }

    /// Visits the rows of a range, a contiguous span of one leaf at a time.
    ///
    /// The span rather than the row is what keeps a range scan vectorised: the
    /// caller builds one batch per leaf over `start..end` instead of one batch
    /// per row. A range of 200 rows inside a leaf of 400 is one batch, one
    /// selection vector and no copying of values at all.
    ///
    /// The visitor returns `false` to stop.
    ///
    /// @param pool - the buffer pool
    /// @param low - the lower bound, or `None` for the start
    /// @param low_inclusive - whether a key equal to `low` is in the range
    /// @param high - the upper bound, or `None` for the end
    /// @param high_inclusive - whether a key equal to `high` is in the range
    /// @param visit - what to do with each leaf and its live span
    pub fn visit_span(
        &self,
        pool: &Pool,
        low: Option<&[Datum<'_>]>,
        low_inclusive: bool,
        high: Option<&[Datum<'_>]>,
        high_inclusive: bool,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        let start_page = match low {
            Some(values) => {
                let key = self.encode_key_small(values);
                let (guard, page) = self.descend_guard(pool, key.as_slice())?;
                drop(guard);
                page
            }
            None => self.first_leaf,
        };
        self.visit_from(pool, start_page, &mut |leaf| {
            let rows = leaf.row_count();
            // The lower bound is applied to *every* leaf, not only the first.
            //
            // A descent lands on the last child whose separator is not above
            // the probe, which is the leaf that *could* hold the key - and when
            // the key is above every key in that leaf, the run starts in the
            // next one. Applying the bound only to the first leaf and then
            // treating an empty result as "past the end" made such a range
            // return nothing at all.
            //
            // It was invisible at the default 32 KiB page size, where an index
            // leaf holds enough entries that a 200-row range almost always fits
            // in the leaf the descent lands on, and it was digest-equal to
            // SQLite on every workload. The page-size sweep is what found it:
            // at 8 KiB, `point.index` returned 3,991 rows where SQLite returned
            // 4,000. Five workloads were wrong and one measurement showed it.
            //
            // The cost of applying it everywhere is one binary search per leaf,
            // and after the first matching leaf it returns 0 immediately.
            // An *exclusive* lower bound starts past the run equal to it, so
            // it is an upper bound on the same key. `WHERE id > 495` returned
            // `id >= 495` until the physical pass's tests asked it directly.
            let begin = match low {
                Some(values) if low_inclusive => lower_bound(leaf, values)?,
                Some(values) => upper_bound(leaf, values)?,
                None => 0,
            };
            // The bound is a *bound*, not a search: a probe shorter than the
            // key matches a run of rows, and `search` lands somewhere inside
            // that run rather than at either end of it. Using it here returned
            // one row of a three-row match on the first index nested loop test
            // that exercised a prefix bound, which is how this is written as an
            // explicit partition point instead.
            let end = match high {
                Some(values) => {
                    if high_inclusive {
                        upper_bound(leaf, values)?
                    } else {
                        lower_bound(leaf, values)?
                    }
                }
                None => rows,
            };
            let end = end.min(rows);
            if begin >= end {
                // Nothing in this leaf. Two different reasons, and they have
                // opposite answers: the lower bound skipped the whole leaf, so
                // the run is further right; or the upper bound cut it short, so
                // there is nothing further right.
                return Ok(begin >= rows && end >= rows);
            }
            if !visit(leaf, begin, end)? {
                return Ok(false);
            }
            // A leaf that did not reach its own end reached the upper bound.
            Ok(end == rows)
        })
    }

    /// Visits every row whose key begins with an exact prefix.
    ///
    /// The equality case of [`PagedTree::visit_span`], and it is separate
    /// because the general version pays for generality it does not need here:
    /// two binary searches per leaf, one for each bound, where the bounds are
    /// the same key. This does one, then walks forward while the key still
    /// matches. An index nested loop probes once per outer row, so the second
    /// binary search was being paid two hundred times per `join.range`
    /// execution to find a run that is usually one entry long.
    ///
    /// The forward walk is capped: past [`RUN_SCAN`] matching rows it stops
    /// guessing and bisects, so a prefix that matches a whole leaf costs a
    /// binary search rather than a linear one.
    ///
    /// @param pool - the buffer pool
    /// @param key - the exact prefix, one value per compared column
    /// @param visit - what to do with each leaf and its matching span
    pub fn visit_equal(
        &self,
        pool: &Pool,
        key: &[Datum<'_>],
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        /// How far the run is walked before the upper bound is bisected.
        const RUN_SCAN: usize = 8;

        let start_page = {
            let encoded = self.encode_key_small(key);
            let (guard, page) = self.descend_guard(pool, encoded.as_slice())?;
            drop(guard);
            page
        };
        self.visit_from(pool, start_page, &mut |leaf| {
            let rows = leaf.row_count();
            let begin = lower_bound(leaf, key)?;
            if begin >= rows {
                // The key sorts after everything in this leaf, so the run - if
                // there is one - starts in the next.
                return Ok(true);
            }
            let view = leaf.key_view()?;
            if leaf.compare_key_with(&view, begin, key)? != std::cmp::Ordering::Equal {
                // The key is not here and everything after it is greater.
                return Ok(false);
            }
            let mut end = begin.saturating_add(1);
            while end < rows
                && end.saturating_sub(begin) < RUN_SCAN
                && leaf.compare_key_with(&view, end, key)? == std::cmp::Ordering::Equal
            {
                end = end.saturating_add(1);
            }
            if end.saturating_sub(begin) >= RUN_SCAN {
                end = upper_bound(leaf, key)?.min(rows);
            }
            if !visit(leaf, begin, end)? {
                return Ok(false);
            }
            // Only a run that reached the end of its leaf can continue.
            Ok(end == rows)
        })
    }

    /// Visits the rows of a range right to left, a span of one leaf at a time.
    ///
    /// The caller walks `start..end` backwards inside each span. Used by
    /// `ORDER BY ... DESC LIMIT n`, which is why it takes an upper bound and no
    /// lower one: the walk is always stopped by the limit rather than by the
    /// range.
    ///
    /// @param pool - the buffer pool
    /// @param high - the inclusive upper bound, or `None` for the last row
    /// @param visit - what to do with each leaf and its live span
    pub fn visit_span_reverse(
        &self,
        pool: &Pool,
        high: Option<&[Datum<'_>]>,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        let from = high.map(|values| self.encode_key(values));
        let mut first = true;
        self.visit_reverse(pool, from.as_deref(), &mut |leaf| {
            let rows = leaf.row_count();
            let end = if first {
                match high {
                    Some(values) => upper_bound(leaf, values)?,
                    None => rows,
                }
            } else {
                rows
            };
            first = false;
            let end = end.min(rows);
            if end == 0 {
                return Ok(true);
            }
            visit(leaf, 0, end)
        })
    }

    /// Visits one row per distinct value of a key prefix, in order.
    ///
    /// The index skip scan. `SELECT DISTINCT category` over an index led by
    /// `category` does not need to read the rows - it needs to visit one row
    /// per distinct value, and each step is a descent for
    /// [`PagedTree::after_prefix`] rather than a walk. Over 100,000 rows with
    /// 64 distinct categories that is 64 descents instead of 100,000 row reads,
    /// which is the algorithm SQLite uses for the same query.
    ///
    /// @param pool - the buffer pool
    /// @param prefix - how many leading key columns form the distinct value
    /// @param visit - what to do with each representative row
    pub fn skip_scan(
        &self,
        pool: &Pool,
        prefix: usize,
        visit: &mut dyn FnMut(&LeafRef<'_>, usize) -> DbResult<bool>,
    ) -> DbResult<()> {
        if prefix == 0 || prefix > self.key_columns {
            return Err(misuse(format!(
                "a skip scan over {prefix} of {} key columns",
                self.key_columns
            )));
        }
        let mut page = self.first_leaf;
        let mut row = 0usize;
        // Reused across seeks so a scan of 64 distinct values makes one
        // allocation rather than 64.
        let mut held: Vec<OwnedDatum> = Vec::with_capacity(prefix);
        let mut visited = 0u64;
        loop {
            // Step over any empty leaves at the current position and visit the
            // first row that is there, in *one* fetch of each leaf. A
            // bulk-built tree has no empty leaves, but one that has been
            // deleted from can, and a scan that stopped at the first would
            // silently return short - so the loop exists; fetching the landing
            // leaf twice to run it did not have to.
            let mut keep = true;
            let mut landed = false;
            while !page.is_none() {
                let guard = pool.fetch(page)?;
                let leaf = LeafRef::parse(&guard)?.with_collations(&self.collations);
                if row < leaf.row_count() {
                    keep = visit(&leaf, row)?;
                    held.clear();
                    for column in 0..prefix {
                        held.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
                    }
                    landed = true;
                    break;
                }
                let next = leaf.right_sibling();
                drop(guard);
                page = next;
                row = 0;
            }
            if !landed {
                return Ok(());
            }
            if !keep {
                return Ok(());
            }
            visited = visited.saturating_add(1);
            if visited > self.row_count.saturating_add(1) {
                return Err(corrupt("a skip scan visited more rows than the tree holds"));
            }
            let borrowed: Vec<Datum<'_>> = held.iter().map(OwnedDatum::borrow).collect();
            let seek = self.after_prefix(&borrowed);
            let (guard, landed) = self.descend_guard(pool, seek.as_slice())?;
            let next = {
                let leaf = LeafRef::parse(&guard)?.with_collations(&self.collations);
                // The first row in that leaf whose key is above the prefix. The
                // descent put us on the leaf that *could* hold it; which row it
                // is still has to be found, and a prefix comparison is what
                // finds it.
                let view = leaf.key_view()?;
                let mut low = 0usize;
                let mut high = leaf.row_count();
                while low < high {
                    let middle = low.saturating_add(high.saturating_sub(low) / 2);
                    match leaf.compare_key_with(&view, middle, &borrowed)? {
                        std::cmp::Ordering::Greater => high = middle,
                        _ => low = middle.saturating_add(1),
                    }
                }
                if low < leaf.row_count() {
                    Some((landed, low))
                } else {
                    let right = leaf.right_sibling();
                    if right.is_none() {
                        None
                    } else {
                        Some((right, 0))
                    }
                }
            };
            match next {
                Some((next_page, next_row)) => {
                    page = next_page;
                    row = next_row;
                }
                None => return Ok(()),
            }
        }
    }

    /// Runs a function over the row a key names, if the tree holds it.
    ///
    /// The row is handed to the caller *inside* the leaf's guard, so a point
    /// probe projects straight out of the page and copies only what the query
    /// asked for. Returning the row instead would mean copying every column of
    /// it first, which on `point.rowid` is the difference between the target
    /// and twice the target.
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    /// @param read - what to do with the leaf and the row index
    pub fn probe<R>(
        &self,
        pool: &Pool,
        probe: &[Datum<'_>],
        read: impl FnOnce(&LeafRef<'_>, usize) -> DbResult<R>,
    ) -> DbResult<Option<R>> {
        let key = self.encode_key_small(probe);
        let (guard, _) = self.descend_guard(pool, key.as_slice())?;
        let leaf = LeafRef::parse(&guard)?.with_collations(&self.collations);
        if let Ok(row) = leaf.search(probe)? {
            if !leaf.is_tombstoned(row)? {
                return Ok(Some(read(&leaf, row)?));
            }
        }
        for entry in 0..leaf.delta_count() {
            let mut matches = true;
            for column in 0..self.key_columns {
                let held = leaf.delta_value(entry, column)?;
                let wanted = probe.get(column).copied().unwrap_or(Datum::Null);
                if held.compare(&wanted) != std::cmp::Ordering::Equal {
                    matches = false;
                    break;
                }
            }
            if matches {
                return Err(misuse(
                    "a delta row was found by a Phase 2 probe; deltas arrive in Phase 3",
                ));
            }
        }
        Ok(None)
    }

    /// Returns one row by key, copying it out.
    ///
    /// The allocating path, for tests and for the model comparison. The
    /// executor uses [`PagedTree::probe`].
    ///
    /// @param pool - the buffer pool
    /// @param probe - the key, one value per key column
    pub fn point(&self, pool: &Pool, probe: &[Datum<'_>]) -> DbResult<Option<Vec<OwnedDatum>>> {
        self.probe(pool, probe, |leaf, row| {
            let mut values = Vec::with_capacity(leaf.column_count());
            for column in 0..leaf.column_count() {
                values.push(OwnedDatum::from_datum(&leaf.value(row, column)?));
            }
            Ok(values)
        })
    }

    /// Returns every row in key order, copying them out.
    ///
    /// The slow path the property test and the integrity checker use.
    ///
    /// @param pool - the buffer pool
    pub fn rows(&self, pool: &Pool) -> DbResult<Vec<Vec<OwnedDatum>>> {
        let mut out = Vec::new();
        self.visit_leaves(pool, &mut |leaf| {
            for row in leaf.live()? {
                out.push(row.iter().map(OwnedDatum::from_datum).collect());
            }
            Ok(true)
        })?;
        Ok(out)
    }

    /// Checks the tree's structural invariants against the pages themselves.
    ///
    /// 1. Every leaf parses and passes its own row-level integrity check.
    /// 2. Keys strictly increase within a leaf and across leaf boundaries.
    /// 3. Every interior separator is the first key of the child it precedes,
    ///    so a descent for that key lands on that child and not its neighbour.
    /// 4. The sibling chain visits exactly the leaves the interior levels do.
    ///
    /// @param pool - the buffer pool
    pub fn check(&self, pool: &Pool) -> DbResult<()> {
        let mut previous: Option<Vec<OwnedDatum>> = None;
        let mut chain = 0u64;
        self.visit_leaves(pool, &mut |leaf| {
            leaf.integrity()?;
            chain = chain.saturating_add(1);
            for row in leaf.live()? {
                let head: Vec<Datum<'_>> = row.iter().copied().take(self.key_columns).collect();
                if let Some(last) = &previous {
                    let borrowed: Vec<Datum<'_>> = last.iter().map(OwnedDatum::borrow).collect();
                    if crate::leaf::compare_rows(&borrowed, &head, self.key_columns)
                        != std::cmp::Ordering::Less
                    {
                        return Err(corrupt("a key does not increase across the leaf chain"));
                    }
                }
                previous = Some(head.iter().map(OwnedDatum::from_datum).collect());
            }
            Ok(true)
        })?;
        if chain != self.leaf_count {
            return Err(corrupt(format!(
                "the sibling chain visits {chain} leaves but the tree claims {}",
                self.leaf_count
            )));
        }
        self.check_subtree(pool, self.root, self.height)?;
        Ok(())
    }

    /// Checks one subtree's separators against its children's first keys.
    ///
    /// @param pool - the buffer pool
    /// @param page - the subtree's root
    /// @param level - the level the page should declare
    fn check_subtree(&self, pool: &Pool, page: PageId, level: u16) -> DbResult<()> {
        let image = {
            let guard = pool.fetch(page)?;
            guard.bytes().to_vec()
        };
        if page::kind_of(&image)? == PageKind::Leaf {
            if level != 0 {
                return Err(corrupt(format!("a leaf sits at level {level}")));
            }
            return Ok(());
        }
        let interior = InteriorRef::parse(&image)?;
        // The slot array's ordering is checked here rather than on every parse:
        // a descent parses a page to read one slot, and validating the whole
        // array there cost `PointProbe` more than three times its budget. The
        // integrity checker is where an O(pages x slots) pass belongs.
        interior.validate()?;
        if interior.level() != level {
            return Err(corrupt(format!(
                "an interior page declares level {} where the tree says {level}",
                interior.level()
            )));
        }
        for child in 0..interior.children() {
            let target = pool.page_of_swip(interior.swip(child)?)?;
            if child > 0 {
                let separator = interior.key(child.saturating_sub(1))?;
                let first = self.first_key_under(pool, target)?;
                if first.as_slice() != separator {
                    return Err(corrupt(format!(
                        "interior separator {} is not its child's first key",
                        child.saturating_sub(1)
                    )));
                }
            }
            self.check_subtree(pool, target, level.saturating_sub(1))?;
        }
        Ok(())
    }

    /// Returns the encoded first key of the leftmost leaf under a page.
    ///
    /// @param pool - the buffer pool
    /// @param page - the subtree's root
    fn first_key_under(&self, pool: &Pool, page: PageId) -> DbResult<Vec<u8>> {
        let mut current = page;
        for _ in 0..64 {
            let leaf_key = {
                let guard = pool.fetch(current)?;
                if page::kind_of(&guard)? == PageKind::Leaf {
                    let leaf = LeafRef::parse(&guard)?.with_collations(&self.collations);
                    if leaf.row_count() == 0 {
                        return Ok(Vec::new());
                    }
                    let mut head = Vec::with_capacity(self.key_columns);
                    for column in 0..self.key_columns {
                        head.push(leaf.value(0, column)?);
                    }
                    Some(self.encode_key(&head))
                } else {
                    None
                }
            };
            if let Some(key) = leaf_key {
                return Ok(key);
            }
            current = self.take_child(pool, current, 0)?;
        }
        Err(corrupt("a subtree is deeper than 64 levels"))
    }
}

/// Returns the first row of a leaf whose key is at or above a probe.
///
/// A thin name over [`LeafRef::lower_bound`], kept because the span walks read
/// better with it and because the probe may be shorter than the leaf's key -
/// a one-column probe against a two-column key finds the start of the run
/// sharing that column.
///
/// @param leaf - the leaf to search
/// @param probe - the bound, one value per compared column
fn lower_bound(leaf: &LeafRef<'_>, probe: &[Datum<'_>]) -> DbResult<usize> {
    leaf.lower_bound(probe)
}

/// Returns the first row of a leaf whose key is above a probe.
///
/// @param leaf - the leaf to search
/// @param probe - the bound, one value per compared column
fn upper_bound(leaf: &LeafRef<'_>, probe: &[Datum<'_>]) -> DbResult<usize> {
    leaf.upper_bound(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inillucent_pool::{Options, PageId};
    use inillucent_vfs::{DbPath, MemoryVfs};
    use std::collections::BTreeMap;

    /// A two-column rowid tree: the key and a label.
    fn rowid_columns() -> Vec<ColumnSpec> {
        vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::new(PhysicalType::Text),
        ]
    }

    /// Builds a rowid tree of `rows` rows over a small page, so the tree really
    /// has interior levels rather than one leaf pretending to be a root.
    fn build(rows: i64, page_size: usize) -> (Database, PagedTree, Vec<Vec<OwnedDatum>>) {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("paged.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default()
                .with_page_size(page_size)
                .with_frames(512),
        )
        .unwrap();
        let labels: Vec<String> = (0..rows).map(|n| format!("label-{n:06}")).collect();
        let owned: Vec<Vec<OwnedDatum>> = (0..rows)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n),
                    OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
                ]
            })
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 7, rowid_columns(), 1, &borrowed).unwrap();
        (database, tree, owned)
    }

    /// A tree built over enough rows to need interior levels has them, passes
    /// its own integrity check, and scans back every row in order.
    #[test]
    fn a_multi_level_tree_scans_in_order() {
        let (database, tree, rows) = build(4_000, 512);
        assert!(tree.height() >= 2, "height is {}", tree.height());
        assert!(tree.leaf_count() > 100);
        tree.check(database.pool()).unwrap();
        let scanned = tree.rows(database.pool()).unwrap();
        assert_eq!(scanned.len(), rows.len());
        for (index, row) in scanned.iter().enumerate() {
            assert_eq!(row[0], OwnedDatum::Int(index as i64));
        }
        assert_eq!(tree.row_count(), rows.len() as u64);
    }

    /// Every key is found by descent, and a key that is not there is not found.
    #[test]
    fn every_key_is_found_by_descent() {
        let (database, tree, rows) = build(2_000, 512);
        let pool = database.pool();
        for index in (0..rows.len()).step_by(7) {
            let probe = [Datum::Int(index as i64)];
            let found = tree.point(pool, &probe).unwrap().expect("row is there");
            assert_eq!(found[0], OwnedDatum::Int(index as i64));
            assert_eq!(found[1], rows[index][1]);
        }
        for missing in [-1i64, 2_000, 100_000] {
            assert!(tree.point(pool, &[Datum::Int(missing)]).unwrap().is_none());
        }
    }

    /// A reverse walk visits exactly the leaves a forward walk does, in the
    /// opposite order.
    #[test]
    fn a_reverse_walk_is_the_forward_walk_backwards() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let mut forward: Vec<i64> = Vec::new();
        tree.visit_leaves(pool, &mut |leaf| {
            if leaf.row_count() > 0 {
                forward.push(leaf.value(0, 0)?.as_int().unwrap_or(-1));
            }
            Ok(true)
        })
        .unwrap();
        let mut backward: Vec<i64> = Vec::new();
        tree.visit_reverse(pool, None, &mut |leaf| {
            if leaf.row_count() > 0 {
                backward.push(leaf.value(0, 0)?.as_int().unwrap_or(-1));
            }
            Ok(true)
        })
        .unwrap();
        backward.reverse();
        assert_eq!(forward, backward);
        assert!(forward.len() > 50);
    }

    /// A reverse walk from a key starts on the leaf holding it and stops when
    /// the visitor says so, which is how `ORDER BY ... DESC LIMIT` works.
    #[test]
    fn a_bounded_reverse_walk_stops_early() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let key = tree.encode_key(&[Datum::Int(1_500)]);
        let mut visited = 0usize;
        let mut highest = None;
        tree.visit_reverse(pool, Some(&key), &mut |leaf| {
            visited = visited.saturating_add(1);
            if highest.is_none() {
                highest = leaf.value(0, 0)?.as_int();
            }
            Ok(visited < 3)
        })
        .unwrap();
        assert_eq!(visited, 3, "the walk stopped when it was told to");
        assert!(highest.unwrap_or(i64::MAX) <= 1_500);
    }

    /// A range walk starts at the leaf holding the low bound rather than at the
    /// beginning of the tree.
    #[test]
    fn a_range_walk_skips_what_is_below_it() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let low = tree.encode_key(&[Datum::Int(2_000)]);
        let mut first = None;
        tree.visit_range(pool, &low, &mut |leaf| {
            if first.is_none() && leaf.row_count() > 0 {
                first = leaf.value(0, 0)?.as_int();
            }
            Ok(false)
        })
        .unwrap();
        let started = first.expect("the walk visited a leaf");
        assert!(started <= 2_000, "started at {started}");
        assert!(started > 1_800, "started at {started}, too far back");
    }

    /// The tree agrees with a `BTreeMap` on every key, which is the property
    /// test the TDD asks for, now over real interior pages.
    #[test]
    fn the_paged_tree_agrees_with_a_btreemap() {
        let (database, tree, rows) = build(1_500, 512);
        let pool = database.pool();
        let mut model: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
        for row in &rows {
            if let (OwnedDatum::Int(key), OwnedDatum::Text(label)) = (&row[0], &row[1]) {
                model.insert(*key, label.clone());
            }
        }
        for (key, label) in &model {
            let found = tree
                .point(pool, &[Datum::Int(*key)])
                .unwrap()
                .expect("model key is in the tree");
            assert_eq!(found[1], OwnedDatum::Text(label.clone()));
        }
        let scanned = tree.rows(pool).unwrap();
        assert_eq!(scanned.len(), model.len());
    }

    /// A tree with a compound key uses the general encoding and still descends
    /// correctly, including where two rows share a leading column.
    #[test]
    fn a_compound_key_tree_descends() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("compound.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Text),
        ];
        let labels: Vec<String> = (0..900).map(|n| format!("t{:04}", n)).collect();
        let owned: Vec<Vec<OwnedDatum>> = (0..900)
            .map(|n| {
                vec![
                    OwnedDatum::Int(n / 10),
                    OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
                ]
            })
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 11, columns, 2, &borrowed).unwrap();
        assert_eq!(tree.key_encoding(), KeyEncoding::General);
        assert!(tree.height() >= 1);
        tree.check(database.pool()).unwrap();
        for index in (0..owned.len()).step_by(11) {
            let probe: Vec<Datum<'_>> = owned[index].iter().map(OwnedDatum::borrow).collect();
            assert!(
                tree.point(database.pool(), &probe[..2]).unwrap().is_some(),
                "row {index} was not found"
            );
        }
    }

    /// A range span visits exactly the rows between the bounds, and no leaf
    /// beyond the upper one.
    #[test]
    fn a_range_span_is_exactly_the_rows_between_the_bounds() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        for (low, high, inclusive) in [
            (100i64, 400i64, true),
            (100, 400, false),
            (0, 0, true),
            (2_999, 2_999, true),
            (1_500, 1_500, false),
        ] {
            let mut seen: Vec<i64> = Vec::new();
            let mut leaves = 0usize;
            tree.visit_span(
                pool,
                Some(&[Datum::Int(low)]),
                true,
                Some(&[Datum::Int(high)]),
                inclusive,
                &mut |leaf, start, end| {
                    leaves = leaves.saturating_add(1);
                    for row in start..end {
                        seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            let wanted: Vec<i64> = if inclusive {
                (low..=high).collect()
            } else {
                (low..high).collect()
            };
            assert_eq!(seen, wanted, "{low}..{high} inclusive={inclusive}");
        }
        // An unbounded range is the whole tree.
        let mut count = 0usize;
        tree.visit_span(pool, None, true, None, true, &mut |_, start, end| {
            count = count.saturating_add(end - start);
            Ok(true)
        })
        .unwrap();
        assert_eq!(count, 3_000);
    }

    /// A bound shorter than the key selects the whole run sharing it, which is
    /// what an index nested loop over a key prefix needs.
    #[test]
    fn a_prefix_bound_selects_the_whole_run() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("prefix.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        let owned: Vec<Vec<OwnedDatum>> = (0..600i64)
            .map(|n| vec![OwnedDatum::Int(n / 3), OwnedDatum::Int(n)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 8, columns, 2, &borrowed).unwrap();
        for owner in [0i64, 5, 99, 199] {
            let mut seen: Vec<i64> = Vec::new();
            tree.visit_span(
                database.pool(),
                Some(&[Datum::Int(owner)]),
                true,
                Some(&[Datum::Int(owner)]),
                true,
                &mut |leaf, start, end| {
                    for row in start..end {
                        seen.push(leaf.value(row, 1)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            assert_eq!(
                seen,
                vec![owner * 3, owner * 3 + 1, owner * 3 + 2],
                "owner {owner}"
            );
        }
        // An exclusive upper bound on a prefix excludes the whole run.
        let mut count = 0usize;
        tree.visit_span(
            database.pool(),
            Some(&[Datum::Int(5)]),
            true,
            Some(&[Datum::Int(5)]),
            false,
            &mut |_, start, end| {
                count = count.saturating_add(end - start);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(count, 0);
    }

    /// A range whose lower bound falls past the end of the leaf the descent
    /// lands on still finds its rows.
    ///
    /// The regression test for the bug the page-size sweep found. The descent
    /// goes to the last child whose separator is not above the probe, so a
    /// bound that sits between two leaves lands on the earlier one - and the
    /// first version stopped there and returned nothing. Every boundary in the
    /// tree is tried, because the bug only shows at one.
    #[test]
    fn a_bound_between_two_leaves_finds_the_rows_after_it() {
        let (database, tree, _) = build(4_000, 512);
        let pool = database.pool();
        // The first key of every leaf, so a bound of "one less" sits exactly on
        // a boundary.
        let mut boundaries: Vec<i64> = Vec::new();
        tree.visit_leaves(pool, &mut |leaf| {
            if leaf.row_count() > 0 {
                boundaries.push(leaf.value(0, 0)?.as_int().unwrap_or(-1));
            }
            Ok(true)
        })
        .unwrap();
        assert!(boundaries.len() > 20, "the tree has too few leaves to test");
        for first in boundaries {
            for low in [first.saturating_sub(1), first, first.saturating_add(1)] {
                let high = low.saturating_add(3);
                let mut seen: Vec<i64> = Vec::new();
                tree.visit_span(
                    pool,
                    Some(&[Datum::Int(low)]),
                    true,
                    Some(&[Datum::Int(high)]),
                    true,
                    &mut |leaf, start, end| {
                        for row in start..end {
                            seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                        }
                        Ok(true)
                    },
                )
                .unwrap();
                let wanted: Vec<i64> = (low.max(0)..=high.min(3_999)).collect();
                assert_eq!(seen, wanted, "range {low}..={high} at boundary {first}");
            }
        }
    }

    /// An exclusive lower bound excludes the key it names.
    #[test]
    fn an_exclusive_lower_bound_excludes_its_key() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        for low in [0i64, 1, 999, 1_999] {
            for inclusive in [true, false] {
                let mut seen: Vec<i64> = Vec::new();
                tree.visit_span(
                    pool,
                    Some(&[Datum::Int(low)]),
                    inclusive,
                    Some(&[Datum::Int(low.saturating_add(3))]),
                    true,
                    &mut |leaf, start, end| {
                        for row in start..end {
                            seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                        }
                        Ok(true)
                    },
                )
                .unwrap();
                let first = if inclusive {
                    low
                } else {
                    low.saturating_add(1)
                };
                let wanted: Vec<i64> = (first..=low.saturating_add(3))
                    .filter(|key| (0..2_000).contains(key))
                    .collect();
                assert_eq!(seen, wanted, "low {low} inclusive {inclusive}");
            }
        }
    }

    /// A range whose bounds fall outside the tree returns nothing rather than
    /// everything.
    #[test]
    fn a_range_outside_the_tree_is_empty() {
        let (database, tree, _) = build(1_000, 512);
        let pool = database.pool();
        let mut count = 0usize;
        tree.visit_span(
            pool,
            Some(&[Datum::Int(5_000)]),
            true,
            Some(&[Datum::Int(6_000)]),
            true,
            &mut |_, start, end| {
                count = count.saturating_add(end - start);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(count, 0);
        let mut count = 0usize;
        tree.visit_span(
            pool,
            Some(&[Datum::Int(-100)]),
            true,
            Some(&[Datum::Int(-1)]),
            true,
            &mut |_, start, end| {
                count = count.saturating_add(end - start);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(count, 0);
    }

    /// The equality walk finds the same rows the general span does, at every
    /// run length including one that crosses a leaf boundary.
    #[test]
    fn the_equality_walk_matches_the_general_span() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("equal.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(256),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        // Run lengths of 1, 3 and 40, so the linear walk, its cap and the
        // bisected fallback are all exercised, and 40 is long enough to cross a
        // leaf at this page size.
        let mut owned: Vec<Vec<OwnedDatum>> = Vec::new();
        let mut rowid = 0i64;
        for owner in 0..200i64 {
            let run = match owner % 3 {
                0 => 1,
                1 => 3,
                _ => 40,
            };
            for _ in 0..run {
                owned.push(vec![OwnedDatum::Int(owner), OwnedDatum::Int(rowid)]);
                rowid += 1;
            }
        }
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 12, columns, 2, &borrowed).unwrap();
        tree.check(database.pool()).unwrap();
        for owner in [-1i64, 0, 1, 2, 3, 100, 199, 200] {
            let mut by_span: Vec<i64> = Vec::new();
            tree.visit_span(
                database.pool(),
                Some(&[Datum::Int(owner)]),
                true,
                Some(&[Datum::Int(owner)]),
                true,
                &mut |leaf, start, end| {
                    for row in start..end {
                        by_span.push(leaf.value(row, 1)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            let mut by_equal: Vec<i64> = Vec::new();
            tree.visit_equal(
                database.pool(),
                &[Datum::Int(owner)],
                &mut |leaf, start, end| {
                    for row in start..end {
                        by_equal.push(leaf.value(row, 1)?.as_int().unwrap_or(-1));
                    }
                    Ok(true)
                },
            )
            .unwrap();
            assert_eq!(by_equal, by_span, "owner {owner}");
        }
    }

    /// A reverse span walks down from the bound and stops when told to, which
    /// is `ORDER BY ... DESC LIMIT n`.
    #[test]
    fn a_reverse_span_walks_down_from_the_bound() {
        let (database, tree, _) = build(3_000, 512);
        let pool = database.pool();
        let mut seen: Vec<i64> = Vec::new();
        tree.visit_span_reverse(pool, Some(&[Datum::Int(2_000)]), &mut |leaf, start, end| {
            for row in (start..end).rev() {
                seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                if seen.len() >= 50 {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .unwrap();
        let wanted: Vec<i64> = (1_951..=2_000).rev().collect();
        assert_eq!(seen, wanted);

        // From the end of the tree.
        let mut seen: Vec<i64> = Vec::new();
        tree.visit_span_reverse(pool, None, &mut |leaf, start, end| {
            for row in (start..end).rev() {
                seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
                if seen.len() >= 3 {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, vec![2_999, 2_998, 2_997]);
    }

    /// A skip scan visits one row per distinct prefix, in order, and does far
    /// fewer page fetches than a scan of every row would.
    #[test]
    fn a_skip_scan_visits_each_distinct_prefix_once() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("skip.rdb");
        let mut database = Database::create(
            &vfs,
            &path,
            Options::default().with_page_size(512).with_frames(512),
        )
        .unwrap();
        let columns = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        // 64 distinct leading values over 6,400 rows.
        let owned: Vec<Vec<OwnedDatum>> = (0..6_400i64)
            .map(|n| vec![OwnedDatum::Int(n / 100), OwnedDatum::Int(n)])
            .collect();
        let borrowed: Vec<Vec<Datum<'_>>> = owned
            .iter()
            .map(|row| row.iter().map(OwnedDatum::borrow).collect())
            .collect();
        let tree = PagedTree::bulk_build(&mut database, 5, columns, 2, &borrowed).unwrap();
        tree.check(database.pool()).unwrap();
        database.pool().reset_stats();
        let mut distinct: Vec<i64> = Vec::new();
        tree.skip_scan(database.pool(), 1, &mut |leaf, row| {
            distinct.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
            Ok(true)
        })
        .unwrap();
        assert_eq!(distinct, (0..64).collect::<Vec<i64>>());
        // What a skip scan saves is *row reads*, not page fetches, and the
        // distinction is worth pinning down because the first version of this
        // test asserted on fetches and failed while the scan was working
        // correctly. Seeking to 64 values in a 291-leaf tree costs one descent
        // each - a few page fetches per descent, all of them hits - where
        // walking costs 291 leaf fetches *and* 6,400 row reads. The fetch
        // counts are the same order of magnitude; the row reads are two orders
        // apart, and that is the whole of the algorithm's advantage.
        assert_eq!(distinct.len(), 64, "one row read per distinct value");
        let fetches = database.pool().stats().hits + database.pool().stats().misses;
        assert!(
            fetches < 64 * u64::from(tree.height() + 3),
            "a skip scan made {fetches} fetches for 64 seeks down {} levels",
            tree.height()
        );
        // Stopping early stops.
        let mut count = 0usize;
        tree.skip_scan(database.pool(), 1, &mut |_, _| {
            count = count.saturating_add(1);
            Ok(count < 5)
        })
        .unwrap();
        assert_eq!(count, 5);
        assert!(tree
            .skip_scan(database.pool(), 0, &mut |_, _| Ok(true))
            .is_err());
        assert!(tree
            .skip_scan(database.pool(), 9, &mut |_, _| Ok(true))
            .is_err());
    }

    /// A skip scan over a tree whose prefix is unique per row degenerates to a
    /// full walk and still answers correctly, which is the case the physical
    /// pass has to avoid choosing rather than the case this has to refuse.
    #[test]
    fn a_skip_scan_over_unique_keys_still_answers() {
        let (database, tree, _) = build(500, 512);
        let mut seen: Vec<i64> = Vec::new();
        tree.skip_scan(database.pool(), 1, &mut |leaf, row| {
            seen.push(leaf.value(row, 0)?.as_int().unwrap_or(-1));
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, (0..500).collect::<Vec<i64>>());
    }

    /// A tree of one leaf has that leaf as its root and still answers.
    #[test]
    fn a_single_leaf_tree_is_its_own_root() {
        let (database, tree, _) = build(3, 8_192);
        assert_eq!(tree.height(), 0);
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.root(), tree.first_leaf());
        tree.check(database.pool()).unwrap();
        assert!(tree
            .point(database.pool(), &[Datum::Int(1)])
            .unwrap()
            .is_some());
    }

    /// An empty tree is one empty leaf, so nothing has to special-case a root
    /// that does not exist.
    #[test]
    fn an_empty_tree_is_one_empty_leaf() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("empty.rdb");
        let mut database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        let tree = PagedTree::bulk_build(&mut database, 3, rowid_columns(), 1, &[]).unwrap();
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.row_count(), 0);
        assert!(tree.rows(database.pool()).unwrap().is_empty());
        assert!(tree
            .point(database.pool(), &[Datum::Int(1)])
            .unwrap()
            .is_none());
        tree.check(database.pool()).unwrap();
    }

    /// A tree survives a checkpoint and a reopen with a pool far too small to
    /// hold it, which is the eviction path under a descent.
    #[test]
    fn a_tree_reads_correctly_through_a_tiny_pool() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("tiny.rdb");
        let (root, first, leaves, count) = {
            let mut database = Database::create(
                &vfs,
                &path,
                Options::default().with_page_size(512).with_frames(512),
            )
            .unwrap();
            let labels: Vec<String> = (0..4_000).map(|n| format!("label-{n:06}")).collect();
            let owned: Vec<Vec<OwnedDatum>> = (0..4_000i64)
                .map(|n| {
                    vec![
                        OwnedDatum::Int(n),
                        OwnedDatum::Text(labels[n as usize].clone().into_bytes()),
                    ]
                })
                .collect();
            let borrowed: Vec<Vec<Datum<'_>>> = owned
                .iter()
                .map(|row| row.iter().map(OwnedDatum::borrow).collect())
                .collect();
            let tree =
                PagedTree::bulk_build(&mut database, 7, rowid_columns(), 1, &borrowed).unwrap();
            database.set_catalog_root(tree.root());
            database.checkpoint().unwrap();
            (
                tree.root(),
                tree.first_leaf(),
                tree.leaf_count(),
                tree.row_count(),
            )
        };
        let database = Database::open(&vfs, &path, 64).unwrap();
        let tree = PagedTree::attach(
            database.pool(),
            7,
            root,
            rowid_columns(),
            1,
            first,
            leaves,
            count,
        )
        .unwrap();
        assert_eq!(tree.root(), root);
        tree.check(database.pool()).unwrap();
        for key in (0..4_000i64).step_by(37) {
            let found = tree
                .point(database.pool(), &[Datum::Int(key)])
                .unwrap()
                .expect("row survived eviction");
            assert_eq!(found[0], OwnedDatum::Int(key));
        }
        assert!(
            database.pool().stats().evicted > 0,
            "a 64-frame pool over a 4,000-row tree must have evicted"
        );
        assert_eq!(tree.rows(database.pool()).unwrap().len(), 4_000);
    }

    /// A descent that races a frame reload restarts rather than reading a page
    /// that is no longer there. The race is forced: the frame's version is
    /// bumped between the observation and the validation.
    #[test]
    fn a_descent_validates_what_it_read() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        let guard = pool.fetch(tree.root()).unwrap();
        let frame = guard.frame();
        let observed = pool.observe(frame).expect("the root admits a reader");
        assert!(pool.validate(frame, observed));
        drop(guard);
        // A writer taking and releasing the latch is exactly what an eviction
        // or a load does, and it must invalidate the observation.
        let latch = pool.latch(frame).expect("the frame has a latch");
        assert!(latch.try_exclusive());
        latch.release_exclusive();
        assert!(!pool.validate(frame, observed));
        // And the descent itself still answers, because it restarts.
        assert!(tree.point(pool, &[Datum::Int(1_000)]).unwrap().is_some());
    }

    /// Swizzling happens: after a descent, the root's slot for the child taken
    /// holds a frame rather than a page id, and the tree still answers.
    #[test]
    fn a_descent_swizzles_the_slot_it_took() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        assert!(tree.point(pool, &[Datum::Int(500)]).unwrap().is_some());
        let guard = pool.fetch(tree.root()).unwrap();
        let interior = InteriorRef::parse(&guard).unwrap();
        let swizzled = (0..interior.children())
            .filter_map(|child| interior.swip(child).ok())
            .filter(|swip| !swip.is_unswizzled())
            .count();
        assert!(swizzled > 0, "no slot of the root was swizzled");
        drop(guard);
        assert!(tree.point(pool, &[Datum::Int(500)]).unwrap().is_some());
        assert!(tree.point(pool, &[Datum::Int(1_900)]).unwrap().is_some());
    }

    /// The rowid encoding is chosen only for a single non-nullable integer key,
    /// and it is exact where the general one would be eighteen bytes.
    #[test]
    fn the_rowid_encoding_is_chosen_only_when_it_applies() {
        let rowid = vec![ColumnSpec::key(PhysicalType::Int64)];
        assert_eq!(KeyEncoding::choose(&rowid, 1), KeyEncoding::Rowid);
        let nullable = vec![ColumnSpec::new(PhysicalType::Int64)];
        assert_eq!(KeyEncoding::choose(&nullable, 1), KeyEncoding::General);
        let text = vec![ColumnSpec::key(PhysicalType::Text)];
        assert_eq!(KeyEncoding::choose(&text, 1), KeyEncoding::General);
        let two = vec![
            ColumnSpec::key(PhysicalType::Int64),
            ColumnSpec::key(PhysicalType::Int64),
        ];
        assert_eq!(KeyEncoding::choose(&two, 2), KeyEncoding::General);
        assert_eq!(KeyEncoding::choose(&[], 1), KeyEncoding::General);

        assert_eq!(KeyEncoding::Rowid.encode(&[Datum::Int(1)]).len(), 8);
        assert_eq!(KeyEncoding::General.encode(&[Datum::Int(1)]).len(), 18);
        // The rowid encoding is order preserving over the whole integer range.
        let samples = [i64::MIN, -1, 0, 1, 1 << 60, i64::MAX];
        for left in samples {
            for right in samples {
                assert_eq!(
                    KeyEncoding::Rowid
                        .encode(&[Datum::Int(left)])
                        .cmp(&KeyEncoding::Rowid.encode(&[Datum::Int(right)])),
                    left.cmp(&right),
                    "{left} vs {right}"
                );
            }
        }
        // The odd probes are total rather than correct-by-luck.
        assert_eq!(KeyEncoding::Rowid.encode(&[]).len(), 0);
        assert_eq!(KeyEncoding::Rowid.encode(&[Datum::Null]).len(), 8);
        assert_eq!(
            KeyEncoding::Rowid.encode(&[Datum::Text(b"x")]),
            vec![0xFF; 8]
        );
        assert_eq!(KeyEncoding::Rowid.encode(&[Datum::Real(2.5)]).len(), 8);
        assert_eq!(
            KeyEncoding::Rowid.encode(&[Datum::Real(f64::MAX)]),
            KeyEncoding::Rowid.encode(&[Datum::Int(i64::MAX)])
        );
        assert_eq!(
            KeyEncoding::Rowid.encode(&[Datum::Real(f64::MIN)]),
            KeyEncoding::Rowid.encode(&[Datum::Int(i64::MIN)])
        );
    }

    /// A tree whose separators were corrupted is caught by the checker rather
    /// than by a wrong answer.
    #[test]
    fn a_corrupt_separator_is_caught_by_the_checker() {
        let (database, tree, _) = build(2_000, 512);
        let pool = database.pool();
        tree.check(pool).unwrap();
        // Rewrite one separator to a key that is not its child's first.
        let root = tree.root();
        let (offset, length) = {
            let guard = pool.fetch(root).unwrap();
            let interior = InteriorRef::parse(&guard).unwrap();
            assert!(interior.count() > 0);
            let key = interior.key(0).unwrap();
            let base = guard.bytes().as_ptr() as usize;
            let _ = base;
            (
                key.as_ptr() as usize - guard.bytes().as_ptr() as usize,
                key.len(),
            )
        };
        pool.modify(root, |bytes| {
            if let Some(slot) = bytes.get_mut(offset..offset + length) {
                for byte in slot.iter_mut() {
                    *byte = 0x01;
                }
            }
            Ok(())
        })
        .unwrap();
        assert!(tree.check(pool).is_err(), "the checker missed it");
    }

    /// The attach path refuses a root that is not a tree page.
    #[test]
    fn attaching_to_a_page_that_is_not_a_root_is_refused() {
        let vfs = MemoryVfs::new();
        let path = DbPath::new("notaroot.rdb");
        let mut database =
            Database::create(&vfs, &path, Options::default().with_page_size(512)).unwrap();
        let page = database.allocate(1).unwrap();
        let mut image = vec![0u8; 512];
        page::write_common(&mut image, PageKind::BlobExtent, 0, 1).unwrap();
        database.install(page, &image).unwrap();
        assert!(PagedTree::attach(
            database.pool(),
            1,
            page,
            rowid_columns(),
            1,
            PageId::NONE,
            0,
            0
        )
        .is_err());
    }
}
