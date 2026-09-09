# SQL support

inillucent speaks SQLite's SQL dialect on its own storage. This page says which SQL runs, which
thirteen cases out of 416 do not produce SQLite's exact bytes, and which constructs are refused.

**None of the thirteen is a missing feature, and none of them is silent.** All thirteen answer, and
each reports something a caller can read. [Feature comparison](feature-comparison.md) is the same
material in full, table by table.

## How this was measured

416 SQL scripts were run through `inillucent-shell` and through a pinned `sqlite3` 3.53.4, each over
its own fresh database, and every byte of both output streams was compared.

- **403 of 416 produce SQLite's exact bytes.**
- **409 of 416 work**, once the six vector search cases are counted for what they are: features
  SQLite does not have, so there is no SQLite output for them to match.
- **0 are refused here that SQLite answers, and 0 are accepted here that SQLite rejects.**

208 of those cases are also a checked in test, `crates/inillucent-compat/tests/semantics.rs`, so a
construct that changes its answer in either direction fails a build rather than waiting for somebody
to audit it. The probe harness is `tools/feature-probe/`.

Counted against SQLite's own enumerations rather than against a case list: **212 function names of
218**, **67 pragmas of 67**, **63 dot commands of 65**, **5 collations of 5**.

## What runs

**Queries.** `SELECT` with inner, cross and outer joins, planned as a hash join, an index nested loop
or a scan. `GROUP BY`, `HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT` and `OFFSET`. Compound selects
(`UNION`, `UNION ALL`, `EXCEPT`, `INTERSECT`). Common table expressions, including recursive ones.
Derived tables in `FROM`. Subqueries in `WHERE`, in `IN`, in `EXISTS` and as values, including
correlated ones. Window functions with all three frame units.

**Writes.** `INSERT`, `UPDATE` and `DELETE`, with `RETURNING`, with `ON CONFLICT DO UPDATE` and
`DO NOTHING`, and with `UPDATE ... FROM`. `WITH` on all three. Row values in every comparison and in
their `IN` form.

**Schema.** `CREATE TABLE`, including `WITHOUT ROWID` and `STRICT`, and `CREATE TABLE ... AS SELECT`.
`CREATE INDEX` through a bottom up bulk builder, including partial indexes, indexes on expressions,
and indexes on a `WITHOUT ROWID` table. `CREATE VIEW`. `CREATE VIRTUAL TABLE`. `CREATE TRIGGER` with
`BEFORE`, `AFTER` and `INSTEAD OF`, `FOR EACH ROW`, `WHEN` and `RAISE`. All four `ALTER TABLE` forms.
`DROP`. `ANALYZE`, which writes `sqlite_stat1`. `REINDEX`. `VACUUM` and `VACUUM INTO`.

**Constraints.** `NOT NULL`, `UNIQUE`, `PRIMARY KEY`, `CHECK`, `DEFAULT`, and foreign keys with all
five referential actions, immediate and deferred, plus `PRAGMA foreign_key_check`. Foreign keys
compile to triggers, so one mechanism serves `PRAGMA foreign_keys`,
`DEFERRABLE INITIALLY DEFERRED`, `ON DELETE CASCADE`, `SET NULL`, `SET DEFAULT` and `RESTRICT`.

**Values.** Type affinity applied on write. `CAST`. `COLLATE` with `BINARY`, `NOCASE` and `RTRIM`.
`LIKE` and `GLOB`. 212 built in function names, including 28 JSON functions, 29 maths functions and
7 date and time functions. User defined scalar functions, aggregates and collations.

**Transactions.** `BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `RELEASE` and `ROLLBACK TO`. `ATTACH`
and `DETACH`, and temporary objects, with a super journal deciding a commit that spans two files.

**Introspection.** `EXPLAIN QUERY PLAN` in SQLite's idiom, plain `EXPLAIN`, `sqlite_schema` and
`sqlite_master`, `table_info`, `table_xinfo`, `table_list`, `index_list`, `index_xinfo`,
`database_list`, `integrity_check`, `quick_check`, and the table valued pragma forms such as
`FROM pragma_table_info('t')`.

**Table valued functions.** `generate_series`, `json_each`, `json_tree`, the `pragma_*` family, and
any eponymous module a caller registers.

## Extensions

| extension | state |
|---|---|
| **JSON** | over a binary form, with all 28 function names |
| **FTS5** | including `bm25()` and `fts5vocab` |
| **R-Tree** | the module and its queries |
| **`inillucent_search`** | this engine's own hybrid vector and keyword index — see [Vector search](vector-search.md) |

Every extension's shadow tables are ordinary trees in the same file, so they commit and roll back
with the transaction that wrote them.

## The thirteen cases that are not byte for byte

### Six are vector search, which SQLite does not have

The `vec0` table, the distance functions and the operator spellings `<->`, `<#>`, `<=>`, `<+>`, `<~>`
and `<%>`. All six work. There is no SQLite output for them to be equal to, so they cannot count as
agreement however well they behave. This can never be closed, and closing it is not wanted.

### Three describe SQLite's own C structures

`EXPLAIN`'s bytecode program, `.vfslist`'s `szOsFile`, and `.stats`' lookaside counters. Each prints
the same report, in the same shape, over the facts this engine has.

`EXPLAIN` lays out SQLite's eight columns under SQLite's own header and column widths, so a listing
lines up under the same `addr opcode p1 p2 p3 p4 p5 comment` rule. What the rows hold is this
engine's operator chain, because SQLite lists the opcodes of a bytecode program and this engine
compiles none. `bytecode('...')` reads the same rows.

`.vfslist` prints four lines per file system in the reference's format, over the two this build has.
`szOsFile` is the size of a C struct in a library that is not linked into this program.

`.stats` prints the same two column shape over the counters this engine keeps: page cache fetches,
hits, misses and rewarms, frames cooled and evicted, pages read and written. Lookaside slots and
pcache overflow bytes are facts about SQLite's allocator.

Printing SQLite's numbers in these three would mean printing facts about a library that is not here.
That is a fabrication rather than compatibility, so none of the three will ever be closed.

### Three are decisions this engine made, and each was measured

Both experiments below were run when the weighted headline stood at 3.83x rather than today's 4.26x, and neither has been re-run since. What they measure is the *difference* between the two settings, which is why they are still quoted.

**`PRAGMA page_size` reports 32768** where SQLite reports 4096. Both were measured on the same gate:
32768 gave 3.83x weighted with the `schema` family at 1.15x; 4096 with a matched cache budget gave
3.44x with `schema` at **6% slower than SQLite**, under the floor the performance contract requires.
The pragma reports what the file is, which is its job.

**`PRAGMA locking_mode` reports `exclusive`.** `normal` works and gives real access from several
processes: 37 stress rounds, two processes each writing 12,000 rows into one file, zero lost writes
and zero failed integrity checks. `exclusive` is the *default* because running the gate with `normal`
as the default read 3.03x with a lower bound of 2.95x, under the 3.00x bar — it takes `write` from
1.94x to 1.19x, `transaction` from 11% slower to 170% slower, and `schema` from 1.34x to 52% slower.
Releasing the file between statements means reading the meta record again before each one, in every
program, including every program that never opens a second connection.

**`.recover`** differs on one line of nineteen, and it is the line that names the page size.

Adopting SQLite's values in the first two would take the byte for byte number from 403 to 406, at a
measured cost to the performance bars.

### One is the two pinned reference artifacts disagreeing with each other

`.limit` reports `trigger_depth 1000`. The downloaded `sqlite3.exe` says 100, because it was built
with `SQLITE_MAX_TRIGGER_DEPTH=100`, which its own `PRAGMA compile_options` confirms. The pinned
amalgamation's default is 1000, and the locally built oracle reports 1000. Twelve of the thirteen
lines agree. No value closes this row: whichever of the two references is agreed with, the other one
disagrees.

## What is refused, by name

A refusal is visible and an application can work around it. These say what they are, and each returns
exit code `3` rather than `1`, so a caller can tell "not built" from "your SQL is wrong".

| construct | why |
|---|---|
| `.expert` and `.session` | the two of `sqlite3`'s 65 dot commands this shell has not got |
| `fts5(...)`, `fts5_locale()`, `fts5_get_locale()`, `fts5_insttoken()` | four function names that hand out C pointers or belong to FTS5's locale machinery. A stub would be a wrong answer rather than a missing one |
| `fts3_tokenizer()` | the same, and absent from the pinned SQLite library too, so it is a difference against the shell rather than against the library an application links |
| modules `fts4aux` and `fts3tokenize` | also absent from the pinned library, so also a difference against the shell. The FTS5 equivalent `fts5vocab` is here |
| a second writer | one writer at a time. Readers never block, under snapshot isolation |
| threads inside one process | the engine is single threaded by construction. Several *processes* on one file are supported under `PRAGMA locking_mode = normal` |
| SQLite's file format | this engine writes its own format. A SQLite file is imported with [`inillucent migrate`](migrating.md), not opened in place |

## Where this differs in behaviour rather than in output

| | inillucent | SQLite 3.53.4 |
|---|---|---|
| file format | `.rdb`, with its own redo log segments | SQLite's |
| page size | 32 KiB by default, 8 to 64 KiB allowed | 4 KiB by default |
| page cache | a pool of frames, 4,096 frames at 128 MiB by default, set when the file is opened. A frame's page is allocated the first time that frame is claimed, so the budget is a ceiling rather than an amount taken at open | `cache_size`, 2 MiB by default, also grown into |
| journal modes | all six, and `delete` is the default as it is in SQLite. `PRAGMA journal_mode = wal` selects the redo log, and a database left in WAL reopens in WAL | six |
| writers | one at a time; readers never block, under snapshot isolation | one at a time; readers block in rollback mode, not in WAL |
| processes on one file | many, under `PRAGMA locking_mode = normal` | many, over byte range locks |
| threads | one | serialised or multi thread |
| rollback | an undo buffer of before images, for rows and for schema | rollback journal or WAL |
| a transaction larger than the page pool | must fit. The pool does not evict a dirty page before its commit | spills to the journal |
| a `SELECT` result | produced when the first row is stepped; later steps walk rows already produced | streamed one row per step |

Two of those are limits worth planning around. A transaction that writes more pages than the pool
holds needs a larger pool, set when the file is opened. And a `DROP TABLE` cannot be undone inside a
transaction: attempting it leaves the connection unable to read that table.

## Reproducing the probe

```sh
cargo build --release
pwsh tools/sqlite-reference.ps1      # the pinned SQLite 3.53.4 oracle, on Windows
bash tools/sqlite-reference.sh       # on Linux
cargo run -p inillucent-compat --bin inillucent-manifest -- check
cargo run -p inillucent-compat --bin inillucent-manifest -- report
```

[Repository](repository.md) covers the test runner and the rest of the assurance program.
