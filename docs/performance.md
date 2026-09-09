# Performance against SQLite

Four measurements decide whether this engine is worth changing to: how long a workload takes, how
much processor it burns, how much memory it holds, and how big the file is. Three are wins and one is
a loss, and all four are here.

**[Feature comparison](feature-comparison.md) carries the full run**, per workload, per family, with
every interval and every control. This page is the summary.

## The four numbers

Measured at 100,000 rows on Windows, over the ten workload families the performance contract weights,
30 paired rounds per run, four consecutive runs, medians of the two middle runs.

| | SQLite 3.53.4 | inillucent | |
|---|---|---|---|
| **elapsed time**, weighted over the ten families | the reference | 3.95x the speed | **295% faster** |
| **elapsed time**, the 95% lower bound the gate grades on | | 3.63x | **263% faster**, against a bar asking 200% |
| **processor time**, one round of the whole plan | 1,293 ms | 414 ms | **68% less processor** |
| **peak resident memory**, one round of the whole plan | 37.19 MiB | 42.59 MiB | **15% more** — the one loss |
| **the database file**, the same imported fixture | the reference | 1.036x | within 4% |

Every workload's answer is hashed and compared with SQLite's before its timing is allowed to count.
**All 30 workloads agreed on every round of all four runs.**

Both engines get the same memory budget: a pool of 4,096 frames of 32 KiB here, 128 MiB, and
`PRAGMA cache_size = -131072` on SQLite's arm, also 128 MiB. Both run under `synchronous = FULL`.
Neither uses a plan cache.

The processor and memory figures are a matched pair: **one child process each**, both opening a
finished file the parent built, both running one round of the same plan. Neither figure is a delta
taken inside a running program.

## By family

`weight` is what the contract gives the family in the headline. `bar` is what the contract asks of
it, expressed as the family's own ratio.

| family | weight | what it measures | measured | 95% lower bound | bar |
|---|---|---|---|---|---|
| `read.point` | 16% | one row by rowid, by integer key, and through a secondary index | **3,092% faster** (31.92x) | 28.32x | 2.00x — met |
| `large.values` | 4% | text and blobs across the boundary where a value stops fitting in a leaf | **1,134% faster** (12.34x) | 9.27x | 1.50x — met |
| `read.analytical` | 10% | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | **591% faster** (6.91x) | 5.62x | 5.00x — met |
| `read.range` | 12% | selective ranges, forward and reverse, covering and not | **395% faster** (4.95x) | 3.90x | 3.00x — met |
| `read.join` | 8% | two table and four table joins | **329% faster** (4.29x) | 2.88x | 3.00x — bar missed |
| `write` | 20% | insert, update, delete, upsert, with and without indexes | **89% faster** (1.89x) | 1.69x | 1.50x — met |
| `open.prepare` | 8% | parse, bind, step one row, reset | **64% faster** (1.64x) | 1.18x | 5.00x — bar missed |
| `extension` | 8% | JSON, FTS5, R-Tree | **48% faster** (1.48x) | 1.29x | 1.50x — bar missed |
| `schema` | 4% | `CREATE INDEX` and its backfill | **31% faster** (1.31x) | 1.27x | 3.00x — bar missed |
| `transaction` | 10% | autocommit, small batches, large batches, savepoints | **27% slower** (0.79x) | 0.55x | no slower than SQLite — **under the floor** |

`transaction` being slower than SQLite is a release blocking condition, and one workload causes it.
The next section says which and why.

## The workloads that are slower

Thirty workloads. Twenty four are faster than SQLite. These six are not.

| workload | family | how much slower | why |
|---|---|---|---|
| `txn.large` | `transaction` | **669% slower** | 2,000 `UPDATE`s in one transaction, 1,896 ns each against SQLite's 482 |
| `extension.fts.build` | `extension` | **104% slower** | four tree writes per document — `%_content` and `%_docsize`, and at the flush a dictionary row and a doclist row for each of 507 terms — where SQLite writes about 1,000 rows and one segment blob |
| `prepare.trivial` | `open.prepare` | **92% slower** | `SELECT 1` compiled on every call, in 25 allocations. Split by the profiler: 417 ns to parse, 520 more to bind, and the rest to build a pipeline |
| `write.insert.batch` | `write` | **56% slower** | 2,000 inserts in one transaction; a split writes four whole page images to the log |
| `join.range` | `read.join` | 14% slower | an index range and a row fetch per entry, where SQLite amortises one statement's overhead over two hundred rows and this does not |
| `range.lookaside` | `read.range` | 13% slower | the same shape |

**None of `txn.large`'s gap is in the storage engine, and that was settled by removing the storage
engine from it.** The whole tree write was ablated out of the in-place update path — the statement
found its row, decided what to write, and returned without writing — and the workload measured
**1.54 microseconds against 1.53**. What is left is what a statement costs before it reaches a tree:
an already-prepared `SELECT 1`, which reads no table and binds no parameter, costs **827 ns and
twenty-two heap allocations**, because the operator chain, the column names and the `EXPLAIN`
description are rebuilt on every execution. The same `UPDATE` bound to a rowid that matches nothing
costs 950 ns, which is more than half of the 1,896 it costs when it does match.

That one cost is under `transaction`, `open.prepare`, `extension` and `schema` alike, and it is why a
query that spreads one statement's overhead across two hundred rows — `join.range`,
`range.lookaside` — sits just under SQLite while a point lookup sits thirty times over it. The
mechanism that removes it is already in the tree and is not connected to anything:
`physical::build_statement` holds the operator chain across executions and rebuilds only the source,
and nothing in the execution path calls it. Connecting it needs the chain to stop borrowing the
catalog and a parameter to be read when the expression is evaluated rather than folded in when it is
built.

The other end of the same table is the `read.point` family at **3,092% faster** and `large.values` at
**1,134% faster**. A point lookup by rowid, a lookup that misses, a lookup through a secondary index
and a read of a value too wide for a leaf are each between ten and thirty times SQLite's speed.

### What an `UPDATE` that changes a value's length used to cost

Until task-1890 a heap slot could only be written over by a value of **exactly** the same length, so
`txn.large` — which replaces an eight byte `note 1234` with a forty-two byte
`row 1234 lorem ipsum ...` — took none of the in-place path at all. Every statement became a
tombstone plus an insert into the leaf's delta area, and every thirty-second one a compaction over
every live row of the leaf.

A longer value is now written at the bottom of the leaf's heap, after the tombstone bitmap and the
delta area have been moved down by its length, and the slot is repointed at it; a shorter one is
written where the old one lay and the slot's length is lowered. The bytes left behind are what SQLite
calls fragments and the next compaction reclaims them. The log record did not change: an in-place
update is replayed by re-running the write over a page that LSN ordering has already put back into
the state the original write saw, and the relocation is a function of that page and the value alone.

## Memory

**42.59 MiB against SQLite's 37.19 — 15% more.** The contract asks for 5% less, so this bar is
missed, and it is the only headline that is a loss.

It has been worked twice. It was **102% more** two rounds of work ago and **43% more** one round ago.
The four changes that took it from 102% to 43% were: bounding the redo buffer to 512 KiB, stopping
the index build holding three copies of the tree, collecting a version log that nothing was
collecting, and capping the allocator's free list in bytes as well as in blocks.

What took it from 43% to 15% was the **file**, not another buffer. The attribution reads the process
high water mark after every workload and prints the page pool's own bytes beside the total:

| workload | peak, before | pool | everything else | peak, after | pool | everything else |
|---|---|---|---|---|---|---|
| the file opened and the pool warmed | 31.50 | 22.59 | 8.90 | **26.74** | **17.84** | 8.90 |
| every read workload | 31.54 | 22.59 | 8.95 | 26.79 | 17.84 | 8.94 |
| `write.insert.batch` | 34.73 | 22.94 | 11.55 | 30.92 | 18.19 | 12.20 |
| `schema.index` | **51.35** | 29.84 | 12.23 | **46.61** | 24.66 | 12.78 |

The "everything else" column does not move. The whole 4.74 MiB came out of the page pool, and the
pool fell because the file did.

Where the remaining 5.4 MiB is:

| | inillucent | SQLite | what it is |
|---|---|---|---|
| the cached database | 16.56 MiB | about 16 MiB | the `.rdb` is 1.036x the `.db`. Under 0.6 MiB left here |
| the process floor | 8.49 MiB | about 4.2 MiB | **4.1 MiB of it is what any Rust binary in this workspace costs before the engine exists** — a trivial 110 KB one measures the same. About 2.2 MiB is this engine's own code and statics |
| `schema.index`'s rise | 12.50 MiB | about 15.9 MiB | the pages the new index occupies plus the sort's arena. This one is **smaller** than SQLite's |

So most of what is left is the operating system's, which neither engine escapes, and one
`CREATE INDEX`.

**And the peak is reached by that one statement.** Read afresh over four runs, the high-water mark
after every workload:

| workload | peak MiB | this workload added |
|---|---|---|
| the file opened and the pool warmed | 25.13 | 25.13 |
| every read workload, all eleven of them | 25.18 | 0.05 in total |
| `write.insert.batch` | 28.95 | 3.77 |
| the other four write workloads | 30.07 | 1.12 in total |
| **`schema.index`** | **42.61** | **12.54** |
| every remaining workload | 42.61 | nothing |

The whole plan holds **30.07 MiB** until it builds an index, and building one adds twelve and a half.
So the bar is not missed by a buffer that is slightly too big everywhere; it is missed by one
statement, and by the arena its sort holds.

That arena is the one lever left, and it is priced rather than pulled: task-1869 measured spilling
the sorted run to a temporary file at about **8 ms on a 27 ms statement**, which puts the `schema`
family under the 1.00x floor the contract sets. Buying memory with a floor is the trade that ticket
declined and this one declines again.

## Disk

The imported fixture, both engines given the same data:

| | inillucent | SQLite |
|---|---|---|
| the medium fixture | 17,432,576 B | **1.036x** — within 4% |

An integer column is now as wide as its own values rather than always eight bytes: the builder picks
the narrowest of 1, 2, 4 and 8 bytes that holds every value in a leaf, and writes the choice into a
field the format always carried. A file written before that change reads unchanged, because its pages
say eight. Tree by tree over the medium fixture, at 32 KiB pages:

| tree | pages before | pages after | entries per page |
|---|---|---|---|
| `main_table` | 468 | **388** | 241 |
| `main_category` | 85 | **34** | 2,941 |
| `main_key` | 57 | **23** | 3,704 |
| `side_table` | 30 | **16** | 1,190 |
| `side_owner` | 15 | **4** | 4,167 |
| `wide`, a text column | 58 | 58 | 6.9 |

`main_category` leads on a column with 64 distinct values, so its width is one byte, which is why it
is the biggest saving. `wide` does not move at all, which is the check that nothing narrowed that
should not have.

Built through `INSERT ... SELECT` rather than imported, the picture is different: 200,000 rows are
15.9 MB here against SQLite's 8.7 MB, at 32 KiB page granularity. The redo log
retires to **0.0 MB** at a checkpoint, where it used to hold 94.6 MB across two segments for ever.

## Linux

**The same binary measured 53% faster on Linux, not 279%.** That difference was settled by
experiment rather than argued about, and the finding is that it is not a Linux problem.

With a size classed free list in place of the system allocator, a `SELECT 1` compile goes from
46.95 ms to 38.97 ms on Windows (17% faster) and from 39.91 ms to 38.20 ms on Linux (4% faster) —
and **the two platforms then run the same speed**, 38.97 against 38.20. On the Windows compile the C
runtime's heap is 59% of the time.

SQLite does per statement work with the operating system that Windows charges heavily for and Linux
barely does. So SQLite's arm — the denominator of every ratio on this page — moves across platforms
while this engine's does not. The absolute work is the same on both, and lowering it is what the
missed bars need. The allocator change that took Windows from 3.24x to 3.86x has not been measured on
Linux.

## What is not measured here

- **One scale.** These are the medium fixture, 100,000 rows. At 5,000 rows the headline is 3.09x, and
  at 600,000 it is 4.63x. Families behave differently at each, and two of them invert: `write` is
  **47% slower** at 5,000 rows, 89% faster at 100,000 and **450% faster** at 600,000, because a
  bigger table amortises what a write costs per statement over more of a page; `extension` is 29%
  faster at small, 48% at medium and 71% at large, and it is the one family that **meets its bar at
  600,000 rows** and misses it at the other two.
- **One machine.** Windows 11 on x64. The disk matters more than it looks: part way through a four
  run sequence, `txn.batched` — 200 commits and 200 `fsync`s — goes from 288 ms to 836 ms **on
  SQLite's own arm**, on the same fixture with the same binary, because the volume stops keeping up
  with the couple of gigabytes a sequence writes. That row is published beside every run so a reader
  can tell a slow volume from a slow engine. Twelve runs across three sequences were taken and the
  pattern held in all of them.
- **Per workload processor time**, which the gate reports but which is quantised to the Windows
  scheduler tick of 15.625 ms. Only the per round totals on this page should be quoted.

## Reproducing it

```sh
cargo build --release

# the pinned SQLite 3.53.4 oracle
pwsh tools/sqlite-reference.ps1      # Windows
bash tools/sqlite-reference.sh       # Linux

# The fixtures are not checked in — they are 17 MB, 94 MB and 600 MB — and each
# gate run needs its own copy: the schema.index workload leaves an index behind
# on the SQLite arm, so a second run against the same file stops on
# `index main_label already exists`.
bash tools/build-gate-fixtures.sh <dir>
cp <dir>/medium.db <dir>/medium-run1.db

target/release/inillucent-fullgate <dir>/medium-run1.db --scale medium --rounds 30 \
    --page-size 32768 --frames 4096
target/release/inillucent-readgate  <dir>/medium-read.db  --scale medium
target/release/inillucent-shellrss                        # peak memory, one shell each, same data
```

The gate binaries and the shell install `inillucent-alloc` as their global allocator. It is part of
the build in the same way fat link time optimisation and a single codegen unit are: SQLite ships its
own memory subsystem, so measuring a Rust workspace on the platform allocator measures a build
configuration rather than an engine. It is worth 3.24x to 3.86x on the medium gate.

[Repository](repository.md) covers the rest of the instruments and the test runner.
