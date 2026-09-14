# task-1911 (part two) — completing the roadmap

The first pass of this ticket closed five items and, in closing two others, found that the roadmap's
own plan for them was aimed at the wrong code. It then filed four follow-up tickets. That was wrong:
the work belongs to this ticket, and this document is the design for finishing it here.

Four workstreams, on four crates that barely touch:

| item | what | crate |
|---|---|---|
| 5 | `write.insert.batch` — the leaf's delta area | `inillucent-tree` |
| 7 | delete the retired engine | `inillucent-compat`, the workspace manifest |
| 10 | segmented generations for the vector index | `inillucent-search`, `inillucent-core` |
| 6 | the FTS5 segment format | `inillucent-ext` |
| 3 | reuse the compiled operator chain | `inillucent-exec`, `inillucent-engine` |

Each section says what is measured now, what the change is, what proves it, and what would make it a
wrong answer rather than a slow one — because three of the five have a failure mode that returns rows
rather than an error.

---

## 1. Item 5 — `write.insert.batch`, and the O(K²) in the delta area

### What it does today

**0.50x against SQLite, interval [0.46, 0.52]** — 43.19 ms against 21.39, for 2,000 inserts in one
transaction, measured on a quiet box. The interval is tight, so this is a real gap rather than noise.

The roadmap blamed the retrieval engine's delta log and proposed finding entries by bisection. That
plan was aimed at a file the workload never opens: `write.insert.batch` inserts into `main_table`, a
plain relational table with no search index on it, so `Store::deltas_above` is never called. The seek
was built anyway, because it is right where it does apply, and it does not touch this number.

### Where the cost actually is

`crates/inillucent-tree/src/leaf.rs`. A leaf holds a sorted region and an **unsorted delta area** of
up to `DELTA_LIMIT` = 32 rows. `LeafRef::locate(key, key_columns)` walks the delta area linearly, and
for each entry calls `self.delta_value(index, column)` once per key column.

`delta_value` **re-walks the delta row from column 0 on every call**. It starts a cursor at zero and
calls `Datum::tagged_span` for every column before the one it was asked for. So a key of K columns
costs 1 + 2 + … + K span decodes per delta entry — O(K²) — and `locate` pays that up to 32 times.
`main_table` carries two secondary indexes, so a batch insert pays `locate` three times a row.

**This file already solved exactly this problem, for the other half of the leaf.** `LeafRef::key_view`
builds the key columns' accessors once and passes them down, and the comment above it says why: a
binary search "re-derived the key columns' directory entries and slice bounds from the page" per
comparison, "and on a skip scan — which searches a leaf per seek per distinct value — it was the
whole cost of the query". The delta area never got the same treatment.

### The change

`locate` decodes each delta row **once**, left to right, comparing each key column as it arrives and
stopping at the first that differs. O(K) instead of O(K²), and it stops earlier on a mismatch, which
is the common case. `delta_value` stays for its other callers.

**No page format change.** No new header field, no new array, no version bump. This is a decode-order
change inside one function, and that is the whole reason it is safe to do inside another ticket: a
change to the leaf's on-disk layout is a change to recovery, and that wants its own run at it.

### What proves it

A **decode count**, not a stopwatch. A counting harness over a leaf with a known delta area, asserting
the number of span decodes `locate` performs for a multi-column key. A count reads the same on an idle
box and a loaded one; a timing ratio does not, and this workspace already makes that argument about
its `compiles` counter.

Then the gate, `--families write --rounds 40`, before and after with a release rebuild each time, read
as the **paired ratio** against SQLite rather than the absolute times. This box moves 20% between
runs; if both arms move together that is the box.

---

## 2. Item 7 — deleting the retired engine

### What is removable

The engine that reached SQLite file format parity is still in the tree, measured between 30% and 95%
slower than SQLite, which is why the rearchitecture happened. It was blocked on the C ABI that
replaces `inillucent-capi`; `drivers/inillucent-driver-capi` is that ABI, with its own header and
conformance suite, so the block is gone.

| crate | lines | what still names it |
|---|---:|---|
| `inillucent-vm` | 16,356 | `inillucent-session`, 4 files in `inillucent-compat` |
| `inillucent-session` | 8,331 | `inillucent-legacy`, 19 files in `inillucent-compat` |
| `inillucent-capi` | 5,350 | itself, 2 files in `inillucent-compat` |
| `inillucent-legacy` | 660 | `inillucent-capi`, 15 files in `inillucent-compat` |

30,697 lines. No shipped binary, no driver and not `inillucent-engine` reaches any of them.

### What is not, and the roadmap understated this

`inillucent-storage` (13,026) and `inillucent-transaction` (5,738) **stay**. `inillucent-engine` and
`inillucent-migrate` both depend on `inillucent-sqlite-reader`, which depends on both of them and on
`inillucent-catalog`; `inillucent-catalog` depends on `inillucent-storage` too, through the `Pager`
and `BTreeCursor` its `analyze.rs`, `ddl.rs` and `load.rs` read with. **Reading a SQLite file in order
to migrate away from it is what keeps 18,764 lines of the old engine alive**, and that is a feature
rather than a leftover.

### The real work is 30 test files, and each one is a decision

Several of the 30 are **differential** suites: they run the old engine beside the new one and require
the same answer. Deleting the old engine deletes the comparison, so each case needs a decision, and
the decision is never "delete the test":

1. **Re-point at the pinned SQLite oracle.** Most of these used the old engine as a second opinion,
   and the oracle is the better one — `crates/inillucent-compat/src/differential.rs` already drives
   it. This is the default wherever the assertion is about SQL behaviour.
2. **Re-point at the new engine alone**, asserting the value the old engine used to agree about, where
   the case is about this engine's own internals.
3. **`tests/capi.rs` re-points at `drivers/inillucent-driver-capi`.**
4. **Delete a case only when its entire content was "the two engines agree"** and nothing survives —
   named in the report, with a count, because a suite that goes green by losing its assertions is what
   the testing standard rates worst.

Never an empty test, never a test that cannot fail. `tests/selection.toml` carries `covers` values
naming the deleted crates; those move to a crate that still exists or the `selection` test fails.

`policy.rs`'s `no_new_crate_reaches_into_the_retired_engine` becomes vacuous once there is no retired
engine to reach into, and has to be re-pointed or retired deliberately rather than left asserting
nothing.

### Order

Rewrite the 30 first, with the crates still present, so each suite is seen passing against its
replacement **before** the thing it compared against goes away. Remove the crates second.

---

## 3. Item 10 — segmented generations for the vector index

### What it does today

Adding content no longer rebuilds the graph — task-1894 made a commit **fold**, loading the published
generation and inserting each delta entry into it, one graph insert per row written rather than one
per row in the table. That took nine and a half minutes off an ordinary `INSERT` over 598,560
passages.

**Publishing is still proportional to the corpus.** A generation is one serialised index, held in
`%_gen(id, generation, ordinal, bytes)` as a run of chunk rows, so writing a new one reads and writes
the whole thing however few rows changed. That is why the default delta log is a share of the table,
`max(1024, rows / 8)`, rather than a constant — a constant would publish far too often — and it is why
write latency under the default still rises with the table.

`read_generation` also **scans the whole `%_gen` table and filters by generation**, which is the same
scan-instead-of-seek shape just fixed in `deltas_above`.

### The change

Many small immutable segments, merged at read time, so both the graph work and the bytes written are
proportional to the batch. This is what the systems built for this do, and the argument is theirs:
[YugabyteDB's vector LSM](https://www.yugabyte.com/blog/yugabytedb-vector-indexing-architecture/)
indexes an in-memory buffer with HNSW, flushes it to disk as an immutable chunk and fans a search
across every chunk, merging the results;
[Milvus](https://www.cs.purdue.edu/homes/csjgwang/pubs/SIGMOD21_Milvus.pdf) builds vector indexes only
over immutable segments, because a graph is expensive to update in place and cheap to build once.

1. A commit flushes its delta log into a **new segment built from just those rows**.
2. A search fans across every live segment and merges the top-k. Exact for an exhaustive scan; for
   HNSW it is the approximation every segmented vector store makes.
3. A newer segment **shadows** an older one for the same row id, and a delete is a tombstone the merge
   has to see.
4. Segments merge on a rule — FTS5's `automerge` shape, M segments at a level becoming one at the
   next — declarable rather than hard-coded.
5. `INSERT INTO t(t) VALUES('compact')` still collapses everything into one clean segment.
6. **An existing file still reads**: a `%_state` naming a single `generation` and no manifest either
   reads as a one-segment index or migrates on open.

### What would make it a wrong answer

Three things, and each is silent:

- **A deleted row coming back**, because the tombstone was in a segment the merge did not consult.
- **An updated row returning its old vector**, because shadowing picked the wrong segment.
- **An old file reading zero segments**, which answers zero rows — the exact defect this ticket has
  already fixed once, in the vector index that returned nothing after a reopen.

Each gets its own test, and each test has to fail without the change.

### What proves it

**A slope, not a point.** Write latency against corpus size, at three sizes an order of magnitude
apart, before and after. The claim is that publishing goes from O(corpus) to O(batch); one
measurement cannot show that and a pair of them at the same size shows nothing at all. If the slope
does not change, the design did not work, and that is the finding.

---

## 4. Item 6 — the FTS5 segment format

The first pass merged the dictionary row and the doclist row into one and measured it: the paired
ratio was 0.50x/0.56x before and 0.55x/0.53x after — **no change**, against a prediction of 26%. The
row count was never what this workload pays for; the bytes are.

What is left is the segment format, and it is the whole remaining gap. FTS5 here writes three trees
per document as it goes — `%_content`, `%_docsize` and the term's row. SQLite accumulates the batch in
memory and writes a handful of segment blobs at commit, merging them incrementally with `automerge`
and `crisismerge`.

**The structure is already half there.** `%_idx` is keyed `(segid, term)`, and `segid` is documented in
the source as "this index has one logical segment, so every term is in segment zero and the key is
really the term". Giving each flush its own `segid`, merging on a rule, and having the reader union
across segids is the same shape SQLite has — and the same shape item 10 needs for the vector index,
which is why the two are one idea and are designed together.

An index written before this must still read, and the dictionary is already self-describing per row
from the first pass, so the precedent for how to do that is in the file.

Measured the same way as before: a genuine A/B with a release rebuild each side, read as the paired
ratio, at two round counts.

---

## 5. Item 3 — reusing the compiled operator chain

### The prize, re-measured

`inillucent-execprofile`, both arms over the same plan, prepared stages and parameters, so the only
difference is the chain build:

| statement | rebuilt | reused | saved |
|---|---:|---:|---:|
| `SELECT 1` | 921 ns | 401 ns | 130% faster |
| a point lookup by rowid | 2,340 ns | 858 ns | 173% faster |
| a 200-row range scan | 110,759 ns | 100,042 ns | 11% faster |

The last row is the commercial one: `join.range` and `range.lookaside` are that shape and sit 14-15%
under SQLite, and `read.join` misses its family bar on the 95% lower bound alone.

### The blocker, named precisely

`physical::build_statement` already builds a chain once and rebuilds only the source. **Nothing in the
engine calls it** — `Cached::Select` goes through `run_any_prepared`, which builds afresh every time.
Its only callers are benchmarks and tests, which hold the `Statement` in a local.

It returns `Statement<'t>` holding `plan: &'t PhysicalPlan`, `catalog: &'t dyn TreeCatalog`,
`head: Box<dyn Sink + 't>` and `pool: Option<&'t Pool>`. **The catalog is the connection.** Compiled
plans already outlive a borrow of `&self` by living behind an `Rc` in
`statements: RefCell<HashMap<u64, HashMap<String, Rc<Cached>>>>`; the catalog cannot. So caching a
`Statement` on the connection is a self-referential structure — not a discipline to get right, a shape
Rust does not have.

The two candidate designs are reference-counted handles throughout (`physical.rs`, `Source<'t>`,
`Space<'_>` and the engine's `trees: HashMap<u32, PagedTree>`, whose write path takes `&mut`), or
splitting the chain into a borrow-free recipe with the borrows re-acquired at run time. **Which one,
and where the measured win actually comes from, is the question put to Fable 5.1 for guidance**, and
this section is completed from its answer before any code is written. The reason for asking rather
than starting is in the roadmap's own words: this "wants its own run at it with the discipline
designed rather than discovered".

### The discipline, whichever design wins

A cached chain that holds tree handles across executions has one failure mode and it is a panic: an
index nested loop holds a borrow of the tree it reads, the write path takes the trees mutably, and
`UPDATE t SET a = (SELECT max(a) FROM t)` reads the table it writes. The governed crates
`deny(clippy::panic)` and `deny(clippy::unwrap_used)` on any path that reads a page, so a `RefCell`
double-borrow is not an available outcome: the write path takes the borrow **fallibly** and refuses by
name.

The tests are the shapes that would panic, each asserting the **answer** rather than the absence of a
crash: an update whose value reads the table it writes, a correlated subquery over the written table,
a self-join update, and a trigger that reads its own table.

---

## 6. Documentation

Every page that states something these five change, and the site:

- `docs/roadmap.md` — items come off it; the ones that stay carry today's numbers.
- `docs/performance.md`, `docs/feature-comparison.md` — re-measured families.
- `docs/relational-architecture.md` §"Keeping a vector index current" — `compact = N` exists because
  publishing costs the corpus, and item 10 changes that.
- `docs/architecture.md`, `docs/vector-search.md` — segments, and how a search reads across them.
- `docs/repository.md` — four crates fewer.
- `AGENTS.md` and `agent-skills/` — the crate list and the search skill.
- `README.md`, `drivers/README.md`, `examples/rag-agent/`.
- **`inillucent.com`**, in the site repository: `src/data/documentation.ts` still says "The
  current distance metric is cosine", which stopped being true earlier in this ticket, and
  `src/data/content.ts` carries the headline numbers.

## 7. Review

The finished work is reviewed again and everything that review raises is addressed here, not
filed onward.
