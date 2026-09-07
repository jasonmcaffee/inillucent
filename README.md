# inillucent

An embedded database in Rust with two engines in one repository:

- **A relational engine** that speaks SQLite's SQL dialect on its own storage: B+trees with PAX
  (column-within-page) leaves, a buffer pool, a redo write-ahead log with group commit, snapshot
  isolation, and a push-based vectorised executor. Its goal is to be a **faster SQLite** for the
  applications SQLite serves, with the same SQL and the same observable semantics.
- **A retrieval engine** for retrieval augmented generation: HNSW with predicates honoured inside
  the walk, exhaustive search as a first-class plan, int8 quantisation, BM25 with coverage and
  proximity weighting, three fusion methods, a calibrated confidence beside every score, and
  persistence. It does the job of **PostgreSQL + pgvector + an embedding server**, in process.

The retrieval engine reaches SQL through the `inillucent_search` virtual table, so one file can hold
ordinary tables and a hybrid index that commits and rolls back with them. Neither engine links
another database: the only SQLite in the tree is a pinned 3.53.4 build run as a child-process oracle
(`docs/dependency-policy.md`, enforced by a test).

This document was rewritten on 2026-09-06 (task-1802) from what the sprint's tickets measured. Every
number names the ticket or file it came from; nothing here was estimated.

## Where it stands against the goal

The goal is a highly performant SQLite replacement offering the same features, plus embedding search
similar to pgvector. Measured against that:

| goal | state | evidence |
|---|---|---|
| Faster than SQLite | **Yes on Windows at 100k rows and up, on the bar, not above it.** Weighted geomean 3.11x at medium, lower bound 3.09x against a 3.00x bar, one 30-round run (task-1838 §4; the four-run qualification is owed); 3.72x at large; 2.29x at small. On Linux the same binary is 1.42x at medium. | task-1838 §4, task-1834 §5h |
| No family slower than SQLite | **Not yet: three families, down from six.** At medium over 30 rounds the lower bounds are `open.prepare` 0.79x, `schema` 0.56x, `extension` 0.50x. `write`, `transaction`, `read.range` and `large.values` have cleared. The contract's 1.00x floor is not met. | task-1838 §4 |
| Same features as SQLite | **Not yet, but the list is short.** The new engine runs 46 of 50 inventoried constructs. Shipped by task-1838: trigger firing and foreign key enforcement (immediate and deferred), `LEFT`/`RIGHT`/`FULL OUTER JOIN`, derived tables in `FROM`, recursive CTEs, correlated subqueries, user-defined scalar and aggregate functions and collations, and the `VIRTUAL` generated column that used to shift later columns. Still missing: temp tables, `ATTACH`, `VACUUM`, plain `EXPLAIN`, and the table-valued pragma form. | task-1838 §1-3, `new_engine_surface.rs` |
| Same durability and isolation | **Yes, single process, one writer.** WAL with group commit, snapshot isolation, ARIES-style redo recovery, undo for `ROLLBACK`/`SAVEPOINT`, crash campaigns under a deterministic simulator. Multi-process access and SQLite's file format are deliberate non-goals. | task-1832, task-1816 |
| Embedding search like pgvector | **Yes, and graded better than pgvector on 15 of 17 primary comparisons** with zero worse; in production on a 598,560-chunk mailbox at recall 1.000 and 27 ms p95. Reachable from SQL only through the `inillucent_search` virtual table: there is no `vector` column type or distance operator in the grammar yet. | `inillucent-scorecard.md`, task-1775 |
| Old engine deleted, one engine shipped | **No.** The SQLite-file-format engine (`inillucent-storage`, `-transaction`, `-vm`, `-session`, the `inillucent` facade, `inillucent-capi`) is still in the tree and is still what `inillucent::Database::open` reaches. The CLI and the migrator run on the new engine. | code audit, task-1834 §8 |

The improvement plan for every row that is not green is `tasks/rust-db-phase-2-tdd.md`.

## Features

### The relational engine

**SQL that runs today** (task-1834 §5m inventory, `crates/inillucent-compat/tests/new_engine_surface.rs`):
`SELECT` with inner and cross joins (hash join, index nested loop, and a scan), `GROUP BY`, `HAVING`,
`DISTINCT`, `ORDER BY`, `LIMIT`/`OFFSET` (constants), compound selects (`UNION`, `UNION ALL`,
`EXCEPT`, `INTERSECT`), non-recursive CTEs, uncorrelated subqueries in `WHERE`, `IN`, `EXISTS`, and
as values, window functions with all three frame units, `LIKE`/`GLOB`, `CAST`, `COLLATE` (`BINARY`,
`NOCASE`, `RTRIM`), 39 scalar, 27 math, 7 date-time and 26 JSON functions, `INSERT`/`UPDATE`/`DELETE`
with `RETURNING` and `ON CONFLICT DO UPDATE`, `CREATE TABLE` (including `WITHOUT ROWID` with a primary
key, `STRICT` accepted), `CREATE INDEX` through a bottom-up bulk builder, `CREATE VIEW`,
`CREATE VIRTUAL TABLE`, `DROP`, all four `ALTER TABLE` forms, `ANALYZE` writing `sqlite_stat1`,
`REINDEX`, `EXPLAIN QUERY PLAN` in SQLite's idiom, `BEGIN`/`COMMIT`/`ROLLBACK`, `SAVEPOINT`/`RELEASE`/
`ROLLBACK TO`, `sqlite_schema` and `sqlite_master`, and the pragmas `table_info`, `table_xinfo`,
`table_list`, `index_list`, `index_xinfo`, `database_list`, `page_size`, `page_count`,
`freelist_count`, `cache_size`, `synchronous`, `busy_timeout`, `integrity_check`, `quick_check`,
`wal_checkpoint`.

**Extensions**: JSON over a binary form, FTS5 with `bm25()`, the R-Tree, `json_each`,
`generate_series`, and `inillucent_search`. Their shadow tables are ordinary trees, so they commit
and roll back with the transaction.

**Storage and durability**: 32 KiB pages (8 to 64 allowed), a buffer pool of 4,096 frames (128 MiB)
by default with a cooling FIFO and pointer swizzling, double-written meta pages, a segmented redo WAL
(`RDBWAL01`) with crc32c on every record, `synchronous` `OFF`/`NORMAL`/`FULL`, group commit,
fuzzy checkpoints, recovery on every open, snapshot isolation with a version log and garbage
collection, one writer at a time with `busy_timeout`, an undo buffer for rollback of rows and
schema, and blob extents for values wider than a leaf. Every open replays the log; a database
closed without a checkpoint is readable again (task-1834 §9).

**Tooling**: `inillucent-shell`, a `sqlite3`-shaped shell whose fifteen-script parity suite passes
12 of 15 byte-for-byte against the pinned `sqlite3`; `inillucent-migrate`, which imports a SQLite
file or a legacy retrieval index into an `.rdb` by copy, verify by count and digest, and publish by
rename; and `inillucent-fullgate`, `inillucent-readgate`, `inillucent-searchgate` and
`inillucent-probeprofile`, the paired benchmark instruments.

**Assurance**: 2,102 test functions across 28 crates; a differential harness that runs the same SQL
through the pinned SQLite 3.53.4 and compares transcripts; a SQLLogicTest subset; a `BTreeMap`
model reference driven by operation traces; a deterministic fault-injecting VFS (`inillucent-sim`)
under the pool, the log and the transaction engine; eight libFuzzer targets over the codecs; 100%
branch coverage held on the pool's interior, latch, meta, extent, free map and swip modules and the
tree's key codec; every governed crate denies `unwrap`, `expect`, `panic` and slice indexing, and
22 of 28 forbid `unsafe`.

**What it refuses, by name** (a refusal, never a wrong answer, except the one marked):

| construct | state |
|---|---|
| `LEFT`, `RIGHT`, `FULL` outer joins | refused in the physical pass |
| foreign key enforcement | **not enforced**; the binder builds the triggers, the executor cannot run a trigger |
| `CREATE TRIGGER` | refused on purpose, because a stored trigger would never fire |
| recursive CTEs, a derived table in `FROM` | refused |
| a correlated subquery used as a value | refused |
| `CREATE TEMP TABLE`, `temp.` objects, `ATTACH`, `DETACH` | refused |
| `VACUUM`, `VACUUM INTO` | refused |
| user-defined functions and collations | no registration path on the new connection |
| `LIMIT`/`OFFSET` bound to a parameter, `UPDATE ... FROM`, `WITH` on DML, partial and expression indexes, `CREATE TABLE ... AS SELECT`, row values | refused in the binder |
| a `VIRTUAL` generated column | **wrong answer**: every later column reads one place early (task-1834 §13) |
| `STRICT` type-class enforcement, `ALTER TABLE ADD COLUMN ... DEFAULT` on existing rows, views carried by the SQLite importer, `PRAGMA table_info` on a view, table-valued pragmas, plain `EXPLAIN` | missing or refused |
| a second process on the same file, a second writer, SQLite's file format, the C ABI on the new engine | design non-goals of task-1816 |

### The retrieval engine

Storage with dictionary-encoded filter columns; cosine over L2-normalised vectors; exhaustive search
chosen by a cost model when the filter is narrow; an HNSW graph (m 16, ef_construction 64) whose
traversal honours a predicate; int8 scalar quantisation with full-precision rescoring; an inverted
index with BM25, Snowball stemming, identifiers kept whole, and coverage, proximity, phrase, tier and
prefix weights each with an off switch; reciprocal rank fusion and two score-based fusions; a
`confidence` on absolute bounds beside every `score` so the engine can say "nothing here answers
that"; generation-directory persistence with a 5.3 s reopen; the `nomic-embed-text-v1.5` embedder in
process through ONNX Runtime, on CPU or one or more GPUs; a model manifest and cache header so two
embedding models can be graded on identical corpora; and a grading harness that drives inillucent and
PostgreSQL through one interface. The engine's own documentation follows the comparisons below.

## How it compares with SQLite 3.53.4

Every number in this section was reported by a sprint ticket and is reproducible from the raw gate
output under `_agent_output/`. The instrument is `inillucent-fullgate`: the same SQL, the same data,
the same `synchronous = FULL`, the same transaction boundaries, a matched cache budget on both arms,
interleaved A/B, 30 paired rounds, and every workload's answer digested and compared with SQLite's
before a timing is allowed to count. The ratio is SQLite time over inillucent time, so above 1.00x
means inillucent is faster. The bar is the **lower 95% bound**, not the centre. Neither arm uses a plan
cache: `open.prepare` compiles inside the clock on both sides, and every other workload prepares once
and rebinds on both sides (task-1834 §5c).

### Speed: the headline

The contract (`compat/perf/contract.toml`) asks for a weighted geometric mean of at least **3.00x**
at medium scale (100,000 rows) and **no family below 1.00x**.

| scale | rows | Windows x64 weighted | lower bound | Linux x64 (WSL2) weighted | lower bound | bar |
|---|---|---|---|---|---|---|
| small | 5,000 | 2.30x | 2.29x | 1.16x | 1.15x | 3.00x |
| **medium** | 100,000 | **3.05x to 3.14x** (four runs) | **3.00x to 3.08x** | 1.45x | 1.42x | 3.00x |
| large | 600,000 | 3.83x | 3.72x | 1.72x | 1.68x | 3.00x |

At medium on Windows the lower bound cleared 3.00x on all four thirty-round runs (3.08, 3.01, 3.08,
3.00). It sits on the bar rather than above it, and a single run had already been retracted once for
landing on the wrong side of it, so the spread is reported rather than one number. On Linux the same
binary is under half of that, and the cause is the denominator: SQLite is three to nine times faster
on Linux on every per-statement workload while inillucent is 8 to 53% faster, so the ratio falls
although inillucent itself got quicker. A speed claim about this engine has to name the platform and
the scale. (task-1834 §5e, §5h)

### Speed: per family, lower bounds, Windows x64, final Phase 5 build

| family | weight | what it measures | small | medium | large | bar | medium |
|---|---|---|---|---|---|---|---|
| `read.point` | 0.16 | one row by rowid, integer key, secondary index | 25.82x | **19.13x** | 20.23x | 2.00x | MET |
| `read.range` | 0.12 | selective ranges, forward and reverse, covering and not | 3.04x | **3.25x** | 3.72x | 3.00x | MET |
| `read.join` | 0.08 | two and four table joins | 2.65x | 2.81x | 2.61x | 3.00x | missed |
| `read.analytical` | 0.10 | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | 4.34x | 4.87x | 4.12x | 5.00x | missed |
| `write` | 0.20 | insert, update, delete, upsert, with and without indexes | **0.38x** | **1.73x** | 5.91x | 1.50x | MET |
| `transaction` | 0.10 | autocommit, small batches, large batches, savepoints | 0.93x | **1.09x** | 0.97x | 1.00x | MET |
| `large.values` | 0.04 | text and blobs across the inline/overflow boundary | 8.09x | **8.74x** | 4.58x | 1.50x | MET |
| `open.prepare` | 0.08 | parse, bind, step one row, reset | 0.67x | **0.70x** | 0.72x | 5.00x | under the floor |
| `schema` | 0.04 | `CREATE INDEX` and its backfill | 1.13x | **0.54x** | 0.57x | 3.00x | under the floor |
| `extension` | 0.08 | JSON, FTS5, R-Tree | 0.34x | **0.35x** | 0.53x | 1.50x | under the floor |

The read-only gate, which imports the fixture once rather than once per round, reads the read
families higher and is the number to compare across phases: `read.point` 29.17x, `read.range` 4.44x
(low 3.59x), `read.join` 4.87x (low 3.32x), `read.analytical` 5.95x (low 5.17x) at medium, and a
warm rowid `PointProbe` of 300 ns against a 500 ns bar (task-1819).

The workloads that hold the slow families down, medium unless stated:

| workload | inillucent | SQLite | ratio | cause, as diagnosed |
|---|---|---|---|---|
| `prepare.trivial` (`SELECT 1`, compiled per call) | 1,270 ns | 482 ns | 0.31x | the binder's fixed cost; parse 332, bind 409, build 372, run 163 ns (task-1834 §5i) |
| `txn.large` (2,000 `UPDATE`s in one transaction) | 2.7 ms | 0.80 ms | 0.24x | per-update constant on the allocating read path plus delete-and-insert into the delta area; not compaction (§5b) |
| `write.insert.batch` at small | 43.9 ms | 5.4 ms | 0.12x | a per-write constant; the delta limit is at its measured optimum (§5g) |
| `extension.fts.build` | 23.7 ms | 2.78 ms | 0.11x | 22.6 of 23.7 ms inside the module; the engine's own floor is 4.0 ms, which is already 0.73x (task-1833) |
| `extension.rtree.insert` | 21.7 ms | 2.56 ms | 0.12x | same shape as FTS5's build (task-1833) |
| `extension.json` | 4.42 ms | 1.18 ms | 0.26x | 1.1 µs per `json_extract` call against 0.4 (task-1833) |
| `schema.index` | 19.7 ms floor | 10.1 ms budget | 0.54x | the bulk build's floor sits above the bar (task-1833) |

### Speed: absolute time, medium, nanoseconds per round (task-1834 §5h)

| workload | inillucent Windows | inillucent Linux | SQLite Windows | SQLite Linux |
|---|---|---|---|---|
| `point.rowid` (4,000 lookups) | 2,283,900 | 1,681,472 | 47,996,350 | 7,548,543 |
| `point.miss` | 1,236,300 | 885,384 | 45,577,150 | 6,227,480 |
| `point.index` | 3,928,150 | 3,075,357 | 49,998,450 | 8,735,701 |
| `range.covering` | 3,036,100 | 2,757,800 | 15,841,950 | 4,907,549 |
| `join.selective` | 1,530,700 | 1,294,602 | 24,715,150 | 4,207,326 |
| `large.read` | 894,950 | 584,792 | 28,362,250 | 3,098,046 |
| `scan.aggregate` | 7,369,200 | 6,817,120 | 92,845,350 | 80,730,183 |
| `write.insert.autocommit` (one fsync per row) | 20,283,850 | 73,237,198 | 134,440,400 | 164,466,748 |

### Memory

| | inillucent (new engine) | SQLite 3.53.4 |
|---|---|---|
| page size | 32 KiB default, 8 to 64 KiB | 4 KiB default |
| cache | a buffer pool of frames times page size; 4,096 frames = 128 MiB by default, set at open. The budget is a **ceiling**: a frame's page is allocated the first time that frame is claimed (task-1838) | `cache_size`, 2 MiB by default, also grown into |
| what the gates matched | 32 MiB on both arms (write and full gates), 128 MiB on both (read gates) | same |
| a transaction larger than the pool | must fit: the pool is no-steal, so dirty pages cannot be evicted before commit; documented limit (task-1816) | spills to the journal |
| a `SELECT` result | materialised on the first `step`; `Statement::step` walks rows already produced (task-1834 §9) | streamed one row per `step` |

**Measured, task-1838.** Two instruments, one defect they found, and one they exposed.

`inillucent-fullgate` now runs this engine in a **child process of its own**, so both arms are whole
processes measured the same way. Each child opens a finished database the parent built and runs one
round of the same plan; neither figure is a delta. At medium with a 4,096-frame 32 KiB pool matched
to a 128 MiB SQLite cache:

| | peak working set | user CPU | kernel CPU |
|---|---|---|---|
| `sqlite-bench` (whole child) | 37.2 MiB | 461 ms | 570 ms |
| inillucent (whole child) | 82.5 MiB | 313 ms | 78 ms |

**2.22x SQLite's peak at a matched cache budget.** The first reading was **4.75x**, and the
difference is a defect this measurement found: the buffer pool allocated and zeroed every frame at
open, so a 128 MiB budget was 128 MiB of resident memory from the first statement whether the
database needed it or not. Three pool sizes made the cause unambiguous - 256 frames 55.5 MiB, 1,024
frames 80.7 MiB, 4,096 frames 176.9 MiB, moving byte for byte with the budget. Frames now allocate on
first claim (`Pool::claim_frame`), which is how SQLite grows into `cache_size`; the child's peak fell
to 82.5 MiB and every family's ratio stayed inside the run-to-run spread. What is left is the ~33 MiB
of pages this fixture actually touches plus the gate binary, which is a much larger program than
`sqlite-bench` and is counted here.

`inillucent-shellrss` asks the same question of two shells rather than two harnesses, and gets a
worse answer for a reason worth stating. Each shell builds its own copy of the same 200,000-row table
from the same SQL - there is no one file both can open - checkpoints it, then opens it fresh and runs
a count, a point lookup, a grouped aggregate and a sum over every row:

| shell | peak working set | user CPU | kernel CPU |
|---|---|---|---|
| `sqlite3` 3.53.4 | 7.20 MiB | 15.6 ms | 15.6 ms |
| `inillucent-shell` | 65.05 MiB | 93.8 ms | 46.9 ms |

**9.0x, and none of it is the reads.** Running only `SELECT 1` against the same file costs the same
65.03 MiB, so the figure is the cost of *opening* what this engine wrote. The file is the reason: the
same 200,000 rows are 10.7 MB as a SQLite database and **221 MB** as an `.rdb`, because inserting from
a query puts every row through a leaf's delta area and splits the leaf at `DELTA_LIMIT` - 32 rows to a
32 KiB page, where the same table built another way holds 962. `INSERT INTO w SELECT id, v FROM u`
over 100,000 rows is 3,130 pages for 104 pages of data, and it reproduces byte for byte at `5eeb269`,
before this ticket - it is the space half of the `write.insert.batch` 0.35x and `txn.large` 0.18x that
the floor work owes. Without the checkpoint it is worse again: the build leaves 940 MB of WAL segments
and the next open replays all of them, 1,026 MiB and 2.9 s of processor time.

The gate also reports, per round, what this engine costs *inside* the gate process: +9.9 MiB of
working set, 328 ms user, 62 ms kernel. That is a delta over a region rather than a process's peak,
and the gate prints the distinction under the table so the two are not quoted as one measurement.
Per workload the heap moves only where a write does - `write.insert.batch` grows it by 9.9 MiB, every
read workload by 0.00 MiB - and pool residency over a round goes **722 to 1,072 frames**, 22.6 MiB to
33.5 MiB of the 128 MiB budget.

What was already known and still holds: the Phase 1 numbers were taken with inillucent's trees fully
resident against SQLite at a 2 MiB cache and were corrected downwards when the caches were matched
(`read.analytical` 6.25x to 4.10x, `scan.sort` 7.58x to 3.20x, task-1817), so every number above is
under a matched budget. On disk the Phase 3 fixtures - which are **imported**, not built through
`INSERT` - are 16.8 MB as a SQLite file and 23.7 MB as an `.rdb` at medium, 93.7 MB against 131.2 MB
at large: about 1.4x, at 32 KiB page granularity. The 20x above is what the write path does to a
database built with SQL, and the two numbers are not in conflict; they are two paths into a file.

### CPU

Both engines run a statement on one thread, so every ratio above is also a ratio of CPU time on the
CPU-bound families. `read.point`, `read.range`, `read.analytical`, `read.join` and `large.values` are
CPU-bound; `write.insert.autocommit`, `txn.autocommit` and `large.write` are bounded by one `fsync`
per commit under `synchronous = FULL`, and both engines pay it identically (20.3 ms against 134.4 ms
for 2,000 autocommit inserts on Windows, both far slower on WSL2). The new engine is single-threaded
by construction: its pool and trees are `RefCell`, a `Connection` borrows the `Database`, and there is
no parallel scan (task-1816 lists parallel scans as after-scope).

**Measured, task-1838.** One round of the medium plan, each engine in its own child process:
inillucent spends **313 ms user and 78 ms kernel**, `sqlite-bench` **461 ms user and 570 ms kernel**.
The user-time ratio, 1.47x, is the one that tracks the wall-clock headline, because both arms are one
thread with no idle in it. The kernel time is the interesting half: SQLite pays **7x** what this
engine pays, which is where a WAL that writes whole 32 KiB pages once per commit differs from a
rollback journal plus a WAL under `synchronous = FULL`.

Per-workload CPU is reported too, but **it is quantised to the Windows scheduler tick (15.625 ms)**
and the gate says so above the table: a workload that runs for 3 ms reads as either 0 or 15.6 ms, so
only the per-round totals should be quoted. Neither engine is reported as a percentage of a core:
both are one thread, so a percentage would only restate the wall clock.

### Features and semantics

| | inillucent (new engine) | SQLite 3.53.4 |
|---|---|---|
| SQL dialect | SQLite's; 60 of 60 grammar productions parse (`compat/syntax-report.md`) | reference |
| qualification against the pinned oracle | 44 of 50 inventoried constructs run; repointed suites 24 pass / 81 fail (60 engine gaps, 16 tests of the dropped file-format requirement, 5 chosen refusals); read-only SLT subset 110 accepted, 0 divergent, 37 refused; differential corpus 244 queries, 227 agreed, 16 refused, 1 dialect | reference |
| file format | its own (`.rdb` + `RDBWAL01` segments); SQLite files are imported, not opened | SQLite |
| journal modes | WAL only; `journal_mode` answers `wal`, `locking_mode` answers `exclusive` | DELETE, TRUNCATE, PERSIST, MEMORY, WAL, OFF |
| processes on one file | one; no OS file lock is taken on the new path | many, byte-range locks |
| writers | one at a time, readers never block (snapshot isolation) | one at a time; readers block in rollback mode, not in WAL |
| threads | single-threaded | serialised or multi-thread |
| rollback | undo buffer of before-images, rows and schema; a `DROP` cannot be undone inside a transaction yet | rollback journal or WAL |
| triggers, foreign keys | yes; `BEFORE`/`AFTER`/`INSTEAD OF`, `FOR EACH ROW`, `WHEN`, `RAISE`, recursion capped at 1,000 frames. Foreign keys compile to triggers, so `PRAGMA foreign_keys`, `DEFERRABLE INITIALLY DEFERRED`, `ON DELETE CASCADE`/`SET NULL`/`SET DEFAULT`/`RESTRICT` and `PRAGMA foreign_key_check` all run through the one mechanism (task-1838 §1) | yes |
| extensions | JSON, FTS5, R-Tree, `json_each`, `generate_series`, `inillucent_search` | JSON1, FTS3/4/5, R-Tree, geopoly, session, RBU, ... |
| C API | `inillucent-capi` exports 133 `sqlite3_*` symbols, over the **old** engine; a driver and C ABI for the new engine is task-1837 | `sqlite3.h` |
| shell | `inillucent-shell`, 12 of 15 scripts byte-identical to `sqlite3` | `sqlite3` |

The **old** engine, still in the tree, is the one that reached SQLite file-format parity:
`compat/sqlite-3.53.4.toml` holds 274 capabilities of which 267 pass on both Windows and Linux and 7
optional ones are missing (session extension, pre-update hook, snapshot API, unlock-notify, RBU,
geopoly, R-Tree geometry callbacks). It was measured at 0.05x to 0.70x SQLite across the families
(task-1816's "today" column), which is why the rearchitecture happened.

## How it compares with PostgreSQL + pgvector

The retrieval engine is graded by `inillucent-bench` against a correctly configured PostgreSQL with
pgvector (`hnsw.iterative_scan = relaxed_order`, `ef_search 400`, `max_scan_tuples 40000`,
`scan_mem_multiplier 4` on filtered queries), reading byte-identical vectors, over 2,613 queries in
nine families on a 185,078-chunk corpus this repository builds from public data. Each primary
comparison is decided by a 95% paired bootstrap interval and a paired randomisation test against a
threshold declared before the run.

**17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates all
pass.** (`inillucent-scorecard.md`, generated 2026-08-31)

| family | measurement | inillucent | best pgvector |
|---|---|---|---|
| Hybrid | document identity, nDCG@10 | **0.9756** | 0.8148 |
| Hybrid | natural language headings, nDCG@10 | **0.7477** | 0.6271 |
| Passage | passage evidence, graded nDCG@10 | **0.7045** | 0.6027 |
| Passage | one transposed character, graded nDCG@10 | **0.6794** | 0.3969 |
| Passage | three keywords, graded nDCG@10 | **0.6266** | 0.4651 |
| Multi-source | evidence in two sources, evidence recall@10 | **0.6237** | 0.1923 |
| Abstention | questions with no answer, confident answer rate | **0.0050** | 1.0000 |
| Lexical | natural language headings, MRR | **0.7221** | 0.5896 |
| Lexical | rare identifiers, MRR | **0.5442** | 0.1357 |
| Filtered | `source = jira`, recall@10 in filter | **1.000** | 0.3280 |
| Latency | no predicate, p50 | **0.704 ms** | 1.519 ms |

Speed and footprint on the same corpus (`product-overview.md`):

| | inillucent | PostgreSQL + pgvector |
|---|---|---|
| unfiltered search, p50 / p95 | **0.77 / 1.76 ms** | 3.04 / 5.45 ms |
| filtered to a minority source, p50 / p95 | **1.65 / 2.64 ms** | 45.06 / 66.80 ms |
| serving an index, peak memory | **1.86 GB** (int8 vectors) | a server process plus its indexes; not measured on this corpus, 3,167 MB on the 598k-chunk one below |
| building an index, peak memory | 2.68 GB | |
| on disk | 819 MB | |
| build / save / reopen | 175 s on one core / 0.3 s / 5.3 s | |
| processes to run | none | PostgreSQL plus an embedding server |

In production (task-1774, task-1775, task-1779): Nikaya, a Gmail retrieval assistant, moved its
598,560-chunk mailbox from pgvector to inillucent. Semantic p50 went from 80.6 ms cold / 33.7 ms warm
to **4.41 ms**; the lexical branch went from returning **zero rows on 17 of 30** natural-language
questions to zero on none; recall@100 against an exact scan went from 0.899 (min 0.77) to **1.000**,
because the deployed configuration answers every search on the parallel exhaustive path at
**27.1 ms p50 / 28.6 ms p95** over the whole corpus, seven times faster than the pgvector branch it
replaced. Cost: **3.80 GB resident** and 2.40 GB on disk for the index, a 9 min 27 s single-threaded
build, against 3,167 MB of pgvector and GIN index deleted from a 5,849 MB database. The MCP process
that used to open its own copy of the index (3,831.6 MB) now asks the server and holds 13.2 MB.

The retrieval side is a library with a Rust API and a virtual table; it is not a `vector` column
type. What pgvector offers as `CREATE INDEX ... USING hnsw`, `<=>` and `ORDER BY ... LIMIT k` is
reached here as `CREATE VIRTUAL TABLE t USING inillucent_search(..., dims = 768)` and
`WHERE t MATCH ? AND vector = ? AND k = ?`; the gap is named in the Phase 2 TDD.

## What is not there yet

In priority order, each with a failing test or a measured number already in the tree. The plan for
each is `tasks/rust-db-phase-2-tdd.md`. Items 1 through 4 and 7 were closed by task-1838 and are
struck below rather than deleted, so the list still reads as a record of what was owed.

1. ~~**Two silent wrong answers**: the `VIRTUAL` generated column shift, and foreign keys accepted
   and not enforced.~~ Both fixed, task-1838 §1.
2. ~~**Triggers**, which are also the foreign-key mechanism.~~ Shipped, task-1838 §1.
3. ~~**Outer joins**, derived tables in `FROM`, recursive CTEs, correlated subqueries as values.~~
   Shipped, task-1838 §2.
4. ~~**User-defined functions and collations** on the new connection.~~ Shipped, task-1838 §3.
5. **The floor**: `open.prepare` 0.79x, `schema` 0.56x, `extension` 0.50x at medium; `write` at
   small; `txn.large` 0.18x. The space cost that came with them is fixed: an append no longer splits
   a leaf that is not full, so a page holds what it can rather than `DELTA_LIMIT` rows
   (task-1838 §4).
6. **Linux**: 1.45x weighted where Windows is 3.02x, because SQLite is much faster there and the new
   engine is not much faster there.
7. ~~**Process-level memory and CPU** for both engines, unmeasured.~~ Measured, task-1838 §6: see
   [Memory](#memory) and [CPU](#cpu). 1.31x SQLite's peak working set for the same data, 1.43x its
   user CPU per gate round, one tenth its kernel time.
8. **Temp tables and `ATTACH`**, `VACUUM`, `STRICT` enforcement, plain `EXPLAIN`, table-valued
   pragmas. (`ADD COLUMN ... DEFAULT` backfill and views on import were done in task-1838 §1-2.)
9. **Vector search as SQL**: a vector type, distance functions, `ORDER BY distance LIMIT k` planned
   onto the retrieval engine.
10. **Deleting the old engine** and re-rooting `inillucent::Database` onto the new one.
11. **The retrieval index's footprint**: 3.80 GB resident for 598k chunks, BM25 rebuilt on load,
    single-threaded graph build.
12. **Multi-process and multi-thread access**, deliberate non-goals of task-1816 that a SQLite
    replacement will eventually be asked about.

## Repository layout

| group | crates | non-test lines |
|---|---|---|
| shared foundation | `inillucent-base`, `inillucent-vfs`, `inillucent-value`, `inillucent-sim` | 12,003 |
| shared SQL front end | `inillucent-sql` (lexer, parser, binder, planner), `inillucent-scalar` (functions, JSON, window frames), `inillucent-catalog`, `inillucent-ext` (registry, vtab contract, FTS5, R-Tree) | 32,221 |
| new engine | `inillucent-pool`, `inillucent-wal`, `inillucent-tree`, `inillucent-txn`, `inillucent-exec`, `inillucent-engine`, `inillucent-model` (test oracle), `inillucent-sqlite-reader` (import only) | 38,944 |
| old engine, to be deleted | `inillucent-storage`, `inillucent-transaction`, `inillucent-vm`, `inillucent-session`, `inillucent` (facade), `inillucent-capi` | 42,661 |
| retrieval | `inillucent-core` (the engine), `inillucent-search` (the virtual table), `inillucent-bench` (the pgvector grading harness) | 21,438 |
| tooling | `inillucent-compat` (manifest, oracle, gates, 65 test files), `inillucent-cli`, `inillucent-migrate` | 28,242 |

`inillucent-engine::connect::Database` is the entry point to the new engine: `open` creates or
opens-and-recovers, `import` reads a SQLite file, `connect` gives a `Connection` with
`execute_batch`, `query`, `prepare_with_tail` and `explain`. `inillucent::Database` is still the old
engine's facade. `architecture.md` and `product-overview.md` describe the retrieval engine;
`tasks/task-1816-rearchitecture-tdd.md` is the design the new engine follows;
`docs/invariants/layering.toml` is the dependency contract a test enforces.

## Building, testing and reproducing the numbers

```sh
cargo build --release
cargo test --workspace --no-fail-fast        # 168 binaries, 2,099 tests; 81 are red on purpose (task-1834 §13)

pwsh tools/sqlite-reference.ps1              # the pinned SQLite 3.53.4 oracle (Windows)
bash tools/sqlite-reference.sh               # Linux

# The fixtures are not checked in: _agent_output/task-1819-readgate/reproduce/build-fixtures.sh
F=_agent_output/task-1832-phase3/fixtures
target/release/inillucent-fullgate $F/medium.db --scale medium --rounds 30 --page-size 32768 --frames 4096
target/release/inillucent-readgate $F/medium.db --scale medium
target/release/inillucent-probeprofile $F/medium.db --scale medium --page-size 32768 --frames 4096
target/release/inillucent-searchgate --documents 500 --rounds 30
target/release/inillucent-shellrss                   # peak RSS, one shell each, same data
target/release/inillucent-prepareprofile $F/medium.db --iterations 4000   # where a compile goes
target/release/inillucent-shell my.rdb

cargo run -p inillucent-compat --bin inillucent-manifest -- check      # validate the parity manifest
cargo run -p inillucent-compat --bin inillucent-manifest -- report     # regenerate compat-report.{json,md}
cargo run -p inillucent-compat --bin inillucent-manifest -- layering   # check the dependency contract
```

`cargo test --workspace` without `--no-fail-fast` stops at the first failing binary and has reported
about a quarter of the suite; use the flag. The qualification suites skip rather than fail when the
oracle at `.sqlite-ref/3.53.4/` is absent, so build it first.

---

The rest of this document is the retrieval engine's own documentation: how it is graded, the corpus
it is graded on, how to run the comparison, the embedding pipeline, and how to compare embedding
models.

## Where the retrieval engine stands against pgvector

On the corpus this repository builds, over 2,613 queries in nine families, against the better of the
two pgvector configurations:

**17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates:**
**all pass.**

Every family declares one measurement that is judged; the rest are diagnostics that are printed and
do not vote. A comparison is called *better* only when a 95% paired bootstrap interval over the
per-query scores clears both zero and a practical threshold declared before the run. The one
*equivalent* is a source where both engines reach recall 1.000 within the filter and neither can do
better. The one *inconclusive* is confluence filtered recall, where inillucent leads 0.9960 to 0.9760
and the interval runs 0.0000 to 0.0440 on 25 queries — a lead the run declines to call a win.

| family | measurement | inillucent | best pgvector |
|---|---|---|---|
| Hybrid | document identity, nDCG@10 | **0.9756** | 0.8148 |
| Hybrid | natural language headings, nDCG@10 | **0.7477** | 0.6271 |
| Passage | passage evidence, graded nDCG@10 | **0.7045** | 0.6027 |
| Passage | one transposed character, graded nDCG@10 | **0.6794** | 0.3969 |
| Passage | three keywords, graded nDCG@10 | **0.6266** | 0.4651 |
| Multi-source | evidence in two sources, evidence recall@10 | **0.6237** | 0.1923 |
| Abstention | questions with no answer, confident answer rate | **0.0050** | 1.0000 |
| Lexical | natural language headings, MRR | **0.7221** | 0.5896 |
| Lexical | rare identifiers, MRR | **0.5442** | 0.1357 |
| Filtered | `source = jira`, recall@10 in filter | **1.000** | 0.3280 |
| Latency | no predicate, p50 | **0.704 ms** | 1.519 ms |

The abstention row is the one worth pausing on. Given a question that nothing in the corpus answers,
the baseline returns a confident top result every single time; inillucent does it on one query in two
hundred. That is not a ranking difference, it is the difference between a system that can say "no"
and one that cannot — and it is the failure that never announces itself, because ten confident
looking passages about nothing look exactly like ten good ones.

The full card, including every diagnostic, every interval and every measurement that is a inillucent
setting rather than a comparison, is in [inillucent-scorecard.md](inillucent-scorecard.md).

### What the lexical side does that plain BM25 does not

PostgreSQL full text search has two properties BM25 lacks, and both of them matter on a corpus this
size. `to_tsquery` joins query terms with `&`, so a chunk missing one word never appears at all; and
`ts_rank_cd` is cover density ranking, so a chunk whose query terms sit close together outranks one
that mentions the same words in different paragraphs. Scoring any term with BM25 finds far more of
the right chunks — 49.5 rows of 50 against 6.7 — and puts them lower.

inillucent keeps the recall and takes the two properties as gradients rather than gates:

| setting | what it does | default |
|---|---|---|
| `lexical_coverage` | scales a score by the share of the query's idf mass the chunk holds, raised to this exponent | 3.0 |
| `lexical_proximity` | scales it by `matched terms / smallest window holding one of each`, blended by this weight | 1.0 |
| `lexical_tier` | rank by how many query terms a chunk holds first, score second — the ordering `&` gives PostgreSQL | off |
| `lexical_prefix` | let a query term match the terms it prefixes, as `:*` does | off |
| `lexical_phrase` | scales a score by whether the matched terms came in the query's own order inside that window, blended by this weight | 0.75 |

`lexical_phrase` is the one `ts_rank_cd` does not have an answer to. Cover density asks how tightly
the terms sit; it does not ask whether they came in the order the question asked them in, and
"offer eligibility rules" and "rules for eligibility of an offer" have the same window width and are
not the same answer.

Every one of them is measured rather than assumed, and every one has an off switch that restores
plain BM25. `lexical_tier` is off because `lexical_coverage` does the same job better where they
disagree, and on because it is what rescues a caller who sets the coverage exponent to 0.
`lexical_prefix` is off because on a 494,000 term dictionary it credits a chunk with holding a query
term it does not hold, which is the exact judgement coverage weighting depends on.

### Choosing those defaults without paying for a graded run

A `grade` rebuilds the index every time and the build is most of the run. Nothing in the ranking
settings needs a new index, so `tune` builds one and sweeps every setting against it:

```sh
./target/release/inillucent-bench tune --cache ~/.cache/inillucent-corpus/corpus.cache \
  --coverages 0,1,2,3 --proximities 0,0.5,1 --weights 0.2,0.35,0.5 \
  --prefixes true,false --tiers true,false --seed-offset 100
```

55 settings in 31 seconds on an 18,685 chunk corpus, against 1 minute 19 for one `grade` of the same
corpus and 4 minutes 21 for one on the full one. `--seed-offset` shifts the query set seeds, so a
setting is chosen on queries the graded run will not use.

## How it is graded

### How a measurement becomes a verdict

The card used to count measurements won, with anything above `1e-4` a win. All three parts of that
were wrong in the same direction. `1e-4` is a hundredth of what one query in ninety changing its
mind moves a mean by, so noise was being counted. Every row got a vote, so nDCG, success@1,
success@10 and reciprocal rank turned one behaviour into four wins. And `rows returned` was scored
higher-is-better, so fifty irrelevant chunks beat ten useful ones.

What replaced it:

- **One primary metric per family.** Everything else is a diagnostic: printed, argued about, never
  voted on.
- **Paired statistics.** Every primary comparison is decided by a 95% paired bootstrap interval and
  a paired randomization test over the per-query scores, both seeded so a verdict is reproducible.
  Smucker, Allan and Carterette found these agree with each other and are the right tests for
  retrieval; both are reported because they answer different questions — the interval says how large
  the difference is, the p-value says whether it could be noise.
- **A practical threshold declared before the run.** 0.01 on the ranking measures, five per cent on
  latency. With enough queries every difference eventually becomes detectable, including differences
  far too small to matter.
- **Four verdicts, not three.** *better* when the interval clears both zero and the threshold,
  *equivalent* when the whole interval sits inside it, *worse* in the other direction, and
  *inconclusive* when the run cannot tell. A run that cannot separate two engines says so. "Both
  engines are at the metric's ceiling" is reported separately from "we cannot tell", because those
  are not the same statement.
- **Completeness is a gate.** Returning thirty rows where fifty exist is still a defect; it is just
  not a relevance win.

### What every run leaves behind

An aggregate card can be read but not interrogated. Every run now writes, beside the card:

```
runs/<unix time>-<commit>/
  manifest.json     commit and dirty flag, corpus file and size, model, device,
                    every query seed, every ranking setting, host, thresholds
  per-query.jsonl   one line per engine per query: the ranking, each hit's
                    relevance grade, the component scores, the latency, and the
                    metrics that query contributed
```

That is what makes the intervals recomputable without repaying the run, lets a miss be looked at
rather than guessed at, and lets a run be re-judged after a relevance judgement is corrected.

### The query families

The first three grade a **document**. This corpus writes each document's title and heading into the
front of every one of its chunks — because the corpus it reproduces did — so a title query is
answered by any chunk of the right page. That is worth grading and it is not what an agent needs,
which is the paragraph.

| family | the query | what counts as correct |
|---|---|---|
| document identity | the document's own title | any chunk of that document |
| heading | a section heading | the chunks under it |
| identifier | a rare literal token | the chunks holding it |
| **passage evidence** | one body sentence, with every word of the chunk's breadcrumb removed so it cannot be answered by the shared title text, and the two rarest remaining words removed as a deliberate vocabulary gap | **graded**: 3 for the passage that answers, 2 for the rest of its document |
| **transposition** | the same queries, two adjacent characters swapped in the rarest word | unchanged, so the gap between the two scores is exactly what the mistake cost |
| **shorthand** | the same queries cut to their three rarest content words | unchanged |
| **multi-source** | two headings from documents in two different sources, joined | both sets are answer bearing, and the family is scored on whether **both** arrived |
| **unanswerable** | distinctive words of two documents from sources the builder draws from disjoint pools | nothing is relevant |

The passage family's remaining bias is stated rather than hidden: its words are still drawn from the
passage it grades. It is a much weaker bias than a title query — the container leak is gone and the
two strongest lexical anchors with it — and the ground truth stays objective, which a generated
paraphrase would not.

### Confidence is a different number from score

The unanswerable family is the failure that does not announce itself: ten confident-looking passages
for a question with no answer, and an agent writes a paragraph out of them. Measuring it needs an
absolute notion of confidence, and per-list min-max normalization destroys one by construction —
it maps the best hit of every list to exactly 1.0, whether the list is good or hopeless.

Theoretical min-max fixes that by dividing each side by a bound the results had no say in: cosine
over normalized vectors is bounded by one, and BM25 by the query's own idf mass at saturation. It
took the confident-answer rate on unanswerable questions from 1.000 to 0.000.

As a *ranker* it lost, and for a structural reason. Its lexical bound assumes some chunk could hold
every query term, and a multi-source question is built so that none can, so the whole lexical side
collapses towards zero and the ranking becomes vector-only: 0.446 against min-max's 0.690 on that
family. That is correct behaviour for a confidence and wrong behaviour for an order.

So the engine stopped asking one number to do both. Every hit carries a `score`, from whichever
fusion ranks best, and a `confidence`, always computed on absolute bounds whatever fusion ordered
the list. The abstention threshold is set on confidence, the ranking is decided by score, and each
engine is calibrated on its own scale against held-out answerable queries — so the comparison
assumes nothing about a inillucent score and a `ts_rank_cd` score meaning the same thing.

## The corpus

The engine is graded on a corpus assembled from public data, built by this repository. Every number on the score card can be reproduced by anyone with this repository, an internet connection and a few hours.

It is synthetic in the sense that matters: the six sources, the documents, the titles, the authors, the spaces, the labels and the identifiers are all constructed by `synth.rs`. The sentences inside the chunks are real public text, because a lexical index scored on generated filler measures nothing. Term frequencies, sentence length, vocabulary growth and the way rare words cluster are properties BM25 depends on, and text from a template has none of them.

| source | stands for | built from | licence |
|---|---|---|---|
| confluence | wiki pages | English and Simple English Wikipedia articles | CC BY-SA 4.0 |
| github | source files | eight repositories in eight languages | MIT, BSD 3 Clause, Apache 2.0 |
| slack | chat threads | Wikipedia Talk and User talk pages | CC BY-SA 4.0 |
| jira | issue threads | GitHub issues from those repositories | factual metadata |
| figma | design files | the longest articles, reformatted as frames and text layers | CC BY-SA 4.0 |
| miro | boards | articles reformatted as clustered notes | CC BY-SA 4.0 |

None of this text is committed here. It is downloaded and rebuilt on demand, which keeps the repository small and satisfies the share alike licences by attribution rather than by redistribution. Each source draws from a disjoint pool: if one article supplied both a page chunk and a design file chunk, a query matching one would match its twin in another source and every filtered measurement would be distorted.

### Building it

```sh
# 1. Download the public material. Wikipedia dumps, eight shallow clones and the
#    issue threads. About 2 GB, mostly the clones.
./scripts/fetch-public-corpus.sh

# 2. Turn it into the compact files the builder reads.
python3 scripts/extract-wikipedia.py ~/.cache/inillucent-corpus/raw ~/.cache/inillucent-corpus/derived
python3 scripts/extract-github.py    ~/.cache/inillucent-corpus/raw ~/.cache/inillucent-corpus/derived

# 3. Assemble the corpus: 186,786 chunks across 39,366 documents.
./target/release/inillucent-bench synth-build --out ~/.cache/inillucent-corpus/corpus.jsonl

# 4. Check it supports every graded scenario, and that those scenarios can be
#    answered rather than only generated. Do this before step 5, which is the
#    step that costs hours: skipping it cost two complete embedding runs.
./target/release/inillucent-bench synth-check --corpus ~/.cache/inillucent-corpus/corpus.jsonl

# 5. Embed it. On the processor this is eight to twelve hours for the full corpus;
#    on one GPU it is minutes. Resumable either way: rerun the same command and it
#    continues where it stopped.
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
./target/release/inillucent-bench synth-embed \
  --corpus  ~/.cache/inillucent-corpus/corpus.jsonl \
  --cache   ~/.cache/inillucent-corpus/corpus.cache \
  --devices cuda:0,cuda:1 --batch 64 --window-batches 16

# 6. Load the same rows and the same vectors into PostgreSQL for the baseline.
createdb -h 127.0.0.1 -p 5433 inillucent_synth
./target/release/inillucent-bench synth-load \
  --corpus ~/.cache/inillucent-corpus/corpus.jsonl \
  --cache  ~/.cache/inillucent-corpus/corpus.cache
```

`--scale` on `synth-build` multiplies every source's document and chunk count while keeping the proportions between sources, so a smaller corpus can be built for a faster cycle and a larger one to test beyond this size. Embedding time scales with it.

The vectors loaded into PostgreSQL are the same bytes the cache holds. Nothing is recomputed, so a difference between the two engines cannot come from the embedder.

## Running the graded comparison

```sh
cargo build --release
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib

# Build an index and report what it built.
./target/release/inillucent-bench build --cache ~/.cache/inillucent-corpus/corpus.cache --quantized

# Run every scenario against inillucent and both pgvector configurations, and write
# the score card. Four and a half minutes on the full corpus, half of it the index
# build. The ranking settings all have flags, and all default to the measured winners.
./target/release/inillucent-bench grade --cache ~/.cache/inillucent-corpus/corpus.cache --per-source 30

# Iterate on inillucent alone, skipping the two pgvector configurations.
./target/release/inillucent-bench grade --cache ~/.cache/inillucent-corpus/corpus.cache --inillucent-only
```

`grade` writes `inillucent-scorecard.md` and, beside it, the same measurements as JSON so the card can be re-rendered or re-judged without repaying the run.

A prefix of this corpus is not a sample of it. Chunks are numbered in ingestion order and that order correlates with source, so `--limit N` gives nearly all one source. `strided_sample` exists for this reason, and `synth-check` asserts the property still holds.

Defaults: `--database-url postgres://127.0.0.1:5433/inillucent_synth`, `--model-dir ~/.cache/inillucent-models/nomic-embed-text-v1.5`.

## Tests

```sh
cargo test --release
```

The engine's own tests cover the pieces a search engine gets quietly wrong: that filtered traversal returns a full result set on a minority source, that filtered recall against exhaustive cosine stays high, that a filter naming a value the corpus lacks selects nothing rather than everything, that stemming matches what PostgreSQL's `english` configuration produces, that BM25 saturates term frequency and normalizes for document length, that quantization keeps the ranking, and that a saved index answers the same queries after loading. It also covers the ranking work the score card turns on: that reduce-as-you-go top k selection agrees with sorting every candidate, on ties as well; that coverage weighting raises a chunk holding the whole query and leaves a one term query alone; that proximity prefers the chunk whose terms sit together and that its weight is a real off switch; that the smallest covering window is found, including when the best one is at the end; that tiering puts every-term matches first and still returns the partial ones; that the two score based fusions normalise in the documented way; and that a batch never exceeds the attention budget, never drops a text and never mixes lengths.

The corpus builder's tests cover what makes a corpus usable for grading rather than merely large: that chunks stay near their target length and none swallows the rest of its document, that chunking never splits a word or a line of code, that the article pool is shared so no source is starved, that chunk counts per document reproduce the measured quantiles, and that authors are unique and stable across rebuilds.

The relational engine's crates are tested separately, and quickly:

```sh
cargo test -p inillucent-base -p inillucent-vfs -p inillucent-sim -p inillucent-compat
```

They cover what a storage layer gets quietly wrong. Every codec is exercised with hundreds of
thousands of seeded random inputs and must return an error rather than panic on any of them. The
locking protocol is proved across two real processes, not two handles in one, because POSIX advisory
locks are per process and a same-process test would pass on a broken implementation. A dead process
must release its locks. The simulator must lose an unsynced write sometimes and a synced one never,
over every seed. A recorded schedule must replay an event-for-event identical trace. And the same
26-case conformance suite runs against the in-memory VFS, the real file system, and the simulator,
so "the simulator behaves like a disk" is a checked claim rather than a hope.

## Using the engine

```rust
use inillucent_core::filter::Filter;
use inillucent_core::index::{Index, IndexConfig};

let mut index = Index::new(IndexConfig { dims: 768, quantized: true, ..Default::default() });
index.add(chunks, &vectors);   // one vector per chunk
index.commit();                // builds the graph, the lexical index and the codes

let filter = Filter::source("slack");
let compiled = index.compile(&filter);
let hits = index.hybrid_search("how does the release process work", &query_vector, &compiled, 10, None);
```

`exhaustive_search` is a first class query path, not a test fixture. It is exact, the cost model routes selective predicates to it because at that size it is also faster, and it is the reference every accuracy number is measured against.

## Embeddings

The engine takes vectors and never embeds anything itself, which is what lets the harness give both engines identical vectors. `embed.rs` defines the boundary and carries the `search_document: ` and `search_query: ` prefixes `nomic-embed-text-v1.5` is trained with, plus the Matryoshka widths the model supports.

`embed_onnx.rs` runs the model as ONNX through `ort`, in process, with no child process and no HTTP hop. It sits behind the `onnx` feature so the engine stays free of a native library for callers that supply their own vectors. Texts are sorted by length before batching, because every sequence in a batch is padded to the longest one in it and the corpus runs from 6 to 6227 characters a chunk.

It needs the weights and the ONNX runtime library present:

```sh
brew install onnxruntime
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
# weights in ~/.cache/inillucent-models/nomic-embed-text-v1.5/
#   model.onnx  tokenizer.json  tokenizer_config.json
#   special_tokens_map.json  config.json

./target/release/inillucent-bench embed-check --cache ~/.cache/inillucent-corpus/corpus.cache
```

`embed-check` asks whether the vectors in the cache were made from the text in the cache. That is a real failure this pipeline can produce: the text is assembled in one step and embedding takes hours in another, so rebuilding the text without rerunning the embedding pairs every vector with the wrong chunk. Nothing would look broken, the index would build and queries would return rows, and every retrieval number would be quietly wrong.

### Running it on a GPU

`--devices` names the processors the corpus is embedded on: `cpu`, `cuda`, `cuda:1`, or several
comma separated. Each one gets its own session, and a window of the corpus is split across them by
total text length — longest first into whichever device is least loaded — so a genuinely slower
card is simply given less of the next window. The vector file is still appended in strict corpus
order, so chunk N is record N whatever ran it, and the run stays resumable.

The CUDA execution provider is registered with `error_on_failure`. `ort` defaults to logging the
failure and falling back to the processor, which is the worst outcome available here: a run meant
to take an hour silently becomes one that takes a day and nothing in the output says why. Set
`INILLUCENT_CUDA_BIN` and `INILLUCENT_CUDNN_BIN` to have the libraries preloaded from a specific install
rather than found on `PATH`.

**Measured on 185,078 chunks: 7 minutes 10 seconds against the README's eight to twelve hour**
**estimate on a laptop processor.** What is worth knowing is where that comes from. On the 18,685
chunk corpus at batch 64:

| configuration | wall clock |
|---|---|
| one session, one card | 38 s |
| two sessions, one card | 17 s |
| two sessions, two cards | 18 s |
| four sessions, two cards | 22-27 s |

Two sessions is 2.1x and **a second card is worth nothing over a second session on the first one**.
A 137M parameter encoder does not saturate a modern GPU; what a second session hides is the
host-side serial work between inference calls, tokenizing and mean-pooling `[batch, seq, 768]`. A
fourth session is worse than two, because that work starts contending with itself.

### Why a batch is bounded by tokens rather than by texts

Attention allocates one score per pair of positions per head, so a batch's memory grows with the
**square** of its longest sequence. `batch_size` alone does not bound it: 64 texts at the 1,900
token limit asks ONNX Runtime for 64 x 12 x 1900² x 4 bytes, which is 11.1 GB in one allocation, and
a corpus run dies 45% of the way through. Texts are tokenized once up front, sorted by true token
count, and grouped against `max_batch_cells`, a ceiling on `texts in the batch x longest, squared`.
A single text that exceeds the budget on its own is still run: refusing it would drop a chunk from
the corpus.

**The budget counts cells and does not know what a cell costs**, and what it costs is the model's
head count. `--max-batch-cells` defaults to 24,000,000, which was calibrated on a 12-head encoder:
ONNX Runtime's fused `MultiHeadAttention` wants roughly `cells x heads x 4 bytes x 2.4`, so the same
budget is about 2.8 GB at 12 heads and **3.7 GB at 16**. `qwen3-embedding-0.6b` has 16 and failed
twice on a 32 GB card at exactly that ceiling — once on one text of 5,327 tokens (28.4M cells,
3.76 GB) and once on 29 texts of 895 tokens (23.2M cells, 3.61 GB), two batch shapes with nothing in
common but their cell count. Lower the budget for a model with more heads than the default assumes;
8,000,000 brings that model's worst batch to 1.1 GB.

It is a flag rather than a manifest field on purpose. It describes the card, not the model, and a
manifest field would move every manifest digest whenever somebody tuned a batch — which would make
every cache on disk unreadable to a comparison for a reason that has nothing to do with any model.

## Comparing embedding models

`grade` holds the embedding constant and compares engines; it says so in its own caveats. That is
the right design for grading an index and exactly the wrong one for grading an embedder, so
`grade-embedding` is the other half: the engine is held constant and the model varies.

A model is described by a **manifest**, `model.json` beside its weights, rather than by constants in
the harness:

```json
{
  "id": "nomic-embed-text-v1.5",
  "dims": 768,
  "mrl_widths": [64, 128, 256, 512, 768],
  "prefixes": { "query": "search_query: ", "document": "search_document: ", ... },
  "pooling": "mean",
  "max_tokens": 1900,
  "model_file": "model.onnx",
  "token_type_ids": true,
  "backend": "onnx",
  "output": "token_embeddings",
  "tokenizer_sha256": "d241a60d…",
  "weights_sha256": "147d5aa8…"
}
```

`inillucent-bench models --dir <d>` seals one, filling in the two file digests. Every property a
model needs in order to be run the way its author intended lives there and nowhere else, which is
what lets eight models share one code path: a BERT export that declares `token_type_ids`, a
ModernBERT export that does not, a decoder export that also wants `position_ids` and an empty
past-key-value cache, and one model that has no ONNX at all and is served by `llama-server`. The
embedder fills in exactly the inputs the graph declares, read off the session rather than off the
manifest, because the graph is the authority on what the graph needs.

`synth-embed` writes the manifest's identity into the cache header:

```
INLCACH4  corpus 110858c33ffd  model nomic-embed-text-v1.5  manifest db59adb9504a
          185078 chunks  768 dims  1900 max tokens  34 truncated  seeds d28c510dda24
```

Then a comparison can refuse rather than warn:

```sh
./target/release/inillucent-bench grade-embedding \
  --cache-set caches/nomic-embed-text-v1.5.cache \
  --cache-set caches/gte-modernbert-base.cache \
  --baseline nomic-embed-text-v1.5 --device cuda:0 --out embedding-scorecard.md
```

It reads every cache's header — and nothing else — before loading a single vector, and stops with a
named error if two caches disagree on the corpus digest, the chunk count or the query seed table; if
a cache's width contradicts its manifest; if a manifest has been edited since the vectors were made;
or if a cache carries no provenance at all. Refusing costs a few hundred bytes of reading. Finding
out half way through costs the run.

### The four lanes

**Dense** is exhaustive cosine over every chunk, with no graph, no lexical side and no fusion: the
embedding on its own. The graph reaches 0.925 recall against exhaustive cosine on this corpus, and
letting 7.5% of the answer move for reasons unrelated to the model would be larger than the effect
being measured.

**Hybrid** is the real pipeline with the shipped ranking settings, applied identically to every arm —
left exactly as `IndexConfig::default()` built them rather than copied into setters, because a second
copy of a default is somewhere the two can drift. A model that wins in isolation and loses once BM25
is fused beside it has not helped an agent.

**Cost** is chunks per second on the processor and on `cuda:0`, weights on disk, tokens per chunk,
and the share of the corpus each arm truncated. Timed on **distinct** stride-sampled chunks: a
repeated-input embedding benchmark on this machine reports roughly three times the real rate, because
shared prefixes collapse in the prompt cache. Truncation is printed beside throughput because a model
that is fast for having read less of each chunk is not fast.

**Matryoshka** is each model's narrowed ranking against its own full-width exact ranking, so the
storage saving is priced per model instead of taken from a model card. A model with no Matryoshka
training appears only at its full width, which is the honest way to show it has none.

Every primary row is judged by the same paired bootstrap and randomization test `grade` uses, against
the same 0.01 practical threshold, and per-query rows go to `runs/<id>/per-query.jsonl` with a
`model / lane` column so a miss can be diffed model against model.

### Two things this found before they became numbers

`snowflake-arctic-embed-m-v2.0` ships `padding: BatchLongest` and `truncation: max_length 512` inside
its own `tokenizer.json`. Its first run reported **exactly 512.0 tokens for every chunk** in a corpus
whose median chunk is about 240: every text padded to the batch maximum with the padding attended to
as though it were text, and every text cut at 512 before the harness could see its real length — so
the arm would have been graded as an 8,192-token model with a truncation share of zero. Every
tokenizer is now disarmed on load, so the manifest's bound is the only bound.

The manifest digest covers every field, and adding one half way through an embedding run moved every
digest and made every cache written before it unreadable to a comparison. That is the guard working,
and the answer is not to soften the digest: `synth-embed` resumes from the vectors already on disk
and re-stamps the header, and `embed-check` then re-embeds a sample and refuses unless the stored
vectors are what the current manifest produces. `the_baseline_manifests_digest_is_pinned_so_a_schema_change_is_visible`
now fails loudly on any schema change and says what to do about it.

### Building on Windows

`ORT_DYLIB_PATH` points at `onnxruntime.dll` from the GPU release rather than at a Homebrew dylib.
Git Bash does not inherit the MSVC `INCLUDE` and `LIB` that `onig_sys` needs, so dump them from
`vcvars64.bat` once and export them into the shell before `cargo build`. `scripts/`
`fetch-public-corpus.sh` resolves the interpreter (`python` where there is no `python3`), falls back
to a hard link where symlinks do not work, and calls `api.github.com` directly when `gh` is not
authenticated — its unauthenticated limit is 60 requests an hour and the corpus needs 48.

If pgvector was installed by running its SQL with absolute paths to `vector.dll`, because the
PostgreSQL install directory is not writable, two things follow. `synth-load` no longer requires
`CREATE EXTENSION vector` to succeed; it checks the type exists instead. And the cluster needs
`dynamic_library_path = '$libdir;C:/path/to/pgvector'`, because pgvector's parallel HNSW build
launches background workers with `bgw_library_name = "vector"`, which is resolved through that path
rather than through the absolute paths in the function definitions.

`ort` uses `load-dynamic` because its system linking strategy wants a static library and Homebrew ships only a dylib. CoreML is not usable for this model: the execution provider registers, fails to compile the rotary embedding operators because `attention_mask` has an unbounded dimension, falls back to CPU, and runs slower than plain CPU.
