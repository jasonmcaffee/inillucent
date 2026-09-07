# task-1856 — six Inillucent bugs, folded into one ticket

Six tickets were filed separately and combined here: task-1848, task-1851, task-1852, task-1853,
task-1854 and task-1855. Their descriptions were copied into task-1856 and the originals deleted, so
this document and that ticket are the whole record.

Four further defects were found while measuring these, and each is fixed here with its own account
below: a rowid alias's `PRIMARY KEY ON CONFLICT` clause was never read, `INSERT OR REPLACE` deleted
only the first row in its way, `ON CONFLICT ... DO NOTHING` silenced a `CHECK` and a `NOT NULL`, and
`random()` answered one constant for the life of the engine.

Everything below was measured against the pinned SQLite 3.53.4 oracle. Where a number is quoted it
was taken on this box on 2026-09-07.

---

## task-1855 — a `DESC` index inverts its own range bounds

### What it was

```sql
CREATE TABLE t(id INTEGER PRIMARY KEY, c INTEGER);
CREATE INDEX ic ON t(c DESC);
INSERT INTO t VALUES (1,10),(2,20),(9,90);
SELECT count(*) FROM t WHERE c >= 10;   -- sqlite: 3    ours: 1
```

A silent wrong answer on an ordinary `SELECT`. `c >= 20` and `c <= 20` both answered correctly by
coincidence, which is what kept it hidden: with three rows an inverted bound selects the same count.

### Why the two halves disagreed

The planner reads a key column's direction from the **catalog**, and the new engine's trees store
every key column ascending. task-1849 closed that for an *imported* index by not importing one:
`is_ascending` dropped it and named it in `ImportedDatabase::skipped`. `CREATE INDEX ic ON t(c DESC)`
built one anyway, and the planner then drew all three of its conclusions against a tree that
disagreed with the catalog — the range bounds, whether an ordering was already provided, and which
way to walk.

So the import's answer (drop it) and `CREATE INDEX`'s answer (build it and lie about it) were
different answers to one question.

### What was done

The mismatch is removed at its source rather than patched three times. `stored_ascending` in
`inillucent_catalog::paged` records what the tree actually is — every key column ascending — on every
index the paged engine derives, and the planner therefore reasons about the tree that exists.
`index_shape` no longer disqualifies such a tree from being "already sorted", because it is sorted;
and the import **keeps** a descending index instead of dropping it, since `in_key_order` already
re-sorts SQLite's entries into this tree's own order.

The index is now *usable* rather than absent, and `ORDER BY c DESC` over it is a reverse walk, which
the planner already asks for when a term's direction and the key's disagree.

**Nothing observable is lost.** The `CREATE INDEX ... DESC` text is stored and returned by
`sqlite_schema` verbatim, and this engine's `index_info`/`index_xinfo` do not report a direction
column at all. Storing a genuinely descending key column is a format change — the key encoding, the
leaf comparisons and every scan — and belongs to whichever phase decides to pay for it.

### How it is graded

- four `desc.*` cases in `semantics.rs`, over **nine** rows so that an inverted bound cannot select
  the right count by coincidence: the bounds, the orderings, a compound `(a DESC, b)` key, and the
  write path maintaining a `UNIQUE` descending index;
- `the_import_keeps_a_descending_index_and_answers_over_it`, which grades the imported half and the
  created half over the same twelve questions and asserts the index is not in `skipped`;
- `an_order_by_over_a_descending_index_is_not_reversed`, which from task-1849 until now was passing
  because `members_score` was not in the file at all, and is now grading a real index.

---

## task-1853 — a constraint's own `ON CONFLICT` clause

Every case here is a **refusal of a statement SQLite performs**, which is the direction that is easy
to miss: a user hits an error where the reference writes the row.

### 1 and 2 — `ON CONFLICT IGNORE` and `ON CONFLICT REPLACE` did nothing

`a TEXT UNIQUE ON CONFLICT IGNORE` puts the algorithm on the *constraint*, so a plain `INSERT` that
collides on `a` skips that row and writes the rest. The engine read only the statement's own `OR`
clause, so a four-row insert wrote nothing.

`resolution_for(statement, clash.conflict)` replaces `resolution(statement)` on the insert path and
`resolution_of(statement.on_conflict.or(clash.conflict))` on the update path. The precedence is
SQLite's throughout: an `ON CONFLICT ... DO UPDATE` beats everything, then the statement's `OR`, then
the constraint's clause, then `ABORT`. `write_one`'s single-probe fast path is entered only when the
table's own key carries no clause of its own — the probe has not happened yet there, so the only
clause it can consult is the key's.

### 3 — `CHECK ... ON CONFLICT` was a parse error

```
CREATE TABLE t(a TEXT, b INTEGER, CONSTRAINT small CHECK(b < 9) ON CONFLICT FAIL);
Parse error near line 1: near "ON": syntax error, expected )
```

The widest of the four, because the `CREATE TABLE` failed and every statement after it said
`no such table` — and the table's `CREATE` text is stored and re-parsed on every open, so a grammar
that cannot read it back is a database that cannot be opened.

**Two things were measured rather than assumed, and both changed the answer:**

- SQLite's `ccons` has **no** `onconf`, so the same clause on a *column*-level `CHECK` is a syntax
  error there. Accepting it here would be a statement this engine takes and the reference refuses,
  so the clause was added to `TableConstraint::Check` only.
- SQLite's `tcons ::= CHECK LP expr RP onconf` parses the clause and never reads it. Measured: with
  `ON CONFLICT FAIL` on a table `CHECK`, an `INSERT` of three rows whose second fails keeps **none**
  of them, which is `ABORT`. So it is recorded on `CheckInfo::conflict` and not acted on, and the
  measurement is written down beside the field.

### 4 — `OR REPLACE` did not substitute a `DEFAULT` for a NULL

SQLite's rule for a `NOT NULL` violation resolved as `REPLACE` is to store the column's `DEFAULT`,
falling back to `ABORT` only when there is none. The binder now hands over `not_null_defaults` —
bound, so `DEFAULT (3+4)` stores 7 rather than the text of it — and `declarations_are_met` fills the
row in place. A default that is itself NULL is no default, which is graded.

### The four, before and after

| script | sqlite | before | after |
|---|---|---|---|
| `UNIQUE ON CONFLICT IGNORE`, four-row insert | `10\|p 20\|q 30\|z 50\|r` | refused | agrees |
| `UNIQUE ON CONFLICT REPLACE`, same | `10\|p 20\|q 40\|z 50\|r` | refused | agrees |
| `CONSTRAINT small CHECK(b < 9) ON CONFLICT FAIL` | accepted | parse error | agrees |
| `UPDATE OR REPLACE t SET a='x', c=NULL` | `x\|d` | refused | agrees |

---

## task-1854 — `changes()`, `total_changes()` and `last_insert_rowid()`

### The success path

All three exist as scalars in the dialect and all three answered `0`, for every statement, for ever.
An application reading `changes()` from SQL to decide whether an `UPDATE ... WHERE` matched anything
— the ordinary way to do an optimistic update — concluded that nothing matched. The C API was
already right, which is what made it invisible.

The counters travel on the **parameter set**, which is the route a folded subquery already takes and
for the same reason: a plan is cached by its text, so a value baked into the plan would answer with
whatever was true when it was first compiled. `Expr::Call` carries them the way `Expr::Time` already
carried `now`, and reading them once per statement is not an approximation — SQLite moves the
counters when a statement *finishes*.

### The failure path

This needed the count to **survive the error**, which the `Changes` a write builds does not: it is
gone the moment the statement raises. The tally lives on the `WriteTarget` instead, which outlives
it, and `Engine::write` reads it on both paths. `FAIL` keeps the rows it wrote and everything else
puts them back, so the tally is taken only for `FAIL`.

`last_insert_rowid()` is the one that is *not* put back: SQLite documents it as the last rowid
attempted, so a statement that wrote a row and then undid it still moves it.

| statement | sqlite | before | after |
|---|---|---|---|
| `UPDATE t SET id=id+10 WHERE id<=2` (aborts) | `0 \| 3` | `0 \| 0` | `0 \| 3` |
| `UPDATE OR FAIL t ...` (keeps row 1) | `1 \| 4` | `0 \| 0` | `1 \| 4` |
| `INSERT INTO t VALUES (7,'r'),(8,'p')` (fails on the second) | rowid `7` | `0` | `7` |

The two counters are different questions and SQLite answers them differently, which was measured
rather than assumed: an `INSERT` of two rows with an `AFTER INSERT` trigger that inserts one row each
answers `changes() = 2` and moves `total_changes()` by **4**. A `ROLLBACK` does not put
`total_changes()` back.

`compare_with_counters` in `differential.rs` returned early on a failed statement and so graded
neither. That early return is narrowed: the rows are still not compared — there are none — and the
counters and the autocommit flag now are.

---

## task-1851 — `integrity_check` over an index that disagrees with its table

task-1849's defect left a database that `PRAGMA integrity_check` declared healthy: index `u` held two
entries under `'x'`, `SELECT count(*) FROM t WHERE a='x'` answered 2 from the index and 1 from the
table, and the checker saw nothing. That write is closed; the detector is this.

`PagedTree::check` is about **one tree in isolation** — its key order, its sibling chain, its
separators — and every one of its checks passes over such a database. `check_indexes_agree` asks the
other question, in SQLite's own wording, because an application matching on `integrity_check`'s
answer is matching on that text:

- `non-unique entry in index <name>` — two entries under one key in a `UNIQUE` index. The entries
  are in key order, so it is one comparison against the previous entry, not a second pass. A prefix
  holding a NULL is skipped: every NULL is distinct, which is the same rule `distinct_prefix`
  applies on the write path.
- `row <rowid> missing from index <name>` — a table row whose entry is not there, which is also what
  an entry whose *key* does not match its row looks like: the recomputed key is not found.
- `wrong # of entries in index <name>` — an entry naming a row the table does not hold.

**A partial index and an index on an expression are checked for uniqueness only.** Deciding which
rows should have an entry means evaluating a predicate and deciding what a key should be means
evaluating an expression; both need a binder the checker does not have, and counting them as though
every row had an entry would report a healthy partial index as damaged.

The damage is built by **writing the index tree directly**, through
`ImportedDatabase::write_index_entry_unchecked`, because no SQL statement can produce it any more —
that is what task-1849 means, and it is why a checker for these states cannot be exercised through
SQL. A detector that has never been shown the damage it looks for is a detector nobody has tested.

The false-positive half is graded against the oracle over the shapes that would trip a careless
checker: two NULLs in a `UNIQUE` column, a partial index, an index on an expression, a compound index
over a `WITHOUT ROWID` table, and writes over all of it.

---

## task-1848 — the driver's `connect_as`

`inillucent_driver::Connection<'d>` borrows its `Database`, so a long-lived object cannot hold both:
that is a self-referential struct and Rust will not have it. A consumer in that position holds the
`Database` and connects per call, and every one of those was a new session — so a `CREATE TEMP TABLE`
typed into a query console was gone by the next statement, an `ATTACH` did not outlive its own call,
and a connection pragma had to be re-applied every time.

`Database::connect_as(session)` and `Connection::session()` mirror what the engine has had since it
grew sessions.

**The C ABI turned out to be the broken consumer, not a hypothetical one.** `inillucent_conn` held
only a database pointer and every entry point called `database.connect()` — a new session per call —
because a C handle cannot hold a borrow. The handle now opens a session at `inillucent_connect` and
every call continues it. No symbol changed, so `abi.toml` is untouched.

`suite.json` gains `a_temp_table_survives_a_connection_per_call` and a new case key
`"connection": "per_call"`, which asks a runner to open a fresh connection for every statement over
one session. The Rust runner honours it explicitly; the Python runner satisfies it by doing nothing,
because the C ABI's connections already work that way — which makes the Python run the end-to-end
proof. `drivers/README.md` gains a "Sessions, and a connection per call" section.

---

## task-1852 — `extension.fts.build`

### Profiled before planned, which is the rule Part E states

The build path is instrumented permanently, the way `create_index`'s stages are, and the gate prints
the breakdown beside the ratio. A workload that has been under the floor for two tickets should not
make a third re-derive where its time goes.

**Before**, 500 documents at medium:

```
500 rows, content 1.4 ms, tokenize 1.2 ms, docsize 1.0 ms, group 0.4 ms,
          terms 3.5 ms, totals 0.0 ms, flush 1.2 ms            9.67 ms, 0.31x
```

Splitting `terms` found the answer, and it was not the shape a guess would have picked: **2.8 of the
3.5 ms was the 507 terms the transaction met for the first time**, against 0.4 ms for the 4,000 it
had already seen. Splitting *that* gave `dict write` 1.8 ms and `dict read` 0.5 ms — one keyed insert
into `%_idx` per new term, as the terms arrived.

### What was changed, each measured

1. **One lock instead of three on the cached-term path**, and no allocation to describe the
   occurrences. `term_row`, `append_staged` and `buffer_is_full` each locked the buffer, and a
   `Vec<(usize, Vec<u32>)>` was built per term per document to describe occurrences that are written
   out as varints. `append_in_one_lock` does the whole of it in one critical section, reading the
   postings straight out of the sorted run. `a_run_appends_the_bytes_the_entry_would` checks the two
   writers against each other, because a doclist written two ways is a format with two definitions.
2. **The dictionary rows are staged and written in term order at the flush.** `%_idx` is an index
   tree keyed by the term and the terms of a document arrive in whatever order the text put them in.
   Every reader that *scans* `%_idx` flushes first: the prefix search, `integrity`, the delete-all
   command; `remove` drops a staged row rather than deleting one the table does not hold yet.
3. **A term whose dictionary row was just created no longer has its doclist looked up**, because the
   answer cannot be anything but absent — and `max_rowid` is not asked once the buffer's mark is
   above the table's.
4. **The tokeniser allocated a `Vec<char>` per character.** `fold` returned one; `fold_into` writes
   into the token instead. The shape stays one-to-many — the German sharp s lowercases to `ss` — it
   is the collection that went. 1.2 ms to 0.3 ms.
5. **A shadow write copied every text and every blob twice**, once into an `OwnedDatum` and once
   again when the tree borrowed it back. On `%_data` the blob is a term's whole doclist.
   `as_datums` borrows from the caller's values.

**After**:

```
500 rows, content 1.5 ms, tokenize 0.4 ms, docsize 1.0 ms, group 0.2 ms,
          terms 0.3 ms, new terms 0.4 ms, dict read 0.1 ms,
          dict write 1.7 ms, flush 2.8 ms                       7.90 ms, 0.37x
```

### What the profile says about the rest of the gap, plainly

Writing the dictionary in perfect key order still costs about 3.2 µs a row against 2.4 µs for a
`%_data` row, so the per-write cost is the engine's and not the order's. This engine's own
`write.insert.batch` is 15.7 µs a row; a shadow write at 2.5-3 µs is not slow.

FTS5 here does **four tree writes per document** — `%_content`, `%_docsize`, the new term's `%_idx`
row and its `%_data` doclist — where SQLite's fts5 accumulates the batch in an in-memory hash table
and writes a handful of segment blobs at commit. Closing the remainder is a segment format change,
not a micro-optimisation, and it would touch every reader of `%_idx` and `%_data`.

### The gate

`inillucent-fullgate` medium, 30 rounds, page size 32768, 4096 frames, **four consecutive runs**,
each over its own copy of the fixture — `schema.index` leaves an index behind on the SQLite arm, so a
shared copy would make run 2 measure a different table.
(`_agent_output/task-1856/gate/four-final/`.)

| | run 1 | run 2 | run 3 | run 4 |
|---|---|---|---|---|
| `extension` ratio | 1.18x | 1.21x | 1.23x | 1.26x |
| **`extension` low bound** | **1.05x** | **1.07x** | **1.07x** | **1.09x** |
| `extension.fts.build` | 0.35x | 0.37x | 0.37x | 0.37x |
| weighted geomean | 3.94x | 4.10x | 4.07x | 4.06x |
| **weighted low bound** | **3.79x** | **3.94x** | **3.96x** | **3.93x** |
| the floor line | above | above | above | above |

**H5 is met.** Every run prints `every required family is above it`, the `extension` low bound is at
or above 1.00x on all four, the weighted lower bound is at or above 3.00x on all four, and no other
family's low bound is below 1.00x — `open.prepare` 1.05-1.10, `schema` 1.10-1.23, `transaction`
1.03-1.10.

Against task-1846's four-run baseline, whose `extension` low bounds were **0.99, 1.00, 0.99, 1.06**
with `extension.fts.build` at 0.29x-0.30x. The bound is no longer straddled: the smallest of the four
is 1.05x where the largest of the four before was 1.06x.

The `MISSED` verdicts beside `open.prepare`, `schema` and `extension` are against those families'
*bars* — 5.00x, 3.00x and 1.50x — which is a different question from the floor and was MISSED on the
baseline's four runs too. That is why the run still ends `gate: NOT MET`; the floor line above it is
the one this ticket is about.

**Nothing in `compat/perf/contract.toml` was touched** — not a bar, not a weight, not a fixture.

The full gate was run twice over four runs each: once on the tree as the FTS work left it
(`four-a/`, `extension` lows 1.01, 1.08, 1.08, 1.08) and once again after task-1851's checker was
rewritten (`four-final/`, above), because a change landing after a measurement makes the measurement
about a tree that no longer exists.

---

## The four defects this ticket found on its own

Each was measured against the oracle and fixed here.

1. **A rowid alias's `PRIMARY KEY ON CONFLICT` clause was never read.** `rowid_conflict` read the
   column's `not_null_conflict` — the clause of a *different* constraint, one the table need not even
   declare — so `id INTEGER PRIMARY KEY ON CONFLICT REPLACE` raised. `ColumnInfo` gains
   `primary_key_conflict`, filled from the column-level `PRIMARY KEY` and from a table-level
   `PRIMARY KEY(id)` over a rowid alias, which makes no index to carry it.
2. **`INSERT OR REPLACE` deleted only the first row in its way.** One image can collide with a
   *different* row on each of two unique indexes; `update_at` had always looped and `write_one` asked
   once and then wrote, leaving the second collision standing.
3. **`ON CONFLICT ... DO NOTHING` silenced a `CHECK` and a `NOT NULL`.** The upsert clause is about a
   key collision on a named target; SQLite raises for a false predicate or a missing value, and this
   engine skipped the row and reported success. The declaration checks read the statement's own `OR`
   algorithm now, so `INSERT OR IGNORE` still skips both and `DO NOTHING` no longer does.
4. **`random()` answered one constant**, `-2152535657050944081`, in every statement of every
   connection since the new engine existed: the function library was called with a default context
   and the default seed is zero. One number is a legal answer to one call and a wrong answer to two.
   Each call site now has its own stream, advanced per row.
