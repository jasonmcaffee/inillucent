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
| **elapsed time**, weighted over the ten families | the reference | 3.79x the speed | **279% faster** |
| **elapsed time**, the 95% lower bound the gate grades on | | 3.65x | **265% faster**, against a bar asking 200% |
| **processor time**, one round of the whole plan | 1,266 ms | 445 ms | **65% less processor** |
| **peak resident memory**, one round of the whole plan | 37.19 MiB | 42.61 MiB | **15% more** — the one loss |
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
| `read.point` | 16% | one row by rowid, by integer key, and through a secondary index | **3,083% faster** (31.83x) | 28.20x | 2.00x — met |
| `large.values` | 4% | text and blobs across the boundary where a value stops fitting in a leaf | **1,018% faster** (11.18x) | 8.50x | 1.50x — met |
| `read.analytical` | 10% | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | **579% faster** (6.79x) | 5.57x | 5.00x — met |
| `read.range` | 12% | selective ranges, forward and reverse, covering and not | **403% faster** (5.03x) | 4.04x | 3.00x — met |
| `read.join` | 8% | two table and four table joins | **324% faster** (4.24x) | 2.87x | 3.00x — bar missed |
| `write` | 20% | insert, update, delete, upsert, with and without indexes | **71% faster** (1.71x) | 1.50x | 1.50x — met |
| `open.prepare` | 8% | parse, bind, step one row, reset | **61% faster** (1.61x) | 1.20x | 5.00x — bar missed |
| `extension` | 8% | JSON, FTS5, R-Tree | **43% faster** (1.43x) | 1.23x | 1.50x — bar missed |
| `schema` | 4% | `CREATE INDEX` and its backfill | **38% faster** (1.38x) | 1.35x | 3.00x — bar missed |
| `transaction` | 10% | autocommit, small batches, large batches, savepoints | **19% slower** (0.84x) | 0.59x | no slower than SQLite — **under the floor** |

`transaction` being slower than SQLite is a release blocking condition, and one workload causes it.
The next section says which and why.

## The workloads that are slower

Thirty workloads. Twenty three are faster than SQLite. These seven are not.

| workload | family | how much slower | why |
|---|---|---|---|
| `txn.large` | `transaction` | **376% slower** | 2,000 `UPDATE`s in one transaction, 1.56 µs each against SQLite's 450 ns |
| `extension.fts.build` | `extension` | **150% slower** | 2,000 tree writes per 500 documents — `%_content`, `%_docsize`, then 507 dictionary rows and 507 doclists at the flush — where SQLite writes about 1,000 rows and one segment blob |
| `prepare.trivial` | `open.prepare` | **138% slower** | `SELECT 1` compiled on every call: 1,258 ns against 420, in 25 allocations. Split by the profiler: 320 ns to parse, 476 to bind, 608 to build the pipeline |
| `write.insert.batch` | `write` | **72% slower** | 2,000 inserts in one transaction, writing 2,491 KiB of log |
| `range.lookaside` | `read.range` | 6% slower | |
| `join.range` | `read.join` | 6% slower | |
| `extension.json` | `extension` | 3% slower | the constant argument to `json_extract` is parsed again on every call |

`txn.large` replaces a ten byte value with a fifty byte one, two thousand times, in one transaction.
The lengths differ, so the write cannot go into the slot in place and each statement becomes an
insert into the leaf's delta area, with every thirty second one triggering a compaction over the whole
leaf. A leaf now holds twice as many rows as it did before the file got smaller, so a compaction
costs twice as much: 4.1 ms against 10.2. The same table written without a width change,
`write.upsert`, did not move at all.

That is the trade the file size bought, and it is paid in one place. The fix is a compaction that
does not rewrite the whole page. Raising the delta area's limit from 32 entries to 64 was measured
on both arms and does not buy it back: it halves the compactions and makes every read of a written
leaf walk twice as far, and the second effect is larger — `large.values` loses half its speed for a
0.01x gain in `write`.

The other end of the same table is the `read.point` family at **3,083% faster** and `large.values` at
**1,018% faster**. A point lookup by rowid, a lookup that misses, a lookup through a secondary index
and a read of a value too wide for a leaf are each between ten and thirty times SQLite's speed.

## Memory

**42.61 MiB against SQLite's 37.19 — 15% more.** The contract asks for 5% less, so this bar is
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

- **One scale.** These are the medium fixture, 100,000 rows. At 5,000 rows the headline is 2.56x, and
  at 600,000 it is 4.06x. Families behave differently at each: `extension` is 43% faster at medium
  and 27% faster at large; `write` is 51% *slower* at small, 71% faster at medium and 471% faster at
  large.
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
