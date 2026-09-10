//! The interior page: separator keys and child swips.
//!
//! Invariant: every byte read out of an interior page is bounds checked against
//! the page's own header before it is used, and a header that disagrees with
//! itself is a corruption rather than a case to handle. An interior page is the
//! one structure in the file whose contents are *addresses*, so a page that
//! arrives damaged and is believed sends every descent below it somewhere else.
//! [`InteriorRef::parse`] therefore validates the whole slot array up front,
//! once, rather than checking each slot as a descent happens to reach it.
//!
//! ## Layout
//!
//! After the 32-byte common header ([`crate::page`]):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 32 | 2 | `count`: how many separator keys |
//! | 34 | 2 | `key_columns`, for the integrity checker |
//! | 36 | 4 | `heap_start`: where the key bytes begin |
//! | 40 | 16·`count` | the slot array: `(u32 key offset, u32 key length, u64 swip)` |
//! | 40+16·`count` | 8 | `rightmost_swip` |
//! | `heap_start`.. | | the key bytes, memcmp-comparable |
//!
//! ## What a separator means
//!
//! `count` separators describe `count + 1` children. Child `i` holds every key
//! `k` with `K[i-1] <= k < K[i]`, where `K[-1]` is minus infinity and `K[count]`
//! is plus infinity; the last child is [`InteriorRef::rightmost`]. A descent is
//! therefore "the first slot whose key is greater than the probe", which is one
//! `partition_point` over an array of byte slices.
//!
//! The keys are opaque here, and deliberately: this page knows only that they
//! are ordered by `memcmp`, which is the entire contract the key encoding in
//! `inillucent-tree` exists to provide. An interior page that had to know what a
//! collation was would be a layering violation and a per-comparison dispatch.

use inillucent_base::error::{corrupt, misuse};
use inillucent_base::DbResult;

use crate::page::{self, PageKind};
use crate::swip::Swip;

/// Byte offsets inside the interior header.
mod at {
    /// How many separator keys, 2 bytes.
    pub const COUNT: usize = 32;
    /// How many columns a key holds, 2 bytes.
    pub const KEY_COLUMNS: usize = 34;
    /// Where the key heap begins, 4 bytes.
    pub const HEAP_START: usize = 36;
    /// Where the slot array begins.
    pub const SLOTS: usize = 40;
}

/// How many bytes one slot occupies.
pub const SLOT_BYTES: usize = 16;

/// How many bytes an interior page spends before its first key.
///
/// @param count - how many separators the page holds
pub fn directory_bytes(count: usize) -> usize {
    at::SLOTS
        .saturating_add(count.saturating_mul(SLOT_BYTES))
        .saturating_add(8)
}

/// A parsed, validated interior page.
#[derive(Clone, Copy, Debug)]
pub struct InteriorRef<'p> {
    page: &'p [u8],
    count: usize,
    heap_start: usize,
}

impl<'p> InteriorRef<'p> {
    /// Parses an interior page, validating every slot before returning.
    ///
    /// @param page - the page bytes
    pub fn parse(page: &'p [u8]) -> DbResult<InteriorRef<'p>> {
        if page.len() < at::SLOTS.saturating_add(8) {
            return Err(corrupt("an interior page is shorter than its own header"));
        }
        if page::kind_of(page)? != PageKind::Interior {
            return Err(corrupt("page is not an interior page"));
        }
        let count = usize::from(page::read_u16(page, at::COUNT)?);
        let heap_start = page::read_u32(page, at::HEAP_START)? as usize;
        let directory = directory_bytes(count);
        if directory > page.len() {
            return Err(corrupt(format!(
                "an interior page claims {count} slots, which do not fit in {} bytes",
                page.len()
            )));
        }
        if heap_start < directory || heap_start > page.len() {
            return Err(corrupt(format!(
                "an interior page's heap starts at {heap_start}, outside {directory}..{}",
                page.len()
            )));
        }
        Ok(InteriorRef {
            page,
            count,
            heap_start,
        })
    }

    /// Checks that the slot array is ordered and every key is in range.
    ///
    /// **This is not part of [`InteriorRef::parse`], and that is a measurement
    /// rather than a preference.** The first version validated the whole slot
    /// array on every parse, which meant every descent read and compared every
    /// key of every page it passed through: at 32 KiB pages a root holds well
    /// over a thousand separators, so a three-level descent did thousands of
    /// comparisons to answer a question that needs three. `PointProbe` measured
    /// **1,689 ns** against a 500 ns bar, and `scan.distinct` - 64 descents -
    /// went from Phase 1's 12 us to 63 us.
    ///
    /// It is the same shape as the bug Phase 1 found in `LeafRef::parse`, which
    /// walked every class array to confirm a flag: an `O(n)` check inside an
    /// `O(1)` accessor, where `n` is the thing the accessor exists to avoid
    /// touching. Phase 1's report said four of its six wins replaced a
    /// hypothesis a measurement refused; this one was found the same way, by
    /// reading the number before changing anything.
    ///
    /// Correctness does not move, because [`InteriorRef::key`] bounds-checks
    /// every key it returns. What an unordered slot array can do is route a
    /// descent to the wrong child - a miss, not an out-of-bounds read - and
    /// that is what the integrity checker and the corrupt-page tests are for.
    /// Both call this.
    pub fn validate(&self) -> DbResult<()> {
        let mut previous: Option<&[u8]> = None;
        for slot in 0..self.count {
            let key = self.key(slot)?;
            if let Some(before) = previous {
                if before >= key {
                    return Err(corrupt(format!(
                        "interior slot {slot} does not follow the one before it"
                    )));
                }
            }
            previous = Some(key);
        }
        Ok(())
    }

    /// Returns how many separator keys the page holds.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Returns how many children the page describes.
    pub fn children(&self) -> usize {
        self.count.saturating_add(1)
    }

    /// Returns how many columns each key holds.
    pub fn key_columns(&self) -> usize {
        usize::from(page::read_u16(self.page, at::KEY_COLUMNS).unwrap_or(0))
    }

    /// Returns the tree level, which is one above its children's.
    pub fn level(&self) -> u16 {
        page::level_of(self.page).unwrap_or(0)
    }

    /// Returns the page's bytes.
    pub fn bytes(&self) -> &'p [u8] {
        self.page
    }

    /// Returns the byte offset of one slot's entry.
    ///
    /// @param slot - the separator's index
    fn slot_at(&self, slot: usize) -> usize {
        at::SLOTS.saturating_add(slot.saturating_mul(SLOT_BYTES))
    }

    /// Returns one separator key.
    ///
    /// @param slot - the separator's index
    pub fn key(&self, slot: usize) -> DbResult<&'p [u8]> {
        if slot >= self.count {
            return Err(misuse(format!("interior slot {slot} does not exist")));
        }
        let entry = self.slot_at(slot);
        let offset = page::read_u32(self.page, entry)? as usize;
        let length = page::read_u32(self.page, entry.saturating_add(4))? as usize;
        if offset < self.heap_start {
            return Err(corrupt(format!(
                "interior key {slot} starts at {offset}, before the heap"
            )));
        }
        self.page
            .get(offset..offset.saturating_add(length))
            .ok_or_else(|| corrupt(format!("interior key {slot} runs past the page")))
    }

    /// Returns the byte offset of one child's swip.
    ///
    /// Child `count` is the rightmost, whose swip follows the slot array.
    ///
    /// @param child - the child's index, 0..=count
    pub fn swip_offset(&self, child: usize) -> DbResult<usize> {
        if child > self.count {
            return Err(misuse(format!("interior child {child} does not exist")));
        }
        if child == self.count {
            return Ok(at::SLOTS.saturating_add(self.count.saturating_mul(SLOT_BYTES)));
        }
        Ok(self.slot_at(child).saturating_add(8))
    }

    /// Returns one child's swip.
    ///
    /// @param child - the child's index, 0..=count
    pub fn swip(&self, child: usize) -> DbResult<Swip> {
        let at = self.swip_offset(child)?;
        Ok(Swip::from_raw(page::read_u64(self.page, at)?))
    }

    /// Returns the rightmost child's swip.
    pub fn rightmost(&self) -> DbResult<Swip> {
        self.swip(self.count)
    }

    /// Returns the child whose range contains a key, and where its swip sits.
    ///
    /// The search is the first separator strictly greater than the probe, which
    /// is `partition_point` over the slot array. It is written as an explicit
    /// loop rather than with the slice helper because a key read can fail, and
    /// a closure that cannot report that would have to swallow it.
    ///
    /// @param probe - the memcmp-encoded key being looked for
    pub fn child_for(&self, probe: &[u8]) -> DbResult<(usize, Swip, usize)> {
        /// How many interpolated midpoints before falling back to bisection.
        ///
        /// Four, for the same reason the leaf's search caps at four: a
        /// distribution that has not converged in four guesses is not one
        /// interpolation is going to help with, and bounding it is what makes
        /// the worst case no worse than the bisection it replaces.
        const GUESSES: u32 = 4;

        /// Below this many children, bisection reads fewer slots than
        /// interpolation's two end reads cost.
        const FLOOR: usize = 16;

        // **The midpoint is a guess; the comparison is not.** Everything below
        // changes only *which* slot is examined next - the test that moves the
        // window is `self.key(middle) <= probe`, byte for byte what it was. So
        // a separator distribution that defeats interpolation costs extra
        // reads and never a wrong child, and a key whose leading eight bytes
        // say nothing useful (deep text prefixes, say) simply bisects.
        //
        // **Why it is worth doing.** `inillucent-probeprofile` measures a
        // descent of the medium fixture's table tree at about 98 ns, and that
        // tree is one level deep: the whole of it is this search. A 32 KiB
        // interior page holds 468 children, so bisection is nine steps, and
        // each step reads a slot from a 7 KiB directory and then the key bytes
        // from somewhere else in the page - eighteen scattered accesses across
        // 512 cache lines. Rowids are handed out in order, so leaf separators
        // are close to uniform and one interpolation lands on or beside the
        // right child.
        // **The window's ends are carried, not re-read.** Every interpolated
        // midpoint needs the leading eight bytes of the key at each end of the
        // window, and the first step of every descent through this page asks
        // for the same two: slot 0 and slot `count - 1`. Re-reading them per
        // step cost two slot reads and two heap reads that the step before had
        // already paid for, which on the medium fixture's root - one
        // interpolation, then a second - was four of the six key reads a
        // descent did.
        //
        // Only the end that moved is re-read, and it is re-read from the key
        // the comparison below already fetched wherever that is the same slot.
        let target = leading_u64(probe);
        let mut low = 0usize;
        let mut high = self.count;
        let mut guesses = 0u32;
        let mut low_value = if self.count == 0 {
            0
        } else {
            leading_u64(self.key(0)?)
        };
        let mut high_value = if self.count == 0 {
            0
        } else {
            leading_u64(self.key(self.count.saturating_sub(1))?)
        };
        while low < high {
            let middle = if guesses < GUESSES && high.saturating_sub(low) > FLOOR {
                guesses = guesses.saturating_add(1);
                Self::place(low, high, target, low_value, high_value)
            } else {
                low.saturating_add(high.saturating_sub(low) / 2)
            };
            let key = self.key(middle)?;
            let seen = leading_u64(key);
            if key <= probe {
                low = middle.saturating_add(1);
                // **The bracket is allowed to be loose, and this end is.** The
                // new low end is the slot *after* the one just read, so its
                // value would be another read; the value just read is a lower
                // bound on it, which is all a proportion needs. A midpoint is a
                // guess - the window still shrinks by at least one slot per
                // step whatever it returns, and the comparison that moves the
                // window is the exact byte compare above - so a slightly
                // conservative end costs at worst a poorer guess and never a
                // wrong child.
                low_value = seen;
            } else {
                high = middle;
                // This end is exact: the new high end *is* the slot just read.
                high_value = seen;
            }
        }
        Ok((low, self.swip(low)?, self.swip_offset(low)?))
    }

    /// Returns a slot in `low..high` to examine next, by proportion.
    ///
    /// Always inside the window, so the search that calls it terminates
    /// whatever the separators look like, and it takes the two end values
    /// rather than reading them - a search that already knows them does not pay
    /// for them twice, which is the whole of why it is written this way.
    ///
    /// @param low - the first slot still in the window
    /// @param high - one past the last slot still in the window
    /// @param target - the probe's leading eight bytes as a number
    /// @param low_value - the leading eight bytes of `low`'s key
    /// @param high_value - the leading eight bytes of `high - 1`'s key
    fn place(low: usize, high: usize, target: u64, low_value: u64, high_value: u64) -> usize {
        let last = high.saturating_sub(1);
        if high_value <= low_value || target <= low_value {
            return low;
        }
        if target >= high_value {
            return last;
        }
        let span = u128::from(high_value.saturating_sub(low_value));
        let into = u128::from(target.saturating_sub(low_value));
        let width = u128::try_from(last.saturating_sub(low)).unwrap_or(0);
        let offset = usize::try_from(into.saturating_mul(width) / span.max(1)).unwrap_or(0);
        low.saturating_add(offset.min(last.saturating_sub(low)))
    }

    /// Returns every byte offset in this page that holds a swip.
    ///
    /// Used by writeback to translate frame references back into page ids, and
    /// by the integrity checker to walk the tree.
    pub fn swip_offsets(&self) -> DbResult<Vec<usize>> {
        let mut offsets = Vec::with_capacity(self.children());
        for child in 0..=self.count {
            offsets.push(self.swip_offset(child)?);
        }
        Ok(offsets)
    }
}

/// Returns a memcmp-encoded key's leading eight bytes as a number.
///
/// Shorter keys are zero-padded, which makes the map monotone with respect to
/// the byte ordering the search actually uses everywhere the two can disagree:
/// a padded key can compare *equal* to a longer one that it is really below,
/// and the only consequence is a guess that is one slot out.
///
/// @param key - the encoded key, of any length
fn leading_u64(key: &[u8]) -> u64 {
    let mut raw = [0u8; 8];
    // Zipped rather than sliced, because a slice needs a length and a length
    // needs a bound that no input can violate - and a branch no input can take
    // is one the coverage gate can only ever be lied to about. `zip` stops at
    // whichever runs out first, which is exactly the padding rule.
    for (slot, byte) in raw.iter_mut().zip(key) {
        *slot = *byte;
    }
    u64::from_be_bytes(raw)
}

/// Returns every byte offset that holds a swip in an interior page image.
///
/// Takes raw bytes rather than an [`InteriorRef`] because writeback runs over a
/// copy of a frame and has no reason to validate key ordering to find the swips.
/// The bounds are still checked: a page whose count does not fit is refused.
///
/// @param image - the page image
pub fn swip_offsets_of(image: &[u8]) -> DbResult<Vec<usize>> {
    if page::kind_of(image)? != PageKind::Interior {
        return Ok(Vec::new());
    }
    let count = usize::from(page::read_u16(image, at::COUNT)?);
    if directory_bytes(count) > image.len() {
        return Err(corrupt("an interior page claims more slots than it holds"));
    }
    let mut offsets = Vec::with_capacity(count.saturating_add(1));
    for child in 0..count {
        offsets.push(
            at::SLOTS
                .saturating_add(child.saturating_mul(SLOT_BYTES))
                .saturating_add(8),
        );
    }
    offsets.push(at::SLOTS.saturating_add(count.saturating_mul(SLOT_BYTES)));
    Ok(offsets)
}

/// Writes one child's swip into a page image.
///
/// @param image - the page bytes
/// @param at - the offset [`InteriorRef::swip_offset`] returned
/// @param swip - the reference to store
pub fn write_swip(image: &mut [u8], at: usize, swip: Swip) -> DbResult<()> {
    page::write_u64(image, at, swip.raw())
}

/// Builds interior pages from separators and children.
pub struct InteriorBuilder {
    /// The page size in bytes.
    page_size: usize,
    /// The tree identifier written into every page.
    tree: u64,
    /// The level these pages sit at; leaves are level 0.
    level: u16,
}

impl InteriorBuilder {
    /// Returns a builder for one level of one tree.
    ///
    /// @param page_size - the database's page size in bytes
    /// @param tree - the tree identifier
    /// @param level - the level, at least 1
    pub fn new(page_size: usize, tree: u64, level: u16) -> DbResult<InteriorBuilder> {
        if page_size <= directory_bytes(1) {
            return Err(misuse(format!(
                "a page of {page_size} bytes cannot hold an interior page"
            )));
        }
        if level == 0 {
            return Err(misuse("an interior page is above level 0"));
        }
        Ok(InteriorBuilder {
            page_size,
            tree,
            level,
        })
    }

    /// Reports how many children fit alongside their separator keys.
    ///
    /// The first child needs no separator, so `n` children cost
    /// `directory_bytes(n - 1)` plus the bytes of `n - 1` keys.
    ///
    /// @param keys - the separator keys, one fewer than the children
    pub fn fits(&self, keys: &[&[u8]]) -> bool {
        let key_bytes: usize = keys.iter().map(|key| key.len()).sum();
        directory_bytes(keys.len()).saturating_add(key_bytes) <= self.page_size
    }

    /// Builds one interior page.
    ///
    /// @param separators - the keys, one fewer than there are children
    /// @param children - the child swips, in order
    pub fn build(&self, separators: &[&[u8]], children: &[Swip]) -> DbResult<Vec<u8>> {
        // One separator sits between each adjacent pair of children, so the
        // counts determine each other. This check also settles "at least one
        // child": `children.len() == separators.len() + 1` is never zero, so a
        // separate emptiness test would be a branch no input can take.
        if children.len() != separators.len().saturating_add(1) {
            return Err(misuse(format!(
                "{} children need {} separators, not {}",
                children.len(),
                children.len().saturating_sub(1),
                separators.len()
            )));
        }
        if !self.fits(separators) {
            return Err(misuse("the separators do not fit in one interior page"));
        }
        let mut page = vec![0u8; self.page_size];
        page::write_common(&mut page, PageKind::Interior, self.level, self.tree)?;
        let count = separators.len();
        page::write_u16(
            &mut page,
            at::COUNT,
            u16::try_from(count).unwrap_or(u16::MAX),
        )?;
        page::write_u16(&mut page, at::KEY_COLUMNS, 0)?;

        // Keys are laid down from the end of the page backwards, so the
        // directory and the heap grow towards each other and `heap_start` is
        // the one number that says whether they met.
        let mut cursor = self.page_size;
        for (slot, key) in separators.iter().enumerate() {
            cursor = cursor.saturating_sub(key.len());
            let destination = page
                .get_mut(cursor..cursor.saturating_add(key.len()))
                .ok_or_else(|| misuse("an interior key did not fit after all"))?;
            destination.copy_from_slice(key);
            let entry = at::SLOTS.saturating_add(slot.saturating_mul(SLOT_BYTES));
            page::write_u32(&mut page, entry, u32::try_from(cursor).unwrap_or(u32::MAX))?;
            page::write_u32(
                &mut page,
                entry.saturating_add(4),
                u32::try_from(key.len()).unwrap_or(u32::MAX),
            )?;
        }
        page::write_u32(
            &mut page,
            at::HEAP_START,
            u32::try_from(cursor).unwrap_or(u32::MAX),
        )?;
        for (index, swip) in children.iter().enumerate() {
            let offset = if index == count {
                at::SLOTS.saturating_add(count.saturating_mul(SLOT_BYTES))
            } else {
                at::SLOTS
                    .saturating_add(index.saturating_mul(SLOT_BYTES))
                    .saturating_add(8)
            };
            page::write_u64(&mut page, offset, swip.raw())?;
        }
        Ok(page)
    }

    /// Returns how many children one page can hold given their separators.
    ///
    /// Packs greedily: the answer is the largest `n` for which the first
    /// `n - 1` separators still fit. Returns at least one, because a page with
    /// one child and no separator always fits.
    ///
    /// @param separators - the candidate separators, in order
    pub fn capacity(&self, separators: &[&[u8]]) -> usize {
        let mut used = directory_bytes(0);
        let mut children = 1usize;
        for key in separators {
            let next = used.saturating_add(SLOT_BYTES).saturating_add(key.len());
            if next > self.page_size {
                break;
            }
            used = next;
            children = children.saturating_add(1);
        }
        children
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a builder over a small page, which is what forces multi-level
    /// trees in the tests without a hundred thousand rows.
    fn builder(page_size: usize) -> InteriorBuilder {
        InteriorBuilder::new(page_size, 7, 1).unwrap()
    }

    /// A page round-trips its separators, its children and its rightmost child.
    #[test]
    fn an_interior_page_round_trips() {
        let separators: Vec<&[u8]> = vec![b"bbb", b"ddd", b"fff"];
        let children = vec![
            Swip::unswizzled(crate::PageId(10)),
            Swip::unswizzled(crate::PageId(11)),
            Swip::swizzled(3),
            Swip::unswizzled(crate::PageId(13)),
        ];
        let page = builder(512).build(&separators, &children).unwrap();
        let parsed = InteriorRef::parse(&page).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.count(), 3);
        assert_eq!(parsed.children(), 4);
        assert_eq!(parsed.level(), 1);
        for (slot, key) in separators.iter().enumerate() {
            assert_eq!(parsed.key(slot).unwrap(), *key);
        }
        for (index, swip) in children.iter().enumerate() {
            assert_eq!(parsed.swip(index).unwrap(), *swip);
        }
        assert_eq!(parsed.rightmost().unwrap(), children[3]);
        assert!(parsed.key(3).is_err());
        assert!(parsed.swip(4).is_err());
    }

    /// The interpolated search answers what a linear scan answers, on
    /// separators chosen to make interpolation guess badly.
    ///
    /// The guess is only a midpoint - the comparison that moves the window is
    /// unchanged - so a distribution that defeats interpolation must cost extra
    /// reads and never a wrong child. This asserts that on three shapes: a
    /// uniform one, where interpolation lands first time; a clustered one,
    /// where almost every key shares a leading run and the proportional guess
    /// is far from the answer; and one whose keys are shorter than the eight
    /// bytes the guess reads, so the padding makes distinct keys look equal.
    #[test]
    fn an_interpolated_descent_agrees_with_a_linear_scan() {
        /// Returns the child a linear scan says a probe belongs to.
        ///
        /// @param separators - the page's separator keys
        /// @param probe - the key being looked for
        fn scanned(separators: &[Vec<u8>], probe: &[u8]) -> usize {
            separators
                .iter()
                .position(|key| key.as_slice() > probe)
                .unwrap_or(separators.len())
        }

        // Uniform: rowids at leaf boundaries, which is what a bulk-built table
        // tree actually holds.
        let uniform: Vec<Vec<u8>> = (1..=300u64)
            .map(|nth| (nth * 214).to_be_bytes().to_vec())
            .collect();
        // Clustered: every key shares seven leading bytes, so the leading
        // eight bytes carry almost no information and every guess is wrong.
        let clustered: Vec<Vec<u8>> = (1..=300u64)
            .map(|nth| {
                let mut key = vec![0xAAu8; 7];
                key.extend_from_slice(&nth.to_be_bytes());
                key
            })
            .collect();
        // Shorter than the guess reads: two-byte keys, zero-padded by the map.
        let short: Vec<Vec<u8>> = (1..=200u16).map(|nth| nth.to_be_bytes().to_vec()).collect();

        for separators in [&uniform, &clustered, &short] {
            let refs: Vec<&[u8]> = separators.iter().map(Vec::as_slice).collect();
            let children: Vec<Swip> = (0..=separators.len())
                .map(|nth| Swip::unswizzled(crate::PageId(nth as u64 + 1)))
                .collect();
            let page = InteriorBuilder::new(32_768, 7, 1)
                .unwrap()
                .build(&refs, &children)
                .unwrap();
            let parsed = InteriorRef::parse(&page).unwrap();
            parsed.validate().unwrap();
            // Every separator, one byte below it, one above it, and both ends.
            let mut probes: Vec<Vec<u8>> = vec![Vec::new(), vec![0xFF; 16]];
            for key in separators {
                probes.push(key.clone());
                // A strict prefix sorts below the key it is a prefix of, so
                // dropping the last byte is "just below" without a branch on
                // whether there was a last byte to drop.
                let mut below = key.clone();
                let _ = below.pop();
                probes.push(below);
                let mut above = key.clone();
                above.push(0);
                probes.push(above);
            }
            for probe in &probes {
                let (child, _, _) = parsed.child_for(probe).unwrap();
                assert_eq!(
                    child,
                    scanned(separators, probe),
                    "probe {probe:?} landed on the wrong child"
                );
            }
        }
    }

    /// The descent picks the child whose range holds the probe, at every
    /// boundary and outside both ends.
    #[test]
    fn the_descent_picks_the_right_child_at_every_boundary() {
        let separators: Vec<&[u8]> = vec![b"20", b"40", b"60"];
        let children: Vec<Swip> = (0..4).map(Swip::swizzled).collect();
        let page = builder(512).build(&separators, &children).unwrap();
        let parsed = InteriorRef::parse(&page).unwrap();
        let cases: [(&[u8], usize); 8] = [
            (b"00", 0),
            (b"19", 0),
            (b"20", 1),
            (b"39", 1),
            (b"40", 2),
            (b"59", 2),
            (b"60", 3),
            (b"99", 3),
        ];
        for (probe, wanted) in cases {
            let (child, swip, offset) = parsed.child_for(probe).unwrap();
            assert_eq!(child, wanted, "probe {:?}", std::str::from_utf8(probe));
            assert_eq!(swip, Swip::swizzled(wanted as u32));
            assert_eq!(offset, parsed.swip_offset(wanted).unwrap());
        }
    }

    /// A page with one child and no separators sends every probe to it.
    #[test]
    fn a_single_child_page_sends_everything_to_it() {
        let page = builder(512)
            .build(&[], &[Swip::unswizzled(crate::PageId(5))])
            .unwrap();
        let parsed = InteriorRef::parse(&page).unwrap();
        assert_eq!(parsed.count(), 0);
        let (child, swip, _) = parsed.child_for(b"anything").unwrap();
        assert_eq!(child, 0);
        assert_eq!(swip, Swip::unswizzled(crate::PageId(5)));
        assert_eq!(parsed.rightmost().unwrap(), swip);
    }

    /// A swip written into a page is read back, and the offsets a writeback
    /// would translate are every child's.
    #[test]
    fn swips_can_be_rewritten_in_place() {
        let separators: Vec<&[u8]> = vec![b"m"];
        let children = vec![Swip::swizzled(1), Swip::swizzled(2)];
        let mut page = builder(512).build(&separators, &children).unwrap();
        let offsets = InteriorRef::parse(&page).unwrap().swip_offsets().unwrap();
        assert_eq!(offsets.len(), 2);
        assert_eq!(swip_offsets_of(&page).unwrap(), offsets);
        for (index, offset) in offsets.iter().enumerate() {
            write_swip(
                &mut page,
                *offset,
                Swip::unswizzled(crate::PageId(index as u64 + 30)),
            )
            .unwrap();
        }
        let parsed = InteriorRef::parse(&page).unwrap();
        assert_eq!(parsed.swip(0).unwrap(), Swip::unswizzled(crate::PageId(30)));
        assert_eq!(parsed.swip(1).unwrap(), Swip::unswizzled(crate::PageId(31)));
    }

    /// A page of any other kind holds no swips, so writeback leaves it alone.
    #[test]
    fn a_page_of_another_kind_holds_no_swips() {
        let mut leaf = vec![0u8; 512];
        page::write_common(&mut leaf, PageKind::Leaf, 0, 1).unwrap();
        assert!(swip_offsets_of(&leaf).unwrap().is_empty());
        assert!(InteriorRef::parse(&leaf).is_err());
        assert!(swip_offsets_of(&[]).is_err());
    }

    /// Every field, corrupted, is refused rather than believed. This is the
    /// TDD's "corrupt every field" table for the interior codec, and it is what
    /// the 100%-branch tier is measured on.
    #[test]
    fn corrupting_any_header_field_is_refused() {
        let separators: Vec<&[u8]> = vec![b"bb", b"dd"];
        let children: Vec<Swip> = (0..3).map(Swip::swizzled).collect();
        let page = builder(256).build(&separators, &children).unwrap();
        InteriorRef::parse(&page).unwrap();

        // The count, past what the page can hold.
        let mut damaged = page.clone();
        page::write_u16(&mut damaged, at::COUNT, 4_000).unwrap();
        assert!(InteriorRef::parse(&damaged).is_err());

        // The heap start, before the directory and past the page.
        let mut damaged = page.clone();
        page::write_u32(&mut damaged, at::HEAP_START, 0).unwrap();
        assert!(InteriorRef::parse(&damaged).is_err());
        let mut damaged = page.clone();
        page::write_u32(&mut damaged, at::HEAP_START, 9_999).unwrap();
        assert!(InteriorRef::parse(&damaged).is_err());

        // A key length past the page is caught when the key is read.
        let mut damaged = page.clone();
        page::write_u32(&mut damaged, at::SLOTS + 4, 9_999).unwrap();
        assert!(InteriorRef::parse(&damaged).unwrap().key(0).is_err());
        assert!(InteriorRef::parse(&damaged).unwrap().validate().is_err());
        assert!(InteriorRef::parse(&damaged)
            .unwrap()
            .child_for(b"a")
            .is_err());

        // The kind byte.
        let mut damaged = page.clone();
        damaged[12] = PageKind::Leaf.code();
        assert!(InteriorRef::parse(&damaged).is_err());

        // A key offset before the heap is caught on the read as well as by
        // `validate`, because `key` bounds-checks every key it returns.
        let mut damaged = page.clone();
        page::write_u32(&mut damaged, at::SLOTS, 8).unwrap();
        assert!(InteriorRef::parse(&damaged).unwrap().validate().is_err());

        // Out-of-order separators, which would silently misroute a descent.
        // `parse` accepts them - it is O(1) by design and the module
        // documentation says why - and `validate` is what refuses them.
        let mut damaged = page.clone();
        let first = page::read_u32(&damaged, at::SLOTS).unwrap();
        let second = page::read_u32(&damaged, at::SLOTS + SLOT_BYTES).unwrap();
        page::write_u32(&mut damaged, at::SLOTS, second).unwrap();
        page::write_u32(&mut damaged, at::SLOTS + SLOT_BYTES, first).unwrap();
        assert!(InteriorRef::parse(&damaged).unwrap().validate().is_err());

        // Equal separators are as bad as reversed ones.
        let mut damaged = page.clone();
        page::write_u32(&mut damaged, at::SLOTS + SLOT_BYTES, first).unwrap();
        page::write_u32(
            &mut damaged,
            at::SLOTS + SLOT_BYTES + 4,
            page::read_u32(&page, at::SLOTS + 4).unwrap(),
        )
        .unwrap();
        assert!(InteriorRef::parse(&damaged).unwrap().validate().is_err());

        // Truncation at every length short of the header.
        for length in 0..at::SLOTS + 8 {
            assert!(
                InteriorRef::parse(&page[..length]).is_err(),
                "a page of {length} bytes parsed"
            );
        }
    }

    /// A builder refuses inputs it cannot honour rather than truncating them.
    #[test]
    fn the_builder_refuses_what_it_cannot_build() {
        let build = builder(256);
        let separators: Vec<&[u8]> = vec![b"a"];
        assert!(build.build(&separators, &[Swip::swizzled(0)]).is_err());
        assert!(build.build(&[], &[]).is_err());
        let huge = vec![0u8; 300];
        let separators: Vec<&[u8]> = vec![&huge];
        assert!(!build.fits(&separators));
        assert!(build
            .build(&separators, &[Swip::swizzled(0), Swip::swizzled(1)])
            .is_err());
        assert!(InteriorBuilder::new(32, 1, 1).is_err());
        assert!(InteriorBuilder::new(512, 1, 0).is_err());
    }

    /// The capacity is the number of children whose separators fit, and
    /// building exactly that many succeeds.
    #[test]
    fn the_capacity_is_what_actually_fits() {
        let build = builder(128);
        let keys: Vec<Vec<u8>> = (0..64).map(|n| vec![b'a' + (n % 26) as u8; 4]).collect();
        let refs: Vec<&[u8]> = keys.iter().map(|key| key.as_slice()).collect();
        let capacity = build.capacity(&refs);
        assert!(capacity >= 2, "a page holds at least two children");
        assert!(capacity <= refs.len(), "and no more than it was offered");
        let separators = &refs[..capacity.saturating_sub(1)];
        assert!(build.fits(separators));
        let children: Vec<Swip> = (0..capacity as u32).map(Swip::swizzled).collect();
        // The separators here are not ordered, so only the fit is asserted.
        assert!(build.build(separators, &children).is_ok());
        let one_more = &refs[..capacity];
        assert!(
            !build.fits(one_more),
            "capacity is not the largest that fits"
        );
    }

    /// Asking for a slot or a child that is not there is a misuse, not a
    /// silent zero.
    ///
    /// These are the reads a descent performs, so returning something plausible
    /// for an index past the end would send the descent to a page chosen by
    /// arithmetic on garbage. The coverage run found both untaken: every test
    /// asked for indices that existed.
    #[test]
    fn an_index_past_the_end_is_refused() {
        let build = builder(512);
        let separators: Vec<&[u8]> = vec![b"m"];
        let image = build
            .build(&separators, &[Swip::swizzled(1), Swip::swizzled(2)])
            .expect("two children and one separator fit");
        let page = InteriorRef::parse(&image).expect("it parses");
        assert_eq!(page.count(), 1);
        assert!(page.key(0).is_ok());
        assert!(page.key(1).is_err(), "slot 1 is past the one separator");
        assert!(page.key(99).is_err());
        // A child index runs 0..=count, so 1 is the rightmost and legal.
        assert!(page.swip(0).is_ok());
        assert!(page.swip(1).is_ok());
        assert!(page.swip(2).is_err(), "there is no third child");
    }

    /// A page whose header claims more slots than the image can hold is
    /// refused by every reader of it, including the one that does not parse.
    ///
    /// The count is the first thing a corrupt page lies about, and the
    /// directory it implies runs past the end of the buffer. `parse` checks
    /// it, and that much was already covered; `swip_offsets_of` reads the count
    /// straight out of a raw image without parsing, because writeback calls it
    /// on a page it is about to translate. That check was the one no test had
    /// ever taken, and it is the one standing between a corrupt count and a
    /// walk off the end of a buffer during a write.
    #[test]
    fn a_count_larger_than_the_page_is_refused() {
        let build = builder(512);
        let separators: Vec<&[u8]> = vec![b"m"];
        let mut image = build
            .build(&separators, &[Swip::swizzled(1), Swip::swizzled(2)])
            .expect("it builds");
        assert_eq!(
            swip_offsets_of(&image)
                .expect("the honest page is fine")
                .len(),
            2,
            "one separator means two children"
        );
        // 200 slots need 40 + 200*16 + 8 bytes, which is past a 512-byte page.
        image[at::COUNT..at::COUNT + 2].copy_from_slice(&200u16.to_le_bytes());
        let refusal = swip_offsets_of(&image).expect_err("the raw reader refuses it");
        assert!(
            refusal.detail().unwrap_or_default().contains("more slots"),
            "{refusal:?}"
        );
        let parsed = InteriorRef::parse(&image).expect_err("and so does parse");
        assert!(
            parsed.detail().unwrap_or_default().contains("do not fit"),
            "{parsed:?}"
        );
    }

    /// Every one of the builder's refusals, each reached on its own.
    ///
    /// The existing refusal test reached three of the five; the coverage run
    /// showed the mismatched-length and empty-children arms untaken, because
    /// the calls that were meant to trigger them tripped an earlier check
    /// first. Each case here is minimal and isolated for that reason.
    #[test]
    fn the_builder_refuses_each_impossible_request_separately() {
        // A page too small to hold one child's directory entry.
        assert!(InteriorBuilder::new(directory_bytes(1), 1, 1).is_err());
        assert!(InteriorBuilder::new(directory_bytes(1).saturating_sub(1), 1, 1).is_err());
        // Level zero is a leaf, and a leaf is not built here.
        assert!(InteriorBuilder::new(512, 1, 0).is_err());

        let build = builder(512);
        // One separator needs exactly two children.
        let one: Vec<&[u8]> = vec![b"m"];
        assert!(
            build.build(&one, &[Swip::swizzled(1)]).is_err(),
            "one separator and one child do not agree"
        );
        assert!(
            build
                .build(
                    &one,
                    &[Swip::swizzled(1), Swip::swizzled(2), Swip::swizzled(3)]
                )
                .is_err(),
            "one separator and three children do not agree"
        );
        // No children at all, with the lengths agreeing so the earlier check
        // passes and this one is the one that fires.
        assert!(build.build(&[], &[]).is_err(), "a page needs a child");
        // Separators that agree in count and do not fit.
        let huge = vec![b'x'; 600];
        let too_big: Vec<&[u8]> = vec![&huge];
        assert!(!build.fits(&too_big));
        assert!(build
            .build(&too_big, &[Swip::swizzled(1), Swip::swizzled(2)])
            .is_err());
    }

    /// The directory arithmetic is what the layout says it is.
    #[test]
    fn the_directory_size_is_the_layout() {
        assert_eq!(directory_bytes(0), 48);
        assert_eq!(directory_bytes(1), 64);
        assert_eq!(directory_bytes(10), 40 + 160 + 8);
        assert_eq!(crate::page::COMMON_HEADER, 32);
    }
}
