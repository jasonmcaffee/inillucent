# SQL support

inillucent speaks SQLite's SQL dialect on its own storage. This page says which SQL runs and which
cases out of 416 do not produce SQLite's exact bytes.

**Nothing is refused.** Twelve cases are not byte for byte: six answer differently, and six are
vector search features SQLite has no equivalent for. None of the twelve is silent. Each answers,
and each reports something a caller can read. [Feature comparison](feature-comparison.md) is the same
material in full, table by table.

Words used here and not explained here - pragma, collation, affinity, rowid, storage class - are
in [the glossary](glossary.md), one sentence each. Every pragma is in [Pragmas](pragmas.md).

## How this was measured

416 SQL scripts were run through `inillucent-shell` and through a pinned `sqlite3` 3.53.4, each over
its own fresh database, and every byte of both output streams was compared.

- **404 of 416 produce SQLite's exact bytes** - 97.1% of the total, and 98.5% of the 410 cases that
  have a SQLite answer to compare against.
- **0 are refused here that SQLite answers**, and 0 are accepted here that SQLite rejects. Window
  functions were the last twelve cases to close: all eleven window-only functions, every frame unit,
  every bound and every `EXCLUDE` clause now match the pinned SQLite exactly.
- **6 answer differently, and 6 are vector search features SQLite has no equivalent for**, so there
  is no SQLite output for them to match.

208 of those cases are also a checked in test, `crates/inillucent-compat/tests/semantics.rs`, so a
construct that changes its answer in either direction fails a build rather than waiting for somebody
to audit it. The probe harness is `tools/feature-probe/`.

Counted against SQLite's own enumerations rather than against a case list: **172 of the 177 function
names the pinned SQLite library answers**, **67 pragmas of 67**, **63 dot commands of 65**, **5
collations of 5**. [The function register](#the-function-register) says where 177 comes from and names
the five.

**Two pragma counts, and they are different questions.** 67 is what SQLite's own `pragma_list`
reports, and this engine answers every one of them.
The register holds the **68 pragmas this engine recognises**, the extra being `defensive`, which
SQLite exposes through `sqlite3_db_config` rather than as a pragma. [Pragmas](pragmas.md) is the whole table, generated from the register, and
`tools/doc-facts/check.mjs` fails when a document names a different number.

### The function register

`PRAGMA function_list` in SQLite's own shell reports **218** names, and 41 of those are extensions the
shell itself defines rather than functions the library answers: `readfile`, `writefile`, `sha3`,
`zipfile`, `ieee754`, the `decimal` family, the `shell_*` helpers and the rest. They are not part of
SQLite, so they are not a gap here. That leaves **177 library names**.

inillucent answers **172** of the 177. The five it does not are `fts3_tokenizer`, `fts5`,
`fts5_get_locale`, `fts5_insttoken` and `fts5_locale`: the first two hand out a C pointer to a
tokenizer and to the FTS5 API, and the other three are FTS5's locale machinery, which this build does
not carry.

Its own register holds **190** names: those 172, plus 18 vector functions SQLite has no equivalent
for. `inillucent functions` prints 213 rows because it prints one row per name and argument count.

It also names what the connection itself has registered - anything an application defined through
`create_scalar_function` or `create_aggregate_function`, and `embed(TEXT)` in a build carrying the
`embed` feature, where the count is 214. Those rows carry `builtin = 0`. Until this was fixed, the
register read the static built-in list alone, so `embed` answered `SELECT length(embed('hello'))` with 3072
and `inillucent functions embed` printed nothing.

**Where a registered function may be called from is decided by its flags.** A registration is
`direct_only` unless it says otherwise, and a `direct_only` function may be named by a statement and
not by the schema: not by a `DEFAULT`, a `CHECK`, a generated column, an index expression, a
partial-index predicate, a view or a trigger. A function that is neither `direct_only` nor
`innocuous` may be named by the schema only while `PRAGMA trusted_schema` is on, which it is by
default. A schema that names one it may not is refused with `<name> may only be used from top-level
SQL`, when the statement that reads it is bound - and at `CREATE INDEX` for an index expression,
because that is the one form this engine binds while it builds it. `embed(TEXT)` is `direct_only`:
it loads a 275 MB model, and a `CHECK` that named it would load that model on every insert.

```sh
inillucent functions --output json --limit 0
node tools/feature-probe/registers.js    # both registers, compared name by name
```

## What runs

**Queries.** `SELECT` with inner, cross and outer joins, planned as a hash join, an index nested loop
or a scan. `GROUP BY`, `HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT` and `OFFSET`. A `HAVING` needs no `GROUP BY`
before it: a query with an aggregate among its result columns is one group over the whole table, and
`SELECT count(*) AS n FROM t HAVING n > 0` filters that one group. A `HAVING` on a query with no
`GROUP BY` and no aggregate among its result columns is refused, in SQLite's words -
`HAVING clause on a non-aggregate query` - because SQLite refuses it too. Compound selects
(`UNION`, `UNION ALL`, `EXCEPT`, `INTERSECT`). Common table expressions, including recursive ones.
Derived tables in `FROM`. Subqueries in `WHERE`, in `IN`, in `EXISTS` and as values, including
correlated ones. A correlated `IN` is answered by rewriting it as `EXISTS`, which keeps SQLite's NULL
rules: an empty list is false even for a NULL on the left, a NULL on the left over a list with rows
is NULL, and a list holding a NULL turns a non-match into NULL. The one shape that is refused is a
correlated `IN` whose block groups, limits or is itself a compound query, because the rewrite puts
the equality in the block's `WHERE` and a `WHERE` runs before either.

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
`LIKE` and `GLOB`. 190 built in function names, including 30 JSON functions, 29 maths functions and
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
| **JSON** | over a binary form, with all 30 function names, `json_*` and `jsonb_*` alike |
| **FTS5** | including `bm25()` and `fts5vocab` |
| **R-Tree** | the module and its queries |
| **`inillucent_search`** | this engine's own hybrid vector and keyword index. [Vector search](vector-search.md) covers it |

Every extension's shadow tables are ordinary trees in the same file, so they commit and roll back
with the transaction that wrote them.

## The twelve cases that are not byte for byte

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

### Two follow from a decision this engine made, and it was measured

The experiment below was run against the earlier 3.83x weighted headline and has not been taken
again. What it measures is the *difference* between two settings, and that difference is what is
quoted here.

**`PRAGMA page_size` reports 32768** where SQLite reports 4096. Both were measured on the same gate:
32768 gave 3.83x weighted with the `schema` family at 1.15x; 4096 with a matched cache budget gave
3.44x with `schema` at **6% slower than SQLite**, under the floor the performance contract requires.
The pragma reports what the file is, which is its job.

**`PRAGMA locking_mode` used to be a third row here and is not any more.** It reports `normal`, where
SQLite also reports `normal`. It is the default because `exclusive` never releases the file between
statements, so a second process either waits out the whole life of the first or reads state from
before it. `exclusive` is still a real switch, and a program that never opens a second connection
can take it for the throughput: releasing the file between statements means reading the meta record
and the log's tail again before each one.

What multi-process access means here is one writer at a time. A second writer waits up to
`PRAGMA busy_timeout` and is then refused with `busy`, naming what the holder has the file for.
Rows in the file equal commits acknowledged, which
`crates/inillucent-compat/tests/process_concurrency.rs` asserts against two real writer processes.

**`.recover`** differs on one line of nineteen, and it is the line that names the page size.

Adopting SQLite's 4,096 byte page would close both `PRAGMA page_size` and `.recover`, and take the
byte for byte number from 404 to 406, at the measured cost to the `schema` family above.

### One is the two pinned reference artifacts disagreeing with each other

`.limit` reports `trigger_depth 1000`. The downloaded `sqlite3.exe` says 100, because it was built
with `SQLITE_MAX_TRIGGER_DEPTH=100`, which its own `PRAGMA compile_options` confirms. The pinned
amalgamation's default is 1000, and the locally built oracle reports 1000. Twelve of the thirteen
lines agree. No value closes this row: whichever of the two references is agreed with, the other one
disagrees.

## What is refused, by name

**No SQL statement is refused.** What remains on this list is a shell command, five function names,
two modules and three properties of the engine. Each says what it is, and a construct that is not
built returns exit code `3` rather than `1`, so a caller can tell "not built" from "your SQL is
wrong".

| construct | why |
|---|---|
| `.expert` and `.session` | the two of `sqlite3`'s 65 dot commands this shell has not got |
| `fts5(...)`, `fts5_locale()`, `fts5_get_locale()`, `fts5_insttoken()` | four function names that hand out C pointers or belong to FTS5's locale machinery. A stub would be a wrong answer rather than a missing one |
| `fts3_tokenizer()` | the same, and absent from the pinned SQLite library too, so it is a difference against the shell rather than against the library an application links |
| modules `fts4aux` and `fts3tokenize` | also absent from the pinned library, so also a difference against the shell. The FTS5 equivalent `fts5vocab` is here |
| a second writer | one writer at a time. A reader is refused with `busy` while a writer holds the file, after `PRAGMA busy_timeout`; there is no shared-memory log index, so there is no snapshot for a reader to read from while a writer is working. `docs/roadmap.md` has the protocol that would change that |
| threads inside one process | the engine is single threaded by construction. Several *processes* on one file are supported under `PRAGMA locking_mode = normal` |
| SQLite's file format | this engine writes its own format. A SQLite file is imported with [`inillucent migrate`](migrating.md), not opened in place |

## Where this differs in behaviour rather than in output

| | inillucent | SQLite 3.53.4 |
|---|---|---|
| file format | `.rdb`, with its own redo log segments | SQLite's |
| page size | 32 KiB by default, 8 to 64 KiB allowed | 4 KiB by default |
| page cache | a pool of frames, 4,096 frames at 128 MiB by default, set when the file is opened. A frame's page is allocated the first time that frame is claimed, so the budget is a ceiling rather than an amount taken at open | `cache_size`, 2 MiB by default, also grown into |
| journal modes | all six, and `delete` is the default as it is in SQLite. `PRAGMA journal_mode = wal` selects the redo log, and a database left in WAL reopens in WAL. What the mode selects here is how a **checkpoint** is protected: an application's `ROLLBACK` is undone from the log under every mode, including `off`, so `memory` and `off` are one choice rather than two | six |
| writers | one at a time; a reader waits for a writer, and is refused with `busy` after `PRAGMA busy_timeout` | one at a time; readers block in rollback mode, not in WAL |
| processes on one file | many, under `PRAGMA locking_mode = normal` | many, over byte range locks |
| threads | one | serialised or multi thread |
| rollback | an undo buffer of before images, for rows and for schema | rollback journal or WAL |
| a transaction larger than the page pool | must fit. The pool does not evict a dirty page before its commit | spills to the journal |
| a `SELECT` result | produced when the first row is stepped; later steps walk rows already produced | streamed one row per step |

Two of those are limits worth planning around. A transaction that writes more pages than the pool
holds needs a larger pool, set when the file is opened. And a `DROP TABLE` cannot be undone inside a
transaction: attempting it leaves the connection unable to read that table.

### Four smaller differences, each with a test that holds it still

A correctness audit measured these and they are not fixed. Each one has a test asserting
the behaviour as it is, so a change to any of them is a change somebody made on purpose.

| | inillucent | SQLite 3.53.4 |
|---|---|---|
| `pragma_foreign_keys` as a table-valued function | not offered. The table-valued forms are the pragmas that answer rows; `foreign_keys` is a setting and is reachable as `PRAGMA foreign_keys` | offered, one row holding the flag |
| an index on a `VIRTUAL` generated column | refused. `CREATE INDEX` needs a stored value to key on, and a `VIRTUAL` column has none in the row | allowed; the index stores the computed value |
| `vector-search`'s result columns | the primary key appears twice: once as the table's own column and once as the column the search names | no equivalent; SQLite has no vector search |
| a JSON-text vector handed to the `inillucent_search` hybrid table | refused as `syntax`, which does not say that the argument was the wrong shape | no equivalent |

The first two are also absent from `inillucent capabilities`, which is why they are written down
here: the capability table is checked against the running engine in both directions, and a row that
does not exist is the one thing it cannot check.

## Reproducing the probe

```sh
cargo build --release
pwsh tools/sqlite-reference.ps1      # the pinned SQLite 3.53.4 oracle, on Windows
bash tools/sqlite-reference.sh       # on Linux
cargo run -p inillucent-compat --bin inillucent-manifest -- check
cargo run -p inillucent-compat --bin inillucent-manifest -- report
```

[Repository](repository.md) covers the test runner and the rest of the assurance program.
