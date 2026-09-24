# Closed items

What came off [the roadmap](roadmap.md), with the measurement that closed each, and what is settled
and will not be pursued, with the reason. A reader who remembers a roadmap number can find what
happened to it here.

## A macOS archive

This was roadmap item 4, on the grounds that every platform's archive was built on that platform
and there was no Mac. The release is built on the Windows machine instead: zig cross links the
Mach-O, `rcodesign` signs it and replaces `lipo`, `codesign`, `productsign`, `notarytool` and
`stapler`, and Apple's notary is an HTTPS API. 0.1.3 was the first release with macOS binaries, and
0.1.7 publishes all of them:

- `inillucent-0.1.7.pkg`, signed with a Developer ID and notarised by Apple, universal for Apple
  silicon and Intel
- `inillucent-0.1.7-universal-apple-darwin.tar.gz`, the same binaries
- the npm packages `@blackrainbowlabs/cli-darwin-arm64` and `@blackrainbowlabs/cli-darwin-x64`
- the PyPI wheel `inillucent-0.1.7-py3-none-macosx_13_0_universal2.whl`
- the Homebrew formula in `black-rainbow-labs/inillucent`

`tasks/task-1995-macos-releases-without-a-mac-tdd.md` records the three things Apple refused in the
first `.pkg`, and `AGENTS.md` how the release is run.

## Memory

**40.76 MiB against SQLite's 37.22, which is 9.5% more**, on the same 128 MiB budget, while running
397% faster and spending 50% less processor (2026-09-23). It came down three times, from 102% more, then 43%, then
14%. The last step was the second design in the performance TDD: a bulk index build used to write each page into a buffer
pool frame that then had to be written out and evicted, and it writes into the file directly now, so
`schema.index` - which is what sets this plan's high water mark - raises it by 10.73 MiB rather than
12.50.

The remaining 3.7 MiB is a page pool holding a file that is within 4% of SQLite's, a process floor, and
one `CREATE INDEX`. **The allocator is not part of it, and that is measured rather than assumed**: a
130 KB Rust program whose `main` reads its own working set and returns peaks at **3.62 MiB with
`inillucent-alloc` installed and 3.62 MiB without it**, 0.66 MiB private either way. It has no initial
reservation to size down - its free lists start empty and a full class hands its block back to the
system allocator - so the question the tenth design in the performance TDD asked, whether two to
three of these mebibytes were the allocator's arena, is answered no. [Where the memory goes](performance.md#memory) attributes
every megabyte. Closed by decision: this is where it stays.

## `write.insert.batch` is faster than SQLite

**It reads 1.47x on 2026-09-23, 7.0 µs a row against SQLite's 10.2**, where this was roadmap item 2
under the title "`write.insert.batch` is about 67% slower than SQLite". A later change closed it: a leaf's
delta area keeps a directory in key order and is sized by the page's free space rather than capped at
32 rows, and a compaction whose rows fit the page's existing column widths splices them in. The `write`
family went from 2.12x on 2026-09-20 to 3.04x. [Performance](performance.md#what-moved-since-2026-09-20)
has the run. What follows is the item as it stood when it was open, kept for the measurements in it.
**About 0.60x**: 2,000 inserts in one transaction. It was 72% slower, then 43%, and it sits inside a
family that clears its bar, so it blocks nothing.

**That same change took the cost of an index from about 5.2 µs a row to about 2.0, on the index count
sweep.** The sweep is the measurement this item lacked: the gate's `main_table` has two secondary
indexes, so a change aimed at index maintenance measured there is one point of a curve.
`inillucent-writeprofile --sweep` inserts 5,000 rows in one transaction into a 100,000 row table
carrying 0, 2, 5 and 10 indexes, and `inillucent-perfhistory --only insert.indexes` asks SQLite the
same of a 20,000 row table. Two changes, measured separately in one quiet window, fastest of five
interleaved rounds, microseconds a row:

| indexes | before | the delta area sized by the free gap | and the compaction splice |
|---:|---:|---:|---:|
| 0 | 7.93 | 5.04 | 5.14 |
| 2 | 18.28 | 10.47 | **9.12** |
| 5 | 33.45 | 17.16 | **15.50** |
| 10 | 73.06 | 39.97 | **37.20** |
| cost per index at 2 | 5.17 | 2.71 | **1.99** |
| compactions at 10 indexes | 1,624 | 423 | 423, 391 of them spliced |

Against SQLite, net of process startup, the wall ratio at 2 indexes went from 0.08x to 0.19x and at
10 indexes from 0.43x to **1.28x** - the first arm of this workload this engine wins. Rows `before`,
`directory` and `directory-and-splice` in `tests/performance-history.tsv`.

- **The delta area has a directory in key order and no count limit.** It compacted every 32 rows
  whatever the leaf held, which the performance review had priced as "the two indexes are 69% of
  this workload". With a directory a lookup is a binary search, so the area can take the whole free
  gap: 1,624 compactions became 423. The audit predicted 18 to 20% of the workload; it was 43% at two
  indexes, because the compaction count fell by 3.7x rather than the 8x the audit assumed and every
  compaction became cheaper as well.
- **A compaction splices its delta rows into the packed page when the rows fit its widths**, instead
  of reading, pricing and writing every kept row again. It is 7% to 15% on top of the
  first change at two indexes and more, and nothing without an index: a table's own tree appends at
  its right edge and rarely compacts.

Both are page format changes, so the file format is 2. This build reads format 1;
`docs/relational-architecture.md` section 5a says how, and what the earlier releases answer for a
format 2 file.

**The 0.72x this item carried until now was measured by a gate that was not asking both arms the same
question.** `inillucent-writegate` never ran a workload's own `pre`, and `sqlite_bench.c` runs one
before it starts its clock - so on `txn.batched` and `txn.large`, which both carry
`UPDATE side_table SET note = 'note ' || id`, SQLite did work this engine skipped. That was fixed
in `aa140c7`, and every workload agrees again. Measured after that fix, four runs alternating between
this build and a control, at a 32 KiB page: `write.insert.batch` reads **0.56x and 0.63x**, and the
`write` family 1.67x and 1.80x against its 1.50x bar.

**This item has now named the wrong cause twice, and the second time the measurement says which
number was the misleading one.** The first text blamed `locate()`'s walk of each leaf's unsorted delta
area; that was counted and came to under eight per cent. The second blamed the split record's log
volume, which is real and is not what the workload waits for.

`inillucent-writelogattrib` on the medium fixture at the gate's own geometry, a 32 KiB page, 2,000
inserts into `main_table` with its two secondary indexes, and then the identical run with both indexes
dropped:

| | with both indexes | without either | the two indexes |
|---|---:|---:|---:|
| wall | 50.43 ms | 15.47 ms | **34.96 ms, 69%** |
| applying the changes to pages | 46.37 ms | 11.77 ms | 34.60 ms |
| log written | 1,563.9 KiB | 1,163.4 KiB | 400.5 KiB |
| leaf compactions | 181 | 58 | 123 |
| splits | 9 | 8 | 1 |

| where the log goes | records | bytes | share |
|---|---:|---:|---:|
| `Structural` (a split) | 9 | 864.7 KiB | **55%** |
| `InsertRow` | 6,000 | 687.5 KiB | 44% |
| `CompactLeaf` | 181 | 11.3 KiB | 0.7% |
| `AllocPage` and the commit | 10 | 0.4 KiB | 0.03% |

A split costs **98,384 bytes** at this page size - three whole pages for one row that would not fit -
so a logical split record would take 55% off the log's volume. **It would take about 2% off the
workload's time**, because the log is written once and synced once at the commit and the bytes are not
what the workload is waiting for. The time is the 34.96 ms of index maintenance: 8.7 µs for each of
the four thousand index row insertions, against 2.4 µs for each of the two thousand table rows.

**And the third measurement says which part of the index maintenance it is.** `WriteStats` gained
`room_nanos`, the time inside `make_room` - compacting a leaf, or splitting one - because the rows
above say how many there were and not what they took. It read 23.97 ms of a 44.17 ms transaction
then, 54% of it.

**Making room is now 10.57 ms of 29.60, and it has been split into the four passes it actually is.**
`LeafRef::live_source` is `live_order` and then `materialise` - deciding which rows
survive, then reading every one of them - and `compact_image` reports its sizing pass and its encode
apart. Medians of five runs, 32 KiB page, the same fixture:

| | with both indexes | without either |
|---|---:|---:|
| wall | 29.60 ms | 14.30 ms |
| **making room** | **10.57 ms** | 4.12 ms |
| building the image | 7.77 | 1.59 |
| - the merge, which rows are live | 1.98 | 0.50 |
| - reading every one of them | 1.90 | 0.33 |
| - the sizing pass | 1.16 | 0.12 |
| - the encode | 2.22 | 0.35 |

**Only one of those four does not grow with the page size, and it is the one a splice cannot
remove.** At an 8 KiB page the merge is 1.89 ms against 1.98 here - it is per delta row, and a delta
area holds at most thirty-two whatever the page holds - while reading the rows, sizing the page and
encoding it all roughly double, because a 32 KiB leaf keeps four times as many rows. An attribution
of this stage taken at 8 KiB therefore understates it by about half, and the gate runs at 32 KiB.

**2.84 ms of it came off by asking the sizing pass a simpler question.** `pack_all_rows` wants one
bit - do *all* the live rows fit one page - and `fit_widths` answered it by pricing the leaf a row at
a time, resolving the whole candidate layout and recomputing the page size on every row, because its
other caller stops at the first row that does not fit. `fit_all_widths` observes every column's shape
in one pass, resolves once and compares once: 4.00 ms to 1.16, and the transaction 33.91 to 29.60.
The two cannot disagree, because the price of a run of rows never falls as rows are added - so a leaf
that fits whole had every prefix of it fit, and the layout the incremental loop ends on is `resolve`
over the shapes of all the rows. `fit_all_widths_agrees_with_fit_widths` asserts the page bytes and
not only the verdict.

**What that is worth at the gate**, once the gate was fixed to measure again. Four runs at a 32 KiB
page, alternating between this build and a control with the sizing pass put back, so that drift in
the box shows up in both:

| | control | this build |
|---|---|---|
| `write.insert.batch`, this engine's arm | 38.34 ms, 36.95 ms | **33.14 ms, 32.93 ms** |
| the same workload's ratio | 0.46x, 0.58x | **0.56x, 0.63x** |
| the `write` family | 1.54x, 1.74x | **1.67x, 1.80x** |

**Read this engine's own arm rather than the ratio.** The box was not quiet - another ticket held
both GPUs and the local model server throughout - and it shows in the SQLite arm, which drifted from
18.49 ms to 21.33 ms across the four runs while this engine's arm varied by 3.8% in the control and
0.6% here. On its own arm the change is **12.2% faster**, 37.65 ms to 33.04 ms as medians, which is
the same figure `inillucent-writelogattrib` reports for the same workload off the gate.

**What is left for a splice is the encode, 2.22 ms of 29.60.** A compaction that spliced its delta
rows into the column-major image rather than re-encoding every kept row still has to decide which
rows survive, and still has to settle the slot widths: `compact_image` narrows a column when the
widest value in it was tombstoned, and `CompactLeaf` carries an empty image and a `from_lsn` so that
recovery re-derives those bytes rather than copying them. A splice that chose different widths would
produce a correct page that is not the same page, and nothing would say so, because the checksum is
computed over whatever was produced. Settling the widths means observing every value, and
`live_source`'s own measurement says reading values straight through the mini-columns instead of
materialising them once is *slower* - `txn.large` 4.1 ms to 5.7. So the splice's ceiling is 7% of the
transaction, before its own memcpys, offset rewrites and class-array shifts cost anything, against a
second row source on the hottest write path and its own crash campaign.

**And two things outside making room are now larger than that ceiling.** Timed with temporary
per-write timers, which cost about 27% of the wall themselves and so give shares rather than
absolutes, the apply time of the same transaction divides as: making room 38%, **locating the key
16%**, placing the row with its log and undo records 14%, **the room check 10%**, encoding the row
3%, the descent 2%. The room check is `LeafMut::room_for`, which reads - and it is reached through
`Pool::modify`, which takes the page mutably and marks the frame dirty, once per row written.

What this item carries is the number rather than a guess: the delta walk was under eight per cent,
the split record is 55% of the bytes and about 2% of the time, making room is 36% of the transaction,
and inside it the encode a splice would replace is 7%.

And the earlier delta walk measurement, kept because it is what closed the first guess: **8,329 calls,
119,645 entries walked, 5.1 ms**, 14.4 entries a call, against 66.8 ms of apply time at an 8 KiB page.
Under eight per cent, and that is the whole walk rather than what a fingerprint block would save - a
probe that matches still decodes, and the block itself costs a hash per insert and 64 bytes a leaf.
`crates/inillucent-compat/src/bin/writelogattrib.rs`'s own header already recorded that a previous fix
to that decode "did not move the gate ratio"; this is the number behind that sentence.

## What the performance designs closed

Four of its ten designs are built and measured; the pair of four-run gates that measures them was taken
back to back on one box, because the same pinned SQLite binary reads 2.16x faster on a quiet box than
on a busy one and a stored baseline is therefore not a comparison. **3.55x weighted before, 4.53x
after**, with processor time 0.635 of SQLite's before and 0.400 after.

**A commit is one log append and one sync of it** (design 1). It used to be a checkpoint: the log
folded into the file, and a rollback journal holding the pre-image of every page the fold was about to
overwrite, at six to eight `fsync` class calls a statement. The fold is deferred now - until the log
passes four mebibytes, until a caller asks, or until the connection closes - and it is made safe
without a rollback journal by appending the after image of every page it is about to write to the log
first. Measured on the gate's own new counters: `txn.autocommit`'s hundred statements make **100 log
writes, 100 log syncs, no data file syncs and no folds**, where they used to make 202 syncs and write
3,252 KiB of log for 50 KiB of rows. `txn.autocommit` went from 0.13x to 0.94x and
`write.insert.autocommit` from 0.47x to 3.04x; the `transaction` family from 1.25x to 2.36x and `write`
from 1.46x to 2.12x, both lower bounds now clear of the 1.50x bar.

**A bulk index build writes each page once** (design 2). `schema.index` went from 0.66x to 1.37x and
the plan's peak resident set from 42.45 MiB to 40.76. Its crash campaign cuts 1,200 points of a
`CREATE INDEX`, including the one cut where the statement commits and the power then goes: that
snapshot holds a database whose catalog has never been written in place, so the committed index exists
in the log and nowhere else, and recovery rebuilds it over pages that were synced before the commit.

**`count(*)` is one addition a batch** (design 4). The operators answered it by calling the accumulator
once a row with a `NULL` argument, so a hundred thousand row scan made a hundred thousand calls that
each compared a discriminant and added one. `scan.aggregate` went from 11.41x to **52.16x** and
`scan.group` from 7.89x to **27.51x**, which took `read.analytical` from 5.29x to 10.48x and its lower
bound from 4.67x to 8.14x, over the 5.00x bar it had been missing.

**The retrieval index builds on every core** (design 9). `HnswParams::build_threads` defaults to
`available_parallelism()`, the two legs of a hybrid search run under `rayon::join`, and
`distance::dot` dispatches once to an AVX2 and FMA kernel with eight 256-bit accumulators. The index
build went from 129.7 s to **16.8 s** for 185,078 chunks at 768 dimensions, and vector search p50 from
0.934 ms to **0.8462**. The acceptance condition was the score card's ranking verdicts, because a
parallel build's graph is not the serial one: they are byte for byte what they were, **15 better, 1
equivalent, 1 inconclusive, 0 worse, every correctness gate passing**, which is why the default is the
parallel build everywhere rather than only in the command line.

The wide kernel does not produce bit identical answers to the scalar one and cannot - a different
number of accumulators is a different summation order - and the measured worst disagreement over ten
thousand random L2 normalised pairs at 768 dimensions is **5.4e-8**, under half a unit in the last
place of an `f32` near 1.0.

## Two per family bars that arithmetic cannot reach

`open.prepare` is `prepare.point` at 4.96x and `prepare.trivial` at 0.51x; for the family to reach
its 5.00x bar, `prepare.trivial` would have to reach 5.05x, and SQLite compiles, binds, steps and
resets `SELECT 1` in 483 ns, so the bar asks for 96 ns. `schema` is one `CREATE INDEX`, 26.78 ms
against SQLite's 33.54, whose stages are `scan 3.7, sort 5.5, pack 11.4, catalog 0.3, seal 5.8`: a
packer costing nothing at all leaves 15.4 ms, which is 2.18x against a 3.00x bar. Both bars are in
`compat/perf/contract.toml`, written before any measurement. They are targets rather than
requirements and fail nothing. Whether to move them is a decision about the contract, not work on
the engine, so they are not roadmap items.

## The operator chain is rebuilt on every execution

Built as part of the engine rework. A `Compiled` with no lifetime owns the borrow free part of a statement's chain and
re-acquires the tree borrows inside `run`; `Cached::Select` carries a slot of `Untried | Reusable |
Never`, and a re-entrant execution falls back to a fresh build rather than refusing. The index
nested loop tower (`JoinRecipe` in `crates/inillucent-exec/src/compiled.rs`) and the same slot on
the write path's `Cached::Insert`, `Update` and `Delete` (`crates/inillucent-engine/src/plans.rs`)
landed in the same ticket. Paired measurement, 40 rounds of 200 executions per arm with the arm
order swapped every round:

| statement | rebuilt | reused | saved | rounds reused faster |
|---|---:|---:|---:|---:|
| `SELECT 1` | 3,486 ns | 1,280 ns | 63% | 40 of 40 |
| a point lookup by rowid | 20,149 ns | 12,471 ns | 39% | 35 of 40 |
| a 200-row range scan | 1,191,358 ns | 1,221,768 ns | **none** | **17 of 40** |
| a covering range scan | 33,772 ns | 27,379 ns | 18% | 37 of 40 |
| a point join | 20,204 ns | 11,564 ns | 41% | 38 of 40 |

The 200 row range scan does not move, and the sequential arms' 11 to 14% was noise: 17 of 40 rounds
went the other way. Its cost is per entry across 200 probes, which no per statement saving can
reach; that is [roadmap item 1](roadmap.md#1-the-extension-and-join-families-either-side-of-their-bars).
Building this surfaced four defects that were live in `build_statement` and invisible only because
nothing reused a chain: `Statement::run` never re-ran `subquery::fold`, so a second execution of a
statement whose source key reads a subquery refused; it formatted an `EXPLAIN` string and threw it
away every execution; `Correlated` and `LateralModule` copy the parameters at build time and nothing
counted that as a read; and `build_materialised_join` bakes its inner rows in at build time. The
first two are fixed; the last two are excluded from reuse by the verdict.

## Linux

**53% faster there, where Windows measured 279% at the time.** Settled by experiment: with a size
classed free list in place of the system allocator the two platforms run the same absolute speed,
38.97 ms against 38.20, and it is SQLite's own arm that moves across platforms rather than this
engine's. [Linux](performance.md#linux) has the measurement and the caveat: nothing since the
allocator change has been measured on Linux, so the Linux figure is older than the 397% Windows
headline. Re-measuring wants a Linux machine that is not also running the Windows arm; both inside
one box would measure the contention and not the platform.

## `extension.fts.build`

**0.60x against SQLite**: 10.92 ms against 6.23 ms, measured on a quiet box over 30 rounds. Two
changes were built for it and measured. One bought nothing and was kept; the other cost half of
query throughput and was reverted.

**The doubled write, kept.** FTS5 used to do four tree writes per document; the engine rework made the last
two one row, `%_idx` carrying the doclist inline rather than an integer naming the `%_data` row it
lived in. Measured as a genuine A/B with the change alternated in and out of the tree and a release
rebuild each time, the paired ratio read 0.50x/0.56x before and 0.55x/0.53x after: the row count was
never what this workload pays for, the bytes are. It is kept because it is simpler and because old
files still read; `crates/inillucent-compat/tests/fts5_legacy_layout.rs` manufactures an old layout
file and requires the same answers.

**The segment format, built and reverted.** A `segid` per flush, tombstones at a negative segid, a
manifest at `%_data` row `-1` whose absence identifies a pre-segment file, and an automerge fold. It
made `fts.build` no faster and roughly halved `fts.query`, taking the `extension` family under the
contract's 1.00x floor:

| | `fts.query` | `extension` family, 95% low |
|---|---:|---:|
| before the segment format | **1.32x** | not measured |
| with it | 0.45x | **0.93x**, under the floor |
| after a per-segment prefix seek | 0.60x | 1.08x |
| after skipping a needless merge on one segment | 0.65x | 1.13x |
| after a tombstone-presence bit in the manifest | 0.57x - 0.71x | 1.15x |
| **reverted** | **1.43x** | **1.40x** |

The remaining cost was the manifest re-read from disk on every query, which could not be cached
because a module had no hook that said "another connection may have committed since you last
looked". That hook exists now: `VirtualTable::committed_elsewhere` and `schema_changed`.
Re-attempting the format with a cached manifest is what
[roadmap item 1](roadmap.md#1-the-extension-and-join-families-either-side-of-their-bars) names for
the `extension` family.

## The old engine is deleted

`inillucent-vm` (16,356 lines), `inillucent-session` (8,331), `inillucent-capi` (5,350) and
`inillucent-legacy` (660) are gone from the workspace: 30,697 lines, the engine that reached SQLite
file format parity, its connection, its facade and its `sqlite3_*` C ABI. It was measured between
30% and 95% slower than SQLite across the families, which is why the rearchitecture happened, and
`drivers/inillucent-driver-capi` had already replaced what the C ABI was waiting on.

36 files in `inillucent-compat` named one of the four, and every one was rewritten before the crates
came out so every suite could be run against the replacement first. Differential tests that ran the
old engine beside the new one were re-pointed at the pinned SQLite oracle, or at the new engine
alone asserting the value the old one used to agree about; `tests/capi.rs` was deleted, because it
proved an ABI against the official `sqlite3.h` that the shipping driver does not implement and was
never trying to; and old VM bytecode cases with nothing to survive went with it. Three capability
rows in `compat/sqlite-3.53.4.toml` (`vm.bytecode.verifier`, `vm.statement.interrupt`, `txn.hooks`)
moved to `status = "missing"` as a result.

**`inillucent-storage` and `inillucent-transaction` stay.** `inillucent-engine` and
`inillucent-migrate` both depend on `inillucent-sqlite-reader`, which depends on both of them, because
reading a SQLite file in order to migrate away from it is what keeps 18,764 lines of the old engine
alive, and that is a feature rather than a leftover. `policy.rs`'s
`no_new_crate_reaches_into_the_retired_engine` ratchet watches only these two crates.
[Dependency policy](dependency-policy.md) has the edges and the ratchet.

## A generation is one blob

Adding content stopped rebuilding the graph when a commit became a **fold**: the
published generation is loaded and each entry of the delta log inserted into it, one graph insert
per row written rather than one per row in the table. What was still proportional to the corpus was
publishing, because a generation was one serialised index.

Segmented generations closed that: many small immutable segments merged at read time,
the way an LSM tree works, so both the graph work and the bytes written are proportional to the
batch rather than to the corpus. `crates/inillucent-search/src/module.rs` and `merge.rs` hold it
(`SegmentMeta`, `flush`, `merge_cascade`). The default delta log became a constant 1,024 entries at
the same time; it had been `max(1024, rows / 8)`.
[Keeping a vector index current](relational-architecture.md#10-keeping-a-vector-index-current) has
the measured range and how to choose `compact = N`.

## Eight items closed together

Eight items came off the roadmap during the engine rework. Each is named here so a reader who remembers the old
numbers can find what happened to them.

- **A vector index answering zero rows instead of the rows it holds.** This is the wrong answer this
  engine
  is built not to have. Two faults, both a write that is never committed: `create_vector_index`
  returned without the `seal()` every other directive ends with, and `ImportedDatabase::write` reads
  `next_txn` and moves it on at once, so `current_txn()` answered the *following* number for the rest
  of the statement and every index entry was logged into a transaction nothing commits. A third
  change makes the index fast as well as correct: the backfill and the write path now
  flush the module, so an index publishes a generation instead of replaying its whole delta log on
  every query: 2.34 s against 0.66 for the exhaustive scan, before. `examples/rag-agent`'s ten
  questions all answer through an index now, and `scripts/verify-indexed.sh` is what says so.
- **`embed(TEXT)` called once per row when it is a constant.** A deterministic scalar whose arguments
  do not vary within a statement is now evaluated once for the statement. It is folded at translation when
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
- **No-steal was not in force in the engine that ships, so an uncommitted row could survive a
  crash.** This is the one a reviewer found rather than a campaign, and it is the most serious thing
  the ticket touched. `Pool::writeback` declines to write a page belonging to an open transaction
  only when `Pool::holds_uncommitted` says so, and that reads a watermark nothing ever set: the only
  code in the workspace that called `Pool::uncommitted_handle` was `inillucent-txn`'s `Engine`, which
  is not the engine this project ships. So the watermark stayed at `u64::MAX`, `holds_uncommitted`
  answered `false` for every page, and the long comment beside it calling no-steal "a condition
  rather than a convention" described a crate that is not in the product.

  Two ordinary ways in. `BEGIN; INSERT ...; PRAGMA wal_checkpoint;` then a power loss: the pragma
  had no guard against running inside a transaction, and the checkpoint then recorded a point
  *above* the open transaction's own records and retired the segments holding them. And any
  transaction whose dirty pages outgrow the buffer pool, where the evictor writes an uncommitted
  page and the page-LSN rule then makes redo skip every committed record at or below its stamp.
  A bulk load larger than 4,096 frames of 32 KiB is an ordinary thing to do.

  The watermark is armed now, the checkpoint records `durable.min(uncommitted_lsn)` rather than
  `durable`, and `PRAGMA wal_checkpoint` inside a
  transaction that has written is refused, which is what the pinned SQLite 3.53.4 does, checked
  rather than assumed, and a bare `BEGIN` that has written nothing still checkpoints.

  **The two new campaigns run under `PRAGMA journal_mode = off`, and that is deliberate.** Under the
  default the rollback journal happened to put the uncommitted page back, so both tests passed
  whether or not no-steal was armed. A test that passes for a reason it is not about is a test that
  cannot fail, and `off` removes the safety net so the only thing left protecting the row is the
  mechanism the campaign is named after.

- **Four ways a checkpoint lost a database that the crash had left intact.** The crash
  campaigns in `crates/inillucent-compat/tests/durability.rs` were re-pointed onto the shipping
  engine during this ticket. The two they had never been run against, `PRAGMA journal_mode =
  truncate` and `= persist`, failed at the 35th cut point of the commit. All three defects are
  below, each with the thing that makes it a defect rather than a tuning choice. The campaigns now
  record 101 cut points each with **zero detected damage**, and `tests/crash/truncate-full-crash.txt`
  and `tests/crash/persist-full-crash.txt` are the schedules a review reads rather than takes on
  trust.

  **The journal was never synced before a page was overwritten.** `Pool::checkpoint` sealed the
  journal at its head, which is before `flush` has saved a single pre-image, because pre-images are
  saved by the writeback loop that runs next. So it synced an empty file, and every pre-image the
  checkpoint then wrote was still in the file's buffers while the same loop overwrote the pages those
  pre-images belonged to. That is the one ordering a rollback journal exists to forbid, and it is
  stated as the invariant at the top of `crates/inillucent-pool/src/journal.rs`. `flush` now takes two
  passes: save every pre-image, sync once, then write the pages. A batch of a thousand pages still
  pays for one sync.

  **The journal had no checksums, so recovery wrote torn bytes over a good database.** At the failing
  cut point the database file was intact. Page 3 stored the checksum `b59f5196` and computed
  `b59f5196`. The corruption the test reported was manufactured by recovery itself, out of a journal
  whose seventeen sectors the crash model had left Torn, Garbage and Dropped. Nothing in the file
  format could tell a replay that a pre-image was not the bytes that had been written. Every record
  now carries a CRC over the transaction's nonce, the
  page id and the image; the header carries one over itself; and `replay_hot_journal` stops at the
  first record that fails its check. Stopping there restores everything that is owed: a record can
  only be unverifiable if it was written after the last sync, and a page is only overwritten after
  the sync that covers its own pre-image, so a record that fails names a page the crash never
  reached, as does every record appended after it. The magic is `RDBJRNL2`; a journal an older build
  left behind is removed rather than replayed.

  **The two meta pages were the only pages a checkpoint overwrote without a pre-image.** With the
  first two fixed, the campaigns reached cut point 47 and came back with no tables at all. The
  journal had correctly rolled the data pages back to before the checkpoint, and the meta page still
  read `generation 5, checkpoint_lsn 17160`, which is a checkpoint that had not finished. Redo
  believed it,
  started above it, and skipped the records that would have re-applied what the journal had just
  undone; the catalog's own root was one of the rolled-back pages. The shadow meta page does not
  cover this and was never going to: `checkpoint` writes the *same* image to both slots, so the
  second is a second chance for the new record to survive rather than an older copy to fall back on.
  A checkpoint now journals both meta pages and syncs before it writes them, so the record that says
  a checkpoint happened is undone by the same mechanism as the pages it describes.

  **The reason all three hid is that the campaigns named after the rollback journal were not
  reaching it**, and that is fixed rather than noted. The journal holds pre-images only while a
  *checkpoint* is moving pages out of the log and into the data file. The commit campaigns commit
  into the log and stop, so every cut point they covered fell inside the log. `TRUNCATE` and
  `PERSIST` were covered by accident. `PRAGMA journal_mode = truncate` is a real change from the
  connection's default and runs two checkpoints on its way in, which is where all three defects
  were found. `PRAGMA journal_mode = delete` matches the default, returns without doing
  anything, and left the **default** journal mode the only one never tested inside a checkpoint.

  There are now four campaigns that crash inside the checkpoint itself, and their assertion is the
  stronger one: the failure is armed *after* the transaction is acknowledged, so the committed
  state is the only answer allowed at any cut point, where a crash inside a commit can only be
  asked for the old database or the new one.

  | schedule | cut points | outcome |
  |---|---:|---|
  | `tests/crash/delete-full-checkpoint-crash.txt` | 44 | every one recovered to the committed state |
  | `tests/crash/delete-full-checkpoint-io-error.txt` | 44 | every one recovered to the committed state |
  | `tests/crash/delete-full-checkpoint-disk-full.txt` | 44 | every one recovered to the committed state |
  | `tests/crash/truncate-full-checkpoint-crash.txt` | 50 | every one recovered to the committed state |
  | `tests/crash/persist-full-checkpoint-crash.txt` | 50 | every one recovered to the committed state |

  `crates/inillucent-compat/tests/search_crash.rs` had the same gap and it is closed the same way.
  Its `TAIL` was two `SELECT count(*)` statements, both served out of the buffer pool, so they made
  no VFS call at all: `a_rollback_journal_commit_is_atomic_across_both` was covering the log rather
  than the journal its name claims. With `PRAGMA wal_checkpoint` appended to that tail it covers
  the journal and passes.

  **And closing that gap found a fourth defect, in `wal` mode, where there was no journal at all.**
  With the checkpoint campaigns reaching further, `wal_crash.rs` and `search_crash.rs` both failed at
  cut 32: different files, different workloads, the same call. A checkpoint writes pages into the
  data file **in place**, and this engine's log is logical: once a page's content is below the
  recorded checkpoint point, the records that built it are redundant and their segments are retired,
  so the log no longer describes it. A page the checkpoint half wrote before a power loss is
  therefore content nothing can rebuild. Not the log, which has moved past it, and not the page,
  which is torn. The trace is unambiguous:

  ```
  seq=116 write /sim/wal.db offset=8192  len=4096     page 2
  seq=117 write /sim/wal.db offset=12288 len=4096     page 3
          crash: neither synced; sectors 16-31 come back Torn, Garbage, Dropped
  meta after the crash: gen=4 ckpt_lsn=56, the previous one, correctly not advanced
  recovery: page 3 checksum fe9063aa is not the computed f53956bb
  ```

  Everything about that is right except the outcome. The meta record correctly still named the
  *previous* checkpoint, and the log still held every record above it. Recovery failed because the
  page it needed to rebuild was one the log had stopped describing.

  SQLite is not exposed to this, and the reason is structural rather than careful: its log holds
  whole page images and a checkpoint is a copy, so an interrupted one is simply redone. A connection
  in `wal` now takes a `delete` journal. See `journal_for` in `crates/inillucent-engine/src/engine/locks.rs`.
  It holds the pre-images for the duration of a checkpoint and removes the file once the
  checkpoint's meta record is durable. It is the cost the default mode already pays, and what it
  buys is that an interrupted checkpoint is undoable in every mode rather than in three of the five.
  `off` is the one mode that still gets nothing, because that is what it asks for.

  Two of the checked-in schedules moved in ways to read rather than skim.
  `tests/crash/delete-full-short-write.txt` went from **one** detected corruption to **none**: a
  short write that lands on the journal now fails its record's checksum, so the replay stops instead
  of putting half a page image back, and the database is left whole rather than left detectably
  damaged. And every `DELETE`-mode campaign covers 22 cut points where the `TRUNCATE` and `PERSIST`
  ones cover 101, which is not a gap in the engine but a gap in the campaign: `PRAGMA journal_mode =
  delete` matches the connection's default and returns without doing anything, while the other two
  spellings run two checkpoints on their way in and the campaign then crashes inside those as well.
  The default mode is therefore the least exercised of the three, which is the wrong way round.

  Two smaller things came off the same thread. `Journal::finish` synced at `SyncMode::Normal` in its
  `truncate` and `persist` arms, which is the level that is allowed not to reach the media. What
  makes a journal stop being hot in those two modes is a change to that file, so until it lands the
  next open still finds pre-images naming a database whose commit completed. Both are `Full` now,
  which is what the `delete` arm already did. And `Body::Pad`, the filler the log writes to keep a
  synced write's tail on a device sector boundary, was being handed to the redo applier and counted
  among the records recovery applied; it names no page and carries nothing to apply, so it is now
  skipped where every other record that belongs to no transaction is decided.

- **The seventeen failing tests.** There were none. `schema_forms` 14, `planner` 5 and `ordering` 2
  all pass, with the pinned `sqlite3` present rather than absent. An earlier change had already
  removed the cause: `crates/inillucent-compat/src/interchange.rs` moves a database between the engines as
  `.dump` output replayed by the reference shell, instead of handing `sqlite3` a file it cannot read.
  This document was simply never updated.

## Recovery reads a page before redo has had a chance to rewrite it

**Closed by `cdc58eb`.** It was roadmap item 6, described there in the past
tense - the fix landed, the pair of tests landed, and the item stayed on the
open list. What closed it is
`replay_with_repair` in `crates/inillucent-engine/src/recovery.rs`, and what
holds it closed is `crates/inillucent-compat/tests/torn_page_with_image.rs`.

**Root cause named, and it is one level deeper than the hypothesis was.** The guess was a read on
the open path, before the tolerant pass that repairs the catalog root. It is not: the read is inside
redo itself. A logical row record changes a page by reading it - an `INSERT` into a leaf reads the
leaf, adds the row and writes it back - so a crash that tore a page failed the replay at the
**first** record naming that page, even when a later record in the same window carried the page
whole. The window's end state was knowable and recovery refused the file anyway.

It was found by naming every read in `open_file`. At cut 8 of
`crates/inillucent-compat/tests/free_map_checkpoint_crash.rs`'s `journal_mode = off` sweep the
refusal reads `replaying the log: page 4 checksum ... is not the computed ...`, and the three reads
before redo - the bootstrap open, the catalog attach, the catalog read - are all named and none of
them is it. Those names stay, because the next person asking this question should not have to
instrument a build to answer it.

**The fix, and where it runs.** When the logical pass fails with a corruption code, every record in
the window that carries a whole page image is applied - `WritePage`, a `CompactLeaf` that carries
one, and a split's three pages - and the same pass runs again. The images need no catalog and no row
decoder, which is what lets them go first. Re-running is sound because redo is idempotent on the
page-LSN rule: a record the first attempt applied has stamped its pages with its own LSN, so the
second attempt skips it. The free map's own read moved inside what the retry covers, because that
page is a page like any other and it is the one a checkpoint rewrites every time.

**On the failure and not before it, and that is measured rather than chosen.** Applying the images
unconditionally makes `read_checkpointed_catalog` succeed where it used to fail, which flips the
`repaired` flag and seeds the logical pass with the checkpoint-time catalog rather than the
end-of-window one. `wal_crash`'s commit campaign priced that: the one cut of twenty-three that
reaches the new state stopped reaching it. A committed transaction lost is a worse defect than the
one being fixed.

`crates/inillucent-compat/tests/torn_page_with_image.rs` is the pair the item asks for. A page the
window carries whole **and** that a record reads is torn and the database opens, answering all 199
rows; a page a record reads and no record carries is torn and the open refuses with
`SQLITE_CORRUPT`, naming the page. Both pages are chosen by reading the log rather than by being
named, so neither goes stale when the layout moves, and the fixture crashes rather than closing -
closing checkpoints the log away and there would be no window to be about.

`journal_mode = off` is documented to mean a torn checkpoint page is not recoverable at all, and
cuts 8 to 18 of that sweep still refuse: the log holds no image for page 4 there, so there is
nothing to rebuild it from. That is the mode behaving as specified, and it is what the second test
asserts deliberately rather than by accident.
