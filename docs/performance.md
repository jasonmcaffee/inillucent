# Performance against SQLite

Four measurements decide whether this engine is worth changing to: how long a workload takes, how
much processor it burns, how much memory it holds, and how big the file is. Three are wins and one is
a loss, and all four are here.

**[Feature comparison](feature-comparison.md) carries the full run**, per workload, per family, with
every interval and every control. This page is the summary.

## The four numbers

Measured at 100,000 rows on Windows, over the ten workload families the performance contract weights,
30 paired rounds per run, four consecutive runs, medians of the two middle runs.

**Measured 2026-09-20 on `main`**, which is the engine you get. Every number on this page is that
run.

| | SQLite 3.53.4 | inillucent | |
|---|---|---|---|
| **elapsed time**, weighted over the ten families | the reference | 4.53x the speed | **353% faster** |
| **elapsed time**, the 95% lower bound the gate grades on | | 4.21x | **321% faster**, against a bar asking 200% |
| **processor time**, one round of the whole plan | 1,168 ms | 461 ms | **60% less processor** |
| **peak resident memory**, one round of the whole plan | 37.21 MiB | 40.76 MiB | **9.5% more**, the one loss |
| **the database file**, the same imported fixture | 16,830,464 B | 17,432,576 B | **3.6% larger** |

Every workload's answer is hashed and compared with SQLite's before its timing is allowed to count.
**All 30 workloads agreed on every round of all four runs.**

The four runs read 4.37x, 4.56x, 4.59x and 4.49x, with 95% lower bounds of 3.98x, 4.28x, 4.13x and
4.37x. The bound the contract grades on cleared its 3.00x requirement on all four.

**The processor ratio sits exactly on its bar, and one run of the four is above it.** The contract
asks for at most 0.400 and the median of the two middle runs is 0.400. The four runs read 0.430,
0.381, 0.408 and 0.392, so the first one misses. What moves it is the reference arm rather than this
one: our own processor time is 453, 438, 484 and 469 ms across those runs, a spread of 7%, while
SQLite's is 1,055, 1,148, 1,188 and 1,195 ms, and the run that misses is the run where SQLite used
the least. An earlier sitting read 0.385 on all four.

**What task-2000 moved, measured as a pair in one sitting.** The commit before that work reads 3.55x
weighted with a 3.42x bound and burns 0.635 of SQLite's processor; this run reads 4.53x, 4.21x and
0.400. Both arms are four runs of this protocol on the same machine, minutes apart, because a pair
measured on two different box states is not a pair: the same pinned SQLite binary on the same fixture
reads **2.16x faster on a quiet box than on one that has just run the test suite**, so a before-figure
taken at another time flatters or penalises everything compared against it.

Four families moved and six did not:

| family | before | after | |
|---|---:|---:|---|
| `schema` | 0.65x | **1.36x** | 109% faster than it was |
| `read.analytical` | 5.36x | **10.75x** | 101% |
| `transaction` | 1.23x | **2.37x** | 93% |
| `write` | 1.47x | **2.15x** | 46% |

and inside them, five workloads carry almost all of it: `txn.autocommit` from 0.13x to 0.93x,
`write.insert.autocommit` from 0.47x to 3.02x, `scan.aggregate` from 11.39x to 53.78x, `scan.group`
from 7.94x to 27.59x, and `schema.index` from 0.65x to 1.36x. Every other workload is within 4% of
where it was, which is this gate's run to run noise.

**An earlier sitting read 4.63x, and the difference is the reference arm.** That sitting graded the
commit four changes back and its before arm read 3.51x, which agrees with this sitting's 3.55x. Run
for run, this engine's own nanoseconds are the same or better on every workload: `prepare.trivial`
6.5% quicker, `write.update.indexed` 6.2%, `scan.aggregate` 2.9%, `write.insert.batch` 1.0%, and the
rest inside each workload's own interval in both directions. SQLite's arm is what differs - 2.9%
slower on `scan.sort`, 6.0% slower on `write.update.indexed`, 2.0% quicker on `prepare.trivial` - and
a ratio moves when either half does. The figure published here is the newer sitting because it grades
the commit this page describes.

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
| `read.point` | 16% | one row by rowid, by integer key, and through a secondary index | **2,834% faster** (29.34x) | 27.30x | 2.00x, met |
| `large.values` | 4% | text and blobs across the boundary where a value stops fitting in a leaf | **1,093% faster** (11.93x) | 7.95x | 1.50x, met |
| `read.analytical` | 10% | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | **948% faster** (10.48x) | 8.14x | 5.00x, met |
| `read.range` | 12% | selective ranges, forward and reverse, covering and not | **406% faster** (5.06x) | 3.96x | 3.00x, met |
| `read.join` | 8% | two table and four table joins | **322% faster** (4.22x) | 2.87x | 3.00x, missed on the lower bound |
| `transaction` | 10% | autocommit, small batches, large batches, savepoints | **136% faster** (2.36x) | 1.73x | no slower than SQLite, met |
| `write` | 20% | insert, update, delete, upsert, with and without indexes | **112% faster** (2.12x) | 1.93x | 1.50x, met |
| `extension` | 8% | JSON, FTS5, R-Tree | **57% faster** (1.57x) | 1.35x | 1.50x, missed on the lower bound |
| `open.prepare` | 8% | parse, bind, step one row, reset | **46% faster** (1.46x) | 1.14x | 5.00x, missed |
| `schema` | 4% | `CREATE INDEX` and its backfill | **37% faster** (1.37x) | 1.19x | 3.00x, missed |

**The release condition is that no required family is below the 1.00x floor, and three of the four
runs met it.** On the first run `schema`'s lower bound read 0.68x against a median ratio of 1.36x;
the other three read 1.03x, 1.36x and 1.35x. `schema` is one workload, `schema.index`, run once a
round, so the family has a three-value bootstrap and the widest interval on the page. No other family
went under on any run.

**`read.analytical` meets its bar again, and by a wide margin.** It reads 10.48x with a lower bound of
8.14x against the 5.00x the bar asks of the bound, where the same measurement before task-2000 read
5.29x and 4.67x. Two of its four workloads are what moved: `scan.aggregate` from 11.41x to **52.16x**
and `scan.group` from 7.89x to **27.51x**. The operators answered `count(*)` by calling the
accumulator once a row with a `NULL` argument, and a hundred thousand row scan therefore made a
hundred thousand calls that each compared a discriminant and added one; a batch's live count is one
addition. The other two did not move - `scan.sort` reads 5.09x and `scan.distinct` 1.65x - and
`scan.distinct` is still what holds the bound furthest from the family's ratio.

**`read.join` misses its bar on the lower bound, on all four runs**, at 2.85x, 2.93x, 2.88x and 2.82x
against a 3.00x requirement, with the family reading 4.22x. `join.selective` reads 20.58x and
`join.range` **0.87x**; the range join is the slow half and it is what holds the bound under the
requirement.

A join-only run reads the family much higher - 6.46x, 6.26x, 6.14x and 5.60x, with lower bounds of
4.11x, 4.00x, 4.02x and 3.67x, when the gate was given `--families read.join` on 2026-09-15 - and
that is the measurement this page used to carry. **A family measured on its own is not the same
measurement as the same family inside the whole plan**, because the plan's other twenty-eight
workloads decide what is in the pool when the join runs. The figure in the table above is the
whole-plan one, which is what the contract grades. The bounds before task-1911's chain reuse were
2.97x, 3.00x, 3.00x and 2.99x against the same 3.00x bar, which is what put the family on
[the roadmap](roadmap.md) and what taking it off was measured against.

**`extension` still misses**, at 1.57x with lower bounds of 1.17x, 1.33x, 1.38x and 1.45x against a
1.50x bar. `extension.fts.build` is the worst workload in the family at **0.70x**, 8.07 ms for 500
rows against SQLite's 5.77, and it is what holds the bound down; the rest of the family is clear, with
`extension.rtree.insert` at 1.80x, `extension.rtree.query` at 5.17x and `extension.fts.query` at
1.50x. **Where that workload's time goes is now measured rather than argued about**: building the
index for one document costs 7.8 µs and everything between the `INSERT` and the index costs 8.4 µs,
so **52% of the workload is the SQL and virtual table write path rather than the indexing**. The
stages inside the index are content 1.4 ms, tokenize 0.4 ms, docsize 1.1 ms, group 0.2 ms, terms 0.3
ms, new terms 0.4 ms over 507 terms, dictionary read 0.1 ms and dictionary write 1.7 ms, over five
hundred documents.

An extension-only run on 2026-09-15 read the family 1.58x, 1.60x, 1.59x and 1.67x with lower bounds of
1.40x, 1.39x, 1.39x and 1.45x, with `extension.fts.build` at 0.56x to 0.58x and `extension.fts.query`
at 1.70x to 1.85x - 1.77x on its own, against the 1.43x the reverted segment format left it at. That
run is the measurement [the roadmap](roadmap.md#1-extension-misses-its-bar-on-the-lower-bound) argues
from; the family misses the same way in both, which is why it is still on that list.

**`transaction` is 2.36x from 1.25x, and the reason is what a commit costs.** A commit used to be a
checkpoint: the log was folded into the file and a rollback journal held the pre-image of every page
the fold was about to overwrite, which is six to eight `fsync` class calls a statement. A commit is
now one append to the log and one sync of it, and the fold happens when the log has grown past four
mebibytes, when a caller asks for it, or when the connection closes. Measured on the gate's own
counters, `txn.autocommit`'s hundred statements make **100 log writes, 100 log syncs, no data file
syncs and no folds** - one `fsync` class call a statement, where the same workload used to make 202
of them and write 3,252 KiB of log for 50 KiB of rows.

`txn.autocommit` itself went from 0.13x to **0.94x**, which is 1.27 ms a statement against SQLite's
1.18. Both engines now do exactly one `fsync` a commit, so what is left of the difference is
everything either of them does around that one call, and this device's `fsync` is most of the
millisecond. `txn.batched` reads 3.56x and `txn.large` 3.97x, neither moved.

## The workloads that are slower

Thirty workloads. Twenty three are faster than SQLite. These seven are not, and one of them joined
the list rather than leaving it: `txn.autocommit` was 0.13x before task-2000 and is 0.94x now, which
is a seven fold improvement and still short of parity.

| workload | family | ratio | how much slower | per operation | why |
|---|---|---|---|---|---|
| `prepare.trivial` | `open.prepare` | 0.49x | **104% slower** | 837 ns against 460 | `SELECT 1` compiled on every call. `inillucent-prepareprofile` splits it: 793 ns and **24 allocations**, of which 8 are in the parse, 6 more by the end of planning and 10 in the physical and pipeline stages |
| `extension.fts.build` | `extension` | 0.69x | **45% slower** | 16.2 µs a document against 11.5 | **52% of it is not the index.** Building the index for one document is 7.8 µs and everything between the `INSERT` and the index is 8.4 µs |
| `write.insert.batch` | `write` | about 0.60x | **about 67% slower** | 16.5 µs a row against 10.0 | 2,000 inserts in one transaction into a table carrying two secondary indexes. The two indexes are **69% of it**, measured below. The 0.72x this row carried until task-2029 came from a gate that ran each workload's `pre` on the SQLite arm and not on this one, so the two arms were not doing the same work |
| `join.range` | `read.join` | 0.87x | **15% slower** | 54.9 µs against 48.1 | an index range and a probe per entry, about 210 ns a probe, where SQLite amortises one statement's overhead over two hundred rows |
| `txn.autocommit` | `transaction` | 0.94x | **6% slower** | 1.27 ms a statement against 1.18 | one `fsync` each, and on this device an `fsync` is most of the millisecond. What is left is everything either engine does around that one call |
| `range.lookaside` | `read.range` | 0.97x | **3% slower** | 55.5 µs against 54.2 | the same shape as `join.range`, 200 rowid probes at about 228 ns |
| `extension.json` | `extension` | 0.99x | **1% slower** | 290 ns a call against 288 | the extraction, plus one uncontended mutex and two comparisons a call. The parse of a repeated document and path is already cached |

At the other end of the same table: `scan.aggregate` 52.16x, `point.miss` 51.40x, `large.read`
39.94x, `point.rowid` 30.70x, `scan.group` 27.51x, `join.selective` 20.58x, `point.index` 16.21x,
`range.reverse` 15.73x and `range.covering` 8.43x.

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
page size, for one row that would not fit. So a logical split record, which is roadmap item 2, would
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
area holds at most thirty-two - and the other three roughly double between an 8 KiB page and this
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

What is left of the lever named by [roadmap item 2](roadmap.md) - a compaction that splices its delta
rows in rather than re-encoding every kept row - is the 2.22 ms encode. It cannot also remove the
merge or the sizing pass, because the widths it would have to reproduce exactly are a function of
every value the leaf keeps.

**And the delta area is not where the time is either.** `LeafRef::locate` walks each leaf's unsorted
delta area on every insert, which `docs/roadmap.md` named as the cause. Counted directly at an 8 KiB
page: **8,329 calls, 119,645 entries walked, 5.1 ms**, 14.4 entries a call, against 66.8 ms of apply
time. Under eight per cent, and that is the whole walk - a fingerprint block over it would save less,
because a probe that matches still decodes and the block costs a hash per insert.

**`txn.large` is no longer on this list.** It was the slowest workload on the board at 0.09x, it decided
the `transaction` floor, and it is now **3.97x**, where the median round takes 2.85 ms against
SQLite's 11.70. Two things got it there and only one of them is the engine.

**How a ratio on this page is taken.** A family's or a workload's ratio is the gate's own paired-round
figure, which pairs the two arms round by round and reports the middle of the thirty. The number
printed here is the median of the two middle runs of four. An absolute time printed beside it is the
median of the same four runs' own medians. The two are different summaries of one set of rounds, so
dividing the printed times gives a number close to the printed ratio rather than exactly it: 2.85 and
11.70 divide to 4.10 where the paired figure is 3.97. The paired figure is the one the contract grades
and the one quoted.

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
for the cheapest case: a tombstone, a delta insert, and a compaction every `DELTA_LIMIT` writes
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

**40.76 MiB against SQLite's 37.21, which is 9.5% more.** The contract asks for 5% less, so this bar is
missed, and it is the only headline that is a loss.

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

**The same attribution on today's engine**, which is the one the headline above is taken from. The
file opens at 24.95 MiB with 16.56 of it in the pool, every read workload leaves the mark where it
is, the write family raises it to 30.20, and `schema.index` takes it to its peak:

| workload | peak MiB | rise MiB | pool MiB | everything else MiB |
|---|---:|---:|---:|---:|
| the file opened and the pool warmed | 24.95 | 24.95 | 16.56 | 8.38 |
| every read workload | 25.00 | at most 0.02 | 16.56 | 8.44 |
| `write.insert.batch` | 28.20 | 3.20 | 16.84 | 11.36 |
| `txn.batched` | 30.20 | 0.98 | 16.88 | 11.19 |
| `schema.index` | **40.75** | **10.53** | 22.91 | 11.18 |

Where the remaining 3.7 MiB is:

| | inillucent | SQLite | what it is |
|---|---|---|---|
| the cached database | 16.56 MiB | about 16 MiB | the `.rdb` is 1.036x the `.db`. Under 0.6 MiB left here |
| the process floor | 8.38 MiB | about 4.2 MiB | **3.62 MiB of it is what any Rust binary on this machine costs before the engine exists**, measured rather than inferred: a 130 KB program whose `main` reads its own working set and returns peaks at 3.62 MiB over five runs, 0.66 MiB of it private. The remaining 4.76 MiB is this engine's own code, its statics and opening the file |
| `schema.index`'s rise | 10.53 MiB | about 15.9 MiB | the pages the new index occupies plus the sort's arena. This one is **smaller** than SQLite's, and it is 1.97 MiB smaller than it was before design 2 stopped the build going through the pool |

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
order [the roadmap](roadmap.md#3-the-retrieval-indexs-footprint) had assumed. The store is 620 MiB
resident for 565 MiB on disk - it is not expanded much by loading, it is simply all of it in memory.

**And the peak is reached by that one statement.** Read afresh over four runs, the high-water mark
after every workload:

| workload | peak MiB | this workload added |
|---|---|---|
| the file opened and the pool warmed | 24.95 | 24.95 |
| every read workload, all eleven of them | 25.00 | 0.05 in total |
| `write.insert.batch` | 28.20 | 3.20 |
| the other four write workloads | 29.21 | 1.01 in total |
| `txn.batched` | 30.20 | 0.98 |
| **`schema.index`** | **40.75** | **10.53** |
| every remaining workload | 40.75 | nothing |

The whole plan holds **30.20 MiB** until it builds an index, and building one adds ten and a half - it
added twelve and a half before task-2000's design 2 stopped the build going through the page pool. So the bar is not missed by a buffer that is slightly too big everywhere; it is missed by
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
Linux problem. The Linux arm has not been re-measured since; the Windows headline has moved to 353%
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

- **One scale.** These are the medium fixture, 100,000 rows. At 5,000 rows the headline is 3.46x, and
  at 600,000 it is 5.13x. Families behave differently at each, and `write` inverts: it is **45% slower
  than SQLite** at 5,000 rows (0.69x), 92% faster at 100,000 and **545% faster** at 600,000 (6.45x),
  because a bigger table spreads what a statement costs to set itself up over more of a page.
  `extension` moves the other way: 1.58x at 5,000 rows and 1.51x at 600,000, against 1.30x at 100,000.
  Both of those clear the 1.50x bar's central value, but the gate grades a family on its 95% lower
  bound, and that is 1.33x at both ends, so `extension` reads MISSED at all three scales. The reason
  it is better at the ends than in the middle is the same one: FTS5's build is four ordinary row
  writes per document, and a row write is where the per-statement cost lands.
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
- **Per workload processor time**, which the gate reports but which is quantised to the Windows
  scheduler tick of 15.625 ms. Only the per round totals on this page should be quoted.

## Reproducing it

```sh
cargo build --release

# the pinned SQLite 3.53.4 oracle
pwsh tools/sqlite-reference.ps1      # Windows
bash tools/sqlite-reference.sh       # Linux

# The fixtures are not checked in, because they are 17 MB, 94 MB and 600 MB. Each
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
