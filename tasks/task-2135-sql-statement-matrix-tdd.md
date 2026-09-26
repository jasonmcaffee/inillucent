# task-2135: the SQL statement matrix

A test suite that runs every SQL statement form inillucent accepts, in every context an application
puts it in, against real database files, graded against the pinned SQLite 3.53.4, and fast enough to
run on every change.

This document is the design. The implementation is task-2137, for Opus. Defects the suite finds go
on task-2136, "Inillucent Test Suite Bugs Found", never into this document and never fixed inside
task-2137 unless the fix is one line and is its own commit.

---

## 1. Why the current suite misses real bugs

### 1.1 What the last four tickets found

Four tickets wrote real applications on inillucent in the last week: the RAG agent examples
(task-2130), the todo service (task-2131, task-2132), the coffee shop (task-2133) and the consumer
probe (task-2120). Each found defects that every existing suite passed over. The commit messages list
them. Sorted by what kind of defect each one is:

| Defect | Construct alone | What it was combined with |
|---|---|---|
| `LEFT JOIN` lost rows | works | an index on part of its `ON` |
| `RIGHT` and `FULL` joins lost unmatched rows | work | an index seek on the `ON` |
| window functions refused | work | inside a derived table, a CTE or a view |
| a correlated subquery refused | works | beside a window function |
| `WHERE` on a view's columns ignored by the planner | works | a view or derived table as the source |
| `UPDATE ... FROM` changed a row several times | works | several join rows matching one target row |
| `UPDATE ... FROM` dropped arguments | works | a table valued function or a derived table in `FROM` |
| `json_group_array` lost the JSON subtype | works | nested inside another JSON function |
| `min`, `max` and an aggregate `ORDER BY` ignored collation | work | a column with a `COLLATE` |
| `json_each` returned NULL columns | works | a lateral join with `WHERE t.id = j.value` |
| a trigger's write to a virtual table was lost | works | a trigger body |
| a scalar subquery refused | works | as a value in a virtual table insert, or in a trigger body |
| `INSERT ... SELECT` into FTS5 refused | works for ordinary tables | a virtual table as the target |
| `embed()` refused | works | in a virtual table's `VALUES` row |
| an index on a generated column refused | works | a `VIRTUAL` generated column |
| bare `REINDEX` failed | works | a database holding a `WITHOUT ROWID` table |
| `ORDER BY` name matched the wrong column | works | an alias with the same name as a column |

Every row has the same pattern. The construct works when a test asks about it on its own. It fails
when it is combined with a second construct: a source kind (view, derived table, CTE, virtual table,
table valued function), an access path (an index, a partial index, a collation), or a placement (a
trigger body, an `UPDATE ... FROM`, a subquery). The existing suites were written one construct at a
time, by people, so they test the combinations somebody thought of.

### 1.2 What exists today

The repository already has most of the parts. None of them does the job on its own.

| Suite | What it grades | Why it did not catch the defects above |
|---|---|---|
| `differential::differential_part8` | 1,143 hand written scripts, byte for byte against the pinned `sqlite3` shell | hand written, so each case is one construct; runs on `:memory:`, not a file; starts two shell processes per case and takes **258 s**, the slowest target in the `differential` tier |
| `differential::semantics` | 235 whole scripts through both shells | same as above; **106 s** |
| `differential::*` using `differential::compare` | per statement, typed values, through the persistent `sqlite-oracle` process | each file covers one area and was written by hand |
| `engine::conformance` | `tests/conformance/select-foundational.test`, one sqllogictest file of recorded answers | one schema, `SELECT` only |
| `engine::tlp_differential`, `engine::pqs_differential` | generated predicates, graded against the engine itself | only base tables, only `WHERE`; no views, CTEs, derived tables, virtual tables, triggers or DML |
| `tools/feature-probe` | 416 cases, one per feature | one construct per case by design |
| `drivers/inillucent-driver/tests/capability.rs` | 49 capability rows, both directions | one probe per row |
| `fuzz/fuzz_targets/sql_text.rs` | arbitrary text into `prepare` | never runs a statement, so it finds crashes in the parser and binder only |

Two conclusions follow:

1. **The missing piece is combinations, not constructs.** Adding more hand written cases grows the
   same list that already missed these defects.
2. **The expensive part of the current differential suites is process startup.** Part 8 spends most
   of its 258 s starting two shells for each of 1,143 cases. The persistent `sqlite-oracle` process
   that `differential::compare` already uses answers a statement in well under a millisecond. The
   new suite uses that process, one per test thread, for its whole run.

---

## 2. What was learned from other projects

Research summary. Sources are listed in section 13.

| Project or method | What it does | What this design takes from it |
|---|---|---|
| SQLite's sqllogictest | 7.2 million generated queries in a plain text format: `statement ok`, `statement error`, `query <types> <sort mode>`; sort modes `nosort`, `rowsort`, `valuesort`; `hash-threshold` replaces a large result with its MD5 | the file format for hand written cases (the repository already parses it in `crates/inillucent-compat/src/slt.rs`); `rowsort` for any query without a total `ORDER BY` |
| SQLite TH3 and TCL suites | 100% branch coverage; every test instance is run under many configurations; OOM and I/O fault injection | the configuration arms (section 7); a coverage report used to find untested code (section 9.3) |
| DuckDB | `.test` files run on every change, `.test_slow` files only in the full run; `require` skips a file when a feature is absent | the change, merge and nightly split (section 8) |
| CockroachDB logic tests | the same file runs under many named configurations, so one expectation checks several execution paths | one case runs at several arms and through several surfaces (sections 7 and 5.5) |
| SQLancer: TLP, NoREC, PQS | properties that must hold without a reference engine; together they found several hundred logic bugs, many in SQLite | already in `engine::tlp_differential` and `pqs_differential`; extended to every source kind (section 6.3) |
| SQLancer: DQE | `SELECT`, `UPDATE` and `DELETE` with the same `WHERE` must select the same rows | a new oracle for DML (section 6.3) |
| EET and CODDTest | rewrite a query into an equivalent form and compare the answers | the placement wrappers (section 5.3): a query must answer the same at the top level, as a derived table, as a CTE, as a view |
| Combinatorial testing (PICT, NIST ACTS) | a covering array includes every pair (or every triple) of axis values at least once; most real defects need two or three interacting values; a pairwise array is 80% to 95% smaller than the full product | the interaction layer (section 5.2) |
| Bounded exhaustive testing (SmallCheck) | enumerate every input up to a small size; most defects appear at small sizes and the failing input is already minimal | the construct layer enumerates every grammar variant at depth one (section 5.1) |
| SQLsmith, Squirrel | random grammar generation, mostly for crashes | the nightly random layer, graded against the oracle (section 5.4) |
| cargo-nextest | one process per test; `--partition slice:m/n` gives even shards | shard support in `inillucent-testrun` (section 8.3); a panic in one case must not end the run |

Two pitfalls the research agrees on, and how this design answers each:

- **Row order.** A query without `ORDER BY` that is compared in order is really a comparison of two
  query plans. Every generated query either ends in an `ORDER BY` over every output column, or is
  compared as a sorted multiset (`rowsort`). No case is compared in order without a total `ORDER BY`.
- **Error text.** Two engines do not word their errors the same. Errors are compared by status and
  extended result code, never by message. The one exception is `RAISE`, whose message is the value
  under test, which `differential::compare` already handles.

---

## 3. Goals and limits

### 3.1 Goals

1. **Every statement form is run.** Every variant of every AST enum in `crates/inillucent-sql/src/ast.rs`,
   every function `PRAGMA function_list` reports, every pragma in `pragma_register.rs`, every module in
   `PRAGMA module_list` and every collation in `PRAGMA collation_list` has at least one case that
   runs it and grades the answer. A test enforces this (section 9.1), so a new variant or function
   cannot be added without a case.
2. **Every pair of contexts is run.** For each statement family, every pair of values from its axes
   (section 4) appears in at least one case on every change, and every triple appears in at least
   one case on every merge.
3. **Real files, both engines.** Each case opens a database file on disk on both sides. Every case
   that writes closes the database and opens it again, then asks its questions a second time, and
   runs `PRAGMA integrity_check` on both engines (testing standard rule 1.4).
4. **Graded against the pinned SQLite** where SQLite has the construct, and against a property or a
   brute force answer computed in Rust where it does not (vector search, `inillucent_search`).
5. **Fast.** The part that runs on every change finishes in **under 60 s of wall clock** on the
   24 core development machine. Section 8 has the budget and how it is kept.
6. **A failure is a small, stable, replayable case.** A generated case has a stable id. A failure
   prints the id, the seed, the arm and the smallest statement list that still fails. The smallest
   case is saved to a retained corpus that runs on every change from then on.

### 3.2 Limits

- **Not crash durability.** Kill points, torn writes and fault injection stay in the `durability`
  tier. The matrix reopens a cleanly closed file.
- **Not the shell's text output.** `semantics.rs` and `tools/feature-probe` compare what the shell
  prints. The matrix compares typed values through the engine. Both stay.
- **Not two processes on one file.** Concurrency stays in its existing suites.
- **Not performance.** The gates and `budget.rs` stay the performance instruments.
- **No new production dependency.** The covering array generator, the case generator and the
  shrinker are written in `inillucent-compat`, which is a test only crate. SQLite stays a child
  process, as `docs/dependency-policy.md` requires.

---

## 4. The axes

A case is one statement family with one value chosen on each of its axes. The axes are the list of
things that decided whether the defects in section 1.1 appeared.

### 4.1 Statement families

From `Statement` in `ast.rs:1010`, grouped by the code path each one exercises.

| Family | Statement forms |
|---|---|
| `select` | `SELECT` core with `DISTINCT`/`ALL`, `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` with `NULLS FIRST`/`LAST`, `LIMIT`/`OFFSET`, `VALUES` |
| `join` | `JoinKind` `Comma`, `Inner`, `Cross`, `Left`, `Right`, `Full`; `ON`, `USING`, `NATURAL`; `INDEXED BY`, `NOT INDEXED` |
| `compound` | `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`, with `ORDER BY` and `LIMIT` on the compound |
| `cte` | `WITH`, `WITH RECURSIVE`, `MATERIALIZED`, `NOT MATERIALIZED`, a CTE used twice |
| `window` | every `WindowFunc`, every aggregate as a window; `FrameUnit` `ROWS`/`RANGE`/`GROUPS`; every `FrameBound`; every `FrameExclude`; named windows; `FILTER` |
| `subquery` | scalar, `IN (SELECT)`, `IN table`, `EXISTS`, `NOT EXISTS`, correlated and not, row values |
| `expression` | every `Expr` variant, every `BinaryOp` and `UnaryOp`, `CAST` to each affinity, `COLLATE`, `LIKE`/`GLOB`/`REGEXP`/`MATCH` with `ESCAPE`, `CASE` with and without an operand, `BETWEEN`, `IS [NOT] DISTINCT FROM` |
| `function` | every row of `PRAGMA function_list`: scalar, aggregate (with `DISTINCT`, `ORDER BY`, `FILTER`), window, date and time, math, JSON and JSONB, `printf` |
| `insert` | `VALUES` (one and many rows), `SELECT`, `DEFAULT VALUES`, column list or none, each `ConflictAction`, one and several `ON CONFLICT` arms with `DO UPDATE ... WHERE` and `DO NOTHING`, `RETURNING` |
| `update` | `SET` one and several columns, row value `SET`, `FROM`, `ORDER BY`/`LIMIT`/`OFFSET`, each `ConflictAction`, `RETURNING` |
| `delete` | `WHERE`, `ORDER BY`/`LIMIT`/`OFFSET`, `RETURNING` |
| `ddl_table` | `CREATE TABLE` with every `ColumnConstraint` and `TableConstraint`, `WITHOUT ROWID`, `STRICT`, `AS SELECT`, `TEMP`, `IF NOT EXISTS`; every `AlterAction`; `DROP` of each `ObjectKind` |
| `ddl_index` | `UNIQUE`, partial (`WHERE`), on an expression, with `COLLATE`, `DESC`, on a generated column, `USING` a module |
| `ddl_view` | `CREATE VIEW` with a column list, over a join, a compound, a CTE, an aggregate and a window |
| `trigger` | `BEFORE`/`AFTER`/`INSTEAD OF` × `INSERT`/`UPDATE`/`UPDATE OF`/`DELETE`, `FOR EACH ROW`, `WHEN`, bodies holding each DML form, `RAISE` with each `RaiseAction`, recursive triggers on and off |
| `constraint` | `NOT NULL`, `UNIQUE`, `CHECK`, `PRIMARY KEY`, foreign keys with every `ReferentialAction`, `DEFERRABLE INITIALLY DEFERRED`, `PRAGMA foreign_keys` on and off, `defer_foreign_keys` |
| `transaction` | `BEGIN` `DEFERRED`/`IMMEDIATE`/`EXCLUSIVE`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `RELEASE`, `ROLLBACK TO`, nested savepoints, a failing statement inside a transaction |
| `vtab` | `fts5` (with its query syntax, `bm25`, `highlight`, `snippet`), `rtree`, `json_each`, `json_tree`, `generate_series`, the `pragma_*` table valued functions, `dbstat`, `inillucent_search` |
| `schema` | `ATTACH`, `DETACH`, a table in `temp`, a table in an attached database, `sqlite_schema` reads |
| `maintenance` | `VACUUM`, `VACUUM INTO`, `ANALYZE` (whole, one table, one index), `REINDEX` (bare, table, index, collation), `EXPLAIN`, `EXPLAIN QUERY PLAN` (grade that it runs and names the right tables, not its text) |
| `pragma` | each pragma in `REGISTER`, read and (where it takes one) set, then read back after a reopen |
| `vector` | `VECTOR(N)` columns, every distance operator and `vector_*` function, `USING inillucent_hnsw` in exact and approximate mode, `embed()` with a registered function |

### 4.2 Context axes

These apply across families. A family lists which of them it takes.

| Axis | Values |
|---|---|
| **source** (what the statement reads) | rowid table; `INTEGER PRIMARY KEY` table; `WITHOUT ROWID` table; `STRICT` table; view; derived table `(SELECT ...)`; CTE; `MATERIALIZED` CTE; table valued function; FTS5 table; `inillucent_search` table; `TEMP` table; table in an attached database |
| **access** (how the planner can reach the rows) | no index; index on the filtered column; covering index; partial index; index on an expression; index with a `COLLATE`; `UNIQUE` index; after `ANALYZE`; `NOT INDEXED` |
| **placement** (where the statement sits) | top level; derived table; CTE; view body; `IN` subquery; `EXISTS`; scalar subquery; correlated subquery; trigger body; `UPDATE ... FROM` source; `INSERT ... SELECT` source; `RETURNING` expression |
| **data** (what the rows hold) | empty table; one row; NULLs in the filtered and joined columns; duplicates; every storage class mixed in one column; text that looks like a number; case variants under `NOCASE`; values at the integer and real limits |
| **affinity** (declared type of the column) | `INTEGER`, `REAL`, `TEXT`, `BLOB`, `NUMERIC`, none, and each `STRICT` type |
| **binding** (how values reach the statement) | literal in the text; `?N` parameter; `:name` parameter; the same prepared statement run twice with different values |
| **transaction** (the state around the statement) | autocommit; inside `BEGIN ... COMMIT`; inside `BEGIN ... ROLLBACK`; inside a savepoint that is rolled back |
| **arm** (file configuration, from `crates/inillucent-compat/src/matrix.rs`) | `default`, `sqlite_page`, `small_pool`, `truncate_journal`, `persist_journal`, `waiting` |

### 4.3 Constraints between axes

Not every combination is legal. Each family declares its constraints as a Rust predicate, and the
covering array generator never emits a combination the predicate refuses. Examples:

- `RETURNING` placement only with the `insert`, `update` and `delete` families.
- A `WITHOUT ROWID` source needs a `PRIMARY KEY`, and `access = NOT INDEXED` is not allowed on a
  table valued function.
- The FTS5 and `inillucent_search` sources take only `access = no index`.
- A trigger body placement cannot hold a `WITH` clause on its statement (SQLite refuses it; the case
  for that refusal is a construct case, section 5.1, not an interaction case).
- `transaction = ROLLBACK` is only meaningful for families that write.

A constraint that is wrong makes a case disappear, which is the failure the suite exists to prevent.
So the generator also reports, per family, how many combinations the constraints removed, and a
test asserts that number (section 9.2). A constraint that starts removing more than it did is a
visible change in a reviewed file.

---

## 5. The four layers of cases

### 5.1 Layer 1: construct cases (hand written, depth one)

One or more cases for every value in section 4.1, each on its own, plus the refusals: every
statement SQLite refuses must be refused by inillucent with the same status. These are hand written
sqllogictest files, because a person has to pick the values that make each construct's answer
interesting (a `LAG` with a default, a `CHECK` that fails on the second row, a `GLOB` with a
character class).

- **Where:** `crates/inillucent-compat/tests/corpora/matrix/<family>/*.slt`.
- **Format:** the sqllogictest format `slt.rs` already parses, with no expected result block. The
  answer comes from the oracle at run time. Section 6.1 lists the few directives added to the format.
- **Source for the list:** `compat/syntax.toml` already has a positive and a negative example for
  every production on SQLite's syntax diagrams. The implementation starts by converting those rows
  into Layer 1 cases that run, rather than only parse. The 1,143 part 8 cases and the 416 feature
  probe cases are then moved or copied in (section 10).
- **Size:** about 2,500 cases. The function list alone is 213 rows at about four cases each.

### 5.2 Layer 2: interaction cases (generated, covering arrays)

For each family, a template takes one value per axis and produces a case: the schema, the data,
the index, the statement in its placement, and the questions to ask afterwards.

```text
family = window
axes   = source (13) x access (9) x placement (12) x data (8) x frame (unit x start x end x exclude)
full product for one window function: about 13 x 9 x 12 x 8 x 60 = 674,000 cases
strength two (every pair):             about 13 x 12 x a small factor = 900 to 1,500 cases
strength three (every triple):         about 15,000 to 25,000 cases
```

- **Generator:** an IPOG covering array builder written in `crates/inillucent-compat/src/statement_matrix/cover.rs`.
  IPOG adds one axis at a time and extends the array greedily, and it is short (about 200 lines) and
  deterministic for a given seed. No crate is added.
- **Strength two on every change, strength three on every merge.** The difference is a parameter.
- **Each template writes a total `ORDER BY`** over every output column unless the case is about
  ordering, in which case the comparison is `rowsort` plus a check that the ordering keys are
  nondecreasing.
- **A case has a stable id:** `<family>-<first 12 hex digits of SHA3-256 of the case's canonical text>`. SHA3-256 because it is the digest `inillucent-compat` already has; only its stability matters.
  The canonical text is the statements with whitespace normalised, so reordering the generator's code
  does not rename cases and the known difference list (section 6.4) stays valid.
- **Size:** about 10,000 cases at strength two across all families; about 150,000 at strength three.
  The implementation measures both and records them (section 9.2).

### 5.3 Layer 3: placement equivalence (generated, no oracle needed)

The defects about views, CTEs and derived tables were each "the same query answers differently when
it is wrapped". So every `SELECT` a Layer 2 template produces is also run in each wrapping that
keeps its meaning, and all answers must be the same multiset:

| Wrapping | Form |
|---|---|
| top level | `Q` |
| derived table | `SELECT * FROM (Q) AS d` |
| CTE | `WITH c AS (Q) SELECT * FROM c` |
| materialized CTE | `WITH c AS MATERIALIZED (Q) SELECT * FROM c` |
| view | `CREATE VIEW v AS Q; SELECT * FROM v` |
| outer filter on a wrapping | `SELECT * FROM (Q) AS d WHERE <P over d's columns>` against `Q` with `P` added |
| compound with nothing | `Q UNION ALL SELECT ... WHERE 0` |
| stored | `CREATE TABLE s AS Q; SELECT * FROM s` |

The outer filter row is the task-2134 view push down defect exactly. This layer is also graded
against SQLite, but its main value is that it needs no oracle, so it also runs for the inillucent
only features in section 6.2.

### 5.4 Layer 4: random statements (generated, nightly)

A grammar driven generator (the approach of SQLsmith and of SQLite's own sqllogictest generator)
builds random statements over a random small schema, up to a depth limit, and grades each against
the oracle. It runs only in the nightly job, from a seed that is the date. Every run records its seed
and case count in `tests/nightly-history.tsv`.

It finds what the axes did not name. Anything it finds is shrunk (section 6.5), saved to the retained
corpus, and then its axis is added to section 4.2 if one is missing.

### 5.5 Surfaces

An application reaches the engine through more than `Connection::prepare`. Layer 1 also runs through:

- `SharedDatabase` (the todo service defect about TEMP triggers and `total_changes` was only there);
- a prepared statement stepped, reset and run again with new bindings;
- `execute_batch` with the whole case as one script (the trigger body splitting defect was only there).

The command line and MCP surfaces keep their own suites and are not part of the matrix.

---

## 6. Grading

### 6.1 The case file directives

The existing sqllogictest records, plus four directives. Each is one line.

| Directive | Meaning |
|---|---|
| `statement ok` / `statement error <status>` | as sqllogictest; `<status>` is an inillucent status name (`constraint`, `syntax`, `not_found`, `readonly`, ...) compared with the status SQLite's extended code maps to |
| `query <types> <nosort\|rowsort\|valuesort>` with no result block | grade against the oracle |
| `query <types> <sort>` with a result block | grade against the recorded answer; used for inillucent only features |
| `reopen` | close both databases and open them again; the following queries read what recovery produced |
| `arms <list>` | run this file only at these arms; default is every arm the tier runs |
| `oracle none` | this file has no SQLite equivalent; every query needs a recorded answer or a property |
| `known <bug id>` | this record is expected to disagree, see section 6.4 |

`nosort` is refused by the loader unless the query's top level `ORDER BY` names every result
column, so an ordered comparison of an unordered query cannot be written.

### 6.2 What is compared

For every statement, on both engines:

1. **Outcome.** Both succeed, or both fail. When both fail, the status must match. When inillucent
   answers `unsupported` and SQLite succeeds, the case is a **gap**, not a wrong answer, and it is
   graded against the capability table: the construct must appear in `CAPABILITIES` as `no` or
   `partial`, or the case fails. That keeps `inillucent capabilities` honest over every construct in
   the matrix instead of over 49 probes.
2. **Rows and columns.** Typed values through `TaggedValue`. Integers, text and blobs are compared
   exactly. Reals are compared by bits, as `oracle.rs` does, with one exception: the transcendental
   math functions (`exp`, `ln`, `log`, `pow`, `sin` and the rest of `MathFunc`) may differ by one unit
   in the last place, because SQLite calls the C library and Rust calls its own. The tolerance is
   declared per function in one table, and a difference of more than one unit still fails. Column
   names are compared too, because the ORDER BY alias defect was a naming defect.
3. **Counters.** `changes`, `total_changes`, `last_insert_rowid` and `autocommit`, as
   `differential::compare` already does.
4. **After the case.** For a case that wrote: `reopen`, then every query in the case again, then
   `PRAGMA integrity_check` on both sides, which must answer `ok`.
5. **No panic.** Each case runs inside `std::panic::catch_unwind`. A panic is a failure with the case
   id, and the run continues with the next case on a fresh connection.

For features SQLite does not have:

| Feature | What the answer is graded against |
|---|---|
| `VECTOR(N)` and the distance operators | the distance computed in Rust from the stored values |
| `inillucent_hnsw` in exact mode | a brute force nearest neighbour list computed in Rust; must be equal |
| `inillucent_hnsw` in approximate mode | recall at k against the brute force list, as a count over a fixed corpus (rule 1.7) |
| `inillucent_search` | the placement equivalence of section 5.3, plus: every hit satisfies the keyword or vector constraint, recomputed in Rust |
| `DELETE`/`UPDATE ... LIMIT` (the pinned SQLite is built without it) | the same statement rewritten as `WHERE rowid IN (SELECT rowid ... ORDER BY ... LIMIT ...)`, run on both engines |

### 6.3 Properties checked on every generated query

These do not need SQLite, so they also catch a defect SQLite shares:

- **TLP**, over every source kind rather than only base tables: `Q WHERE P`, `Q WHERE NOT P` and
  `Q WHERE P IS NULL` together are exactly `Q`.
- **NoREC**: `SELECT count(*) FROM s WHERE P` equals `SELECT sum(CASE WHEN P THEN 1 ELSE 0 END) FROM s`.
- **DQE**: for a predicate `P`, the rows `SELECT rowid FROM t WHERE P` names are the rows that
  `UPDATE t SET marker = 1 WHERE P` changes and the rows `DELETE FROM t WHERE P` removes. The
  `UPDATE ... FROM` defect that changed a row several times breaks this.
- **Index agreement**: the same query with `NOT INDEXED` and with each index in turn answers the same
  (testing standard rule 1.6).
- **ANALYZE agreement**: the same query before and after `ANALYZE` answers the same.

The existing `tlp_differential.rs` and `pqs_differential.rs` keep running. Their generators are
reused by Layer 2 rather than copied.

### 6.4 Known differences

A case that disagrees and is not fixed yet is recorded, not deleted and not ignored (testing
standard rule 1.3). The record is one line in
`crates/inillucent-compat/tests/corpora/matrix/known.list`: the case id, a tab, the bug number on task-2136,
"Inillucent Test Suite Bugs Found", a tab, and one sentence.

- A listed case that **still disagrees** passes.
- A listed case that **now agrees** fails, with a message saying to remove its line. A fix cannot
  land without the list shrinking.
- A listed id that **no case has** fails, so a renamed case cannot leave a line behind.

This is the same two way check `differential_part8.rs` already makes on its `allow.list`.

Deliberate differences that are not bugs (the `LIMIT` on `DELETE`, page size artifacts) are rules in
a `deliberate.toml` with a reason each, matched by construct rather than by case id, because they
would otherwise need a line for every generated case that uses them.

### 6.5 Shrinking a failure

A generated case that fails is reduced before it is reported:

1. Remove statements from the setup one at a time while the case still fails the same way.
2. Remove rows from each `INSERT`.
3. Remove clauses (`WHERE` terms, `ORDER BY` terms, joins, CTEs) and replace subexpressions with
   literals.
4. Stop when no single removal keeps the failure.

"The same way" means the same kind of difference on the same statement index: outcome, status,
rows, counters, reopen or panic. The shrunk case is written to
`crates/inillucent-compat/tests/corpora/matrix/retained/<family>-<id>.slt` and replayed on every
change from then on (section 8). Shrinking runs only when a case fails, so it costs nothing on a
green run.

---

## 7. Real files and configuration arms

- **Every case runs on files.** Both engines open a file in a scratch directory under the target
  directory: `<CARGO_TARGET_TMPDIR>/matrix/<shard>/<thread>/<case id>/{sqlite.db,inillucent.rdb}`.
  The oracle is sent `open` with its path. The part 8 corpus runs on `:memory:` today, which is why it
  never reopened anything.
- **Setup is shared, not repeated.** Cases in one family with the same schema and data share a
  fixture. The fixture directory is built once per thread on both engines, closed, and then the
  directory is copied for each case (the same trick `perfhistory` uses, because inillucent's log is a
  set of numbered segment files that a single file copy would miss). A file copy of a small database
  costs well under a millisecond; building the schema and inserting the rows costs more.
- **Each scratch directory is removed when its case passes**, with `remove_database` from
  `inillucent-base::testing` for the files and then the directory. A failing case's directory is kept
  and its path printed.
- **The arms come from `crates/inillucent-compat/src/matrix.rs`** so the suite and the stories agree
  about what `small_pool` means. The oracle side runs every arm at SQLite's defaults, except
  `sqlite_page`, where the oracle also sets `PRAGMA page_size = 4096` before the first write.

---

## 8. When the suite runs, and how it stays fast

### 8.1 The three cadences

| Cadence | Where | What runs | Wall clock budget on 24 cores |
|---|---|---|---|
| **change** (`--changed`, every agent ticket) | new tier `matrix`, see 8.2 | Layer 1 at the `default` arm; Layer 2 at strength two at `default`; Layer 3 on those; the retained corpus at every arm | **under 60 s** |
| **merge** (CI on every push, `--cadence merge`) | new tier `matrix_deep` | Layer 1 and Layer 2 at strength two at every arm; Layer 2 at strength three at `default`; the surfaces of section 5.5 | under 15 minutes |
| **nightly** (`packaging/nightly.ps1`, 02:00) | tier `nightly` | Layer 2 at strength three at every arm; Layer 4 for a fixed number of cases from the date's seed | under 60 minutes |

The change budget is the constraint that shapes the rest. Two numbers set it:

- The `differential` tier's current tail is part 8 at 258 s and `semantics` at 106 s. Moving part 8
  onto the matrix runner (section 10) takes the largest single target out of the change run.
- A statement costs microseconds in inillucent in process and well under a millisecond in the
  persistent oracle. With the setup copied from a fixture, an interaction case (setup copy, one to
  five statements on each engine, reopen, `integrity_check`) should cost about 2 to 5 ms of processor
  time. 12,500 change cases at 4 ms is 50 s of processor time, which is 2 to 3 s of wall clock
  over 24 jobs.

Those per case costs are estimates. **The first step of the implementation measures them** (section
11, phase 0) on 200 real cases. If the measured cost puts the change budget out of reach, the change
cadence runs strength two over a subset of axes, and the full strength two moves to merge. The
budget is never met by dropping families.

#### Measured in phase 0

Measured on 2026-09-25 with `inillucent-matrix run`, which drives the same `Runner` the suites use,
in the debug build the tests run in, on the 24 core development machine, with the scratch files on
the D: drive where the target directory is. Processor time is this process plus the oracle process,
read from the oracle's own accounting.

| What | Processor time per case | Wall clock per case |
|---|---|---|
| the first 200 part 8 cases, each building its own tables, one thread | 23.2 ms (20.2 inillucent, 3.0 oracle) | 67 ms |
| a case that only reads, on a database of its own (`SELECT 1`), one thread | 9.8 ms | 25 ms |
| the same at the `sqlite_page` arm (4,096 byte pages) | 5.6 ms | 21 ms |
| a case that writes once, then reopens and runs `integrity_check`, one thread | 25 ms | 94 ms |
| the same, 480 cases on 24 threads | 31.6 ms | 7.5 ms |
| a read only case run inside a shared copy of its fixture, one thread | 0.47 ms | 0.2 ms |
| the same, 2,400 cases on 24 threads | 0.62 ms | 0.2 ms |
| copying a fixture directory | | 1.5 to 2.4 ms alone, about 6 ms on 24 threads |
| all 1,532 Layer 1 cases at the `default` arm, 24 threads | 24.5 ms | 9.4 ms (14.4 s in all) |

Three things follow, and each changed the implementation:

1. **Opening and closing a database is most of the cost of a small case.** About 10 ms of the
   engine's processor time at the default arm goes to the open and the close, and about 5 ms at
   4,096 byte pages. So a read only case that shares its fixture with others runs inside one opened
   copy of that fixture, which costs 0.5 ms instead of 10 to 25 ms. It asks exactly the questions it
   would have asked of its own copy: a read cannot change the file or the connection's counters. A
   case that writes still gets its own copy, its reopen and its integrity check.
2. **A commit waits on the disk, and 24 threads committing at once wait on each other.** A four
   statement fixture took about 110 ms alone and about 4 s with every thread committing. A fixture
   whose setup only creates schema and inserts rows is now built in one transaction, which made it one
   commit, and a case is placed on a test thread by its fixture's key when it only reads, so the
   fixture is built once per thread that needs it.
3. **The estimate of 2 to 5 ms of processor time per interaction case holds only for the cases that
   read.** A case that writes costs about 25 ms of processor time and about 7.5 ms of wall clock at
   full load. The Layer 2 templates therefore put most of their questions in read only cases over a
   shared fixture, and keep a write in the cases whose subject is a write.

After these changes the `matrix` tier, holding only Layer 1, took 21.3 s of wall clock through
`inillucent-testrun --tier matrix`.

#### Measured at the end of the implementation

Measured on 2026-09-26 with `inillucent-testrun`, on the same machine, with each target's time
recorded in `tests/timings.toml` so the runner starts the longest first.

| Cadence | Cases | Wall clock | Processor time |
|---|---|---|---|
| change, tier `matrix` | Layer 1 (1,707 hand written cases, the retained corpus at every arm) and 2,522 generated at strength two | 56.9 s, 34 targets | 1,082 s |
| merge, tier `matrix_deep` | Layer 1 and 2,784 generated at strength two at all six arms, 21,189 more at strength three at `default`, the surfaces | 881.9 s, 78 targets | 18,700 s |
| nightly, `matrix` and `matrix_random` | 23,973 generated at strength three at all six arms, 4,000 random | 3,000.4 s, 24 targets | 58,278 s |

`counts.toml` holds the generated counts per family, strength and cadence.

Five measurements changed the implementation after phase 0:

1. **The merge tier's first full run took 29.6 minutes** and 16,613 s of processor time, with one
   process per family; `expression` alone took 1,775 s. The strength three triples at two arms were
   61% of its 69,000 case runs. The merge tier now runs the triples at `default` only, and the
   nightly runs them at every arm. Each family is split into shards of about 250 s.
2. **Shards alone made it slower per case**, 25,966 s of processor time in 20 minutes, because each
   shard divided its cases into eight groups and a fixture is shared only inside a group. The merge
   modules have two groups each, the number of test threads a process runs, and the tier takes 881.9 s.
3. **The change tier took 66.7 s** while its targets had no recorded times, because the runner then
   started its slowest targets last. With their times recorded it takes 56.9 s. `insert`,
   `function`, `select`, `delete` and `update` run as two shards and the retained corpus as three.
4. **The nightly triples took 62 minutes at eight shards**, which used 16 of the machine's 24 cores,
   and 29,968 s of processor time. At twenty shards they take 50 minutes; the processor time rises
   to 58,278 s because 40 test threads share 24 cores, and the wall clock is what the budget is.
   The first nightly also showed the random layer's four shards writing one directory each, which
   corrupted each other's databases; the directory now names the shard.
5. **A matrix run against an older engine grew to 66 GB in one process.** `inillucent-testrun` now
   caps each test process at 8 GiB and every process it starts at a quarter of physical memory, and
   the runner arms the engine's statement budget around every case.

### 8.2 Tiers and selection

Two new tiers in `tests/selection.toml`:

```toml
[[tier]]
name = "matrix"
purpose = "every statement form and every pair of contexts, against the pinned SQLite, on files"
cadence = "change"

[[tier]]
name = "matrix_deep"
purpose = "every triple of contexts and every configuration arm"
cadence = "merge"
```

The suites are modules of `crates/inillucent-compat/tests/matrix/main.rs` and
`tests/matrix_deep/main.rs`, one module per family, so the runner starts each family in its own
process (section 2.1 of the testing standard). Each row's `covers` names what the family drives:

| Family modules | `covers` |
|---|---|
| `select`, `join`, `compound`, `cte`, `window`, `subquery`, `expression` | `inillucent-engine` |
| `function` | `inillucent-engine`, `inillucent-scalar` |
| `insert`, `update`, `delete`, `constraint`, `trigger`, `transaction` | `inillucent-engine` |
| `ddl_table`, `ddl_index`, `ddl_view`, `schema`, `maintenance`, `pragma` | `inillucent-engine` |
| `vtab`, `vector` | `inillucent-engine` and the module crates they load |
| `retained` | `inillucent-engine` |

Every row declares `requires = ["oracle"]`, so a machine without `.sqlite-ref/` reports the suite
under `--strict` instead of passing it (testing standard section 9). The Layer 3 and section 6.3
property checks run without the oracle and are graded even when it is absent.

`--changed` works as it does for `differential`: a change to the parser, binder, planner or executor
reaches `inillucent-engine` and selects every `matrix` module. That is the intent. A change the
matrix could break is almost always a change to one of those crates, and the budget in 8.1 is set so
that running all of them is acceptable.

### 8.3 Inside one run

- **Shards.** A family whose case count makes it the slowest target is split with a new optional
  `shards = N` on its `[[target]]` row. The runner starts the module `N` times with
  `INILLUCENT_SHARD=i/N` in the environment, and the module keeps the cases whose id hash modulo `N`
  is `i`. This is the `slice` partition cargo-nextest recommends, done by the runner we already
  have. Each shard gets its own row in `tests/timings.toml` so longest first scheduling sees it.
- **Threads.** Inside a process the family's cases are split across libtest's threads by grouping
  them into `#[test]` functions (8 to 16 per family). Each test thread starts **one** oracle process
  and keeps it for all of its cases, opening and closing files through the protocol.
- **One failure does not stop the family.** A group runs every case and then asserts once, listing
  every failing id, as `semantics.rs::grade` does.
- **Counts, not clocks.** No test asserts a duration (rule 1.7). The budget in 8.1 is kept by a
  tooling test that asserts the number of cases each family generates at each strength against the
  numbers recorded in `crates/inillucent-compat/tests/corpora/matrix/counts.toml`. A change that
  doubles a family's case count fails that test and has to update the file, which is where somebody
  decides whether the extra time is worth it.

---

## 9. Proving the suite covers what it claims

### 9.1 Every variant has a case

A tooling suite, `tooling::matrix_inventory`, fails when something the engine accepts has no case:

- **AST variants.** A walker in `crates/inillucent-compat/src/statement_matrix/inventory.rs` parses
  every case with the engine's own parser and records each enum variant it meets. The walker matches
  each enum in `ast.rs` **with no wildcard arm**, so adding a variant to `ast.rs` stops the walker
  compiling until someone decides where the variant's case goes.
- **Registers.** The same suite reads `PRAGMA function_list`, `pragma_list`, `module_list` and
  `collation_list` from a live engine and fails for any name no case uses. Reading the live engine
  instead of a list in the test means a new function cannot be missed.
- **Syntax register.** Every `positive` statement in `compat/syntax.toml` appears in a Layer 1 case
  that runs it, and every `negative` one in a case that expects the refusal.
- **Capability rows.** Every row in `CAPABILITIES` names at least one matrix case id that exercises
  it.

The report is written to `_agent_output/matrix/inventory.md` on every run so a person can read the
coverage rather than infer it.

### 9.2 Every pair is present

The covering array builder asserts, after building, that every pair (or triple) of allowed axis
values appears in the array. That check is cheap and runs every time. `counts.toml` records the
number of cases and the number of combinations the constraints removed, per family and strength.

### 9.3 Line coverage as a finder, not a gate

Once per implementation phase, run `cargo llvm-cov` over the `matrix` tier with the
`llvm-tools-preview` component and write the uncovered functions of `inillucent-sql`,
`inillucent-engine` and `inillucent-scalar` to `_agent_output/matrix/coverage.md`. Uncovered code
in the binder, planner or executor is a missing axis value or a missing construct case. This is a
report to act on, not a threshold: a percentage threshold is met by adding cases that execute code
without asserting anything, which is the failure rule 1.1 describes. `llvm-tools-preview` is a
toolchain component and adds no crate to the workspace.

---

## 10. What happens to the suites that exist

| Suite | Change |
|---|---|
| `differential_part8` and its 1,143 cases | the cases move into `corpora/matrix/<family>/` as Layer 1 files; its `allow.list` lines move into `known.list` with their reasons; the suite is removed after the moved cases pass on the matrix runner. Expected effect: the largest target in the `differential` tier (258 s) is replaced by cases that cost milliseconds each |
| `semantics.rs` | stays. It compares shell output, which the matrix does not |
| `tools/feature-probe` | stays as the published comparison. Its 416 cases are copied into Layer 1 so they also run on every change |
| `tests/conformance/*.test` and `engine::conformance` | stay; the matrix loader reads the same format |
| `tlp_differential.rs`, `pqs_differential.rs` | stay; their predicate generators move into `statement_matrix` and both suites call them from there |
| `differential::compare` | stays; the matrix grader reuses `observe()` and the counter comparison instead of copying them |

Nothing is deleted until the moved cases have run green on the new runner, and the implementation
ticket lists the files it proposes to delete in a comment for Jason rather than deleting them, per
the repository's rule about removing files an agent did not create.

---

## 11. Implementation plan

Each phase ends with its suites registered in `tests/selection.toml`, `cargo fmt` run, and
`inillucent-testrun --changed` exiting 0 apart from the cases recorded in `known.list`.

| Phase | Work | Done when |
|---|---|---|
| 0. Measure | the case type, the oracle pool, the fixture copy, the grader over 200 cases from part 8 moved to files | the per case processor time and the fixture copy time are measured and written into section 8.1 of this document |
| 1. Runner | `statement_matrix/` module: case loader for the section 6.1 format, grader, `reopen`, `integrity_check`, `catch_unwind`, `known.list` two way check, the tiers `matrix` and `matrix_deep`, `shards` in `inillucent-testrun` | the 200 cases pass or are listed |
| 2. Layer 1 | convert `compat/syntax.toml`, move part 8, copy feature probe cases, write the missing construct cases; the inventory suite of 9.1 | `tooling::matrix_inventory` passes with no exceptions |
| 3. Layer 2 | IPOG builder with the pair check; templates for every family; stable ids; `counts.toml` | strength two runs in the change budget; strength three in the merge budget |
| 4. Layer 3 and properties | wrappings, TLP, NoREC, DQE, index and ANALYZE agreement over the Layer 2 queries | the escaped defects in section 1.1 each fail on a build from before its fix (see 11.1) |
| 5. Arms and surfaces | the six arms, `SharedDatabase`, prepared statement reuse, `execute_batch` | merge cadence under its budget |
| 6. Nightly | Layer 4 generator and shrinker, retained corpus, history rows | one nightly run recorded in `tests/nightly-history.tsv` |
| 7. Documents | this document's measured numbers; `tests/inillucent-testing-tdd.md` sections 2, 2.1, 6; `AGENTS.md` tier table; `docs/` pages that list the tiers; `agent-skills/inillucent-develop` | `node tools/doc-style/check.mjs` and `tooling documentation::` pass |

### 11.1 The suite must be able to fail (rule 1.2)

Before phase 4 is called done, the implementer checks out the engine sources from the parent of each
of these fix commits into a scratch worktree, runs the matrix there, and records which case fails:

- `7a01303c` (the todo service fixes: joins on a partly indexed `ON`, `UPDATE ... FROM`, windows in
  derived tables, JSON subtype, collation in `min`/`max`),
- `f593b836` (`WHERE` on a view's columns),
- `106b304f` (`UPDATE ... FROM` changing a row once),
- `bfb53c82` (a correlated subquery beside a window function),
- `ec3af662` (`INSERT ... SELECT` into a virtual table, `embed()` in a virtual table row),
- `8b9725ac` (bare `REINDEX` with a `WITHOUT ROWID` table).

A fix commit that no matrix case catches means an axis is missing; add the axis value, not a single
case for that defect. The table of commit, failing case id and axis goes into this document.

#### What each fix's absence fails

Measured on 2026-09-26 against commit 5ce1ef2e's matrix, with `inillucent-testrun --tier matrix`
(the change cadence) in a scratch worktree. For five commits the build is today's engine with the
fix's own source changes reversed; where a later commit had changed a file again, that file was
taken whole from the fix's parent. `ec3af662` could not be reversed that way and still compile, so
its build is the parent commit itself with today's matrix harness laid over it; that build also
lacks the 26 commits on the main branch after it, so its failures are not all this fix's, and the row names the
ones that are.

| Fix | Build | Generated cases that fail | Example case and its axis values |
|---|---|---|---|
| `7a01303c` (todo service fixes) | reversed | 495 `window`, 15 `update`, 21 `trigger`, 16 `ddl_view`, 10 `join`; the `vtab` target stops making progress and is killed at the 900 s budget | `window-09fc458677fe` (function=lead, placement=view, source=rowid): "a window function reaching the pipeline builder" is not built. `update-9ad01f041420` (form=from_tvf, target=without_rowid): `UPDATE ... FROM json_each(...)` changes 0 rows where SQLite changes 1 |
| `f593b836` (`WHERE` on a view's columns) | reversed | 43 guarded filter wrappings across `function` and `join` | `function-007a53e613d8` (function=min, source=fts5, placement=top): the guarded filter fails with "integer overflow" where SQLite never computes the guard for the rows the filter removes |
| `106b304f` (`UPDATE ... FROM` changes a row once) | reversed | 25 `update` | `update-0b63780119d7` (form=from_table, target=without_rowid): `changes()` is 3 where SQLite says 2 |
| `bfb53c82` (a correlated subquery beside a window function) | reversed | 36 `window` | `window-5e391031f1a5` (function=row_number, beside=correlated, placement=derived): refused as not built |
| `ec3af662` (`INSERT ... SELECT` into a virtual table) | parent | 4 `vector`, and the `vtab` process grows until its 8 GiB cap stops it | `vector-6f82351251c5` (operation=search_table, placement=derived): the fixture's `INSERT INTO docs(rowid, body, vector) SELECT ...` into an `inillucent_search` table is refused |
| `8b9725ac` (bare `REINDEX` with a `WITHOUT ROWID` table) | reversed, `reindex.rs` only | 13 `maintenance` | `maintenance-b9be3f42a138` (operation=reindex, index=partial): `REINDEX` fails with "the index has no catalog row" |

**`f593b836` was the one fix no generated case caught** when this was first run: only
`t2134-pd-001`, the Layer 1 case written with the fix, failed. The axis value that was missing is a
column that fails for exactly the rows an outer `WHERE` removes. The Layer 3 wrapping
`guarded_filter` in `statement_matrix/templates/frame.rs` adds it to every generated read, and the
table above is with it.

The `vtab` target's stall on the `7a01303c` build was traced, with `INILLUCENT_MATRIX_TRACE`, to `update-from-series`, a Layer 1 case from the
todo service corpus: `UPDATE ... FROM generate_series(...)` runs without end there. The statement
budget the runner arms does not stop it, because the loop does not pass through a budget check; the
runner's own 900 s budget does.

### 11.2 Bugs found

Every disagreement that is an inillucent defect:

1. gets a numbered section in the description of task-2136, "Inillucent Test Suite Bugs Found", with
   the shrunk case, the two answers and the arm;
2. gets a line in `known.list` naming that number, so the suite is green and the defect is visible;
3. is not fixed in the implementation ticket, unless the fix is a single line, in which case it is
   its own commit and the bug section says so.

The implementation ticket is done when the suite runs at all three cadences within budget, the
inventory passes, section 11.1 is filled in, and every disagreement is either fixed or recorded.

---

## 12. Risks

| Risk | What reduces it |
|---|---|
| a constraint in 4.3 silently removes the combination that has the defect | the removed count per family is recorded in `counts.toml` and changes are reviewed; Layer 4 is not constrained by the axes |
| the pinned SQLite is wrong or differs by build option | follow the program, not the documentation (dependency policy); build option differences go in `deliberate.toml` with the option named |
| generated case ids change when a template changes, orphaning `known.list` lines | ids hash the case text, and an orphaned line fails the suite, so the list is corrected in the same change |
| the change tier grows past its budget as families are added | `counts.toml` makes growth a reviewed change; `shards` spreads a large family; strength three stays at merge |
| the oracle process dies partway through | the pool restarts it and replays the case once; a second failure is reported as an oracle failure, not an engine failure, and does not pass |
| reals differ in the last place between the C library and Rust | the one unit tolerance is declared per math function in one table, and applies nowhere else |

---

## 13. Sources

- SQLite, "SQL Logic Test": https://sqlite.org/sqllogictest
- SQLite, "How SQLite Is Tested": https://www.sqlite.org/testing.html, and TH3: https://sqlite.org/th3.html
- "Understanding and Reusing Test Suites Across Database Systems" (2024): https://arxiv.org/html/2410.21731v1
- sqllogictest-rs: https://github.com/risinglightdb/sqllogictest-rs
- SQLancer: https://github.com/sqlancer/sqlancer
- Rigger and Su, "Testing Database Engines via Pivoted Query Synthesis", OSDI 2020: https://www.usenix.org/system/files/osdi20-rigger.pdf
- Rigger and Su, "Finding Bugs in Database Systems via Query Partitioning" (TLP): https://www.manuelrigger.at/preprints/TLP.pdf
- CODDTest (2025): https://arxiv.org/pdf/2501.11252
- Squirrel (CCS 2020): https://arxiv.org/pdf/2006.02398
- SQLsmith: https://github.com/anse1/sqlsmith
- DuckDB sqllogictest: https://duckdb.org/docs/stable/dev/sqllogictest/intro
- CockroachDB logic tests: https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/logictest/logic.go
- cargo-nextest partitioning: https://nexte.st/docs/ci-features/partitioning/
- NIST ACTS and combinatorial testing: https://csrc.nist.gov/projects/automated-combinatorial-testing-for-software
- Microsoft PICT: https://github.com/microsoft/pict
