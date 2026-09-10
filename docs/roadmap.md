# What is not there yet

In the order it is being worked, each with the measurement behind it.

## 1. Memory is the one headline SQLite still wins

**42.61 MiB against SQLite's 37.19 — 15% more**, on the same 128 MiB budget, while running 326%
faster and spending 67% less processor. The performance contract asks for 5% *less*, so this bar is
missed.

It has come down twice: it was 102% more, then 43% more, now 15%. The remaining 5.4 MiB is a page
pool holding a file that is now within 4% of SQLite's, a process floor of which **4.1 MiB is what any
Rust binary in this workspace costs before the engine exists**, and one `CREATE INDEX`.
[Where the memory goes](performance.md#memory) attributes every megabyte.

## 2. Four per family bars are missed

Each on four consecutive runs. These are targets rather than requirements, and every one of these
families is still faster than SQLite. `transaction` was under the contract's **floor** on all four
runs and is no longer on this list at all: task-1890 took it to 241% faster.

| family | measured | the bar asks |
|---|---|---|
| `open.prepare` | 58% faster (1.58x) | 400% faster |
| `read.join` | 311% faster (4.11x) | 200% faster — missed on the 95% lower bound only, 2.89x |
| `extension` | 30% faster (1.30x) | 50% faster |
| `schema` | 27% faster (1.27x) | 200% faster |

**Two of the four are not gaps, and the arithmetic says so.** `open.prepare` is `prepare.point` at
4.96x and `prepare.trivial` at 0.51x; for the family to reach 5.00x, `prepare.trivial` would have to
reach 5.05x, and SQLite compiles, binds, steps and resets `SELECT 1` in 483 ns — so the bar is asking
for 96 ns. `schema` is one `CREATE INDEX`, 26.78 ms against SQLite's 33.54, whose stages are
`scan 3.7, sort 5.5, pack 11.4, catalog 0.3, seal 5.8`: a packer costing nothing at all leaves
15.4 ms, which is 2.18x. Neither bar has been moved — they are in `compat/perf/contract.toml`, they
were written before any measurement, and changing one to meet a number is not a decision this
document makes.

`extension` is the reachable one, and item 6 has it. `read.join` misses on its lower bound alone, and
what would close it is the operator chain reuse in item 3.

## 3. The operator chain is rebuilt on every execution

Measured, both arms warmed and the order reversed so a warm cache could not flatter either: reusing a
compiled chain is worth **738 ns to 358** on `SELECT 1`, **1,413 to 786** on a point lookup, and
**71,672 to 59,983** on the 200-row range scan that `join.range` and `range.lookaside` are shaped
like — which is the 14-15% those two sit under SQLite.

`physical::build_statement` already holds a chain across executions and rebuilds only the source, and
nothing calls it. What stops it is ownership rather than effort: an index nested loop holds a borrow
of the tree it reads, and the engine keeps its trees in a map whose write path takes them mutably, so
caching a chain means reference counting the trees. The failure mode of getting that borrow
discipline wrong is a runtime panic on a shape as ordinary as an `UPDATE` that reads the table it
writes, which is why it wants its own run at it with the discipline designed rather than discovered.

## 4. Linux

**53% faster there, where Windows measured 279% at the time.** This was settled by experiment: with
a size classed free list in place of the system allocator the two platforms run the same absolute
speed, 38.97 ms against 38.20, and it is SQLite's own arm that moves across platforms rather than
this engine's. [Linux](performance.md#linux) has the measurement. Neither the allocator change that
took Windows from 3.24x to 3.86x nor anything since has been measured on Linux, so the Linux figure
is older than the 326% Windows headline rather than a comparison with it.

## 5. `write.insert.batch`

**72% slower than SQLite**: 2,000 inserts in one transaction, writing 2,491 KiB of log. It sits
inside a family that clears its bar, so it blocks nothing, and the plan is to keep the delta entries
sorted by their encoded key and find them by bisection instead of scanning and decoding each one.

## 6. `extension.fts.build`

**178% slower than SQLite** — 10.97 ms against 3.68 — and what is left is not micro cost. Writing
the dictionary in perfect key order still costs about 3.2 µs a row against about 2.4 µs for a
`%_data` row, and this engine's own `write.insert.batch` is 15.7 µs a row — so a shadow table write
is not slow.

FTS5 here does four tree writes per document: `%_content`, `%_docsize`, the new term's `%_idx` row
and its `%_data` doclist. SQLite's accumulates the batch in memory and writes a handful of segment
blobs at commit. Closing the rest is a segment format change touching every reader of `%_idx` and
`%_data`.

The gate prints the breakdown beside the ratio: over 500 documents, `content` 2.1 ms, `tokenize` 0.5,
`docsize` 1.6, `group` 0.2, `terms` 0.4, new terms 0.4 over 507 terms, dictionary write 2.2, flush
3.6. Making the dictionary row and the doclist row one row instead of two is worth about 2.9 ms of
the 10.97, which is what would take the family over its bar at this scale; it already reads 1.58x at
5,000 rows and 1.51x at 600,000.

## 7. Deleting the old engine

The engine that reached SQLite file format parity is still in the tree. It was measured between 30%
and 95% slower than SQLite across the families, which is why the rearchitecture happened.

Re rooting is done: `inillucent` is a re-export of `inillucent-engine` and the old facade is
`inillucent-legacy`. What remains is the deletion of `inillucent-legacy`, `inillucent-capi`,
`inillucent-session`, `inillucent-vm`, `inillucent-transaction` and `inillucent-storage` minus its
reader, and it is blocked on the driver's C ABI, which is what replaces `inillucent-capi`.

## 8. The retrieval index's footprint

**1.3 GB resident for a 3.1 GB index of 600,589 chunks** with the vectors read from the file, and 3.1
GB with them held in memory. The vectors are out of the default resident set; the graph and the
keyword postings are still all in memory and nothing has tried to make either smaller.

## 9. Threads

Access from several **processes** works: the same SHARED, RESERVED, PENDING and EXCLUSIVE protocol as
SQLite, under `PRAGMA locking_mode = normal`, measured over 37 stress rounds with two writing
processes and no lost writes. Threads inside one process do not. The engine is single threaded by
construction — its pool and trees use `RefCell`, a connection borrows the database, and there is no
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

The thing that is not built is **segmented generations** - many small immutable segments merged at
read time, the way an LSM tree works - which would make both the graph work and the bytes written
proportional to the batch rather than to the corpus. Until then a table that needs its write latency
pinned declares `compact = N`, which fixes the delta log at `N` entries and therefore fixes the graph
work per published generation, at the price of writing the generation every `N` rows.
[Keeping a vector index current](relational-architecture.md#10-keeping-a-vector-index-current)
publishes the measured range and how to choose `N`.

## 11. A second metric on the vector index

`ORDER BY vector_distance_l2(v, ?) LIMIT k` plans as a scan and a temporary tree, because the graph
is built over unit vectors and cosine is what it minimises. The distance functions themselves answer
for every metric. pgvector spells the same restriction as an operator class, and
`WITH (metric = ...)` is where the spelling goes here once the structure has a second metric to name.

## 12. A macOS archive

Every platform's archive is built on that platform, and there is no macOS build machine.
`cargo install inillucent-cli` builds it from source in the meantime.

## 13. A registered function cannot be called from the write path

`embed(TEXT)` works in a projection, in a `WHERE` predicate, in an `ORDER BY` and in an
`INSERT ... SELECT`. It is refused, with the `unsupported` status and exit code 3, in three places:

```sql
INSERT INTO note (body, v) VALUES (?1, embed(?1));   -- refused
UPDATE note SET v = embed(body) WHERE id = 1;        -- refused
INSERT INTO note (body) VALUES (?1) RETURNING embed(body);  -- refused
```

The reason is one field. The physical pass resolves a registered function's body through the
catalog, and a `RowSpace` — the space the write path compiles its expressions against — is built
from a table's layout rather than from a catalog, because a `RowSpace` is carried through about a
dozen signatures and holding a borrow would put a lifetime on all of them. So the lookup finds
nothing and the translation refuses by name rather than treating the function as absent, which is the
right behaviour for a gap and the wrong behaviour to leave in place.

**The fix is a catalog parameter on `RowSpace::compile` and on the callers that reach it**, every one
of which already has a `Target` and therefore a `Target::catalog()`. It was scoped out of task-1900
because that ticket's subject was installing the embedder rather than the DML expression path, and
because the shape that matters for an embedding — `ORDER BY vector_distance_cos(v, embed('…'))` —
was the half that could be fixed in three lines and was.

`crates/inillucent-compat/tests/functions.rs` holds both halves:
`a_registered_scalar_reaches_order_by_and_where` is what task-1900 made work, and
`a_registered_scalar_in_a_values_row_refuses_by_name` pins what it did not, so closing this gap is a
deliberate change to a named expectation rather than something that quietly starts working.

Until then, `INSERT ... SELECT` is the documented shape for writing a computed vector, and it is what
`docs/embeddings.md` and the search skill show.

## The failing tests

Seventeen, all accounted for, and every one of them failed before the last two rounds of work as
well.

| binary | count | what they are |
|---|---|---|
| `schema_forms.rs` | 14 | **all 14 fail on the same thing**: they need the pinned `sqlite3` to read a file this engine wrote, or the reverse. Writing SQLite's file format was withdrawn as a requirement. Two of them get past every assertion they were written for and fail only at that step, and the behaviour they cover is checked against the oracle elsewhere without needing file format interoperation |
| `planner.rs` | 2 | `sqlite_stat1` exchanged with the oracle: the same class of thing |
| `ordering.rs` | 1 | a known difference in the order of tied rows |

The recommendation on the table is to retire the file format cases in `schema_forms.rs` and keep the
rest as tests that drive the engine. All fourteen now fail for one reason, and it is a requirement
this project has already dropped.

## Where to go next

- [Performance](performance.md) — the measurements behind items 1 to 6
- [Feature comparison](feature-comparison.md) — the full run, per workload
- [Repository](repository.md) — the crates and the test runner
