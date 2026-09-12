# What is not there yet

In the order it is being worked, each with the measurement behind it.

## 1. Memory is the one headline SQLite still wins

**42.61 MiB against SQLite's 37.19 — 15% more**, on the same 128 MiB budget, while running 326%
faster and spending 67% less processor. The performance contract asks for 5% *less*, so this bar is
missed.

It has come down twice: it was 102% more, then 43% more, now 15%. The remaining 5.4 MiB is a page
pool holding a file that is now within 4% of SQLite's, a process floor of which **4.1 MiB is what any
Rust binary in this workspace costs before the engine exists**, and one `CREATE INDEX`.
[Where the memory goes](performance.md#memory) attributes every megabyte.

## 2. Four per family bars are missed

Each on four consecutive runs. These are targets rather than requirements, and every one of these
families is still faster than SQLite. `transaction` was under the contract's **floor** on all four
runs and is no longer on this list at all: task-1890 took it to 241% faster.

| family | measured | the bar asks |
|---|---|---|
| `open.prepare` | 58% faster (1.58x) | 400% faster |
| `read.join` | 311% faster (4.11x) | 200% faster — missed on the 95% lower bound only, 2.89x |
| `extension` | 30% faster (1.30x) | 50% faster |
| `schema` | 27% faster (1.27x) | 200% faster |

**Two of the four are not gaps, and the arithmetic says so.** `open.prepare` is `prepare.point` at
4.96x and `prepare.trivial` at 0.51x; for the family to reach 5.00x, `prepare.trivial` would have to
reach 5.05x, and SQLite compiles, binds, steps and resets `SELECT 1` in 483 ns — so the bar is asking
for 96 ns. `schema` is one `CREATE INDEX`, 26.78 ms against SQLite's 33.54, whose stages are
`scan 3.7, sort 5.5, pack 11.4, catalog 0.3, seal 5.8`: a packer costing nothing at all leaves
15.4 ms, which is 2.18x. Neither bar has been moved — they are in `compat/perf/contract.toml`, they
were written before any measurement, and changing one to meet a number is not a decision this
document makes.

`read.join` misses on its lower bound alone, and what would close it is the operator chain reuse in
item 3. **`extension` no longer has a named fix.** It used to say "item 6 has it", on the strength of
a prediction that merging FTS5's dictionary row and doclist row was worth 2.9 ms of
`extension.fts.build`'s 10.97. task-1911 built that merge and measured it, and the prediction was
wrong — item 6 has the numbers.

## 3. The operator chain is rebuilt on every execution

Measured, both arms warmed and the order reversed so a warm cache could not flatter either: reusing a
compiled chain is worth **738 ns to 358** on `SELECT 1`, **1,413 to 786** on a point lookup, and
**71,672 to 59,983** on the 200-row range scan that `join.range` and `range.lookaside` are shaped
like — which is the 14-15% those two sit under SQLite.

`physical::build_statement` already holds a chain across executions and rebuilds only the source, and
nothing calls it. What stops it is ownership rather than effort: an index nested loop holds a borrow
of the tree it reads, and the engine keeps its trees in a map whose write path takes them mutably, so
caching a chain means reference counting the trees. The failure mode of getting that borrow
discipline wrong is a runtime panic on a shape as ordinary as an `UPDATE` that reads the table it
writes, which is why it wants its own run at it with the discipline designed rather than discovered.

## 4. Linux

**53% faster there, where Windows measured 279% at the time.** This was settled by experiment: with
a size classed free list in place of the system allocator the two platforms run the same absolute
speed, 38.97 ms against 38.20, and it is SQLite's own arm that moves across platforms rather than
this engine's. [Linux](performance.md#linux) has the measurement. Neither the allocator change that
took Windows from 3.24x to 3.86x nor anything since has been measured on Linux, so the Linux figure
is older than the 326% Windows headline rather than a comparison with it.

Re-measuring wants a Linux machine that is not also running the Windows arm. Both inside one box —
WSL beside Windows — would measure the contention rather than the platform. task-1911 watched that
happen at a smaller scale: with four agents on the box, `extension.fts.build` read 0.50x and 0.64x
for the same code an hour apart, because SQLite's own arm moved 62% between the runs.

## 5. `write.insert.batch`

**72% slower than SQLite**: 2,000 inserts in one transaction, writing 2,491 KiB of log. It sits
inside a family that clears its bar, so it blocks nothing. Measured again on a quiet box in
task-1911: **0.50x, interval [0.46, 0.52]**, 43.19 ms against 21.39.

**This item named the wrong code until task-1911, and the plan it carried was aimed at a file this
workload never reaches.** It said the delta entries were scanned and decoded one at a time and that
finding them by bisection would close it. The delta log belongs to `inillucent-search`, and
`write.insert.batch` inserts into `main_table` — a plain relational table, with no
`CREATE VIRTUAL TABLE ... USING inillucent_search` in its definition — so `deltas_above` is never
called by it.

The scan-and-decode is real and is in `crates/inillucent-tree/src/leaf.rs`. `locate()` walks each
leaf's **unsorted delta area** — up to `DELTA_LIMIT` 32 entries, with a typed decode per column — on
every insert, and `main_table` carries two secondary indexes, so a batch pays it three times a row.
Closing it means sorting or indexing that area, which is a change to the on-disk leaf format and
therefore to recovery, and it wants its own ticket rather than a pass inside somebody else's.

The bisection the old plan asked for **was** built, in the place it actually applies:
`ShadowStore::scan_from` seeks to a key instead of walking and filtering, with the trait's default
doing the correct-but-slow skip so no implementor can be forgotten, and `Store::deltas_above` seeks
to `covered + 1`. A counting store proves it: 1,000 delta entries, and a seek past the watermark
visits 1,001 rows where the scan-and-filter visited 2,000. That is worth having — a table carrying a
vector index reads its delta log on every write, which is what item 14 of the old list made usable —
and it is not what this number measures.

## 6. `extension.fts.build`

**Measured on a quiet box in task-1911: 11.52 ms against SQLite's 6.28, 0.56x.** The 10.97 against
3.68 this item used to quote is from an older run; SQLite's own arm is slower on this box today, so
the two ratios are not comparable and only the new pair is.

**The doubled write is gone, and it bought nothing.** FTS5 here used to do four tree writes per
document — `%_content`, `%_docsize`, the new term's `%_idx` row and its `%_data` doclist. task-1911
made the last two one row: `%_idx` carries the doclist inline rather than an integer naming the
`%_data` row it lived in. This item predicted that was worth about 2.9 ms of the 10.97, which is 26%,
and would take the family over its bar.

It is worth nothing measurable. Measured as a genuine A/B — the same fixture and command, the change
alternated in and out of the tree with a release rebuild each time, at two round counts:

| rounds | | inillucent | SQLite | ratio |
|---|---|---:|---:|---:|
| 15 | before | 11.40 ms | 5.85 ms | 0.50x |
| 15 | after | 13.14 ms | 7.14 ms | 0.55x |
| 40 | before | 11.52 ms | 6.28 ms | 0.56x |
| 40 | after | 12.99 ms | 6.68 ms | 0.53x |

The absolute times are not the comparison: SQLite's arm, which nothing in that change can touch, rose
14% and 6% in the two "after" runs, so both arms moved together and that is the box. The paired ratio
controls for it, and it reads the same number in both directions. A 26% win would have shown as about
0.70x against 0.56x, outside every interval here.

The breakdown says where it went. Before: `dict write 1.8 ms, flush 2.9`. After: `dict write 4.1,
flush 4.1`. The second write really did disappear — and the doclist bytes went with it, out of a
rowid-keyed `%_data` row and into a `WITHOUT ROWID` row keyed by `(segid, term)`, where rewriting
costs about what the extra row saved. **The row count was never what this workload pays for. The
bytes are.**

The merge is kept, because it is simpler and because old files still read — `%_idx`'s third column is
self-describing, an `Integer` being an older build's `%_data` page and a `Blob` this build's doclist,
and `crates/inillucent-compat/tests/fts5_legacy_layout.rs` manufactures an old-layout file out of one
this build wrote and requires the same answers. Breaking the branch that reads the indirection makes
a four-document index answer **nothing**, which is why that branch is tested rather than reasoned
about.

What is left is the segment format, and it is the whole of the remaining gap. SQLite accumulates the
batch in memory and writes a handful of segment blobs at commit, merging them incrementally as more
arrive ([`automerge` and `crisismerge`](https://www.sqlite.org/fts5.html)). This engine still writes
three trees per document as it goes.

## 7. Deleting the old engine

The engine that reached SQLite file format parity is still in the tree. It was measured between 30%
and 95% slower than SQLite across the families, which is why the rearchitecture happened.

**The thing it was waiting for exists.** `drivers/inillucent-driver-capi` is the C ABI that replaces
`inillucent-capi`, with its own header, tests and conformance suite, so "blocked on the driver's C
ABI" stopped being true.

What is removable, audited rather than assumed:

| crate | lines | what still names it |
|---|---:|---|
| `inillucent-vm` | 16,356 | `inillucent-session`, and 4 files in `inillucent-compat` |
| `inillucent-session` | 8,331 | `inillucent-legacy`, and 19 files in `inillucent-compat` |
| `inillucent-capi` | 5,350 | itself, and 2 files in `inillucent-compat` |
| `inillucent-legacy` | 660 | `inillucent-capi`, and 15 files in `inillucent-compat` |

30,697 lines, and nothing outside the test crate reaches any of them: no shipped binary, no driver,
and not `inillucent-engine`.

**`inillucent-storage` and `inillucent-transaction` stay**, and the exception is larger than "minus
its reader" suggests. `inillucent-engine` and `inillucent-migrate` both depend on
`inillucent-sqlite-reader`, which depends on both of them and on `inillucent-catalog`, which depends
on `inillucent-storage` in turn — `analyze.rs`, `ddl.rs` and `load.rs` all read through its `Pager`
and `BTreeCursor`. Reading a SQLite file in order to migrate away from it is what keeps 18,764 lines
of the old engine alive, and that is a feature rather than a leftover.

So the deletion is 30 files of `inillucent-compat` rewritten first, several of them **differential**
tests that run the old engine beside the new one and compare. Deleting the old engine deletes the
comparison, so each of those needs a decision about what it checks instead, and `tests/capi.rs` needs
re-pointing at `drivers/inillucent-driver-capi`.

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

## 10. A generation is one blob, so publishing one still costs the whole corpus

Adding content no longer rebuilds the graph. task-1894 made a commit **fold**: the published
generation is loaded and each entry of the delta log is inserted into it, which is one graph insert
per row written rather than one per row in the table. The nine and a half minutes over 598,560
passages that used to sit here was a single-pass build running inside somebody's `INSERT`; that build
now happens only when it is asked for, by `INSERT INTO t(t) VALUES('compact')`.

What is still proportional to the corpus is **publishing**. A generation is one serialised index, so
writing a new one reads and writes the whole thing however few rows changed. That is why the default
delta log is a share of the table, `max(1024, rows / 8)`, rather than a constant: a constant would
publish the generation far too often. It also means the default lets one commit in every eight rows
pay `rows / 8` graph inserts, so write latency under the default still rises with the table.

The thing that is not built is **segmented generations** — many small immutable segments merged at
read time, the way an LSM tree works — which would make both the graph work and the bytes written
proportional to the batch rather than to the corpus. That is what the systems built for this do:
[YugabyteDB's vector LSM](https://www.yugabyte.com/blog/yugabytedb-vector-indexing-architecture/)
indexes an in-memory buffer with HNSW, flushes a full buffer to disk as an immutable chunk, and fans
a search across every in-memory and on-disk chunk;
[Milvus](https://www.cs.purdue.edu/homes/csjgwang/pubs/SIGMOD21_Milvus.pdf) manages dynamic data the
same way and builds vector indexes only over the immutable segments, because a graph is expensive to
update in place and cheap to build once. It is the same argument FTS5 makes for its own segments in
item 6, and closing both is one idea.

Until then a table that needs its write latency pinned declares `compact = N`, which fixes the delta
log at `N` entries and therefore fixes the graph work per published generation, at the price of
writing the generation every `N` rows.
[Keeping a vector index current](relational-architecture.md#10-keeping-a-vector-index-current)
publishes the measured range and how to choose `N`.

## 11. A macOS archive

Every platform's archive is built on that platform, and there is no macOS build machine.
`cargo install inillucent-cli` builds it from source in the meantime.

## What task-1911 closed

Five items came off this list, and the numbering above is what is left. Each is named here so a
reader who remembers the old numbers can find what happened to them.

- **A vector index answering zero rows instead of the rows it holds** — the wrong answer this engine
  is built not to have. Two faults, both a write that is never committed: `create_vector_index`
  returned without the `seal()` every other directive ends with, and `ImportedDatabase::write` reads
  `next_txn` and moves it on at once, so `current_txn()` answered the *following* number for the rest
  of the statement and every index entry was logged into a transaction nothing commits. A third
  change makes the index worth having rather than merely correct: the backfill and the write path now
  flush the module, so an index publishes a generation instead of replaying its whole delta log on
  every query — 2.34 s against 0.66 for the exhaustive scan, before. `examples/rag-agent`'s ten
  questions all answer through an index now, and `scripts/verify-indexed.sh` is what says so.
- **`embed(TEXT)` called once per row when it is a constant.** A deterministic scalar whose arguments
  do not vary within a statement is now evaluated once for the statement — folded at translation when
  every argument is a literal, and at execution setup when one is a bound parameter, because a
  compiled chain is re-bound and a parameter folded at compile time would be correct only for the
  values it was built against. `embed` is registered `deterministic`, which it always was. The
  documented query over `examples/rag-agent`'s 2,661 passages went from **105.7 s to 1.50 s**.
- **A registered function refused in the write path.** `INSERT ... VALUES`, `UPDATE ... SET` and
  `RETURNING` now reach one, through a catalog parameter on `RowSpace::compile` and the callers that
  reach it. `INSERT ... SELECT` is no longer the only shape that works.
- **A second metric on the vector index.** `WITH (metric = 'l2')`, honoured by the graph rather than
  only parsed: the metric decides the distance *and* whether vectors are normalised at all, the
  persisted generation records which metric it was built under and refuses one that disagrees, and
  the planner probes only when the `ORDER BY` function matches, falling back to the scan otherwise. A
  generation with no stored metric reads as cosine, so existing files are unaffected. It also
  uncovered a live defect: `streaming_search` ranked by cosine whatever metric the set was built
  under.
- **The seventeen failing tests.** There were none. `schema_forms` 14, `planner` 5 and `ordering` 2
  all pass, with the pinned `sqlite3` present rather than absent. task-1869 had already removed the
  cause — `crates/inillucent-compat/src/interchange.rs` moves a database between the engines as
  `.dump` output replayed by the reference shell, rather than handing `sqlite3` a file it cannot read
  — and this document was simply never updated.

## Where to go next

- [Performance](performance.md) — the measurements behind items 1 to 6
- [Feature comparison](feature-comparison.md) — the full run, per workload
- [Repository](repository.md) — the crates and the test runner
