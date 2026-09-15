# task-1846 — the schema floor, and the three `CREATE INDEX` forms

**Status: built and measured.** This began as a design and is kept as the record of what was
designed, what the measurements said, and where the design was wrong.

Four pieces:

- **A** — `schema` was the last family under the 1.00x floor. `CREATE INDEX` at medium cost 49.4 ms
  against SQLite's ~31; it is now **26.4–27.1 ms**, and the family reads **1.19x–1.22x** across four
  consecutive 30-round runs where it read 0.66x.
- **B** — partial indexes, `CREATE INDEX ix ON t(a) WHERE b > 5`.
- **C** — indexes on expressions, `CREATE INDEX ix ON t(lower(a))`.
- **D** — `CREATE INDEX` on a `WITHOUT ROWID` table.

B, C and D were the three `Differs` rows in `crates/inillucent-compat/tests/semantics.rs`. That file
is now **106 of 106** with no `Differs` row left: its 92, task-1849's 7, and 7 added here.

**H5 is still not met, and not because of any of the four.** The floor clause is missed on two of the
four runs by `extension`, whose low bound straddled 1.00x before this ticket existed — see the
acceptance section.

---

## Part A — the bulk builder

### What the measurement said, and where it disagreed with the ticket

The ticket's stage table came from `indexprofile`, which printed **the median of the totals beside
the stages of the last round** — different rounds of a statement that varies by several milliseconds.
The two never added up, and the residual read as a 7.4 ms prologue that does not exist. `indexprofile`
now takes a median per stage and reports the residual as its own column; the prologue is **0.7 ms**.

The second and larger disagreement was between `indexprofile` and the gate itself. `indexprofile`
builds over a freshly imported table; **the gate builds after its write workloads** — 2,000 inserts,
2,000 scattered updates, 2,000 deletes, 500 upserts — so by then almost every leaf of `main_table`
carries a delta entry or a tombstone. The same statement measured **26.3 ms** on one and **38.9 ms**
on the other, and the difference was entirely in a stage `indexprofile` could not see. `fullgate` now
prints the stage breakdown for its own `schema.index` round.

| stage | at `ea608dd` | now (in the gate) |
|---|---:|---:|
| scan | 13.3 | **3.8** |
| sort | 5.3 | 4.9 |
| flatten (arena → the builder's rows) | — | 1.5 |
| pack | 15.1 | **6.5** |
| catalog (`record`, `rebuild_tables`, `refresh_catalog`) | 7.0 | **0.1** |
| seal (the log commit and its sync) | — | 6.7 |
| prologue (parse, bind, allocate a root, re-parse) | — | 0.5 |
| **total** | **49.4 ms** | **28.0 ms** |

Splitting `catalog` from `seal` settles the ticket's own claim: the catalog work really is under a
tenth of a millisecond, and the tail is the sync both engines pay under `synchronous = FULL`. There
is nothing to close there.

### A1. One arena instead of three hundred thousand allocations

`index_entries` built a `Vec<OwnedDatum>` per row — one allocation for the vector, one for
`OwnedDatum::Text`'s copy of the label — and `build_tree_from` then materialised a *second*
`Vec<Vec<Datum>>` of the whole input for the packer. Three allocations per row, three hundred
thousand of them to build a tree of two hundred leaves.

`EntrySet` holds the same information in three vectors, each reserved once from the row count:
`cells` (`rows * width` fixed-size cells), `bytes` (every payload appended once), and `prefix` (a
fixed-width sort key). Nothing is allocated per row.

### A2. A sort prefix, not a whole key

The order the sort produces must be the order **the tree** compares in, or a descent binary-searches
separators the leaves do not obey — a scan answers right and a seek answers wrong, which is the defect
`in_key_order`'s own comment was written about. The obvious way to get that is to encode each entry's
whole key with the tree's `KeyEncoding` and sort the bytes.

It was measured and it is too expensive: encoding a hundred thousand fifty-five-byte keys cost
**5.5 ms of a 9.3 ms scan**, measured by building once with the encoding removed, to produce five and
a half megabytes the sort then reads back in a random order.

So each entry keeps the **first sixteen bytes** of that encoding, produced by the same encoder on
payloads clipped to sixteen — the escape only ever lengthens a payload, so the clipped form still
fills the prefix, which makes it an exact prefix rather than a second encoding to keep in step. Two
entries the prefix ties are ordered by `compare_under`, the comparison the tree's own search and its
integrity checker use. The prefix is a *speed* choice with a correct fallback behind it, never a claim
about the order.

The collation is applied **before** the clip and the value is then encoded as BINARY. The other order
is wrong: `RTRIM` over a clip that lands inside a run of spaces strips spaces the whole value treats
as interior, and the result is a prefix of nothing. There are tests for that shape and for `NOCASE`.

`order()` is an LSD radix over the prefix, one word at a time, with the bytes every word agrees on
found once and skipped, ping-ponging between two buffers. A run the words cannot separate falls back
to `compare_under`.

**A defect the guard caught.** The first radix ran its counting passes most-significant-byte first.
An LSD radix is only a sort if the passes run *upward* from the least significant byte; run the other
way it produces a plausible-looking permutation that is not sorted. The unit test that checks the
radix order against the tree's own comparison over thousands of entries failed on it immediately.

### A3. The packer stops copying

`LeafBuilder::pack_with`, `encode_with`, `PagedTree::bulk_build_logged` and
`ImportedDatabase::build_tree_from` are generic over `AsRef<[Datum<'d>]>`. `Vec<Datum>` already
implements it, so **every existing call site compiles unchanged** — including the compaction path in
`write.rs` that made task-1845 back this out — and the index build passes `&[&[Datum]]` whose slices
point into the arena.

### A4. The scan reads the leaf once, not once per value

`LeafRef::value` re-reads the directory entry and re-derives the class array and slot bounds on every
call — the same waste `key_view` exists to remove inside a search — and an index build calls it twice
for every row in the table. The mini-columns are now derived once per leaf.

### A5. `visit_live` — the stage the ticket did not name

A leaf that has been written to cannot take the vectorised path: tombstones and the delta area have
to be merged. `LeafRef::live` performs that merge and hands back **every column of every live row in
a fresh `Vec`**; an index reads two of six. On a freshly imported table that never arises, and in the
gate it was **14.3 ms of a 38.9 ms statement**.

`LeafRef::visit_live` performs the same merge — the sorted region minus its tombstones, the delta
area's rows replacing the ones they shadow, the newest of two delta entries for one key winning —
projecting only the columns asked for and materialising nothing per row. It does not sort, because
the index build sorts the whole table's entries once.

Two implementations of one merge is exactly the shape that drifts, so `semantics.rs` gained
`index.after.writes` and `index.after.writes.without.rowid`: scripts that insert, update and delete
before the `CREATE INDEX` and compare every byte against the reference.

### A6. Also on the way

`escape_into` copies runs between `0x00` bytes instead of pushing per byte, and `NOCASE` folds into
the buffer instead of collecting a `Vec` per value.

---

## Part B — partial indexes

1. **Accepted.** The binder's refusal is gone. The predicate needs no new directive field: the engine
   re-parses the canonical SQL it stores, and `index_from_create_sql` already put it on
   `IndexInfo::partial_sql`.
2. **Maintained.** `BoundIndexExprs` rides the bound statement the way `BoundCheck` already does;
   `WriteDeclarations` compiles it and `IndexExprs::holds` answers, per row image, whether the row
   belongs in the index. An `UPDATE` asks about **both** images, so a row that moves across the
   predicate joins the index or leaves it. The list is empty for every table with neither a partial
   index nor an expression key — which is every table the gate measures.
3. **Used.** Only when the predicate appears **unchanged as a conjunct** of the statement's `WHERE`.
   That is SQLite's rule and deliberately the crudest sound one: `WHERE b > 5 AND a = 1` uses an index
   declared `WHERE b > 5`, and `WHERE b > 6` does not, though it implies it. A cleverer implication
   test is a place for a wrong answer to live.

**The build path splits.** An ordinary index is filled by the arena scan the gate measures; a partial
or expression index is filled by a `SELECT` the binder, planner and executor already know how to run,
rather than growing a second expression evaluator inside the DDL path. A predicate naming a column the
table has not got is refused there, by the binder.

### The silent wrong answer this found

`inillucent-exec`'s physical pass has a covering rule of its own, applied *after* the planner has
spoken: a plain table scan is replaced by a scan of the smallest tree carrying every column the query
reads. Its test is to build the pipeline against the candidate's layout and see whether it translates
— a question about **columns**, with no way to notice that a tree holds fewer **rows** than the table.
`create_index` was adding the partial index to that set like any other, and

```sql
SELECT rowid FROM u;              -- returned one of two rows
SELECT rowid FROM u WHERE b = 1;  -- returned the other
EXPLAIN QUERY PLAN SELECT rowid FROM u;  -- SCAN u
```

with `DELETE` and `UPDATE` silently affecting nothing, because they find their rows the same way.
Fixed in all four places the covering set is built — `create_index`, the fixture import, the catalog
re-attach and the `DROP`-inside-a-transaction rollback — behind one named predicate,
`covers_every_row`, so the four cannot drift. An index on an *expression* stays a candidate: it holds
an entry for every row, and the columns it does not carry are unmapped in its layout, so the existing
translation test already refuses it where it should.

---

## Part C — indexes on expressions

`IndexKeyColumn::column` became `Option<u16>` beside a new `expr_sql`, so a reader that needs a column
— a module-backed index — has to say what it does when there is not one. Maintenance is the same
`BoundIndexExprs`. The planner matches the *bound* key expression against a term's own side, and
`AccessPath::IndexSeek::columns` became `Vec<Option<u16>>` so a computed key can say it has no column
behind it: the probe then takes **no affinity**, which is SQLite's rule and would otherwise compare a
converted value against an unconverted one.

The binder binds those expressions onto the FROM term in a scope holding **only that term** — a
predicate reading `b` must mean this table's `b` even when another term in the query has one. An index
whose text will not bind is left out, which leaves it unchosen rather than chosen on a guess.

---

## Part D — `CREATE INDEX` on a `WITHOUT ROWID` table

`index_shape` appends the table's primary key, in its key order, where a rowid would go.
**`SourceLayout` gained `identity`** — the tree columns that identify the *table* row — and the
non-covering lookup in `physical.rs` probes with it. `key_columns` could not answer that question: on
an index layout it names the whole entry rather than this part of it, and it is deliberately emptied
when the tree is not "already sorted", which would have left a `DESC` or collated primary key with no
identity at all.

`PRAGMA index_list` also reports `partial`, which was a hard zero — true while a partial index could
not exist, and a wrong answer now that one can.

---

## What the acceptance says, and the half of it that is not this ticket's

**Four consecutive 30-round `inillucent-fullgate` medium runs, on the merged tree:**

| run | `schema` | `extension` | weighted low | floor |
|---|---|---|---|---|
| 1 | 1.22x [1.17, 1.45] | 1.14x [**0.99**, 1.33] | 3.80x | extension UNDER |
| 2 | 1.19x [1.16, 1.25] | 1.15x [1.00, 1.34] | 3.85x | all above |
| 3 | 1.21x [1.13, 1.22] | 1.16x [**0.99**, 1.34] | 3.80x | extension UNDER |
| 4 | 1.19x [1.15, 1.67] | 1.16x [1.06, 1.44] | 3.75x | all above |

1. **`schema` is met on all four**, 1.19x-1.22x with lows 1.13-1.17, from 0.66x. That is what this
   ticket owned. The weighted lower bound clears 3.00x on all four.
2. **The floor clause is met on two of four, and the family that misses is `extension`.** The ticket
   asserted that "A is the only thing standing in the way"; measurement says otherwise, and the proof
   that it is not this ticket's doing is in the baseline that assertion was written against:
   task-1845's four runs had `extension` lows of 1.02, 1.01, **0.99**, 1.03, and its run 3 prints the
   identical `extension 0.99x UNDER THE FLOOR` line. The family straddled 1.00x before task-1846
   existed. Its ratio is marginally *better* now - 1.14-1.16x against 1.11-1.17x - and the
   differences are hundredths, inside the run-to-run spread.

   The workload dragging it is `extension.fts.build` at 0.30x, which Phase 3's Part E already names
   as outstanding. It is now **task-1852**, because a bound that one run lands above at 1.06 and
   another below at 0.99 from identical code cannot be closed by re-running the gate.

   **So H5 is not met, and this ticket is not the reason.**
3. The read gate was run either side of Part D's change to the non-covering lookup: `read.point`
   31.00x to 30.72x, `read.range` 5.03x to 4.95x, `read.join` 4.86x to 4.57x, `read.analytical`
   6.30x to 6.20x, PointProbe 312.9 ns to 293.0 ns. Every workload inside the other run's interval.
4. **`semantics.rs` at 106 of 106**, with no `Differs` row: the 92 it had, task-1849's 7
   `UNIQUE`-under-`UPDATE` shapes, 2 over a table that has been written to, and 5 where the two
   tickets meet. The `agreed >= CASES.len() - 3` slack is now `assert_eq!(agreed, CASES.len())`.

   Those last five were measured **before** the merge so their predictions could go red: `into` and
   `two.indexes` had to flip from DIFFERS, and `outof`, `staying` and `maintenance.crossing` had to
   *hold* - they agreed beforehand only because nothing was checked at all, and had to go on agreeing
   for the opposite reason once the check was reached. Two flipped, three held, exactly as
   predicted.
5. **`cargo test --workspace`**: four failing binaries of 196 - `harness` (1), `ordering` (1),
   `planner` (2), `schema_forms` (14) - every one of those 18 tests in the set task-1845 recorded at
   `ea608dd`. `policy` is green where the baseline had its format check red.

## Non-goals, unchanged

No bar, weight or fixture in `compat/perf/contract.toml` was touched. The old engine is not deleted.
Multi-process and multi-thread access stay out of scope.

## Found and not fixed here

- An `UPDATE` that violates a **secondary `UNIQUE` index** without moving the table's own key was
  accepted silently. It reproduced on the `ea608dd` binary, so it predated this ticket and was not
  one of A-D. Filed as **task-1849**, fixed there, and merged back into this branch - the interaction
  between its fix and this ticket's partial indexes is the five cases above, which neither ticket
  could have tested alone: before 1846 the index cannot be created, and before 1849 the check is
  never reached.
- **`extension.fts.build` at 0.30x** is the last family under the floor, and the only thing left
  between the engine and H5. Filed as **task-1852**.
