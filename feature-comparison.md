# Feature comparison

**inillucent against SQLite 3.53.4, and its retrieval engine against PostgreSQL + pgvector.**

The project has two goals and this document is the scorecard for both:

1. a **highly performant SQLite replacement offering the same features**, and
2. an **embedding solution that matches pgvector**.

Written for task-1858 on **2026-09-07** at commit `382eb78` and **re-measured for task-1859**, which
closed most of what the first run found. Every row below is a measurement, not a reading of the
source: each feature is a whole SQL script run through `inillucent-shell` and through the pinned
`sqlite3` 3.53.4, over its own fresh database, with every byte of both streams compared. That is the
discipline `crates/inillucent-compat/tests/semantics.rs` applies - to 164 constructs now - widened
here to **416 cases across the whole feature surface**. The harness is checked in as
`tools/feature-probe/` and its transcripts are under `_agent_output/`;
[Reproducing this](#reproducing-this) says how to run it.

**Every count and every row below is from the re-run**, not from the code. A document that says a
feature works because somebody implemented it is the thing the probe exists to replace.

---

## The headline

| | | at `382eb78` |
|---|---|---|
| **416 probed features** | **365 agree with SQLite byte for byte** | was 302 |
| features both refuse, with different wording | **11** — counted as agreement | was 10 |
| features SQLite answers and inillucent refuses | **25** | was 45 |
| features both answer, **differently** | **10** — and **none of them is silent** | was 52, of which 14 were silent |
| features inillucent accepts that SQLite rejects | **0** | was 2 |
| vector features with no SQLite equivalent | **5**, all working | unchanged |

**The five goals, measured:**

| goal | state |
|---|---|
| Same SQL as SQLite | **Essentially, yes.** Every case in `select`, `join`, `compound`, `cte`, `subquery`, `window`, `txn`, `attach`, `temp`, `schema`, `constraint`, `ddl-index`, `ddl-view`, `ddl-trigger`, `types`, `fn-agg`, `fn-json`, `fn-math`, `fn-time` and `integrity` agrees. What is left is one lateral join (`FROM t, json_each(t.d)`), a second `ON CONFLICT` clause, and `REGEXP` — which is the reference *shell*'s function rather than SQLite's. |
| Same observable semantics | **No silent difference is left.** All seventeen the first run found are closed; every remaining difference either refuses by name or is an engine identity a caller can read (`PRAGMA page_size`, `journal_mode`, `locking_mode`). |
| The PRAGMA surface an application uses | **59 of the 67 pragmas SQLite lists answer, 8 are silent in both by design, and none is silent here.** `user_version`, `application_id` and `schema_version` live in the meta page; `query_only`, `recursive_triggers`, `case_sensitive_like`, `max_page_count` and `cache_size` are honoured. |
| Embedding search like pgvector | **The ranking is better and the SQL surface is thinner** — 15 of 17 primary comparisons better than pgvector with none worse — and the filtered search now returns **every** row the exhaustive plan returns: recall 1.000 at 400 and 20,000 rows, at every filter and every `LIMIT`. See [Vector search](#vector-search-against-postgresql--pgvector). |
| Faster than SQLite | **Yes, and re-measured after this work.** One 30-round medium gate at task-1859's commit: weighted geomean **3.94x**, lower bound **3.84x** against a 3.00x bar, every required family above the 1.00x floor, 30 of 30 workloads digest-equal. That sits inside the **3.79x–3.96x** band task-1856 measured over four runs at `382eb78`, so this ticket's work cost nothing measurable. |

**What is left, and why:** [What is missing, in the order it should be closed](#what-is-missing-in-the-order-it-should-be-closed).

---

## How to read the tables

Each table is one feature per row, side by side. SQLite 3.53.4 is the reference, so its column says
what it does; inillucent's column says whether it does the same.

| symbol | meaning |
|---|---|
| **yes** | byte-for-byte the same answer, including the error message when both refuse |
| **differs** | both answer, and the answers are not the same |
| **silent** | both answer, the answers are not the same, and **nothing tells the caller** — the worst outcome, because an application cannot see it. **No row in this document says this any more**; the word is kept so the seventeen that used to say it can be read against what they say now |
| **no** | SQLite answers and inillucent refuses, by name |
| **extra** | inillucent has it and SQLite does not |

A **deliberate gap** is a row that says **no** or **differs** on purpose, with the reason given in
the row. It is not a backlog item, and the difference between the two is the whole point of naming
it.

---

## SQL statements and clauses

### SELECT — 19 of 19

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
| a bare column beside an aggregate (`SELECT id, max(a) FROM t`) | picks the row `max` came from | **yes** — the group carries a witness alongside the aggregate and the bare column is read out of it, so the `id` is the one that `max` actually came from rather than the first or the last |
| `SELECT DISTINCT a ... ORDER BY b` | yes | **yes** — the sort key is carried through the distinct pass and dropped from the result |

### Joins — 15 of 15

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `JOIN ... ON`, `LEFT`, `RIGHT`, `FULL`, `CROSS` | yes | **yes** |
| comma join, four-table join, join onto a subquery | yes | **yes** |
| `LEFT JOIN` with a `WHERE` on the right table | yes | **yes** |
| `JOIN ... USING (k)` | yes | **yes** |
| `LEFT JOIN ... USING (k)` | yes | **yes** |
| `NATURAL JOIN`, `NATURAL LEFT JOIN` | yes | **yes** |
| `USING` across three tables | yes | **yes** |
| a self join (`FROM t x JOIN t y ON y.a = x.a AND y.id > x.id`) | yes | **yes** |

**This was the largest single gap and it was one line.** Four of the five spellings failed with
`ambiguous column name: k`, which did suggest one cause rather than five: a `USING` or `NATURAL`
join *coalesces* the named column, and the right-hand copy was suppressed from `*` but not from an
unqualified reference, so `ORDER BY k` found two candidates. A qualified `b.k` still reaches the
right-hand copy - NULL in a `LEFT JOIN`, where the coalesced column carries the left value.

The self join was the planner choosing a rowid *range* for an inner term, which the physical pass
then refused by name. A range is an outermost-term path; the bound is now left as a residual and
tested over the pair.

### Compound selects, subqueries and CTEs — 27 of 27

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `UNION`, `UNION ALL`, `EXCEPT`, `INTERSECT`, three-way, with `LIMIT` | yes | **yes** |
| scalar subquery, `IN`, `NOT IN` with NULLs, `EXISTS`, `NOT EXISTS` | yes | **yes** |
| correlated subqueries, including with their own `LIMIT` | yes | **yes** |
| derived table in `FROM` | yes | **yes** |
| row values: `(a,b) = (1,2)` and `(a,b) IN (VALUES ...)` | yes | **yes** |
| row value against a subquery: `(a,b) = (SELECT a,b FROM t ...)` | yes | **yes** — the query is bound once and read column by column |
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

### UPSERT — 6 of 7

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `ON CONFLICT DO NOTHING`, with and without a target | yes | **yes** |
| `ON CONFLICT DO UPDATE` with `excluded`, on a secondary unique index | yes | **yes** |
| `ON CONFLICT DO UPDATE ... RETURNING` | yes | **yes** |
| **`ON CONFLICT DO UPDATE ... WHERE`** | the arm runs only where the predicate holds | **yes** — the arm's `WHERE` is tested against the row already there, and a row it does not keep is left alone |
| two `ON CONFLICT` clauses on one statement | yes | **no** — `unsupported: more than one ON CONFLICT clause`. A **deliberate gap**: the conflict a write hits does not carry *which* constraint reported it, so there is nothing to match a second clause's target against. Making it work means the conflict carrying its constraint's columns and the insert plan carrying one arm per clause |

---

## Schema

### CREATE TABLE — 16 of 18

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| typed and typeless columns, `IF NOT EXISTS`, `DROP TABLE ... IF EXISTS` | yes | **yes** |
| `WITHOUT ROWID`, composite `PRIMARY KEY` | yes | **yes** |
| `STRICT`, and `ANY` inside a `STRICT` table | yes | **yes** |
| generated columns, `VIRTUAL` and `STORED` | yes | **yes** |
| `DEFAULT` literals and expressions, `CURRENT_TIMESTAMP`/`DATE`/`TIME` | yes | **yes** |
| quoted identifiers `"x"`, `[x]`, `` `x` ``, reserved words as column names | yes | **yes** |
| `CREATE TABLE ... AS SELECT` | stores the **affinity's** name — `a INT`, `b TEXT` — in its own layout | **yes** — byte for byte, including the rule that a declaration under fifty characters goes on one line |
| `INTEGER PRIMARY KEY DESC` | **not** a rowid alias; a real index is built | **yes** |
| `CHECK` containing a subquery | `subqueries prohibited in CHECK constraints` | **refuses**, in the same words. The reference's *shell* then draws a caret under the offending token in a spelling this one does not use, which is why the probe records the pair as refusing differently |
| `WITHOUT ROWID` with no primary key | refused | **refused**, wording differs |

### CREATE INDEX — 12 of 12

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| plain, `UNIQUE`, composite, `COLLATE`, `DESC` | yes | **yes** |
| **partial** (`... WHERE`), on an **expression**, on a **`WITHOUT ROWID`** table | yes | **yes** |
| `DROP INDEX`, `REINDEX`, `REINDEX name` | yes | **yes** |
| `INDEXED BY`, `NOT INDEXED` | yes | **yes** |
| `ANALYZE` writing `sqlite_stat1` | yes | **yes** |

### Views and triggers — 15 of 15

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `CREATE VIEW`, with a column list, over a join, `DROP VIEW` | yes | **yes** |
| writing through an `INSTEAD OF` trigger | yes | **yes** |
| `BEFORE`/`AFTER` `INSERT`/`UPDATE`/`DELETE`, `OLD`/`NEW`, `WHEN`, `UPDATE OF` | yes | **yes** |
| `RAISE(ABORT)`, `RAISE(IGNORE)`, a trigger writing another table, `DROP TRIGGER` | yes | **yes** |
| **`PRAGMA recursive_triggers=ON`** then a self-inserting trigger | recurses to the cap | **yes**. A trigger body is *inlined* by the binder and a trigger already being bound is skipped, which is what makes the inlining terminate and is SQLite's behaviour with the pragma off; with it on, a body statement that writes the trigger's own table is handed the trigger back, bounded by the depth cap |

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

### Type affinity and storage classes — 19 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| affinity applied on write for `INTEGER`, `TEXT`, `REAL`, `BLOB`, `NUMERIC`, and through a rowid alias | yes | **yes** |
| `CAST` between every class, comparison across classes | yes | **yes** |
| integer division, modulo, division by zero, hex literals, blob literals | yes | **yes** |
| `TRUE`/`FALSE`/`NULL`, unicode round trip, NULL arithmetic | yes | **yes** |
| values wider than a page, text and blob | yes | **yes** |
| the literal `-9223372036854775808` | integer | **yes** — the sign is folded into the literal, as the reference's own parser does |
| `0.0/0.0`, and `1e999-1e999` | NULL | **yes** — SQLite has no NaN, and neither does this now |

### Operators — 11 of 12

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| arithmetic, concatenation, unary, precedence, bitwise | yes | **yes** |
| `BETWEEN`, `IN` list, `NOT IN`, `CASE` both forms | yes | **yes** |
| `LIKE` with `ESCAPE`, `GLOB`, string comparison, `COLLATE` in an expression | yes | **yes** |
| JSON `->` and `->>` | yes | **yes** |
| `IS DISTINCT FROM`, `IS NOT DISTINCT FROM` | 1, 1 | **yes** — the keyword inverts the sense, which it was dropping |
| `REGEXP` | the CLI registers one | **no** — `no such function: regexp`. A **deliberate gap**: `regexp` is a function the reference *shell* registers, not one SQLite has; an application registers its own through the same `create_function` this engine offers |

### Collations — 4 of 5

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `BINARY`, `NOCASE`, `RTRIM` in expressions, columns, `ORDER BY` and unique indexes | yes | **yes** |
| `PRAGMA collation_list` | lists five: `decimal`, `BINARY`, `NOCASE`, `RTRIM`, `uint` | **differs** — lists the three this engine has. `decimal` and `uint` come from the reference CLI's bundled extensions rather than from SQLite; a **deliberate gap**, and an application registers its own collations through `create_collation` |

### Scalar functions — 17 of 19

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `abs`, `sign`, `round`, `max`/`min`, `length`, `substr`, `instr`, `replace` | yes | **yes** |
| `upper`, `lower`, `trim`/`ltrim`/`rtrim` with and without a character set | yes | **yes** |
| `printf`/`format` including `%05.2f` and `%08.3d`, `quote`, `hex`, `unhex`, `char`, `unicode` | yes | **yes** |
| `coalesce`, `ifnull`, `nullif`, `iif`, `typeof`, `likelihood`, `likely`, `unlikely` | yes | **yes** |
| `zeroblob`, `randomblob`, `octet_length`, `concat`, `concat_ws`, `like()`, `glob()` | yes | **yes** |
| `changes()`, `total_changes()`, `last_insert_rowid()` | yes | **yes** |
| blob `instr`/`length`/`substr`, negative `substr` offsets | yes | **yes** |
| `round(1e308, 2)` | `1.0e+308` | **yes** — a number with more magnitude than `digits` can move is already rounded to that many places |
| `length(char(0))`, `length(char(65,0,66))` | 0, 1 | **yes** — the count stops at the first NUL, which is SQLite's documented definition; `hex()` still shows every byte |
| `printf %q`, `%Q`, `%w` | refused | **refused**, wording differs |
| `load_extension` | loads one | **refused** in both — this build has no extension loader and the reference's is compiled out |

### Aggregates — 8 of 8

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `count`, `sum`, `total`, `avg`, `max`, `min`, over NULLs and an empty set | yes | **yes** |
| `group_concat` with a separator, `string_agg`, `DISTINCT` inside an aggregate | yes | **yes** |
| **`FILTER (WHERE ...)` on a plain aggregate** | yes | **yes** — a filtered call is fed a row at a time; the vectorised mini-column paths cannot skip rows, so they are not offered one |
| **`group_concat(b ORDER BY a DESC)`** | yes | **yes** — the rows are collected and sorted before the fold, keyed with the tree's own key encoding |
| `sum()` overflowing an integer | `integer overflow` | **yes** — and `total()` and `avg()` still answer a double over the same values, which is what they are documented to be |

### Date and time — 9 of 9

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `date`, `time`, `datetime`, `julianday`, `unixepoch`, `timediff`, round trips | yes | **yes** |
| modifiers: `±N days/months/years/minutes`, `start of month/year/day`, `weekday N`, `auto` | yes | **yes** |
| `strftime` `%f`, `%J`, and the whole common specifier set | yes | **yes** |
| `strftime` `%g`, `%k`, `%l` | `24`, ` 9`, ` 9` | **yes** |
| `strftime` with a specifier SQLite has not got (`%y`, `%Z`) | NULL | **yes** — the whole call is NULL rather than the two characters echoed back |
| `strftime('%s', ...)`, `unixepoch()` | `1709283907` | **yes**. Both were one second low: a Julian day is a binary fraction, so the value came back as `1709283906.9999998` and flooring it lost a second. It is rounded to a millisecond first, which is the resolution SQLite works in |
| the `subsec` modifier, on `date`, `time` and `datetime` | `2024-03-01 12:00:00.000` | **yes** |

**The whole specifier family was walked**, not the three that were reported: every specifier the
reference lists, plus `julianday`, `unixepoch`, `timediff` and the `auto` modifier, diffed byte for
byte. That is task-1856's own lesson - fixing one member of a family and assuming the rest is how the
next three stay hidden.

### Maths — 4 of 4

Every trigonometric, hyperbolic, logarithmic, power and rounding function agrees:
`sin cos tan asin acos atan atan2 sinh cosh tanh asinh acosh atanh ln log log2 log10 exp pow power
sqrt ceil ceiling floor trunc mod pi degrees radians`.

### JSON — 11 of 11

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `json`, `json_valid` with flags, `json_quote`, `json_array`, `json_object` | yes | **yes** |
| `json_extract`, `json_type`, `json_insert`/`replace`/`set`/`remove`, `json_patch` | yes | **yes** |
| `json_array_length`, `json_pretty`, `json_error_position`, `json_each`, `json_tree` | yes | **yes** |
| JSON stored in a column and queried with `->>` | yes | **yes** |
| **`json_group_array`, `json_group_object`** | yes | **yes**, in both the `json_` and `jsonb_` spellings and under `GROUP BY`. A NULL is a *member* of the document rather than a row to skip |
| `jsonb_extract` over a `jsonb()` blob | `2` | **yes** — a primitive comes back as itself and only a container comes back as JSONB, which is what "just like `json_extract` except the value is JSONB" means |

### Table-valued functions — 4 of 5

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `generate_series(a,b)`, `generate_series(a)` bounded by a `LIMIT`, with a step | yes | **yes** |
| `pragma_table_info`, `pragma_index_list`, `pragma_index_info` as tables | yes | **yes** |
| **`json_each(t.d)` joined against a table** | yes | **no** — a **deliberate gap**, and the largest one left. The argument reads a column of the *outer* row, which makes it a lateral join: the module has to be re-driven once per outer row, and this engine asks a module once and materialises what it answers. That is a new operator rather than a fix, and it deserves its own ticket - the shape is the standard way to expand a JSON array per row |

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

## PRAGMA — 24 of 35 probed cases; 59 of 67 pragmas answer, none silent here

This was the widest gap in the document, and its shape was what made it serious: 21 answered and
**38 were accepted and answered nothing at all** — no value and no error, which a caller cannot tell
from an empty result.

**Nothing on SQLite's list is silent now.** Every name on it either answers or refuses; a name on
*nobody's* list — `PRAGMA nonesuch` — is still silent, because that is what SQLite does with one and
the parity is the point. There are three dispositions, and `crates/inillucent-engine/src/pragma.rs`
names which each pragma has:

- **Honoured** — it does what it says.
- **Reported** — a value this engine has exactly one of. Reading it gives the truth; setting it to
  that value succeeds; setting it to anything else **refuses**, which is the rule
  `journal_mode = DELETE` has always followed.
- **Refused** — the subject does not exist here. There is now nothing in this class.

| | pragmas |
|---|---|
| **answered by both** (59) | everything on SQLite's list except the eight below |
| **silent in both** (8) | `case_sensitive_like` `data_store_directory` `foreign_key_check` `foreign_key_list` `incremental_vacuum` `optimize` `shrink_memory` `temp_store_directory` |
| **SQLite answers, inillucent is silent** | **none** |
| **SQLite answers, inillucent refuses** | **none** |

The ones worth naming individually, all measured against the reference:

| pragma | SQLite 3.53.4 | inillucent |
|---|---|---|
| **`user_version`** | reads and writes the header word | **yes** — it lives in the meta page's reserved region, and a file written before it existed reads zero and still verifies its own checksum |
| `application_id`, `schema_version`, `data_version` | the same | **yes**. The schema cookie moves once per schema change and *only* there — not on an open or an `ATTACH`, which is the one thing an application watching it must be able to rule out |
| **`freelist_count`** | 0 on a fresh database, and it moves | **yes**. It answered **261,883** on a five-page database: it was counting every bit the free map had room for, and one map page over a 4 KiB file describes 32,736 pages whether the file has them or not |
| **`case_sensitive_like = ON`** | `'ABC' LIKE 'a%'` → 0 | **yes**, for the operator and for the `like()` function spelling |
| `cache_size = -4000` | honoured; reads back `-4000` | **yes** — it caps how many pages the pool holds, which is SQLite's own definition of the setting. Asking for more than the pool was opened with reads back what it has |
| `query_only`, `max_page_count`, `recursive_triggers` | honoured | **yes** — `query_only` refuses a write with SQLite's own `attempt to write a readonly database` |
| `table_info` on a **view** | describes the view's columns | **yes** — the view's `SELECT` is bound and its columns answered |
| `table_info` on a table with a generated column | renumbers `cid` over the columns it reports | **yes** |
| `table_xinfo` | seven columns, the last saying a column is generated | **yes** — 2 is `VIRTUAL`, 3 is `STORED`, 1 is a virtual table's hidden argument |
| `index_xinfo` | six columns and the trailing rowid entry | **yes**. The direction needed a new `declared_descending` on the catalog: this engine's trees are always ascending and the planner must be told so (task-1855 measured three inverted conclusions when it was not), but the *declaration* is a different question and is what this pragma reports |
| `table_list` | includes `sqlite_schema` and `sqlite_temp_schema`, and says `view` | **yes** |
| `collation_list`, `pragma_list`, `function_list`, `module_list`, `compile_options` | answer, as directives and as `pragma_*` tables | **yes** — a tool can ask this engine what it supports |
| `journal_mode` | `delete` by default, settable to five modes | **differs by design** — `wal` always; `DELETE`, `MEMORY` and the rest are refused by name |
| `locking_mode` | `normal` | **differs by design** — `exclusive`; one file is one pool here |
| `auto_vacuum`, `secure_delete` | settable | **reported as 0, and a set to anything else refuses.** This engine never auto-vacuums and never zeroes a freed page, so accepting the setting would be the silent lie this whole section is about |
| `ignore_check_constraints = ON` | skips `CHECK` | **refused** — a `CHECK` is always enforced here, and there is no way to ask for less |
| `automatic_index` | 1 | **differs** — 0, because this engine never builds one |
| `cache_spill`, `wal_autocheckpoint` | a threshold | **differ** — 0, because the pool evicts by clock and the log is folded in at an explicit checkpoint rather than every N frames |
| `page_size`, `page_count` | 4096, 2 | **differ by design** — 32768, 5 |

The eleven probed cases that do not agree are all in that last group: an engine identity a caller can
read, or a value this engine is honest about not having. There is no case left where a pragma is
accepted and answers nothing.

---

## EXPLAIN, transactions and multi-file work

### EXPLAIN — 4 of 5, and the fifth is refused on purpose

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `EXPLAIN QUERY PLAN`, scan / index / join / sort | a tree: `QUERY PLAN` then `` `--SCAN t `` | **yes** — the shell draws the same tree from the same four columns, with `|--` for a middle child and `` `-- `` for the last |
| the search term in an index plan | `SEARCH t USING COVERING INDEX ia (a=?)` | **yes** — the key column is named |
| plain `EXPLAIN` | lists the bytecode | **no** — refused by name, on purpose. There is no bytecode to list: this engine compiles to a pipeline of operators, not to a register machine, and inventing opcode rows that no interpreter runs would be a fiction a reader could not use |

The plan *content* already agreed — the same scans, the same index choices, the same temp b-tree.
What differed was that SQLite's shell renders the four columns as a tree; the fix was in the shell,
not the planner, which is what `.eqp` and `EXPLAIN QUERY PLAN` now share.

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

### FTS5 — 10 of 14

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| `CREATE VIRTUAL TABLE ... USING fts5`, `MATCH`, multiple columns, column filters | yes | **yes** |
| phrase queries, `NEAR`, `AND`/`OR`/`NOT`, prefix `qui*` | yes | **yes** |
| `bm25()` ranking, `ORDER BY rank`, `'optimize'` and `'rebuild'` commands | yes | **yes** |
| `DELETE FROM` an fts5 table | yes | **yes** |
| contentless tables (`content=''`) | yes | **yes** |
| **`UPDATE` on an fts5 table** | yes | **yes** — a module row is read back through an ordinary `SELECT` and rewritten, so an `UPDATE` is a delete and an insert to the index and the untouched columns keep their values |
| **`tokenize='porter unicode61'`** | `run` matches `running` | **yes** — the porter stemmer wraps whichever tokenizer follows it, which is what the two-word spelling means; `unicode61` on its own still does no stemming, as SQLite's does not |
| **`highlight()` and `snippet()`** | yes | **no** — a **deliberate gap**. Both need the *offsets* of the matched terms inside the stored column, and this index keeps positions per term rather than byte ranges per row; producing them means re-tokenising the row at query time and mapping token positions back to bytes. Worth its own ticket |
| **external content tables** (`content='c'`) | yes | **refused by name** — `fts5: an external content table (content=) is not supported`. It was silently accepted and answered an empty index, which is exactly the shape this document exists to remove. A module here reaches its own shadow tables and nothing else, so reading rows out of a *user* table is a new capability rather than a fix. `content=''` — a **contentless** table, a different thing — is supported |
| `fts5vocab` | yes | **no** — `no such module` |
| FTS3/FTS4 | yes | **no** — `no such module: fts4`, and this is a **deliberate gap** rather than a backlog item. FTS3/4 are the superseded designs; SQLite keeps them for files written before 2015. A new engine has no such files to read, and the migration path reads an FTS5 index, so there is nothing an FTS3 implementation here would serve |

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
| `VACUUM INTO 'copy.db'` | writes a compacted copy of the database | **yes as a backup, not as a compaction.** It is the form that mattered: `VACUUM INTO` is how a backup is taken from SQL, with no shell and no file copied out from under a writer. Here it checkpoints, writes the copy, then **opens the copy and checks every tree** before returning, so a file it produced is a file something has read. It refuses an existing output file in SQLite's own words. What it does not do is make the copy smaller — see the row below |
| `VACUUM` | rebuilds the file, reclaiming free space | **yes, with one difference stated rather than hidden**: it folds the log into the file and leaves it self-contained, and it does **not** defragment. Free pages here go back to the free map as they are released and are handed out again, so what is left for the statement to do is a checkpoint; `PRAGMA freelist_count` says what is free either way. A logical rebuild of a live database is its own ticket |
| `VACUUM` inside a transaction | `cannot VACUUM from within a transaction` | **yes**, in the same words — and not a formality: a checkpoint folds *committed* frames into the file, and an open transaction's are not committed |
| `PRAGMA auto_vacuum`, `incremental_vacuum` | yes | **reported as 0 / silent** — this engine never auto-vacuums, and `PRAGMA auto_vacuum = FULL` refuses rather than accepting a setting it will not honour |

---

## Syntax, limits and the shell

### Syntax — 8 of 9

| feature | SQLite 3.53.4 | inillucent |
|---|---|---|
| keyword case insensitivity, all four identifier quotings, reserved words as columns | yes | **yes** |
| embedded quotes, newlines in literals, deep nesting, a missing trailing semicolon | yes | **yes** |
| a 200-character identifier, a 5,000-character literal | yes | **yes** |
| **a `-- comment` after the last statement** | ignored | **yes** — a trailing comment is a statement with nothing in it, and a statement with nothing in it does nothing. It is the shape a `.sql` migration file ends in |
| a bare double-quoted string falling back to a literal | refused with a hint | **refused**, hint omitted |

### Limits

| limit | SQLite 3.53.4 | inillucent |
|---|---|---|
| columns per table | 2000 | **1000 probed and accepted** |
| terms in a compound select | 500 | **100 probed and accepted** |
| expression nesting | 1000 | **100 probed and accepted** |
| attached databases | 10 | **10** |
| a 40-term join, a 500-term `IN` list, a 2 MB text value | yes | **yes** |
| an unbounded recursive CTE stopped by an outer `LIMIT` | streams and stops at the limit | **yes** — the limit is pushed into the recursion, so `WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n) SELECT * FROM n LIMIT 5` stops after five passes instead of running to the settle cap |
| `.limit` reporting the limits | yes | **no** — a **deliberate gap**. `.limit` reports `SQLITE_LIMIT_*`, which is the C API's per-connection limit register; this engine's limits are a `Limits` struct with different members, and printing SQLite's names over them would be a made-up mapping |

### The shell — 29 of 39 dot commands

`inillucent-shell` is `sqlite3`-shaped and takes the same command-line flags (`-csv`, `-json`,
`-header`, `-cmd`, …).

| command | SQLite 3.53.4 | inillucent |
|---|---|---|
| `.schema`, `.schema T`, `.fullschema`, `.dump`, `.import`, `.read`, `.output`, `.once` | yes | **yes** |
| `.headers`, `.mode` (`json`, `line`, `column`, `insert`, `quote`, `markdown`, `box`, `table`, `html`), `.nullvalue`, `.width` | yes | **yes** |
| `.backup`, `.restore`, `.open`, `.echo`, `.bail` | yes | **yes** |
| **`.tables`, `.indexes`** | column-major, as many columns as fit the screen | **yes**. The layout was measured rather than guessed: the fewest rows that fit eighty columns, each column padded to its own widest name, five spaces between. `.indexes` matches its pattern as `%p%` against the *index* name while `.tables` matches the table name exactly — a difference in the reference, and now a difference here |
| **`.databases`, `.changes`, `.eqp`** | the ` r/w` suffix, a `total_changes:` line, the plan tree | **yes** |
| **`.save`, `.clone`** | write the database out | **yes**, including the `<name>... done` progress line |
| **`.parameter`**, and named parameters (`.parameter set :x 5` then `SELECT :x`) | yes | **yes** — the shell holds the bindings and hands them to every statement it runs, so a parameter can be exercised without writing a program |
| **`.timeout`, `.log`** | yes | **yes** |
| a trailing `;` on a dot command (`.print hello;`) | dropped | **yes** — including `.separator ;`, which is then a command with no argument at all and answers with its usage line |
| `.mode csv` line endings | CR LF | **differs**, and it is a **platform artefact** rather than a decision. This shell writes the RFC 4180 CR LF that the reference writes; the reference then puts it through a Windows text-mode stdout, which turns each one into CR CR LF. Matching it means writing a malformed line ending on purpose |
| `.help TOPIC` | a page of per-command usage, options and all | **differs** — the full one-line list. A **deliberate gap**: the reference's `.help .mode` is fifty lines of option documentation for one command, and reproducing that text for thirty-nine commands is transcription rather than compatibility |
| `.sha3sum`, `.lint`, `.limit`, `.vfslist`, `.stats`, `.recover`, `.selftest` | yes | **no** — `unknown command`, and all seven are **deliberate gaps**. Each reaches for something this engine does not have: a SHA-3 over the b-tree page images (`.sha3sum`), the `SQLITE_LIMIT_*` register (`.limit`), the VFS registry (`.vfslist`), the bytecode step counters (`.stats`), a page-salvage walk of a corrupt SQLite file (`.recover`), the built-in `selftest` table (`.selftest`), and a fixed set of schema lint rules (`.lint`) |

---

## Architecture and operations

These cannot be probed with SQL. They are read from the tree and from the design documents, and each
one is a deliberate decision rather than an omission.

| | SQLite 3.53.4 | inillucent |
|---|---|---|
| file format | the SQLite format, readable by every tool | its own `.rdb` plus `RDBWAL01` log segments. **A SQLite file cannot be opened** — `database disk image is malformed` — it is imported |
| import from SQLite | — | `inillucent-migrate --sqlite-file src.db dest.rdb`, copy → verify by count and digest → publish by rename; tables, views, triggers, FTS5 indexes and `sqlite_sequence` all carried. **See below** |
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
| assurance | TH3, `testfixture`, ~600 tests per line of code | **the same four red binaries as `382eb78`**, all pre-existing and accounted for (task-1856); a differential oracle against the pinned build; SQLLogicTest; a `BTreeMap` model reference; a fault-injecting VFS; 8 fuzz targets; 23 of 29 crates deny `unwrap`/`panic`/indexing and 22 of 29 forbid `unsafe` |

### The migration path

The only supported route from an existing SQLite application is `inillucent-migrate --sqlite-file`,
and it works for tables, `WITHOUT ROWID` tables, generated columns, partial indexes and foreign
keys — verified by count and by digest. All four of the defects the first run found are closed:

| what | what happened | now |
|---|---|---|
| **any trigger in the source** | the whole migration was refused: *"the new engine does not run triggers, so these cannot be carried"* | **carried.** The claim had been untrue since task-1838 — triggers ship, and every trigger case in this document agrees with SQLite. The refusal was the only thing left of it |
| **any FTS5 table in the source** | refused with *"the declaration of `f_data` did not parse: database disk image is malformed"* | **carried, and the index is rebuilt.** Two causes: SQLite writes a shadow table's declaration with a *quoted* name (`CREATE TABLE 'f_data'(...)`) and this parser took a string literal where a name belongs, and the shadow tables were being copied as ordinary tables. The shadow tables are now skipped and the index is rebuilt from `<name>_content`, docids preserved |
| **a view in the source** | migrated, appeared in `sqlite_schema` as `view\|v`, and then `SELECT * FROM v` answered `no such table: v` | **readable.** The view was not being dropped by the migration at all — it was dropped by `load_schema` on *every* open, which read the schema rows and kept only the tables |
| `sqlite_sequence` | not carried, so `AUTOINCREMENT` state was lost and the next row reused a number | **carried**, so the next `AUTOINCREMENT` value continues where the source left off |

An **external content FTS5 table** in the source is now refused by name rather than migrated into an
empty index — see [FTS5](#fts5--10-of-14).

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
| vector arithmetic `+ - *`, `avg(vector)`, `sum(vector)` | **none, and refused by name** — `unsupported: arithmetic over a vector column`, `unsupported: an aggregate over a vector column`. They used to be *accepted*, coercing the blob to `0.0`, which is the worst of the three possible answers |
| HNSW and IVFFlat index types | **HNSW only**, `CREATE INDEX ... USING inillucent_hnsw (v)` |
| `WITH (m = …, ef_construction = …)`, `SET hnsw.ef_search` | **none** — the index takes no parameters and there is no runtime knob. `PRAGMA hnsw_ef_search` is not a pragma this engine has, and like any name SQLite does not list it answers nothing. The search widens itself instead: see below |
| an ordering on any distance planned onto the index | **cosine only.** `ORDER BY vector_distance_l2(v, ?) LIMIT k` plans as `SCAN` + temp b-tree |
| a mismatched dimension raises | **raises** — `different vector dimensions 4 and 3`, and a non-vector argument raises `vector_distance_cos: argument 2 is not a vector`. A NULL argument is still NULL, which is what every other scalar function does |
| **filtered search** (`WHERE ... ORDER BY v <=> ? LIMIT k`), with `hnsw.iterative_scan` to keep recall | **yes**, and it needs no knob. See below |
| embedding generation | neither has it in SQL; inillucent has an in-process embedder in the **retrieval engine** (`nomic-embed-text-v1.5` through ONNX Runtime, CPU or GPU) reachable from the API, not from SQL |
| ACID, replication, backups, many writers, many processes | PostgreSQL's | single process, one writer |

### The filtered vector search, which was losing nine rows in ten

The first run measured this on 400 unit vectors of 16 dimensions with a predicate selecting 5% of
them:

```sql
CREATE INDEX ie ON e USING inillucent_hnsw (v);
SELECT id FROM e WHERE src = 'a' ORDER BY vector_distance_cos(v, ?) LIMIT 10;
```

| plan | rows returned, before |
|---|---|
| exhaustive (no index) | `280,260,400,320,40,340,140,380,160,80` — **10** |
| indexed | `280` — **1** |

`EXPLAIN QUERY PLAN` said `SEARCH e USING VECTOR INDEX ie (k=2)`: the index was probed for *k*
neighbours and the predicate was applied to what came back, so every neighbour that failed the
filter was a row the query lost. Recall@10 was **0.1**, and nothing reported it.

**The probe now widens itself until it has *k* survivors or the graph is exhausted.** It is the same
idea as pgvector's `hnsw.iterative_scan`, with the loop inside the engine rather than behind a
setting: ask the graph for *k*, run the query's own residual predicates over what comes back, and if
fewer than *k* rows survive, ask for four times as many and try again. A filter that keeps
everything costs one pass, and a filter that keeps one row in a hundred pays for what it needs. The
loop stops when the graph has no more to give, so a predicate that nothing satisfies terminates
rather than doubling forever.

**Measured, at both scales, over the whole grid** — filters keeping 100%, 50%, 5% and 1% of the
rows, at `LIMIT` 1, 10 and 100, the indexed plan's rows compared against the exhaustive plan's
row for row:

| corpus | comparisons | recall | rows |
|---|---|---|---|
| 400 rows, 16 dimensions | 12 of 12 | **1.000** | identical to the exhaustive plan |
| 20,000 rows, 16 dimensions | 12 of 12 | **1.000** | identical to the exhaustive plan |

That is a recall assertion rather than a row count: two plans returning ten rows each is not the
same as their returning *the same* ten. The harness is
`_agent_output/task-1859-feature-gaps/vector-recall.txt`, and three of the cases are checked in as
tests in `crates/inillucent-compat/tests/vector.rs`.

The retrieval engine's own HNSW already did this — it honours a predicate inside the walk and
chooses an exhaustive plan by cost model when the filter is narrow, which is why the graded Filtered
family above reads recall **1.000** against pgvector's 0.328. What was missing was the SQL path
reaching any of it. The goal row "embedding search like pgvector" is now met by the SQL in front of
the engine as well as by the engine.

---

## Performance

**Re-measured after this ticket's work**, because a great deal of it is on the read path — the
vector probe now loops, `USING` joins coalesce a column, every aggregate goes through one feed, and
`LIKE` asks the catalog whether it is case sensitive. `inillucent-fullgate`, medium scale (100,000
rows), 30 paired rounds, every workload's answer digested and compared with SQLite's before a timing
counts. **Nothing in `compat/perf/contract.toml` was touched** — the bars, the weights and the
fixtures are the ones task-1846 set.

| | task-1856, four runs at `382eb78` | task-1859 |
|---|---|---|
| weighted geometric mean | — | **3.94x** |
| weighted lower bound (bar 3.00x) | **3.79x** / 3.94x / 3.96x / 3.93x | **3.84x** — `MET` |
| the 1.00x floor | above | **above**, every required family |
| digests | equal | **30 of 30 equal** |

Per family at medium, this run: `read.point` **28.69x**, `large.values` **14.10x**,
`read.analytical` **6.40x**, `read.range` **4.37x**, `read.join` **4.27x**, `write` **2.09x**,
`transaction` **1.57x**, `open.prepare` **1.44x**, `extension` **1.19x**, `schema` **1.16x**.

Three families are above the floor but under their own bars — `open.prepare` (bar 5.00x), `schema`
(3.00x) and `extension` (1.50x) — which is why the gate still prints `NOT MET` overall. That is the
same three as before this work, and closing them is not this ticket. The full output is
`_agent_output/task-1859-feature-gaps/medium-gate.txt`.

Footprint and the platform note below are from task-1856 and were not re-measured.

Footprint, both arms as whole child processes: medium **79.83 MiB** peak against SQLite's 37.20 MiB
(2.15x), 0.79x the user CPU and 0.15x the kernel CPU; large 418.88 MiB against 179.68 MiB (2.33x).
On disk, 200,000 rows are 15.9 MB against SQLite's 8.7 MB (1.83x). On Linux the same binary was
1.53x weighted at medium (task-1838 §5), and the cause is measured rather than argued: SQLite does
per-statement operating-system work that Windows charges heavily for, so its denominator moves across
platforms and this engine's does not.

---

## What was silently different, and is not any more

These were the cases where both engines answered, the answers differed, and **nothing told the
caller**. It was the list that mattered, because every other row in this document is something an
application can see and work around.

**All seventeen are closed, and the probe finds no new one.** The ten cases that still differ are
[named below](#what-still-differs-and-why); every one of them is an engine identity a caller can
read with a pragma, or a difference in the reference *shell* rather than in SQLite.

| # | construct | SQLite 3.53.4 | inillucent, before | now |
|---|---|---|---|---|
| 1 | `WHERE ... ORDER BY vector_distance_cos(v, ?) LIMIT k` over an HNSW index | every matching row | a tenth of them | **the same rows, recall 1.000** |
| 2 | `INSERT ... ON CONFLICT DO UPDATE SET a = ... WHERE t.a > 500` | the arm is skipped | the arm runs | **skipped** |
| 3 | `PRAGMA recursive_triggers=ON` and a self-inserting trigger | recurses | fires once | **recurses to the cap** |
| 4 | `PRAGMA case_sensitive_like=ON`, then `'ABC' LIKE 'a%'` | 0 | 1 | **0** |
| 5 | `1 IS DISTINCT FROM NULL` | 1 | 0 | **1** |
| 6 | the literal `-9223372036854775808` | integer | real | **integer** |
| 7 | `sum()` overflowing an integer | `integer overflow` | a real | **`integer overflow`** |
| 8 | `0.0/0.0` | NULL | NaN | **NULL** |
| 9 | `jsonb_extract(jsonb('{"a":2}'), '$.a')` | `2` | raw jsonb bytes | **`2`** |
| 10 | `strftime('%g'/'%k'/'%l', ...)` | `24`, ` 9`, ` 9` | the literal text `%g`, `%k`, `%l` | **`24`, ` 9`, ` 9`** |
| 11 | `strftime('%s', ...)` | `1709283907` | `1709283906` | **`1709283907`** |
| 12 | the `subsec` modifier | `...12:00:00.000` | no fractional part | **`...12:00:00.000`** |
| 13 | `fts5(..., tokenize='porter unicode61')` | `run` matches `running` | accepted and ignored | **stems** |
| 14 | `PRAGMA freelist_count` on a fresh database | 0 | 261,883 | **0, and it moves** |
| 15 | `round(1e308, 2)` | `1.0e+308` | `Inf` | **`1.0e+308`** |
| 16 | `length(char(0))` | 0 | 1 | **0** |
| 17 | `CREATE TABLE d AS SELECT a, b FROM t` | stores the affinity's name | stored the resolved type | **the affinity's name, in SQLite's layout** |

And the two things it accepted that SQLite refuses — a `CHECK` containing a subquery, and an
`INTEGER PRIMARY KEY DESC` treated as a rowid alias — both refuse and build a real index now.

The **38 silent pragmas** belonged on this list too, as one entry rather than 38: a statement that is
accepted and answers nothing is indistinguishable from one that answered nothing legitimately. None
of them is silent now.

### What still differs, and why

Ten cases. Each is readable by the caller, and each is here because this engine is not SQLite rather
than because it is pretending to be:

| construct | difference | why it stands |
|---|---|---|
| `PRAGMA page_size`, `page_count` | 32768 / 5 against 4096 / 2 | the page size is a design decision, and the page count follows from it |
| `PRAGMA journal_mode`, `locking_mode` | `wal` / `exclusive` against `delete` / `normal` | one file is one pool with one log; setting either to something else refuses by name |
| `PRAGMA automatic_index` | 0 against 1 | this planner never builds a transient index, so the honest answer is 0 |
| `PRAGMA wal_checkpoint`, `optimize`, `shrink_memory` | `0\|5\|5` against `0\|-1\|-1` | SQLite answers `-1` for a database with no WAL; this one always has a log and reports what it folded in |
| `PRAGMA collation_list` | three against five | `decimal` and `uint` come from the reference **shell**'s bundled extensions, not from SQLite |
| `.mode csv` line endings | CR LF against CR CR LF | a Windows text-mode stdout in the reference, not a decision here |
| `.help TOPIC` | the one-line list against a page of per-command usage | transcription, not compatibility |

---

## What is left, in the order it should be closed

Everything task-1858 ranked 1 through 9 was **task-1859**, and is closed. What follows is what the
re-run still finds, ranked the same way: by what stops an application that runs on SQLite today from
running on this instead.

1. **`json_each(t.d)` joined against a table.** The largest thing left, and the only one an ordinary
   query hits. The argument reads a column of the *outer* row, which makes it a lateral join: the
   module must be re-driven once per outer row, and this engine asks a module once and materialises
   what it answers. That is a new operator, and it is the standard way to expand a JSON array per
   row.
2. **FTS5's `highlight()` and `snippet()`.** Both need the byte offsets of the matched terms inside
   the stored column; the index keeps positions per term. Re-tokenising the row at query time and
   mapping token positions back to bytes is the work.
3. **FTS5 external content tables.** Refused by name rather than silently empty, which is the right
   state to be refused in. Supporting them means a module being able to read a *user* table, which
   is a capability the module interface does not have.
4. **Two `ON CONFLICT` clauses on one statement.** The conflict a write hits does not carry which
   constraint reported it, so there is nothing to match a second clause's target against.
5. **The modules this engine has not got**: `dbstat`, `sqlite_dbpage`, `geopoly`, `sqlite_offset`,
   `fts5vocab`, `rtreecheck`/`rtreedepth`/`rtreenode`, an R-Tree auxiliary column, and the session
   extension over the new engine.
6. **The pgvector surface this engine does not offer**: `halfvec`/`bit`/`sparsevec`, the operator
   spellings, `l1_distance`/`hamming_distance`/`jaccard_distance`, `vector_dims`/`l2_normalize`/
   `subvector`, IVFFlat, and an ordering on a distance other than cosine planned onto the index. All
   are absent by name rather than silently wrong.
7. **The deliberate non-goals**, listed here so they are not mistaken for oversights: SQLite's file
   format, the `sqlite3_*` C ABI on the new engine, more than one process on a file, more than one
   writer, more than one thread, journal modes other than WAL, plain `EXPLAIN`, loadable extensions,
   FTS3/FTS4, `REGEXP` and the reference shell's other bundled extensions, and the seven dot commands
   that read registers this engine does not keep.

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

The cases the engine's own suite carries are `crates/inillucent-compat/tests/semantics.rs` — **164
now**, up from 110, every one of them a construct this document moved — plus `vector.rs` for the
filtered-search recall and `new_engine_writes.rs` for the write path. They run in CI and fail when a
construct changes its mind in either direction. The probe is wider than the suite by design: it is
the instrument that *finds* a difference, and a difference it finds becomes a case here.

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
