# Closed items

This page lists the items that came off [the roadmap](roadmap.md) and the measurement that closed
each one. It also lists what is settled and will not be pursued, with the reason. A reader who
remembers a roadmap item can find what happened to it here.

The page is history. Each entry says what the item was, what was done, and the number that closed
it, with its date. The current numbers are in [Performance](performance.md) and
[Retrieval quality](retrieval-quality.md).

## Terms used on this page

| Term | Meaning |
|---|---|
| [Page](glossary.md) | The unit the database file is read and written in. The gates run at a 32 KiB page |
| [Buffer pool](glossary.md) | The pages the engine holds in memory. One slot in it is a **frame** |
| [Write ahead log](glossary.md) | The file every change is written to before it reaches the database file. This page calls it "the log" |
| [Checkpoint](glossary.md) | Copying the changes in the log into the database file |
| [Journal](glossary.md) | A file that holds a copy of each page from before a checkpoint changes it, so a crash can put the old page back |
| [Redo](glossary.md) | Applying log records to pages when a file is opened after a crash |
| [LSN](glossary.md) | The position of a record in the log. Every page stores the LSN of the last record that changed it |
| [Delta area](glossary.md) | A region at the end of a B-tree leaf where new rows go first. A **compaction** sorts them into the leaf |
| Crash campaign | A test that stops the engine at every write and sync of a workload (each one a **cut point**), reopens the file, and checks what survived |
| Gate | A program that runs the same workloads on inillucent and on SQLite 3.53.4 and reports the speed ratio. A ratio above 1.00x means inillucent is faster |
| Family | A group of gate workloads with one weight and one bar in `compat/perf/contract.toml` |
| [Generation](glossary.md) | One published version of a built search index |

## Summary

| Item | How it closed | The number that closed it | Date |
|---|---|---|---|
| [A macOS archive](#a-macos-archive) | built, signed and notarised on the Windows machine | every release since 0.1.7 ships macOS binaries | 2026-09-19 |
| [Memory](#memory) | settled at its current level | 40.76 MiB against SQLite's 37.22, 9.5% more | 2026-09-23 |
| [`write.insert.batch`](#writeinsertbatch-is-faster-than-sqlite) | a new delta area and a compaction splice | 1.47x, 7.0 µs a row against 10.2 | 2026-09-23 |
| [Four performance designs](#what-the-performance-designs-closed) | built and measured | weighted 3.55x before, 4.53x after | 2026-09-20 |
| [Two family bars](#two-family-bars-the-workloads-cannot-reach) | settled: a decision about the contract | `open.prepare` needs `SELECT 1` in 83 ns | 2026-09-23 |
| [The operator chain](#the-operator-chain-is-reused-between-executions) | a compiled chain reused between executions | `SELECT 1` 63% cheaper | during the engine rework |
| [Linux](#linux) | settled by experiment | the same speed on both systems, 38.97 ms and 38.20 ms | before 2026-09-20 |
| [Threads](#threads) | `SharedDatabase`: many threads, one statement at a time | 8 threads, 8,000 rows, none lost | 2026-09-15 |
| [`extension.fts.build`](#extensionftsbuild) | a segment format built and reverted | now 0.95x | 2026-09-23 |
| [The old engine](#the-old-engine-is-deleted) | deleted | 30,697 lines removed | during the engine rework |
| [Search index publishing](#a-generation-is-one-blob) | segmented generations | a commit's cost no longer grows with the table | during the engine rework |
| [Seven correctness items](#eight-items-closed-together) | fixed, with crash campaigns | zero damage at every cut point | during the engine rework |
| [Torn page during recovery](#recovery-reads-a-page-before-redo-has-had-a-chance-to-rewrite-it) | fixed in `cdc58eb` | 199 of 199 rows answered | 2026-09-15 |

## A macOS archive

**What it was.** Roadmap item 4. Each platform's archive was built on that platform, and there was no
Mac.

**What was done.** Every target is built on the Windows machine:

- zig links the Mach-O binaries.
- `rcodesign` signs them and replaces `lipo`, `codesign`, `productsign`, `notarytool` and `stapler`.
- Apple's notary service is an HTTPS API, so notarising needs no Mac.

**Result.** 0.1.3 was the first release with macOS binaries. 0.1.7 (2026-09-19) was the first to
publish all of them, and every release since does the same:

| What | Name |
|---|---|
| installer, signed with a Developer ID, notarised, universal for Apple silicon and Intel | `inillucent-<version>.pkg` |
| archive with the same binaries | `inillucent-<version>-universal-apple-darwin.tar.gz` |
| npm packages | `@blackrainbowlabs/cli-darwin-arm64`, `@blackrainbowlabs/cli-darwin-x64` |
| PyPI wheel | `inillucent-<version>-py3-none-macosx_13_0_universal2.whl` |
| Homebrew formula | the tap `black-rainbow-labs/inillucent` |

`tasks/task-1995-macos-releases-without-a-mac-tdd.md` records the three things Apple refused in the
first `.pkg`. `AGENTS.md` says how a release is run.

## Memory

**Result on 2026-09-23.** Peak resident memory for one round of the gate plan was **40.76 MiB against
SQLite's 37.22, which is 9.5% more**. Both ran on the same 128 MiB buffer pool budget. In the same
run inillucent was 397% faster and used 50% less processor time.

**How it came down.** The gap was 102%, then 43%, then 14%, then 9.5%.

| Step | What changed |
|---|---|
| 102% to 43% | the redo buffer bounded to 512 KiB; the index build stopped holding three copies of the tree; a version log that nothing collected is now collected; the allocator's free list is capped in bytes as well as in blocks |
| 43% to 14% | the database file became smaller |
| 14% to 9.5% | a bulk index build writes each page to the file directly, and no longer through a buffer pool frame |

The last step changed the plan's peak from 42.45 MiB to 40.76. `schema.index` sets the peak, and it
now raises it by 10.53 MiB where it raised it by 12.50.

**Why it stays.** The remaining 3.54 MiB is a buffer pool holding a file within 4% of SQLite's size,
the fixed cost of a process, and one `CREATE INDEX`. The allocator is not part of it. A 130 KB Rust
program that reads its own working set and exits peaks at **3.62 MiB with `inillucent-alloc`
installed and 3.62 MiB without it**, 0.66 MiB private either way. `inillucent-alloc` makes no initial
reservation: its free lists start empty. [Where the memory goes](performance.md#memory) attributes
every megabyte.

Closed by decision. The memory figure stays where it is.

## `write.insert.batch` is faster than SQLite

**What it was.** Roadmap item 2, "`write.insert.batch` is about 67% slower than SQLite". The workload
inserts 2,000 rows in one transaction into a table with two secondary indexes. It read about 0.60x.

**Result on 2026-09-23.** **1.47x, 7.0 µs a row against SQLite's 10.2.** The `write` family went from
2.12x on 2026-09-20 to 3.04x. [Performance](performance.md#what-moved-since-2026-09-20) has the run.

**What was done.** Two changes to the leaf page:

1. **The delta area has a directory in key order and no count limit.** It used to compact every 32
   rows. With a directory, a lookup in the delta area is a binary search, so the delta area can use
   all of the page's free space. At 10 indexes, compactions fell from 1,624 to 423.
2. **A compaction splices its delta rows into the packed page when they fit the page's existing
   column widths.** It no longer reads, sizes and writes every kept row again. At 10 indexes, 391 of
   the 423 compactions were splices.

Both change the page format, so the file format is 2. The current build reads format 1.
[What a file's format version promises](relational-architecture.md#6-what-a-files-format-version-promises)
explains how, and what earlier releases do with a format 2 file.

### The index count sweep

`inillucent-writeprofile --sweep` inserts 5,000 rows in one transaction into a 100,000 row table
with 0, 2, 5 and 10 indexes. Each change was measured separately in one quiet window on 2026-09-22.
Each figure is the fastest of five interleaved rounds, in microseconds a row. The rows are
`before`, `directory` and `directory-and-splice` in `tests/performance-history.tsv`.

| indexes | before | with the directory | with the directory and the splice |
|---:|---:|---:|---:|
| 0 | 7.93 | 5.04 | 5.14 |
| 2 | 18.28 | 10.47 | **9.12** |
| 5 | 33.45 | 17.16 | **15.50** |
| 10 | 73.06 | 39.97 | **37.20** |
| cost per index, at 2 | 5.17 | 2.71 | **1.99** |
| compactions, at 10 indexes | 1,624 | 423 | 423, 391 of them spliced |

`inillucent-perfhistory --only insert.indexes` asks SQLite the same of a 20,000 row table. After
subtracting process startup, the ratio against SQLite at 2 indexes went from 0.08x to 0.19x. At 10
indexes it went from 0.43x to **1.28x**.

### Measurements taken while the item was open

These were taken before the two changes, at a 32 KiB page, on the medium fixture. They show where
the time went.

`inillucent-writelogattrib`, 2,000 inserts with both indexes and then with both dropped:

| | with both indexes | without either | the two indexes |
|---|---:|---:|---:|
| wall time | 50.43 ms | 15.47 ms | **34.96 ms, 69%** |
| applying changes to pages | 46.37 ms | 11.77 ms | 34.60 ms |
| log written | 1,563.9 KiB | 1,163.4 KiB | 400.5 KiB |
| leaf compactions | 181 | 58 | 123 |
| splits | 9 | 8 | 1 |

| log record | records | bytes | share of the log |
|---|---:|---:|---:|
| `Structural` (a split) | 9 | 864.7 KiB | **55%** |
| `InsertRow` | 6,000 | 687.5 KiB | 44% |
| `CompactLeaf` | 181 | 11.3 KiB | 0.7% |
| `AllocPage` and the commit | 10 | 0.4 KiB | 0.03% |

A split wrote 98,384 bytes of log at this page size: three whole pages. A smaller split record would
remove 55% of the log's bytes and about 2% of the time, because the log is written and synced once
at the commit. The time was index maintenance: 8.7 µs for each of the 4,000 index row insertions,
against 2.4 µs for each of the 2,000 table rows.

`WriteStats::room_nanos` times `make_room`, which compacts or splits a leaf. Medians of five runs:

| | with both indexes | without either |
|---|---:|---:|
| wall time | 29.60 ms | 14.30 ms |
| **making room** | **10.57 ms** | 4.12 ms |
| building the new page image | 7.77 ms | 1.59 ms |
| of which: choosing the live rows | 1.98 ms | 0.50 ms |
| of which: reading the live rows | 1.90 ms | 0.33 ms |
| of which: sizing the page | 1.16 ms | 0.12 ms |
| of which: encoding the page | 2.22 ms | 0.35 ms |

The sizing step was 4.00 ms before `fit_all_widths` replaced a row by row check with one pass. That
took the transaction from 33.91 ms to 29.60 ms. The test `fit_all_widths_agrees_with_fit_widths`
checks that both produce the same page bytes. At the gate, alternating four runs between that build
and a control, inillucent's own time went from 37.65 ms to 33.04 ms as medians, 12.2% faster.

An earlier guess blamed the walk of each leaf's unsorted delta area. It measured **8,329 calls,
119,645 entries walked, 5.1 ms** against 66.8 ms of apply time at an 8 KiB page: under 8%.

An earlier figure of 0.72x came from a gate that did not run a workload's `pre` statement, while
`sqlite_bench.c` did. Commit `aa140c7` fixed the gate.

## What the performance designs closed

Four of the ten designs in `tasks/task-2000-inillucent-performance-tdd.md` are built and measured.
The before and after gates were taken back to back on one machine on 2026-09-20. The same SQLite
binary reads 2.16x faster on a quiet machine than on a busy one, so a stored baseline is not a fair
comparison.

| | before | after |
|---|---:|---:|
| weighted speed | 3.55x | **4.53x** |
| processor time, as a share of SQLite's | 0.635 | **0.400** |

| Design | What changed | Result |
|---|---|---|
| 1. A commit is one log append and one sync | A commit used to run a checkpoint with a journal, at six to eight `fsync` calls a statement. The checkpoint now waits until the log passes 4 MiB, a caller asks, or the connection closes. It writes the new image of each page to the log before it overwrites the page, so it needs no journal | `txn.autocommit`'s 100 statements make 100 log writes, 100 log syncs, no data file syncs and no checkpoints. They used to make 202 syncs and write 3,252 KiB of log for 50 KiB of rows. `txn.autocommit` 0.13x to 0.94x, `write.insert.autocommit` 0.47x to 3.04x, the `transaction` family 1.25x to 2.36x, `write` 1.46x to 2.12x |
| 2. A bulk index build writes each page once | The build writes pages to the file directly | `schema.index` 0.66x to 1.37x. Peak memory 42.45 MiB to 40.76. The crash campaign cuts 1,200 points of a `CREATE INDEX`, including a power loss just after the commit |
| 4. `count(*)` is one addition a batch | The aggregate used to be called once a row | `scan.aggregate` 11.41x to **52.16x**, `scan.group` 7.89x to **27.51x**, `read.analytical` 5.29x to 10.48x, its lower bound 4.67x to 8.14x, over its 5.00x bar |
| 9. The retrieval index builds on every core | `HnswParams::build_threads` defaults to every core. The two halves of a hybrid search run in parallel. `distance::dot` uses an AVX2 and FMA kernel with eight 256 bit accumulators | Index build 129.7 s to **16.8 s** for 185,078 chunks at 768 dimensions. Vector search p50 0.934 ms to **0.8462 ms** |

A parallel build produces a different graph from a serial one. The condition for design 9 was that
the score card's ranking verdicts stay the same. They did: **15 better, 1 equivalent, 1
inconclusive, 0 worse**, with every correctness gate passing. So the parallel build is the default
everywhere.

The AVX2 kernel adds numbers in a different order from the scalar kernel, so the answers are not bit
identical. Over ten thousand random normalised pairs at 768 dimensions, the largest difference was
**5.4e-8**, under half a unit in the last place of an `f32` near 1.0.

## Two family bars the workloads cannot reach

`compat/perf/contract.toml` sets a bar for each family. The bars were written before any measurement.
They are targets and fail nothing. Two of them cannot be reached by work on the engine, so they are
not roadmap items. Moving them is a decision about the contract. Numbers from 2026-09-23:

| Family | Bar | Result | Why the bar is out of reach |
|---|---|---|---|
| `open.prepare` | 5.00x | 1.69x | The family is `prepare.point` at 5.10x and `prepare.trivial` at 0.57x. For the family to reach 5.00x, `prepare.trivial` would have to reach 4.90x. SQLite compiles, binds, steps and resets `SELECT 1` in 407 ns, so the bar asks for 83 ns |
| `schema` | 3.00x | 1.31x | One `CREATE INDEX`: 26.47 ms against SQLite's 34.63. Its stages are `scan 3.6 ms, sort 4.8, pack 15.1 to 16.3, catalog 0.2, seal 1.1`. If writing the index pages cost nothing, the workload would take about 10.5 ms, about 3.3x. So the bar needs the index written for almost nothing |

`prepare.trivial` makes 13 allocations, down from 24, measured by `inillucent-prepareprofile`. Nine of
the 13 are part of the compiled statement that the call returns.

## The operator chain is reused between executions

**What it was.** Every execution of a prepared statement rebuilt its chain of operators.

**What was done.** Built during the engine rework:

- A `Compiled` owns the part of the chain that borrows nothing, and takes the tree borrows again
  inside `run`.
- `Cached::Select` holds a slot that is `Untried`, `Reusable` or `Never`. An execution that starts
  while the same statement is already running builds a fresh chain.
- The index nested loop join (`JoinRecipe` in `crates/inillucent-exec/src/compiled.rs`) and the write
  statements `Cached::Insert`, `Update` and `Delete` (`crates/inillucent-engine/src/plans.rs`) use
  the same slot.

**Result.** 40 rounds of 200 executions per arm, with the order of the arms swapped every round:

| statement | rebuilt | reused | saved | rounds where reuse was faster |
|---|---:|---:|---:|---:|
| `SELECT 1` | 3,486 ns | 1,280 ns | 63% | 40 of 40 |
| a point lookup by rowid | 20,149 ns | 12,471 ns | 39% | 35 of 40 |
| a 200 row range scan | 1,191,358 ns | 1,221,768 ns | **none** | **17 of 40** |
| a covering range scan | 33,772 ns | 27,379 ns | 18% | 37 of 40 |
| a point join | 20,204 ns | 11,564 ns | 41% | 38 of 40 |

The 200 row range scan spends its time on each of its 200 probes, so a saving per statement does not
change it. That cost is
[roadmap item 1](roadmap.md#1-the-extension-and-join-families-either-side-of-their-bars).

The work found four defects in `build_statement`:

| Defect | Status |
|---|---|
| `Statement::run` never ran `subquery::fold` again, so a second execution of a statement whose key reads a subquery was refused | fixed |
| every execution formatted an `EXPLAIN` string and discarded it | fixed |
| `Correlated` and `LateralModule` copy the parameters when the chain is built | such chains are not reused |
| `build_materialised_join` stores the inner rows when the chain is built | such chains are not reused |

## Linux

**What it was.** The same binary measured 53% faster than SQLite on Linux, while Windows measured 279%
faster at the time.

**What was found.** With a free list sorted by size class in place of the system allocator, a
`SELECT 1` compile went from 46.95 ms to 38.97 ms on Windows and from 39.91 ms to 38.20 ms on Linux.
The two systems then ran at the same speed. SQLite's time is what changes between the two systems:
Windows charges more than Linux for the calls SQLite makes to the operating system on each statement.

**Status.** Settled. Nothing since the allocator change has been measured on Linux, so the Linux
figure is older than the Windows figure of 397%. A new measurement needs a Linux machine that is not
also running the Windows arm. [Linux](performance.md#linux) has the details.

## Threads

**What it was.** Roadmap item 3. Several processes could share one database file, and several
threads in one process could not share one database.

**What was done.** `SharedDatabase` in `drivers/inillucent-driver/src/shared.rs` lets any number of
threads in one process use one database. Exactly one statement runs at a time, and a transaction
keeps its turn until it ends. SQLite calls this serialized mode. A web server can give one
`SharedDatabase` to a pool of workers, and each worker clones it.

The database is opened on a thread of its own and never leaves that thread. The other threads send
it statements over a channel. So `inillucent-driver` needs no `unsafe` code and keeps
`#![forbid(unsafe_code)]`. The cost is one thread per shared database and one channel round trip per
statement.

**Tests.** `drivers/inillucent-driver/tests/threads.rs` checks four things:

| Test | What it checks |
|---|---|
| `eight_threads_inserting_a_thousand_rows_each_land_eight_thousand` | 8 threads insert 1,000 rows each, and the table holds 8,000 rows with no key repeated |
| `a_reader_sees_the_state_before_a_transaction_or_the_state_after_it` | a reader during a 1,000 row transaction sees 0 rows or 1,000, never a number between |
| `a_database_is_used_and_dropped_on_another_thread` | dropping the database on another thread releases the file, and the file reopens |
| `a_dropped_transaction_rolls_back_and_gives_the_turn_up` | a transaction dropped without a commit rolls back and lets the next thread run |

Several processes use the same locking protocol as SQLite (SHARED, RESERVED, PENDING, EXCLUSIVE)
under `PRAGMA locking_mode = normal`, the default. One writer holds the file at a time. A second
writer waits for `PRAGMA busy_timeout` and then fails with `busy`.
`crates/inillucent-compat/tests/durability/process_concurrency.rs` starts two real writer processes and checks
that the rows in the file equal the commits the engine acknowledged. Before that test existed, two
real processes lost 43% of their acknowledged commits on every round.

**Not planned.** Statements do not run in parallel. A parallel executor is not on the roadmap.

## `extension.fts.build`

**What it was.** The slowest workload in the `extension` family: building an FTS5 index over 500
documents.

**Status on 2026-09-23.** **0.95x, 10.9 µs a document against SQLite's 10.4.** It was 0.69x on
2026-09-20. It improved with the write path change that closed
[`write.insert.batch`](#writeinsertbatch-is-faster-than-sqlite).

Two changes were built for it earlier, when it read 0.60x (10.92 ms against 6.23 ms, 30 rounds on a
quiet machine).

**One row a document, kept.** FTS5 used to write four rows a document. The engine rework merged the
last two: `%_idx` holds the document list inline, where it used to hold the number of a `%_data` row.
Alternating the change in and out with a release build each time, the ratio read 0.50x and 0.56x
before and 0.55x and 0.53x after. The workload pays for bytes written, and the row count does not
change the bytes. The change is kept because it is simpler. Files in the old layout still read, and
`crates/inillucent-compat/tests/engine/fts5_legacy_layout.rs` checks that they give the same answers.

**A segment format, built and reverted.** Each flush wrote a new segment, with a manifest at
`%_data` row `-1` and an automatic merge. It made `fts.build` no faster and roughly halved
`fts.query`:

| | `fts.query` | `extension` family, 95% lower bound |
|---|---:|---:|
| before the segment format | **1.32x** | not measured |
| with it | 0.45x | **0.93x**, under the 1.00x floor |
| after a prefix seek per segment | 0.60x | 1.08x |
| after skipping a merge when there is one segment | 0.65x | 1.13x |
| after a bit in the manifest that says whether there are deletions | 0.57x to 0.71x | 1.15x |
| **reverted** | **1.43x** | **1.40x** |

The remaining cost was reading the manifest from disk on every query. A module had no way to learn
that another connection had committed, so the manifest could not be cached. That hook now exists:
`VirtualTable::committed_elsewhere` and `schema_changed`. Trying the format again with a cached
manifest is part of
[roadmap item 1](roadmap.md#1-the-extension-and-join-families-either-side-of-their-bars).

## The old engine is deleted

**What was removed.** Four crates, 30,697 lines:

| Crate | Lines | What it was |
|---|---:|---|
| `inillucent-vm` | 16,356 | the engine that first reached SQLite file format parity |
| `inillucent-session` | 8,331 | its connection |
| `inillucent-capi` | 5,350 | its `sqlite3_*` C ABI |
| `inillucent-legacy` | 660 | its facade |

That engine measured between 30% and 95% slower than SQLite across the families, which is why the
engine was rebuilt. `drivers/inillucent-driver-capi` had already replaced the C ABI.

**What changed in the tests.** 36 files in `inillucent-compat` named one of the four crates. All were
rewritten before the crates were removed:

- Tests that compared the old engine with the new one now compare the new engine with the pinned
  SQLite, or check the new engine's answer alone.
- `tests/capi.rs` was deleted. It tested an ABI against the official `sqlite3.h`, and the shipping
  driver does not implement that ABI.
- Three capability rows in `compat/sqlite-3.53.4.toml` (`vm.bytecode.verifier`,
  `vm.statement.interrupt`, `txn.hooks`) moved to `status = "missing"`.

**What stays.** `inillucent-storage` and `inillucent-transaction`, 18,764 lines. `inillucent-engine`
and `inillucent-migrate` use `inillucent-sqlite-reader` to read SQLite files for migration, and
`inillucent-sqlite-reader` depends on both crates. The test
`no_new_crate_reaches_into_the_retired_engine` in `policy.rs` stops any other crate depending on
them. [Dependency policy](dependency-policy.md) lists the dependencies.

## A generation is one blob

**What it was.** Publishing a vector index wrote the whole index as one serialised generation, so
the cost of a commit grew with the table.

**What was done.** A generation is now many small segments that never change after they are
written. A search merges them when it reads, the way an LSM tree works. The graph work and the bytes
written at a commit grow with the batch, and do not grow with the table. The code is
`flush` and `merge_cascade` in `crates/inillucent-search/src/module.rs`, `merge.rs`, and
`SegmentMeta` in `store.rs`.

The default delta log became a constant 1,024 entries at the same time. It had been
`max(1024, rows / 8)`.
[Keeping a vector index current](relational-architecture.md#10-keeping-a-vector-index-current) has
the measured range and how to choose `compact = N`.

<a id="eight-items-closed-together"></a>

## Seven items closed during the engine rework

### A vector index returned zero rows

A vector index answered with no rows when its table held rows. There were two faults, and both
wrote index entries into a transaction that never committed:

- `create_vector_index` returned without calling `seal()`.
- `ImportedDatabase::write` advanced `next_txn` early, so `current_txn()` returned the next number
  for the rest of the statement.

A third change made the index fast: the backfill and the write path now flush the module, so a
query reads a published generation. Before, every query replayed the whole delta log, and took
2.34 s against 0.66 s for an exhaustive scan. All ten questions in `examples/rag-agent/cli-example` are answered
through an index, and `scripts/verify-indexed.sh` checks that.

### `embed(TEXT)` ran once a row when its argument was a constant

A deterministic function whose arguments do not change within a statement is now evaluated once for
the statement. When every argument is a literal it is evaluated when the statement is compiled. When
an argument is a bound parameter it is evaluated when execution starts, because a compiled
statement can be bound again with other values. The documented query over the 2,661 passages in
`examples/rag-agent/cli-example` went from **105.7 s to 1.50 s**.

### A registered function was refused when writing

`INSERT ... VALUES`, `UPDATE ... SET` and `RETURNING` can now call a registered function. Before,
only `INSERT ... SELECT` could.

### A second distance metric on the vector index

`WITH (metric = 'l2')` is used by the graph. The metric decides the distance and whether vectors are
normalised. A stored generation records its metric and refuses a query with a different one. The
planner uses the index only when the `ORDER BY` function matches the metric, and scans otherwise. A
generation with no stored metric reads as cosine, so existing files are unaffected. The work found
that `streaming_search` ranked by cosine whatever the metric was, and fixed it.

### An uncommitted row could survive a crash

The engine must never write a page changed by an open transaction to the database file. This is the
**no steal** rule. `Pool::writeback` follows the rule only when `Pool::holds_uncommitted` says a page
is uncommitted. `Pool::holds_uncommitted` read a watermark that only `inillucent-txn`'s `Engine` set,
and that engine does not ship. So in the shipping engine, `Pool::holds_uncommitted` returned `false`
for every page.

Two ordinary cases reached the defect:

- `BEGIN; INSERT ...; PRAGMA wal_checkpoint;` followed by a power loss. The checkpoint recorded a
  point above the open transaction's records and retired the log segments that held them.
- A transaction whose changed pages outgrow the buffer pool (4,096 frames of 32 KiB). The buffer
  pool wrote an uncommitted page, and redo then skipped committed records at or below that page's
  LSN.

**Fix.** The watermark is set. A checkpoint records `durable.min(uncommitted_lsn)` in place of
`durable`. `PRAGMA wal_checkpoint` inside a transaction that has written is refused, as the pinned
SQLite 3.53.4 refuses it. A `BEGIN` that has written nothing still checkpoints.

The two new crash campaigns run with `PRAGMA journal_mode = off`. With a journal, the journal put
the uncommitted page back, and the tests passed whether the fix was in place or not.

### Four ways a checkpoint lost an intact database

The crash campaigns in `crates/inillucent-compat/tests/durability/durability.rs` were moved to the shipping
engine. The two modes they had never run against, `PRAGMA journal_mode = truncate` and `persist`,
failed at the 35th cut point. Four defects were found and fixed:

| Defect | Fix |
|---|---|
| The journal was synced before any page copy was written to it, so the copies could still be in memory when the checkpoint overwrote their pages | `flush` makes two passes: write every page copy, sync once, then write the pages |
| The journal had no checksums, so recovery wrote torn bytes over a good database | Every record has a CRC over the transaction's nonce, the page number and the image. The header has its own CRC. `replay_hot_journal` stops at the first record that fails. The journal's magic is `RDBJRNL2`, and a journal from an older build is removed without being replayed |
| The two meta pages were written without a copy in the journal. After a crash the data pages were rolled back, but the meta page still named the new checkpoint, so redo skipped records it needed | A checkpoint journals both meta pages and syncs before writing them |
| In `wal` mode there was no journal. The log is logical, so once a checkpoint passes a page, the log no longer holds what built it. A page torn during a checkpoint could not be rebuilt | A connection in `wal` mode takes a `delete` journal for the length of each checkpoint. See `journal_for` in `crates/inillucent-engine/src/engine/locks.rs` |

The fourth defect showed as a checksum failure at cut 32 in both `wal_crash.rs` and
`search_crash.rs`:

```
seq=116 write /sim/wal.db offset=8192  len=4096     page 2
seq=117 write /sim/wal.db offset=12288 len=4096     page 3
        crash: neither synced; sectors 16-31 come back Torn, Garbage, Dropped
meta after the crash: gen=4 ckpt_lsn=56, the previous one, correctly not advanced
recovery: page 3 checksum fe9063aa is not the computed f53956bb
```

SQLite's log holds whole page images, so an interrupted checkpoint is copied again. inillucent's log
holds row changes, so it needs the journal. `PRAGMA journal_mode = off` still has no journal, which is
what `off` asks for.

**Why the defects were missed.** The journal holds page copies only while a checkpoint runs. The
commit campaigns crashed inside the commit, which only writes the log. `PRAGMA journal_mode = delete`
matches the default and does nothing, so the default mode was never crashed inside a checkpoint.
`truncate` and `persist` run two checkpoints when they are set, which is where the first three
defects were found. `search_crash.rs` had the same gap: its last statements were two
`SELECT count(*)` reads served from the buffer pool. It now ends with `PRAGMA wal_checkpoint`.

**Campaigns that crash inside a checkpoint.** The failure is armed after the transaction is
acknowledged, so the committed state is the only correct answer at every cut point. From the
schedules checked in on 2026-09-22:

| Schedule | Cut points | Outcome |
|---|---:|---|
| `tests/crash/delete-full-checkpoint-crash.txt` | 55 | every one recovered to the committed state |
| `tests/crash/delete-full-checkpoint-io-error.txt` | 55 | every one recovered to the committed state |
| `tests/crash/delete-full-checkpoint-disk-full.txt` | 55 | every one recovered to the committed state |
| `tests/crash/truncate-full-checkpoint-crash.txt` | 54 | every one recovered to the committed state |
| `tests/crash/persist-full-checkpoint-crash.txt` | 54 | every one recovered to the committed state |

The commit campaigns in the same schedules:

| Schedule | Cut points | Detected damage |
|---|---:|---:|
| `tests/crash/delete-full-crash.txt` | 94 | 0 |
| `tests/crash/truncate-full-crash.txt` | 166 | 0 |
| `tests/crash/persist-full-crash.txt` | 166 | 0 |
| `tests/crash/delete-full-short-write.txt` | 94 | 3 detected, 1 commit lost to a half written call |

A short write that lands on the journal now fails its record's checksum, so the replay stops before
it writes half a page back.

Two smaller fixes came from the same work:

- `Journal::finish` synced at `SyncMode::Normal` in `truncate` and `persist` mode, which may not reach
  the disk. It now syncs at `Full`, as `delete` mode already did.
- `Body::Pad`, the filler the log writes to align a synced write to a sector, was passed to redo and
  counted as an applied record. Redo now skips it.

### The seventeen failing tests

There were none. `schema_forms` (14), `planner` (5) and `ordering` (2) all pass with the pinned
`sqlite3` present. An earlier change had removed the cause:
`crates/inillucent-compat/src/interchange.rs` moves a database between the engines as `.dump`
output replayed by the reference shell. This page had not been updated.

## Recovery reads a page before redo has had a chance to rewrite it

**What it was.** Roadmap item 6. It was fixed in commit `cdc58eb` on 2026-09-15 and stayed on the list afterwards.

**Cause.** A row record in the log changes a page by reading it first: an `INSERT` reads the leaf,
adds the row and writes the leaf back. When a crash tore a page, redo failed at the first record
that read that page, even when a later record in the same log held the whole page.

The cause was found by naming every read in `open_file`. At cut 8 of the `journal_mode = off` sweep
in `crates/inillucent-compat/tests/durability/free_map_checkpoint_crash.rs`, the error reads
`replaying the log: page 4 checksum ... is not the computed ...`. The three reads before redo (the
first open, attaching the catalog, reading the catalog) are named in the error text, and none of
them was the cause. The names stay in the code.

**Fix.** `replay_with_repair` in `crates/inillucent-engine/src/recovery.rs`. When redo fails with a
corruption error, every record that holds a whole page image is applied: `WritePage`, a
`CompactLeaf` that holds one, and a split's three pages. Then redo runs again. Running redo twice is
safe because each applied record stamps its pages with its LSN, so the second pass skips it. The
read of the free map moved inside the retry, because a checkpoint rewrites that page every time.

The images are applied only after a failure. Applying them on every open changed which catalog redo
started from, and in the `wal_crash` commit campaign one cut of 23 then lost a committed
transaction.

**Test.** `crates/inillucent-compat/tests/engine/torn_page_with_image.rs` has two cases. Both choose the
page by reading the log, and both crash the fixture without closing it:

| Case | Result |
|---|---|
| a torn page that a later record holds whole | the database opens and answers all 199 rows |
| a torn page that no record holds whole | the open fails with `SQLITE_CORRUPT` and names the page |

With `journal_mode = off`, a page torn during a checkpoint cannot be recovered. Cuts 8 to 18 of that
sweep still fail to open, because the log holds no image of page 4. That is the documented behavior
of `off`, and the second case tests it.

## Where to go next

- [Roadmap](roadmap.md): what is still open.
- [Performance](performance.md): the current speed and memory figures.
- [Retrieval quality](retrieval-quality.md): the current search quality and latency figures.
