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
| **elapsed time**, weighted over the ten families | the reference | 4.26x the speed | **326% faster** |
| **elapsed time**, the 95% lower bound the gate grades on | | 4.13x | **313% faster**, against a bar asking 200% |
| **processor time**, one round of the whole plan | 1,266 ms | 422 ms | **67% less processor** |
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
| `read.point` | 16% | one row by rowid, by integer key, and through a secondary index | **2,658% faster** (27.58x) | 24.95x | 2.00x — met |
| `large.values` | 4% | text and blobs across the boundary where a value stops fitting in a leaf | **1,157% faster** (12.57x) | 9.39x | 1.50x — met |
| `read.analytical` | 10% | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | **594% faster** (6.94x) | 5.77x | 5.00x — met |
| `read.range` | 12% | selective ranges, forward and reverse, covering and not | **381% faster** (4.81x) | 3.83x | 3.00x — met |
| `read.join` | 8% | two table and four table joins | **311% faster** (4.11x) | 2.89x | 3.00x — bar missed on the lower bound |
| `transaction` | 10% | autocommit, small batches, large batches, savepoints | **241% faster** (3.41x) | 2.74x | no slower than SQLite — met |
| `write` | 20% | insert, update, delete, upsert, with and without indexes | **92% faster** (1.92x) | 1.65x | 1.50x — met |
| `open.prepare` | 8% | parse, bind, step one row, reset | **58% faster** (1.58x) | 1.15x | 5.00x — bar missed |
| `extension` | 8% | JSON, FTS5, R-Tree | **30% faster** (1.30x) | 1.09x | 1.50x — bar missed |
| `schema` | 4% | `CREATE INDEX` and its backfill | **27% faster** (1.27x) | 1.24x | 3.00x — bar missed |

**No family is below the 1.00x floor on any of the four runs**, which is the release condition.
`transaction` was below it on all four before task-1890 and is now 3.41x; what that took, and the
defect in the measurement it uncovered, is the section after next.

## The workloads that are slower

Thirty workloads. Twenty four are faster than SQLite. These six are not.

| workload | family | ratio | how much slower | why |
|---|---|---|---|---|
| `extension.fts.build` | `extension` | 0.36x | **178% slower** | four tree writes per document — `%_content` and `%_docsize`, and at the flush a dictionary row and a doclist row for each of 507 terms — where SQLite writes about 1,000 rows and one segment blob |
| `prepare.trivial` | `open.prepare` | 0.51x | **98% slower** | `SELECT 1` compiled on every call, in 25 allocations. Split by the profiler: 417 ns to parse, 520 more to bind, and the rest to build a pipeline |
| `write.insert.batch` | `write` | 0.58x | **72% slower** | 2,000 inserts in one transaction; a split writes four whole page images to the log |
| `join.range` | `read.join` | 0.87x | 15% slower | an index range and a row fetch per entry, where SQLite amortises one statement's overhead over two hundred rows and this does not |
| `range.lookaside` | `read.range` | 0.88x | 14% slower | the same shape |
| `extension.json` | `extension` | 0.96x | 5% slower | the extraction itself, plus two uncontended mutex acquisitions per call; the parse of a repeated document and path is already cached |

**`txn.large` is no longer on this list.** It was the slowest workload on the board at 0.09x, it decided
the `transaction` floor, and it is now **3.70x**, where the median round takes 2.66 ms against
SQLite's 9.65. Two things got it there and only one of them is the engine.

**How a ratio on this page is taken.** A family's or a workload's ratio is the gate's own paired-round
figure — it pairs the two arms round by round and reports the middle of the thirty — and the number
printed here is the median of the two middle runs of four. An absolute time printed beside it is the
median of the same four runs' own medians. The two are different summaries of one set of rounds, so
dividing the printed times gives a number close to the printed ratio rather than exactly it: 2.66 and
9.65 divide to 3.63 where the paired figure is 3.70. The paired figure is the one the contract grades
and the one quoted.

### What a statement costs before it reaches a tree

The whole tree write was ablated out of the in-place update path — the statement found its row,
decided what to write, and returned without writing — and `txn.large` measured **1.54 microseconds
against 1.53**. None of its gap was in leaves, delta areas or compactions. It was in what a statement
costs to set itself up:

| `UPDATE side_table SET note = ?2 WHERE id = ?1` | ns each | heap allocations |
|---|---|---|
| before | 2,219 | 33.7 |
| a layout shared rather than deep-copied three times a statement | 1,896 | 21.3 |
| the row space, assignments and declarations built once per compiled statement | 1,318 | 14.3 |
| the undo image taken from the caller instead of read again | **1,235** | **13.3** |
| the same statement, where no row matches | **615** | **7.0** |

What made the middle row possible is that **a parameter is now read when the expression is
evaluated** rather than folded into it when it is built. `translate` answered `?2` with an
`Expr::Literal` holding whatever was bound at the time, so nothing compiled could outlive one
execution's values — which is why `Statement::rebindable` existed to refuse a re-run. An
`Expr::Parameter` reads a cell the statement refreshes instead, and it costs a repeated literal
nothing it did not already cost: a text literal clones per evaluation either way.

### The workload that was measuring nothing

`fullgate` runs a round's workloads in order against one database, and `txn.batched` and `txn.large`
are the same statement over the same scattered rowids with the same `row {iteration} lorem ipsum ...`
text and the same repeat. So by the time `txn.large` ran, **every row it touched already held the
bytes it was about to write** — 2,000 updates that changed nothing, under a description saying
"2,000 `UPDATE`s in one transaction".

That was hiding a real defect rather than only mis-measuring. `only_change` answered `None` both when
*nothing* differed and when *several* columns did, so the caller took the most expensive path it has —
a tombstone, a delta insert, and a compaction every `DELTA_LIMIT` writes — for the cheapest case
there is. Running the same `UPDATE` twice over the same rows:

| pass | ns each | allocations | inserted | in place |
|---|---|---|---|---|
| values differ | 1,723 | 13.3 | 0.00 | 0.25 |
| values identical, before | 4,067 | 56.7 | 0.25 | 0.00 |
| values identical, after | **867** | **10.8** | 0.00 | 0.00 |

An update that changes nothing is no longer written at all. Nothing observable is skipped: `changes()`
still counts the row, both trigger times still fire, `RETURNING` still returns, and the index loop
already skipped an entry that had not moved.

And because making that case cheap would otherwise have turned `txn.large` into a workload that
measures an operation doing no work — a test that cannot fail — **the workload was corrected too**.
`txn.batched` and `txn.large` now reset `side_table.note` first, outside the timed region on both
arms, where `sqlite_bench.c` already runs a workload's setup. `txn.large` reads **0.09x** on the old
workload with the old engine, **0.59x** on the old workload with this one, and **3.63x** once the
workload asks a real question. SQLite's own arm goes from 834 microseconds to 9.65 milliseconds when
it has to perform the same 2,000 updates.

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

**The same binary measured 53% faster on Linux, where Windows measured 279% at the time.** That
difference was settled by experiment rather than argued about, and the finding is that it is not a
Linux problem. The Linux arm has not been re-measured since; the Windows headline has moved to 326%
in the meantime, so treat the pair as the finding it was rather than as a comparison with the number
at the top of this page.

With a size classed free list in place of the system allocator, a `SELECT 1` compile goes from
46.95 ms to 38.97 ms on Windows (17% faster) and from 39.91 ms to 38.20 ms on Linux (4% faster) —
and **the two platforms then run the same speed**, 38.97 against 38.20. On the Windows compile the C
runtime's heap is 59% of the time.

SQLite does per statement work with the operating system that Windows charges heavily for and Linux
barely does. So SQLite's arm — the denominator of every ratio on this page — moves across platforms
while this engine's does not. The absolute work is the same on both, and lowering it is what the
missed bars need. Neither the allocator change that took Windows from 3.24x to 3.86x nor anything
since has been measured on Linux.

## What is not measured here

- **One scale.** These are the medium fixture, 100,000 rows. At 5,000 rows the headline is 3.46x, and
  at 600,000 it is 5.13x. Families behave differently at each, and `write` inverts: it is **45% slower
  than SQLite** at 5,000 rows (0.69x), 92% faster at 100,000 and **545% faster** at 600,000 (6.45x),
  because a bigger table spreads what a statement costs to set itself up over more of a page.
  `extension` moves the other way: 1.58x at 5,000 rows and 1.51x at 600,000, against 1.30x at 100,000.
  Both of those clear the 1.50x bar's central value, but the gate grades a family on its 95% lower
  bound, and that is 1.33x at both ends — so `extension` reads MISSED at all three scales. The reason
  it is better at the ends than in the middle is the same one: FTS5's build is four ordinary row
  writes per document, and a row write is where the per-statement cost lands.
- **One machine.** Windows 11 on x64. The disk matters more than it looks: part way through a four
  run sequence, `txn.batched` — 200 commits and 200 `fsync`s — goes from 309 ms to 895 ms **on
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
