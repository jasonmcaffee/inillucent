# task-1911 — closing the outstanding roadmap

`docs/roadmap.md` lists fifteen items and a table of seventeen failing tests. This says which of them
this ticket closes, which it deliberately does not, and what the evidence for each will be.

The order is the order the work is done in, and it is not the roadmap's order. It starts with the one
item that returns a wrong answer.

---

## 0. What is in scope and what is not

| roadmap item | what it is | this ticket |
|---|---|---|
| 14 | a vector index answers zero rows instead of the rows it holds | **fix** |
| 15 | `embed(TEXT)` is called once per row when it is a constant | **fix** |
| 13 | a registered function cannot be called from the write path | **fix** |
| 11 | a second metric on the vector index | **build** |
| 5 | `write.insert.batch`, 72% slower than SQLite | **fix** |
| 6 | `extension.fts.build`, 178% slower than SQLite | **fix** |
| 3 | the operator chain is rebuilt on every execution | **fix** |
| 2 | four per family bars are missed | follows from 3 and 6; re-measured, not separately worked |
| 1 | memory, 15% above SQLite | re-measured only |
| 7 | deleting the old engine | **recommended, not done** — see §9 |
| 4 | Linux | not measured here — see §9 |
| 8 | the retrieval index's footprint | not worked — see §9 |
| 9 | threads | not worked — see §9 |
| 10 | segmented generations | not worked — see §9 |
| 12 | a macOS archive | not possible here — see §9 |
| the failing tests | seventeen | **retire the fourteen, keep the three** |

Every item marked *fix* or *build* ends with a test that fails without the change. Every item in §9
ends with a paragraph in `docs/roadmap.md` that says what is true now, with the measurement behind it.

---

## 1. Item 14 — a vector index loses its newest writes

### What it does today

```sh
inillucent --db t.rdb batch "
CREATE TABLE t (id INTEGER PRIMARY KEY, v VECTOR(4));
INSERT INTO t (id,v) VALUES (1, x'0000803F000000000000000000000000');
INSERT INTO t (id,v) VALUES (2, x'000000000000803F0000000000000000');
INSERT INTO t (id,v) VALUES (3, x'00000000000000000000803F00000000');"

inillucent --db t.rdb exec "CREATE INDEX t_v ON t USING inillucent_hnsw (v)"
inillucent --db t.rdb query "SELECT * FROM t_v_state"     # rows 0, covered 0, ordinal 0
```

The search that answered two rows a moment earlier answers none. Nothing fails and nothing is logged.

### It is two faults, not one

`docs/roadmap.md` reads this as one fault — "the store's newest writes are not persisted" — wearing
two faces. Reproducing it says otherwise. There are two, they are independent, and each one alone
produces a subset of the symptoms.

**Fault A — `create_vector_index` never seals.** `crates/inillucent-engine/src/ddl.rs:1700`. Every
other arm of the directive dispatch ends with `self.seal()?`; this one returns `Outcome::empty()`.
`seal()` is what writes the commit record for a schema change that is its own transaction, so the
backfill is written to the log and never committed.

**Fault B — a statement's module writes are logged under a transaction number nobody commits.**
`ImportedDatabase::write` takes its transaction number with

```rust
let txn = self.next_txn.get();
self.next_txn.set(txn.saturating_add(1));
```

so for the rest of that statement `current_txn()` answers `txn + 1`. `follow_vector_indexes` runs
after the trees are no longer borrowed — deliberately, so it can undo — and reaches `change_module`,
which builds its `WalLog` with `txn: self.current_txn()`. The table's row is logged under `txn` and
the index entry under `txn + 1`, and `txn + 1` is committed by nothing.

The workspace already knows this hazard and works around it in one other place:
`undo_to_floor` takes the transaction as a parameter, and its doc comment says why —
*"the statement's own rather than `current_txn`: outside a batch `write` has already taken a number
and moved `next_txn` past it"*. `change_module` was never given the same treatment.

### Why the two faults look like one

| what is run | what survives a reopen | which fault |
|---|---|---|
| `CREATE INDEX` on a full table, on its own | nothing | A |
| `CREATE INDEX` then another statement, same batch | the backfill, not the later row | A is rescued by the next statement claiming `txn + 1`; B loses that statement's own entry |
| twenty inserts into an indexed table, one batch | nineteen | B, on the last statement only, for the same reason |
| five inserts into an indexed table, five statements | **none of the five** | B, on every one |

The last row is what the roadmap measured and what made it look like "the newest writes". It is not
the newest writes. Outside a batch **every** insert into an indexed table loses its index entry;
inside one, all but the last are rescued by the following statement re-using the number. Measured
here: five inserts through five processes leave `t_v_state` reading `rows 0` and `t_v_content`
holding 0 rows.

### The fix

**B.** A `statement_txn: Cell<Option<u64>>` on `ImportedDatabase`. `write` sets it to the number it
took, clears it on every exit, and `current_txn()` reads it before falling back to `next_txn`:

```rust
fn current_txn(&self) -> u64 {
    match self.batch.get() {
        Some(held) => held,
        None => self.statement_txn.get().unwrap_or_else(|| self.next_txn.get()),
    }
}
```

That is one number, set in one place, and it makes everything a statement reaches land in the
transaction that statement commits — `change_module` is the caller that is broken today, and it is
not the only caller of `current_txn()` inside a statement.

**A.** `create_vector_index` ends with `self.seal()?`, like every other directive. Inside a batch
`seal` is already a no-op, so this changes only the statement that stands alone.

### What proves it

`crates/inillucent-compat/tests/functions.rs` holds
`a_vector_index_does_not_survive_a_reopen`, which **asserts the bug**: three rows in the session that
built the index, none after reopening. Closing this gap is therefore a deliberate change to a named
expectation, which is what that test was written for. It is rewritten as
`a_vector_index_survives_a_reopen` and asserts the opposite, and three cases are added beside it:

1. a backfill over a full table, reopened — the rows are there;
2. inserts into an already-indexed table, **each its own statement**, reopened — every one is there;
3. a backfilled index that later takes an insert — the insert reaches it (the roadmap records that
   a backfilled index "does not recover, either", and that is Fault B again).

And the search itself, end to end: build the index over `examples/rag-agent`'s corpus and ask a
question that a scan answers, then ask it through the index, and require the same top row.

---

## 2. Item 15 — `embed()` runs once per row

`ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5` calls the model once for
every row: 65 seconds over 2,661 passages, of which 64 are 2,661 embeddings of one sentence. The
same question through a one-row subquery takes 0.9 s.

SQLite's rule is the one to copy, and it is stated plainly in
[its own documentation](https://sqlite.org/deterministic.html): *a deterministic scalar function with
constant arguments can be factored out of loops so that it is only called once per statement rather
than once per row.* `FunctionFlags::deterministic` already exists here, already carries that
sentence, and nothing reads it. `embed` is registered without it.

### The trap, and where the fold therefore goes

A compiled chain is reused across re-binds. Folding an argument that is a bound parameter at
translate time would make the chain correct only for the values it was built against —
`BoundExpr::Parameter` already carries a comment about exactly this.

So there are two folds, not one, and they happen at different times:

| the call's arguments | when it is folded | what it is folded to |
|---|---|---|
| all literals | translation, once per compile | a literal |
| reads a parameter | **execution setup, once per execution** | a literal for that execution |
| reads a column | never | left alone, evaluated per row |

The second one is the one that matters for `embed`, because the question is always `?1`. The place
for it is where the parameters are known and the chain is about to run — the same setup pass that
`literal_value_in` in `crates/inillucent-exec/src/physical.rs` already uses to fold the probe vector
of a vector index, which task-1907 gave a catalog so it could resolve `embed` at all.

`embed` is then registered **with** `FunctionFlags::deterministic`, which it has always been: the
same text through the same weights gives the same vector.

### What proves it

A counting function. A registered scalar that increments a cell, used in the same three shapes, with
the count asserted: 1 for all-literals, 1 for a parameter, *rows* for a column reference. A count is
1 in every case on an idle box and on a loaded one, which a stopwatch reading is not — the plan-cache
guard in this workspace makes the same argument about `compiles`.

Then the real one, on `examples/rag-agent`: the documented plain query and the subquery form return
the same five titles, and the plain one no longer takes a minute. That is the shape
`docs/vector-search.md` tells a reader to write, and the example's README currently has to explain
why not to.

---

## 3. Item 13 — a registered function in the write path

```sql
INSERT INTO note (body, v) VALUES (?1, embed(?1));          -- refused
UPDATE note SET v = embed(body) WHERE id = 1;               -- refused
INSERT INTO note (body) VALUES (?1) RETURNING embed(body);  -- refused
```

The refusal is `unsupported`, exit code 3, by name. The physical pass resolves a registered
function's body through the catalog; a `RowSpace` is built from a table's layout rather than from a
catalog, because it is carried through about a dozen signatures and holding a borrow would put a
lifetime on all of them. The lookup finds nothing and the translation refuses.

The fix the roadmap names is the right one: **a catalog parameter on `RowSpace::compile` and on the
callers that reach it**, every one of which already has a `Target` and therefore a
`Target::catalog()`. A parameter, not a field, so nothing gains a lifetime.

`crates/inillucent-compat/tests/functions.rs` holds
`a_registered_scalar_in_a_values_row_refuses_by_name`, which pins the refusal. It becomes
`a_registered_scalar_reaches_the_write_path` and asserts all three shapes write the vector the
function returns — read back and compared against the same call in a projection, so the test cannot
pass by writing something.

With this and item 15 in place, `docs/embeddings.md` and the search skill stop having to say
`INSERT ... SELECT` is the only shape that works.

---

## 4. Item 11 — a second metric

`ORDER BY vector_distance_l2(v, ?) LIMIT k` plans as a scan and a temporary tree, because the graph
is built over unit vectors and cosine is what it minimises. The distance functions themselves answer
for every metric; it is the *index* that has one.

pgvector spells this as an operator class. Here the spelling is
`WITH (metric = 'cosine' | 'l2' | 'inner_product')` on the index, and `metric=` in the
`inillucent_search(...)` table form, because those two already reach the same store — the index form
is a spelling of the table form, and `create_vector_index` already passes its settings through as the
store's options.

Three pieces:

1. `Options` gains `metric`, defaulting to cosine, written into `%_config` like every other option
   and read back by `reconcile`, so an index built by an older layout still reads as cosine.
2. The graph is built and probed under the declared metric. Cosine over unit vectors and inner
   product differ by the normalisation, and L2 is its own distance; `inillucent-core` already has all
   three as functions, so this is which one the builder and the search are handed.
3. The planner matches the `ORDER BY` function against the index's declared metric and only probes
   when they agree. **A mismatch falls back to the scan** — which is what it does today for
   everything but cosine, and is a correct answer rather than a refusal.

### What proves it

An index declared `l2`, probed by `vector_distance_l2`, returning the same top-k as the exhaustive
scan over the same rows; and the same index probed by `vector_distance_cos` planning as a scan, with
`EXPLAIN` asserting which of the two happened. Point 3 is the one worth a test that fails: an index
that probed under the wrong metric would return plausible, wrong rows.

---

## 5. Item 5 — `write.insert.batch`

2,000 inserts in one transaction, 72% slower than SQLite, writing 2,491 KiB of log. The delta entries
are scanned and decoded one at a time to find one. The plan the roadmap names is to keep them sorted
by their encoded key and find them by bisection.

Concretely, in `crates/inillucent-search/src/store.rs`: `deltas_above` walks the whole delta tree and
filters on `sequence > covered`. The tree is keyed by sequence and is therefore already in order, so
what it wants is a seek to `covered + 1` and a walk from there — the scan reads and decodes every
entry below the watermark on every call, and `put` calls it indirectly on every row.

Measured before and after on the `write.insert.batch` case of the gate, which is the number in the
roadmap, and on the same four consecutive runs the family bars are read over.

---

## 6. Item 6 — `extension.fts.build`

10.97 ms against SQLite's 3.68. The breakdown the gate already prints: `content` 2.1 ms, `tokenize`
0.5, `docsize` 1.6, `group` 0.2, `terms` 0.4, new terms 0.4 over 507 terms, dictionary write 2.2,
flush 3.6.

FTS5 here does four tree writes per document: `%_content`, `%_docsize`, the new term's `%_idx` row
and its `%_data` doclist. **Making the dictionary row and the doclist row one row is worth about
2.9 ms of the 10.97**, which is what takes the family over its bar at this scale.

That is the change: one row per term carrying both the dictionary entry and the doclist, rather than
an `%_idx` row pointing at a `%_data` row. It touches every reader of `%_idx` and `%_data`, which is
why it is its own piece of work rather than a tweak.

The full segment format change SQLite uses — accumulate the batch in memory, write a handful of
segment blobs at commit, merge them incrementally
([FTS5's `automerge` and `crisismerge`](https://www.sqlite.org/fts5.html)) — is a bigger change than
this ticket, and this ticket does not make it. It is left in `docs/roadmap.md` with the measurement
that says what it is worth.

---

## 7. Item 3 — reusing the compiled operator chain

Measured, both arms warmed and the order reversed: 738 ns to 358 on `SELECT 1`, 1,413 to 786 on a
point lookup, 71,672 to 59,983 on the 200-row range scan. Those last two are the 14–15%
`join.range` and `range.lookaside` sit under SQLite, and closing them is what takes `read.join` over
its lower bound.

`physical::build_statement` already holds a chain across executions and rebuilds only the source, and
nothing calls it. What stops it is ownership: an index nested loop holds a borrow of the tree it
reads, and the engine keeps its trees in a map whose write path takes them mutably.

**The discipline, designed rather than discovered.** The trees move behind `Rc<RefCell<PagedTree>>`.
A cached chain holds `Rc` clones; the write path takes `borrow_mut()` for the duration of one tree
operation and no longer. The failure mode to design against is the one the roadmap names — an
`UPDATE` that reads the table it writes — and it is a `BorrowMutError` at runtime, which is a panic,
which the governed crates forbid on any path that reads a page. So:

- every borrow is scoped to one operation and never held across a call that could re-enter;
- the write path takes `try_borrow_mut` and turns a failure into a `DbResult` refusal rather than a
  panic, so the worst case is an error with a name;
- a test drives `UPDATE t SET a = (SELECT max(a) FROM t)`, a correlated subquery over the written
  table, a self-join update, and a trigger that reads its own table — each asserting the *answer*,
  and each of which would panic today if the borrows were wrong.

This is the one item here whose risk is a runtime panic rather than a wrong number, which is why it
is worked last of the code items and behind its own test list.

---

## 8. The failing tests

Seventeen, in three groups.

| binary | count | what happens to them |
|---|---|---|
| `schema_forms.rs` | 14 | **retired.** All fourteen fail on the same step: they need the pinned `sqlite3` to read a file this engine wrote, or the reverse. Writing SQLite's file format was withdrawn as a requirement, so they are tests of something this project does not do. Two of them get past every assertion they were written for and fail only there; those two keep their assertions and lose the interoperation step, rather than being deleted. |
| `planner.rs` | 2 | kept. `sqlite_stat1` exchanged with the oracle is the same withdrawn requirement, so the exchange goes and the statistics are asserted against this engine's own catalog. |
| `ordering.rs` | 1 | kept, and **fixed or pinned**: a known difference in the order of tied rows. If the order is decidable it is made to match; if it is not, the test asserts the set rather than the sequence and says why in a comment. |

No test is deleted without its assertions being moved somewhere that still runs them. A test that
cannot fail is worse than no test, and a suite that went green by losing fourteen files would be
exactly that.

---

## 9. What this ticket does not do, and why

Each of these gets a paragraph in `docs/roadmap.md` saying what is true, with a measurement rather
than an estimate.

**Item 7, deleting the old engine.** `drivers/inillucent-driver-capi` now exists, which is what the
roadmap says `inillucent-capi` was waiting for, so the deletion is no longer blocked. It is still not
done here: an agent working a ticket does not delete files it did not create. The dependency audit —
which crates still reach `inillucent-legacy`, `inillucent-capi`, `inillucent-session`,
`inillucent-vm`, `inillucent-transaction` and `inillucent-storage`, and what breaks when each goes —
is produced and left as a task comment for a person to act on.

**Item 4, Linux.** The Linux figure is older than the Windows headline it is compared against, so the
comparison is not one. Re-measuring it needs a Linux build of the gate and a machine that is not
also running this one; doing it inside WSL on the same box would measure the contention rather than
the platform, which is the fifth of the six ways a gate reports a falsehood. It stays as it is, with
the roadmap saying plainly that the two numbers are from different builds.

**Item 8, the retrieval index's footprint.** 1.3 GB resident for a 3.1 GB index. Nothing has tried to
make the graph or the keyword postings smaller, and trying is a study rather than a fix.

**Item 9, threads.** The engine is single threaded by construction — `RefCell` in the pool and the
trees, a connection that borrows the database, no parallel scan. Item 3 moves the trees behind
`Rc<RefCell<…>>`, which is a step toward it and is not it.

**Item 10, segmented generations.** A generation is one serialised index, so publishing one reads and
writes the whole thing however few rows changed. The fix is many small immutable segments merged at
read time, which is what
[YugabyteDB's vector LSM](https://www.yugabyte.com/blog/yugabytedb-vector-indexing-architecture/) and
[Milvus](https://www.cs.purdue.edu/homes/csjgwang/pubs/SIGMOD21_Milvus.pdf) both do: an in-memory
buffer indexed with HNSW, flushed to disk as immutable chunks, with the search fanning out across all
of them. It is a storage format change and a search change together, and it is the largest single
thing left on the roadmap.

**Item 12, a macOS archive.** Every platform's archive is built on that platform and there is no
macOS build machine. Not a software problem.

**Items 1 and 2.** Memory is re-measured after the changes above, because item 3 changes what the
trees are held behind and item 5 changes how much log a batch writes. `open.prepare` and `schema`
are not moved: the roadmap's own arithmetic says their bars ask for 96 ns and for a packer costing
nothing, and a bar is not edited to meet a number.

---

## 10. Order, and what a finished item looks like

1. Item 14 — the wrong answer.
2. Item 15 and item 13 — the two `embed` gaps, together, because they meet in the same physical pass.
3. Item 11 — the second metric.
4. Item 5 and item 6 — the two write costs.
5. Item 3 — the chain, behind its borrow discipline.
6. The failing tests.
7. Re-measure, then the documents.

Each one is finished when:

- the code is in, with the doc comments and the argument for why the obvious thing is wrong;
- its test is in the file `tests/inillucent-testing-tdd.md` §2.1 names, registered in
  `tests/selection.toml`, and **fails without the change**;
- `target/debug/inillucent-testrun --changed` is green;
- `cargo fmt` has run;
- and every document that states a number this change moved has been re-read, not just the roadmap.

The last one is the one that gets skipped. `docs/performance.md`, `docs/vector-search.md`,
`docs/embeddings.md`, `examples/rag-agent/README.md`, `examples/rag-agent/AGENTS.md` and
`agent-skills/` all currently describe behaviour that items 13, 14 and 15 change — the example's
README explains at length why not to build the index and why not to write the documented query, and
both of those explanations stop being true.
