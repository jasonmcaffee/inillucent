# What is not there yet

In the order it is being worked, each with the measurement behind it.

## 1. Memory is the one headline SQLite still wins

**42.40 MiB against SQLite's 37.20, which is 14% more**, on the same 128 MiB budget, while running 330%
faster and spending 70% less processor. The performance contract asks for 5% *less*, so this bar is
missed.

It has come down twice: it was 102% more, then 43% more, now 14%. The remaining 5.4 MiB is a page
pool holding a file that is now within 4% of SQLite's, a process floor of which **4.1 MiB is what any
Rust binary in this workspace costs before the engine exists**, and one `CREATE INDEX`.
[Where the memory goes](performance.md#memory) attributes every megabyte.

## 2. Four per family bars are missed

Each on four consecutive runs. These are targets rather than requirements, and every one of these
families is still faster than SQLite. `transaction` was under the contract's **floor** on all four
runs and is no longer on this list at all: task-1890 took it to 241% faster, and it reads 152% faster
today because the rollback journal now does the sync a rollback journal is for.
[Performance](performance.md#by-family) has that change and what it bought.

| family | measured | the bar asks |
|---|---|---|
| `open.prepare` | 56% faster (1.56x), lower bound 1.17x | 400% faster |
| `read.join` | 332% faster (4.32x), lower bound **3.00x** | 200% faster. Two of the four runs came in under the bound |
| `extension` | 52% faster (1.52x), lower bound 1.36x | 50% faster, missed on the lower bound only |
| `schema` | 40% faster (1.40x), lower bound 1.16x | 200% faster |

**Two of the four are not gaps, and the arithmetic says so.** `open.prepare` is `prepare.point` at
4.96x and `prepare.trivial` at 0.51x; for the family to reach 5.00x, `prepare.trivial` would have to
reach 5.05x, and SQLite compiles, binds, steps and resets `SELECT 1` in 483 ns, so the bar is asking
for 96 ns. `schema` is one `CREATE INDEX`, 26.78 ms against SQLite's 33.54, whose stages are
`scan 3.7, sort 5.5, pack 11.4, catalog 0.3, seal 5.8`: a packer costing nothing at all leaves
15.4 ms, which is 2.18x. Neither bar has been moved. They are in `compat/perf/contract.toml`, they
were written before any measurement, and changing one to meet a number is not a decision this
document makes.

**`read.join` and `extension` both moved, and neither moved far enough to come off this list.**
`read.join`'s lower bound was 2.89x and is 3.00x, which is the bar itself: the four runs read 2.97x,
3.00x, 3.00x and 2.99x, so it straddles rather than clears it, and a number that straddles a
threshold has not met it. `extension`'s was 1.09x and is 1.36x against a 1.50x bar, the largest
single move on this table, and it came from reverting work rather than from adding any (item 6).

`read.join` misses on its lower bound alone. Item 3's chain reuse reaches **part** of it and not the
part the name suggests: paired measurement puts `join.selective`'s shape at 41% saved and the 200-row
range scan at nothing at all, so `join.range` is untouched. Its cost is per entry across 200 probes,
which no per-statement saving can reach. **`extension` no longer has a named fix.** It used to say "item 6 has it", on the strength of
a prediction that merging FTS5's dictionary row and doclist row was worth 2.9 ms of
`extension.fts.build`'s 10.97. task-1911 built that merge and measured it, and the prediction was
wrong. Item 6 has the numbers.

## 3. The operator chain is rebuilt on every execution

**The number this item used to carry was measured wrong, and the correction matters more than the
item.** `inillucent-execprofile` ran its two arms one after the other, and at the 200-row range scan's
size this box moves 30% between two runs of identical work, so the arms were measuring the box as
much as the change. task-1911 replaced that with a **paired** mode: 40 rounds of 200 executions per
arm, the same key sequence in both arms of a round, the arm order swapped every round, and the median
of per-round differences reported beside a count of how many rounds each arm won.

| statement | rebuilt | reused | saved | rounds reused faster |
|---|---:|---:|---:|---:|
| `SELECT 1` | 3,486 ns | 1,280 ns | 63% | 40 of 40 |
| a point lookup by rowid | 20,149 ns | 12,471 ns | 39% | 35 of 40 |
| a 200-row range scan | 1,191,358 ns | 1,221,768 ns | **none** | **17 of 40** |
| a covering range scan | 33,772 ns | 27,379 ns | 18% | 37 of 40 |
| a point join | 20,204 ns | 11,564 ns | 41% | 38 of 40 |

The absolute times are large because several agents were compiling on the box; the ratios and the
round counts are what survive that, which is what pairing is for.

**The 200-row range scan does not move**, and the sequential arms' 11-14% was noise: 17 of 40 rounds
went the other way. So this item does not close `range.lookaside` or `join.range`, and
[performance](performance.md) already gives the right reason for the second. Its cost is per entry
across 200 probes, and no per-statement saving can reach that. The saving on every line that does move
is the chain build, which is 2 µs; on a 62 µs statement that is invisible and on a 1 µs statement it
is most of the work.

**What it does reach is the point join**, which is `join.selective`'s shape, and that sits inside
`read.join`'s weighted geometric mean, and that family misses its bar on the 95% lower bound alone.

### What was built, and what is left

`physical::build_statement` builds a chain once and rebuilds only the source, and nothing in the
engine called it: `Cached::Select` went through `run_any_prepared` every time.

The obstacle was never a borrow discipline, it was a lifetime. `Statement<'t>` borrows
`&'t dyn TreeCatalog`, and the catalog is the connection, so caching one on the connection is a
self-referential structure. Reference-counting the catalog was rejected on a stronger argument than
size: `PagedTree` writes take `&mut self`, so an `Rc<PagedTree>` held in a cache makes `Rc::get_mut`
fail for the life of that cache. Every write would refuse, or the tree goes behind a `RefCell`, which
is the double-borrow panic the governed crates forbid. Worse, the connection **replaces tree values
without emptying the statement cache** on `reopen`, `reattach_entries` and `DETACH`, so a cached
handle would silently keep the previous tree and its previous root page. That is a wrong answer rather
than an error.

What was built instead is a `Compiled` with **no lifetime**, owning the borrow-free part of the chain
and re-acquiring the tree borrows inside `run`, where the compiler still checks them.
`Cached::Select` carries a `RefCell<Slot>` of `Untried | Reusable | Never`, and a re-entrant execution
of the same statement falls back to a fresh build rather than panicking or refusing. The reuse
verdict is decided by **what the builder did** rather than by re-inspecting the plan, the way
`rebindable` already is.

Building it surfaced four defects that were live in `build_statement` and invisible only because
nothing reused a chain. `Statement::run` never re-ran `subquery::fold`, so a second execution of a
statement whose source key reads a subquery **refused**; it also formatted an `EXPLAIN` string and
threw it away every execution. Both are fixed. `Correlated` and `LateralModule` copy the parameters at
build time and nothing counts that as a read, so `rebindable` would have said yes to a chain that
answers the first execution's question; `build_materialised_join` bakes its inner rows in at build
time, so a reuse across a write answers from stale rows. Both are excluded by the verdict rather than
patched.

**What is not built yet** is the index nested loop tower: a `JoinRecipe` holding the borrow-free part
of each inner stage, with the borrows re-acquired per execution. The same slot on the write
path's `Cached::Update`, `Delete` and `Insert`, so `keys_of` reuses too. Until the first of those, a
statement with more than one stage is refused by the verdict and still rebuilds, which means the point
join's 41% is measured but not yet collected.

## 4. Linux

**53% faster there, where Windows measured 279% at the time.** This was settled by experiment: with
a size classed free list in place of the system allocator the two platforms run the same absolute
speed, 38.97 ms against 38.20, and it is SQLite's own arm that moves across platforms rather than
this engine's. [Linux](performance.md#linux) has the measurement. Neither the allocator change that
took Windows from 3.24x to 3.86x nor anything since has been measured on Linux, so the Linux figure
is older than the 330% Windows headline rather than a comparison with it.

Re-measuring wants a Linux machine that is not also running the Windows arm. Both inside one box,
WSL beside Windows, would measure the contention and not the platform. task-1911 watched that
happen at a smaller scale: with four agents on the box, `extension.fts.build` read 0.50x and 0.64x
for the same code an hour apart, because SQLite's own arm moved 62% between the runs.

## 5. `write.insert.batch`

**72% slower than SQLite**: 2,000 inserts in one transaction, writing 2,491 KiB of log. It sits
inside a family that clears its bar, so it blocks nothing. Measured again on a quiet box in
task-1911: **0.50x, interval [0.46, 0.52]**, 43.19 ms against 21.39.

**This item named the wrong code until task-1911, and the plan it carried was aimed at a file this
workload never reaches.** It said the delta entries were scanned and decoded one at a time and that
finding them by bisection would close it. The delta log belongs to `inillucent-search`, and
`write.insert.batch` inserts into `main_table`, a plain relational table with no
`CREATE VIRTUAL TABLE ... USING inillucent_search` in its definition, so `deltas_above` is never
called by it.

The scan-and-decode is real and is in `crates/inillucent-tree/src/leaf.rs`. `locate()` walks each
leaf's **unsorted delta area**, which holds up to `DELTA_LIMIT` 32 entries with a typed decode per
column, on
every insert, and `main_table` carries two secondary indexes, so a batch pays it three times a row.
Closing it means sorting or indexing that area, which is a change to the on-disk leaf format and
therefore to recovery, and it wants its own ticket rather than a pass inside somebody else's.

The bisection the old plan asked for **was** built, in the place it actually applies:
`ShadowStore::scan_from` seeks to a key instead of walking and filtering, with the trait's default
doing the correct-but-slow skip so no implementor can be forgotten, and `Store::deltas_above` seeks
to `covered + 1`. A counting store proves it: 1,000 delta entries, and a seek past the watermark
visits 1,001 rows where the scan-and-filter visited 2,000. That matters, because a table carrying a
vector index reads its delta log on every write, which is what item 14 of the old list made usable.
and it is not what this number measures.

## 6. `extension.fts.build`

**0.60x against SQLite**: 10.92 ms against 6.23 ms, measured on a quiet box over 30 rounds. The
10.97 against 3.68 this item used to quote is from an older run against a slower SQLite arm; only the
new pair is comparable.

**Two changes were built for this item and measured. One bought nothing and was kept; the other cost
half of query throughput and was reverted.**

### The doubled write, kept

FTS5 here used to do four tree writes per document: `%_content`, `%_docsize`, the new term's `%_idx`
row and its `%_data` doclist. task-1911 made the last two one row: `%_idx` carries the doclist inline
rather than an integer naming the `%_data` row it lived in. This item predicted that was worth about
2.9 ms of the 10.97, which is 26%.

It was worth nothing measurable. Measured as a genuine A/B, with the same fixture and command and the
change alternated in and out of the tree with a release rebuild each time, at two round counts, the
paired
ratio read 0.50x/0.56x before and 0.55x/0.53x after. The second write really did disappear, and the
doclist bytes went with it, out of a rowid-keyed `%_data` row and into a `WITHOUT ROWID` row keyed by
`(segid, term)`, where rewriting costs about what the extra row saved. **The row count was never what
this workload pays for. The bytes are.**

It is kept because it is simpler and because old files still read. `%_idx`'s third column is
self-describing, an `Integer` being an older build's `%_data` page and a `Blob` this build's doclist,
and `crates/inillucent-compat/tests/fts5_legacy_layout.rs` manufactures an old-layout file out of one
this build wrote and requires the same answers. Breaking the branch that reads the indirection makes a
four-document index answer **nothing**, which is why that branch is tested rather than reasoned about.

### The segment format, built and reverted

The rest of the gap is what SQLite does that this does not: accumulate the batch in memory and write a
handful of segment blobs at commit, merging them incrementally with
[`automerge` and `crisismerge`](https://www.sqlite.org/fts5.html).

task-1911 built it: a `segid` per flush, tombstones at a negative segid so they cannot collide with a
term row, a manifest at `%_data` row `-1` whose *absence* identifies a pre-segment file, and an
automerge fold. It was correct, it was tested, and old files still read.

**It made `extension.fts.build` no faster and roughly halved `extension.fts.query`**, which took the
whole `extension` family under the performance contract's 1.00x floor, which is the bar that fails a
release
outright, on the argument that an engine which is fast on average and slow at one thing is not a
faster engine.

Three rounds of work went into recovering the query cost and did not:

| | `fts.query` | `extension` family, 95% low |
|---|---:|---:|
| before the segment format | **1.32x** | not measured |
| with it | 0.45x | **0.93x**, under the floor |
| after a per-segment prefix seek | 0.60x | 1.08x |
| after skipping a needless merge on one segment | 0.65x | 1.13x |
| after a tombstone-presence bit in the manifest | 0.57x - 0.71x | 1.15x |
| **reverted** | **1.43x** | **1.40x** |

The one remaining cost could not be removed safely, and finding that out was worth the round on its
own: the manifest is re-read and re-decoded from disk on **every query**, and it cannot be cached,
because **`VirtualTable::begin` is called exactly once in the whole engine**, at
`CREATE VIRTUAL TABLE` time, and never again. A read-only `SELECT` calls nothing on a module at all,
and `VirtualTable::savepoint` is never called anywhere. So a module has no hook that says "another
connection may have committed since you last looked", and a cached manifest decides **which rows
match** rather than merely how they rank.

So the segment format is not built, and the reason is now a measurement rather than an estimate. The
lifecycle gap above is the thing to fix first if it is attempted again, because without it a segmented
index cannot cache anything a transaction boundary would invalidate.

## 7. The old engine is deleted

`inillucent-vm` (16,356 lines), `inillucent-session` (8,331), `inillucent-capi` (5,350) and
`inillucent-legacy` (660) are gone from the workspace: 30,697 lines, the engine that reached SQLite
file format parity, its connection, its facade and its `sqlite3_*` C ABI. It was measured
between 30% and 95% slower than SQLite across the families, which is why the rearchitecture happened,
and `drivers/inillucent-driver-capi` (its own header, tests and conformance suite) had already replaced
what the C ABI was waiting on before this ticket started.

**36 files in `inillucent-compat` named one of the four, not the 30 first estimated here**, and the
original audit undercounted because four files (`concurrency.rs`, `lifecycle.rs`,
`src/bin/hotprofile.rs`, `src/bin/txnprofile.rs`) each named two of the four crates and were counted
once instead of twice. All 36 were rewritten before the crates came out, so every suite could be run
against the replacement first. Several were **differential** tests that ran the old engine beside the
new one and compared; each of those took one of:

- **re-pointed at the pinned SQLite oracle**, which was the majority, once `src/differential.rs`
  itself was
  moved onto `inillucent_engine::connect` (its `start_inillucent`/`observe`/`compare` now drive the new
  engine, leaked to a `'static` `Connection` so every existing call site kept its shape);
- **re-pointed at the new engine alone**, asserting the value the old engine used to agree about:
  `new_engine_search.rs`'s `inillucent_search`-over-two-stores comparison became an assertion of the
  new store's own captured rows, since only one store is left to captured against;
- **`tests/capi.rs` deleted, not re-pointed.** It proved `inillucent-capi`'s ABI by compiling a
  probe against the *official* `sqlite3.h`, which `inillucent-driver-capi` does not implement and was
  never trying to; `drivers/inillucent-driver-capi/tests/{abi,conformance}.rs` already prove the
  ABI that ships, with a real C compiler, the same way;
- **deleted with nothing surviving**: old-VM-bytecode-specific cases (a fused-copy optimiser, a
  program verifier, an opcode-cost breakdown) and old-engine-only capabilities the new engine does not
  have at all (incremental blob I/O, `serialize`/`deserialize`, update/commit/rollback hooks, a second
  engine to interop with at the file-format level). Three capability rows in
  `compat/sqlite-3.53.4.toml` (`vm.bytecode.verifier`, `vm.statement.interrupt`, `txn.hooks`) moved to
  `status = "missing"` as a result, joining the seven already there for other reasons. A running count
  of what the new engine has not built rather than a claim it still passes.

Nine `src/bin/*.rs` profiling and gating tools also named the old engine directly and were **re-pointed
rather than deleted** once it was clear their subject was never "the old engine" but "inillucent,
whichever engine ships". `scorecard.rs` and `prepareperf.rs` are the release performance gate and the
`open.prepare` benchmark this document's own item 2 numbers come from, and deleting them would have
made the suite look healthier while measuring less. `searchgate.rs` lost its second arm the same way
`new_engine_search.rs` did, because there is no old store to compare against. It is now an absolute
report rather
than a two-store gate, which is a real change to what it claims and is written down in its own doc
comment rather than hidden.

**`inillucent-storage` and `inillucent-transaction` stay**, and the exception is larger than "minus
its reader" suggests. `inillucent-engine` and `inillucent-migrate` both depend on
`inillucent-sqlite-reader`, which depends on both of them and on `inillucent-catalog`, which depends
on `inillucent-storage` in turn, because `analyze.rs`, `ddl.rs` and `load.rs` all read through its
`Pager`
and `BTreeCursor`. Reading a SQLite file in order to migrate away from it is what keeps 18,764 lines
of the old engine alive, and that is a feature rather than a leftover. `policy.rs`'s
`no_new_crate_reaches_into_the_retired_engine` ratchet now watches only these two crates;
`inillucent-vm` came off its `RETIRED`/`ALLOWED` lists with the crate itself.

What the audit did not anticipate: a capability manifest (`compat/sqlite-3.53.4.toml`) citing unit
tests inside the four crates by name, an evidence collector (`src/bin/evidence.rs`) and a Linux test
script (`tools/linux-tests.sh`) both naming the four crates by package for `cargo test`/`cargo build`,
and a shared `PragmaSpec`/`REGISTER` table that generates `compat/api/pragmas.toml` and lived in
`inillucent-session` with no equivalent on the new engine, moved down into
`crates/inillucent-sql/src/pragma_register.rs`, where both engines' front ends can reach it without
either depending on the other.

## 8. The retrieval index's footprint

**1.3 GB resident for a 3.1 GB index of 600,589 chunks** with the vectors read from the file, and 3.1
GB with them held in memory. The vectors are out of the default resident set; the graph and the
keyword postings are still all in memory and nothing has tried to make either smaller.

## 9. Threads

Access from several **processes** works: the same SHARED, RESERVED, PENDING and EXCLUSIVE protocol as
SQLite, under `PRAGMA locking_mode = normal`, measured over 37 stress rounds with two writing
processes and no lost writes. Threads inside one process do not. The engine is single threaded by
construction: its pool and trees use `RefCell`, a connection borrows the database, and there is no
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

The thing that is not built is **segmented generations**: many small immutable segments merged at
read time, the way an LSM tree works. That would make both the graph work and the bytes written
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

## 12. Recovery can read a page before redo has had a chance to rewrite it

Found while hardening a test for task-1911's free-map checkpoint fix, not root-caused further.
Recovery reads page 4 and fails its checksum before the redo pass that would have rebuilt it ever
runs, so a page the log could have repaired makes the whole open fail instead. Reproduced at cut 7
of the free-map checkpoint crash campaign (`crates/inillucent-compat/tests/free_map_checkpoint_crash.rs`)
under `PRAGMA journal_mode = off`, with every current checkpoint fix in place - it is not the
free-map defect that campaign exists to check, and does not reproduce under the default `delete`
journal, whose rollback journal still repairs a torn page on its own.

It is the same shape `open_file`'s own comment describes for the catalog root - a page whose current
bytes fail their checksum before anything has replayed a single record, at a point in `open_file`
where recovery has not run yet and cannot run first either, because its own row decoder needs a
shape that comes from the very read that is failing. The catalog root has a repair pass for exactly
this circle: a tolerant first pass replays what it can without the catalog, then the catalog is read
again. Page 4 is not the catalog root, so that repair pass does not reach it.

`journal_mode = off` is documented (`crates/inillucent-pool/src/journal.rs`) to mean a torn
checkpoint page is not recoverable at all, so part of this is that mode behaving as specified. What
is not explained by that alone is the read happening before redo rather than after - `Applier::page_lsn`
already answers `Ok(None)` for a page it cannot read, precisely so redo can rebuild one instead of
failing on it, which means the failing read here is happening somewhere earlier than redo, on a path
the catalog root's own repair pass does not generalize to. Not investigated past this.

## What task-1911 closed

Eight items came off this list, and the numbering above is what is left. Each is named here so a
reader who remembers the old numbers can find what happened to them.

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
  in `wal` now takes a `delete` journal. See `journal_for` in `crates/inillucent-engine/src/lib.rs`.
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
  all pass, with the pinned `sqlite3` present rather than absent. task-1869 had already removed the
  cause: `crates/inillucent-compat/src/interchange.rs` moves a database between the engines as
  `.dump` output replayed by the reference shell, instead of handing `sqlite3` a file it cannot read.
  This document was simply never updated.

## Where to go next

- [Performance](performance.md): the measurements behind items 1 to 6
- [Feature comparison](feature-comparison.md): the full run, per workload
- [Repository](repository.md): the crates and the test runner
