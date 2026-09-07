# Feature comparison

**inillucent against SQLite 3.53.4, and its retrieval engine against PostgreSQL + pgvector.**

The project has two goals and this document is the scorecard for both:

1. a **highly performant SQLite replacement offering the same features**, and
2. an **embedding solution that matches pgvector**.

Written for task-1858 on **2026-09-07** at commit `382eb78`, from a differential probe run that day
on this machine. Every row below is a measurement, not a reading of the source: each feature is a
whole SQL script run through `inillucent-shell` and through the pinned `sqlite3` 3.53.4, over its own
fresh database, with every byte of both streams compared. That is the discipline
`crates/inillucent-compat/tests/semantics.rs` applies to 110 constructs, widened here to **416 cases
across the whole feature surface**. The harness is checked in as `tools/feature-probe/` and its
transcripts are under `_agent_output/`; [Reproducing this](#reproducing-this) says how to run it.

---

## The headline

| | |
|---|---|
| **416 probed features** | **302 agree with SQLite byte for byte** |
| features SQLite answers and inillucent refuses | **45** |
| features both answer, **differently** | **52** — and 14 of those are silent |
| features inillucent accepts that SQLite rejects | **2** |
| vector features with no SQLite equivalent | **5**, all working |

**The four goals, measured:**

| goal | state |
|---|---|
| Same SQL as SQLite | **Not yet.** The core language is essentially complete — every case in `select`, `compound`, `cte`, `window`, `txn`, `attach`, `temp`, `constraint`, `ddl-index`, `ddl-view`, `fn-math` and `integrity` agrees. What is missing is concentrated: **four of the five join spellings** (`USING`, `NATURAL`, `NATURAL LEFT`, `LEFT ... USING`) fail with `ambiguous column name`, and a **self join** is refused. |
| Same observable semantics | **Fourteen silent differences**, listed in [What is silently different](#what-is-silently-different). The one that matters most is `ON CONFLICT ... DO UPDATE ... WHERE`, whose `WHERE` is not consulted, so an upsert applies an update SQLite skips. |
| The PRAGMA surface an application uses | **21 of the 67 pragmas SQLite lists answer. 38 more are accepted and answer nothing at all** — no value, no error. `user_version` is one of them, and it is what every migration framework reads. |
| Embedding search like pgvector | **The ranking is better and the SQL surface is thinner** — 15 of 17 primary comparisons better than pgvector with none worse, but **a filtered vector search under the index returns a tenth of the rows it should**, silently. See [Vector search](#vector-search-against-postgresql--pgvector). |
| Faster than SQLite | **Yes**, and not re-measured here. task-1856's four 30-round gates at this commit read a weighted lower bound of **3.79x–3.96x** at medium against a 3.00x bar, with every family above the 1.00x floor. |

**The gaps ranked, and where the work is:** [What is missing, in the order it should be closed](#what-is-missing-in-the-order-it-should-be-closed).
The implementation ticket is **task-1859**.

---

## How to read the tables

Each table is one feature per row, side by side. SQLite 3.53.4 is the reference, so its column says
what it does; inillucent's column says whether it does the same.

| symbol | meaning |
|---|---|
| **yes** | byte-for-byte the same answer, including the error message when both refuse |
| **differs** | both answer, and the answers are not the same |
| **silent** | both answer, the answers are not the same, and **nothing tells the caller** — the worst outcome, because an application cannot see it |
| **no** | SQLite answers and inillucent refuses, by name |
| **extra** | inillucent has it and SQLite does not |

---

## SQL statements and clauses

### SELECT — 17 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `WHERE`, `ORDER BY`, `LIMIT`, `OFFSET` (both spellings) | yes | **yes** |
| `DISTINCT`, over one column and several | yes | **yes** |
| `GROUP BY`, on a column and on an expression | yes | **yes** |
| `HAVING`, including on a select alias | yes | **yes** |
| `ORDER BY` by ordinal, `NULLS FIRST`/`LAST`, over a window function | yes | **yes** |
| `VALUES` as a statement and as a `FROM` term | yes | **yes** |
| `SELECT` with no `FROM`, qualified star, aliases | yes | **yes** |
| aggregate over an empty set | yes | **yes** |
| `count(DISTINCT a, b)` | refused, `wrong number of arguments` | **refused**, same message |
| a bare column beside an aggregate (`SELECT id, max(a) FROM t`) | picks the row `max` came from | **no** — *the physical pass does not handle the expression a rowid outside an aggregate yet* |
| `SELECT DISTINCT a ... ORDER BY b` | yes | **no** — *ORDER BY over an expression not in a DISTINCT select list* |

### Joins — 10 of 15

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `JOIN ... ON`, `LEFT`, `RIGHT`, `FULL`, `CROSS` | yes | **yes** |
| comma join, four-table join, join onto a subquery | yes | **yes** |
| `LEFT JOIN` with a `WHERE` on the right table | yes | **yes** |
| `JOIN ... USING (k)` | yes | **no** — `ambiguous column name: k` |
| `LEFT JOIN ... USING (k)` | yes | **no** — same |
| `NATURAL JOIN` | yes | **no** — same |
| `NATURAL LEFT JOIN` | yes | **no** — same |
| `USING` across three tables | yes | **no** — same |
| a self join (`FROM t x JOIN t y ON y.a = x.a AND y.id > x.id`) | yes | **no** — *a rowid range as an inner join term* |

**This is the largest single gap in the SQL surface.** `USING` and `NATURAL` are not exotic — they
are how a join between two tables that share a key name is normally written, and every one of them
fails on the same message, which suggests one cause rather than five.

### Compound selects, subqueries and CTEs — 26 of 27

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `UNION`, `UNION ALL`, `EXCEPT`, `INTERSECT`, three-way, with `LIMIT` | yes | **yes** |
| scalar subquery, `IN`, `NOT IN` with NULLs, `EXISTS`, `NOT EXISTS` | yes | **yes** |
| correlated subqueries, including with their own `LIMIT` | yes | **yes** |
| derived table in `FROM` | yes | **yes** |
| row values: `(a,b) = (1,2)` and `(a,b) IN (VALUES ...)` | yes | **yes** |
| row value against a subquery: `(a,b) = (SELECT a,b FROM t ...)` | yes | **no** — `unsupported: row values` |
| `WITH`, several terms, a column list, `MATERIALIZED`/`NOT MATERIALIZED` | yes | **yes** |
| `WITH RECURSIVE`, including a tree walk with a depth counter | yes | **yes** |
| `WITH` on `INSERT`, `UPDATE` and `DELETE` | yes | **yes** |

### Window functions — 11 of 11

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `row_number`, `rank`, `dense_rank`, `ntile`, `cume_dist`, `percent_rank` | yes | **yes** |
| `lag`, `lead` with a default, `first_value`, `last_value`, `nth_value` | yes | **yes** |
| `PARTITION BY`, a named `WINDOW` clause reused | yes | **yes** |
| `ROWS`, `RANGE` and `GROUPS` frames | yes | **yes** |
| `EXCLUDE CURRENT ROW` | yes | **yes** |
| `FILTER` on an aggregate **over a window** | yes | **yes** |

### INSERT, UPDATE, DELETE — 22 of 24

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `INSERT VALUES` multi-row, `INSERT ... SELECT`, `DEFAULT VALUES` | yes | **yes** |
| `INSERT OR IGNORE`/`REPLACE`/`ROLLBACK`/`FAIL`/`ABORT`, `REPLACE INTO` | yes | **yes** |
| `UPDATE`, `UPDATE ... FROM`, `UPDATE OR IGNORE`/`OR REPLACE` | yes | **yes** |
| a correlated `UPDATE` subquery | yes | **yes** |
| `DELETE`, `DELETE` all rows | yes | **yes** |
| `RETURNING` on insert, update, delete, with an expression, beside a trigger | yes | **yes** |
| writes to a `WITHOUT ROWID` table, and an upsert on one | yes | **yes** |
| `DELETE`/`UPDATE ... ORDER BY ... LIMIT` | refused (not compiled in) | **refused** — both refuse |

### UPSERT — 5 of 7

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `ON CONFLICT DO NOTHING`, with and without a target | yes | **yes** |
| `ON CONFLICT DO UPDATE` with `excluded`, on a secondary unique index | yes | **yes** |
| `ON CONFLICT DO UPDATE ... RETURNING` | yes | **yes** |
| **`ON CONFLICT DO UPDATE ... WHERE`** | the arm runs only where the predicate holds | **silent** — the predicate is ignored and the arm always runs |
| two `ON CONFLICT` clauses on one statement | yes | **no** — `unsupported: more than one ON CONFLICT clause` |

---

## Schema

### CREATE TABLE — 14 of 18

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| typed and typeless columns, `IF NOT EXISTS`, `DROP TABLE ... IF EXISTS` | yes | **yes** |
| `WITHOUT ROWID`, composite `PRIMARY KEY` | yes | **yes** |
| `STRICT`, and `ANY` inside a `STRICT` table | yes | **yes** |
| generated columns, `VIRTUAL` and `STORED` | yes | **yes** |
| `DEFAULT` literals and expressions, `CURRENT_TIMESTAMP`/`DATE`/`TIME` | yes | **yes** |
| quoted identifiers `"x"`, `[x]`, `` `x` ``, reserved words as column names | yes | **yes** |
| `CREATE TABLE ... AS SELECT` | stores `CREATE TABLE d(a INT,b TEXT)` | **differs** — stores `a INTEGER`, the resolved type rather than the source's |
| `INTEGER PRIMARY KEY DESC` | **not** a rowid alias; a real index is built | **differs** — treated as a rowid alias, no index built |
| `CHECK` containing a subquery | `subqueries prohibited in CHECK constraints` | **accepts** it |
| `WITHOUT ROWID` with no primary key | refused | **refused**, wording differs |

### CREATE INDEX — 12 of 12

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| plain, `UNIQUE`, composite, `COLLATE`, `DESC` | yes | **yes** |
| **partial** (`... WHERE`), on an **expression**, on a **`WITHOUT ROWID`** table | yes | **yes** |
| `DROP INDEX`, `REINDEX`, `REINDEX name` | yes | **yes** |
| `INDEXED BY`, `NOT INDEXED` | yes | **yes** |
| `ANALYZE` writing `sqlite_stat1` | yes | **yes** |

### Views and triggers — 14 of 15

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `CREATE VIEW`, with a column list, over a join, `DROP VIEW` | yes | **yes** |
| writing through an `INSTEAD OF` trigger | yes | **yes** |
| `BEFORE`/`AFTER` `INSERT`/`UPDATE`/`DELETE`, `OLD`/`NEW`, `WHEN`, `UPDATE OF` | yes | **yes** |
| `RAISE(ABORT)`, `RAISE(IGNORE)`, a trigger writing another table, `DROP TRIGGER` | yes | **yes** |
| **`PRAGMA recursive_triggers=ON`** then a self-inserting trigger | recurses to the cap (5 rows) | **silent** — fires once (2 rows) |

### ALTER TABLE — 6 of 8, and the two are message wording

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `RENAME TO`, `RENAME COLUMN`, `ADD COLUMN`, `DROP COLUMN` | yes | **yes** |
| a rename propagating into a view's stored SQL | yes | **yes** |
| `ADD COLUMN NOT NULL DEFAULT` on a populated table | yes | **yes** |
| `ADD COLUMN NOT NULL` with no default, `ADD COLUMN UNIQUE` | refused | **refused**, wording differs |

### Constraints — 16 of 16

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `NOT NULL`, `UNIQUE`, `PRIMARY KEY`, table-level `CHECK`, column `CHECK` on insert and update | yes | **yes** |
| `PRIMARY KEY AUTOINCREMENT`, and `sqlite_sequence` after it | yes | **yes** |
| a constraint's own `ON CONFLICT` clause, `NOT NULL ON CONFLICT REPLACE DEFAULT` | yes | **yes** |
| foreign keys: immediate, `ON DELETE CASCADE`/`SET NULL`/`SET DEFAULT`, `ON UPDATE CASCADE`, `DEFERRABLE INITIALLY DEFERRED` | yes | **yes** |
| `PRAGMA foreign_key_check`, `PRAGMA foreign_key_list` | yes | **yes** |
| a row colliding on two unique indexes at once | yes | **yes** |

Constraints are the part of this engine that has had the most attention — task-1845, task-1849,
task-1850 and task-1856 each closed a set here — and it shows: every case agrees.

---

## Values, expressions and functions

### Type affinity and storage classes — 17 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| affinity applied on write for `INTEGER`, `TEXT`, `REAL`, `BLOB`, `NUMERIC`, and through a rowid alias | yes | **yes** |
| `CAST` between every class, comparison across classes | yes | **yes** |
| integer division, modulo, division by zero, hex literals, blob literals | yes | **yes** |
| `TRUE`/`FALSE`/`NULL`, unicode round trip, NULL arithmetic | yes | **yes** |
| values wider than a page, text and blob | yes | **yes** |
| the literal `-9223372036854775808` | integer | **silent** — becomes a real, `-9.2233720368547758e+18` |
| `0.0/0.0` | NULL | **silent** — `NaN` |

### Operators — 10 of 12

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| arithmetic, concatenation, unary, precedence, bitwise | yes | **yes** |
| `BETWEEN`, `IN` list, `NOT IN`, `CASE` both forms | yes | **yes** |
| `LIKE` with `ESCAPE`, `GLOB`, string comparison, `COLLATE` in an expression | yes | **yes** |
| JSON `->` and `->>` | yes | **yes** |
| `IS DISTINCT FROM`, `IS NOT DISTINCT FROM` | 1, 1 | **silent** — 0, 0 |
| `REGEXP` | the CLI registers one | **no** — `no such function: regexp` |

### Collations — 4 of 5

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `BINARY`, `NOCASE`, `RTRIM` in expressions, columns, `ORDER BY` and unique indexes | yes | **yes** |
| `PRAGMA collation_list` | lists them | **silent** — answers nothing |

### Scalar functions — 15 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `abs`, `sign`, `round`, `max`/`min`, `length`, `substr`, `instr`, `replace` | yes | **yes** |
| `upper`, `lower`, `trim`/`ltrim`/`rtrim` with and without a character set | yes | **yes** |
| `printf`/`format` including `%05.2f` and `%08.3d`, `quote`, `hex`, `unhex`, `char`, `unicode` | yes | **yes** |
| `coalesce`, `ifnull`, `nullif`, `iif`, `typeof`, `likelihood`, `likely`, `unlikely` | yes | **yes** |
| `zeroblob`, `randomblob`, `octet_length`, `concat`, `concat_ws`, `like()`, `glob()` | yes | **yes** |
| `changes()`, `total_changes()`, `last_insert_rowid()` | yes | **yes** |
| blob `instr`/`length`/`substr`, negative `substr` offsets | yes | **yes** |
| `round(1e308, 2)` | `1.0e+308` | **differs** — `Inf` |
| `length(char(0))` | 0 | **differs** — 1 |
| `load_extension` | loads one | **no** — `no such function` |

### Aggregates — 5 of 8

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `count`, `sum`, `total`, `avg`, `max`, `min`, over NULLs and an empty set | yes | **yes** |
| `group_concat` with a separator, `string_agg`, `DISTINCT` inside an aggregate | yes | **yes** |
| **`FILTER (WHERE ...)` on a plain aggregate** | yes | **no** — `unsupported: FILTER on a call with no OVER` |
| **`group_concat(b ORDER BY a DESC)`** | yes | **no** — `unsupported: ORDER BY inside an aggregate` |
| `sum()` overflowing an integer | `integer overflow` | **accepts** — silently returns a real |

### Date and time — 7 of 9

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `date`, `time`, `datetime`, `julianday`, `unixepoch`, `timediff`, round trips | yes | **yes** |
| modifiers: `±N days/months/years/minutes`, `start of month/year/day`, `weekday N`, `auto` | yes | **yes** |
| `strftime` `%f`, `%J`, and the whole common specifier set | yes | **yes** |
| `strftime` `%g`, `%k`, `%l` | `24`, ` 9`, ` 9` | **silent** — echoed back as the literal text `%g`, `%k`, `%l` |
| `strftime('%s', ...)` | `1709283907` | **silent** — `1709283906`, one second low |
| the `subsec` modifier | `2024-03-01 12:00:00.000` | **silent** — no fractional part |

### Maths — 4 of 4

Every trigonometric, hyperbolic, logarithmic, power and rounding function agrees:
`sin cos tan asin acos atan atan2 sinh cosh tanh asinh acosh atanh ln log log2 log10 exp pow power
sqrt ceil ceiling floor trunc mod pi degrees radians`.

### JSON — 9 of 11

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `json`, `json_valid` with flags, `json_quote`, `json_array`, `json_object` | yes | **yes** |
| `json_extract`, `json_type`, `json_insert`/`replace`/`set`/`remove`, `json_patch` | yes | **yes** |
| `json_array_length`, `json_pretty`, `json_error_position`, `json_each`, `json_tree` | yes | **yes** |
| JSON stored in a column and queried with `->>` | yes | **yes** |
| **`json_group_array`, `json_group_object`** | yes | **no** — *the physical pass does not handle the aggregate JsonGroupArray yet* |
| `jsonb_extract` over a `jsonb()` blob | `2` | **silent** — returns the raw jsonb bytes (`^S2`) |

### Table-valued functions — 4 of 5

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `generate_series(a,b)`, `generate_series(a)` bounded by a `LIMIT`, with a step | yes | **yes** |
| `pragma_table_info`, `pragma_index_list`, `pragma_index_info` as tables | yes | **yes** |
| **`json_each(t.d)` joined against a table** | yes | **no** — *the tree read for FROM term 0 does not carry column 1* |

### The function register

`compat/api/builtins.toml` lists **127 function names**. The pinned SQLite build's own
`pragma_function_list` has 218, but most of the difference is the CLI's bundled extensions
(`sha3`, `base64`, `zipfile`, `readfile`, `sqlar`, `decimal`, `ieee754`, …) rather than the library.
Against the **130 core library functions** the pinned build actually exposes, inillucent is missing
these — and only these:

```
current_date  current_time  current_timestamp   (work as keywords, not as function names)
if  unknown                                     (undocumented/test)
json_array_insert  jsonb_array_insert
load_extension
median  percentile  percentile_cont  percentile_disc
sqlite_compileoption_get  sqlite_compileoption_used  sqlite_log  sqlite_offset
subtype  unistr  unistr_quote
```

and it adds three of its own: `vector_distance_cos`, `vector_distance_l2`, `vector_dot`.

---

## PRAGMA — 21 of 67

This is the widest gap in the document, and its shape is what makes it serious. **No pragma is
refused.** Of the 67 pragmas SQLite lists, 21 answer, 8 answer nothing in both engines because they
are setters, and **38 are accepted and answer nothing at all** — no value and no error. A caller
cannot tell that from an empty result.

| | pragmas |
|---|---|
| **answered by both** (21) | `busy_timeout` `cache_size` `database_list` `defer_foreign_keys` `encoding` `foreign_keys` `freelist_count` `index_info` `index_list` `index_xinfo` `integrity_check` `journal_mode` `locking_mode` `page_count` `page_size` `quick_check` `synchronous` `table_info` `table_list` `table_xinfo` `wal_checkpoint` |
| **SQLite answers, inillucent is silent** (38) | `analysis_limit` `application_id` `auto_vacuum` `automatic_index` `cache_spill` `cell_size_check` `checkpoint_fullfsync` `collation_list` `compile_options` `count_changes` `data_version` `default_cache_size` `empty_result_callbacks` `full_column_names` `fullfsync` `function_list` `hard_heap_limit` `ignore_check_constraints` `journal_size_limit` `legacy_alter_table` `max_page_count` `mmap_size` `module_list` `pragma_list` `query_only` `read_uncommitted` `recursive_triggers` `reverse_unordered_selects` `schema_version` `secure_delete` `short_column_names` `soft_heap_limit` `temp_store` `threads` `trusted_schema` `user_version` `wal_autocheckpoint` `writable_schema` |
| **silent in both** (8) | `case_sensitive_like` `data_store_directory` `foreign_key_check` `foreign_key_list` `incremental_vacuum` `optimize` `shrink_memory` `temp_store_directory` |

Four of these are worth naming individually:

| pragma | SQLite 3.53.4 | inillucent |
|---|---|---|
| **`user_version`** | reads and writes the header word | **silent** — a migration framework reading it gets no row |
| `application_id`, `schema_version`, `data_version` | the same | **silent** |
| **`freelist_count`** | 0 on a fresh 2-page database | **differs** — answers **261,883** on a fresh 5-page database, and does not move after a `DELETE` |
| **`case_sensitive_like = ON`** | `'ABC' LIKE 'a%'` → 0 | **silent** — still 1 |
| `cache_size = -4000` | honoured; reads back `-4000` | **differs** — ignored; reads back `-131072` |
| `journal_mode` | `delete` by default, settable to five modes | **no** — `wal` always; `DELETE`, `MEMORY` and the rest are refused by name (a documented design choice) |
| `table_xinfo` | seven columns, the last saying a column is generated | **differs** — six columns; a generated column is indistinguishable |
| `index_xinfo` | six columns and the trailing rowid entry | **differs** — three columns, no collation, direction or key flag |
| `table_info` on a **view** | describes the view's columns | **silent** — answers nothing |
| `table_list` | includes `sqlite_schema` and `sqlite_temp_schema` | **differs** — user tables only |
| `page_size` | 4096 | **differs** by design — 32768 |

`pragma_pragma_list`, `pragma_function_list`, `pragma_module_list` and `pragma_compile_options` do
not exist as table-valued functions either, so a tool cannot ask this engine what it supports.

---

## EXPLAIN, transactions and multi-file work

### EXPLAIN — 0 of 5, and four of them are shape rather than substance

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `EXPLAIN QUERY PLAN`, scan / index / join / sort | a tree: `QUERY PLAN` then `` `--SCAN t `` | **differs** — the raw four-column form, `0\|0\|0\|SCAN t` |
| the search term in an index plan | `SEARCH t USING COVERING INDEX ia (a=?)` | **differs** — `(?=?)`, the column is not named |
| plain `EXPLAIN` | lists the bytecode | **no** — refused on purpose; there is no bytecode |

The plan *content* agrees — the same scans, the same index choices, the same temp b-tree. What
differs is that SQLite's shell renders the four columns as a tree and this one prints them.

### Transactions — 11 of 11

`BEGIN`/`COMMIT`/`ROLLBACK`/`END`, `DEFERRED`/`IMMEDIATE`/`EXCLUSIVE`, `SAVEPOINT`/`RELEASE`/
`ROLLBACK TO` including nested, DDL rolled back, `DROP TABLE` rolled back, a statement that fails
part way leaving nothing behind, `COMMIT` with no transaction, a nested `BEGIN` — all agree.

### ATTACH and temporary objects — 11 of 11

`ATTACH`/`DETACH`, a query and a join across two files, a transaction and a rollback spanning two
files, `ATTACH ':memory:'`, `PRAGMA database_list`; `CREATE TEMP TABLE`/`VIEW`/`TRIGGER`,
`CREATE TEMP TABLE ... AS SELECT`, `sqlite_temp_schema`, a temp table shadowing a main one — all
agree. The attached-database cap is 10 in both.

---

## Extensions

### FTS5 — 8 of 14

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `CREATE VIRTUAL TABLE ... USING fts5`, `MATCH`, multiple columns, column filters | yes | **yes** |
| phrase queries, `NEAR`, `AND`/`OR`/`NOT`, prefix `qui*` | yes | **yes** |
| `bm25()` ranking, `ORDER BY rank`, `'optimize'` and `'rebuild'` commands | yes | **yes** |
| `DELETE FROM` an fts5 table | yes | **yes** |
| contentless tables (`content=''`) | yes | **yes** |
| **`highlight()` and `snippet()`** | yes | **no** — `no such function` |
| **`UPDATE` on an fts5 table** | yes | **no** — `no layout imported for the table being written` |
| **external content tables** (`content='c'`) | yes | **differs** — `rebuild` leaves the index empty |
| **`tokenize='porter unicode61'`** | `run` matches `running` | **silent** — accepted and ignored |
| `fts5vocab` | yes | **no** — `no such module` |
| FTS3/FTS4 | yes | **no** — `no such module: fts4` |

### R-Tree — 2 of 3

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `rtree`, `rtree_i32`, a window query | yes | **yes** |
| auxiliary columns (`+label`) | yes | **no** — *an rtree table needs an odd number of columns between 3 and 11* |
| `rtreecheck`, `rtreedepth`, `rtreenode` | yes | **no** |

### The other modules — 2 of 6

| module | SQLite 3.53.4 | inillucent |
|---|---|---|
| `json_each`, `json_tree`, `generate_series`, `pragma_*` | yes | **yes** |
| `dbstat` | yes | **no** |
| `sqlite_dbpage` | yes | **no** |
| `geopoly` (and its 14 functions) | yes | **no** |
| `sqlite_offset()` | yes | **no** |
| the session extension / changesets | yes | **no** on the new engine (`inillucent-session` exists over the old one) |
| `csv`, `fsdir`, `zipfile`, `sqlar`, `completion`, `bytecode`, `sqlite_stmt`, `tables_used` | yes | **no** |
| loadable extensions (`load_extension`, `sqlite3_load_extension`) | yes | **no** |
| **vector search in SQL** | no — pgvector is a PostgreSQL extension | **extra** |

### VACUUM

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `VACUUM` | rebuilds the file | **no** — refused by name |
| `VACUUM INTO 'copy.db'` | writes a compacted copy | **no** — refused by name |
| `PRAGMA auto_vacuum`, `incremental_vacuum` | yes | **silent** |

---

## Syntax, limits and the shell

### Syntax — 7 of 9

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| keyword case insensitivity, all four identifier quotings, reserved words as columns | yes | **yes** |
| embedded quotes, newlines in literals, deep nesting, a missing trailing semicolon | yes | **yes** |
| a 200-character identifier, a 5,000-character literal | yes | **yes** |
| **a `-- comment` after the last statement** | ignored | **no** — `-- trailing binds to nothing, which the new engine does not run yet` |
| a bare double-quoted string falling back to a literal | refused with a hint | **refused**, hint omitted |

The trailing comment is small and it is the shape a `.sql` migration file ends in.

### Limits

| limit | SQLite 3.53.4 | inillucent |
|---|---|---|
| columns per table | 2000 | **1000 probed and accepted** |
| terms in a compound select | 500 | **100 probed and accepted** |
| expression nesting | 1000 | **100 probed and accepted** |
| attached databases | 10 | **10** |
| a 40-term join, a 500-term `IN` list, a 2 MB text value | yes | **yes** |
| an unbounded recursive CTE stopped by an outer `LIMIT` | streams and stops at the limit | **no** — `a recursive CTE did not settle; it produced rows for a million passes` |
| `.limit` reporting the limits | yes | **no** — no such dot command |

### The shell — 18 of 39 dot commands

`inillucent-shell` is `sqlite3`-shaped and takes the same command-line flags (`-csv`, `-json`,
`-header`, `-cmd`, …).

| command | SQLite 3.53.4 | inillucent |
|---|---|---|
| `.schema`, `.schema T`, `.fullschema`, `.dump`, `.import`, `.read`, `.output`, `.once` | yes | **yes** |
| `.headers`, `.mode` (`json`, `line`, `column`, `insert`, `quote`, `markdown`, `box`, `table`, `html`), `.nullvalue`, `.width` | yes | **yes** |
| `.backup`, `.open`, `.echo`, `.bail` | yes | **yes** |
| `.tables`, `.databases`, `.changes`, `.eqp`, `.indexes` | yes | **differs** — column padding, the `r/w` suffix, `total_changes:`, the plan tree, a duplicated row |
| `.mode csv` line endings | `\r\n` | **differs** — `\n` |
| `.help TOPIC` | per-command usage | **differs** — always the full list |
| `.save`, `.clone`, `.parameter`, `.sha3sum`, `.lint`, `.limit`, `.vfslist`, `.stats`, `.timeout`, `.recover`, `.selftest`, `.log` | yes | **no** — `unknown command` |
| **named parameters** (`.parameter set :x 5` then `SELECT :x`) | yes | **no** — the dot command is missing, so parameters cannot be exercised from the shell |

---

## Architecture and operations

These cannot be probed with SQL. They are read from the tree and from the design documents, and each
one is a deliberate decision rather than an omission.

| | SQLite 3.53.4 | inillucent |
|---|---|---|
| file format | the SQLite format, readable by every tool | its own `.rdb` plus `RDBWAL01` log segments. **A SQLite file cannot be opened** — `database disk image is malformed` — it is imported |
| import from SQLite | — | `inillucent-migrate --sqlite-file src.db dest.rdb`, copy → verify by count and digest → publish by rename. **See the defects below** |
| export to SQLite | — | `.dump` produces SQL a `sqlite3` can replay |
| processes per file | many, byte-range locks | **one**; a second process is a design non-goal of task-1816 |
| writers | one at a time; readers block in rollback mode, not in WAL | **one at a time; readers never block** (snapshot isolation) |
| threading | single-thread, multi-thread and serialised modes | **single-threaded** |
| journal modes | `DELETE`, `TRUNCATE`, `PERSIST`, `MEMORY`, `WAL`, `OFF` | **WAL only** |
| durability | rollback journal or WAL, `synchronous` OFF/NORMAL/FULL | redo WAL with group commit, crc32c per record, fuzzy checkpoints that **retire** segments, ARIES-style recovery, undo for `ROLLBACK`/`SAVEPOINT`, `synchronous` OFF/NORMAL/FULL |
| isolation | serialisable, one writer | snapshot isolation with a version log and garbage collection |
| page size | 512–65536, 4096 default | 8–64 KiB, **32 KiB default** |
| C API | `sqlite3.h`, ~290 functions | **`inillucent_driver.h`, 53 symbols**, per-symbol stability in `drivers/abi.toml`, plus a capability table of 24 rows a caller can ask before composing a statement (`cancel` is the one `No`) |
| the legacy `sqlite3_*` ABI | — | `inillucent-capi` exports 133 `sqlite3_*` symbols over the **old** engine only |
| language bindings | dozens, everywhere | **Python**, in the standard library only, as the reference binding |
| backup API | `sqlite3_backup_*` | `inillucent_backup_to` in the driver, `.backup` in the shell |
| serialize / deserialize, incremental blob I/O, authorizer, update/commit/rollback/preupdate hooks, progress handler, tracing, `unlock_notify`, snapshots, custom VFS | yes | **not on the new engine** |
| encryption at rest | SEE, a commercial add-on | none |
| user-defined functions and collations | yes | **yes**, scalar and aggregate, through the driver |
| assurance | TH3, `testfixture`, ~600 tests per line of code | **188 test binaries green and 4 red (18 tests)** at this commit, all four accounted for and pre-existing (task-1856); a differential oracle against the pinned build; SQLLogicTest; a `BTreeMap` model reference; a fault-injecting VFS; 8 fuzz targets; 23 of 29 crates deny `unwrap`/`panic`/indexing and 22 of 29 forbid `unsafe` |

### The migration path has three defects

The only supported route from an existing SQLite application is `inillucent-migrate --sqlite-file`,
and it works for tables, `WITHOUT ROWID` tables, generated columns, partial indexes and foreign
keys — verified by count and by digest. Three things stop it being usable on a real database:

| what | what happens |
|---|---|
| **any trigger in the source** | the whole migration is refused: *"the new engine does not run triggers, so these cannot be carried"*. That claim has been **untrue since task-1838** — triggers ship, and every trigger case in this document agrees with SQLite |
| **any FTS5 table in the source** | refused with *"the declaration of `f_data` did not parse: database disk image is malformed"*, which is neither accurate nor actionable |
| **a view in the source** | migrates, appears in `sqlite_schema` as `view\|v`, and then `SELECT * FROM v` answers `no such table: v` |
| `sqlite_sequence` | not carried, so `AUTOINCREMENT` state is lost |

---

## Vector search, against PostgreSQL + pgvector

### Ranking quality — better on 15 of 17, worse on none

Graded by `inillucent-bench` against a correctly configured pgvector
(`hnsw.iterative_scan = relaxed_order`, `ef_search 400`, `max_scan_tuples 40000`,
`scan_mem_multiplier 4`), reading byte-identical vectors, over 2,613 queries in nine families on a
185,078-chunk corpus this repository builds from public data. Each comparison is decided by a 95%
paired bootstrap interval and a paired randomisation test against a threshold declared before the
run. From `inillucent-scorecard.md`; not re-run by this review.

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

In production (task-1774, task-1775): Nikaya moved a 598,560-chunk mailbox off pgvector. Semantic p50
80.6 ms cold / 33.7 ms warm → **4.41 ms**; the lexical branch went from zero rows on 17 of 30
natural-language questions to zero on none; recall@100 against an exact scan 0.899 → **1.000**, at
**3.83 GB resident** against pgvector's 3,167 MB of index in a 5,849 MB database.

### The SQL surface — 5 features, against pgvector's much wider one

| pgvector | inillucent |
|---|---|
| `vector`, `halfvec`, `bit`, `sparsevec` types | **`VECTOR(N)` only.** `HALFVEC(4)` and `BIT(8)` are *accepted* — as ordinary declared type names with text affinity and no vector meaning at all |
| operators `<->` `<#>` `<=>` `<+>` `<~>` `<%>` | **none.** `v <=> ?` is a syntax error |
| `l2_distance`, `inner_product`, `cosine_distance`, `l1_distance`, `hamming_distance`, `jaccard_distance` | **`vector_distance_cos`, `vector_distance_l2`, `vector_dot`** — three of six |
| `vector_dims`, `vector_norm`, `l2_normalize`, `binary_quantize`, `subvector` | **none** |
| vector arithmetic `+ - *`, `avg(vector)`, `sum(vector)` | **none** — and `v + v` and `avg(v)` are *accepted*, coercing the blob to `0.0` |
| HNSW and IVFFlat index types | **HNSW only**, `CREATE INDEX ... USING inillucent_hnsw (v)` |
| `WITH (m = …, ef_construction = …)`, `SET hnsw.ef_search` | **none** — the index takes no parameters and there is no runtime knob (`PRAGMA hnsw_ef_search` is silently accepted and does nothing) |
| an ordering on any distance planned onto the index | **cosine only.** `ORDER BY vector_distance_l2(v, ?) LIMIT k` plans as `SCAN` + temp b-tree |
| a mismatched dimension raises | **silent** — `vector_distance_cos` over a 3-wide query against a 4-wide column answers **NULL** |
| **filtered search** (`WHERE ... ORDER BY v <=> ? LIMIT k`), with `hnsw.iterative_scan` to keep recall | **broken.** See below |
| embedding generation | neither has it in SQL; inillucent has an in-process embedder in the **retrieval engine** (`nomic-embed-text-v1.5` through ONNX Runtime, CPU or GPU) reachable from the API, not from SQL |
| ACID, replication, backups, many writers, many processes | PostgreSQL's | single process, one writer |

### The defect: a filtered vector search loses nine rows in ten

Measured on 400 unit vectors of 16 dimensions with a predicate selecting 5% of them:

```sql
CREATE INDEX ie ON e USING inillucent_hnsw (v);
SELECT id FROM e WHERE src = 'a' ORDER BY vector_distance_cos(v, ?) LIMIT 10;
```

| plan | rows returned |
|---|---|
| exhaustive (no index) | `280,260,400,320,40,340,140,380,160,80` — **10** |
| indexed | `280` — **1** |

`EXPLAIN QUERY PLAN` says `SEARCH e USING VECTOR INDEX ie (k=2)`: the index is probed for *k*
neighbours and the predicate is applied to what comes back, so every neighbour that fails the filter
is a row the query simply loses. Recall@10 is **0.1**, and nothing reports it.

This is the exact failure pgvector's iterative scan exists to fix, and it is a capability
**this project already has**: the retrieval engine's own HNSW honours a predicate *inside* the walk
and chooses an exhaustive plan by cost model when the filter is narrow, which is why the graded
Filtered family above reads recall **1.000** against pgvector's 0.328. The SQL path does not reach
any of that. The goal row "embedding search like pgvector" is met by the engine and not by the SQL
in front of it.

---

## Performance

Not re-measured by this review — nothing in the tree changed after task-1856, which measured it at
this commit. `inillucent-fullgate`, medium scale (100,000 rows), 30 paired rounds, four consecutive
runs, every workload's answer digested and compared with SQLite's before a timing counts.

| | run 1 | run 2 | run 3 | run 4 |
|---|---|---|---|---|
| weighted lower bound (bar 3.00x) | **3.79x** | **3.94x** | **3.96x** | **3.93x** |
| the 1.00x floor | above | above | above | above |
| `extension` low bound | 1.05x | 1.07x | 1.07x | 1.09x |

Per family at medium: `read.point` ~21x, `read.analytical` ~5.7x, `read.range` ~3.8x, `read.join`
~3.9x, `large.values` ~10x, `write` ~1.9x, `transaction` ~1.4x, `open.prepare` ~1.05x, `schema`
~1.1x, `extension` ~1.07x. Three families are above the floor but under their own bars —
`open.prepare` (bar 5.00x), `schema` (3.00x) and `extension` (1.50x) — which is why the gate still
prints `NOT MET` overall.

Footprint, both arms as whole child processes: medium **79.83 MiB** peak against SQLite's 37.20 MiB
(2.15x), 0.79x the user CPU and 0.15x the kernel CPU; large 418.88 MiB against 179.68 MiB (2.33x).
On disk, 200,000 rows are 15.9 MB against SQLite's 8.7 MB (1.83x). On Linux the same binary was
1.53x weighted at medium (task-1838 §5), and the cause is measured rather than argued: SQLite does
per-statement operating-system work that Windows charges heavily for, so its denominator moves across
platforms and this engine's does not.

---

## What is silently different

The fourteen cases where both engines answer, the answers differ, and **nothing tells the caller**.
This is the list that matters, because every other row in this document is something an application
can see and work around.

| # | construct | SQLite 3.53.4 | inillucent |
|---|---|---|---|
| 1 | `WHERE ... ORDER BY vector_distance_cos(v, ?) LIMIT k` over an HNSW index | every matching row | **a tenth of them** |
| 2 | `INSERT ... ON CONFLICT DO UPDATE SET a = ... WHERE t.a > 500` | the arm is skipped | **the arm runs** |
| 3 | `PRAGMA recursive_triggers=ON` and a self-inserting trigger | recurses | **fires once** |
| 4 | `PRAGMA case_sensitive_like=ON`, then `'ABC' LIKE 'a%'` | 0 | **1** |
| 5 | `1 IS DISTINCT FROM NULL` | 1 | **0** |
| 6 | the literal `-9223372036854775808` | integer | **real** |
| 7 | `sum()` overflowing an integer | `integer overflow` | **a real** |
| 8 | `0.0/0.0` | NULL | **NaN** |
| 9 | `jsonb_extract(jsonb('{"a":2}'), '$.a')` | `2` | **raw jsonb bytes** |
| 10 | `strftime('%g'/'%k'/'%l', ...)` | `24`, ` 9`, ` 9` | **the literal text `%g`, `%k`, `%l`** |
| 11 | `strftime('%s', ...)` | `1709283907` | **`1709283906`** |
| 12 | the `subsec` modifier | `...12:00:00.000` | **no fractional part** |
| 13 | `fts5(..., tokenize='porter unicode61')` | `run` matches `running` | **accepted and ignored** |
| 14 | `PRAGMA freelist_count` on a fresh database | 0 | **261,883** |

And the two things it accepts that SQLite refuses: a `CHECK` containing a subquery, and an integer
`sum()` that overflows.

The **38 silent pragmas** belong on this list too, as one entry rather than 38: a statement that is
accepted and answers nothing is indistinguishable from one that answered nothing legitimately.

---

## What is missing, in the order it should be closed

Ranked by what stops an application that runs on SQLite today from running on this instead.

1. **The filtered vector search.** A silent tenth-of-the-rows answer on the query the second goal
   exists for, and the capability to fix it is already in the retrieval engine.
2. **`USING` and `NATURAL` joins, and a self join.** Four join spellings and one join shape, all on
   one message.
3. **The pragma surface.** `user_version` above all, then `application_id`, `schema_version`,
   `collation_list`, `table_info` on a view, the missing columns of `table_xinfo` and `index_xinfo`,
   and `freelist_count`'s wrong number. A pragma this engine does not implement should **refuse**,
   not answer nothing.
4. **The migration path.** Triggers refused on a claim that has been false for four tickets; FTS5
   refused as "malformed"; a view that migrates and cannot be read; `sqlite_sequence` dropped.
5. **The remaining silent differences**, rows 2–14 of the table above.
6. **The refusals an ordinary query hits**: a bare column beside an aggregate, `DISTINCT` with an
   `ORDER BY` outside the select list, `FILTER` on a plain aggregate, `group_concat(... ORDER BY ...)`,
   `json_group_array`/`json_group_object`, `json_each` joined against a table, a row value against a
   subquery, two `ON CONFLICT` clauses, a trailing `--` comment, and a recursive CTE bounded by an
   outer `LIMIT`.
7. **FTS5's remainder**: `highlight()`, `snippet()`, `UPDATE`, external content tables, `fts5vocab`,
   and the `porter` tokenizer actually stemming.
8. **`VACUUM` and `VACUUM INTO`.**
9. **The shell's missing dot commands**, `.parameter` first — without it a parameter cannot be
   exercised from the shell at all.
10. **The deliberate non-goals**, listed here so they are not mistaken for oversights: SQLite's file
    format, the `sqlite3_*` C ABI on the new engine, more than one process on a file, more than one
    writer, more than one thread, journal modes other than WAL, plain `EXPLAIN`, and loadable
    extensions.

Everything in 1 through 9 is **task-1859**, which updates this document as it closes each row.

---

## Reproducing this

The probe is checked in as [`tools/feature-probe/`](tools/feature-probe/), so this document can be
regenerated from a clone rather than trusted.

```sh
cargo build --release --bin inillucent-shell   # the engine under test
tools/sqlite-reference.sh                      # the pinned reference, if .sqlite-ref is absent

node tools/feature-probe/run.js                # 416 cases through both shells, one fresh database each
node tools/feature-probe/summarise.js          # the per-area table
node tools/feature-probe/pragmas.js            # every PRAGMA the reference lists, asked of both
node tools/feature-probe/vector-features.js    # the vector surface, one pgvector feature at a time
```

Everything it writes lands in `_agent_output/feature-probe/`, gitignored. `results.json` carries the
script, both transcripts and the verdict for every case, which is what to read when a row moves.
`tools/feature-probe/README.md` says how to add a case and the two rules that keep one meaningful.

The 110 cases the engine's own suite carries are `crates/inillucent-compat/tests/semantics.rs`, which
runs in CI and fails when a construct changes its mind in either direction. This probe is wider and
is not yet a test; making its findings into checked-in cases is part of task-1859.

---

## Where the rest of the documentation is

| | |
|---|---|
| `README.md` | what the project is, the measured comparison with SQLite, and the retrieval engine |
| `tasks/rust-db-phase-2-tdd.md` | the improvement plan, Phase 2 and Phase 3 |
| `tasks/task-1816-rearchitecture-tdd.md` | the design the new engine follows |
| `drivers/README.md` | the driver, for somebody writing a binding |
| `inillucent-scorecard.md` | the graded comparison with pgvector |
| `product-overview.md`, `architecture.md` | the retrieval engine |
| `compat/README.md` | the parity manifest and the harness that fills it |
