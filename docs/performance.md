# Performance against SQLite

Four measurements decide whether this engine is worth changing to: how long a workload takes, how
much processor it burns, how much memory it holds, and how big the file is. Three are wins and one is
a loss, and all four are here.

**[Feature comparison](feature-comparison.md) carries the full run**, per workload, per family, with
every interval and every control. This page is the summary.

## The four numbers

Measured at 100,000 rows on Windows, over the ten workload families the performance contract weights,
30 paired rounds per run, four consecutive runs, medians of the two middle runs.

**Measured 2026-09-23 on `main` at `6f84ce6` with task-2082's `7f93661` applied**, which is the engine
you get. Every number on this page is that run unless a section says otherwise. It was taken in a
quiet window: the other two agents working in this repository were paused, their process lists were
checked empty before the first reading, and the box read 14% busy with nothing of theirs running.

| | SQLite 3.53.4 | inillucent | |
|---|---|---|---|
| **elapsed time**, weighted over the ten families | the reference | 4.97x the speed | **397% faster** |
| **elapsed time**, the 95% lower bound the gate grades on | | 4.62x | **362% faster**, against a bar asking 200% |
| **processor time**, one round of the whole plan | 1,082 ms | 555 ms | **50% less processor** |
| **peak resident memory**, one round of the whole plan | 37.22 MiB | 40.76 MiB | **9.5% more**, the one loss |
| **the database file**, the same imported fixture | 16,830,464 B | 17,432,576 B | **3.6% larger** |

Every workload's answer is hashed and compared with SQLite's before its timing is allowed to count.
**All 34 workloads agreed on every round of all four runs.**

The four runs read 4.94x, 5.03x, 4.93x and 5.01x, with 95% lower bounds of 4.72x, 4.70x, 4.46x and
4.53x. The bound the contract grades on cleared its 3.00x requirement on all four.

### Both engines ran on the performance cores, and that had to be arranged

**The machine is an Intel Core Ultra 9 285, which has 8 performance cores and 16 efficiency cores.**
`inillucent-fullgate` runs this engine in its own process and SQLite in a child process. Left to
itself on 2026-09-23, Windows ran the gate process on the efficiency cores and the SQLite child on the
performance cores, so every paired round compared this engine on the slower cores with SQLite on the
faster ones. Nothing in the gate's output said so. The same gate, pinned each way, over the read
families, milliseconds for this engine and for SQLite:

| workload | both on performance cores | both on efficiency cores | not pinned |
|---|---|---|---|
| `scan.aggregate` | 1.76 and 92.9 | 2.97 and 115.9 | **2.95** and **92.9** |
| `scan.group` | 2.77 and 76.5 | 6.48 and 90.9 | **6.31** and **77.9** |
| `scan.sort` | 22.5 and 116.6 | 32.2 and 153.8 | **31.7** and **120.9** |
| `join.range` | 28.4 and 24.1 | 34.2 and 34.2 | **33.7** and **25.1** |

Not pinned, this engine's times are the efficiency core times and SQLite's are the performance core
times. **The published run pins both.** The gate process is started with processor affinity mask
`0xC03C03`, which is logical processors 0, 1, 10, 11, 12, 13, 22 and 23, the eight that Windows
reports with the higher efficiency class, and the SQLite child inherits the mask. The same weighted
headline three ways, four runs each except where it says two:

| where both arms ran | weighted | 95% lower bound |
|---|---|---|
| **both on the performance cores**, the published figure | **4.97x** | 4.62x |
| both on the efficiency cores, two runs | 5.72x | 5.35x |
| not pinned, this engine on efficiency cores and SQLite on performance cores | 4.40x | 4.27x |

**The published figure answers "how do the two engines compare on the same hardware",** and the
performance cores are the fastest hardware this machine has. The 4.40x is
real too: it is what this gate reports on this machine when nothing is pinned, and it is the figure
someone else taking the same measurement without a mask may see. It is not a property of either
engine, because it changes with where the scheduler puts each process.

**This also explains a difference that looked like a regression.** task-2074's two gates of
`420e68a` on 2026-09-22 read `scan.aggregate` at 1.80 and 1.76 ms, and task-2082's passes of the same
commit on 2026-09-23 read it at 2.96 to 2.98 ms. Those are the two core types. The 2026-09-20 run
this page used to carry read `scan.aggregate` at 1.70 to 1.73 ms against SQLite's 92.5 to 93.8 over
its four runs, which is the performance core figure on both arms, so on that day both processes
landed on the performance cores without being asked. Since task-2085 the gates pin themselves and
print which cores each arm ran on; [Reproducing it](#reproducing-it) says how.

### What moved since 2026-09-20

| family | 2026-09-20 | 2026-09-23 | |
|---|---:|---:|---|
| `write` | 2.12x | **3.04x** | 43% faster than it was |
| `open.prepare` | 1.46x | **1.69x** | 16% |
| `extension` | 1.57x | **1.73x** | 10% |
| `large.values` | 11.93x | 12.35x | 4% |
| `read.point` | 29.34x | 29.85x | 2% |
| `read.analytical` | 10.48x | 10.71x | 2% |
| `transaction` | 2.36x | 2.37x | none |
| `read.join` | 4.22x | 4.21x | none |
| `read.range` | 5.06x | 5.02x | 1% slower |
| `schema` | 1.37x | 1.31x | 5% slower |
| **weighted** | **4.53x** | **4.97x** | **10% faster** |

The write family is most of it, and task-2074 is most of the write family: a leaf's delta area is
sized by the page's free space rather than capped at 32 rows, and a compaction whose rows fit the
page's existing column widths splices them in. `write.insert.batch` went from about 0.60x to
**1.47x**, `write.update.indexed` to 3.37x and `write.delete` to 4.16x. `extension.fts.build` rode
the same change from 0.69x to 0.95x, which is most of `extension`'s move. `open.prepare` is
`prepare.trivial` going from 0.49x to 0.57x after task-2026 took a compile from 24 allocations to 13.

`schema` moved the other way within its own noise: it is one workload, run once a round, and its
median ratio went from 1.37x to 1.31x while its interval, bootstrapped from three values, is the
widest on the page.

**The processor figure got worse, and most of the reason is four workloads the 2026-09-20 plan did
not have.** It was 0.400 of SQLite's and is 0.500 now. The plan gained the four `read.correlated`
workloads since then (task-2066 section 4.3.1 and task-2076), and on this engine's arm they take
182 ms of each round against under half a millisecond on SQLite's; see
[the workloads that are slower](#the-workloads-that-are-slower). The contract does not weight them
into the elapsed time headline, but the processor figure is one round of the whole plan, so they are
in it. The four runs read 0.470, 0.520, 0.500 and 0.500 against a bar of 0.400, so the processor bar
is missed on all four.

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
| `read.point` | 16% | one row by rowid, by integer key, and through a secondary index | **2,885% faster** (29.85x) | 26.75x | 2.00x, met |
| `large.values` | 4% | text and blobs across the boundary where a value stops fitting in a leaf | **1,135% faster** (12.35x) | 9.14x | 1.50x, met |
| `read.analytical` | 10% | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | **971% faster** (10.71x) | 8.45x | 5.00x, met |
| `read.range` | 12% | selective ranges, forward and reverse, covering and not | **402% faster** (5.02x) | 3.95x | 3.00x, met |
| `read.join` | 8% | two table and four table joins | **321% faster** (4.21x) | 2.80x | 3.00x, missed on the lower bound |
| `write` | 20% | insert, update, delete, upsert, with and without indexes | **204% faster** (3.04x) | 2.42x | 1.50x, met |
| `transaction` | 10% | autocommit, small batches, large batches, savepoints | **137% faster** (2.37x) | 1.89x | no slower than SQLite, met |
| `extension` | 8% | JSON, FTS5, R-Tree | **73% faster** (1.73x) | 1.55x | 1.50x, met on three runs of four |
| `open.prepare` | 8% | parse, bind, step one row, reset | **69% faster** (1.69x) | 1.27x | 5.00x, missed |
| `schema` | 4% | `CREATE INDEX` and its backfill | **31% faster** (1.31x) | 0.94x | 3.00x, missed |

**The release condition is that no required family is below the 1.00x floor, and one of the four
runs met it.** `schema`'s lower bound read 0.94x, 0.81x and 0.95x on the first three runs and 1.30x
on the fourth, against a median ratio of 1.31x. `schema` is one workload, `schema.index`, run once a
round, so the family's bootstrap has three values and the widest interval on the page. On
2026-09-20 it went under on one run of four. No other family went under on any run.

**`extension` meets its bar for the first time on the lower bound**, at 1.55x against 1.50x, with
lower bounds of 1.56x, 1.48x, 1.54x and 1.57x. `extension.fts.build` is still the worst workload in
the family, at **0.95x**, 10.9 µs a document against 10.4, where it was 0.69x. The rest of the family:
`extension.rtree.insert` 2.32x, `extension.rtree.query` 4.88x, `extension.fts.query` 1.44x and
`extension.json` 0.97x.

What it moved from: on 2026-09-20 the family read 1.57x inside the whole plan with lower bounds of
1.17x, 1.33x, 1.38x and 1.45x, with `extension.fts.build` at 0.69x. A run of the extension family
alone on 2026-09-15 read it 1.58x, 1.60x, 1.59x and 1.67x with lower bounds of 1.40x, 1.39x, 1.39x and 1.45x,
with `extension.fts.build` at 0.56x to 0.58x and `extension.fts.query` at 1.70x to 1.85x - 1.77x on
its own, against the 1.43x the reverted segment format left it at. That run is the measurement
[the roadmap](roadmap.md#1-the-extension-and-join-families-either-side-of-their-bars) argues from.

**`read.join` misses its bar on the lower bound, on all four runs**, at 2.73x, 2.83x, 2.78x and 2.90x
against a 3.00x requirement, with the family reading 4.21x. `join.selective` reads 20.94x and
`join.range` **0.84x**; the range join is the slow half and it is what holds the bound under the
requirement. task-2082 measured `join.range` on three builds across 27 passes and found this engine's
time for it unchanged on every one; the bound moves with SQLite's arm of the same workload.

A join-only run reads the family much higher - 6.46x, 6.26x, 6.14x and 5.60x, with lower bounds of
4.11x, 4.00x, 4.02x and 3.67x, when the gate was given `--families read.join` on 2026-09-15 - and
that is the measurement this page used to carry. **A family measured on its own is not the same
measurement as the same family inside the whole plan**, because the plan's other workloads decide
what is in the pool when the join runs. The figure in the table above is the whole-plan one, which is
what the contract grades. In that run of the join family alone `join.selective` read 35.49x and `join.range` 1.17x.
The bounds before task-1911's chain reuse were 2.97x, 3.00x, 3.00x and 2.99x against the same 3.00x
bar.

**`transaction` is 2.37x, and the reason is what a commit costs.** A commit is one append to the log
and one sync of it, and the log is folded into the file when it has grown past four mebibytes, when a
caller asks for it, or when the connection closes. Measured on the gate's own counters,
`txn.autocommit`'s hundred statements make **100 log writes, 100 log syncs, no data file syncs and no
folds**. `txn.autocommit` reads **0.98x**, 1.17 ms a statement against SQLite's 1.15: both engines do
exactly one `fsync` a commit, and this device's `fsync` is most of the millisecond. `txn.batched`
reads 3.56x and `txn.large` 3.80x, 2.76 ms a round against SQLite's 10.50.

## The workloads that are slower

Thirty workloads are weighted into the headline. Twenty four are faster than SQLite and these six are
not:

| workload | family | ratio | how much slower | per operation | why |
|---|---|---|---|---|---|
| `prepare.trivial` | `open.prepare` | 0.57x | **75% slower** | 729 ns against 407 | `SELECT 1` compiled on every call. `inillucent-prepareprofile` counts **13 allocations** on the path the gate times, where it counted 24: task-2006 removed three and task-2026 eight more. **Nine of the thirteen leave with the compiled statement** - the bound result columns, the column's name, the `Box<BoundSelect>`, the projection expression tree and the output names - so what is left is the compile's answer rather than its scratch |
| `join.range` | `read.join` | 0.84x | **18% slower** | 57.1 µs against 48.6 | an index range and a probe per entry, where SQLite amortises one statement's overhead over two hundred rows |
| `extension.fts.build` | `extension` | 0.95x | **5% slower** | 10.9 µs a document against 10.4 | it was 45% slower on 2026-09-20. FTS5's build is four ordinary row writes a document, so it moved with the write path |
| `range.lookaside` | `read.range` | 0.96x | **4% slower** | 57.4 µs against 55.7 | the same shape as `join.range`, 200 rowid probes |
| `extension.json` | `extension` | 0.97x | **3% slower** | 289 ns a call against 282 | the extraction, plus one uncontended mutex and two comparisons a call. The parse of a repeated document and path is already cached |
| `txn.autocommit` | `transaction` | 0.98x | **2% slower** | 1.17 ms a statement against 1.15 | one `fsync` each, and on this device an `fsync` is most of the millisecond |

`write.insert.batch` left this list: it was about 0.60x and is **1.47x**, 7.0 µs a row against
10.2.

**Four more workloads are far slower, and the contract does not weight them.** They are the
`read.correlated` family: a subquery that names a column of the outer query, answered once per outer
row. They reach no family, no floor and no headline, deliberately, because each is graded against
the join that asks the same question rather than against a bar. They are in the plan, so they are in
the processor and memory figures above.

| workload | this engine | SQLite | how much slower |
|---|---|---|---|
| `correlated.exists`, `EXISTS` over 400 outer rows | 59.69 ms | 0.29 ms | **21,332% slower** |
| `correlated.in`, `IN (SELECT ...)` over the same | 118.19 ms | 0.10 ms | **118,020% slower** |
| `correlated.exists.selective`, a filter keeps 4 outer rows | 2.11 ms | 0.022 ms | **9,395% slower** |
| `correlated.scalar.selective`, a scalar block, 4 outer rows | 2.11 ms | 0.021 ms | **9,839% slower** |

task-2068 made a correlated block 340% faster and task-2076 stopped answering blocks for rows the
filter throws away, which is why the two selective arms are 2 ms rather than 55. Both said the target
was not met, and it is not: SQLite answers these as a join and this engine answers every block by
running it. If an application writes correlated subqueries against large tables, write them as joins
on this engine. **These four are also the one place where the core count changed the answer:**
`correlated.exists` took 59.69 ms pinned to the eight performance cores, 46.35 ms pinned to the
sixteen efficiency cores and 38.74 ms with all twenty four available, while every other workload was
faster on the performance cores. Why is not investigated here.

At the other end of the same table: `point.miss` 52.48x, `scan.aggregate` 52.09x, `large.read`
44.69x, `point.rowid` 30.74x, `scan.group` 27.72x, `join.selective` 20.94x, `point.index` 16.34x,
`range.reverse` 15.63x and `range.covering` 8.41x.

### Where `write.insert.batch`'s time and log volume actually go

Measured 2026-09-20 with `inillucent-writelogattrib` on the medium fixture at the gate's own geometry,
a 32 KiB page: 2,000 inserts into `main_table`, which carries two secondary indexes, in one
transaction. The log is read back from disk with the same decoder recovery uses, so the byte counts
are exact rather than estimated. The same run is repeated with the two indexes dropped, and the
difference is what they cost.

| | with both indexes | without either | the two indexes |
|---|---:|---:|---:|
| wall | 50.43 ms | 15.47 ms | **34.96 ms, 69%** |
| applying the changes to pages | 46.37 ms | 11.77 ms | 34.60 ms |
| log written | 1,563.9 KiB | 1,163.4 KiB | 400.5 KiB |
| leaf compactions | 181 | 58 | 123 |
| splits | 9 | 8 | 1 |

**The indexes are where the time is, and the split records are where the bytes are.** Those are two
different answers and the roadmap named the second:

| record kind | records | bytes | share of the log |
|---|---:|---:|---:|
| `Structural` (a split) | 9 | 864.7 KiB | **55%** |
| `InsertRow` | 6,000 | 687.5 KiB | 44% |
| `CompactLeaf` | 181 | 11.3 KiB | 0.7% |
| `AllocPage`, `Commit` | 10 | 0.4 KiB | 0.03% |

A split record carries the left page, the right page and the parent, whole - 98,384 bytes at a 32 KiB
page size, for one row that would not fit. So a logical split record, which the roadmap once listed, would
take 55% off the log's volume. **It would take about 2% off the workload's time**, because the log is
written once and synced once at the commit and the bytes are not what the workload is waiting for.

**What the time is, measured rather than divided.** `WriteStats::room_nanos` times the inside of
`make_room` - compacting a leaf, or splitting one:

| | with both indexes | without either | the two indexes |
|---|---:|---:|---:|
| wall | 44.17 ms | 15.68 ms | 28.49 ms |
| **making room** | **23.97 ms** | 5.10 ms | **18.87 ms** |

**Making room was 54% of the transaction** when that table was taken. It is **36%** now, 10.57 ms of
29.60, and it has been split into the four passes it actually is (task-2024). Medians of five runs,
same fixture, same geometry:

| | with both indexes | without either |
|---|---:|---:|
| making room | 10.57 ms | 4.12 ms |
| building the image | 7.77 | 1.59 |
| - the merge, which rows are live | 1.98 | 0.50 |
| - reading every one of them | 1.90 | 0.33 |
| - the sizing pass | 1.16 | 0.12 |
| - the encode | 2.22 | 0.35 |

**The merge is the one pass that does not grow with the page size** - it is per delta row and a delta
area held at most thirty-two when this was measured (task-2074 sized it by the free gap instead, and
took the sweep's compactions from 1,624 to 423; see `docs/closed-items.md`) - and the other three roughly double between an 8 KiB page and this
one, because a 32 KiB leaf keeps four times as many rows. An attribution of this stage taken at 8 KiB
understates it by about half, and the gate runs at 32 KiB.

**2.84 ms of the sizing pass came off by asking it a simpler question.** A compaction needs one bit,
do all the live rows fit one page, and `fit_widths` was pricing the leaf a row at a time because its
other caller has to stop at the first row that does not fit. `fit_all_widths` makes one pass,
resolves once and compares once: 4.00 ms to 1.16, and the transaction 33.91 ms to 29.60. It cannot
disagree with the incremental pass, because the price of a run of rows never falls as rows are added,
so a leaf that fits whole had every prefix of it fit.

**At the gate, four runs alternating between that build and a control with the sizing pass put back**,
so that drift in the box shows up in both:

| | control | with the one-pass sizing |
|---|---|---|
| `write.insert.batch`, this engine's arm | 38.34 ms, 36.95 ms | **33.14 ms, 32.93 ms** |
| the same workload's ratio | 0.46x, 0.58x | **0.56x, 0.63x** |
| the `write` family | 1.54x, 1.74x | **1.67x, 1.80x** |

**Read this engine's own arm rather than the ratio here.** The box was not quiet for those runs -
another ticket held both GPUs and the local model server throughout - and it shows in the SQLite arm,
which drifted from 18.49 ms to 21.33 ms across the sitting while this engine's arm varied by 3.8% in
the control and 0.6% with the change. On its own arm the change is **12.2% faster**, 37.65 ms to
33.04 ms as medians, which is what `inillucent-writelogattrib` reports for the same workload off the
gate entirely.

What is left of the lever named by [the closed roadmap item](closed-items.md#writeinsertbatch-is-faster-than-sqlite) - a compaction that splices its delta
rows in rather than re-encoding every kept row - is the 2.22 ms encode. It cannot also remove the
merge or the sizing pass, because the widths it would have to reproduce exactly are a function of
every value the leaf keeps.

**And the delta area is not where the time is either.** `LeafRef::locate` walks each leaf's unsorted
delta area on every insert, which `docs/roadmap.md` named as the cause. Counted directly at an 8 KiB
page: **8,329 calls, 119,645 entries walked, 5.1 ms**, 14.4 entries a call, against 66.8 ms of apply
time. Under eight per cent, and that is the whole walk - a fingerprint block over it would save less,
because a probe that matches still decodes and the block costs a hash per insert.

**`txn.large` is no longer on this list.** It was the slowest workload on the board at 0.09x, it decided
the `transaction` floor, and it is now **3.80x**, where the median round takes 2.76 ms against
SQLite's 10.50. Two things got it there and only one of them is the engine.

**How a ratio on this page is taken.** A family's or a workload's ratio is the gate's own paired-round
figure, which pairs the two arms round by round and reports the middle of the thirty. The number
printed here is the median of the two middle runs of four. An absolute time printed beside it is the
median of the same four runs' own medians. The two are different summaries of one set of rounds, so
dividing the printed times gives a number close to the printed ratio rather than exactly it: for
`scan.sort`, 22.88 ms and 125.22 ms divide to 5.47 where the paired figure is 5.43. The paired figure
is the one the contract grades and the one quoted.

### The cost of each secondary index, on the index count sweep

**One number per index count, because the gate's fixture has one index count.** `main_table` carries
two secondary indexes, so a change aimed at index maintenance measured on `write.insert.batch` is one
point of a curve. task-2074 added the sweep: `inillucent-writeprofile --sweep` inserts 5,000 rows in
one transaction into a 100,000 row table carrying 0, 2, 5 and 10 indexes, in process, with the write
path's own counters; `inillucent-perfhistory --only insert.indexes` inserts 20,000 rows into a 20,000
row table carrying the same index counts, beside SQLite. The indexes are built after the rows are
loaded, on both engines, so the inserts meet leaves at the fill an import leaves.

Measured in one quiet window, the fastest of five interleaved rounds, microseconds a row:

| indexes | before | the delta area sized by the free gap | and the compaction splice |
|---:|---:|---:|---:|
| 0 | 7.93 | 5.04 | 5.14 |
| 2 | 18.28 | 10.47 | 9.12 |
| 5 | 33.45 | 17.16 | 15.50 |
| 10 | 73.06 | 39.97 | 37.20 |
| cost per index at 2 | 5.17 | 2.71 | 1.99 |
| leaf compactions at 10 indexes | 1,624 | 423 | 423, 391 of them spliced |
| time making room at 10 indexes | 194.03 ms | 66.18 ms | 49.49 ms |

The wall ratio against SQLite, net of process startup, from `tests/performance-history.tsv`:

| indexes | before | the directory | and the splice |
|---:|---:|---:|---:|
| 0 | 0.09x | 0.14x | 0.14x |
| 2 | 0.08x | 0.18x | 0.19x |
| 5 | 0.08x | 0.25x | 0.19x |
| 10 | 0.43x | 1.20x | 1.28x |

**Taken again on 2026-09-23 with both arms on the performance cores**, on the engine the rest of
this page describes: the sweep read 5.09, 9.15, 15.33 and 36.37 µs a row at 0, 2, 5 and 10 indexes,
within 3% of the last column above, with the same 423 compactions and 391 splices at 10 indexes. The
wall ratios in `tests/performance-history.tsv` for that run read 0.15x, 0.18x, 0.22x and 1.13x.

**The first change is most of it.** The delta area used to compact every 32 rows whatever its leaf
held, and an index leaf holds thousands, so the indexes paid for a repack of the whole leaf every 32
entries. With a directory in key order a lookup in the area is a binary search, the area takes the
whole free gap, and the compactions fall by 3.7x. **The splice is the smaller second half**, 7% to
15% on top at two indexes and more: most compactions now keep the page's column widths and heap and
write only the new rows' values. SQLite's own cost jumps at 10 indexes on this table, from 64 ms at
5 to 525 ms, which is why that ratio moves more than the others.

**And on the gate**, which is what the rest of this page reports: `inillucent-fullgate` on the medium
fixture at a 32 KiB page, the base commit against this one, 30 rounds each, alternated twice in one
quiet window. The workloads that moved:

| workload | base | base | task-2074 | task-2074 |
|---|---:|---:|---:|---:|
| `write.insert.batch` | 0.88x | 0.81x | 1.52x | 1.52x |
| `write.update.indexed` | 2.13x | 2.10x | 3.77x | 3.52x |
| `write.delete` | 3.75x | 3.47x | 4.69x | 4.61x |
| `extension.fts.build` | 0.70x | 0.73x | 0.98x | 0.99x |
| `extension.rtree.insert` | 2.02x | 1.99x | 2.37x | 2.40x |
| `txn.large` | 4.06x | 4.15x | 3.84x | 3.59x |
| `join.range` | 0.90x | 0.87x | 0.87x | 0.84x |
| the `write` family | 2.43x | 2.33x | 3.28x | 3.24x |
| the weighted headline | 4.96x | 4.84x | 5.10x | 5.16x |

`write.insert.batch` clears 1.00x for the first time, and the `extension` family's lower bound goes
from 1.49x to 1.59x, over its 1.50x bar. **`txn.large` is 10% slower**, consistently: it is 2,000
updates that lengthen a text value in place, and making heap room for a longer value moves the delta
area down by the value's size - the area is larger now, so each move copies more. **`join.range`
moved by about as much as the two base runs differ from each other**, but it is the workload that
holds `read.join`'s lower bound, and that bound went from 3.14x and 3.04x to 2.92x and 2.98x against
a bar of 3.00x. Both are task-2082, and the next section is what it found.

### The two workloads task-2074 cost, measured again

**Neither regression survives pinning the gate to one kind of core.** task-2082 measured both
again with `inillucent-fullgate` on the medium fixture at a 32 KiB page, 30 rounds, the builds
alternated. The pass that decides it was taken with the gate process pinned to this machine's
performance cores: affinity mask `0xC03C03`, logical processors 0, 1, 10 to 13, 22 and 23 on a Core
Ultra 9 285. SQLite runs as a child of the gate and inherits the mask, and the mask was read back
from the SQLite child on every pass: `0xC03C03` each time. task-2064 found why this matters.
Unpinned, the scheduler put this engine on the efficiency cores and SQLite on the performance cores,
so an unpinned pass measures the two engines on different hardware (task-2085 makes the gates pin
themselves).

The test was written down before the pinned passes ran: the regression is real only if task-2074's
mean is at least 3% slower than the build before it **and** every task-2074 pass is slower than
every pass before it. The fix counts only if it is at least 2% faster than task-2074 on the mean
**and** every one of its passes is faster.

Pinned, 14:50 to 15:18:51Z on 2026-09-23 in quiet windows, this engine's time in milliseconds:

| build | `txn.large`, each pass | mean | `join.range`, each pass | mean |
|---|---|---:|---|---:|
| before task-2074 (`420e68a`) | 2.838, 2.803 | 2.821 | 27.55, 27.37 | 27.46 |
| task-2074 (`abf042c`) | 2.845, 2.837, 2.899, 2.827 | 2.852 | 27.46, 27.43, 27.53, 27.59 | 27.50 |
| task-2082 | 2.726, 2.672, 2.726 | 2.708 | 27.46, 27.25, 27.27 | 27.33 |

- **`txn.large` did not regress.** task-2074 is 1.1% slower on the mean, under the 3% the test asks
  for, and its fastest pass (2.827) is faster than the slowest pass before it (2.838).
- **`join.range` did not regress**, 0.1% on the mean. SQLite's arm read 23.8 to 24.1 ms on every
  pinned pass, and the `read.join` lower bound read 2.80x to 2.86x on all three builds, the build
  before task-2074 included. So the bar is missed, but task-2074 did not cause it. The 3.14x and
  3.04x quoted above for the build before task-2074 were taken when both arms ran on performance cores.
- **The fix is 5.0% faster than task-2074 on `txn.large`**, and its slowest pass is faster than
  task-2074's fastest.

The rest of the write family, pinned means in milliseconds:

| workload | before task-2074 | task-2074 | task-2082 |
|---|---:|---:|---:|
| `write.insert.batch` | 24.47 | 14.02 | 13.90 |
| `write.update.indexed` | 33.91 | 20.35 | 20.40 |
| `write.delete` | 20.49 | 16.08 | 16.28 |

task-2074's gain is intact. `write.delete` reads 1.3% slower with the fix, from three passes against
four, and every pass of each build is inside 15.8 to 16.4 ms.

**What the fix is.** Counted on the `write` and `transaction` families alone, which put the tables
in the state `txn.large` meets, all but a few of its 2,000 statements either update in place or
match nothing. The in-place update's code did not change in task-2074, so the carve was not the
cause. `Bind::Scatter` picks rowids up to `main_table`'s row count and `side_table` holds a quarter
of that, so three in four of the updates look up a rowid past the end of `side_table` and land in
its last leaf. `write.insert.autocommit` appended 100 rows to that leaf earlier in the round. With
the 32 row limit they were packed every 32; since task-2074 they stay in the delta area until the
free gap fills, and each lookup past the end binary searched them, decoding one delta key per
halving. `LeafRef::delta_search` now compares the last directory entry first. A probe above it is
past the whole area and is answered after that one comparison. The same search runs twice for every
append at the right edge of a table, the uniqueness check and the write's own `locate`.

`live_between` was changed as well, as the ticket suggested: a range probe into a leaf that has been
written to reads only the sorted rows and the delta directory entries inside its bounds, where it
used to materialise the whole leaf and filter it. It does not show on the gate, because a round reads
before it writes, so every leaf `join.range` probes was packed by the fixture import and has no
delta row. A splice rule was tried and dropped: refusing a splice when the heap held more
unreferenced bytes than the splice would leave free changed no count on the gate.

**The unpinned passes, kept as evidence, are efficiency core figures.** 27 unpinned passes in two
quiet windows (11:35 to 12:13:31Z and 12:35 to 13:06Z) gave the same two answers under the same test and
different absolute times: `txn.large` 3.12 and 3.20 ms before task-2074 in the two windows, 3.20
and 3.19 on task-2074, and 3.04 with
the fix; `join.range` 33.0 to 33.6 ms on every build while SQLite's arm drifted from 24.8 to 31.6 ms
and pulled the `read.join` lower bound between 2.57x and 3.16x. Two of the pinned passes of the
build before task-2074 exited at once, because the worktree it was built in had lost its link to
the SQLite oracle; they are not counted. The gate refuses without the oracle, but only a pass that
takes two seconds instead of two and a half minutes shows it. Every output is in
`_agent_output/task-2082-txn-large/` in the main checkout.

**Every pass in this section ran without a processor affinity mask**, so by what task-2064 found the
same day this engine's arm was on the efficiency cores and SQLite's on the performance cores. The
comparisons between builds are like for like, because every build ran the same way; the absolute
milliseconds are efficiency core figures, and so is the `read.join` bound that followed SQLite's arm.
The published run at the top of this page, pinned, reads `txn.large` at 2.76 ms and `join.range` at
28.57 ms on this engine's arm.


### What a statement costs before it reaches a tree

The whole tree write was ablated out of the in-place update path, so the statement found its row,
decided what to write, and returned without writing. `txn.large` then measured **1.54 microseconds
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
execution's values, which is why `Statement::rebindable` existed to refuse a re-run. An
`Expr::Parameter` reads a cell the statement refreshes instead, and it costs a repeated literal
nothing it did not already cost: a text literal clones per evaluation either way.

### The workload that was measuring nothing

`fullgate` runs a round's workloads in order against one database, and `txn.batched` and `txn.large`
are the same statement over the same scattered rowids with the same `row {iteration} lorem ipsum ...`
text and the same repeat. So by the time `txn.large` ran, **every row it touched already held the
bytes it was about to write**: 2,000 updates that changed nothing, under a description saying
"2,000 `UPDATE`s in one transaction".

That was hiding a real defect rather than only mis-measuring. `only_change` answered `None` both when
*nothing* differed and when *several* columns did, so the caller took the most expensive path it has
for the cheapest case: a tombstone, a delta insert, and a compaction every 32 writes (the delta limit then)
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
measures an operation doing no work, which is a test that cannot fail, **the workload was corrected
too**.
`txn.batched` and `txn.large` now reset `side_table.note` first, outside the timed region on both
arms, where `sqlite_bench.c` already runs a workload's setup. `txn.large` reads **0.09x** on the old
workload with the old engine, **0.59x** on the old workload with this one, and **3.63x** once the
workload asks a real question. SQLite's own arm goes from 834 microseconds to 9.65 milliseconds when
it has to perform the same 2,000 updates.

### What an `UPDATE` that changes a value's length used to cost

Until task-1890 a heap slot could only be written over by a value of **exactly** the same length, so
`txn.large` replaces an eight byte `note 1234` with a forty-two byte
`row 1234 lorem ipsum ...`, so it took none of the in-place path at all. Every statement became a
tombstone plus an insert into the leaf's delta area, and every thirty-second one a compaction over
every live row of the leaf.

A longer value is now written at the bottom of the leaf's heap, after the tombstone bitmap and the
delta area have been moved down by its length, and the slot is repointed at it; a shorter one is
written where the old one lay and the slot's length is lowered. The bytes left behind are what SQLite
calls fragments and the next compaction reclaims them. The log record did not change: an in-place
update is replayed by re-running the write over a page that LSN ordering has already put back into
the state the original write saw, and the relocation is a function of that page and the value alone.

## Memory

**40.76 MiB against SQLite's 37.22, which is 9.5% more**, the same as on 2026-09-20. The contract asks
for 5% less, so this bar is missed, and it is the only headline that is a loss.

It has been worked three times. It was **102% more** three rounds of work ago, **43% more** two rounds
ago and **14% more** one round ago. The four changes that took it from 102% to 43% were: bounding the
redo buffer to 512 KiB, stopping the index build holding three copies of the tree, collecting a
version log that nothing was collecting, and capping the allocator's free list in bytes as well as in
blocks. What took it from 43% to 14% was the **file**, not another buffer.

**What took it from 14% to 9.5% was the index build stopping going through the page pool** (task-2000,
design 2). A bulk build wrote each page into a pool frame, which then had to be written out and
evicted; it writes the page into the file directly and the frame is never taken. `schema.index` is
what sets this plan's high water mark, so the frames it no longer occupies are the peak:

| | before | after |
|---|---:|---:|
| the child's peak | 42.45 MiB | **40.76 MiB** |
| `schema.index` raises the mark by | 12.50 MiB | **10.53 MiB** |
| and the pool at that point holds | 24.66 MiB | **22.91 MiB** |

The attribution reads the process high water mark after every workload and prints the page pool's own
bytes beside the total:

| workload | peak, before | pool | everything else | peak, after | pool | everything else |
|---|---|---|---|---|---|---|
| the file opened and the pool warmed | 31.50 | 22.59 | 8.90 | **26.74** | **17.84** | 8.90 |
| every read workload | 31.54 | 22.59 | 8.95 | 26.79 | 17.84 | 8.94 |
| `write.insert.batch` | 34.73 | 22.94 | 11.55 | 30.92 | 18.19 | 12.20 |
| `schema.index` | **51.35** | 29.84 | 12.23 | **46.61** | 24.66 | 12.78 |

The "everything else" column does not move. The whole 4.74 MiB came out of the page pool, and the
pool fell because the file did.

**The same attribution on today's engine**, from the second run of the 2026-09-23 set, which is the
one the headline above is taken from. The file opens at 25.21 MiB with 16.56 of it in the pool, the
ordinary read workloads leave the mark where it is, the two unfiltered correlated workloads raise it
by about 3 MiB each, and `schema.index` takes it to its peak. The write family no longer raises it:
the correlated workloads run first and hold more than the writes need.

| workload | peak MiB | rise MiB | pool MiB | everything else MiB |
|---|---:|---:|---:|---:|
| the file opened and the pool warmed | 25.21 | 25.21 | 16.56 | 8.65 |
| every read workload before the correlated ones | 25.27 | at most 0.02 | 16.56 | 8.70 |
| `correlated.exists` | 28.33 | 3.06 | 16.56 | 8.71 |
| `correlated.in` | 31.46 | 3.13 | 16.56 | 8.73 |
| `schema.index` | **40.79** | **9.33** | 22.88 | 11.26 |

The correlated rise is in the peak but is not the pool and not the "everything else" the process
keeps: `rss` reads 25.29 MiB after `correlated.in` while the peak reads 31.46, so it is memory that
workload takes and gives back. It does not decide the peak, because `schema.index` goes 9.33 MiB
above it.

Where the remaining 3.7 MiB is:

| | inillucent | SQLite | what it is |
|---|---|---|---|
| the cached database | 16.56 MiB | about 16 MiB | the `.rdb` is 1.036x the `.db`. Under 0.6 MiB left here |
| the process floor | 8.65 MiB | about 4.2 MiB | **3.62 MiB of it is what any Rust binary on this machine costs before the engine exists**, measured rather than inferred: a 130 KB program whose `main` reads its own working set and returns peaks at 3.62 MiB over five runs, 0.66 MiB of it private. The remaining 5.03 MiB is this engine's own code, its statics and opening the file |
| `schema.index`'s rise | 9.33 MiB | about 15.9 MiB | the pages the new index occupies plus the sort's arena, measured from a mark the correlated workloads had already raised. This one is **smaller** than SQLite's |

So most of what is left is the operating system's, which neither engine escapes, and one
`CREATE INDEX`.

**And the engine's own allocator is not part of it.** The same 130 KB program built with
`inillucent-alloc` installed as its global allocator measures **3.62 MiB, the same figure to two
decimal places**, with 0.66 MiB private either way. It has no initial reservation to size down: it is a
size-classed free list whose lists start empty and which hands a block back to the system allocator
when a class is full, so the first allocation is the first one a program makes. That answers the
question task-2000's design 10 asked - whether two to three of these mebibytes were the allocator's
arena - with a no, and it is why **the resident set bar stays missed at 1.00x** rather than being
closed by sizing something down. What is left to attack is the file the pool holds and the index
build's own arena, and nothing else is a buffer anybody chose.

### Where a retrieval index's resident bytes go

Measured 2026-09-15 with `inillucent-indexresidency` on the 600,589 chunk corpus at 768 dimensions,
1,705,097 terms, with the vectors left in the file, which is the default. Each part is read in the
order an open reads it and the resident set is sampled between them, so what a part costs is measured
rather than derived from its file's size. Two runs, the same answer to a tenth of a mebibyte.

| part | on disk MiB | resident MiB | share of resident |
|---|---:|---:|---:|
| `lexical.bin`, the BM25 postings | 614.8 | **890.3** | **53%** |
| `store.bin`, the chunks and their dictionaries | 564.8 | 620.3 | 37% |
| `graph.bin`, the HNSW adjacency | 87.7 | 154.2 | 9% |
| `vectors.bin` | 1,759.5 | 0.0 | none |
| total | 3,026.9 | **1,664.9** | |

The postings are the largest single part and the graph is the smallest, which is the reverse of the
order [the roadmap](roadmap.md#2-the-retrieval-indexs-footprint) had assumed. The store is 620 MiB
resident for 565 MiB on disk - it is not expanded much by loading, it is simply all of it in memory.

**And the peak is reached by that one statement.** Read over four runs on 2026-09-20, before the
correlated workloads joined the plan, the high-water mark after every workload:

| workload | peak MiB | this workload added |
|---|---|---|
| the file opened and the pool warmed | 24.95 | 24.95 |
| every read workload, all eleven of them | 25.00 | 0.05 in total |
| `write.insert.batch` | 28.20 | 3.20 |
| the other four write workloads | 29.21 | 1.01 in total |
| `txn.batched` | 30.20 | 0.98 |
| **`schema.index`** | **40.75** | **10.53** |
| every remaining workload | 40.75 | nothing |

The whole plan held **30.20 MiB** until it built an index, and building one added ten and a half - it
added twelve and a half before task-2000's design 2 stopped the build going through the page pool.
On 2026-09-23 the plan holds 31.46 MiB before the index build, because of the correlated workloads,
and the build adds 9.33 to reach the same 40.8. So the bar is not missed by a buffer that is slightly too big everywhere; it is missed by
one statement, and by the arena its sort holds.

That arena is the one lever left, and it is priced rather than pulled: task-1869 measured spilling
the sorted run to a temporary file at about **8 ms on a 27 ms statement**, which puts the `schema`
family under the 1.00x floor the contract sets. Buying memory with a floor is the trade that ticket
declined and this one declines again.

## Disk

The imported fixture, both engines given the same data:

| | inillucent | SQLite |
|---|---|---|
| the medium fixture | 17,432,576 B | **1.036x**, within 4% |

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
Linux problem. The Linux arm has not been re-measured since; the Windows headline has moved to 397%
in the meantime, so treat the pair as the finding it was rather than as a comparison with the number
at the top of this page.

With a size classed free list in place of the system allocator, a `SELECT 1` compile goes from
46.95 ms to 38.97 ms on Windows (17% faster) and from 39.91 ms to 38.20 ms on Linux (4% faster),
and **the two platforms then run the same speed**, 38.97 against 38.20. On the Windows compile the C
runtime's heap is 59% of the time.

SQLite does per statement work with the operating system that Windows charges heavily for and Linux
barely does. SQLite's arm is the denominator of every ratio on this page, and it moves across
platforms
while this engine's does not. The absolute work is the same on both, and lowering it is what the
missed bars need. Neither the allocator change that took Windows from 3.24x to 3.86x nor anything
since has been measured on Linux.

## What is not measured here

- **The API an application actually has.** Every figure on this page is the pipeline's: the gate
  calls `plan`, `prepare` and `pipeline` directly, which is the shortest path to an answer and not
  the one a caller has. `inillucent-fullgate --api connection` drives `Connection::prepare` and
  `Statement::step` instead, and `--api both` runs the two in the same round so the difference is
  paired rather than compared across two runs of the binary on a machine that moved in between
  (task-2066 section 4.3.10). What sits between them is the plan cache lookup, the parameter count,
  a `String` per result column per execution, and the dirty frame walk on release.
- **A table larger than the buffer pool.** `story_large_table_nightly` builds one in the `nightly`
  tier and scans, sorts and deletes half of it (task-2066 section 4.4.5). Nothing on this page is
  measured at that size, and the families here all fit in the pool - so a number here says what the
  engine does when its working set is resident, and that story says what it does when it is not.

- **One scale.** These are the medium fixture, 100,000 rows. The other two fixtures were taken in the
  same window, pinned the same way, two runs each rather than four:

  | | 5,000 rows | 100,000 rows | 600,000 rows |
  |---|---|---|---|
  | weighted | 3.87x, **287% faster** | 4.97x, **397% faster** | 5.34x, **434% faster** |
  | 95% lower bound | 3.78x | 4.62x | 5.10x |
  | `write` | 1.05x, **5% faster**, under the floor on one run | 3.04x | 6.32x, **532% faster** |
  | `extension` | 1.82x, lower bound 1.66x | 1.73x, lower bound 1.55x | 2.00x, lower bound 1.88x |
  | processor, ours against SQLite's | 242 ms against 840, **71% less** | 555 against 1,082, **50% less** | 1,227 against 945, **30% more** |
  | peak memory, ours against SQLite's | 16.23 MiB against 9.36, **73% more** | 40.76 against 37.22, **9.5% more** | 188.13 against 181.83, **3.5% more** |

  `write` still grows with the table, because a bigger table spreads what a statement costs to set
  itself up over more of a page: at 5,000 rows three of its five workloads are slower than SQLite.
  **At 600,000 rows this engine uses more processor than SQLite for the whole plan**, although it
  finishes it faster; that was not visible before because only the medium plan's processor time was
  published. `extension` now clears its 1.50x bar on the lower bound at all three scales. At 5,000
  rows the memory figure is mostly the process floor described under [Memory](#memory), which is a
  larger share of a small file.
- **One machine.** Windows 11 on x64, which `tests/performance-history.tsv` records in its
  `machine` column as `machine-` and eight hex digits. That label is a digest of the
  machine's own name rather than the name: the column exists so rows taken on two machines
  can be told apart and rows taken on one can be read as a series, and a digest does both
  without publishing whose machine it was. `INILLUCENT_MACHINE` sets the label directly for
  a fleet that would rather read `ci-linux-x64`. The disk matters more than it looks: part way through a four
  run sequence, `txn.batched` is 200 commits and 200 `fsync`s, and it goes from 309 ms to 895 ms
  **on
  SQLite's own arm**, on the same fixture with the same binary, because the volume stops keeping up
  with the couple of gigabytes a sequence writes. That row is published beside every run so a reader
  can tell a slow volume from a slow engine. Twelve runs across three sequences were taken and the
  pattern held in all of them.
- **What a statement costs through the shipped API.** `inillucent-fullgate` drives the engine's own
  `plan`, `prepare` and `pipeline` calls. It never calls `Database::open`, never opens a
  `Connection` and never steps a `Statement`, so none of the figures on this page include what an
  application pays for taking the file lock, asking whether another process has written, and giving
  the lock back - which `locking_mode = normal` makes a statement do whenever it is not inside a
  transaction. task-2046 measured that path and found `SELECT 1` costing 132,884 nanoseconds
  outside a transaction against 1,126 inside one, on the same connection over the same file. It is
  10,095 against 727 now. Both readings were taken on a box with two other agents working, minutes
  apart, so the ratio is the claim and the nanoseconds are not.

  No figure on this page moved when that cost was removed, and none would have moved if it had
  doubled instead. `inillucent-prepareperf` is the instrument for this path; its breakdown table
  prints the two columns beside each other on every run.
- **Per workload processor time**, which the gate reports but which is quantised to the Windows
  scheduler tick of 15.625 ms. Only the per round totals on this page should be quoted.

## Reproducing it

```sh
cargo build --release

# the pinned SQLite 3.53.4 oracle
pwsh tools/sqlite-reference.ps1      # Windows
bash tools/sqlite-reference.sh       # Linux

# The fixtures are not checked in, because they are 1.2 MB, 17 MB and 94 MB. Each
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

**Every program that times this engine against SQLite pins itself to one class of core before it
times anything (task-2085).** The machine these figures come from is an Intel Core Ultra 9 285, with
8 performance cores and 16 efficiency cores. With no affinity set, Windows sometimes ran
`inillucent-fullgate` on the efficiency cores and its `sqlite-bench` child on the performance cores,
so the two arms of one round ran on different hardware. On the read families that made this engine's
`scan.group` read 6.31 ms against 2.77 ms pinned, while SQLite's read the same either way.

- The default is `--cores performance`: the processors with the highest `EfficiencyClass` that
  `GetSystemCpuSetInformation` reports on Windows, or the highest `cpu_capacity` or
  `cpuinfo_max_freq` on Linux. `--cores efficiency` takes the other class, and `--cores any` leaves
  the process unpinned so the unpinned figure can still be taken on purpose. A machine with one core
  class is not pinned, and neither is macOS, which has no call for it.
- The `## configuration` block of `inillucent-fullgate`, `inillucent-writegate` and
  `inillucent-searchgate`, and a `## cores` block at the top of every other such program, print
  the class and the mask, for example `performance - 8 of 24 logical processors, mask 0xC03C03`.
  `tests/performance-history.tsv` records the same thing in its `cores` column.
- A child such as `sqlite-bench` or a shell inherits the mask. The launcher reads the child's mask
  back and refuses to time it when it differs, and `crates/inillucent-compat/tests/affinity.rs` fails
  if a child can end up on other processors.
- Pinning makes one workload slower. `read.correlated` uses more than one thread, and task-2064
  measured `correlated.exists` at 59.69 ms on the 8 performance cores, 46.35 ms on the 16 efficiency
  cores and 38.74 ms unpinned on all 24.

The run at the top of this page was taken before task-2085 landed, with the same mask set on the
gate process from outside it. The scripts that did that, and every transcript behind the page, are in
`_agent_output/task-2064-perf/` in the main checkout, which is not checked in.

The gate binaries and the shell install `inillucent-alloc` as their global allocator. It is part of
the build in the same way fat link time optimisation and a single codegen unit are: SQLite ships its
own memory subsystem, so measuring a Rust workspace on the platform allocator measures a build
configuration rather than an engine. It is worth 3.24x to 3.86x on the medium gate.

[Repository](repository.md) covers the rest of the instruments and the test runner.
