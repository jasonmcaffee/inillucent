//! The hash join, and the table it probes.
//!
//! Invariant: **the key is compared as bytes.** Both sides are encoded
//! through the memcmp key codec before anything is hashed, so two values
//! that are equal under the dialect's rules hash alike and two that are
//! not cannot collide into one group.

use std::collections::HashMap;

use inillucent_base::DbResult;
use inillucent_tree::datum::{Datum, OwnedDatum};
use inillucent_tree::key;

use crate::batch::{Batch, Vector};
use crate::expr::Eval;
use crate::ops::{emit_rows, Flow, Sink};

use super::*;

/// A hash table over encoded join keys.
///
/// The key is the memcmp encoding rather than the values, for the same reason
/// an interior page's separator is: one `Vec<u8>` hashes and compares in one
/// call where a tuple of tagged values dispatches per column. The encoding is
/// exact - `inillucent-tree`'s key module documents how - so two rows hash together
/// exactly when SQL says their keys are equal.
struct HashTable {
    /// Encoded key to the positions in `rows` that carry it.
    index: HashMap<Vec<u8>, Vec<u32>>,
    /// The build side's rows.
    rows: Vec<Vec<OwnedDatum>>,
    /// Which build rows have been matched, for a right/full outer join.
    matched: Vec<bool>,
}
impl HashTable {
    /// Returns an empty table.
    fn new() -> HashTable {
        HashTable {
            index: HashMap::new(),
            rows: Vec::new(),
            matched: Vec::new(),
        }
    }

    /// Forgets every build row, so the join can be run again.
    fn clear(&mut self) {
        self.index.clear();
        self.rows.clear();
        self.matched.clear();
    }

    /// Adds one build row under an encoded key.
    ///
    /// A NULL in the key is dropped rather than indexed: SQL equality is never
    /// true for NULL, so a NULL-keyed build row can never match a probe, and
    /// keeping it in the table would only make it findable.
    ///
    /// @param key - the encoded join key
    /// @param row - the build row
    /// @param has_null - whether any key column was NULL
    fn insert(&mut self, key: Vec<u8>, row: Vec<OwnedDatum>, has_null: bool) {
        let position = self.rows.len() as u32;
        self.rows.push(row);
        self.matched.push(false);
        if has_null {
            return;
        }
        self.index.entry(key).or_default().push(position);
    }

    /// Returns the build rows carrying an encoded key.
    ///
    /// @param key - the encoded probe key
    fn probe(&self, key: &[u8]) -> &[u32] {
        self.index.get(key).map(Vec::as_slice).unwrap_or(&[])
    }
}
/// How a join treats rows that find no partner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinKind {
    /// Only matching pairs.
    Inner,
    /// Every probe row, null-extended when it matches nothing.
    Left,
    /// Every *build* row, null-extended when it matches nothing.
    ///
    /// Only [`NestedLoopJoin`] answers this one, and the reason is the shape of
    /// that operator rather than a gap: it holds the build side as a
    /// materialised vector, so it can remember which of those rows matched and
    /// emit the rest when the input ends. An operator that sees the build side
    /// one tree descent at a time has nothing to keep that mark on.
    Right,
    /// Every probe row and every build row, each null-extended when it matched
    /// nothing.
    Full,
    /// One probe row per probe row that matched, with no build columns.
    Semi,
    /// One probe row per probe row that matched nothing, with no build columns.
    Anti,
}
impl JoinKind {
    /// Reports whether unmatched *probe* rows are kept, null-extended.
    pub fn keeps_probe(self) -> bool {
        matches!(self, JoinKind::Left | JoinKind::Full)
    }

    /// Reports whether unmatched *build* rows are kept, null-extended.
    pub fn keeps_build(self) -> bool {
        matches!(self, JoinKind::Right | JoinKind::Full)
    }
}
/// Builds a hash table from one side, then probes it with the other.
///
/// The build side is pushed in first through [`HashJoin::build`]; after that the
/// operator is an ordinary sink and every batch pushed into it is a probe.
pub struct HashJoin<'s> {
    kind: JoinKind,
    /// How wide the probe side is, learned from the first batch.
    ///
    /// Read at `finish`, where a RIGHT or FULL join null-extends the build rows
    /// nothing matched and there is no batch left to ask.
    probe_width: usize,
    /// The build side's key expressions, over the build row's columns.
    build_keys: Vec<Box<dyn Eval>>,
    /// The probe side's key expressions, over the probe batch's columns.
    probe_keys: Vec<Box<dyn Eval>>,
    table: HashTable,
    downstream: Box<dyn Sink + 's>,
    /// Reused between probes so a join of a million rows makes no allocation
    /// per row.
    scratch: Vec<u8>,
}
impl<'s> HashJoin<'s> {
    /// Returns a hash join with an empty table.
    ///
    /// @param kind - how unmatched rows are treated
    /// @param build_keys - the build side's key expressions
    /// @param probe_keys - the probe side's key expressions
    /// @param downstream - what to push joined rows into
    pub fn new(
        kind: JoinKind,
        build_keys: Vec<Box<dyn Eval>>,
        probe_keys: Vec<Box<dyn Eval>>,
        downstream: Box<dyn Sink + 's>,
    ) -> HashJoin<'s> {
        HashJoin {
            kind,
            probe_width: 0,
            build_keys,
            probe_keys,
            table: HashTable::new(),
            downstream,
            scratch: Vec::new(),
        }
    }

    /// Adds one batch of build rows to the table.
    ///
    /// @param batch - a batch from the build side
    pub fn build(&mut self, batch: &Batch<'_>) -> DbResult<()> {
        // **The build side is charged here (task-1932, H6).** It is the whole
        // of one input held in memory before a single output row exists, and
        // before this the request budget saw none of it: a join whose build
        // side is the large table and whose answer is one row spent one row's
        // worth of a 256 MiB cap.
        inillucent_base::budget::materialise(crate::ops::batch_bytes(batch))?;
        let width = batch.columns.len();
        for nth in 0..batch.live() {
            let mut row = Vec::with_capacity(width);
            for column in 0..width {
                row.push(OwnedDatum::from_datum(&batch.value(nth, column)?));
            }
            let (encoded, has_null) = encode_row_key(&self.build_keys, batch, nth)?;
            self.table.insert(encoded, row, has_null);
        }
        Ok(())
    }

    /// Returns how many rows the build side put in the table.
    pub fn build_rows(&self) -> usize {
        self.table.rows.len()
    }

    /// Adds rows that were materialised rather than streamed.
    ///
    /// The automatic index's entry point: the inner side of a join has already
    /// been read into a vector by the time the operator is built, and turning
    /// it back into batches only so that `build` can take them apart again is
    /// work with no reader. The rows are fed a batch at a time even so, because
    /// the key expressions are compiled against a batch and evaluating them one
    /// row at a time would mean a second evaluator.
    ///
    /// @param rows - the inner side's rows, in the order they were read
    pub fn build_materialised(&mut self, rows: &[Vec<OwnedDatum>]) -> DbResult<()> {
        let width = rows.first().map(Vec::len).unwrap_or(0);
        if width == 0 {
            return Ok(());
        }
        let mut start = 0usize;
        while start < rows.len() {
            let end = start
                .saturating_add(crate::batch::BATCH_ROWS)
                .min(rows.len());
            let chunk = rows.get(start..end).unwrap_or(&[]);
            let mut held: Vec<Vec<Datum<'_>>> = Vec::with_capacity(width);
            for column in 0..width {
                held.push(
                    chunk
                        .iter()
                        .map(|row| {
                            row.get(column)
                                .map(OwnedDatum::borrow)
                                .unwrap_or(Datum::Null)
                        })
                        .collect(),
                );
            }
            let columns: Vec<Vector<'_>> = held
                .iter()
                .map(|values| Vector::Values(values.as_slice()))
                .collect();
            self.build(&Batch::new(chunk.len(), columns))?;
            start = end;
        }
        Ok(())
    }
}
impl Sink for HashJoin<'_> {
    fn push(&mut self, batch: &Batch<'_>) -> DbResult<Flow> {
        // **Every batch, because the scan leaves are not enough (task-1932,
        // H11).** A join reads its probe side once and then does work
        // proportional to the matches, so a cross join over a small table
        // checks a few hundred times at the start and then runs for as long as
        // the product takes with nothing reading the cancellation flag. One
        // atomic load per batch is what makes a cancel reach the statement it
        // is about rather than the one after it.
        inillucent_base::budget::check()?;
        let HashJoin {
            kind,
            probe_width,
            probe_keys,
            table,
            downstream,
            scratch,
            ..
        } = self;
        let width = batch.columns.len();
        *probe_width = width;
        let build_width = table.rows.first().map(Vec::len).unwrap_or(0);
        let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
        for nth in 0..batch.live() {
            scratch.clear();
            let mut has_null = false;
            for expression in probe_keys.iter() {
                let value = expression.value(batch, nth)?;
                has_null |= value.is_null();
                key::encode_into(&value.get(), scratch);
            }
            // **Copied once per probe row, including when it matched
            // nothing (task-1932, M7).** `probe` answers a borrowed slice and
            // this cloned it so the loop below could hold it across the
            // `&mut self` a push needs. Reading the length first means a row
            // that matches nothing - the common case on a selective join -
            // allocates nothing at all.
            let found = if has_null {
                &[][..]
            } else {
                table.probe(scratch)
            };
            let matches: Vec<u32> = match found.is_empty() {
                true => Vec::new(),
                false => found.to_vec(),
            };
            match kind {
                JoinKind::Semi => {
                    if !matches.is_empty() {
                        produced.push(materialise(batch, nth, width)?);
                    }
                }
                JoinKind::Anti => {
                    if matches.is_empty() {
                        produced.push(materialise(batch, nth, width)?);
                    }
                }
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full => {
                    if matches.is_empty() {
                        if kind.keeps_probe() {
                            let mut row = materialise(batch, nth, width)?;
                            row.extend(std::iter::repeat_n(OwnedDatum::Null, build_width));
                            produced.push(row);
                        }
                    } else {
                        for position in matches {
                            if let Some(slot) = table.matched.get_mut(position as usize) {
                                *slot = true;
                            }
                            let mut row = materialise(batch, nth, width)?;
                            if let Some(build) = table.rows.get(position as usize) {
                                row.extend(build.iter().cloned());
                            }
                            produced.push(row);
                        }
                    }
                }
            }
        }
        if produced.is_empty() {
            return Ok(Flow::Continue);
        }
        emit_rows(&produced, downstream.as_mut())
    }

    fn finish(&mut self) -> DbResult<()> {
        // The build rows nothing matched, which only a RIGHT or FULL join
        // keeps and which only the end of the probe side can identify.
        if self.kind.keeps_build() {
            let mut produced: Vec<Vec<OwnedDatum>> = Vec::new();
            for (position, build) in self.table.rows.iter().enumerate() {
                if self.table.matched.get(position).copied().unwrap_or(false) {
                    continue;
                }
                let mut row: Vec<OwnedDatum> =
                    std::iter::repeat_n(OwnedDatum::Null, self.probe_width).collect();
                row.extend(build.iter().cloned());
                produced.push(row);
            }
            if !produced.is_empty() {
                emit_rows(&produced, self.downstream.as_mut())?;
            }
        }
        self.downstream.finish()
    }

    /// Returns this operator and everything below it to its pre-input state.
    fn reset(&mut self) -> DbResult<()> {
        self.table.clear();
        self.scratch.clear();
        self.probe_width = 0;
        self.downstream.reset()
    }
}
