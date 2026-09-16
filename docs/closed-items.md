# Closed items

What came off [the roadmap](roadmap.md), with the measurement that closed each, and what is settled
and will not be pursued, with the reason. A reader who remembers a roadmap number can find what
happened to it here.

## Memory

**42.40 MiB against SQLite's 37.20, which is 14% more**, on the same 128 MiB budget, while running
330% faster and spending 70% less processor. It came down twice, from 102% more, then 43% more.
The remaining 5.4 MiB is a page pool holding a file that is within 4% of SQLite's, a process floor
of which 4.1 MiB is what any Rust binary in this workspace costs before the engine exists, and one
`CREATE INDEX`. [Where the memory goes](performance.md#memory) attributes every megabyte. Closed by
decision: this is where it stays.

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

Built in task-1911. A `Compiled` with no lifetime owns the borrow free part of a statement's chain and
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
reach; that is [roadmap item 1](roadmap.md#1-extension-misses-its-bar-on-the-lower-bound).
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
allocator change has been measured on Linux, so the Linux figure is older than the 330% Windows
headline. Re-measuring wants a Linux machine that is not also running the Windows arm; both inside
one box would measure the contention and not the platform.

## `extension.fts.build`

**0.60x against SQLite**: 10.92 ms against 6.23 ms, measured on a quiet box over 30 rounds. Two
changes were built for it and measured. One bought nothing and was kept; the other cost half of
query throughput and was reverted.

**The doubled write, kept.** FTS5 used to do four tree writes per document; task-1911 made the last
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
looked". That hook exists now: `VirtualTable::committed_elsewhere` and `schema_changed`
(task-1932). Re-attempting the format with a cached manifest is what
[roadmap item 1](roadmap.md#1-extension-misses-its-bar-on-the-lower-bound) names for
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

Adding content stopped rebuilding the graph in task-1894, when a commit became a **fold**: the
published generation is loaded and each entry of the delta log inserted into it, one graph insert
per row written rather than one per row in the table. What was still proportional to the corpus was
publishing, because a generation was one serialised index.

Segmented generations closed that in task-1911: many small immutable segments merged at read time,
the way an LSM tree works, so both the graph work and the bytes written are proportional to the
batch rather than to the corpus. `crates/inillucent-search/src/module.rs` and `merge.rs` hold it
(`SegmentMeta`, `flush`, `merge_cascade`). The default delta log became a constant 1,024 entries at
the same time; it had been `max(1024, rows / 8)`.
[Keeping a vector index current](relational-architecture.md#10-keeping-a-vector-index-current) has
the measured range and how to choose `compact = N`.

## What task-1911 closed

Eight items came off the roadmap in task-1911. Each is named here so a reader who remembers the old
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
  all pass, with the pinned `sqlite3` present rather than absent. task-1869 had already removed the
  cause: `crates/inillucent-compat/src/interchange.rs` moves a database between the engines as
  `.dump` output replayed by the reference shell, instead of handing `sqlite3` a file it cannot read.
  This document was simply never updated.

## Recovery reads a page before redo has had a chance to rewrite it

**Closed by `cdc58eb`.** It was roadmap item 6, described there in the past
tense - the fix landed, the pair of tests landed, and the item stayed on the
open list (task-1969, 6.3). What closed it is
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
