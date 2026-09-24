# Performance against SQLite

Four measurements decide whether this engine is worth changing to: how long a workload takes, how
much processor it burns, how much memory it holds, and how big the file is. Three are wins and one is
a loss, and all four are here.

**[Feature comparison](feature-comparison.md) carries the full run**, per workload, per family, with
every interval and every control. This page is the summary.

## The four numbers

Measured at 100,000 rows on Windows, over the ten workload families the performance contract weights,
30 paired rounds per run, four consecutive runs, medians of the two middle runs.

**Measured 2026-09-23 on `main` at `6f84ce6` with `7f93661` applied**, which is `main` as
it stood apart from `3a39c94`, which merged after the run. That commit changes how `%`, `/`
and `||` read the connection's settings, and it rewrote the two selective correlated workloads to
filter with `a.id % 100 = 0` in place of `a.id + 0 > 396`, which keeps the same four rows. Every
number on this page is that run unless a section says otherwise. It was taken in a
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

**This also explains a difference that looked like a regression.** Two gates of
`420e68a` on 2026-09-22 read `scan.aggregate` at 1.80 and 1.76 ms, and later passes of the same
commit on 2026-09-23 read it at 2.96 to 2.98 ms. Those are the two core types. The 2026-09-20 run
this page used to carry read `scan.aggregate` at 1.70 to 1.73 ms against SQLite's 92.5 to 93.8 over
its four runs, which is the performance core figure on both arms, so on that day both processes
landed on the performance cores without being asked. The gates now pin themselves and
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

The write family is most of it: a leaf's delta area is now
sized by the page's free space rather than capped at 32 rows, and a compaction whose rows fit the
page's existing column widths splices them in. `write.insert.batch` went from about 0.60x to
**1.47x**, `write.update.indexed` to 3.37x and `write.delete` to 4.16x. `extension.fts.build` rode
the same change from 0.69x to 0.95x, which is most of `extension`'s move. `open.prepare` is
`prepare.trivial` going from 0.49x to 0.57x after a compile went from 24 allocations to 13.

`schema` moved the other way within its own noise: it is one workload, run once a round, and its
median ratio went from 1.37x to 1.31x while its interval, bootstrapped from three values, is the
widest on the page.

**The processor figure got worse, and most of the reason is four workloads the 2026-09-20 plan did
not have.** It was 0.400 of SQLite's and is 0.500 now. The plan gained the four `read.correlated`
workloads since then, and on this engine's arm they take
182 ms of each round against under half a millisecond on SQLite's; see
[the workloads that are slower](#the-workloads-that-are-slower). The contract does not weight them
into the elapsed time headline, but the processor figure is one round of the whole plan, so they are
in it. The four runs read 0.470, 0.520, 0.500 and 0.500 against a bar of 0.400, so the processor bar
is missed on all four.

### Measured again at `52c4b5f` on 2026-09-24, and not graded

**The figures above are still the latest graded run.** A further run took four full gate passes of
`main` at `52c4b5f`, pinned to the performance cores, 30 rounds each, and the gate refused to grade
every one of them: SQLite's arm ran **6.71% to 7.42% slower** than on the machine's recorded idle
reference, against a limit of 3%. Firefox and WebView were using about 1.2 cores throughout. They
belong to the machine's owner and were not stopped. A busy machine slows SQLite's arm more than this
engine's, so every ratio from these passes is too high, and they read 5.31x to 5.47x weighted. That
is not a new headline and it is not compared with 4.97x anywhere in this documentation.

Against the graded 2026-09-23 run, SQLite's own times in these passes are 2% to 5% slower on most
workloads and this engine's are 2% to 8% faster. Some differences are far larger than a 7% bias can
explain, and they are the reason this section exists:

| workload | graded 2026-09-23 | at `52c4b5f`, not graded | SQLite at `52c4b5f` |
|---|---|---|---|
| `correlated.exists`, 400 outer rows | 59.69 ms | **0.40 ms** | 0.30 ms |
| `correlated.in`, 400 outer rows | 118.19 ms | **0.92 ms** | 0.10 ms |
| `correlated.exists.selective` | 2.11 ms | **19.5 µs** | 23.7 µs |
| `correlated.scalar.selective` | 2.11 ms | **17.9 µs** | 22.3 µs |
| `join.range`, a probe | 57.1 µs | **53.7 µs** | 51.2 µs |
| `range.lookaside`, a probe | 57.4 µs | **52.4 µs** | 57.8 µs |
| processor time, one round of the plan | 555 ms | **367 ms** | 1,102 ms |
| peak resident set, one round of the plan | 40.76 MiB | 40.88 MiB | 37.22 MiB |

- **The correlated subqueries are 99% cheaper.** Every execution of a correlated block used to grow
  a slot array to 100,001 entries, 3.2 MB, and `correlated.exists` made and freed 58 of them each
  time it ran: 45,414 page faults an execution, and 91,390 for `correlated.in`. A later change keeps
  the engine's own parameters in a list of their own, and those faults are now 0. The two selective
  forms are faster than SQLite. `correlated.exists` is 35% slower and `correlated.in` 809% slower,
  where they were 21,332% and 118,020% slower.
- **The processor figure follows from that.** The four correlated workloads took 182 ms of each
  round on 2026-09-23 and take under 2 ms now, so one round of the whole plan is 367 ms of processor
  against SQLite's 1,102: a ratio of 0.335, under the contract's 0.400 bar, where 0.500 missed it. A
  busy machine inflates processor time less than elapsed time, but the pass was not graded, so the
  bar is not claimed as met.
- **`join.range` is 6% cheaper a probe**, from a leaf column being read once and a probe key whose
  affinity changes nothing no longer being copied. It reads 0.95x, within the busy bias of
  1.00x.
- **`range.lookaside` reads 1.10x**, faster than SQLite for the first time, but that is also within
  the bias, so it stays on the list below until a graded pass says otherwise.
- **Memory did not move**, and it is the one figure a busy machine does not bias.

The workloads that depend on the disk moved between the four passes by more than any engine change:
`txn.autocommit` read 1.88x, 1.91x, 0.99x and 1.00x, and `extension.fts.build` 0.63x, 0.69x, 0.99x
and 0.99x, mostly because SQLite's arm of each changed. Nothing is concluded from them here.

The next graded run replaces the figures at the top of this page.

Both engines get the same memory budget: a pool of 4,096 frames of 32 KiB here, 128 MiB, and
`PRAGMA cache_size = -131072` on SQLite's arm, also 128 MiB. Both run under `synchronous = FULL`.
Neither uses a plan cache.

The processor and memory figures are a matched pair: **one child process each**, both opening a
finished file the parent built, both running one round of the same plan. Neither figure is a delta
taken inside a running program.

## By family

`weight` is what the contract gives the family in the headline. `bar` is what the contract asks of
it, expressed as the family's own ratio.

**The two lower bound columns are two different statistics.** `pooled` is what every gate printed
before the gates changed how they compute this: both workloads' rounds in one list, bootstrapped, from the four runs above. That
bound mostly measures how far apart a family's workloads are. `per round` is what the gates print
now: one value a round, the mean of that round's log ratios over the family's workloads, with the
rounds resampled. It is the lowest and highest of four pinned passes taken of `main` at
`16c01a4` with the change applied, on 2026-09-23. [How a family's interval is
computed](#how-a-familys-interval-is-computed) has both statistics side by side, printed
from the same samples.

| family | weight | what it measures | measured | 95% lower bound, pooled | 95% lower bound, per round | bar |
|---|---|---|---|---|---|---|
| `read.point` | 16% | one row by rowid, by integer key, and through a secondary index | **2,885% faster** (29.85x) | 26.75x | 28.43x to 29.26x | 2.00x, met |
| `large.values` | 4% | text and blobs across the boundary where a value stops fitting in a leaf | **1,135% faster** (12.35x) | 9.14x | 9.56x to 12.26x | 1.50x, met |
| `read.analytical` | 10% | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | **971% faster** (10.71x) | 8.45x | 10.39x to 10.81x | 5.00x, met |
| `read.range` | 12% | selective ranges, forward and reverse, covering and not | **402% faster** (5.02x) | 3.95x | 4.79x to 4.99x | 3.00x, met |
| `read.join` | 8% | two table and four table joins | **321% faster** (4.21x) | 2.80x | 4.08x to 4.27x | 3.00x, met on the per round bound, missed on the pooled one |
| `write` | 20% | insert, update, delete, upsert, with and without indexes | **204% faster** (3.04x) | 2.42x | 2.62x to 2.80x | 1.50x, met |
| `transaction` | 10% | autocommit, small batches, large batches, savepoints | **137% faster** (2.37x) | 1.89x | 1.86x to 1.99x | no slower than SQLite, met |
| `extension` | 8% | JSON, FTS5, R-Tree | **73% faster** (1.73x) | 1.55x | 1.54x to 1.67x | 1.50x, met on all four per round passes, by 2.7% at the narrowest |
| `open.prepare` | 8% | parse, bind, step one row, reset | **69% faster** (1.69x) | 1.27x | 1.66x to 1.69x | 5.00x, missed |
| `schema` | 4% | `CREATE INDEX` and its backfill | **31% faster** (1.31x) | 0.94x | 0.85x to 1.30x | 3.00x, missed |

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

**`read.join` meets its 3.00x bar once the family is graded by the per round statistic.** The four
runs above printed pooled lower bounds of 2.73x, 2.83x, 2.78x and 2.90x, with the family reading
4.21x. `join.selective` reads 20.94x and `join.range` **0.84x**, and the pooled bound is set by how
far apart those two are: it can be predicted from the two ratios alone to within 0.07x. Four pinned
passes of `main` with the per round statistic read the bound at 4.27x, 4.08x, 4.18x and 4.19x.
[How a family's interval is computed](#how-a-familys-interval-is-computed) has the change,
and [`read.join`'s bar under the per round statistic](#readjoins-bar-under-the-per-round-statistic)
has the decision to keep the bar at 3.00x, with what the family reads on five builds back to
`57e87b0`. `join.range` is still slower than SQLite, and a pinned measurement back through history found it lost 26% of this
engine's time since `b0ba286`; [Why `read.join` misses its 3.00x bar, and when it last met
it](#why-readjoin-misses-its-300x-bar-and-when-it-last-met-it) has that history, and [Where the
rest of `join.range`'s time went](#where-the-rest-of-joinranges-time-went-after-57e87b0)
names the commits.

A join-only run reads the family much higher - 6.46x, 6.26x, 6.14x and 5.60x, with lower bounds of
4.11x, 4.00x, 4.02x and 3.67x, when the gate was given `--families read.join` on 2026-09-15 - and
that is the measurement this page used to carry. **A family measured on its own is not the same
measurement as the same family inside the whole plan**, because the plan's other workloads decide
what is in the pool when the join runs. The figure in the table above is the whole-plan one, which is
what the contract grades. In that run of the join family alone `join.selective` read 35.49x and `join.range` 1.17x.
The bounds before the chain reuse change were 2.97x, 3.00x, 3.00x and 2.99x against the same 3.00x
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
| `prepare.trivial` | `open.prepare` | 0.57x | **75% slower** | 729 ns against 407 | `SELECT 1` compiled on every call. `inillucent-prepareprofile` counts **13 allocations** on the path the gate times, where it counted 24: one change removed three and a later one eight more. **Nine of the thirteen leave with the compiled statement** - the bound result columns, the column's name, the `Box<BoundSelect>`, the projection expression tree and the output names - so what is left is the compile's answer rather than its scratch |
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

A change made a correlated block 340% faster and a later one stopped answering blocks for rows the
filter throws away, which is why the two selective arms are 2 ms rather than 55. **The table above is
out of date by two orders of magnitude and is kept because it is the last graded run.** At
`52c4b5f`, measured on a machine the gate called busy, the four read 0.40 ms, 0.92 ms, 19.5 µs and
17.9 µs, because a later change stopped each execution building and freeing a 3.2 MB slot array;
[Measured again at `52c4b5f`](#measured-again-at-52c4b5f-on-2026-09-24-and-not-graded) has the
table. The two selective forms are faster than SQLite there, and a correlated `IN` over 400 outer
rows is still 809% slower, so a correlated `IN` against a large table is still worth writing as a
join. **These four are also the one place where the core count changed the answer:**
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
29.60, and it has been split into the four passes it actually is. Medians of five runs,
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
area held at most thirty-two when this was measured (a later change sized it by the free gap instead, and
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
point of a curve. The delta area change added the sweep: `inillucent-writeprofile --sweep` inserts 5,000 rows in
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

| workload | base | base | after | after |
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
a bar of 3.00x. Both were measured again by the change that made a lookup past a leaf's last delta
key cost one comparison, called the fix below, and the next section is what it found.

### The two workloads the format 2 leaf cost, measured again

**Neither regression survives pinning the gate to one kind of core.** The fix, which makes a lookup
past a leaf's last delta key cost one comparison, measured both
again with `inillucent-fullgate` on the medium fixture at a 32 KiB page, 30 rounds, the builds
alternated. The pass that decides it was taken with the gate process pinned to this machine's
performance cores: affinity mask `0xC03C03`, logical processors 0, 1, 10 to 13, 22 and 23 on a Core
Ultra 9 285. SQLite runs as a child of the gate and inherits the mask, and the mask was read back
from the SQLite child on every pass: `0xC03C03` each time. An earlier measurement found why this matters.
Unpinned, the scheduler put this engine on the efficiency cores and SQLite on the performance cores,
so an unpinned pass measures the two engines on different hardware (a later change makes the gates pin
themselves).

The test was written down before the pinned passes ran: the regression is real only if the delta area change's
mean is at least 3% slower than the build before it **and** every one of its passes is slower than
every pass before it. The fix counts only if it is at least 2% faster than the delta area change on the mean
**and** every one of its passes is faster.

Pinned, 14:50 to 15:18:51Z on 2026-09-23 in quiet windows, this engine's time in milliseconds:

| build | `txn.large`, each pass | mean | `join.range`, each pass | mean |
|---|---|---:|---|---:|
| before the delta area change (`420e68a`) | 2.838, 2.803 | 2.821 | 27.55, 27.37 | 27.46 |
| the delta area change (`abf042c`) | 2.845, 2.837, 2.899, 2.827 | 2.852 | 27.46, 27.43, 27.53, 27.59 | 27.50 |
| the fix | 2.726, 2.672, 2.726 | 2.708 | 27.46, 27.25, 27.27 | 27.33 |

- **`txn.large` did not regress.** The delta area change is 1.1% slower on the mean, under the 3% the test asks
  for, and its fastest pass (2.827) is faster than the slowest pass before it (2.838).
- **`join.range` did not regress**, 0.1% on the mean. SQLite's arm read 23.8 to 24.1 ms on every
  pinned pass, and the `read.join` lower bound read 2.80x to 2.86x on all three builds, the build
  before the delta area change included. So the bar is missed, but the delta area change did not cause it. The 3.14x and
  3.04x quoted above for the build before the delta area change were taken when both arms ran on performance cores.
- **The fix is 5.0% faster than the delta area change on `txn.large`**, and its slowest pass is faster than
  the delta area change's fastest.

The rest of the write family, pinned means in milliseconds:

| workload | before the delta area change | the delta area change | the fix |
|---|---:|---:|---:|
| `write.insert.batch` | 24.47 | 14.02 | 13.90 |
| `write.update.indexed` | 33.91 | 20.35 | 20.40 |
| `write.delete` | 20.49 | 16.08 | 16.28 |

The delta area change's gain is intact. `write.delete` reads 1.3% slower with the fix, from three passes against
four, and every pass of each build is inside 15.8 to 16.4 ms.

**What the fix is.** Counted on the `write` and `transaction` families alone, which put the tables
in the state `txn.large` meets, all but a few of its 2,000 statements either update in place or
match nothing. The in-place update's code did not change in the delta area change, so the carve was not the
cause. `Bind::Scatter` picks rowids up to `main_table`'s row count and `side_table` holds a quarter
of that, so three in four of the updates look up a rowid past the end of `side_table` and land in
its last leaf. `write.insert.autocommit` appended 100 rows to that leaf earlier in the round. With
the 32 row limit they were packed every 32; since that change they stay in the delta area until the
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
different absolute times: `txn.large` 3.12 and 3.20 ms before the delta area change in the two windows, 3.20
and 3.19 with the delta area change, and 3.04 with
the fix; `join.range` 33.0 to 33.6 ms on every build while SQLite's arm drifted from 24.8 to 31.6 ms
and pulled the `read.join` lower bound between 2.57x and 3.16x. Two of the pinned passes of the
build before the delta area change exited at once, because the worktree it was built in had lost its link to
the SQLite oracle; they are not counted. The gate refuses without the oracle, but only a pass that
takes two seconds instead of two and a half minutes shows it.

### Why `read.join` misses its 3.00x bar, and when it last met it

A further measurement answered this with 60 pinned passes on 2026-09-23. Every pass ran with the gate process
pinned to affinity mask `0xC03C03` from outside, which is the only way to pin the builds from before
the gates could pin themselves, and the mask was read back from the SQLite child on every pass. It read `0xC03C03` all 60
times. Builds from before that change cannot pin themselves, so HEAD was pinned the same way, from
outside, rather than left to pin itself. Passes 1 to 3 ran while one test target of another ticket
was running at 6 to 9% box load. Every other pass ran on an empty box.

**Two things are true, and neither one explains the miss by itself.**

**1. The 3.00x bar was never measured.** It is the "low estimate" column of the performance
contract in `tasks/task-1816-rearchitecture-tdd.md`, a table headed "Targets are estimates unless
marked measured", written when `read.join` read 0.154x. `inillucent-readgate` first graded it at
`57e87b0`, which read the family at 4.87x with a lower bound of 3.32x.
`inillucent-fullgate` carried it over unchanged.

**2. The family's lower bound mostly measures how far apart its two workloads are.** Every gate
puts all 60 rounds of both workloads into one list and bootstraps the mean of that list. A resample
draws the two workloads in random proportions. `join.selective` reads about 21x and `join.range`
about 0.87x, so the proportion moves the mean far more than timing noise does. On HEAD each workload
is measured to within 2%: `join.selective` 20.30x to 22.14x and `join.range` 0.85x to 0.89x across
the pinned passes. The family interval printed beside them is 2.76x to 6.67x. The bound can be
predicted from the two workload ratios alone, as exp(m - 1.96 x (d / 2) / sqrt(60)), where m is the
mean of the two log ratios and d is the gap between them. That prediction matches the printed bound
to within 0.07x on every clean pass, at every build. For the bound to reach 3.00x,
0.3735 x ln(`join.selective`) + 0.6265 x ln(`join.range`) has to reach ln 3.

Resampling rounds instead, with each round's value the mean of that round's two log ratios (the
statistic `weighted_headline` already uses for every family), reads 4.15x [4.11x, 4.21x] and 4.17x
[4.16x, 4.25x] on two pinned passes of HEAD. A later change made that the statistic every gate uses; see
[How a family's interval is computed](#how-a-familys-interval-is-computed).

**3. `join.range` did lose time, 26% of this engine's time, in four steps.** On the read gate's plan,
pinned, this engine in milliseconds, forward sweep / reverse sweep, with SQLite's arm at 23.9 to
24.6 ms on every pass:

| build | `join.range` | |
|---|---|---|
| `57e87b0` | 22.21, 23.00, 22.39, 22.00 | the build the bar was first met on |
| `b0ba286` | 22.37 / 22.62 | |
| `ea03335` | 23.44 / 23.36 | **+1.0 ms** somewhere in the 15 commits before it |
| `3322436`, `1334b80`, `59ccf91` | 23.27 to 23.72 | |
| `71d014a` | 24.12 / 24.15 | **+0.8 ms** somewhere in the 30 commits before it |
| `566c688`, the narrow integer slot change | 24.23 / 24.01 | |
| `a8f45b1`, five follow-up changes | 27.03 / 26.77 | **+2.8 ms in this one commit** |
| `f9e2374` | 25.86 / 26.05 | 1.0 ms back, somewhere between |
| `b3ad244` to `a07036b` | 26.07 to 26.53 | |
| `dcc65f2` | 27.37 / 27.52 | **+1.0 ms** somewhere in the 25 commits before it |
| `36939dd`, `5353eb4` | 27.03 to 27.59 | |
| `d389021` | 27.96 / 27.88 | **+0.9 ms** somewhere in the 25 first parent commits before it (41 with the merged branches) |
| HEAD | 27.88 / 28.00 | |

The two sweeps agree at every build to within 0.4 ms. The full gate's plan shows the same drift:
25.61 and 25.41 ms at `b0ba286`, 27.55 and 27.38 at `f9e2374`, and 28.08 to 28.43 on HEAD.

**So the bar was last met on the read gate's plan, at `57e87b0` and `b0ba286`**, where `join.range`
read 1.04x to 1.10x and the pooled bound 3.03x to 3.28x. That is only just over the bar. **On the
full plan, which is the one the contract grades, no build ever met it when pinned.** `b0ba286` read
2.20x and 2.38x, because `join.selective` was 11x at the time. The ticket asked whether the join path
lost something or whether the bar was set from a flattering measurement. The join path did lose 5.7
ms. The bar was not set from any measurement at all, and the statistic it is graded by would have
missed it on the full plan even with that 5.7 ms back. Each of these is now its own ticket.

#### What the +2.8 ms at `a8f45b1` is made of

`a8f45b1` did five things, and three of them have a switch: the frame of reference is the constant
`FRAME_OF_REFERENCE`, the `(u16, u16)` heap pair is `heap_slot_width`, and `panic = "abort"` with
`strip = true` is the release profile, which `CARGO_PROFILE_RELEASE_PANIC` and
`CARGO_PROFILE_RELEASE_STRIP` override. A later measurement built `inillucent-readgate` at `a8f45b1` with each
switched off, with all three off together, and with the profile alone changed, plus `566c688` with
and without the new profile and HEAD (`40ca955`) with and without `panic = "unwind"`. 39 passes in
two quiet windows on 2026-09-23 and 24, medium fixture, 30 rounds, every pass pinned to `0xC03C03`
from outside with the SQLite child's mask read back as `0xC03C03` every time. SQLite's arm read 23.7
to 24.6 ms on every pass after the box had settled. This engine's `join.range`, in milliseconds,
sweeps in the order run (the second of each window reversed):

| build | window 1 | window 2 |
|---|---|---|
| `a8f45b1` | 27.35, 26.52, 26.67 | 27.04, 26.66, 26.63 |
| `a8f45b1`, frame of reference off | 27.27, 26.59, 26.63 | |
| `a8f45b1`, `(u16, u16)` heap pair off | 26.93, 26.69, 26.71 | |
| `a8f45b1`, `panic = "unwind"`, `strip = false` | 25.64, 25.58, 25.45 | 25.58, 25.80, 26.51 |
| `a8f45b1`, `panic = "unwind"`, `strip` kept | | 26.36, 25.58, 25.56 |
| `a8f45b1`, all three off | | 25.32, 25.53, 25.34 |
| `566c688` | 24.28, 23.97, 24.07 | 24.34, 24.24, 24.15 |
| `566c688`, `panic = "abort"`, `strip = true` | | 25.34, 25.41, 25.25 |
| HEAD | | 28.07, 28.18, 28.27 |
| HEAD, `panic = "unwind"` | | 27.91, 28.73, 28.70 |

What each difference was required to be, before the passes ran, and what it was:

- **The two on-disk formats cost nothing.** Frame of reference off moved `join.range` by -0.07 to
  +0.08 ms and the narrow heap pair off by -0.17 to +0.42 ms, against a 0.5 ms threshold for "not the
  cause". They are what took the imported file from 573 pages to 532, and the smaller file is kept.
- **`panic = "abort"` cost about 1.1 ms at `a8f45b1`, and costs nothing at HEAD.** Adding it to
  `566c688` alone costs 1.00, 1.17 and 1.10 ms, so it does not depend on the new code. `strip` has no
  measurable part in it. At HEAD, `panic = "unwind"` reads +0.16, -0.55 and -0.43 ms against
  `panic = "abort"`, under the 0.4 ms threshold in every sweep, so what abort did at `a8f45b1` was a
  code generation effect that later commits no longer show, and nothing is recovered by removing
  it. `panic = "abort"` stays for the reason `a8f45b1` gave, the smaller binary and floor. The 1.0 ms
  that came back by `f9e2374` in the table above is probably this effect going away, which was not
  measured.
- **The read code follow-ups 4 and 5 added cost about 1.2 ms.** `a8f45b1` with all three switched
  off imports the same 573 pages as `566c688` and is still 0.98, 1.29 and 1.19 ms slower, over the
  0.8 ms threshold in every sweep. The format switches revert what is written and not the code that
  reads it: a `base == 0` test in every integer read, a width match in every heap slot read, a
  `Vector` eight bytes wider, and a fourth lookup of the column directory entry on every
  `LeafRef::column` call. Follow-up 1 is not in it, because `make_room` never runs in the read gate,
  whose plan keeps only the read families and none of them writes. Follow-up 3 only changes the
  memory `CREATE INDEX` holds.

So the step is about 1.1 ms of code generation that has since gone and about 1.2 ms of read path
code that was still in HEAD's shape.

**What was recovered.** `LeafRef::column` runs on every probe, once per key column the leaf search
compares and once per inner column an index nested loop projects, and it located and bounds checked
the same directory entry four times: `spec`, `column_width`, `column_base` and `column_offset`, the
type byte parsed twice between them. It now reads the entry once as one slice. A third quiet window
timed HEAD (`40ca955`) against HEAD with only that change, alternated head, fix, fix, head, head,
fix, fix, head, both pinned from outside to `0xC03C03` with the SQLite child's mask read back, and
every workload's digest agreeing with SQLite's on all eight passes:

| | HEAD | HEAD, entry read once |
|---|---|---|
| `join.range`, ms | 28.43, 28.35, 28.53, 28.14 | 26.18, 26.16, 25.98, 26.11 |
| `range.lookaside`, ms (passes 204 and 206) | 28.33, 0.94x | 24.52, 1.11x |
| `point.index` (the same passes) | 16.14x | 18.53x |
| `join.selective` (the same passes) | 20.52x | 23.11x |
| PointProbe, warm (the same passes) | 344.5 ns | 306.9 ns |
| `read.join` family (the same passes) | 4.19x [4.13x, 4.25x] | 4.67x [4.61x, 4.73x] |

`join.range` is 2.04 to 2.55 ms faster in every adjacent pair, against a 0.5 ms threshold set before
the passes ran. That is more than the 1.2 ms `a8f45b1`'s read code cost. This paragraph used to
give a reason that had not been measured, that later code calls `column` more often per probe.
A further measurement counted the calls and it is not true: one `join.selective` and one `join.range` make 512
`column` calls at every build from `566c688` to HEAD. What changed is what each call cost, which is
in the next section. `join.range` is at 0.92x after it, still slower than SQLite, and 3.9 ms slower
than it was at `57e87b0`; the next section accounts for the rest. `range.lookaside` is faster than
SQLite for the first time on this plan.

#### Where the rest of `join.range`'s time went after `57e87b0`

That measurement walked the whole history again with the column read fix applied, because a build without
it pays the old four lookups in `LeafRef::column` and a build with it does not. Measured with the
change on one side only, a step appears wherever the change landed rather than where the time went.

**How.** `inillucent-readgate`, medium fixture, 30 rounds, one pass per build per sweep, pinned from
outside to `0xC03C03` with the SQLite child's mask read back as `0xC03C03` on every pass. 83 builds
in two quiet windows on 2026-09-24: 108 passes from 03:12 to 03:54Z and 101 from 06:06 to 06:44Z,
each build once forward and once reversed. The change was applied as `column-once.patch` from
`f9e2374` on and as the same single read of the entry, by text replacement, to the older readers
(`apply-fix.js` in the evidence folder). A build called "fixed" below has it. A pass counts only if
SQLite's arm read 26.5 ms or less in it. That rule was set after window 1's passes 19 to 30 read
26.6 to 32.1 ms on SQLite's arm while everything else read 24.7 to 26.5, and it refused 28 of 108
passes in window 1 and 18 of 101 in window 2. Window 2 was the noisier of the two, so its figures
are also given as this engine's time over SQLite's in the same pass, which the gate's interleaving
keeps comparable under load. Both windows ran with D: nearly full; every pass read and wrote only on
C:.

**The whole span, fixed at every build, in milliseconds** (window 1 means unless marked):

| from | to | step | where |
|---|---|---|---|
| `57e87b0` 22.73 | `b0ba286` 22.71 | 0 | |
| `b0ba286` 22.71 | `ea03335` 23.76 | **+1.05** | spread over `34e026e` and `9d3d84d`, not resolved |
| `59ccf91` 23.88 | `71d014a` 24.44 | **+0.56** | between `c401bb2` and `71d014a` |
| `566c688` 24.06 | `a8f45b1` 26.21 | **+2.15** | `a8f45b1` itself (window 2, reverse sweep) |
| `a8f45b1` | `a07036b` 26.58 | about 0 | |
| `a07036b` 26.58 | `dcc65f2` 26.26 | -0.32 | |
| `5353eb4` 26.43 | `d389021` 27.30 | **+0.87** | `6f84ce6` |
| `d389021` 27.30 | HEAD 27.17 | -0.13 | |

HEAD (`01f37bb`) read 27.17 ms tonight against 22.73 for `57e87b0` fixed. The box read about 3%
slower than on the earlier measurement's night, so these figures are comparable with each other and not with the
tables above.

**`dcc65f2` cost `column` calls that cost more, not more `column` calls, and HEAD does not pay
them.** Without the change, `a07036b` read 26.95 ms and `dcc65f2` 28.37, +1.42. With it, 26.58 and
26.26. What the change saves grew from 0.37 ms to 2.11 ms a round. A round runs `join.range` 500
times with at most 512 `column` calls each, so that is at least 1.4 ns a call at `a07036b` and at
least 8 ns at `dcc65f2`. The number of calls did not move: a build with a counter in `column`, run with
`--families read.join --rounds 1 --repeat N`, makes 512 calls per `join.selective` and `join.range`
pair at `566c688`, `59ccf91`, `71d014a`, `a07036b`, `dcc65f2`, `5353eb4`, `d389021` and HEAD alike.
The same code became more expensive to call between those two commits, which is what a change in how
`column` is compiled and inlined looks like. The column read fix removes it, so nothing is left in HEAD.

**`d389021`'s step is `6f84ce6`, and it is still in HEAD.** The change saves the same amount before
and after the step: 2.09 ms at `5353eb4` and 2.13 at `d389021`. With the change at every build of
the 21 that touch the engine between `5353eb4` and `d389021`, the one step over 0.5 ms in both
sweeps is from `6833582` to `6f84ce6`: 26.38 to 27.07 ms reversed, and 27.37 to 28.81 forward. As a
ratio to SQLite's arm it is 1.013 to 1.072, and every build after it stays there. `6f84ce6` is
the change where an index seek converts its key the way SQLite does. It fixed four wrong answers, and it
put more work on the path every probe of an index nested loop takes: the probe key goes through the
comparison's affinity rather than a `CAST`, each key position asks whether it is unconverted, and the
seek checks the key and its bounds for NULL before it searches. That is about 0.7 to 1.4 ms a round,
7 to 14 ns a probe over the 100,500 probes a round makes. Nothing measured says which of those it
is.

**`a8f45b1`'s step is still about 2.2 ms with the change applied at both ends.** The earlier ablation found the
step made of about 1.1 ms of `panic = "abort"` code generation and about 1.2 ms of read code, and
measured the abort part at nothing at HEAD. With the lookup removed from both builds, what is left of
the read code is the `base == 0` test in every integer read, the width match in every heap slot read
and the wider `Vector`. In window 2 the step is 24.06 to 26.21 ms
reversed; the forward pass of `566c688` was refused, and as a ratio to SQLite's arm the step is
0.940 to 1.043 over both sweeps. The `566c688..a07036b` walk, fixed at a build every 19 commits,
found no other step: no adjacent pair after `a8f45b1` moved 0.5 ms in both sweeps. Its builds read
from about 0.8 ms below `a8f45b1` (`03fdb42`, `4e8a78f`) to 0.9 ms above it (`a07036b` reversed), so
this walk cannot say whether the abort part went away later and something else came in, or neither.

**The two early steps.** `b0ba286` to `ea03335` is +1.05 ms fixed and +1.29 unfixed, so it is not
`column`. Over four passes each, `356ef19` reads 22.99 to 23.14, `34e026e` 23.00 to 23.61,
`9d3d84d` 23.64 to 24.16 and `d5ea139` 23.61 to 24.34, so about 0.7 ms of it is spread over
`34e026e` ("a shadow read copies the row once rather than twice") and `9d3d84d` ("a wide value is
spilled, not repacked around") and neither holds it alone. `59ccf91` to `71d014a` is +0.56 ms fixed;
window 2 put `c401bb2` to `71d014a` at +0.35 and +0.54 fixed, and the change saves about the same
at both, so this step is not `column` either, which corrects what window 1 suggested. `71d014a` made
the leaf search's three comparisons check a descending direction. Built without that check it reads
24.96 and 24.95 ms against 24.93 and 24.60 with it, so the check costs nothing. The three commits
between `c401bb2` and `71d014a` (`6e19c0b`, `81855a7`, `0f24df5`) do not compile at their own
commit: `inillucent-vm` has a type error in each, so the step cannot be placed more finely than
those four commits. `81855a7` and `0f24df5` change `inillucent-pool`'s `pool.rs` for the rollback
journal and file locking, which is on every page fetch.

**So of HEAD's 4.4 ms over `57e87b0` tonight**, about 2.2 ms is `a8f45b1`, about 0.9 ms is
`6f84ce6`, about 1.05 ms is `34e026e` and `9d3d84d` and about 0.5 ms is `c401bb2..71d014a`. The
smaller steps between them add or remove a few tenths each.

#### Two of those steps, ablated at `52c4b5f`

`6f84ce6`'s per probe cost is the probe key's affinity. Since that commit a nested loop's key passes
through `ApplyAffinity` once per probe, and it copied the key into an owned `Value` and back even
when nothing changed, which for `join.range` is every probe: an integer key into an INTEGER index.
The key had been wrapped in a `Cast` the same way before, so the conversion itself was not new.
`ApplyAffinity` now returns a value that `apply_affinity` would leave alone unchanged. The other
things `6f84ce6` added run once per execution or once per chain build, not once per probe: the NULL
checks in `span_bounds` and the `unconverted` lookups in `nested_key`.

Four builds of the read gate, medium fixture, 30 rounds, pinned to `0xC03C03`, in the order v0 v1
v2 v3 v3 v2 v1 v0 v0 v1 v2 v3. **Not a settled machine**: Firefox and WebView held about 2.2 cores
throughout, and every pass read a SQLite speed index of 9.3% to 11.2%, so the gate graded none of
them. What follows compares this engine's builds against each other in one window, with the order
reversed, and not against SQLite.

| build | `join.range`, ms a round | mean |
|---|---|---|
| v0: HEAD's probe | 26.94, 26.88, 27.16 | 26.99 |
| v1: `ApplyAffinity` returns an unchanged value | 26.69, 26.89, 26.71 | 26.76 |
| v2: v1 with the frame of reference and the four byte heap pair both off | 26.16, 25.92, 26.15 | 26.08 |
| v3: v2 without the `base == 0` test and the width match | 26.52, 26.43, 26.87 | 26.60 |

- The affinity change is worth about 0.23 ms a round, 2.3 ns a probe, and the two builds' passes do
  not overlap. That is part of the 0.7 to 1.4 ms the full history walk put on `6f84ce6`. Nothing else it added
  is on the per probe path, so the rest is not placed.
- The two read branches cost nothing measurable: v3 is no faster than v2.
- Turning the two formats off saved about 0.7 ms here, where the full history walk found it recovered nothing.
  That is a change to what is written on disk, traded against file size, and it is not made here.
  This one comparison needs repeating on a settled machine before it decides anything.

### How a family's interval is computed

**Every gate grades a family on one value a round now.** That value is the mean of that round's log
ratios over the family's workloads, and the bootstrap resamples the rounds. The headline has always
treated each family this way, in `weighted_headline`. `perf::family_interval` is the one function
that computes it, and `inillucent-fullgate`, `inillucent-readgate`, `inillucent-writegate`,
`inillucent-scorecard`, `inillucent-analytical` and `inillucent-prepareperf` all call it. A round in
which any workload of the family has no usable time is left out whole. Keeping the other workloads'
values would give that one round a different mix of workloads.

**Before a later change, the gates put every workload's every round into one list and bootstrapped that
list.** A resample of that list draws the workloads in random proportions. When a family's workloads
are far apart, the proportion moves the mean much more than timing noise does, so the interval
measured the distance between the workloads. `read.join` is the clearest case: `join.selective`
reads about 21x and `join.range` about 0.86x, and the pooled interval printed beside them was about
2.8x to 6.5x. The family's composition is fixed by the plan, so the proportion of each workload is
not a random quantity.

**How it was measured.** 22 passes on 2026-09-23, 20:21:56 to 20:52:09Z, in a quiet window with the
other two agents in this repository paused and their process lists empty. Every pass was started by
a pinned walk script that sets affinity mask
`0xC03C03` on the gate process and reads the mask back from the SQLite child. The child read
`0xC03C03` on all 22 passes, and the passes of `main` also printed that mask in their own
configuration. Medium fixture, 30 rounds, the full gate at a 32 KiB page. `main` was `16c01a4` with
this change applied, built so that each family line is followed by the pooled figure and the per
round figure **computed from the same samples**. The four older builds were built at their own
commit with one extra line that prints the per round figure; the family line they already print is
the pooled figure. Builds alternated, forward and then reverse.

**The full gate on `main`, every family, four passes.** Each cell has the four passes in order.

| family | bar | family ratio | pooled lower bound | met | per round lower bound | met | per round width over pooled width |
|---|---|---|---|---|---|---|---|
| `open.prepare` | 5.00x | 1.71, 1.69, 1.69, 1.71 | 1.28, 1.27, 1.27, 1.30 | 0 of 4 | 1.68, 1.67, 1.66, 1.69 | 0 of 4 | 0.05 to 0.07 |
| `read.point` | 2.00x | 29.63, 29.66, 29.02, 29.21 | 26.76, 26.92, 26.37, 26.52 | 4 of 4 | 28.62, 29.26, 28.60, 28.43 | 4 of 4 | 0.13 to 0.30 |
| `read.range` | 3.00x | 4.97, 4.91, 4.97, 5.04 | 3.87, 3.85, 3.90, 3.94 | 4 of 4 | 4.86, 4.79, 4.91, 4.99 | 4 of 4 | 0.04 to 0.11 |
| `read.join` | 3.00x | 4.37, 4.15, 4.22, 4.23 | 2.90, 2.75, 2.78, 2.81 | **0 of 4** | 4.27, 4.08, 4.18, 4.19 | **4 of 4** | 0.02 to 0.05 |
| `read.analytical` | 5.00x | 10.95, 10.56, 10.56, 10.70 | 8.54, 8.26, 8.24, 8.39 | 4 of 4 | 10.81, 10.39, 10.48, 10.62 | 4 of 4 | 0.03 to 0.06 |
| `write` | 1.50x | 3.22, 3.20, 3.20, 3.29 | 2.81, 2.79, 2.72, 2.80 | 4 of 4 | 2.75, 2.80, 2.62, 2.72 | 4 of 4 | 0.95 to 1.20 |
| `transaction` | 1.00x | 2.26, 2.33, 2.21, 2.21 | 1.88, 1.93, 1.82, 1.78 | 4 of 4 | 1.93, 1.99, 1.98, 1.86 | 4 of 4 | 0.52 to 0.83 |
| `schema` | 3.00x | 1.19, 1.44, 1.48, 1.19 | 0.94, 1.30, 1.30, 0.85 | 0 of 4 | 0.94, 1.30, 1.30, 0.85 | 0 of 4 | 1.00 |
| `extension` | 1.50x | 1.63, 1.70, 1.67, 1.63 | 1.45, 1.55, 1.51, 1.47 | **2 of 4** | 1.54, 1.67, 1.60, 1.54 | **4 of 4** | 0.14 to 0.46 |
| `large.values` | 1.50x | 13.81, 12.10, 10.93, 12.84 | 9.92, 8.78, 7.60, 9.22 | 4 of 4 | 12.26, 11.79, 9.56, 11.43 | 4 of 4 | 0.08 to 0.47 |

The read gate on `main`, two passes, reads `read.join` at 2.86x and 2.81x pooled against 4.19x and
4.06x per round, and meets every other read family's bar under both statistics. The write gate, two
passes, meets both of its bars under both.

**The families that pass only because of the change.** On `main`: **`read.join`**, on all six passes
of the two gates that grade it, and **`extension`**, on two of the four full gate passes, where the
pooled bound read 1.45x and 1.47x and the per round bound 1.54x on both. On the older builds:
`read.analytical` at `f9e2374` on all four of its passes and at `b0ba286` on both of its full gate
passes, and `read.range` at `b0ba286` on one full gate pass of two (2.99x pooled, 3.61x per round).
**No family goes from met to missed on any pass of any build.** No family is on a different side of
the 1.00x floor under the two statistics on any pass: `schema` is under it on two passes of `main`,
and `schema` has one workload, so both statistics give it the same interval.

**`write` is the one family whose bound goes down.** Its per round interval is 0.95 to 1.20 times
as wide as the pooled one, and its lower bound is lower on three passes of four (2.75x against 2.81x,
2.62x against 2.72x, 2.72x against 2.80x). A per round mean can only vary more than the pooled list
implies when the family's workloads are slow in the same rounds and fast in the same rounds. The
pooled list treats every value as independent, so for `write` it claimed more precision than five
workloads that move together have.

#### What was written down before the passes ran, and what happened to it

These five tests were posted before any pinned pass, with the statement that a failure
of any of them would reject the per round statistic.

1. **The two statistics agree on the centre.** With every workload running the same rounds, the per
   round mean is the pooled mean. Held: the read gate prints the per round centre in its family line,
   and on the two passes of `main` it matches the pooled mean to the second decimal on all eight
   family readings.
2. **A family of one workload does not change.** Held: `schema`'s two intervals are identical on
   all ten full gate passes.
3. **The width is timing noise, not the distance between workloads.** For a family of k workloads
   with log half widths h, the per round log half width has to fall between 0.5 x sqrt(sum of h
   squared) / k and 1.5 x the largest h. Held for every family on every one of the 22 passes.
   For `read.join` the per round log half width is 0.010 to 0.057, and the pooled one is 0.31 to 0.42.
4. **The per round bound orders builds the way the family ratio does.** Held on the full plan, where
   the builds are `b0ba286` < `a8f45b1` < `main` < `f9e2374` by both. On the read gate's plan it held
   for `57e87b0` at the top and `main` at the bottom. The three builds between them have family ratios
   within 1.5% of each other (4.51x, 4.49x and 4.56x, the mean of two passes) and per round bounds of
   4.385x, 4.38x and 4.38x, which does not order them. The pooled bound puts `b0ba286` (3.09x) above
   `f9e2374` (3.005x), the reverse of their family ratios, because `b0ba286`'s `join.range` was faster
   and its two workloads were closer together.
5. **Repeat passes of one build give closer per round lower bounds than pooled ones.** **This
   failed**: the per round bound varied more between passes on 33 of 62 groups of one build, one
   gate and one family.

**Why test 5 was the wrong test, and why the statistic is kept.** The pooled lower bound is the
family ratio minus a width set by the distance between the workloads. That distance does not change
between passes, so the pooled bound moves only when the family ratio moves. The per round bound
moves when the family ratio moves and again when the noise in that pass moves. Test 5 therefore
rewarded the statistic whose width does not respond to noise, which is the property test 3 was
written to reject. The pooled bound fails test 3 by a factor of ten on `read.join`, so keeping it is
not an option. A third statistic would have to pass test 5 by being wider than the noise in a single
pass, and the next paragraph is what that would have to cover.

**A single pass's interval does not cover the next pass.** On the four full gate passes of `main`,
the family ratio moved between passes by more than the per round half width in eight of the ten
families. `read.join` read 4.15x to 4.37x, 5.2% apart, with a per round half width of about 1.5%,
so pass 20's interval [4.08x, 4.21x] does not contain pass 1's ratio of 4.37x. Something that is
constant within a pass and different between passes moves every round of it together, and no
statistic computed inside one pass can see it. The per round interval states how precisely one
pass measured the family. It does not state what the next pass will read. **So a family whose per
round bound is within about 5% of its bar has not settled its verdict in one pass.** `extension` is
that family today: 1.54x at its lowest against 1.50x. The published figures on this page are four
runs for this reason, and grading from more than one pass is what the next section covers.

### What moves a family between passes

This section set out to find what is constant within a pass and different between passes before
deciding how many passes a verdict needs. The tests were posted before any pass ran.

**Two options on `inillucent-fullgate` made the measurement possible. Both are off by default, so no
published number moves.** `--samples <file>` appends every round's raw time for both arms and every
workload, and each round's process costs, including a page fault count that `ProcessCost` now
carries. `--engine-child` runs this engine's arm of each round in a new child process, which opens
the `.rdb` the parent built. That is how the SQLite arm has always run. The configuration block says
which of the two the run used.

**The SQLite arm is a measure of how fast the box was.** It is the same `sqlite-bench.exe` on every
pass, whichever build is under test. So its time over the read workloads, against its fastest pass,
says how fast the machine was during that pass. In the family statistic window it read +5% to +7% on the first
four passes, +10% to +15% on the read gate passes that ran right after the two write gate passes,
and +0.2% to +1.1% on the last four. The pass that put `read.join` at 4.37x was the first pass of
that window. Without it, `read.join`'s four passes of `main` span 1.7% instead of 5.2%.

**Twelve passes in one settled window, 2026-09-24 00:43:28 to 01:12:07Z.** The other agents were
paused and their process lists were checked empty before the window opened. Medium fixture, 30
rounds, mask `0xC03C03` in each pass's own configuration, every workload agreed on every pass. The
order was A B B A A B B A A B B A, where A is the default and B is `--engine-child`, so a drift
across the window cannot appear as a difference between A and B. The SQLite arm's speed stayed
within 0.4% to 1.4% of its fastest pass for all 29 minutes, so this window had none of the slowdown
the family statistic window had.

| family | A pass centres, 6 passes | B pass centres, 6 passes | one pass's interval contains another pass's ratio, A | the same, B |
|---|---|---|---|---|
| `open.prepare` | 1.67 to 1.72 | 1.65 to 1.67 | 87% | 93% |
| `read.point` | 28.97 to 30.15 | 29.50 to 29.76 | **57%** | 90% |
| `read.range` | 4.96 to 5.07 | 4.97 to 5.04 | 77% | 87% |
| `read.join` | 4.19 to 4.27 | 4.23 to 4.28 | **67%** | 100% |
| `read.analytical` | 10.62 to 10.77 | 10.51 to 10.73 | 80% | **57%** |
| `write` | 2.89 to 3.49 | 2.90 to 3.49 | 100% | 100% |
| `transaction` | 2.04 to 2.38 | 2.04 to 2.29 | 100% | 97% |
| `schema` | 1.32 to 1.63 | 0.90 to 1.20 | 70% | 100% |
| `extension` | 1.68 to 1.79 | 1.52 to 1.76 | 80% | 83% |
| `large.values` | 10.63 to 12.78 | 9.87 to 12.79 | 73% | 70% |

The centres are the per round statistic's geometric means from the raw samples. The two coverage
columns compare each pass's printed ratio with every other pass's printed interval, 30 pairs a mode.
**The target for that column is about 83%, not 95%.** Two passes that differ only by round noise
each carry that noise, so the difference between them has √2 times the spread of one, and a 95%
interval around one contains the other about 83% of the time. A figure well under 83% means a
component that stays the same through a pass and changes between passes.

**What the passes showed, including the parts that went against the hypothesis.**

1. **In the default mode it is this engine's arm that moves between passes, not SQLite's.** On all
   four read families our arm's pass centres varied more than SQLite's: log standard deviation
   0.0110 against 0.0033 for `read.point`, 0.0068 against 0.0057 for `read.range`, 0.0047 against
   0.0033 for `read.analytical`, and 0.0070 against 0.0068 for `read.join`. The hypothesis written
   before the passes said the SQLite arm would carry the shift. It did not in this window. The
   movement is in short workloads: across the six A passes `point.rowid` spans 6.9%, `point.miss`
   6.4%, `extension.rtree.query` 5.3%, `scan.distinct` 3.9%, `range.reverse` 3.3% and
   `join.selective` 2.4%.
2. **Starting our arm in a new process every round removes most of that.** Pooled over the ten
   families, the between pass standard deviation of the family log ratio fell from 0.0119 in A to
   0.0020 in B. On the read families that is a real reduction, because the noise inside a pass
   barely changed (`read.join` 0.0327 in A, 0.0363 in B). Per workload, `point.miss` went from 6.4%
   to 1.3%, `extension.rtree.query` from 5.3% to 1.0%, `scan.distinct` from 3.9% to 1.0%, and
   `join.selective` from 2.4% to 0.4%. **Two results do not fit.** `point.rowid` still spans 5.2%
   in B. `read.analytical` covers worse in B (57%) than in A (80%).
3. **So what our arm carries from one round to the next inside one process is the constant.** A
   process's memory layout is fixed when it starts: where the executable, the heap and the pool
   land. The short workloads are the ones most sensitive to how their code and data line up with the
   caches. In A every round of a pass shares one layout. In B every round gets a new one, so the
   layout becomes noise inside the pass, which the interval already measures. This is an
   explanation that fits the data. The passes did not test layout directly.
4. **B cannot become the gate's default, because it changes what our arm is timed on.** A new
   process pays for the first touch of every page it uses inside the clock. Our arm took about
   157,400 page faults a round in B and about 147,400 in A, where the pool is warmed first. SQLite's
   child took 11,634 to 11,640 on every pass. In B `schema` fell under the 1.00x floor on all six
   passes, with lower bounds of 0.52x to 0.96x, and `extension` missed its 1.50x bar on five of six,
   with lower bounds of 1.26x to 1.48x. In A both families met their bar or floor on every pass. The
   noise inside a pass rose two to four times for `schema`, `extension` and `large.values`. **The
   test written before the passes would have chosen B.** It did not check what B does to the level
   of each family or to its noise, and on that evidence B is a different measurement, not a
   correction.
5. **The first pass of this window was not slow** (SQLite +1.25% against a range of 0.36% to
   1.39%), so the first pass slowdown in the family statistic window did not repeat here.

**Five more passes, 2026-09-24 05:19:03 to 05:33:10Z, on a machine that was not quiet.** This second
experiment was meant to test whether the box stays slow for minutes after heavy work stops. It ran
one pass meant as the settled baseline, a write load (20 copies of the large fixture, 1.74 GB), two
passes, a build load (a cold release build of the full gate, 80 seconds), and two passes. The
baseline it recorded shows the box was busy before any load: CPU load was 37% at the start and
after the write load, and 16% after the build load. The gate uses about one core in 24, so other
processes were using up to a third of the machine. Memory Compression held 12.7 GB. **So the
question it was built for is unanswered.** No pass met the settled test written down before it ran
(three consecutive rolling values of the SQLite index at or below 2.36%), and the loads barely show
on top of a machine that was recovering on its own. What it showed instead is below.

| pass | SQLite arm | this engine's arm | `read.join` | `read.analytical` | `read.range` | `read.point` |
|---|---|---|---|---|---|---|
| settled window, six default passes | reference | reference | 4.19 to 4.27 | 10.62 to 10.77 | 4.96 to 5.07 | 28.97 to 30.15 |
| 1, meant as the baseline | +20.0% | +10.6% | **4.57** | **11.30** | **5.44** | **32.16** |
| 2, after the write load | +9.9% | +5.8% | **4.42** | **11.20** | **5.32** | 29.42 |
| 3 | +7.7% | +5.9% | **4.38** | **11.10** | 5.06 | 28.99 |
| 4, after the build load | +8.5% | +5.5% | **4.34** | **11.12** | **5.14** | 29.16 |
| 5 | +8.1% | +4.8% | **4.48** | **11.14** | **5.12** | 29.37 |

Each arm's figure is the geometric mean over the 12 read workloads of that pass's median time, over
the median time of the six default passes of the settled window. Bold is outside the settled
window's range. **Every bold value is above the range.**

**A busy machine slows SQLite's arm about twice as much as this engine's, so it makes this engine
look faster.** That is the same signature as the first pass of the family statistic window, which put
`read.join` at 4.37x with SQLite's arm 5% to 7% slow. **Why SQLite's arm suffers more is not
established.** It is not the number of page faults: SQLite's child takes 11,634 to 11,640 a round on
every pass, and this engine's arm took about 147,400. One difference the passes point at is that
SQLite's arm is a new process each round and fills its cache from its file while a workload is timed,
where this engine's pool is warmed before the clock starts. Nothing here tested that.

**Those 147,400 faults were one allocation, and a later change removed it.** 93% of them were in two
workloads, `correlated.in` (91,390 an execution) and `correlated.exists` (45,414). A correlated block
reads the outer row through parameters numbered from 100,000, and the parameter set was one vector
indexed by number, so the first such write grew it to 100,001 entries, 3.2 MB. The set is copied once
per batch, so each execution made and freed 58 of them, and a block that size is paged in again every
time. The engine's own slots are now a separate list (`physical::Slots`). On the medium fixture the
full plan takes 4,196 to 4,285 faults a round after the first, against SQLite's 11,638, and 3,131 of
those are `schema.index`. In a debug build `correlated.exists` went from 75 ms an execution to 7.2 ms
and `correlated.in` from 138 ms to 17.7 ms. Every published `read.correlated` ratio predates this.

### How a verdict should be taken

1. **Not from several consecutive passes.** What moved the ratios between passes was the state of the
   machine, and consecutive passes share it. The five busy passes above were all biased the same way,
   and so were the first four passes of the family statistic window. The mean of several such passes carries
   the same bias, with a narrower interval that makes it look more certain.
2. **From one pass, taken on a machine the pass itself shows was quiet.** SQLite's arm is the same
   program in every pass, so its speed against a recorded quiet reference is a measurement of the
   machine. Over the twelve quiet passes that index was at most 1.39% for a whole pass. On every busy
   pass seen, in either window, it was at least 4%. A pass above 3% should not be graded. **Since
   that change the three gates do this.** Each prints a "was the machine quiet" section with the pass's
   index over the point, range, join and analytical workloads, the reference file and when it was
   recorded. Above 3% every verdict reads NOT GRADED and the gate exits 4, where 1 is a miss and 2 is
   a run that measured nothing. The reference is kept per machine, outside the checkout
   (`%LOCALAPPDATA%inillucentquiet-reference<host><scale>.tsv`), and is written by running a
   gate with `--record-quiet-reference` while the machine is idle. With no reference the pass is
   graded and the section says it was not checked. `--quiet-threshold <percent>` changes the bound.
3. **On a quiet machine, one pass is enough for every verdict today.** What still differs between
   quiet passes is this engine's own process, and it is small. The between pass standard deviation
   of the family log ratio is 0.3% to 1.1% for the read families, 1.35% for `extension` and 3.2% for
   `large.values`. The family closest to its bar is `extension`, at 1.68x to 1.79x against 1.50x,
   which is more than eight of those standard deviations away.
4. **No bound is widened and no bar moves.** Widening the interval to cover a busy machine would
   hide the bias, because the bias is upward and a wider interval around an inflated centre can still
   pass. `--engine-child` stays a diagnostic.

`extension` reading 1.54x at its lowest in the family statistic window, which prompted this measurement, came from the
same kind of pass. On the quiet window its lowest per round bound in six default passes was 1.58x
and its centre was 1.68x to 1.79x.

### `read.join`'s bar under the per round statistic

**The bar stays at 3.00x.** Under the per round statistic, pinned, the family's lower bound on five
builds back to `57e87b0`, two passes each except `main`:

| build | full plan, per round | full plan, pooled | read gate plan, per round | read gate plan, pooled | `join.selective` and `join.range`, full plan |
|---|---|---|---|---|---|
| `57e87b0`, the first build graded | no full gate | no full gate | 4.69x, 4.52x | 3.23x, 3.18x | no full gate |
| `b0ba286` | **3.23x, 3.18x** | 2.42x, 2.38x | 4.42x, 4.35x | 3.11x, 3.07x | 11.15x and 0.97x, 11.03x and 0.95x |
| `a8f45b1` | 4.17x, 4.07x | 2.78x, 2.76x | 4.39x, 4.37x | 2.91x, 2.96x | 20.89x and 0.85x, 20.32x and 0.85x |
| `f9e2374` | 4.33x, 4.21x | 2.92x, 2.82x | 4.56x, 4.20x | 3.04x, 2.97x | 21.27x and 0.90x, 20.62x and 0.87x |
| `main` at `16c01a4` | 4.27x, 4.08x, 4.18x, 4.19x | 2.90x, 2.75x, 2.78x, 2.81x | 4.19x, 4.06x | 2.86x, 2.81x | 20.36x to 21.68x and 0.86x to 0.88x |

**Every build since the family was first graded meets 3.00x on both plans under the per round
statistic, and under the pooled one no build ever met it on the full plan.** The reasons for keeping
3.00x rather than moving it:

1. **3.00x is the number the contract meant, on the statistic it meant.** It is the "low estimate"
   column of `tasks/task-1816-rearchitecture-tdd.md`, the value the TDD gives for the family's ratio,
   and a family's ratio in that document is the geometric mean over its workloads. The per round
   statistic grades exactly that. Nine of the ten family bars come from the same column; the tenth is
   `read.analytical`, at 5.00x against a low estimate of 8x. The bar was never wrong. The statistic
   that graded it was.
2. **Moving it now would be choosing it from the measurement.** `main` reads 4.08x at its lowest. A
   bar of 4.00x would be that reading rounded down, and `compat/perf/contract.toml` says why a
   threshold that moves towards the measurement is not a threshold.
3. **3.00x is within reach of builds that existed.** `b0ba286` met it on the full plan by 6%, at
   3.18x and 3.23x, with `join.selective` at 11x. With today's `join.range` of 0.86x, the family
   drops under 3.00x if `join.selective` falls from about 21x to about 10.5x, or if `join.range` falls
   from 0.86x to about 0.43x. Either is a loss of half a workload's speed, which is what a family bar
   is for.
4. **The bar does not catch the 26% `join.range` lost since `b0ba286`**, and it is not meant to.
   That loss moved the family 8% on the read gate's plan (4.71x at `57e87b0` to 4.32x on `main`, the
   mean of two passes each), and it has its own follow-up work.

`join.range` is still slower than SQLite, at 0.84x to 0.88x. The TDD's reason column for this family
calls `join.range` at 0.015x "a planner bug" and expected the hash join to fix it. It went from 0.015x
to 0.86x and stopped short of SQLite. It is listed in
[the workloads that are slower](#the-workloads-that-are-slower), and the family bar says nothing
about it.

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

Before a later change, a heap slot could only be written over by a value of **exactly** the same length, so
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

**What took it from 14% to 9.5% was the index build stopping going through the page pool** (design 2
of [the performance design](../tasks/task-2000-inillucent-performance-tdd.md)). A bulk build wrote each page into a pool frame, which then had to be written out and
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
question design 10 asked - whether two to three of these mebibytes were the allocator's
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
added twelve and a half before design 2 stopped the build going through the page pool.
On 2026-09-23 the plan holds 31.46 MiB before the index build, because of the correlated workloads,
and the build adds 9.33 to reach the same 40.8. So the bar is not missed by a buffer that is slightly too big everywhere; it is missed by
one statement, and by the arena its sort holds.

That arena is the one lever left, and it is priced rather than pulled: an earlier design measured spilling
the sorted run to a temporary file at about **8 ms on a 27 ms statement**, which puts the `schema`
family under the 1.00x floor the contract sets. Buying memory with a floor is the trade that design
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
  (the performance review). What sits between them is the plan cache lookup, the parameter count,
  a `String` per result column per execution, and the dirty frame walk on release.
- **A table larger than the buffer pool.** `story_large_table_nightly` builds one in the `nightly`
  tier and scans, sorts and deletes half of it (the performance review). Nothing on this page is
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
  transaction. A later measurement of that path found `SELECT 1` costing 132,884 nanoseconds
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
times anything.** The machine these figures come from is an Intel Core Ultra 9 285, with
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
- Pinning makes one workload slower. `read.correlated` uses more than one thread, and a separate
  measurement found `correlated.exists` at 59.69 ms on the 8 performance cores, 46.35 ms on the 16 efficiency
  cores and 38.74 ms unpinned on all 24.

The run at the top of this page was taken before the gates could pin themselves, with the same mask set on the
gate process from outside it.

The gate binaries and the shell install `inillucent-alloc` as their global allocator. It is part of
the build in the same way fat link time optimisation and a single codegen unit are: SQLite ships its
own memory subsystem, so measuring a Rust workspace on the platform allocator measures a build
configuration rather than an engine. It is worth 3.24x to 3.86x on the medium gate.

[Repository](repository.md) covers the rest of the instruments and the test runner.
