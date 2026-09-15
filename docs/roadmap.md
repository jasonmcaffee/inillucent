# What is not there yet

In the order it is being worked, each with the measurement behind it and what closing it looks
like. Only open items are here. What has closed, and the number that closed it, is in
[Closed items](closed-items.md); what is settled and will not be pursued is there too, with the
reason.

Every ratio and percentage on this page also appears in [Performance](performance.md), which is
where it was measured, and a test in `crates/inillucent-compat/tests/documentation.rs` fails when
this page carries a number that page has moved past.

## 1. `read.join` and `extension` miss their bars on the lower bound

Every family is faster than SQLite. Two miss the target their bar sets, and both miss on the 95%
lower bound rather than the point estimate. These are targets rather than requirements, so neither
fails a release.

| family | measured | the bar asks |
|---|---|---|
| `read.join` | 332% faster (4.32x), lower bound **3.00x** | 200% faster. The four runs read 2.97x, 3.00x, 3.00x and 2.99x: a number that straddles a threshold has not met it |
| `extension` | 52% faster (1.52x), lower bound 1.36x | 50% faster, missed on the lower bound only |

**`read.join`.** The statement chain is reused across executions now, which reaches the point join's
shape (41% saved, paired measurement) and not the 200 row range join, whose cost is per entry across
200 probes, each a fresh descent from the root. The family has not been re-measured since the chain
reuse landed. Done means: four consecutive gate runs on a quiet box with the lower bound above
3.00x. If they do not clear it, the next lever is keeping the inner cursor across probes when the
outer side is ordered on the join key, seeking forward instead of descending, measured paired on
`join.range`.

**`extension`.** `extension.fts.build` at 0.59x is what holds the family's lower bound down. A
segment format for FTS5 was built once, made `fts.build` no faster and halved `fts.query`, and was
reverted; the cost was a manifest re-read from disk on every query, because a module had no way to
learn that another connection had committed. A module has that hook now (`committed_elsewhere`,
`schema_changed`). Done means: the segment format re-applied with the manifest cached and dropped
on those hooks, `fts.query` at or above its current 1.43x, and the family's lower bound above 1.50x.
If the query cost comes back, it is reverted again and the number recorded.

## 2. `write.insert.batch` is 43% slower than SQLite

**0.70x**: 2,000 inserts in one transaction. It was 72% slower; the improvement came with the
delta log seek in task-1911. It sits inside a family that clears its bar, so it blocks nothing.

**The cause named here was the wrong one, and the measurement says so.** The text said `locate()`'s
walk of each leaf's unsorted delta area, and the fix designed for it was a 16 bit fingerprint per
delta entry so an insert that misses the delta area would pay halfword compares and no decode. That
design carried its own stop condition - "if the saving is under a fifth of the gap, record it and
stop" - and the stop condition is met before the format change, on the numbers below.

`inillucent-writelogattrib` on the medium fixture, 2,000 inserts into `main_table` with its two
secondary indexes, which is the gate's own shape:

| where the log goes | records | bytes | share |
|---|---:|---:|---:|
| `Structural` (a split) | 40 | 963.1 KiB | **58%** |
| `InsertRow` | 6,000 | 687.5 KiB | 41% |
| `CompactLeaf` | 187 | 11.7 KiB | 0.7% |
| `AllocPage` and the commit | 41 | 1.6 KiB | 0.1% |

1,664 KiB of log for about 240 KiB of rows, and a split costs **24,656 bytes** - three whole 8 KiB
page images for one row that would not fit.

And `locate`'s delta walk, counted directly: **8,329 calls, 119,645 entries walked, 5.1 ms**, 14.4
entries a call, against **66.8 ms** of apply time across both arms. Under eight per cent, and that
is the whole walk rather than what a fingerprint block would save - a probe that matches still
decodes, and the block itself costs a hash per insert and 64 bytes a leaf. Removing all of it would
move 0.70x to about 0.755x: five and a half points of a forty-three point gap, where a fifth is
eight and a half. `crates/inillucent-compat/src/bin/writelogattrib.rs`'s own header already recorded
that a previous fix to that decode "did not move the gate ratio"; this is the number behind that
sentence.

Done, now, means the lever the measurement points at rather than the one that was guessed: **a
split that logs less than three whole pages.** A batch insert at the end of a key range splits
right, and the right page it creates is nearly empty - so the record carries an 8 KiB image of a
page that holds one row. Nothing here designs it; a format change to the split record is its own
ticket with its own crash campaigns, and the honest state of this item is that its cause is now
measured rather than supposed.

## 3. The retrieval index's footprint

**1.3 GB resident for a 3.1 GB index of 600,589 chunks** with the vectors read from the file, and
3.1 GB with them held in memory. The vectors are out of the default resident set; the graph and the
keyword postings are still all in memory and nothing has tried to make either smaller.

Done means, in three steps each with its number on the performance page: a measurement of where the
1.3 GB goes; the graph's adjacency lists laid out as fixed width pages read through the buffer pool
rather than deserialised whole; the postings the same way, a block per term. The resident set
becomes the pool budget. Acceptance on the same corpus: under 512 MiB resident at the default pool
with vectors on disk, p50 latency within 1.5x and p99 within 2x of today's, identical top k.

## 4. Threads

Access from several **processes** works: the same SHARED, RESERVED, PENDING and EXCLUSIVE protocol
as SQLite, under `PRAGMA locking_mode = normal`, measured over 37 stress rounds with two writing
processes and no lost writes. Threads inside one process do not. The engine is single threaded by
construction: its pool and trees use `RefCell`, a connection borrows the database, and there is no
parallel scan. The retrieval engine's graph build is the one thing that uses every core.

Done means the step that is reachable, which is what SQLite calls serialized mode: a database that
can be moved to another thread, and a shared handle that serialises statements behind a lock so a
web server can hand one database to a pool of workers. One transaction at a time becomes a type
rather than a doc comment. Statements do not run in parallel; a parallel executor is not on this
list.

## 5. A macOS archive

Every platform's archive is built on that platform, and there is no macOS build machine. `cargo
install inillucent-cli` builds it from source in the meantime. Everything reachable without the
machine is done; what is left is `packaging/macos/release-macos.sh --version <N> --upload` run on
one, after which the Homebrew formula and the two npm platform packages that wait on it go live.

## 6. Recovery reads a page before redo has had a chance to rewrite it

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

## 7. Two command line lines still reach past the driver

`drivers/README.md` says the driver is the one surface an application reaches the engine through,
and for an application that is true: the C ABI, the four language wrappers and every published
package go through it. The command line did not. Five files under `crates/inillucent-cli/src`
imported `inillucent_engine` directly, and
`crates/inillucent-compat/tests/policy.rs`'s `no_shell_file_reaches_past_the_driver_more_than_it_is_recorded_at`
records how many lines of each do, so the number can only come down.

**Twenty-eight lines to two.** Three of the five files are at zero; the driver grew the eleven
things they reached for.

| file | was | is | what moved |
|---|---:|---:|---|
| `shell.rs` | 10 | 1 | the virtual table modules, the authorizer, the cache statistics, the statement budget and `leading_trivia` |
| `command/mod.rs` | 8 | 0 | the VFS confinement root and the statement budget, both already re-exported |
| `commands.rs` | 7 | 0 | the authorizer trait and its two enums |
| `command/verbs.rs` | 2 | 0 | `Database::import_sqlite_into`, which takes the target a staged migration needs |
| `import.rs` | 1 | 1 | one function signature, which follows `shell.rs` |

What the driver grew: `Database::register_module`, `cache_stats`, `pool_bytes`, `limit`,
`set_limit` and `import_sqlite_into`; `Connection::changes`, `set_authorizer`, `set_defensive`,
`parameter_names` and `statement_length`; and re-exports of `AuthAction`, `Authorization`,
`Authorizer`, `vtab`, `CacheStats`, `Limit` and `leading_trivia`.

**The two that are left are one decision, and it is about the shell's value type.** Both are the
engine's `connect::Connection` and `connect::Database` as types - the shell's statement loop is
written against the engine's own streaming statement (`prepare`, `bind`, `step`, `row`,
`columns`), and its renderer against `inillucent_value::Value<'static>`, which is what
`owned_row_values` produces. The driver's `Statement` answers one materialised `Rows` whose cells
are `inillucent_driver::Value`, a different owning type. So moving the last two lines means either a
conversion per cell of every row the shell prints - which would be a third `Value` conversion in a
workspace whose whole point is that there is one - or rewriting `render.rs`, and with it the output
of 63 dot commands that is matched line for line against the reference shell. Neither is a
substitution; both are a decision about which value type the command line is written against.

Done, now, means that decision is made. The claim in `drivers/README.md` is not false today,
because it is about what an *application* reaches; it becomes false the day somebody reads it as
being about this repository. Until the count is zero, this item is what says so.

## Where to go next

- [Closed items](closed-items.md): what came off this list, and the measurement that closed each
- [Performance](performance.md): the measurements behind items 1 to 3
- [Feature comparison](feature-comparison.md): the full run, per workload
- [Repository](repository.md): the crates and the test runner
