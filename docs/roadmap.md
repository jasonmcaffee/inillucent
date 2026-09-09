# What is not there yet

In the order it is being worked, each with the measurement behind it.

## 1. Memory is the one headline SQLite still wins

**42.61 MiB against SQLite's 37.19 — 15% more**, on the same 128 MiB budget, while taking 279% less
wall clock and 65% less processor. The performance contract asks for 5% *less*, so this bar is
missed.

It has come down twice: it was 102% more, then 43% more, now 15%. The remaining 5.4 MiB is a page
pool holding a file that is now within 4% of SQLite's, a process floor of which **4.1 MiB is what any
Rust binary in this workspace costs before the engine exists**, and one `CREATE INDEX`.
[Where the memory goes](performance.md#memory) attributes every megabyte.

## 2. `transaction` is under the floor

**19% slower than SQLite**, with a 95% lower bound of 69% slower, against a floor that asks for no
family slower than SQLite. This is a release blocking condition.

One workload does it. `txn.large` replaces a ten byte value with a fifty byte one, two thousand times
in one transaction. The lengths differ, so the write cannot go into the slot in place and each
statement becomes an insert into the leaf's delta area, with every thirty second one triggering a
compaction over the whole leaf. A leaf now holds twice as many rows as it did before the file
narrowed, so a compaction costs twice as much: 4.1 ms against 10.2.

The fix is a compaction that does not rewrite the whole page. Raising the delta area's limit from 32
entries to 64 was measured on both arms and does not buy it back.

## 3. Three per family bars are missed

Each on four consecutive runs. These are targets rather than requirements, and each family is faster
than SQLite:

| family | measured | the bar asks |
|---|---|---|
| `open.prepare` | 61% faster | 400% faster |
| `schema` | 38% faster | 200% faster |
| `extension` | 43% faster | 50% faster |

`open.prepare` is `SELECT 1` compiled on every call: 1,258 ns against SQLite's 420, in 25
allocations, split 320 ns to parse, 476 to bind and 608 to build the pipeline.

`schema` is `CREATE INDEX` and its backfill, where the bulk builder's floor sits above the budget.

## 4. Linux

**53% faster there against 279% on Windows.** This was settled by experiment: with a size classed
free list in place of the system allocator the two platforms run the same absolute speed, 38.97 ms
against 38.20, and it is SQLite's own arm that moves across platforms rather than this engine's.
[Linux](performance.md#linux) has the measurement. The allocator change that took Windows from 3.24x
to 3.86x has not been measured on Linux.

## 5. `write.insert.batch`

**72% slower than SQLite**: 2,000 inserts in one transaction, writing 2,491 KiB of log. It sits
inside a family that clears its bar, so it blocks nothing, and the plan is to keep the delta entries
sorted by their encoded key and find them by bisection instead of scanning and decoding each one.

## 6. `extension.fts.build`

**150% slower than SQLite**, and what is left is not micro cost. Writing the dictionary in perfect
key order still costs about 3.2 µs a row against about 2.4 µs for a `%_data` row, and this engine's
own `write.insert.batch` is 15.7 µs a row — so a shadow table write is not slow.

FTS5 here does four tree writes per document: `%_content`, `%_docsize`, the new term's `%_idx` row
and its `%_data` doclist. SQLite's accumulates the batch in memory and writes a handful of segment
blobs at commit. Closing the rest is a segment format change touching every reader of `%_idx` and
`%_data`.

The gate prints the breakdown beside the ratio: over 500 documents, `content` 1.3 ms, `tokenize` 0.4,
`docsize` 1.0, `group` 0.2, `terms` 0.3, new terms 0.4, dictionary write 1.7, flush 2.7.

## 7. Deleting the old engine

The engine that reached SQLite file format parity is still in the tree. It was measured between 30%
and 95% slower than SQLite across the families, which is why the rearchitecture happened.

Re rooting is done: `inillucent` is a re-export of `inillucent-engine` and the old facade is
`inillucent-legacy`. What remains is the deletion of `inillucent-legacy`, `inillucent-capi`,
`inillucent-session`, `inillucent-vm`, `inillucent-transaction` and `inillucent-storage` minus its
reader, and it is blocked on the driver's C ABI, which is what replaces `inillucent-capi`.

## 8. The retrieval index's footprint

**1.3 GB resident for a 3.1 GB index of 600,589 chunks** with the vectors read from the file, and 3.1
GB with them held in memory. The vectors are out of the default resident set; the graph and the
keyword postings are still all in memory and nothing has tried to make either smaller.

## 9. Threads

Access from several **processes** works: the same SHARED, RESERVED, PENDING and EXCLUSIVE protocol as
SQLite, under `PRAGMA locking_mode = normal`, measured over 37 stress rounds with two writing
processes and no lost writes. Threads inside one process do not. The engine is single threaded by
construction — its pool and trees use `RefCell`, a connection borrows the database, and there is no
parallel scan. The retrieval engine's graph build is the one thing that uses every core.

## 10. Incremental insert into the vector graph

Adding content to a retrieval index rebuilds the graph, on one thread: 132.6 s over 185,078 passages,
and about nine and a half minutes over 598,560.

## 11. A second metric on the vector index

`ORDER BY vector_distance_l2(v, ?) LIMIT k` plans as a scan and a temporary tree, because the graph
is built over unit vectors and cosine is what it minimises. The distance functions themselves answer
for every metric. pgvector spells the same restriction as an operator class, and
`WITH (metric = ...)` is where the spelling goes here once the structure has a second metric to name.

## 12. A macOS archive

Every platform's archive is built on that platform, and there is no macOS build machine.
`cargo install inillucent-cli` builds it from source in the meantime.

## The failing tests

Seventeen, all accounted for, and every one of them failed before the last two rounds of work as
well.

| binary | count | what they are |
|---|---|---|
| `schema_forms.rs` | 14 | **all 14 fail on the same thing**: they need the pinned `sqlite3` to read a file this engine wrote, or the reverse. Writing SQLite's file format was withdrawn as a requirement. Two of them get past every assertion they were written for and fail only at that step, and the behaviour they cover is checked against the oracle elsewhere without needing file format interoperation |
| `planner.rs` | 2 | `sqlite_stat1` exchanged with the oracle: the same class of thing |
| `ordering.rs` | 1 | a known difference in the order of tied rows |

The recommendation on the table is to retire the file format cases in `schema_forms.rs` and keep the
rest as tests that drive the engine. All fourteen now fail for one reason, and it is a requirement
this project has already dropped.

## Where to go next

- [Performance](performance.md) — the measurements behind items 1 to 6
- [Feature comparison](feature-comparison.md) — the full run, per workload
- [Repository](repository.md) — the crates and the test runner
