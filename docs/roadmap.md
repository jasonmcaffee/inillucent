# What is not there yet

In the order it is being worked, each with the measurement behind it and what closing it looks
like. Only open items are here. What has closed, and the number that closed it, is in
[Closed items](closed-items.md); what is settled and will not be pursued is there too, with the
reason.

Every ratio and percentage on this page also appears in [Performance](performance.md), which is
where it was measured, and a test in `crates/inillucent-compat/tests/documentation.rs` fails when
this page carries a number that page has moved past.

## 1. `extension` misses its bar on the lower bound

`read.join` no longer does. It was on this list because its four lower bounds read 2.97x, 3.00x,
3.00x and 2.99x against a 3.00x bar, and a number that straddles a threshold has not met it. The
chain reuse that landed in task-1911 had never been measured against the family. Re-measured
2026-09-15, four consecutive runs on the same box, 30 rounds each, `--scale medium --page-size 32768
--frames 4096`:

| run | `read.join` | 95% low | bar |
|---|---:|---:|---:|
| 1 | 6.46x | **4.11x** | 3.00x |
| 2 | 6.26x | **4.00x** | 3.00x |
| 3 | 6.14x | **4.02x** | 3.00x |
| 4 | 5.60x | **3.67x** | 3.00x |

Every lower bound clears the bar, by a third at the narrowest. `join.selective` reads 35.49x and
`join.range` 1.17x; the range join is still the slow half and still the one an ordered probe reuse
would reach, but the family it is in is met and the item does not need it.

**`extension` still misses, and re-applying the reverted segment format cannot close it.** Four runs
the same way:

| run | `extension` | 95% low | bar |
|---|---:|---:|---:|
| 1 | 1.58x | 1.40x | 1.50x |
| 2 | 1.60x | 1.39x | 1.50x |
| 3 | 1.59x | 1.39x | 1.50x |
| 4 | 1.67x | 1.45x | 1.50x |

`extension.fts.build` is the worst workload in every run, at 0.56x to 0.58x, and it is what holds
the bound down. The rest of the family is well clear, with `extension.fts.query` at **1.70x to
1.85x**.

The design said to re-apply the segment format with the manifest cached, and to accept it only if
`fts.query` stays at or above 1.43x **and** the family's lower bound clears 1.50x. The second cannot
follow from the first. [Closed items](closed-items.md#extensionftsbuild) records what that format
did when it was built: it *made `fts.build` no faster* and halved `fts.query`. The workload dragging
the family is `fts.build`, and a change the measurement record says does not move `fts.build` cannot
move the family past a bar `fts.build` is holding down. `fts.query` has meanwhile risen from the
1.43x that revert left to 1.77x on its own.

So what the item needs is `fts.build` itself, and the gate's own per-round breakdown says where its
time goes. 500 rows in 11.6 ms against SQLite's 6.5 ms:

| step | ms |
|---|---:|
| content | 2.7 |
| dictionary write (the flush) | 2.4 |
| docsize | 2.0 |
| tokenize | 0.6 |
| new terms (507 of them) | 0.6 |
| terms | 0.5 |
| group | 0.3 |
| dictionary read | 0.2 |

Three steps are three quarters of it: writing the content row, writing the docsize row, and flushing
the dictionary. SQLite writes about 1,000 rows and one segment blob for the same documents. Done
means a design against those three numbers rather than against the segment format, and it is not
designed here.

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
keyword postings are still all in memory.

**Landing 1 of three is done: the measurement, and it re-aims the other two.** Nothing had said which
part the resident bytes were. `inillucent-indexresidency` reads a saved generation's four parts in
the order an open reads them and samples the resident set between them, so what each part costs is
measured rather than derived from its file's size. On the 600,589 chunk corpus at 768 dimensions,
1,705,097 terms, with the vectors left in the file, measured twice with the same answer:

| part | on disk MiB | resident MiB | share of resident |
|---|---:|---:|---:|
| `lexical.bin`, the BM25 postings | 614.8 | **890.3** | **53%** |
| `store.bin`, the chunks and their dictionaries | 564.8 | 620.3 | 37% |
| `graph.bin`, the HNSW adjacency | 87.7 | 154.2 | 9% |
| `vectors.bin` | 1,759.5 | 0.0 | none, they are read from the file |
| total | 3,026.9 | **1,664.9** | |

Two things follow, and both change what the remaining landings are.

**The postings are the largest, not the graph.** The design put landing 2 on the graph and landing 3
on the postings. The graph is 154 MiB - nine per cent - and the postings are 890 MiB. The order
reverses: postings first.

**And the two of them together are not enough.** The acceptance is under 512 MiB resident at the
default pool with the vectors on disk. Paging the postings and the graph would leave `store.bin`'s
620 MiB, which is already over the bar on its own. The store is a landing the design does not
mention and the arithmetic requires. It holds the chunk text and its dictionaries, and it is 620 MiB
resident for 565 MiB on disk - so unlike the graph it is not being expanded much by being loaded; it
is simply all of it, in memory, because a chunk's text is read by every result.

Done, now: the postings behind the buffer pool, one block per term with delta coded document ids as
the doclist already is; the store the same way, a block per chunk; and the graph last, its adjacency
lists as fixed width pages. Each with its number on the performance page, and the acceptance
unchanged - under 512 MiB resident, p50 within 1.5x and p99 within 2x, identical top k.

## 4. Threads

Access from several **processes** works: the same SHARED, RESERVED, PENDING and EXCLUSIVE protocol
as SQLite, under `PRAGMA locking_mode = normal`, which is the default. One writer holds the file at
a time and a second writer is refused with `busy` after `PRAGMA busy_timeout`. What grades it is
`crates/inillucent-compat/tests/process_concurrency.rs`, which spawns two real writer processes and
asserts that the rows in the file equal the commits the engine acknowledged - one process per
statement and two long-lived ones, under both locking modes, and through `ATTACH`. Threads inside
one process did not.

The line this replaces claimed two processes and zero lost writes over a stress campaign. That
number came from `concurrency.rs`, which runs two *sessions* inside one process. Two real processes
lost 43% of their acknowledged commits on every round until task-1980 (task-1979, section 4).

**Built: `SharedDatabase`, which is serialized mode.** Any number of threads use one database,
exactly one statement runs at a time, and a transaction holds its turn for its whole life. A web
server hands one `SharedDatabase` to a pool of workers and each worker clones it.

**The database is not moved between threads; it gets one of its own.** The design said to make
`Database: Send` after an audit and put it behind `Arc<Mutex<_>>`. The audit found no thread local
and no raw pointer in the engine - the one `thread_local!` in the workspace is a test-only decode
counter - so what stands in the way is `Rc`, and the argument for an `unsafe impl Send` would be
that the `Rc` graph is reachable only through the mutex. That argument has a hole:
`Connection::set_authorizer` takes an `Rc<dyn Authorizer>` the **caller** keeps a clone of, so a
database with one installed would have a live handle on two threads and a non-atomic count between
them. It is closable by leaving `set_authorizer` off the shared surface, but then the soundness of a
shipped `unsafe` rests on a method not being added later.

So the database is opened on a thread of its own and never leaves it, and the handles send it
statements over a channel. `inillucent-driver` keeps `#![forbid(unsafe_code)]` and the confinement
is the compiler's rather than a paragraph's. The cost is a thread per shared database and a channel
round trip per statement - two context switches against a statement that takes longer than that.

`drivers/inillucent-driver/tests/threads.rs` asserts the three properties the design named: eight
threads inserting a thousand rows each land eight thousand rows with no two sharing a key; a reader
sampling throughout a thousand-row transaction sees zero or a thousand and never a number between;
and a database used and dropped on another thread releases its file, which the reopen afterwards
proves. A fourth asserts that a transaction dropped without a commit rolls back **and** gives the
turn up, because a rollback that did not run would leave the next thread's statement inside a
transaction nobody opened.

Statements still do not run in parallel. A parallel executor is not on this list, and
[the architecture overview](architecture-overview.md) says so where a reader meets it.

## 5. A macOS archive

Every platform's archive is built on that platform, and there is no macOS build machine. `cargo
install inillucent-cli` builds it from source in the meantime. Everything reachable without the
machine is done; what is left is `packaging/macos/release-macos.sh --version <N> --upload` run on
one, after which the Homebrew formula and the two npm platform packages that wait on it go live.

## 6. Two command line lines still reach past the driver

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

## 7. PostgreSQL parity: a server, a replica, readers beside a writer, roles and the dialect

There is no listener, no replica, no reader that proceeds while a writer holds the file, no role
and no password. A PostgreSQL client has nothing to connect to. What closing each of those looks
like, in the order they are worked, is designed in
[task-1998, the path from an embedded engine to PostgreSQL parity](../tasks/task-1998-postgres-parity-tdd.md):
a server that runs as a service and speaks the PostgreSQL wire protocol first, a primary with a
replica fed from the redo log second, snapshot readers alongside the one writer third, roles and
row policies fourth, the PostgreSQL dialect fifth, and the operational verbs last.

Two measurements sit behind it, both in the design. Under the same load and with both engines
syncing every commit, one writer here commits 38 single rows a second against PostgreSQL's 2,837,
because the default journal mode syncs three times a commit, and four readers complete four reads
while a 50,000 row transaction is open, because a reader waits for the writer to release the file.
And a probe of one statement per PostgreSQL feature, 174 of them, is accepted for 59 and says
which tokens, types, functions and catalogue tables the dialect rung has to add. Done, for the ladder as a whole, means `psql`, the `postgres` library for
Node and `pg_dump` work against the server unchanged, and a second server holds a copy of the data
that stays current.

## Where to go next

- [Closed items](closed-items.md): what came off this list, and the measurement that closed each
- [Performance](performance.md): the measurements behind items 1 to 3
- [Feature comparison](feature-comparison.md): the full run, per workload
- [Repository](repository.md): the crates and the test runner
