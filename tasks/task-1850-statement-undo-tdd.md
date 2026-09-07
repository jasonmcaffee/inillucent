# task-1850 — A statement that fails partway keeps the rows it already wrote

> **Implemented.** The plan below is what was built; the section at the end records the three
> additions the work turned up and everything that was measured.

## Introduction

SQLite's default conflict algorithm is `ABORT`, and `ABORT` means **the current statement's changes
are undone and the transaction is kept**. This engine undoes nothing. A statement that writes three
rows and fails on the fourth leaves the three, reports an error, and commits them — half a statement,
durably, with a diagnostic that says it failed.

`OR ROLLBACK` has the second half of the same hole: it raises the constraint failure and leaves the
open transaction exactly where it was, so the rows the transaction had already written survive and
the next `COMMIT` succeeds where SQLite says `cannot commit - no transaction is active`.

Both are one missing thing: **there is no statement boundary in the write path**. The undo machinery
exists — `Engine::undo_to`, the `Before` buffer, `SAVEPOINT`/`ROLLBACK TO` — and it is scoped to a
*transaction*. Nothing takes a mark when a statement starts, so nothing can put a statement back.

## What the probe found

54 cases through the pinned SQLite 3.53.4 shell and ours, both directions, in
`_agent_output/task-1850-statement-undo/` (`probe.cjs`, `cases.json`, `baseline.txt`):
**20 agreed, 34 differed.** The ticket describes two of them. The classes:

| class | cases | shape |
|---|---|---|
| no statement undo | 17 | the rows written before the failure are kept |
| `OR ROLLBACK` does not roll back | 8 | plus: the transaction stays open and its savepoints survive |
| a constraint's own `ON CONFLICT` is ignored | 4 | a **refusal of a statement SQLite performs** |
| `changes()` / `total_changes()` are not SQL functions | 2 | out of scope, filed separately |

The undo class is much wider than `UPDATE` and uniqueness. It reaches:

- `DELETE` — `DELETE FROM t` with a `BEFORE DELETE ... RAISE(ABORT)` on the third row deleted the
  first two; SQLite keeps all three.
- `STRICT` typing — `INSERT INTO t VALUES (1),('x'),(3)` kept the `1`.
- `INSERT ... SELECT`, not only `VALUES`.
- foreign keys, in both directions — a `DELETE` a child row restricts had already removed the other
  two parents, and an `INSERT` of three children with a dangling third kept the first two.
- a trigger's **side** writes — `AFTER INSERT ... INSERT INTO log` left three log rows behind a
  statement that failed.
- `RAISE(ABORT)` from a trigger body, and `OR ABORT` written out.

`OR ROLLBACK`'s class likewise covers `INSERT OR ROLLBACK`, `NOT NULL ON CONFLICT ROLLBACK` written
on a column, `UNIQUE ON CONFLICT ROLLBACK` written on one, and `RAISE(ROLLBACK)` in a trigger.

The third class is the both-directions half the ticket did not name. Its table says `IGNORE` and
`REPLACE` are correct — they are, at the *statement* level. Written on the **constraint** they do
nothing at all: `CREATE TABLE t(a TEXT UNIQUE ON CONFLICT IGNORE)` raises on a duplicate where
SQLite skips the row, and `ON CONFLICT REPLACE` raises where SQLite replaces. `conflicting_row`
never reads `IndexInfo::conflict`.

## Goals and Non-Goals

**Goals**

1. A DML statement that fails undoes what it wrote, and the transaction around it survives —
   SQLite's `ABORT`, which is what every unqualified statement gets.
2. `FAIL` and `ABORT` are distinguishable: a multi-row statement failing on its third row keeps the
   first two under `OR FAIL` and keeps none under the default.
3. `OR ROLLBACK` discards the open transaction — its rows, its savepoints and the transaction
   itself — and `autocommit` reads as SQLite's does afterwards.
4. `RAISE(ABORT)`, `RAISE(FAIL)` and `RAISE(ROLLBACK)` in a trigger body each do what they say; today
   all three are `RAISE(ABORT)`, and `RAISE(ABORT)` does not abort.
5. A constraint that carries its own `ON CONFLICT ROLLBACK` or `ON CONFLICT FAIL` — written on a
   `NOT NULL` or on a `UNIQUE` — undoes what it says, with the statement's own `OR` clause
   overriding it. This is the `ROLLBACK`/`FAIL` half only; see the Non-Goals for the rest.
6. The undo reaches everything the statement wrote, including a trigger body's writes to other
   tables and every index entry, and `check_trees` passes after each one.
7. `write.update.indexed`, `txn.large` and `txn.batched` stay inside the run-to-run spread on a
   medium `inillucent-fullgate`, with the numbers quoted. `txn.autocommit` is the workload that
   newly pays and is quoted too.

**Non-Goals — measured, evidenced, and filed as follow-ups rather than fixed here**

- `changes()` and `total_changes()` do not exist as SQL scalar functions. They answer `0` on success
  as well as on failure: `INSERT INTO t VALUES (1),(2),(3); SELECT changes(), total_changes();`
  answers `0|0` where SQLite answers `3|3`. That is a missing function, not an undo defect. The
  C-API counters `dml_differential.rs` compares *are* wired, and the same follow-up owns their
  failure-path arithmetic (SQLite reports `changes()` = the kept rows after `OR FAIL`; we report 0).
- `CONSTRAINT c CHECK(...) ON CONFLICT FAIL` is a **parse error** — `near "ON": syntax error`. The
  conflict clause on a `CHECK` is not in the AST, the catalog or the loader, so honouring it is a
  four-crate change rather than a line. It is a refusal rather than a wrong answer.
- `UPDATE OR REPLACE` writing NULL into a `NOT NULL` column that has a `DEFAULT` raises where SQLite
  substitutes the default. The defaults are applied by the binder and there is no compiled default
  at the constraint check, so this needs the binder to hand them over.
- **A constraint's `ON CONFLICT IGNORE` and `ON CONFLICT REPLACE` are ignored**, so
  `CREATE TABLE t(a TEXT UNIQUE ON CONFLICT IGNORE)` raises on a duplicate where SQLite skips the
  row. That is a *resolution* decision — which arm of the conflict handler runs — and it is a binder
  defect that happens to be reachable through conflict handling, not the missing statement boundary
  this ticket is about. Filed as its own high-priority ticket with the `CHECK ... ON CONFLICT` parse
  error and the `OR REPLACE` default substitution above, and the probe harness goes with it.
- The pre-existing red binaries in the workspace suite (4 of 196 at `8894378`).

## Design

### The statement boundary is `Engine::write`

`Engine::write` already wraps every DML statement: it opens the logs, builds the `WriteView`, calls
the closure that applies the statement, and commits when the statement is its own transaction. It is
the one place that knows both *this is one statement* and *here is the undo buffer* — so it is where
the mark is taken.

```rust
let mark = self.undo.borrow().len();
let (applied, wrote) = { /* WriteView, apply(), logs.wrote() */ };
let changes = match applied {
    Ok(changes) => changes,
    Err(error) => return Err(self.abandon(error, mark, autocommit, wrote)),
};
```

`abandon` reads what the failure says it undoes and does exactly that:

| what the error says | what `abandon` does |
|---|---|
| `Unwind::Nothing` (`FAIL`) | nothing; in autocommit it still commits, because the statement failed and its transaction did not |
| `Unwind::Statement` (`ABORT`, and every untagged error) | `undo_to_floor(mark)` |
| `Unwind::Transaction` (`ROLLBACK`) | `Engine::rollback()`, which already undoes to floor 0 and ends the transaction |

The failure path never commits for `ABORT` or `ROLLBACK`. It does not have to: the restores are
ordinary logged writes and no `Commit` record follows them, so a recovery replays neither the
statement nor its undo and lands on the state before the statement — which is the state the undo
just produced in the pool.

### Undo is now collected outside a transaction too

`WalLog::undo` is handed `Some(&self.undo)` only when a transaction is open, on the reasoning that
"an autocommit statement cannot be abandoned". That is the assumption this ticket falsifies: an
autocommit statement is abandoned by every `ABORT`, which is most of them.

So the buffer is always handed in, and an autocommit statement **clears it when it ends** — on
success in `write`, and in `abandon` after the undo. The buffer therefore holds at most one
statement outside a transaction, which is the same bound it had at zero.

What that costs, precisely, is one `TreeLog::undo` call per row per tree:

- inserting a key that was **absent** copies the key columns and no row — `write_row` computes
  `previous` as `None` and there is nothing to clone;
- replacing a key that was **present** copies the whole previous row, which is the real cost, and
  is paid by `UPDATE` and by `INSERT OR REPLACE`.

Inside a transaction nothing changes: those workloads already pay it. `write.update.indexed`,
`txn.large` and `txn.batched` are `Grouping::Single`, `Single` and `Every(10)` — all inside a
transaction, all already collecting. The workload that newly pays is `txn.autocommit`, and it is
measured below rather than assumed.

The ticket's cheaper shape — "record the undo LSN at statement start and roll forward only on the
failure path" — is what this *is* inside a transaction: `mark` is an integer read off a `Vec`'s
length, and the success path does nothing with it. It cannot be the whole answer outside one,
because outside one there is no buffer to hold a mark into.

### What a failure undoes, said by the failure

`ABORT`, `FAIL` and `ROLLBACK` reach the same `return Err(...)` in `dml.rs` and differ only in what
the layer above undoes. The layer above therefore has to be told, and the error is what travels
between them. A new field on `ErrorContext`:

```rust
pub enum Unwind {
    /// The statement's own writes go back; the transaction stays — SQLite's ABORT.
    Statement,
    /// Nothing goes back — SQLite's FAIL.
    Nothing,
    /// The statement's writes and the whole open transaction — SQLite's ROLLBACK.
    Transaction,
}
```

`None` reads as `Statement`, so **every error in the engine that was never taught about conflict
algorithms gets ABORT**, which is the right default and the reason the STRICT, foreign-key and
trigger cases above are fixed without touching their raise sites.

It is set with `or_unwind` — *set if absent* — and the precedence falls out of the order the sites
run in, innermost first:

1. `RAISE(ROLLBACK|FAIL|ABORT)` in a trigger body tags at evaluation. An explicit `RAISE` beats
   the enclosing statement's `OR`, which is SQLite's rule.
2. A constraint site tags with `statement.or(constraint)` — it has both in hand.
3. `dml::insert` / `update` / `delete` tag the whole call with the statement's own `OR`, catching
   every error nothing more specific claimed.

`Unwind` is deliberately **not** part of `DbError`'s `PartialEq`. Equality there is over what the
error *says*; this is about what the engine does with it, and folding it in would make an error
tagged at a raise site unequal to the same error written out in a test.

### A constraint's own `ON CONFLICT`, for the unwind only

`UNIQUE ... ON CONFLICT ROLLBACK` and `NOT NULL ... ON CONFLICT ROLLBACK` are family 2 — they are
`OR ROLLBACK` written on the constraint instead of on the statement, and they fail the same way for
the same reason. So `Conflict` gains the clause the constraint that reported it carried
(`IndexInfo::conflict`), and the two raise sites tag the error with
`unwind_of(statement.on_conflict.or(clash.conflict))`. `declarations_are_met` already computes that
same `statement.or(constraint)` for `NOT NULL` and only needed the tag.

**What does not change is which arm runs.** `resolution` keeps reading the statement's clause alone,
so a constraint's `ON CONFLICT IGNORE` or `REPLACE` still raises. That is the Non-Goal above and its
own ticket: it is a decision about *resolving* the conflict rather than about undoing the statement,
it is in the binder's and the catalog's half of the problem, and its sibling — a conflict clause on
a `CHECK` — needs grammar. The field this ticket adds is what that ticket will read, so the two are
adjacent rather than entangled.

## Test plan

`crates/inillucent-compat/tests/new_engine_writes.rs`, which applies each statement to both engines
and re-probes every index against its table afterwards, plus `check_trees` per statement.

1. Each of the ticket's seven repros answers as SQLite does.
2. `FAIL` and `ABORT` are distinguishable on the same statement: three rows, the third colliding —
   two kept under `OR FAIL`, none under the default.
3. `OR ROLLBACK` discards the transaction: the rows it had written are gone, the savepoints inside
   it are gone, the following `COMMIT` fails the way SQLite's does, and `autocommit` reads true.
4. A statement failure inside a `SAVEPOINT` leaves the savepoint standing and undoes only the
   statement.
5. `RAISE(ABORT|FAIL|ROLLBACK)` from a trigger, and a trigger's writes to another table undone with
   the statement that fired it.
6. A `UNIQUE ... ON CONFLICT IGNORE|REPLACE|FAIL|ROLLBACK` constraint, and the statement's `OR`
   overriding it.
7. The whole 54-case probe re-run and quoted.
8. `write.update.indexed`, `txn.large`, `txn.batched` and `txn.autocommit` on a medium
   `inillucent-fullgate`, before and after.

---

## What was built, and what it measured

Implemented as designed, with three additions the work turned up.

**1. `RAISE(ABORT)`, `RAISE(FAIL)` and `RAISE(ROLLBACK)` were one thing.** The physical pass built
`Expr::Raise { code, message }` and dropped the action past `IGNORE`, so all three compiled to the
same node — and it was the node that did not abort. The action is now carried and pinned, because a
`RAISE` beats the statement that fired the trigger, which is the one place SQLite's precedence is not
innermost-first. That needed a second setter (`with_raised_unwind`) and a third
(`with_outer_unwind`) for the outermost statement's `OR`, which beats a nested statement's and a
constraint's.

**2. `COMMIT` and `ROLLBACK` with nothing open, and `BEGIN` with something open, all succeeded
silently.** SQLite refuses all three. This is not the missing statement boundary, but it is the only
way SQL can *observe* that an `OR ROLLBACK` ended the transaction — the acceptance criterion could
not be tested without it, since a `COMMIT` that succeeds after the transaction is gone looks exactly
like a transaction that is still there. The engine's own `commit_batch`/`rollback` stay tolerant;
only the three statements refuse.

**3. The autocommit undo buffer costs nothing measurable**, which the design allowed for but did not
assume. See the gate below.

### The probe

`_agent_output/task-1850-statement-undo/`, 54 cases through the pinned SQLite 3.53.4 shell and ours,
both directions.

| | agreed | differed |
|---|---|---|
| before (`8894378`) | 20 | 34 |
| the statement boundary alone | 32 | 22 |
| plus the `FAIL`/`ROLLBACK`/`RAISE` tags | 46 | 8 |
| plus the three transaction-statement refusals | **48** | **6** |

The six that remain are the two families ruled out of scope, and they are on their own tickets: a
constraint's `ON CONFLICT IGNORE`/`REPLACE` being ignored, `CHECK ... ON CONFLICT` not parsing, and
`OR REPLACE` not substituting a `DEFAULT` (**task-1853**); `changes()`/`total_changes()` not existing
as SQL functions (**task-1854**).

**The 20 that agreed before the change are the control.** A harness that manufactured differences
would have differed everywhere, and 34 of 54 is otherwise a number a reader is right to be suspicious
of. Six of the 20 are deliberate negative controls — an ordinary successful campaign, a committed
transaction, an explicit `ROLLBACK`, `OR IGNORE`, `OR REPLACE`, `OR FAIL` — and all six still agree
after the change.

### A second sweep, for the edges the first one did not reach

Fifteen more cases (`cases-extra.json`), aimed at the places a statement undo could plausibly be
incomplete rather than absent: `AUTOINCREMENT` and `sqlite_sequence` after a failure and under
`OR FAIL`, a `WITHOUT ROWID` table on both the insert and the update path, a row carried in and out
of **three** indexes at once with every one of them re-probed, a `DELETE` stopped by a trigger with a
`UNIQUE` index to put back, a `TEMP` table, an upsert's `DO UPDATE` arm colliding with a third row,
`RELEASE` after a failed statement, nested savepoints with a failure between them, two failures in a
row inside one transaction, a failure followed by an explicit `ROLLBACK`, a `STORED` generated
column, and a row whose value is large enough to be stored out of line.

**Thirteen agree.** The two that do not are both pre-existing and neither is about undo:

- **`last_insert_rowid()` answers `0` always**, like `changes()` and `total_changes()` — the same
  missing scalar, added to **task-1854**. Its failure-path arithmetic is the interesting part and is
  recorded there: SQLite answers `7` after `INSERT INTO t VALUES (7,'r'),(8,'p')` fails on the second
  row, so the counter keeps a rowid the statement assigned and then undid.
- **A `DESC` index inverts its own range bounds** — `SELECT count(*) FROM t WHERE c >= 10` answers 1
  of 3 rows. Reproduced on a script with no failure, no conflict and no transaction in it, and
  identical on the release shells built at `8894378` and here, so it predates this ticket entirely.
  It is the read-path defect task-1849 named and closed for *imported* indexes by dropping them;
  `CREATE INDEX ... DESC` in this engine builds one anyway and the planner then reads its direction
  off a catalog the tree disagrees with. Filed as **task-1855**.

The large-value case is worth naming as a pass rather than a line in a list: a row spilled out of
line is restored by `put`, which re-spills it, and the undo record carries the whole row rather than
the reference — so a statement that half-wrote a three-kilobyte value puts it back intact, and the
probe reads its length and prefix back to say so.

### The tests fail without the fix

Seven tests added to `new_engine_writes.rs`. Proved rather than assumed: `git worktree add --detach`
at `8894378`, the test file copied in and nothing else, and the run is **14 passed, 7 failed** — one
failure per new test. On the working tree it is **21 passed, 0 failed**. The release binaries say the
same thing directly: the ticket's first repro answers `11|1  2|2  12|3` from the release shell built
at `8894378` and `1|1  2|2  12|3` from the one built here, which is SQLite's answer.

### The gate

Medium `inillucent-fullgate`, 30 rounds, page size 32768, 4096 frames, three runs per arm,
**interleaved** before/after so a drift on the box cannot read as an effect of the change. Each run
over its own copy of the fixture. The numbers are **inillucent's own median nanoseconds**, not the
ratio against SQLite: the SQLite arm moved between runs by more than the change did (`point.rowid`
read 72.3 ms on one run and 55.0 ms on another), so a ratio here would be reporting the box.

| workload | before (ns, three runs) | after (ns, three runs) |
|---|---|---|
| `write.update.indexed` | 68,675,300 / 68,237,250 / 66,413,150 | 65,824,200 / 65,532,700 / 66,179,800 |
| `txn.large` | 3,418,250 / 3,070,200 / 3,118,350 | 2,973,750 / 3,009,550 / 3,130,150 |
| `txn.batched` | 44,811,500 / 43,526,900 / 43,332,350 | 45,077,000 / 45,743,550 / 42,465,250 |
| `txn.autocommit` | 21,150,700 / 20,423,750 / 19,567,700 | 19,222,200 / 20,112,600 / 19,625,150 |
| `write.insert.batch` | 36,647,700 / 34,571,350 / 34,450,150 | 32,848,500 / 32,917,500 / 33,109,100 |
| `write.delete` | 17,651,300 / 13,783,700 / 14,160,350 | 13,556,700 / 13,883,550 / 14,217,850 |
| `write.upsert` | 2,634,500 / 2,415,300 / 2,282,400 | 2,232,700 / 2,374,200 / 2,738,550 |

Every after-range overlaps its before-range. The headline is `4.19x / 3.94x / 4.06x` before and
`3.99x / 4.05x / 3.99x` after, both arms `MET` against the 3.00x bound, and every workload `agreed`
on both arms.

**`txn.autocommit` is the one that newly collects before-images, and it did not move.** The reason is
visible in `write_row`: a log that wants undo needs the row that was there, and an `UPDATE` was
reading it anyway — `want_previous` was already true on that path. What the change adds outside a
transaction is the `Before` record itself: one `Vec<OwnedDatum>` moved into a buffer that is cleared
at the end of the statement, and for an `INSERT` of a key that was absent, a copy of the key columns
and no row at all. That is below this gate's run-to-run spread.

### The workspace suite

**196 binaries, 4 red, 18 tests** — the same four as the pristine-HEAD baseline
(`harness`: the task-1818 retrieval baseline; `ordering`: seven `ORDER BY` tie-break orders;
`planner`; `schema_forms`: fourteen, including `EXPLAIN` having no bytecode to list). None of them
is about writes, conflicts or transactions.

One binary was red on the first run and is not on the second: `policy`'s
`the_governed_crates_are_formatted`, because the new code needed `cargo fmt`. Worth recording that
`cargo fmt --all` reformats nineteen files in `inillucent-bench`, `inillucent-core` and `drivers`
that are unformatted at HEAD and are nothing to do with this ticket; those were put back, and only
the eight files this ticket touches are formatted.
