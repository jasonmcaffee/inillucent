//! The entries a `CREATE INDEX` builds a tree from, in one arena.
//!
//! Invariant: **[`EntrySet::order`] produces exactly the order the tree
//! compares in.** The sort prefix is an accelerator and never a definition: it
//! is a genuine prefix of the tree's own key encoding, so it can fail to
//! separate two entries but can never place them the wrong way round, and what
//! it ties is ordered by [`compare_under`] - the comparison the tree's own
//! search and its integrity checker use. A tree whose leaves are not in its own
//! key order answers a scan exactly right and a seek wrongly, so the unit tests
//! here check the radix against that comparison over thousands of entries
//! rather than against a fixed expectation.
//!
//! ## Why an arena and not a `Vec<Vec<OwnedDatum>>`
//!
//! `CREATE INDEX main_label ON main_table(label)` over a hundred thousand rows
//! used to pay **three heap allocations per row**: one for the row's
//! `Vec<OwnedDatum>`, one for `OwnedDatum::Text`'s copy of the label, and one
//! more for the `Vec<Datum>` the bulk builder was handed a borrowed copy in. At
//! medium scale that is three hundred thousand allocations to build a tree of a
//! hundred and sixty-odd leaves, and it was most of the `schema` family's
//! distance from SQLite.
//!
//! An [`EntrySet`] holds the same information in three vectors, each reserved
//! once from the table's row count:
//!
//! - `cells`, `rows * width` fixed-size cells in entry order;
//! - `bytes`, every text and blob payload appended once;
//! - `prefix`, a fixed-width sort key per entry.
//!
//! Nothing is allocated per row, nothing is copied twice, and the rows handed
//! to the packer are `&[Datum]` slices into `cells` - which is why
//! [`inillucent_tree::leaf::LeafBuilder::pack_with`] is generic over
//! `AsRef<[Datum]>` rather than taking owned rows.
//!
//! ## Why the sort key is a fixed-width prefix and not the whole key
//!
//! The order the sort produces has to be the order **the tree itself** compares
//! in, or a descent binary-searches separators the leaves do not obey - the
//! defect `in_key_order`'s own comment describes, where an out-of-order import
//! answered a scan correctly and a seek wrongly. The obvious way to get that is
//! to encode each entry's whole key with the tree's own `KeyEncoding` and sort
//! the bytes.
//!
//! It was measured and it is too expensive. Encoding a hundred thousand
//! fifty-five-byte keys cost **5.5 ms of a 9.3 ms scan** - more than everything
//! else the scan does put together - to produce five and a half megabytes that
//! the sort then reads back in a random order.
//!
//! So each entry keeps the **first sixteen bytes** of that encoding, in two
//! machine words, and nothing more. Sixteen bytes is exact where it decides
//! anything: it is a genuine prefix of the full encoding, produced by the same
//! encoder, so ordering by it never contradicts ordering by the whole key.
//! Where two prefixes tie, the entries are ordered by
//! [`inillucent_tree::types::compare_under`] over their values - the comparison
//! the tree's own search and its integrity checker use. The prefix is therefore
//! a *speed* choice with a correct fallback behind it, and never a claim about
//! the order.

use inillucent_tree::datum::Datum;
use inillucent_tree::key;
use inillucent_tree::paged::KeyEncoding;
use inillucent_tree::types::compare_under;
use inillucent_value::collation::{nocase_key_bytes, Collation};

/// How many machine words of the encoded key an entry keeps.
///
/// **Two, because one was not enough and the reason is the data.** A single
/// eight-byte word left the medium fixture's entries in runs of about a
/// hundred: the keys are a class byte, then `row `, then a six-digit number, so
/// one word separates on only three of those digits, and ordering the runs it
/// left by comparing values cost more than the radix saved. Sixteen bytes reach
/// past the number and into the text, where the entries separate.
const PREFIX_WORDS: usize = 2;

/// How many bytes that is.
const PREFIX_BYTES: usize = PREFIX_WORDS * 8;

/// How short a run is before comparing beats counting.
///
/// A counting level clears two hundred and fifty-six counters whatever the run
/// holds, so below about that many entries the comparison is cheaper.
const COMPARE_BELOW: usize = 32;

/// One cell of one entry, pointing into the arena rather than owning bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cell {
    /// SQL NULL.
    Null,
    /// A signed 64-bit integer.
    Int(i64),
    /// An IEEE-754 binary64.
    Real(f64),
    /// UTF-8 text at `at`, `len` bytes long, in the arena.
    Text {
        /// Where the payload starts.
        at: u32,
        /// How long it is.
        len: u32,
    },
    /// Uninterpreted bytes at `at`, `len` bytes long, in the arena.
    Blob {
        /// Where the payload starts.
        at: u32,
        /// How long it is.
        len: u32,
    },
}

/// The entries of one index, arena-allocated and sortable by a radix pass.
pub(crate) struct EntrySet {
    /// How many columns an entry has: the key columns, then the row identity.
    width: usize,
    /// `rows * width` cells, in the order the entries were scanned.
    cells: Vec<Cell>,
    /// Every text and blob payload, appended once.
    bytes: Vec<u8>,
    /// `rows * PREFIX_WORDS` words: each entry's encoded-key prefix, most
    /// significant word first.
    prefix: Vec<u64>,
    /// The collations the key columns are ordered under.
    collations: Vec<Collation>,
    /// The direction of each key column.
    ///
    /// The prefix this set sorts by has to be inverted for a descending column
    /// exactly as the tree's own key is, or the radix pass would order the
    /// entries one way and the tree read them the other.
    directions: Vec<bool>,
    /// How the tree encodes its keys.
    encoding: KeyEncoding,
    /// A buffer the prefix encoder reuses, so it allocates nothing per entry.
    scratch: Vec<u8>,
}

impl EntrySet {
    /// Returns an empty set sized for a table's rows.
    ///
    /// @param width - how many columns an entry has
    /// @param rows - how many entries are expected
    /// @param encoding - the tree's key encoding
    /// @param collations - the key columns' collations, in key order
    /// @param directions - the key columns' directions, in key order
    pub(crate) fn with_capacity(
        width: usize,
        rows: usize,
        encoding: KeyEncoding,
        collations: &[Collation],
        directions: &[bool],
    ) -> EntrySet {
        EntrySet {
            width,
            cells: Vec::with_capacity(rows.saturating_mul(width)),
            // A guess, not a promise: the arena grows if it is wrong, and
            // starting near the right size is what keeps the growth to a
            // handful of reallocations rather than one per row.
            bytes: Vec::with_capacity(rows.saturating_mul(48)),
            prefix: Vec::with_capacity(rows.saturating_mul(PREFIX_WORDS)),
            collations: collations.to_vec(),
            encoding,
            directions: directions.to_vec(),
            scratch: Vec::with_capacity(PREFIX_BYTES.saturating_mul(2)),
        }
    }

    /// Appends one entry, copying its payloads and taking its sort prefix.
    ///
    /// The whole entry is the key of an index tree - the indexed columns and
    /// then the row's identity - so two entries can only compare equal if they
    /// are the same entry.
    ///
    /// @param values - the entry's columns, in tree order
    pub(crate) fn push(&mut self, values: &[Datum<'_>]) {
        for value in values.iter().take(self.width) {
            let cell = match value {
                Datum::Null => Cell::Null,
                Datum::Int(number) => Cell::Int(*number),
                Datum::Real(number) => Cell::Real(*number),
                Datum::Text(payload) => Cell::Text {
                    at: self.append(payload),
                    len: payload.len() as u32,
                },
                Datum::Blob(payload) => Cell::Blob {
                    at: self.append(payload),
                    len: payload.len() as u32,
                },
            };
            self.cells.push(cell);
        }
        // A short row is padded, so `cells` stays a rectangle and an entry's
        // columns are always at `entry * width`.
        for _ in values.len()..self.width {
            self.cells.push(Cell::Null);
        }
        self.scratch.clear();
        encode_prefix(
            self.encoding,
            values,
            &self.collations,
            &self.directions,
            &mut self.scratch,
        );
        for word in 0..PREFIX_WORDS {
            let base = word.saturating_mul(8);
            let mut packed = 0u64;
            for offset in 0..8 {
                let byte = self
                    .scratch
                    .get(base.saturating_add(offset))
                    .copied()
                    .unwrap_or(0);
                packed = (packed << 8) | u64::from(byte);
            }
            self.prefix.push(packed);
        }
    }

    /// Copies one payload into the arena and returns where it landed.
    ///
    /// @param payload - the bytes to keep
    fn append(&mut self, payload: &[u8]) -> u32 {
        let at = self.bytes.len() as u32;
        self.bytes.extend_from_slice(payload);
        at
    }

    /// How many entries there are.
    pub(crate) fn len(&self) -> usize {
        self.cells.len().checked_div(self.width.max(1)).unwrap_or(0)
    }

    /// Returns one word of an entry's sort prefix.
    ///
    /// @param entry - which entry
    /// @param word - which word, most significant first
    fn word(&self, entry: usize, word: usize) -> u64 {
        self.prefix
            .get(entry.saturating_mul(PREFIX_WORDS).saturating_add(word))
            .copied()
            .unwrap_or(0)
    }

    /// Returns one column of one entry as a value.
    ///
    /// @param entry - which entry
    /// @param column - which column of it
    fn datum(&self, entry: usize, column: usize) -> Datum<'_> {
        let cell = self
            .cells
            .get(entry.saturating_mul(self.width).saturating_add(column))
            .copied()
            .unwrap_or(Cell::Null);
        match cell {
            Cell::Null => Datum::Null,
            Cell::Int(number) => Datum::Int(number),
            Cell::Real(number) => Datum::Real(number),
            Cell::Text { at, len } => Datum::Text(self.payload(at, len)),
            Cell::Blob { at, len } => Datum::Blob(self.payload(at, len)),
        }
    }

    /// Orders two entries the way the tree they are building compares them.
    ///
    /// The comparison the tree's own search and its integrity checker use, over
    /// every column of the entry, with the entry's own position as the final
    /// tie-break so the order is total and deterministic.
    ///
    /// @param left - one entry
    /// @param right - the other
    fn compare(&self, left: u32, right: u32) -> std::cmp::Ordering {
        for column in 0..self.width {
            let order = compare_under(
                &self.datum(left as usize, column),
                &self.datum(right as usize, column),
                self.collations
                    .get(column)
                    .copied()
                    .unwrap_or(Collation::Binary),
            );
            // **And the direction, which the radix words already carry.** The
            // prefix is the tree's own encoding, so a `DESC` column arrives
            // here already inverted and the radix levels order it correctly;
            // this fallback compares the *values*, so it has to invert for
            // itself or the two halves of the same sort disagree. A run shorter
            // than `COMPARE_BELOW` never reaches the radix at all, which is why
            // `CREATE INDEX ic ON t(c DESC)` over nine rows built an ascending
            // tree that the reader then compared descending: every query over
            // it was wrong and `PRAGMA integrity_check` said so.
            let order = if self.directions.get(column).copied().unwrap_or(false) {
                order.reverse()
            } else {
                order
            };
            if order != std::cmp::Ordering::Equal {
                return order;
            }
        }
        left.cmp(&right)
    }

    /// Returns the entries in key order.
    ///
    /// **A radix sort over the prefix, one word at a time, then the tree's own
    /// comparison for whatever ties.** Each level sorts a run by one word; a run
    /// the level could not separate is handed to the next word, and whatever is
    /// still tied after the last one - or is short enough that counting is not
    /// worth it - is ordered by [`EntrySet::compare`].
    ///
    /// The prefix is a genuine prefix of the tree's own key encoding, so a level
    /// never puts two entries in an order the whole key would contradict; it can
    /// only fail to separate them, and that is what the fallback is for.
    pub(crate) fn order(&self) -> Vec<u32> {
        let count = self.len();
        let mut ranked: Vec<u32> = (0..count as u32).collect();
        if count < 2 {
            return ranked;
        }
        // Two buffers the levels ping-pong between, allocated once for the whole
        // sort rather than once per run.
        let mut pairs: Vec<(u64, u32)> = Vec::with_capacity(count);
        let mut scratch: Vec<(u64, u32)> = Vec::with_capacity(count);
        // The runs still to place, as `(start, end, word)`. The whole input is
        // one run at word zero.
        let mut pending: Vec<(usize, usize, usize)> = vec![(0, count, 0)];
        while let Some((from, to, level)) = pending.pop() {
            let width = to.saturating_sub(from);
            if width < 2 {
                continue;
            }
            if width < COMPARE_BELOW || level >= PREFIX_WORDS {
                if let Some(run) = ranked.get_mut(from..to) {
                    run.sort_unstable_by(|left, right| self.compare(*left, *right));
                }
                continue;
            }
            pairs.clear();
            for entry in ranked.get(from..to).unwrap_or(&[]) {
                pairs.push((self.word(*entry as usize, level), *entry));
            }
            radix_by_word(&mut pairs, &mut scratch);
            for (offset, (_, entry)) in pairs.iter().enumerate() {
                if let Some(slot) = ranked.get_mut(from.saturating_add(offset)) {
                    *slot = *entry;
                }
            }
            // Whatever this level tied goes to the next word.
            let mut at = 0usize;
            while at < width {
                let held = pairs.get(at).map(|(word, _)| *word).unwrap_or(0);
                let mut end = at.saturating_add(1);
                while pairs.get(end).is_some_and(|(word, _)| *word == held) {
                    end = end.saturating_add(1);
                }
                if end.saturating_sub(at) > 1 {
                    pending.push((
                        from.saturating_add(at),
                        from.saturating_add(end),
                        level.saturating_add(1),
                    ));
                }
                at = end;
            }
        }
        ranked
    }

    /// Writes the entries into a flat run of `Datum`s in the given order.
    ///
    /// The run is one allocation of `rows * width`, and the rows handed to the
    /// bulk builder are slices of it - so the builder reads the arena directly
    /// and nothing is copied a second time.
    ///
    /// @param order - the entry indices, in the order to write them
    pub(crate) fn to_datums(&self, order: &[u32]) -> Vec<Datum<'_>> {
        let mut out: Vec<Datum<'_>> = Vec::with_capacity(order.len().saturating_mul(self.width));
        for entry in order {
            for column in 0..self.width {
                out.push(self.datum(*entry as usize, column));
            }
        }
        out
    }

    /// Drops everything only the sort needed, before the pack that follows it.
    ///
    /// **The arena *is* the index build's high-water mark**, so what it holds
    /// while the leaves are being written is what the gate measures. The sort
    /// prefix is sixteen bytes an entry - 1.6 MiB at a hundred thousand rows -
    /// and it is dead the moment [`EntrySet::order`] has returned: the packer
    /// reads `cells` and `bytes`, and the fallback comparison reads values
    /// rather than the prefix. The scratch buffer goes with it.
    ///
    /// **The payload arena is deliberately left alone.** Shrinking `bytes` and
    /// `cells` to fit was measured with this and returned nothing - the whole
    /// 1.59 MiB the change is worth is the prefix - while the copy
    /// `shrink_to_fit` makes cost about 2 ms of a 27 ms statement. Freeing what
    /// is dead is free; compacting what is live is not.
    pub(crate) fn release_sort_scratch(&mut self) {
        self.prefix = Vec::new();
        self.scratch = Vec::new();
    }

    /// Returns the set in key order, as rows a leaf builder can pack.
    ///
    /// **The reason `to_datums` is no longer on the `CREATE INDEX` path.** The
    /// packer used to need a slice, so the arena was flattened into a
    /// `Vec<Datum>` in key order and then sliced into a `Vec<&[Datum]>` - two
    /// copies of the whole input, 6.4 MiB at a hundred thousand rows, purely to
    /// satisfy a signature. [`inillucent_tree::leaf::Rows`] reads values by
    /// index instead, and the arena already answers that question in
    /// [`EntrySet::datum`], so the copies are gone and the packer reads the
    /// arena directly.
    ///
    /// @param order - the entries in key order, from [`EntrySet::order`]
    pub(crate) fn in_order<'a>(&'a self, order: &'a [u32]) -> Ordered<'a> {
        Ordered { set: self, order }
    }

    /// Returns one payload out of the arena.
    ///
    /// @param at - where it starts
    /// @param len - how long it is
    fn payload(&self, at: u32, len: u32) -> &[u8] {
        let from = at as usize;
        let to = from.saturating_add(len as usize);
        self.bytes.get(from..to).unwrap_or(&[])
    }

    /// Reports whether two adjacent entries share their leading key columns.
    ///
    /// This is what a `UNIQUE` index's duplicate check asks. The entries are
    /// already in key order, so two rows with the same indexed columns are
    /// adjacent, and the comparison is the tree's own.
    ///
    /// A NULL is distinct from every other NULL in SQL, so an entry whose
    /// compared prefix holds one is never a duplicate.
    ///
    /// @param order - the entries in key order
    /// @param at - the position of the left entry of the pair
    /// @param compared - how many leading columns the index declares
    pub(crate) fn shares_key_prefix(&self, order: &[u32], at: usize, compared: usize) -> bool {
        let (Some(left), Some(right)) = (order.get(at), order.get(at.saturating_add(1))) else {
            return false;
        };
        let holds_null =
            (0..compared).any(|column| matches!(self.datum(*left as usize, column), Datum::Null));
        if holds_null {
            return false;
        }
        (0..compared).all(|column| {
            compare_under(
                &self.datum(*left as usize, column),
                &self.datum(*right as usize, column),
                self.collations
                    .get(column)
                    .copied()
                    .unwrap_or(Collation::Binary),
            ) == std::cmp::Ordering::Equal
        })
    }
}

/// Writes the first [`PREFIX_BYTES`] bytes of an entry's encoded key.
///
/// **The same encoder the tree uses, on payloads clipped to the prefix's
/// width.** Clipping a text or blob to sixteen bytes and encoding that produces
/// the same leading bytes as encoding the whole value - the escape only ever
/// lengthens a payload, so the clipped form still fills the prefix - which is
/// what makes this an exact prefix rather than a second encoding that has to be
/// kept in step with the first.
///
/// A collation is applied *before* the clip and the value is then encoded as
/// BINARY. Applying it afterwards would be wrong: `RTRIM` over a clip that
/// happens to end inside a run of spaces would strip spaces that are interior
/// to the real value, and the result would not be a prefix of anything.
///
/// @param encoding - the tree's key encoding
/// @param values - the entry's columns
/// @param collations - the key columns' collations
/// @param out - the buffer to write into, already empty
fn encode_prefix(
    encoding: KeyEncoding,
    values: &[Datum<'_>],
    collations: &[Collation],
    directions: &[bool],
    out: &mut Vec<u8>,
) {
    if encoding == KeyEncoding::Rowid {
        // Eight bytes, no class byte and no tail: the whole key is the prefix.
        encoding.encode_into(values, collations, directions, out);
        return;
    }
    let mut folded: Vec<u8> = Vec::new();
    for (index, value) in values.iter().enumerate() {
        if out.len() >= PREFIX_BYTES {
            return;
        }
        let collation = collations.get(index).copied().unwrap_or(Collation::Binary);
        let clipped = match value {
            Datum::Text(bytes) => Datum::Text(clip(bytes, collation, &mut folded)),
            Datum::Blob(bytes) => Datum::Blob(bytes.get(..PREFIX_BYTES).unwrap_or(bytes)),
            other => *other,
        };
        // BINARY, because `clip` has already applied the collation. Asking the
        // encoder to apply it a second time is what would break the prefix.
        let start = out.len();
        key::encode_into_with(&clipped, Collation::Binary, out);
        // The same inversion the tree's key encoding makes, for the same
        // reason: this prefix is what the entries are sorted by, and it has to
        // sort them into the order the tree will read them in.
        if directions.get(index).copied().unwrap_or(false) {
            if let Some(span) = out.get_mut(start..) {
                for byte in span {
                    *byte = !*byte;
                }
            }
        }
    }
}

/// Returns a payload with its collation applied and clipped to the prefix.
///
/// `folded` is a reusable buffer for the collations that transform the bytes;
/// BINARY needs none and borrows the payload directly, which is the case every
/// index in the gate takes.
///
/// @param bytes - the payload
/// @param collation - the order it is compared under
/// @param folded - a buffer for a transformed copy
fn clip<'a>(bytes: &'a [u8], collation: Collation, folded: &'a mut Vec<u8>) -> &'a [u8] {
    match collation {
        // An application-defined collation is treated as BINARY here for the
        // same reason `collation_of` does: the engine cannot order a tree by a
        // callback into the connection that registered it, and the key encoder
        // makes the same choice - so the prefix stays a prefix of what the
        // encoder produces.
        // `decimal` and `uint` join `Custom` here for exactly the same reason:
        // neither has a byte transformation whose order is the collation order,
        // so neither can key a tree - see
        // `Collation::is_order_preserving_in_keys`.
        Collation::Binary | Collation::Custom(_) | Collation::Decimal | Collation::Uint => {
            bytes.get(..PREFIX_BYTES).unwrap_or(bytes)
        }
        Collation::NoCase => {
            // Lowercasing commutes with clipping, so only the prefix is folded.
            // A NUL inside the prefix is the exception: the key encoder writes
            // the value's whole length after it (task-2079), and a clip cannot
            // know that length, so the whole value is transformed and the
            // result clipped instead.
            folded.clear();
            let head = bytes.get(..PREFIX_BYTES).unwrap_or(bytes);
            if head.contains(&0) {
                nocase_key_bytes(bytes, folded);
                folded.truncate(PREFIX_BYTES);
            } else {
                folded.extend_from_slice(head);
                folded.make_ascii_lowercase();
            }
            folded.as_slice()
        }
        Collation::RTrim => {
            // Trimmed first and clipped after: the other order would strip
            // spaces the whole value does not treat as trailing.
            let mut end = bytes.len();
            while end > 0 && bytes.get(end.saturating_sub(1)) == Some(&b' ') {
                end = end.saturating_sub(1);
            }
            let trimmed = bytes.get(..end).unwrap_or(&[]);
            trimmed.get(..PREFIX_BYTES).unwrap_or(trimmed)
        }
    }
}

/// Sorts `(word, entry)` pairs by word, least significant byte first.
///
/// **Least significant byte first, which is what makes it a sort.** An LSD radix
/// is correct because each pass is stable and the passes run upward from the
/// least significant byte, so a later pass never disturbs the order an earlier
/// one established within a byte it agrees on. Running them the other way round
/// is not a slower sort, it is not a sort - the unit test comparing this order
/// against a reference sort caught exactly that.
///
/// The bytes every word agrees on are found once, up front, and skipped: on the
/// medium fixture's keys the leading class byte and `row ` are constant, so a
/// level does three or four passes rather than eight. The two buffers are
/// ping-ponged, so a pass is one sequential read and one scattered write per
/// entry and never a copy back.
///
/// @param pairs - the pairs, sorted in place
/// @param scratch - a buffer the passes ping-pong into, any contents
fn radix_by_word(pairs: &mut Vec<(u64, u32)>, scratch: &mut Vec<(u64, u32)>) {
    let width = pairs.len();
    if width < 2 {
        return;
    }
    // A byte position matters only where the words disagree, and one pass over
    // them answers that for all eight at once.
    let first = pairs.first().map(|(word, _)| *word).unwrap_or(0);
    let mut differs = 0u64;
    for (word, _) in pairs.iter() {
        differs |= word ^ first;
    }
    scratch.clear();
    scratch.resize(width, (0, 0));
    let mut flipped = false;
    for pass in 0..8usize {
        let shift = pass.saturating_mul(8);
        if (differs >> shift) & 0xFF == 0 {
            continue;
        }
        let (source, target): (&[(u64, u32)], &mut [(u64, u32)]) = if flipped {
            (scratch.as_slice(), pairs.as_mut_slice())
        } else {
            (pairs.as_slice(), scratch.as_mut_slice())
        };
        let mut counts = [0usize; 256];
        for (word, _) in source.iter() {
            let byte = ((*word >> shift) & 0xFF) as usize;
            if let Some(slot) = counts.get_mut(byte) {
                *slot = slot.saturating_add(1);
            }
        }
        let mut running = 0usize;
        for count in counts.iter_mut() {
            let held = *count;
            *count = running;
            running = running.saturating_add(held);
        }
        for pair in source.iter() {
            let byte = ((pair.0 >> shift) & 0xFF) as usize;
            let Some(slot) = counts.get_mut(byte) else {
                continue;
            };
            if let Some(cell) = target.get_mut(*slot) {
                *cell = *pair;
            }
            *slot = slot.saturating_add(1);
        }
        flipped = !flipped;
    }
    // An odd number of passes leaves the answer in the scratch buffer, and the
    // caller reads `pairs`.
    if flipped {
        std::mem::swap(pairs, scratch);
    }
}

/// One [`EntrySet`] read in the order a sort produced, for the leaf builder.
///
/// It owns nothing and copies nothing: a value is a lookup into the arena
/// through the order vector, which is what lets a `CREATE INDEX` pack a tree
/// without ever holding a second copy of its rows.
pub(crate) struct Ordered<'a> {
    /// The arena the values live in.
    set: &'a EntrySet,
    /// The entries, in key order.
    order: &'a [u32],
}

impl<'a> inillucent_tree::leaf::Rows<'a> for Ordered<'a> {
    fn len(&self) -> usize {
        self.order.len()
    }

    fn value(&self, row: usize, column: usize) -> Datum<'a> {
        let Some(entry) = self.order.get(row).copied() else {
            return Datum::Null;
        };
        self.set.datum(entry as usize, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a set from tuples of `(text, rowid)`, which is an index entry.
    fn set_of(rows: &[(&str, i64)]) -> EntrySet {
        set_under(rows, Collation::Binary)
    }

    /// The same, under a named collation.
    fn set_under(rows: &[(&str, i64)], collation: Collation) -> EntrySet {
        let collations = [collation, Collation::Binary];
        let mut set =
            EntrySet::with_capacity(2, rows.len(), KeyEncoding::General, &collations, &[]);
        for (text, rowid) in rows {
            set.push(&[Datum::Text(text.as_bytes()), Datum::Int(*rowid)]);
        }
        set
    }

    /// The order the radix produces is the order the tree's comparison gives.
    ///
    /// The whole point of the prefix is that it accelerates an order it does not
    /// define, so the property to check is that the two agree - over enough
    /// entries that the prefix ties on a good number of them.
    #[test]
    fn radix_order_matches_the_trees_comparison() {
        let rows: Vec<(String, i64)> = (0..3000)
            .map(|number| {
                (
                    format!("row {} lorem ipsum dolor sit amet", (number * 7919) % 3000),
                    number as i64,
                )
            })
            .collect();
        let borrowed: Vec<(&str, i64)> = rows
            .iter()
            .map(|(text, rowid)| (text.as_str(), *rowid))
            .collect();
        let set = set_of(&borrowed);
        let mut expected: Vec<u32> = (0..set.len() as u32).collect();
        expected.sort_by(|left, right| set.compare(*left, *right));
        assert_eq!(set.order(), expected);
    }

    /// Entries whose whole prefix ties still come out in key order.
    ///
    /// Every one of these agrees for more than sixteen bytes, so the prefix
    /// separates none of them and the fallback decides all three.
    #[test]
    fn a_shared_prefix_is_resolved_by_the_whole_value() {
        let set = set_of(&[
            ("aaaaaaaaaaaaaaaaaaaa-z", 1),
            ("aaaaaaaaaaaaaaaaaaaa-a", 2),
            ("aaaaaaaaaaaaaaaaaaaa-m", 3),
        ]);
        assert_eq!(set.order(), vec![1, 2, 0]);
    }

    /// The prefix is applied under the column's collation, not around it.
    ///
    /// Under NOCASE these three are one value, so their order is by rowid; a
    /// prefix taken from the unfolded bytes would put `ZED` before `abc`.
    #[test]
    fn nocase_orders_by_the_folded_prefix() {
        let set = set_under(&[("ZED", 3), ("zed", 1), ("Zed", 2)], Collation::NoCase);
        let order = set.order();
        let rowids: Vec<i64> = order
            .iter()
            .map(|entry| match set.datum(*entry as usize, 1) {
                Datum::Int(number) => number,
                _ => 0,
            })
            .collect();
        assert_eq!(rowids, vec![1, 2, 3]);
    }

    /// A NOCASE prefix follows SQLite's rule for a NUL, so the radix and the
    /// tree's comparison agree about values that hold one (task-2079).
    ///
    /// Under that rule the bytes after a value's first NUL do not count and its
    /// length does. A prefix folded byte by byte, as `clip` did before, would
    /// put `"\0\0y"` before `"\0a"` while the comparison puts it after.
    #[test]
    fn a_nocase_prefix_stops_at_a_nul_as_the_comparison_does() {
        let rows: Vec<(&str, i64)> = vec![
            ("\0\0y", 1),
            ("\0a", 2),
            ("\0B", 3),
            ("A\0zzzzzzzzzzzzzzzzzzzz", 4),
            ("a\0b", 5),
            ("a", 6),
            ("ab", 7),
            ("abcdefghijklmnopq\0x", 8),
            ("ABCDEFGHIJKLMNOPQ\0", 9),
            ("\0", 10),
            ("", 11),
        ];
        let set = set_under(&rows, Collation::NoCase);
        let mut expected: Vec<u32> = (0..set.len() as u32).collect();
        expected.sort_by(|left, right| set.compare(*left, *right));
        assert_eq!(set.order(), expected);
        let rowids: Vec<i64> = set
            .order()
            .iter()
            .map(|entry| match set.datum(*entry as usize, 1) {
                Datum::Int(number) => number,
                _ => 0,
            })
            .collect();
        // `"\0a"` and `"\0B"` are one value, so the rowid orders them, and both
        // come before `"\0\0y"` because they are shorter.
        assert_eq!(rowids, vec![11, 10, 2, 3, 1, 6, 5, 4, 7, 9, 8]);
    }

    /// RTRIM trims the whole value before the prefix is clipped.
    ///
    /// The clip lands inside the run of spaces, so trimming afterwards would
    /// strip spaces this value treats as interior and produce a prefix of
    /// nothing. `a` and `a` + spaces are one value under RTRIM and must not be
    /// separated by the prefix.
    #[test]
    fn rtrim_trims_before_the_prefix_is_clipped() {
        let set = set_under(
            &[("a                    x", 2), ("a", 1), ("a       ", 1)],
            Collation::RTrim,
        );
        let mut expected: Vec<u32> = (0..set.len() as u32).collect();
        expected.sort_by(|left, right| set.compare(*left, *right));
        assert_eq!(set.order(), expected);
    }

    /// The values come back out of the arena unchanged, in the sorted order.
    #[test]
    fn the_arena_round_trips_its_values() {
        let set = set_of(&[("b", 2), ("a", 1)]);
        let flat = set.to_datums(&set.order());
        // `Datum` carries an `f64` and so has no `PartialEq`; the shape a test
        // wants to state is what the values are, which is what this says.
        let described: Vec<String> = flat.iter().map(|value| format!("{value:?}")).collect();
        assert_eq!(
            described,
            vec![
                format!("{:?}", Datum::Text(b"a")),
                format!("{:?}", Datum::Int(1)),
                format!("{:?}", Datum::Text(b"b")),
                format!("{:?}", Datum::Int(2)),
            ]
        );
    }

    /// A NULL is distinct from every other NULL, so it is never a duplicate.
    #[test]
    fn nulls_do_not_share_a_key_prefix() {
        let collations = [Collation::Binary; 2];
        let mut set = EntrySet::with_capacity(2, 2, KeyEncoding::General, &collations, &[]);
        set.push(&[Datum::Null, Datum::Int(1)]);
        set.push(&[Datum::Null, Datum::Int(2)]);
        let order = set.order();
        assert!(!set.shares_key_prefix(&order, 0, 1));
    }

    /// Two entries with the same indexed column do share one.
    #[test]
    fn equal_indexed_columns_share_a_key_prefix() {
        let set = set_of(&[("same", 1), ("same", 2)]);
        let order = set.order();
        assert!(set.shares_key_prefix(&order, 0, 1));
    }

    /// An empty set sorts to nothing rather than panicking.
    #[test]
    fn an_empty_set_has_an_empty_order() {
        let set = set_of(&[]);
        assert!(set.order().is_empty());
        assert!(set.to_datums(&[]).is_empty());
    }

    /// A rowid tree's key is eight bytes, and the prefix is the whole of it.
    #[test]
    fn a_rowid_key_is_its_own_prefix() {
        let collations = [Collation::Binary];
        let mut set = EntrySet::with_capacity(1, 3, KeyEncoding::Rowid, &collations, &[]);
        for rowid in [7i64, -1, 3] {
            set.push(&[Datum::Int(rowid)]);
        }
        let order = set.order();
        let placed: Vec<i64> = order
            .iter()
            .map(|entry| match set.datum(*entry as usize, 0) {
                Datum::Int(number) => number,
                _ => 0,
            })
            .collect();
        assert_eq!(placed, vec![-1, 3, 7]);
    }
}
