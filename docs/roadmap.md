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

The cost is in `crates/inillucent-tree/src/leaf.rs`. `locate()` walks each leaf's unsorted delta
area, up to `DELTA_LIMIT` (32) entries with a typed decode per key column, on every insert, and
`main_table` carries two secondary indexes, so a batch pays it three times a row.

Done means: a 16 bit fingerprint per delta entry at the head of the delta area, flagged by a leaf
header bit, so an insert that misses the delta area pays 32 halfword compares and no decode. Leaves
written before the bit read as they do today and gain the block on their next delta write, so no
page is migrated. `locate()` is shared with recovery and takes the same path. Measured paired before
and after; if the saving is under a fifth of the gap, that is recorded and the next lever, a sorted
delta area, gets its own design.

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

## 6. Recovery can read a page before redo has had a chance to rewrite it

Found while hardening a test for the free map checkpoint fix, not root caused further. Recovery
reads page 4 and fails its checksum before the redo pass that would have rebuilt it ever runs, so a
page the log could have repaired makes the whole open fail instead. Reproduced at cut 7 of
`crates/inillucent-compat/tests/free_map_checkpoint_crash.rs` under `PRAGMA journal_mode = off`, with
every checkpoint fix in place; it does not reproduce under the default `delete` journal, whose
rollback journal repairs a torn page on its own.

It is the same shape `open_file`'s own comment describes for the catalog root: a page whose bytes
fail their checksum before anything has replayed a record, at a point where recovery has not run
and cannot run first, because its row decoder needs a shape that comes from the very read that is
failing. The catalog root has a repair pass for exactly this circle. Page 4 is not the catalog root,
so that pass does not reach it.

`journal_mode = off` is documented to mean a torn checkpoint page is not recoverable at all, so part
of this is that mode behaving as specified. What is not explained by that alone is the read
happening before redo rather than after. Done means: the root cause named; every physical page
image in the log applied before any page other than the meta page is read, so the catalog root's
repair generalises to every page; and a test that tears page 4 with an image in the log and opens,
beside one that tears it without and fails with the documented code.

## 7. Five command line files still reach past the driver

`drivers/README.md` says the driver is the one surface an application reaches the engine through,
and for an application that is true: the C ABI, the four language wrappers and every published
package go through it. The command line does not. Five files under `crates/inillucent-cli/src`
import `inillucent_engine` directly, and
`crates/inillucent-compat/tests/policy.rs`'s `no_shell_file_reaches_past_the_driver_more_than_it_is_recorded_at`
records how many lines of each do, so the number can only come down.

| file | lines | what it reaches for |
|---|---|---|
| `shell.rs` | 10 | sessions, virtual table modules, the authorizer, pool statistics, `ATTACH` |
| `command/mod.rs` | 8 | the command table's context and its budget arming |
| `commands.rs` | 7 | `.dbinfo`, `.stats` and the serialisation verbs |
| `command/verbs.rs` | 2 | `migrate` and `batch`, which drive a transaction |
| `import.rs` | 1 | one function signature taking an engine connection, which follows `shell.rs` |

Done means the driver grows each of those, as thin wrappers over engine methods that exist, the
table ratchets to zero file by file, and at zero the test becomes "no file under the command line
imports the engine". The claim in `drivers/README.md` is not false today, because it is about what
an *application* reaches; it becomes false the day somebody reads it as being about this
repository. Until the count is zero, this item is what says so.

## Where to go next

- [Closed items](closed-items.md): what came off this list, and the measurement that closed each
- [Performance](performance.md): the measurements behind items 1 to 3
- [Feature comparison](feature-comparison.md): the full run, per workload
- [Repository](repository.md): the crates and the test runner
