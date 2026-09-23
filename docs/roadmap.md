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

**And it still misses inside the whole plan, measured 2026-09-20**: 1.57x with lower bounds of 1.17x,
1.33x, 1.38x and 1.45x. `extension.fts.build` at 0.69x is what holds the bound down, and what that
workload's time is has now been measured rather than described: a timer around the whole of
`Fts5Table::add` puts **52% of the workload outside the index**, 8.4 µs a document between the `INSERT`
and the indexing against 7.8 µs for all of the indexing. The stages inside are `content 1.4 ms`,
`docsize 1.1`, `tokenize 0.4`, `dict write 1.7` and `new terms 0.4` over 507 terms, for five hundred
documents. So this item's lever is the virtual table write path as much as the index, and **the one
change that looked obvious cannot ship**: moving the per column token counts out of `%_docsize` and
into the content row saves one shadow row write a document, and `%_content`'s shape is compared against
SQLite's own FTS5 shadow tables by `fts5::the_content_table_holds_the_rows`, which fails on the extra
column.

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

## 2. `write.insert.batch` is about 67% slower than SQLite

**About 0.60x**: 2,000 inserts in one transaction. It was 72% slower, then 43%, and it sits inside a
family that clears its bar, so it blocks nothing.

**task-2074 took the cost of an index from about 5.2 µs a row to about 2.0, on the index count
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
  whatever the leaf held, which the audit had priced as "the two indexes are 69% of this workload"
  (task-2066, C1). With a directory a lookup is a binary search, so the area can take the whole free
  gap: 1,624 compactions became 423. The audit predicted 18 to 20% of the workload; it was 43% at two
  indexes, because the compaction count fell by 3.7x rather than the 8x the audit assumed and every
  compaction became cheaper as well.
- **A compaction splices its delta rows into the packed page when the rows fit its widths**, instead
  of reading, pricing and writing every kept row again (task-2066, C2). It is 7% to 15% on top of the
  first change at two indexes and more, and nothing without an index: a table's own tree appends at
  its right edge and rarely compacts.

Both are page format changes, so the file format is 2. This build reads format 1;
`docs/relational-architecture.md` section 5a says how, and what the earlier releases answer for a
format 2 file.

**The 0.72x this item carried until now was measured by a gate that was not asking both arms the same
question.** `inillucent-writegate` never ran a workload's own `pre`, and `sqlite_bench.c` runs one
before it starts its clock - so on `txn.batched` and `txn.large`, which both carry
`UPDATE side_table SET note = 'note ' || id`, SQLite did work this engine skipped. task-2029 fixed it
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

**Making room is now 10.57 ms of 29.60, and it has been split into the four passes it actually is**
(task-2024). `LeafRef::live_source` is `live_order` and then `materialise` - deciding which rows
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

**What that is worth at the gate**, once task-2029 made the gate measure again. Four runs at a 32 KiB
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

### Landings 2 and 3, as far as no format change takes them (task-2066 §4.3.8)

Both were done as *filings* rather than as blocks behind the buffer pool, which is less than the
paragraph above asks for and needed no change to what is written on disk. An index written by any
build opens under this one and the other way round.

**The postings stopped paying per term.** They were a `HashMap<String, Vec<Posting>>` beside a second
`Vec<String>` of the same terms in sorted order, so each of the 1,705,097 terms was charged for three
times over: a string in the map and a string in the list, a `Vec` header and its own heap block
however few postings the term had, and a hash map slot with its stored hash and its load factor. They
are flat arrays now - one byte array for the terms, one array for the postings, a start per term, and
a binary search instead of a hash - with an overflow map that takes appends and is folded back in
once it holds an eighth of the postings.

**The chunk text is read from the file**, the way `vectors.bin` already was. A search reading ten
results reads ten ranges; a process that opens the index and searches nothing reads none. It needed
no format change because the offset a load has to know is one the reader can count rather than one
the file has to carry.

Measured with `inillucent-indexresidency` on a **185,078 chunk, 494,293 term** index built for this
from the public corpus cache - a third of the corpus above, so read these as a ratio rather than as a
replacement for that table. Same index, same binary built twice, each figure taken twice and agreeing
to 0.2 MiB:

| part | on disk MiB | resident before | resident after |
|---|---:|---:|---:|
| `lexical.bin`, the BM25 postings | 211.1 | 306.2 | **209.4** |
| `store.bin`, the chunks and their dictionaries | 171.7 | 189.4 | 189.4 |
| `graph.bin`, the HNSW adjacency | 26.3 | 45.7 | 45.6 |
| `vectors.bin` | 542.2 | 0.0 | 0.0 |
| total | 951.4 | 541.3 | **444.3** |

**The postings are 31.6% smaller resident and the whole index 17.9% smaller**, and the postings are
now 0.99x their own file where they were 1.45x it. What was being paid for was per term rather than
per posting, so the saving follows the term count: at the 1,705,097 terms of the corpus in the table
above, the same ratio puts 890.3 MiB at about 610.

**`store.bin` does not move in that table, and that is the table's limit rather than the change's.**
`indexresidency` reads the four parts directly with the same public readers `load` calls, which is
what lets it attribute a cost to each one - and it reads the store with `Store::read_from`, the
resident reader, because that is the function whose cost it is reporting. The filing is a decision
`persist::load` makes and the readers do not.

`inillucent-indexresidency --through-load` answers the other question: what a process that opens this
index holds. One number rather than four, because an open is one call. It samples three times in the
same process, so the arena is the only thing that differs between the readings rather than two
compilations being compared:

| | resident MiB | peak MiB |
|---|---:|---:|
| after `persist::load` | **424.2** | 436.1 |
| after reading every chunk's text once | 424.3 | 436.1 |
| with the chunk text held instead | 580.2 | 584.2 |

Run twice, agreeing to 0.2 MiB. **The text costs 156.0 MiB of a 580.2 MiB open, which is 26.9% of
it**, and 163,556,613 bytes of chunk text is 156.0 MiB - so the difference is the arena and nothing
else. Reading every chunk once adds 0.1 MiB, which is what says the filed arena streams rather than
accumulates.

The two filings together take an open of this index from 677.0 MiB to 424.2 MiB, **37.3%**.

**The acceptance is not met and is not claimed.** Under 512 MiB resident at the default pool is the
bar and this index is under it at 424.2 MiB, but the corpus in the table above is three times its
size and nothing here has been run against that one. The graph, landing 3, is untouched. And p50,
p99 and identical top k were not re-measured: the postings change is a change to how they are laid
out in memory rather than to what they hold, and `inillucent-core`'s 282 cases say the same postings
come back, but that is an argument and the acceptance asks for a measurement.

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
