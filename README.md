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

The retrieval engine reaches SQL two ways: through the `inillucent_search` virtual table, and — since
task-1838 — through a `VECTOR(N)` column with distance functions and an index the planner uses. One
file can hold ordinary tables and a hybrid index that commits and rolls back with them. Neither
engine links another database: the only SQLite in the tree is a pinned 3.53.4 build run as a
child-process oracle (`docs/dependency-policy.md`, enforced by a test).

**This document was rewritten on 2026-09-07 (task-1843) from measurements taken that day at commit
`d885e91`, and updated the same day by task-1845, which implemented the plan it produced.** Every
performance number here is from task-1845's own four 30-round medium runs; every semantic claim is
from `crates/inillucent-compat/tests/semantics.rs`, which is the review's differential probe turned
into a checked-in test rather than a script that was run once. Where a number is quoted from an
earlier ticket rather than re-measured, the ticket is named and the reason is given.

## Install

```powershell
# Windows
irm https://raw.githubusercontent.com/jasonmcaffee/inillucent/main/packaging/install.ps1 | iex
```

```sh
# macOS and Linux
curl -fsSL https://raw.githubusercontent.com/jasonmcaffee/inillucent/main/packaging/install.sh | sh
```

Or from whichever package manager is already in the project:

| | |
|---|---|
| **npm** | `npm install -g inillucent` — or `npx inillucent help` with nothing installed |
| **pip** | `pip install inillucent` — the wheel carries the binaries *and* an in-process driver |
| **cargo** | `cargo install inillucent-cli` — builds from source; the fallback on any platform without a prebuilt archive |
| **Homebrew** | `brew install jasonmcaffee/inillucent/inillucent` |
| **Go** | `go install github.com/jasonmcaffee/inillucent/packages/go/cmd/inillucent@latest` |
| **Composer** | `composer require jasonmcaffee/inillucent && vendor/bin/inillucent-install` |

Every one of them installs the same four programs, and every downloader verifies
the release's published SHA-256 before unpacking it. `packaging/README.md` is how
a release is cut; `packaging/windows/README.md` and `packaging/macos/README.md`
record what a signed installer would take on each platform and what it costs.

### The four programs

| | |
|---|---|
| `inillucent` | the command line: 28 verbs — `query`, `exec`, `describe`, `import`, `export`, `search`, `explain`, `backup`, `migrate` … |
| `inillucent-shell` | an interactive shell shaped like `sqlite3`, with 63 of its dot commands and 48 of its 48 command-line options |
| `inillucent-mcp` | the same 27 commands served to an AI agent over MCP |
| `inillucent-migrate` | builds an inillucent database from a SQLite file |

```sh
inillucent create app.rdb
inillucent --db app.rdb exec "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)"
inillucent --db app.rdb exec "INSERT INTO notes (body) VALUES (?1)" --params '["hello"]'
inillucent --db app.rdb query "SELECT * FROM notes"
inillucent --db app.rdb describe notes
inillucent help
```

Exit codes are part of the interface: `0` success, `1` failed, `2` a command line
nobody could act on, and **`3` a construct the engine has not built yet** — so a
script can branch on "not yet" without matching on a message. `--output json`
turns any command's result into the same object a binding sees, with typed
values, an exact `total`, and the driver's own status name on a failure.

### For an agent

```json
{
  "mcpServers": {
    "inillucent": {
      "command": "inillucent-mcp",
      "args": ["--db", "app.rdb"]
    }
  }
}
```

27 tools, generated from the same command table the CLI reads, so the two cannot
drift — `crates/inillucent-compat/tests/command_parity.rs` fails the build if they
do. `--readonly` refuses every statement that changes something, classified by the
binder rather than by scanning the text; `--root DIR` refuses every path outside a
directory. Designed in
[`tasks/task-1836-cli-mcp-and-installers-tdd.md`](tasks/task-1836-cli-mcp-and-installers-tdd.md).

## Where it stands against the goal

> **[`feature-comparison.md`](feature-comparison.md) is the side-by-side scorecard**: every SQLite
> feature against inillucent, in tables, plus the retrieval engine against PostgreSQL + pgvector.
> It was written by task-1858 from a 416-case differential probe - both shells, one fresh database
> each, every byte compared - and re-measured by task-1859 and task-1860. It is the document to read
> before asking whether something works. **409 of the 416 agree**, and **nothing is refused**: of the
> seven that answer differently, three are a page size and a locking mode this engine chose and can
> price the alternative to, one is the two pinned SQLite artifacts disagreeing with each other, and
> three print numbers about SQLite's own C structures - a VDBE program, `sizeof(sqlite3_file)`, a
> lookaside allocator's counters. Each is named there with what it measures.


The goal is a highly performant SQLite replacement offering the same features, plus embedding search
similar to pgvector. Measured against that, today:

| goal | state | evidence |
|---|---|---|
| Faster than SQLite | **Yes on Windows at 100k rows and up.** Weighted geomean 3.68x-3.85x at medium over four 30-round runs, lower bounds 3.45x-3.72x against a 3.00x bar, 30 of 30 workloads digest-equal on every run. It was 3.20x-3.28x before the engine's binaries were given a size-classed free list as their global allocator. On Linux the same binary was 1.53x at medium (task-1838 §5, not re-measured since). | task-1869's four-run gate |
| **Smaller than SQLite** | **No - 1.15x its peak resident set, against a 0.95x bar the contract now carries.** It was 2.02x before task-1869 and 1.43x before task-1870. task-1869 measured that the remainder was the **file** rather than another buffer, and task-1870 acted on it: an integer mini-column is now as wide as its own values - 1, 2, 4 or 8 bytes, chosen per leaf and written in a `slot_width` field the format always had - so the `.rdb` went from **1.41x** the `.db` to **1.036x**, the page cache fell 22.59 → 16.56 MiB with the process heap unmoved, and every read family got *faster*. Of the 7.3 MiB still left, most is the **operating system's** - a trivial 110 KB Rust binary from this workspace already costs 4.1 MiB, and `sqlite-bench` costs 4.2 - and the rest is one `CREATE INDEX`. The price is `transaction`, which fell to **0.87x** and is under its floor | [Where the memory goes](feature-comparison.md#where-the-memory-goes) |
| **Cheaper in processor time** | **Yes, 0.29x-0.35x of SQLite's**, one round of the whole plan, one child process each, against a 0.40x bar | task-1869's four-run gate |
| No family slower than SQLite | **Nine of ten hold; `transaction` is the one that does not, and `txn.large` is why.** At 0.21x it is the slowest workload on the board, so the family's three-workload bootstrap has a wide interval: its lower bound reads 1.04 on the first run of a sequence and 0.63 on the last. **The reference's own arm says how much of that is the disk** - `txn.batched` is 200 commits and 200 `fsync`s, and it goes from 288 ms to 836 ms on SQLite's arm part-way through a four-run sequence, on the same fixture with the same binary. `extension`, which had been cleared by luck at 1.01-1.06, now reads 1.06-1.22. The families under their *bars* - a different question from the floor - are `open.prepare`, `schema`, `extension` and `transaction`. | task-1869's four-run gate |
| Same features as SQLite | **No wrong answer known, no refusal known, and the one *silent* difference is closed.** 48 of 50 inventoried constructs run. The differential probe is a checked-in test of 183 cases (`semantics.rs`), **all of which agree byte for byte**. task-1861's audit found the surface complete but the **register** under-reporting - 161 function names against 218, while the functionality behind most of the difference answered identically - which is a caller being told less than the truth with no error. task-1869 closed it: 212 names, no function the pinned library answers that this engine does not, and `registers.rs` compares all four enumerations on every build so the class of gap fails a build rather than waiting for an audit. | [What it gets wrong](#what-it-gets-wrong-and-what-it-refuses) |
| Same durability and isolation | **Yes, single process, one writer.** A checkpoint retires the segments below it since task-1845, so the same 200,000 rows are 15.9 MB rather than 110.5 MB - 1.83x SQLite's 8.7 MB, where it was 12.7x. WAL with group commit, snapshot isolation, ARIES-style redo recovery, undo for `ROLLBACK`/`SAVEPOINT`, crash campaigns under a deterministic simulator. SQLite's file format is not written; multi-process access is, over the same lock protocol, under `PRAGMA locking_mode = NORMAL` (task-1860). | [Disk](#disk) |
| Embedding search like pgvector | **Yes, and graded better than pgvector on 15 of 17 primary comparisons** with zero worse; in production on a 598,560-chunk mailbox at recall 1.000 and 27 ms p95. Reachable from ordinary SQL: a `VECTOR(N)` column, `vector_distance_cos`/`_l2`/`vector_dot`, `CREATE INDEX ... USING inillucent_hnsw`, and `ORDER BY vector_distance_cos(v, ?) LIMIT k` planned onto the index at **0.98x** the cost of querying the store directly. | `inillucent-scorecard.md`, task-1775, this ticket's `vectorprobe.txt` |
| One engine, one repository that builds | **Half.** The repository builds from a clone again - `drivers/` is committed, and `harness.rs` now asserts every path in `[workspace] members` exists, so the next crate added before it is committed fails on the machine that added it. `inillucent::Database` reaches the new engine; the old engine is still in the tree awaiting task-1837's driver. | [Repository layout](#repository-layout) |

The improvement plan for every row that is not green is `tasks/rust-db-phase-2-tdd.md`, whose Phase 3
section was written from this review.

## Features

### The relational engine

**SQL that runs today** (`crates/inillucent-compat/tests/new_engine_surface.rs`, 47 of 50):
`SELECT` with inner, cross and **outer** joins (hash join, index nested loop, and a scan), `GROUP BY`,
`HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT`/`OFFSET`, compound selects (`UNION`, `UNION ALL`, `EXCEPT`,
`INTERSECT`), CTEs including **recursive** ones, **derived tables in `FROM`**, subqueries in `WHERE`,
`IN`, `EXISTS` and as values including **correlated** ones, window functions with all three frame
units, `LIKE`/`GLOB`, `CAST`, `COLLATE` (`BINARY`, `NOCASE`, `RTRIM`), 128 built-in function names
(including 28 JSON, 29 math and 7 date-time), `INSERT`/`UPDATE`/`DELETE` with `RETURNING` and
`ON CONFLICT DO UPDATE`, `CREATE TABLE` (including `WITHOUT ROWID`, `STRICT` accepted),
`CREATE INDEX` through a bottom-up bulk builder, `CREATE VIEW`, `CREATE VIRTUAL TABLE`,
**`CREATE TRIGGER`** (`BEFORE`/`AFTER`/`INSTEAD OF`, `FOR EACH ROW`, `WHEN`, `RAISE`), **foreign key
enforcement** (immediate and deferred, all five referential actions, `PRAGMA foreign_key_check`),
`DROP`, all four `ALTER TABLE` forms, `ANALYZE` writing `sqlite_stat1`, `REINDEX`,
`EXPLAIN QUERY PLAN` in SQLite's idiom, `BEGIN`/`COMMIT`/`ROLLBACK`, `SAVEPOINT`/`RELEASE`/
`ROLLBACK TO`, **`ATTACH`/`DETACH` and temporary objects** with a super-journal deciding a commit that
spans two files, `sqlite_schema` and `sqlite_master`, user-defined scalar and aggregate functions and
collations, and the pragmas `table_info`, `table_xinfo`, `table_list`, `index_list`, `index_xinfo`,
`database_list`, `page_size`, `page_count`, `freelist_count`, `cache_size`, `synchronous`,
`busy_timeout`, `integrity_check`, `quick_check`, `wal_checkpoint`, `foreign_keys`.

Everything in bold above shipped after the previous README was written, in task-1838 and task-1844.

**Extensions**: JSON over a binary form, FTS5 with `bm25()`, the R-Tree, and `inillucent_search`.
Their shadow tables are ordinary trees, so they commit and roll back with the transaction. `json_each`
and `generate_series` are registered as modules but are **not usable from SQL** — see below.

**Storage and durability**: 32 KiB pages (8 to 64 allowed), a buffer pool of 4,096 frames (128 MiB) by
default with a cooling FIFO and pointer swizzling, frames allocated on first claim rather than at
open, double-written meta pages, a segmented redo WAL (`RDBWAL01`) with crc32c on every record,
`synchronous` `OFF`/`NORMAL`/`FULL`, group commit, fuzzy checkpoints, recovery on every open, snapshot
isolation with a version log and garbage collection, one writer at a time with `busy_timeout`, an undo
buffer for rollback of rows and schema, and blob extents for values wider than a leaf.

**Tooling**: `inillucent`, the verb-shaped command line and its 28 commands; `inillucent-mcp`, which
serves 27 of them to an agent over MCP; `inillucent-shell`, a `sqlite3`-shaped shell; `inillucent-migrate`, which imports a SQLite
file or a legacy retrieval index into an `.rdb` by copy, verify by count and digest, and publish by
rename; and `inillucent-fullgate`, `inillucent-readgate`, `inillucent-searchgate`,
`inillucent-shellrss`, `inillucent-childcost`, `inillucent-vectorprobe` and `inillucent-probeprofile`,
the paired benchmark instruments.

**Assurance**: **2,122 tests across 184 binaries, of which 2,104 pass and 18 fail** (the 18 are
listed under [What is not there yet](#what-is-not-there-yet) and every one is accounted for); a differential harness that runs the same
SQL through the pinned SQLite 3.53.4 and compares transcripts; a SQLLogicTest subset; a `BTreeMap`
model reference driven by operation traces; a deterministic fault-injecting VFS (`inillucent-sim`)
under the pool, the log and the transaction engine; eight libFuzzer targets over the codecs; 100%
branch coverage held on the pool's interior, latch, meta, extent, free map and swip modules and the
tree's key codec; 23 of 29 crates deny `unwrap`, `expect`, `panic` and slice indexing, and 22 of 29
forbid `unsafe`.

### What it gets wrong, and what it refuses

A refusal is visible and an application can work around it. A wrong answer is not. This section is
ordered by that difference. It is no longer a report: the 183 cases behind it are
`crates/inillucent-compat/tests/semantics.rs`, which runs each script through `inillucent-shell` and
the pinned `sqlite3` and compares every byte of both streams. **All 183 agree.**

**Wrong answers: none known** - which is not the same as none, and each of the last three tickets to
say it is the reason to keep saying it that way. The nine the second review found were closed by
task-1845; task-1849 then found six more that this section had called clean; task-1856 then found
nine more that *those* probes were not shaped to see either.

What separates the three sets is *where* they lived, and it is worth writing down because it is how
the next set will be found. The review's nine were each reachable by writing one row and reading it
back. task-1849's six each needed a second row to collide with, which a probe made of single-row
scripts cannot see. task-1856's nine each needed something the probes had no reason to write down at
all: a **declaration** the scripts never used (`UNIQUE ON CONFLICT IGNORE`, a table-level
`CHECK ... ON CONFLICT`, `NOT NULL ... DEFAULT` under `OR REPLACE`), a **question about the
connection** rather than about a row (`changes()`, `total_changes()`, `last_insert_rowid()`,
`random()`), or a **shape with too few rows to disagree** - `WHERE c >= 10` over a descending index
answers the right count with three rows and the wrong one with nine.

| construct | was | now |
|---|---|---|
| `CREATE INDEX ic ON t(c DESC)`, then `WHERE c >= 10` | one row of nine, silently | nine |
| `a TEXT UNIQUE ON CONFLICT IGNORE`, a colliding insert | `UNIQUE constraint failed` | skips the row |
| the same with `ON CONFLICT REPLACE` | `UNIQUE constraint failed` | replaces the row |
| `id INTEGER PRIMARY KEY ON CONFLICT REPLACE` | read the `NOT NULL`'s clause, so raised | replaces |
| `CONSTRAINT c CHECK(b < 9) ON CONFLICT FAIL` | a parse error, and every later statement `no such table` | accepted and ignored, as SQLite does |
| `UPDATE OR REPLACE t SET c = NULL` on `c NOT NULL DEFAULT 'd'` | `NOT NULL constraint failed` | stores `'d'` |
| `INSERT OR REPLACE` colliding on two unique indexes | deleted the first row in its way and left the second | deletes both |
| `INSERT ... ON CONFLICT DO NOTHING` violating a `CHECK` | skipped the row and reported success | raises |
| `changes()`, `total_changes()`, `last_insert_rowid()` | `0`, always, for every statement | what the C API answers |
| `random()` | one constant, for the life of the process | a different number per call |

| construct | was | now |
|---|---|---|
| `UPDATE t SET a='x'` onto another row's value, `UNIQUE(a)` | performed it; the index then held two entries under one key | `UNIQUE constraint failed: t.a` |
| the same under `OR IGNORE` / `OR REPLACE` | performed it; neither arm was reached | skips the row / deletes the row in the way |
| `INSERT ... ON CONFLICT DO UPDATE` whose arm takes a second index's key | performed it | refuses, as SQLite's `DO UPDATE` arm resolves ABORT |
| `UPDATE t SET id=5` with an untouched `UNIQUE(a)` | **refused** a legal statement - the probe found the row's own entry | performs it |
| a `WITHOUT ROWID` key collision | `UNIQUE constraint failed: t.rowid`, naming a column such a table has no | names the primary key's columns |
| a row colliding on two unique indexes at once | named the first declared, on `INSERT` as well as `UPDATE` | names the last declared, as SQLite does |
| `ON CONFLICT DO UPDATE SET a=9` moving an `INTEGER PRIMARY KEY` | wrote the new row and left the old one, so the table held both | the row moves |

Four of the six are one defect and its consequences: `UPDATE` checked uniqueness only when the *table's own*
key moved, so no secondary `UNIQUE` index constrained it and the check it did run had no way to
recognise the row it was updating. Both directions of that are in the table above, and the second is
worth as much as the first: a check added without a notion of "this row" trades a silent accept for a
false refusal, which is why the fix is not the one-line one it looks like.

**The nine the second review found**, all closed by task-1845. Each case declares `Agrees` or
`Differs`, so a construct that changes its mind in either direction fails until its row is moved.

| construct | was | now |
|---|---|---|
| `b INTEGER CHECK (b > 0)`, then `INSERT ... VALUES (1, -5)` | stored −5 | `CHECK constraint failed: b > 0` |
| the same constraint under `UPDATE` | stored −1 | refuses, row unchanged |
| `CREATE TABLE s(x INTEGER) STRICT`, then `INSERT ... VALUES ('abc')` | stored the text | `cannot store TEXT value in INTEGER column s.x` |
| `a INTEGER`, `INSERT ... VALUES ('42')` | `typeof(a)` was `text` | `integer` |
| `a TEXT`, `INSERT ... VALUES (42)` | `typeof(a)` was `integer` | `text` |
| `a REAL`, `INSERT ... VALUES (1)` | `typeof(a)` was `integer`, printed `1` | `real`, prints `1.0` |
| `INTEGER PRIMARY KEY AUTOINCREMENT`, insert, delete, insert | reused key 1 | allocates 2 |
| `json_valid('{}')` | 0 | 1 |
| `strftime('%Y-%W', '2024-03-01')` | `2024-08` | `2024-09` |
| `printf('%05.2f', 3.14159)` | `3.14` | `03.14` |

**What they turned out to be**, because three of them were not what they looked like:

- **`CHECK`, `STRICT` and affinity are one change, not three**, and the order between them is
  load-bearing. SQLite applies affinity first, then `NOT NULL` over every column, then the `STRICT`
  type check, then `CHECK`, then the unique indexes - probed against 3.53.4 rather than assumed. So
  `CREATE TABLE s(a INT, b TEXT NOT NULL) STRICT; INSERT INTO s(a) VALUES ('x')` reports the missing
  `b`, and `INSERT INTO s(b) VALUES (1)` *succeeds*, because affinity has already made it text by the
  time `STRICT` looks. A `STRICT` implementation written before affinity would have been wrong in both
  directions. `WriteDeclarations` (`crates/inillucent-exec/src/declared.rs`) holds all three, compiled
  once per statement.
- **The rowid alias needed affinity too.** `INSERT INTO t(id) VALUES ('42')` on an
  `INTEGER PRIMARY KEY` is row 42 in SQLite. The key path's rule is not *no affinity*, it is
  *affinity, and then a refusal rather than a keep for what does not convert*.
- **`json_valid` was collateral damage from an optimisation.** The executor swaps a text document for
  its parsed JSONB before the call, which is invisible for every function whose question is about the
  *document* and wrong for the one whose question is about the **text**: its flags ask whether the
  argument was RFC-8259, JSON5 or JSONB. It was wrong in both directions - `json_valid('{}')` was 0
  against 1, and `json_valid('{}', 4)` was 1 against 0.
- **`printf`'s zero-padding rule was C's, not SQLite's.** C ignores the `0` flag for `d i o u x X`
  when a precision is given; SQLite's own printf does not implement that, and renders `%08.3d` of 42
  as `00000042`. The guard was wrong for the integer conversions it was written for as well as for
  `%05.2f`.
- **`strftime('%W')`** counted from the year's first *Sunday* and was off by one besides. SQLite's own
  arithmetic is transcribed now, and `%U`, `%V` and `%G` are implemented rather than echoed back as
  literal text - fixing one member of a family and assuming the rest is how the next three stay
  hidden.

**The hang is fixed, and it was two halves of one root.** `SELECT value FROM generate_series(1)
LIMIT 3` used to run past a 25-second timeout - one run held about 1.2 cores and a growing working set
for ten minutes. It answers in 55 ms.

- `TreeCatalog::virtual_rows` returned `Option<Vec<Vec<OwnedDatum>>>`, so a virtual-table scan was
  materialised in full before any operator above it ran, and a `LIMIT` cannot stop a scan that has
  already finished. It is `virtual_cursor` now: the module's rows go down the chain a batch at a time
  and the cursor is abandoned on `Flow::Stop`, the way `scan.rs` abandons a b-tree scan.
- There was no `stop` constraint to bound it because **the eponymous table-valued form did not
  exist**. It does: `FROM generate_series(1, 10)`, `FROM json_each(...)` and
  `FROM pragma_table_info('t')` all bind. The binder needed nothing - `bind_table_arguments` already
  turned the arguments into `Eq` constraints on the module's hidden columns, which is what
  `best_index` consumes - only the catalog had to carry the modules. `json_each`, which refuses
  `CREATE VIRTUAL TABLE` outright, was unreachable from SQL by any route and now is not.

The tests for it carry a **deadline**, because the failure mode is a hang and an assertion on rows is
never reached by a statement that does not return: `new_engine_vtab_stream.rs` runs each case on its
own thread and fails on the deadline.

**Refusals, by name.** These say what they are:

| construct | state |
|---|---|
| `VACUUM`, `VACUUM INTO` | both rebuild the file and reclaim its free space (task-1860); `VACUUM INTO` writes a verified copy and refuses to overwrite |
| plain `EXPLAIN` | answered: it lists the operator chain in the eight columns SQLite lists opcodes in (task-1860) |
| a second process on the same file | supported over SQLite's own lock protocol under `PRAGMA locking_mode = NORMAL` (task-1860); `exclusive` is the default |
| a second writer, SQLite's file format, the C ABI on the new engine | not built: see task-1816's design |
| `.expert` and `.session` | the two dot commands of SQLite's 65 this shell has not got. `.load` and `.progress` were the other two and both answer since task-1869 - `.load` in the reference's own words for a library it cannot open, `.progress` by keeping the same state |
| `fts5(...)`, `fts5_locale()`, `fts5_get_locale()`, `fts5_insttoken()`, `fts3_tokenizer()` | five function names that hand out C pointers or belong to FTS5's locale machinery. A stub would be a **wrong** answer rather than a missing one. `fts5_source_id()` and `optimize()` were the other two the audit found and both answer since task-1869 |
| modules `fts4aux` and `fts3tokenize` | absent from the pinned *library* too, so a difference against the shell. The FTS5 analogue `fts5vocab` **is** here |

**Closed by task-1846**, each byte-compared against `sqlite3`: **partial indexes**, **indexes on
expressions**, and **`CREATE INDEX` on a `WITHOUT ROWID` table** - the last three refusals
`semantics.rs` carried. A partial index is maintained on both images of an `UPDATE`, so a row that
moves across the predicate joins the index or leaves it, and the planner uses one only when the
predicate appears unchanged as a conjunct of the query's `WHERE`, which is SQLite's own rule. An index
on a `WITHOUT ROWID` table carries that table's primary key where a rowid would go, named by
`SourceLayout::identity` so the non-covering lookup probes the table with it.

**Closed by task-1845**, each byte-compared against `sqlite3` including its refusals:
`CREATE TABLE ... AS SELECT` (the stored text is synthesised from the query's result columns, down to
`CREATE TABLE w("1")` for `SELECT 1`), `UPDATE ... FROM`, `WITH` on `INSERT`/`UPDATE`/`DELETE`, row
values in every comparison and their `IN` form, writing through an `INSTEAD OF` trigger, the
table-valued pragmas and eponymous virtual tables, `CREATE TEMP TABLE` through the shell, and undoing
a `DROP TABLE` inside a transaction.

### The retrieval engine

Storage with dictionary-encoded filter columns; cosine over L2-normalised vectors; exhaustive search
chosen by a cost model when the filter is narrow; an HNSW graph (m 16, ef_construction 64) whose
traversal honours a predicate and whose build runs on every core; int8 scalar quantisation with
full-precision rescoring; an inverted index with BM25, Snowball stemming, identifiers kept whole, and
coverage, proximity, phrase, tier and prefix weights each with an off switch; reciprocal rank fusion
and two score-based fusions; a `confidence` on absolute bounds beside every `score` so the engine can
say "nothing here answers that"; generation-directory persistence with the BM25 postings written
rather than rebuilt; the `nomic-embed-text-v1.5` embedder in process through ONNX Runtime, on CPU or
one or more GPUs; a model manifest and cache header so two embedding models can be graded on identical
corpora; and a grading harness that drives inillucent and PostgreSQL through one interface. The
engine's own documentation follows the comparisons below.

## How it compares with SQLite 3.53.4

**The current comparison lives in [`feature-comparison.md`](feature-comparison.md)**, re-measured on
2026-09-08 for task-1869 over four consecutive 30-round runs, and it carries what this section does
not: **processor time and peak resident memory for both engines**, every figure written as N% less
time / less CPU / more memory, and a **per-workload** attribution of where the memory goes with the
buffer pool separated from everything else the process holds. Its medium headline is **3.85x - 74%
less time**, at **67% less CPU** and **43% more memory**, where the memory was 102% more before this
ticket. What follows is the task-1843 run, kept because it is the only one that covers all three
scales.

Every number in this section was measured on 2026-09-07 at commit `d885e91` on this machine
(Windows 11, x64), and the raw gate output is under
`_agent_output/task-1843-inillucent-review-2/gate-20260906T231706/`. The instrument is
`inillucent-fullgate`: the same SQL, the same data, the same `synchronous = FULL`, the same
transaction boundaries, a matched cache budget on both arms, interleaved A/B, 30 paired rounds, and
every workload's answer digested and compared with SQLite's before a timing is allowed to count. The
ratio is SQLite time over inillucent time, so above 1.00x means inillucent is faster. The bar is the
**lower 95% bound**, not the centre. Neither arm uses a plan cache: `open.prepare` compiles inside the
clock on both sides, and every other workload prepares once and rebinds on both sides.

The fixtures are rebuilt by `_agent_output/task-1819-readgate/reproduce/build-fixtures.sh` and **each
run needs its own copy**: the gate's `schema.index` workload leaves `main_label` behind on the SQLite
arm, so a second run against the same file dies on `index main_label already exists`.

### Speed: the headline

The contract (`compat/perf/contract.toml`) asks for a weighted geometric mean of at least **3.00x**
at medium scale (100,000 rows) and **no family below 1.00x**.

| scale | rows | weighted | lower bound | bar | verdict |
|---|---|---|---|---|---|
| small | 5,000 | 2.56x | 2.51x | 3.00x | missed |
| **medium** | 100,000 | **3.20x, 3.26x, 3.28x, 3.23x** | **3.11x, 3.20x, 3.20x, 3.14x** | 3.00x | **met on all four runs** |
| large | 600,000 | **4.06x** | **3.85x** | 3.00x | met |

All thirty workloads were digest-equal with SQLite on every round of every run.

Two of the three scales moved up since the previous README, which reported 2.30x at small and 3.83x at
large: task-1838's leaf-splitting fix and task-1835's FTS5 query path both land in the write and
extension families. Medium is where it was, which is the interesting part — the headline has been on
the bar for three tickets running and the work in between has been spent elsewhere.

**Linux is not re-measured here**, and the reason is that task-1838 §5 settled it by experiment rather
than argument, so a fifth run of the same measurement would add nothing. The finding, kept because it
is the plan for the floor: with a size-classed free list in place of the system allocator, Windows goes
46.95 ms → 38.97 ms (17%) and Linux 39.91 ms → 38.20 ms (4%), and **the two platforms then run the same
speed** (38.97 against 38.20). On a `SELECT 1` compile the Windows CRT heap is **59%** of the time. So
the 1.53x on Linux is not a Linux problem: SQLite does per-statement operating-system work that Windows
charges heavily for and Linux barely does, and this engine does none of it, so SQLite's denominator
moves across platforms and ours does not. The absolute work is the same on both, and it is what the
floor needs.

### Speed: per family, medium, four 30-round runs

| family | weight | what it measures | centre | lower bound | bar | verdict |
|---|---|---|---|---|---|---|
| `read.point` | 0.16 | one row by rowid, integer key, secondary index | 21.51x–22.26x | 19.84x–21.15x | 2.00x | **met** |
| `read.range` | 0.12 | selective ranges, forward and reverse, covering and not | 3.80x–3.89x | 3.05x–3.26x | 3.00x | **met** |
| `read.join` | 0.08 | two and four table joins | 3.90x–3.96x | 2.77x–2.85x | 3.00x | bar missed |
| `read.analytical` | 0.10 | scans, aggregates, `GROUP BY`, `DISTINCT`, sorts | 5.66x–5.75x | 4.83x–4.97x | 5.00x | bar missed |
| `write` | 0.20 | insert, update, delete, upsert, with and without indexes | 1.82x–1.95x | 1.58x–1.77x | 1.50x | **met** |
| `transaction` | 0.10 | autocommit, small batches, large batches, savepoints | 1.32x–1.41x | 0.90x–1.02x | 1.00x | **under the floor on two runs** |
| `large.values` | 0.04 | text and blobs across the inline/overflow boundary | 10.43x–10.93x | 6.99x–8.43x | 1.50x | **met** |
| `open.prepare` | 0.08 | parse, bind, step one row, reset | 1.01x–1.04x | 0.75x–0.79x | 5.00x | **under the floor** |
| `schema` | 0.04 | `CREATE INDEX` and its backfill | 0.54x–0.56x | 0.52x–0.56x | 3.00x | **under the floor** |
| `extension` | 0.08 | JSON, FTS5, R-Tree | 0.87x–0.89x | 0.75x–0.78x | 1.50x | **under the floor** |

`read.join` and `read.analytical` clear the floor comfortably and miss only their own ambitious bars;
the four families in bold are the release blocker.

At the other scales the same families behave differently, which is worth stating because a single
scale can flatter or damn a family: `extension` is 0.87x at medium but **1.27x at large**; `write` is
0.49x at small, 1.82x–1.95x at medium and **5.71x at large**; `schema` is 1.08x at small and 0.53x
everywhere above it.

The read gate — which imports the fixture once rather than once per round, and is the number to
compare across phases — **passes outright at medium**: `read.point` 23.08x, `read.range` 4.35x (low
3.54x), `read.join` 4.48x (low 3.06x), `read.analytical` 5.88x (low 5.10x), and a warm rowid
`PointProbe` of **350.4 ns** against a 500 ns bar, with 12 of 12 statements reusing their prepared
operator chain.

### Speed: the workloads that hold the slow families down

Medians of the four medium runs, in nanoseconds per round.

| workload | inillucent | SQLite | ratio | cause |
|---|---|---|---|---|
| `prepare.trivial` (`SELECT 1`, compiled per call) | 5,251,350 | 1,679,700 | 0.31x–0.32x | the binder's fixed cost. About 1.3 µs per compile against SQLite's 0.45; 25 heap allocations, and on Windows the CRT heap is 59% of it (task-1838 §5) |
| `txn.large` (2,000 `UPDATE`s in one transaction) | 4,018,850 | 809,000 | 0.19x–0.20x | a per-update constant on the allocating read path plus delete-and-insert into the delta area; not compaction |
| `write.insert.batch` | 36,087,550 | 17,759,300 | 0.48x–0.50x | a per-write constant; the delta area is scanned linearly and every entry's key decoded per lookup |
| `schema.index` | 55,312,500 | 29,827,700 | 0.54x–0.56x | the bulk build's floor sits above the bar (19.7 ms against a 10.1 ms budget) |
| `extension.fts.build` | 7,872,150–8,143,100 | 2,974,250–3,055,500 | 0.35x–0.37x | inside the module, and profiled in task-1856 rather than guessed at — the row is that ticket's four runs, not this table's. **Up from 0.11x**, then 0.25x, then 0.30x |
| `extension.json` | 1,744,700 | 1,134,500 | 0.62x–0.68x | ~1.1 µs per `json_extract` call against 0.4; the constant argument is re-parsed per call |
| `extension.rtree.insert` | 2,751,650 | 2,630,900 | 0.89x–0.95x | **was 0.12x**; the shadow-table batching in task-1838 all but closed it |
| `range.lookaside` | 28,974,400 | 26,784,600 | 0.93x–0.95x | the one read workload still under 1.00x, with `join.range` at 0.96x |

### Speed: absolute time, medium, nanoseconds per round

| workload | inillucent | SQLite |
|---|---|---|
| `point.rowid` (4,000 lookups) | 2,216,300 | 48,724,950 |
| `point.miss` | 1,260,850 | 46,497,550 |
| `point.index` | 3,901,900 | 51,050,550 |
| `range.covering` | 3,003,900 | 15,659,650 |
| `join.selective` | 1,571,450 | 25,218,700 |
| `large.read` | 747,000 | 23,255,650 |
| `scan.aggregate` | 7,540,150 | 92,909,000 |
| `write.insert.autocommit` (one fsync per row) | 22,929,150 | 133,403,300 |

### Memory

| | inillucent (new engine) | SQLite 3.53.4 |
|---|---|---|
| page size | 32 KiB default, 8 to 64 KiB | 4 KiB default |
| cache | a buffer pool of frames times page size; 4,096 frames = 128 MiB by default, set at open. The budget is a **ceiling**: a frame's page is allocated the first time that frame is claimed | `cache_size`, 2 MiB by default, also grown into |
| what the gates matched | 128 MiB on both arms | same |
| a transaction larger than the pool | must fit: the pool is no-steal, so dirty pages cannot be evicted before commit; documented limit (task-1816) | spills to the journal |
| a `SELECT` result | materialised on the first `step`; `Statement::step` walks rows already produced | streamed one row per `step` |
| a virtual-table scan | **materialised in full** before any operator above it runs | streamed through `xNext` |

**Measured, both arms as whole child processes, one round of the same plan against a database the
parent built.** Neither figure is a delta.

| | peak working set | user CPU | kernel CPU |
|---|---|---|---|
| medium — `sqlite-bench` | 37.20 MiB | 453 ms | 508 ms |
| medium — inillucent | **79.83 MiB** | **359 ms** | **94 ms** |
| large — `sqlite-bench` | 179.68 MiB | 422 ms | 508 ms |
| large — inillucent | **418.88 MiB** | **531 ms** | **203 ms** |

**2.15x SQLite's peak at medium and 2.33x at large, at a matched cache budget.** Earlier in this
sprint the same measurement read 4.75x, and the difference was a defect it found: the buffer pool
allocated and zeroed every frame at open, so a 128 MiB budget was 128 MiB resident from the first
statement. Frames now allocate on first claim. What is left is the pages the fixture actually touches
plus the gate binary, which is a much larger program than `sqlite-bench` and is counted here.

`inillucent-shellrss` asks the same question of two shells, each building its own copy of the same
200,000-row table and then reopening it and reading:

| shell | peak working set | user CPU | kernel CPU |
|---|---|---|---|
| `sqlite3` 3.53.4 | 6.89 MiB | 0 ms | 16 ms |
| `inillucent-shell` | **31.90 MiB** | 63 ms | 63 ms |

**4.63x, down from 9.0x.** That figure was 65.05 MiB when the previous README was written, and the
whole of the improvement is task-1838's leaf-splitting fix: an append no longer splits a leaf that is
not full, so a page holds what it can rather than 32 rows.

### CPU

Both engines run a statement on one thread, so every ratio above is also a ratio of CPU time on the
CPU-bound families. `read.point`, `read.range`, `read.analytical`, `read.join` and `large.values` are
CPU-bound; `write.insert.autocommit`, `txn.autocommit` and `large.write` are bounded by one `fsync`
per commit under `synchronous = FULL`, and both engines pay it. The new engine is single-threaded by
construction: its pool and trees are `RefCell`, a `Connection` borrows the `Database`, and there is no
parallel scan (task-1816 lists parallel scans as after-scope). The retrieval engine's graph build is
the one thing that uses every core.

The kernel time is the interesting half of the table above: at medium SQLite pays **5.4x** what this
engine pays, which is where a WAL that writes whole 32 KiB pages once per commit differs from a
rollback journal plus a WAL under `synchronous = FULL`. At large, inillucent's **user** time overtakes
SQLite's (531 ms against 422) while the wall clock is four times better — the work is being done in
user space instead of in syscalls, which is the whole design.

Per-workload CPU is reported by the gate too, but **it is quantised to the Windows scheduler tick
(15.625 ms)** and the gate says so above the table: only the per-round totals should be quoted.

### Disk

The same 200,000 rows, built through `INSERT ... SELECT` the way an application builds them, then
`PRAGMA wal_checkpoint`, closed, reopened and checkpointed again:

| | inillucent, before | inillucent, now | SQLite 3.53.4 |
|---|---|---|---|
| the data file | 15.9 MB | 15.9 MB | 8.7 MB |
| its log | **94.6 MB across two segments** | **0.0 MB** | 0 |
| total on disk | **110.5 MB** | **15.9 MB** | 8.7 MB |

**1.83x SQLite, where it was 12.7x.** Both columns are the same script against the same fixture, the
"before" one run against a release build of the tree as it stood before task-1845.

**Calling `retire_segments_below` was necessary and not sufficient**, and the gap is worth recording
because it reads as done. `Wal::retire_segments_below` existed, was documented as "called after a
checkpoint", was covered by six cases in `inillucent-wal/tests/recovery.rs`, and was called from
exactly one place - `inillucent-txn`'s own engine, which is not the engine that ships. Wiring it into
`Database::checkpoint` reclaimed 67.1 MB and left 27.5 MB behind, for ever, through a close and a
reopen and a second checkpoint: the function deletes a segment only when *every* record in it is below
the checkpoint LSN, and the segment being appended to never is, because `note_checkpoint` writes the
checkpoint record into it. `Wal::roll_segment` moves the boundary to the checkpoint point first, so
everything behind it becomes redundant and goes.

Nothing is deleted before the data file holds the pages, a segment holding anything recovery still
needs is left alone, and `new_engine_log_retire.rs` proves both - including that a crash either side
of a retirement recovers the same database. The reclaim case fails on the tree before this change
(`the checkpoint reclaimed nothing: 6779664 -> 6779976`).

For comparison, the Phase 3 gate fixtures — which are **imported** rather than built through SQL — are
16.8 MB as a SQLite file against 23.7 MB as an `.rdb` at medium, and 93.7 MB against 131.2 MB at
large: about 1.4x, at 32 KiB page granularity. The two paths into a file are not in conflict; they are
two paths.

### Features and semantics

| | inillucent (new engine) | SQLite 3.53.4 |
|---|---|---|
| SQL dialect | SQLite's; 60 of 60 grammar productions parse (`compat/syntax-report.md`) | reference |
| inventoried constructs | 48 of 50 run (`new_engine_surface.rs`); the two that do not are plain `EXPLAIN` and the table-valued pragma form | reference |
| differential probe, 416 scripts | **409 agree, 7 differ, 0 refused** (`feature-comparison.md`) | reference |
| type affinity | applied on write; all 19 of the probe's `types` cases agree | applied on write |
| `CHECK`, `STRICT` | enforced; all 16 of the probe's `constraint` cases agree | enforced |
| `NOT NULL`, `UNIQUE`, `PRIMARY KEY`, foreign keys | enforced, with SQLite's codes and messages | enforced |
| file format | its own (`.rdb` + `RDBWAL01` segments); SQLite files are imported, not opened | SQLite |
| journal modes | all six, and `delete` is the default as it is there; `locking_mode` answers `exclusive` by default and `normal` is a real switch | DELETE, TRUNCATE, PERSIST, MEMORY, WAL, OFF |
| log reclamation | segments are retired at a checkpoint: 20,000 rows written in WAL leave one 112-byte segment after `wal_checkpoint(TRUNCATE)` | WAL reset on checkpoint |
| processes on one file | many, under `PRAGMA locking_mode = normal`: 37 stress rounds, two processes each writing 12,000 rows into one file, zero lost writes. The default is `exclusive` because releasing the file between statements costs the gate 3.78x to 3.03x | many, byte-range locks |
| writers | one at a time, readers never block (snapshot isolation) | one at a time; readers block in rollback mode, not in WAL |
| threads | single-threaded | serialised or multi-thread |
| rollback | undo buffer of before-images, rows and schema; a `DROP` cannot be undone inside a transaction, and attempting it leaves the connection unable to read the table | rollback journal or WAL |
| triggers, foreign keys | yes; `BEFORE`/`AFTER`/`INSTEAD OF`, `FOR EACH ROW`, `WHEN`, `RAISE`, recursion capped at 1,000 frames. Foreign keys compile to triggers, so `PRAGMA foreign_keys`, `DEFERRABLE INITIALLY DEFERRED`, `ON DELETE CASCADE`/`SET NULL`/`SET DEFAULT`/`RESTRICT` and `PRAGMA foreign_key_check` all run through the one mechanism | yes |
| extensions | JSON, FTS5, R-Tree, `inillucent_search` | JSON1, FTS3/4/5, R-Tree, geopoly, session, RBU, … |
| table-valued functions | `pragma_*`, `json_each`, `json_tree`, `generate_series`, and any eponymous module a caller registers | `pragma_*`, `json_each`, `generate_series`, … |
| vector search | `VECTOR(N)` columns, `vector_distance_cos`/`_l2`/`vector_dot`, `CREATE INDEX ... USING inillucent_hnsw`, and a planner rule that turns `ORDER BY vector_distance_cos(v, ?) LIMIT k` into a probe of that index followed by an exact rescore. Recall **1.000** against an exhaustive cosine; the SQL path costs **0.98x** the store's own query | none; pgvector is an extension PostgreSQL loads |
| C API | `inillucent-capi` exports 133 `sqlite3_*` symbols, over the **old** engine, which is now `inillucent-legacy`; a driver and C ABI for the new engine is task-1837 | `sqlite3.h` |
| shell | `inillucent-shell`, a `sqlite3`-shaped shell | `sqlite3` |

The **old** engine, still in the tree, is the one that reached SQLite file-format parity:
`compat/sqlite-3.53.4.toml` holds 271 capabilities of which 264 pass and 7 optional ones are missing
(session extension, pre-update hook, snapshot API, unlock-notify, RBU, geopoly, R-Tree geometry
callbacks). It was measured at 0.05x to 0.70x SQLite across the families, which is why the
rearchitecture happened.

## How it compares with PostgreSQL + pgvector

The retrieval engine is graded by `inillucent-bench` against a correctly configured PostgreSQL with
pgvector (`hnsw.iterative_scan = relaxed_order`, `ef_search 400`, `max_scan_tuples 40000`,
`scan_mem_multiplier 4` on filtered queries), reading byte-identical vectors, over 2,613 queries in
nine families on a 185,078-chunk corpus this repository builds from public data. Each primary
comparison is decided by a 95% paired bootstrap interval and a paired randomisation test against a
threshold declared before the run.

**17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates all
pass.** (`inillucent-scorecard.md`, generated 2026-08-31; not re-run by this review, because nothing
in task-1838 or task-1844 touched the ranking, and `the_retrieval_baseline_is_unchanged` is one of the
suite's four non-`schema_forms` failures — see below.)

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
| serving an index, peak memory | **1.86 GB** (int8 vectors) | a server process plus its indexes; 3,167 MB on the 598k-chunk corpus below |
| building an index, peak memory | 2.68 GB | |
| on disk | 819 MB | |
| build / save / reopen | 175 s on one core / 0.3 s / 5.3 s | |
| processes to run | none | PostgreSQL plus an embedding server |

In production (task-1774, task-1775, task-1779): Nikaya, a Gmail retrieval assistant, moved its
598,560-chunk mailbox from pgvector to inillucent. Semantic p50 went from 80.6 ms cold / 33.7 ms warm
to **4.41 ms**; the lexical branch went from returning **zero rows on 17 of 30** natural-language
questions to zero on none; recall@100 against an exact scan went from 0.899 (min 0.77) to **1.000**,
because the deployed configuration answers every search on the parallel exhaustive path at
**27.1 ms p50 / 28.6 ms p95** over the whole corpus. Cost: **3.83 GB resident** and 3.1 GB on disk for
the index, opening in **3.0 s** because the BM25 postings are written rather than rebuilt, against
3,167 MB of pgvector and GIN index deleted from a 5,849 MB database. The MCP process that used to open
its own copy of the index (3,831.6 MB) now asks the server and holds 13.2 MB.

**What the SQL side of vector search does and does not cover.** `CREATE INDEX ... USING
inillucent_hnsw (v)` builds an `inillucent_search` store over the column, backfills the rows already
there, and is kept in step by the engine applying a statement's row images to the module after the
write and before the commit — so the table and its index are one change. `ORDER BY
vector_distance_cos(v, ?) LIMIT k` is planned onto it as `AccessPath::VectorProbe`, graded at recall
**1.000** against a cosine the test computes itself, and measured at **7.204 ms p50 against the store's
own 7.325 ms** over 20,000 vectors at 256 dims — the SQL path is free.

**The operator spellings landed.** `<->`, `<#>`, `<=>`, `<+>`, `<~>` and `<%>` all parse and bind,
alongside pgvector's distance and vector functions, and `CREATE INDEX ... USING ivfflat` is a second
structure beside the graph - all of it probed in `feature-comparison.md`. What is still cosine-only
is the *ordering the planner puts on the index*: `WITH (metric = ...)` is where a second metric goes
when the structure has one.

## What is not there yet

In priority order for the next ticket, each with the evidence already in the tree. The plan for each
is `tasks/rust-db-phase-2-tdd.md`.

1. **Memory is the one measurement where SQLite wins, and there is no bar on it.** The same plan
   under the same 128 MiB budget: **75.25 MiB against SQLite's 37.19 - 102% more** - while taking
   74% less time and 66% less processor. `compat/perf/contract.toml` has ten elapsed-time families
   and no memory family, so nothing fails when it rises. task-1861 attributed it family by family -
   `schema` 66.79 MiB, `write` 45.25, `large.values` 37.48, `extension` 33.14 - and ruled out the
   allocator (2.4 MiB, and swapping it costs 13% more time) and the 32 KiB page size (4 KiB pages are
   *larger*). The index build is the single biggest consumer.
2. **The default page cache is 64x SQLite's**: `PRAGMA cache_size` reports **-131072** here and
   **-2000** there. It is a real switch, and setting it to SQLite's default takes a shell scanning
   200,000 rows from 23.1 MiB resident to 9.3 with the wall clock unchanged - but doing the same to
   the gate's whole plan costs the headline 3.85x to 1.76x. The default wants deciding by
   measurement rather than leaving where it landed.
3. **Three elapsed-time bars are missed**, on four consecutive 30-round runs: `open.prepare` 26% less
   time against a bar asking 80%, `extension` 17% against 33%, `schema` 17% against 67%. And
   `open.prepare`'s lower bound fell **under the 1.00x floor on two of the four runs** (0.99x,
   0.95x), which a single run cannot settle.
4. **Linux**: 1.53x weighted where Windows is now 3.80x-3.91x, and shown by experiment to be the same
   absolute work rather than a Linux-specific fix (task-1838 §5). The allocator that moved Windows
   from 3.24x to 3.86x has not been measured there.
5. **`txn.large` at 0.18x-0.27x and `write.insert.batch` at 0.51x-0.56x.** Both are inside met
   families now, so neither blocks the floor, and both have a named plan in Phase 3's Part E: route
   `UPDATE`'s read through the borrowing probe rather than the allocating `PagedTree::point`,
   overwrite a same-width value in place, one log record per update; and keep delta entries sorted by
   pre-encoded key and bisect.
6. **`extension.fts.build` at 0.35x-0.37x**, which is the module's own cost - task-1835 moved the
   query path and the build path is what task-1856 profiled. The gate now prints the breakdown beside
   the ratio, the way it does for `schema.index`: 500 documents, `content` 1.3 ms, `tokenize` 0.4,
   `docsize` 1.0, `group` 0.2, `terms` 0.3, `new terms` 0.4, `dict write` 1.7, `flush` 2.7. What is
   left is **not** micro-cost. Writing the dictionary in perfect key order still costs ~3.2 us a row
   against ~2.4 us for a `%_data` row, and this engine's own `write.insert.batch` is 15.7 us a row, so
   a shadow write is not slow. FTS5 here does four tree writes per document - `%_content`,
   `%_docsize`, the new term's `%_idx` row and its `%_data` doclist - where SQLite's accumulates the
   batch in memory and writes a handful of segment blobs at commit. Closing the rest is a segment
   format change touching every reader of `%_idx` and `%_data`.
7. **Deleting the old engine.** Re-rooting is done: `inillucent` is a re-export of
   `inillucent-engine`, and the old facade is `inillucent-legacy`. What is left is the deletion -
   `inillucent-legacy`, `inillucent-capi`, `inillucent-session`, `inillucent-vm`,
   `inillucent-transaction` and `inillucent-storage` minus its reader - and it is blocked on
   **task-1837**'s driver, which is the C ABI that replaces `inillucent-capi`.
8. **The retrieval index's footprint**: 3.83 GB resident for a 3.1 GB index of 598,560 chunks. The
   postings are persisted and the graph build is parallel; nothing has tried to make the resident set
   smaller.
9. **Multi-thread access.** Multi-process access landed in task-1860 - the same
   SHARED/RESERVED/PENDING/EXCLUSIVE protocol, under `PRAGMA locking_mode = NORMAL`, measured over 37
   stress rounds with two writing processes and no lost writes - and threading has not.

**The 17 failing tests, all accounted for**, and every one of them was failing before task-1845 too.

| binary | count | what they are |
|---|---|---|
| `schema_forms.rs` | 14 | **all 14 fail on the same thing**: they need the pinned `sqlite3` to read a file this engine wrote, or the reverse, which task-1816 withdrew as a requirement. Two of them (`strict_tables_refuse_the_wrong_class`, `strict_is_enforced_on_a_file_sqlite_wrote`) now get past every `STRICT` assertion in them and fail only at that step - the enforcement they were written for is in place, and `semantics.rs` and `cli.rs` cover it against the oracle without needing file-format interop |
| `planner.rs` | 2 | `sqlite_stat1` exchanged with the oracle: file-format interop of the same class |
| `ordering.rs` | 1 | a known tie-order difference |
| `harness.rs` | 1 | the retrieval baseline, which drifted before this sprint |

task-1844's recommendation - retire the file-format cases in `schema_forms.rs` to `_junk/` the way the
other sixteen were retired, and keep the rest as tests that drive the engine - is still open, and
task-1845 strengthens it: all fourteen now fail for one reason, and it is a requirement the project
has already dropped.

## Repository layout

| group | crates | non-test lines |
|---|---|---|
| shared foundation | `inillucent-base`, `inillucent-vfs`, `inillucent-value`, `inillucent-sim` | 15,431 |
| shared SQL front end | `inillucent-sql` (lexer, parser, binder, planner), `inillucent-scalar` (functions, JSON, window frames), `inillucent-catalog`, `inillucent-ext` (registry, vtab contract, FTS5, R-Tree) | 37,400 |
| new engine | `inillucent-pool`, `inillucent-wal`, `inillucent-tree`, `inillucent-txn`, `inillucent-exec`, `inillucent-engine`, `inillucent-model` (test oracle), `inillucent-sqlite-reader` (import only) | 54,341 |
| old engine, to be deleted | `inillucent-storage`, `inillucent-transaction`, `inillucent-vm`, `inillucent-session`, `inillucent-legacy`, `inillucent-capi` | 48,694 |
| retrieval | `inillucent-core` (the engine), `inillucent-search` (the virtual table), `inillucent-bench` (the pgvector grading harness) | 29,825 |
| facade and tooling | `inillucent` (re-export of the new engine), `inillucent-compat` (manifest, oracle, gates, 67 test files), `inillucent-cli`, `inillucent-migrate` | 32,031 |

`inillucent-engine::connect::Database` is the entry point to the new engine: `open` creates or
opens-and-recovers, `import` reads a SQLite file, `connect` gives a `Connection` with `execute_batch`,
`query`, `prepare_with_tail` and `explain`. `inillucent::Database` is a re-export of it — the two
surfaces no longer differ, so there is no wrapper. `architecture.md` and `product-overview.md` describe
the retrieval engine; `tasks/task-1816-rearchitecture-tdd.md` is the design the new engine follows;
`docs/invariants/layering.toml` is the dependency contract a test enforces. `feature-comparison.md`
is the measured side-by-side against SQLite and against pgvector.

**`drivers/` is the sub project an application binds to**, and it is committed:
`drivers/inillucent-driver` holds every decision, `drivers/inillucent-driver-capi` is the C ABI over it
as a `cdylib` and a `staticlib`, and `drivers/README.md` is the front door for somebody writing a
binding who is not working on the engine. `harness.rs` asserts every path in `[workspace] members`
exists, so the next crate added before it is committed fails on the machine that added it rather than
on the next clone.

## Building, testing and reproducing the numbers

```sh
cargo build --release
cargo test --workspace --no-fail-fast        # 181 binaries; 17 red, all accounted for

# The gate binaries and the shell install `inillucent-alloc` as their global
# allocator. It is part of the build the way fat LTO and one codegen unit are:
# SQLite ships its own memory subsystem, and measuring a Rust workspace on the
# platform allocator measures a build configuration rather than an engine.
# task-1845 measured it at 3.24x -> 3.86x on the medium gate.

pwsh tools/sqlite-reference.ps1              # the pinned SQLite 3.53.4 oracle (Windows)
bash tools/sqlite-reference.sh               # Linux

# The fixtures are not checked in, and each gate run needs its own copy of one:
# the schema.index workload leaves an index behind on the SQLite arm.
bash _agent_output/task-1819-readgate/reproduce/build-fixtures.sh <dir>
cp <dir>/medium.db <dir>/medium-run1.db

target/release/inillucent-fullgate <dir>/medium-run1.db --scale medium --rounds 30 --page-size 32768 --frames 4096
target/release/inillucent-readgate <dir>/medium-read.db --scale medium
target/release/inillucent-probeprofile <dir>/medium-probe.db --scale medium --page-size 32768 --frames 4096
target/release/inillucent-searchgate --documents 500 --rounds 30
target/release/inillucent-shellrss                   # peak RSS, one shell each, same data
target/release/inillucent-vectorprobe --rows 20000 --dims 256   # what the SQL vector path costs
target/release/inillucent-childcost target/release/inillucent-bench open --dir <index>  # load time, peak RSS
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
