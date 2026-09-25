# task-2120: the gaps a consumer found in 0.1.8

A consumer probed inillucent 0.1.8 against SQLite 3.51.0 on macOS: 165 features, 156 passed, and
three real gaps. This document checks each claim against the current `main`, says which ones are
valid, and describes the change for each valid one. Every behaviour below was run against a debug
build of this worktree before any code was changed.

## 1. What the report claims, and what was found

| Claim | Valid? | What was measured on `main` |
|---|---|---|
| Bare `REINDEX` fails on a database holding a `WITHOUT ROWID` table | **yes** | `Error [syntax]: the index has no catalog row` |
| `DELETE ... LIMIT` is refused | **yes** | `near "LIMIT": syntax error`, and `UPDATE ... LIMIT` the same |
| A scalar subquery as a value in `INSERT ... VALUES` is refused | **yes, but not where the report put it** | an ordinary table accepts it; an FTS5 table, a trigger body and an `INSTEAD OF` trigger on a view refuse it |
| `soundex()` and `load_extension()` are missing | valid, not divergences | SQLite 3.51.0 on the reporter's machine lacks `soundex` too |
| `COMMIT`, `ROLLBACK`, `RELEASE`, `sqlite_sequence` | not faults | the reporter ran one process per statement |
| `VACUUM` works | correct | the stale note is in another repository, `~/dev/quill/CLAUDE.md` |

One more defect turned up while reproducing claim 3, and it is in scope because it is the class of
fault the report complains about in claim 1: **a refusal reported under the wrong status.**

| Command | Status | Exit code |
|---|---|---|
| `inillucent exec "<statement the engine has not built>"` | `unsupported` | 3 |
| `inillucent run "<same statement>;"` | `syntax` | 1 |

`AGENTS.md` promises that exit code 3 means "this engine has not built that", so a script can
branch on it. `run` breaks that promise for every statement inside a script.

## 2. Gap 1: bare `REINDEX` and a `WITHOUT ROWID` table

### Cause

A `WITHOUT ROWID` table is its own primary key b-tree. SQLite writes no `sqlite_schema` row for
that key, and neither does inillucent. But the catalog loader in
`crates/inillucent-catalog/src/load.rs` does list the key in `TableInfo::indexes`, with
`origin: IndexOrigin::PrimaryKey` and `root` set to the table's own root, because that is what lets
the planner seek on the key.

`ImportedDatabase::reindex` in `crates/inillucent-engine/src/ddl/reindex.rs` builds its target list
from `TableInfo::indexes` and skips only `IndexOrigin::Module`. So a bare `REINDEX` reached the
primary key of the `WITHOUT ROWID` table, `rebuild_index` looked for its catalog row, found none,
and refused. The same happens for `REINDEX w` on the table itself, which the report did not try.

### Change

One predicate, `has_its_own_tree(table, index)`, decides whether an entry in a table's index list is
a b-tree that `REINDEX` can rebuild. It answers `false` for a module's index, as before, and for the
primary key of a `WITHOUT ROWID` table: `table.without_rowid`, `origin == PrimaryKey`, and
`index.root == table.root`. Both target lists in `reindex` use it.

Skipping the key is correct and loses nothing. Rebuilding it would mean rebuilding the table, and its
order cannot drift from its key, because the rows are stored and read through the same tree.
Secondary indexes on a `WITHOUT ROWID` table are still rebuilt.

### Tests

A new suite, `crates/inillucent-compat/tests/reindex_without_rowid.rs`. All three tests were run
against the original `reindex.rs` and fail there with `the index has no catalog row`:

- a bare `REINDEX` on a database holding a `WITHOUT ROWID` table with a secondary index succeeds,
  the secondary index still answers a seek, and `PRAGMA integrity_check` answers `ok`;
- `REINDEX w`, naming the `WITHOUT ROWID` table, succeeds the same way;
- after a bare `REINDEX`, closing and opening the file again reads every index through the catalog
  alone.

## 3. Gap 2: `DELETE ... LIMIT` and `UPDATE ... LIMIT`

### Why it was refused

The parser already accepts `ORDER BY`, `LIMIT` and `OFFSET` on both statements, and the syntax
register (`compat/syntax.toml`, productions `delete-stmt-limited` and `update-stmt-limited`) requires
it to. The binder then refused the statement on purpose, in the reference build's words, because the
pinned SQLite 3.53.4 used by the differential suites is not compiled with
`SQLITE_ENABLE_UPDATE_DELETE_LIMIT`. The refusal was a compatibility choice: match the build the
suite compares against.

That choice is wrong for the people using the engine. Apple's SQLite, which the reporter uses, is
compiled with the option, and so are many application builds. The batched delete loop,
`DELETE FROM t WHERE <condition> LIMIT 1000`, is the ordinary way to trim a large table without one
large transaction. An engine that accepts the grammar and refuses to run it gives the user nothing.

### What already exists

- `BoundUpdate` and `BoundDelete` already bind `limit` and `offset`.
- `inillucent_exec::dml::keys_query` already puts them on the query that finds the rows to change,
  and `keys.rs` has a test that they arrive there.
- `ORDER BY` is parsed into `ast::Update::order_by` and `ast::Delete::order_by` and then **dropped**:
  nothing binds it. With the refusal removed and nothing else changed, `ORDER BY a DESC LIMIT 2`
  would delete two arbitrary rows. This is the part that needs work.

### Change

1. **Binder** (`crates/inillucent-sql/src/dml.rs`):
   - remove the `limited_dml_refusal` call from `bind_update_body` and `bind_delete_body`;
   - bind `order_by` into a new field `order_by: Vec<BoundOrderTerm>` on `BoundUpdate` and
     `BoundDelete`. Each term is an expression over the target, bound the way an aggregate's own
     `ORDER BY` is bound, because the statement has no result columns for an ordinal to name;
   - refuse `ORDER BY` without `LIMIT` with SQLite's own message,
     `ORDER BY without LIMIT on DELETE` (or `UPDATE`), which is what a build compiled with the
     option answers.
2. **Keys query**: every place that builds the query that finds a write's rows sets
   `select.order_by` from the bound statement. That is `keys_plan` and `update_keys_plan` in
   `crates/inillucent-engine/src/engine/compiled.rs`, the two module paths there, and
   `keys_for_update` and `keys_for_delete` in `crates/inillucent-exec/src/trigger.rs`. The rows are
   then found by an ordinary `SELECT key FROM t WHERE ... ORDER BY ... LIMIT ... OFFSET ...`, which
   is exactly how SQLite implements the option: `sqlite3LimitWhere` rewrites the statement to
   `WHERE rowid IN (SELECT rowid FROM t WHERE ... ORDER BY ... LIMIT ...)`.
3. **Views**: a write to a view with an `INSTEAD OF` trigger finds its rows by running the view
   (`view_rows`). The order, limit and offset go on that query too, which is what SQLite's
   `sqlite3MaterializeView` does with them.

### The differential suites

`crates/inillucent-compat/tests/semantics.rs` has two cases, `dml.delete.limit` and
`dml.update.limit`, that expect both engines to refuse. They become `Differs` cases, with the
reason: the pinned reference is compiled without the option and inillucent runs the statement.
`docs/feature-comparison.md` records the difference in the same words.

### Tests

A new suite, `crates/inillucent-compat/tests/limited_writes.rs`, asserting values rather than
success:

- `DELETE ... ORDER BY a DESC LIMIT 2` removes exactly the two largest rows;
- `DELETE ... WHERE ... LIMIT 1` removes one of the matching rows and leaves the rest;
- `DELETE ... LIMIT 2 OFFSET 1` removes the second and third rows in key order when ordered;
- `UPDATE ... ORDER BY a DESC LIMIT 1` changes only the row with the largest `a`;
- `UPDATE ... FROM ... ORDER BY ... LIMIT` changes the right rows with the joined values;
- the batched loop: repeating `DELETE FROM t WHERE flag = 1 LIMIT 3` until `changes()` is 0
  deletes every flagged row and no other;
- `RETURNING` on a limited delete returns the deleted rows;
- `ORDER BY` without `LIMIT` is refused with SQLite's message;
- a limited delete on an FTS5 table deletes the right number of rows.

## 4. Gap 3: a subquery used as a value that nothing folds

### Cause

An uncorrelated subquery is evaluated once before a statement runs, by
`inillucent_exec::subquery::fold` (for anything planned) or `fold_expressions` (for a `VALUES` list,
an `UPDATE`'s `SET` list and a `RETURNING` list, which are not planned). A slot that nothing filled
is reported by the physical pass as "a correlated subquery used as a value", whether or not the
subquery is correlated.

`Cached::Insert` folds its `VALUES` list. Three paths do not:

| Path | Where | Example that is refused |
|---|---|---|
| an insert into a virtual table | `Cached::VirtualInsert` in `compiled.rs` | `INSERT INTO f(title, body) VALUES ((SELECT title FROM shelf LIMIT 1), 'x')` on an FTS5 table |
| an update of a virtual table | `Cached::VirtualUpdate` | `UPDATE f SET body = (SELECT ...)` |
| a statement inside a trigger body | `trigger::run_body` | `UPDATE t SET total = (SELECT sum(v) FROM t) WHERE id = new.id` |

The trigger case also reaches the `INSTEAD OF` trigger on a view, which is how the view insert in
the table above is refused.

There is a second fault on the trigger path. `run_body` hands each body statement the firing
statement's `Params`. When the firing statement folded a subquery of its own, those `Params` report
`has_subqueries()`, and `fold` treats that as "already folded" and skips the body's subqueries even
where they are planned. So a trigger's `WHEN EXISTS (SELECT ...)`, or its `DELETE ... WHERE a IN
(SELECT ...)`, works or fails depending on whether the statement that fired it held a subquery.

### Change

- `Cached::VirtualInsert` folds the `VALUES` list with the same `fold_values` that
  `Cached::Insert` uses, and passes the folded `Params` to `insert_into_module`.
- `Cached::VirtualUpdate` folds its assignments with `fold_expressions`.
- `trigger::run_body` and the `WHEN` guard in `trigger::fire` give each body statement
  `params.without_subqueries()`, which already exists for the same problem in correlated blocks.
  A planned body statement then folds its own subqueries. A body `INSERT ... VALUES` folds its
  values, and a body `UPDATE` folds its assignments after its keys are found, with the same
  functions the top level uses.

`crates/inillucent-compat/tests/compiled_chain_reuse.rs` had a test,
`a_trigger_body_subquery_is_refused_as_though_it_were_correlated`, that asserted the trigger refusal
and said in its own comment that a fix should turn it red. It is now
`a_trigger_body_subquery_is_answered_on_every_firing`, which asserts the totals the trigger writes
over two firings.

### Tests

A new suite, `crates/inillucent-compat/tests/subquery_values.rs`:

- an FTS5 insert whose value is a scalar subquery stores that value, and `MATCH` finds it;
- an FTS5 update whose value is a scalar subquery;
- an `AFTER INSERT` trigger whose body updates with a scalar subquery writes the running total;
- the same trigger fired by a statement that itself holds a subquery, which is the second fault;
- a `WHEN EXISTS (SELECT ...)` guard fired by a statement holding a subquery;
- an `INSTEAD OF INSERT` trigger on a view, fired with a scalar subquery value.

## 5. The status `run` reports

### Cause

`inillucent run` executes a script statement by statement through the shell's script runner, which
formats a failure as `Error near line N: <message>` and returns it as text. The command layer then
classifies that text, and a message it cannot place is `syntax`. The driver status of the failure
(`unsupported`, `not_found`, `constraint`, ...) is lost on the way.

### Change

The fix goes where the status is lost: the script runner keeps the driver status of the statement
that failed, and `run` reports that status and its exit code. The exact site is found while
implementing; the requirement is that `run` and `exec` report the same status and exit code for the
same failing statement.

### Tests

In `crates/inillucent-compat/tests/cli_commands.rs`, `run_reports_an_unbuilt_construct_as_unsupported`:
a script whose second statement is one the engine has not built exits
3 with status `unsupported` under `--output json`, and a script with a real syntax error still exits
1 with status `syntax`.

## 6. The capability table

The report's main complaint about the inventory is what it does not mention. Rows are added to
`drivers/inillucent-driver/src/capability.rs`, each with a probe that the capability test runs in
both directions:

| Row | Support | Probe |
|---|---|---|
| `update_delete_limit` | yes | answers the count left after `DELETE ... ORDER BY ... LIMIT` |
| `subquery_value_in_a_virtual_table` | yes | answers the value an FTS5 insert stored from a scalar subquery |
| `subquery_in_a_trigger_body` | yes | answers the total a trigger body wrote from a scalar subquery |
| `load_extension` | no | `SELECT load_extension('x')` is refused; the note says there is no C extension interface, and that FTS5 and HNSW are built in |

## 7. Not changed, and why

- **`soundex()`**: absent from the default SQLite build as well (it needs `SQLITE_SOUNDEX`), so code
  written against a stock `sqlite3` does not use it.
- **`load_extension()`**: there is no C extension interface to load into. The capability row in §6
  records this so it is written down.
- **The transaction and `sqlite_sequence` results**: they came from running one process per
  statement. `run` executes a whole script in one session, and the report shows that it works.
- **`VACUUM`**: works, and nothing in this repository claims it does not. The stale note is in
  another repository.
- **`computed_limit` and `window_in_derived_table`**: already declared `no` in the capability table
  with a workaround, and the report confirms the table is accurate. They are separate pieces of
  work.

## 8. The feature probe and the published figures

`tools/feature-probe/cases.js` holds `dml.delete.limit` and `dml.update.limit`. Both engines
refused them, so the probe counted them as agreeing. After this change inillucent answers and the
pinned build refuses, so their verdict becomes `accepted`. The probe is re-run on this branch and
`docs/feature-comparison.md` is corrected from its output: fewer cases agree byte for byte, and
"features inillucent accepts that SQLite rejects" is no longer zero. The explanation goes in the
section that says why the figure is not 100%: the two cases are a compile option the pinned build
lacks.

## 9. Release

Jason asked for this ticket to end with a release numbered **1.0.29**. After task-2115 publishes
0.1.9, this branch is rebased onto `main`, merged and pushed, and the release is cut from a separate
release worktree with `pwsh packaging/ship.ps1 -Version 1.0.29`, started with that worktree as its
working directory. Every route is checked against what the destination serves.

## 10. Verification

- `target/debug/inillucent-testrun --changed` green.
- The report's own reproductions, run through the CLI against a build of this branch, with their
  output kept in `_agent_output/task-2120-gaps/`.
- `cargo fmt`, and every new test file registered in `tests/selection.toml`.
