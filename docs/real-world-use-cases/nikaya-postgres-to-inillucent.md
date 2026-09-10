# Nikaya: removing PostgreSQL from a 5.8 GB Gmail assistant

Nikaya is a local Gmail retrieval assistant — every message and attachment pulled down, normalized,
chunked, embedded, and searchable three ways through a web UI and an MCP tool surface. It has run on
PostgreSQL 17 with pgvector since it was built. In September 2026 it stopped.

This is what that took, on a real mailbox rather than a fixture: **64,378 messages, 66,793 documents,
602,022 chunks, 12,485 attachments, 5,852 MB of PostgreSQL**. It is written for someone deciding
whether to do the same thing, so it leads with what went wrong.

**Read section 2 as history.** Both failures it describes are fixed: the recovery ordering that
corrupted a database when a `CREATE TABLE` followed a reopen went in task-1888, and each is now
pinned by a test named where it is described. The section is kept rather than deleted because what
it says about *how* a corruption of that shape is diagnosed — restoring log segments one at a time
and reading after each — is the part worth having, and because a report that quietly dropped its
worst finding once it was fixed would be a report nobody could trust the rest of.

Everything here was measured on the box it ran on, a Windows 11 workstation with 127.5 GB of memory
and an NVMe SSD, while several other things were running. The arms are interleaved for that reason.

---

## 1. What actually moved

Nikaya's *retrieval* was already inillucent: an earlier move put the vector and keyword branches
into the in-process `inillucent-core::Index` and measured recall@100 going from pgvector's 0.899 to
1.000. What had not moved was the **record** — documents, chunks, participants, attachments, jobs, sessions, and
the vectors themselves — which was still PostgreSQL reached through `sqlx`, with the retrieval index
as a projection of it.

So this is the second half: 16 tables, 1,626,000 rows, and the 19 files that touched `sqlx`.

| | before | after |
|---|---|---|
| record | PostgreSQL 17 + pgvector, over a socket | one `.rdb` file, in process |
| durable vectors | `halfvec(768)` in `chunk_embedding` | `VECTOR(768)` in the same file |
| retrieval | `inillucent-core::Index` | unchanged |
| processes to run | PostgreSQL, the embedder, the server | the embedder, the server |
| on disk | 5,852 MB | 7,422 MB |

The file is **larger**. PostgreSQL compresses large values in
TOAST; this engine does not. The vectors also widen from `halfvec`'s two bytes an element to `f32`'s
four — 925 MB becomes 1,850 MB — which is offset by 1,811 MB of HNSW and roughly 900 MB of GIN index
that are not carried at all.

---

## 2. The two most important things that happened

Both are the same failure wearing different clothes: a committed write produces a log record that
replay turns into a database which cannot read its own rows. One arrived through a routine deploy.

### A schema migration corrupted the live corpus

This is the one to read if you read nothing else. Two commands, nothing between them:

```
> inillucent --db nikaya.rdb query "SELECT id, kind FROM document ORDER BY id LIMIT 3"
0000bf8b-ac16-4aa3-ade1-a5305fcb71e3  email      (and two more)

> inillucent --db nikaya.rdb exec "CREATE TABLE document_index_queue (
    document_id TEXT PRIMARY KEY REFERENCES document(id) ON DELETE CASCADE,
    queued_at INTEGER NOT NULL)"
ok. 0 rows changed.

> inillucent --db nikaya.rdb query "SELECT id, kind FROM document ORDER BY id LIMIT 3"
Error [corrupt]: database disk image is malformed
```

The new table is not mentioned by the statement that fails. What breaks is narrow and repeatable:
`count(*)` still answers 66,793, `SELECT id` alone still works, **any other column of `document`
does not**, and `chunk` is unaffected. That points at one table's row decoding rather than at page
damage.

It reached the live database through the ordinary deployment path. The server applies pending
migrations at startup, so deploying a binary that added one table corrupted the corpus, silently:
`migrate` reported success and the damage appeared on the next read of a document.

The damage is in the log rather than the file, which is established by restoring log segments one at
a time and reading after each:

```
checkpoint only              reads=True   66,776 documents  601,862 chunks
+ segments 317 through 333   reads=True   66,793            602,022
+ segment 334                reads=False
```

Segment 334 is the one holding the `CREATE TABLE`. Every earlier one replays cleanly. So the recovery
is to park that single segment, and **nothing is lost**: the database read back 66,793 documents,
602,022 chunks, 602,022 vectors and 64,378 live messages, including a sync that had run that morning.

The consequence for the application was larger than the incident: **Nikaya could not add a table.**
The queue that would remove its worst remaining regression was written, reconciled against the real
corpus and deliberately not shipped, because shipping it corrupted the corpus.

#### Fixed in task-1888, and this is what it was

Recovery collected the `AllocPage` records it replayed into one list and the `FreePage` records into
another, then claimed every page in the first list and released every page in the second. The frees
therefore had the last word whatever order the log put them in — so a page **freed and then allocated
again inside the replayed range** came back from the recovery marked free while it was live. The next
allocation was handed a page something else already owned.

The corpus above is where it was diagnosed. Segment 318 of Nikaya's log holds, in order,
`AllocPage 211519`, `FreePage 211519` and `AllocPage 211519` again. After the reopen, one
`CREATE TABLE` took pages 211519 and 211520 for its two roots and wrote over a `document` row's
6,040-byte out-of-line value. That is exactly the shape reported above: `count(*)` answers, a
key-only projection answers, and a projection that decodes every column does not.

It is silent at write time, which is why nothing caught it earlier: the statement that takes the page
reports success, and nothing is wrong until something reads a row whose value lived there. A free-map
bit carries no LSN, so nothing below recovery can catch a wrong answer about which pages are free.

Recovery now replays the two record kinds **in log order**, so the last record about a page decides.
`crates/inillucent-compat/tests/new_engine_free_map_recovery.rs` pins it, with a second arm that frees
nothing after the checkpoint and therefore cannot disagree — which is what makes the first arm a
diagnosis rather than a guess: the variable is the page freed and taken again inside the replayed
range, not the checkpoint, the reopen, the wide value or the `CREATE TABLE`.

`ANALYZE` reached the same ending through `sqlite_stat1`, which is a `CREATE TABLE` wearing a
different hat, and is covered by the same fix.

### A killed writer left the database unopenable

**A killed writer left the database unopenable.**

Filling a new column across 601,862 rows was going to take an hour, so the process was killed. After
that:

```
Error [io]: could not open "J:\nikaya-data\nikaya.rdb": database disk image is malformed:
           replaying a compaction of leaf 237505 could not fit its 2031 rows
```

The file was 7.4 GB with 1.07 GB of write ahead log across 24 segments. Moving the log segments aside
let the file open at its last checkpoint with every row intact — **the `.rdb` was fine and the redo
log was what could not be replayed**, which is the opposite of what a redo log is for.

Two things follow, and both belong in anyone else's plan:

- **The recovery cost ten minutes instead of hours of GPU, and only because the staged copy was
  kept.** The migration produces an intermediate `.rdb` before the native schema is built. The plan
  said copy, verify, never delete, and the reason to keep a 25 GB file nobody was going to read again
  is precisely this. PostgreSQL was also still running and still untouched, so there were two ways
  back rather than none.
- **`integrity-check` reported `ok` on that same file.** A check that answers ok about a database
  nothing can open is worse than no check, because it is the first thing an operator reaches for.

---

## 3. Seven ways the obvious SQL breaks or goes quadratic here

The migration was not mostly about types. It was about query plans. Each of these was written the
natural way and was catastrophically slow — one of them did not run at all — and every one of them
looks fine on a small fixture.

### `LIMIT` does not bound the work

The standard keyset page — `WHERE key > ?1 ORDER BY key LIMIT 2000` — costs the rows *after* the key,
not the limit. Measured on a 60,000 row fixture, the same statement at four positions:

| starting after | elapsed |
|---|---:|
| row 1 | **161.8 ms** |
| row 20,000 | 115.3 ms |
| row 40,000 | 73.2 ms |
| row 58,000 | **12.7 ms** |

The time *falls* as the key advances. Walking a table that way is quadratic: 601,862 chunks at a page
of 2,000 is 90 million row materialisations instead of 601,862, and the first adoption attempt read
865 MB/s for twenty-five minutes without finishing one table.

**The fix is two statements per page.** The upper bound comes from a key-only statement, which the
index answers and `LIMIT` does bound; the rows then come from `key > ?1 AND key <= ?2`, bounded at
both ends:

| | start of table | middle | end |
|---|---:|---:|---:|
| one statement with `LIMIT` | 161.8 ms | 115.3 ms | 12.7 ms |
| key scan, then a closed range | **11.1 ms** | **11.4 ms** | **11.9 ms** |

Flat, and 15x faster at the start of a table a fifth the size of the real one.

### A composite key range is a full scan

Writing that closed range over `(document_id, ordinal)` needs `(a > ?1 OR (a = ?1 AND b > ?2))`, and
that is planned as `SCAN` plus a temporary B-tree for the ordering. Paging on the leading column alone
and cutting pages at document boundaries is an index seek.

### `IN` is a full scan where `=` is a seek

`WHERE id IN (?1, …, ?20)` on a primary key is planned `SCAN`; `WHERE id = ?1` is an index seek. A
page of twenty search results scanned all 66,793 documents and the two gigabytes of text hanging off
them. Measured on a 60,000 row fixture: **295.7 ms for an `IN` list of twenty keys** against about a
millisecond for twenty seeks. Every list lookup became a loop of point reads.

### `LEFT JOIN` scans the inner side, `INNER JOIN` seeks it

```
SELECT count(*) FROM chunk c LEFT JOIN chunk_embedding e ON e.chunk_id = c.id
--   SCAN c USING COVERING INDEX sqlite_autoindex_chunk_1
--   SCAN e

SELECT count(*) FROM chunk c JOIN chunk_embedding e ON e.chunk_id = c.id
--   SCAN e USING COVERING INDEX sqlite_autoindex_chunk_embedding_1
--   SEARCH c USING COVERING INDEX sqlite_autoindex_chunk_1 (id=?)
```

Same key, same tables. The outer join is a nested loop with the inner side scanned, which on 601,862
rows is 601,862 squared. `NOT EXISTS` is planned no better. This is what made a Gmail sync sit on one
core for nine minutes without leaving its first phase.

The anti join answers "which chunks have no vector yet", so it had to go. A flag column was tried and
failed twice — see §4 — and the answer is a **queue table**, written and drained inside the same
transactions that write a chunk and store its vector.

### A join on a computed key has no plan at all

`document.provider_id = 'attachment:' || attachment.id` is how an attachment's extracted text is
linked to its bytes. PostgreSQL hashed it. Nothing can index a key built while the query runs, and the
best remaining plan reads every attachment document for every attachment link: 21,591 × 2,341 = **fifty
million reads** of a table whose rows carry message bodies. Two cheap reads and a hash map in Rust.

### A partial index is never chosen

Reproduced on three rows:

```sql
CREATE INDEX t_pending ON t (done) WHERE done IS NULL AND gone IS NULL;
EXPLAIN QUERY PLAN SELECT id FROM t WHERE done IS NULL AND gone IS NULL;
--   SCAN t
```

Its own predicate, character for character. A plain index on the same column is used. Nikaya's two
work queues were partial indexes carried over from PostgreSQL, so the ingestion pass scanned all
66,793 documents to find a backlog that is normally empty: **1,280 ms** per check.

---

### A compound query cannot be used as a derived table

`SELECT ... UNION ALL SELECT ...` runs. `SELECT ... FROM (that) alias` does not:

```
Error: the new engine's physical pass does not handle a compound query yet
```

That was the shape of the query behind every attachment card, so **`GET /api/documents/{id}` — the
entire document detail view — answered 500 for every document in the corpus**, and it survived the
migration undetected until the very end. Worth dwelling on why: the record benchmark measured a
statement of its own that happened to be the first arm of the union, and the retrieval harness stops
at the search results. Neither built an HTTP request. The pass that found it drives the real routes
against a copy of the real corpus, and finding it took thirty seconds once that existed.

Split into two statements, which is the better shape anyway — the arms answer different questions,
and a document is one kind or the other.

## 4. Two more that shaped the design

**A set based `UPDATE` filling a new column can be unrepresentable.** After
`ALTER TABLE chunk ADD COLUMN embedded_at INTEGER`, filling it in one statement fails:

```
Error: the mini-columns do not fit in one page
```

Filling it row by row instead is 601,862 seeks and rewrites of a row over a kilobyte wide, measured at
1 MB/s — and that is the run whose kill left the database unopenable. Both failures pushed the design
to the queue table, which needs no mass write at all: it starts empty, and empty is correct because
every chunk in the corpus already has a vector, which the reconciliation checks rather than assumes
(601,862 chunks checked, 0 added).

**`ANALYZE` fails on the real database.**

```
> inillucent --db nikaya.rdb analyze
Error [syntax]: page is not a blob extent
```

after 3.4 seconds. It does not reproduce on a fixture of sixty 200 KB values, so it needs the shape of
a real table. The consequence is the one that hurts: **with no statistics the planner has no
selectivity to work from and falls back to scans**, which is why a plain index on a column that is
NULL for 15 rows out of 66,793 is still planned as `SCAN document`.

---

## 5. The measurement

Interleaved, three arms, over one shared sample of identifiers drawn once and read by both sides. Two
rounds; the median of the rounds is reported so a round that was slow because something else on the
box was busy is one observation rather than a weighting. PostgreSQL is measured twice because
`plan_cache_mode` is the difference between the number it gives a benchmark and the number it gives a
deployment.

### The record layer, p50

Measured twice, before and after the engine took the fixes these findings produced. Both columns are
from interleaved runs over the same shared sample, so the box's drift between the two sittings falls
on the PostgreSQL side as well and the ratio is what to read.

| operation | PostgreSQL | inillucent | |
|---|---:|---:|---|
| `doc.attachments` | 0.325 ms | **0.273 ms** | 1.19x faster |
| `doc.email` | 0.237 ms | 0.267 ms | 1.13x slower |
| `attachment.text` | 0.336 ms | 0.399 ms | 1.19x slower |
| `doc.participants` | 0.300 ms | 0.387 ms | 1.29x slower |
| `doc.load` | 0.169 ms | 0.423 ms | 2.5x slower |
| `thread.load` | 0.506 ms | 1.960 ms | 3.9x slower |
| `labels.distinct` | 44.7 ms | 92.5 ms | 2.1x slower |
| `status.counts` | 364 ms | **5,670 ms** | 15.6x slower |
| `index.pending` | 0.609 ms | **2,970 ms** | 4,900x slower |

**Point reads are within a small factor either way. Anything that aggregates or scans is worse, and
two are much worse.**

`index.pending` asks which documents still need indexing, and the answer is normally none. It is
planned `SCAN document` — over a table whose rows carry message bodies — because `indexed_at IS NULL`
is not a seekable predicate here, and a partial index whose `WHERE` clause matches the query character
for character is not chosen either. The engine's later fixes moved it from 3,303 ms to 2,970 ms,
which is not the kind of change that matters.

The application's answer to that is the same one it used for the embedding backlog: stop asking. A
queue table written by the transactions that create the work and drained by the ones that finish it
turns the empty case into one seek. It is written, it reconciles against the real corpus correctly
(66,793 documents checked, none queued), **and it is not shipped**, because the `CREATE TABLE` that
created it corrupted the database. That was section 2, and task-1888 fixed it.

`status.counts` is five `count(*)` over 600,000 row tables. PostgreSQL spreads that across parallel
workers; this engine walks a covering index on one core. There is no application-side trick for it
short of caching the counters, which trades a correct number for a fast one.

`corpus.scan20k` is excluded rather than reported as a ratio: the PostgreSQL side counts a subquery,
so the server never sends the 20,000 vectors anywhere, while the inillucent side builds 20,000 chunk
values with their 768 floats each. Each is a real cost on its own side, and they are not the same
work.

### Retrieval, against pgvector, at matching answer quality

The first version of this section compared a search through the whole service — embed the query,
search the index, read twenty documents out of the record — and reported 71 to 76 ms. That is the
right number for how long a search takes and the wrong one for comparing two stores, because two of
the three are not the store. It is replaced here.

What is timed is `index.search` and nothing else. The thirty probes are embedded once, before the
clock starts, and written to a file that the PostgreSQL arm reads, so both engines search for exactly
the same points and the embedder is out of the comparison rather than assumed to cancel. Six arms,
interleaved, two rounds, median of the rounds, first pass of each discarded. k = 20 documents.

**Recall is reported beside latency, because without it the latency means nothing.** An approximate
index is fast in proportion to how much of the corpus it declines to look at. The answer key is an
exhaustive scan — and both engines' exhaustive scans were run, because a key produced by one side of
a comparison is a key worth doubting. They agree on **0.992** of the answer, the remainder being the
1,433 chunks on tombstoned documents that PostgreSQL still holds and the 160 chunks from a sync it
never received.

| | p50 | p95 | peak resident | recall@20 |
|---|---:|---:|---:|---:|
| PostgreSQL + pgvector, `hnsw.ef_search` 40 — **its default** | 3.71 ms | 6.55 ms | | **0.468** |
| PostgreSQL + pgvector, `ef_search` 100 | 10.99 ms | 20.61 ms | | 0.672 |
| PostgreSQL + pgvector, `ef_search` 400 | 18.12 ms | 36.31 ms | | 0.876 |
| PostgreSQL + pgvector, `ef_search` 1000 | 28.08 ms | 52.47 ms | | **0.929** |
| PostgreSQL + pgvector, index off — exact | **5,506 ms** | 14,752 ms | | 1.000 |
| inillucent graph, vectors **filed** (the default) | **4.22 ms** | 6.19 ms | **2,080 MB** | **0.926** |
| inillucent graph, vectors resident | 3.82 ms | 5.75 ms | 3,839 MB | 0.926 |
| inillucent exhaustive, filed — **what Nikaya deploys** | **19.03 ms** | 23.70 ms | **2,080 MB** | **1.000** |
| inillucent exhaustive, resident | 17.79 ms | 21.69 ms | 3,840 MB | 1.000 |

Three things fall out of that table.

**At matching recall, inillucent is about seven times faster.** 4.22 ms against 28.08 ms, both
finding roughly 93% of the exact answer. Comparing 4.22 ms against pgvector's 3.71 ms would be
comparing against an answer that is missing more than half of what the query asked for.

**pgvector's default is the number most people will measure.** `hnsw.ef_search` defaults to 40, and
at 40 it returns 0.468 of the exact answer on this corpus. Nothing warns about this: the query is
fast, the rows come back, and they look like results. Raising the SQL `LIMIT` does not help — the
index has already stopped producing candidates — which is why a sweep from 100 to 3,000 candidates
moved the clock by less than a millisecond and was the thing that gave the cap away.

**Exact answers are a different scale entirely.** Nikaya deploys the exhaustive path deliberately,
because on a mailbox the cost of missing the one message you are looking for is higher than 15 ms.
That scan reads every one of 600,589 vectors in 19 ms; PostgreSQL's equivalent, with the index turned
off, takes **5.5 seconds**. The difference is that one of them is parallel and quantised and the
other is a single-threaded sort.

### What holding the vectors in memory buys

With the embedder and the record reads out of the measurement, the residency difference is finally
visible, and it is small:

| | filed (the default) | resident | cost |
|---|---:|---:|---:|
| exhaustive scan, p50 | 19.03 ms | **17.79 ms** | +1,760 MB for **6%** |
| graph, p50 | 4.22 ms | 3.82 ms | +1,759 MB, inside the round-to-round spread |

The exhaustive path gains about 6%, consistently across both rounds, which makes sense: it reads
every vector, so where the vectors are matters. The graph path touches a few hundred of them and the
difference there changes sign between rounds.

So the default changed for 1.76 GB against 6% on one path and nothing on the other. The same bytes
are in memory either way while they are being used — the difference is whether they are reclaimable
page cache or a heap allocation this process will never give back. The steady state is what is
measured here; the first search after a reboot pays a read of the vector file that the resident arm
paid at open instead.

### Everything else that was timed

| | |
|---|---|
| PostgreSQL to the staged `.rdb`, 16 tables, 1,625,817 rows | **542 s**, peak 4.7 GB |
| staged copy to the native schema, with conversions | **489 s**, peak ~2.2 GB |
| verification against the staged copy | **36 s** |
| retrieval index rebuild, 600,429 chunks, 19.8 M edges | **154 s** (read 84 s, commit 63 s, save 4 s) |
| server start, including the reconciliation against the record | **123 s** |
| one incremental Gmail sync, 17 messages, 160 chunks embedded | **~6 min** |

---

## 6. Did the corpus survive

Four checks, each answering something the previous one could not.

1. **PostgreSQL to the staged copy.** `inillucent migrate`'s own: a row count taken from the server by
   a separate query, and an order independent digest computed on the way in and recomputed by reading
   the published file back. 16 tables, 49 checks, **0 failures**.
2. **Staged copy to the native schema.** Counts per table, then a sample compared field by field:
   2,000 documents, 2,000 vectors **element by element with exact equality**, 2,000 label arrays,
   2,000 timestamps re-converted rather than read back. **0 differences.** Exact equality rather than
   a tolerance, because a tolerance is what would let a subtly wrong vector through.
3. **The record against itself.** 601,862 chunks, 601,862 with a vector, 66,776 documents — the same
   counts PostgreSQL reports. The retrieval index holds 600,429, and the difference is exactly the
   1,433 chunks belonging to 118 tombstoned messages, which the index excludes by design.
4. **Retrieval.** The 30 checked-in probes, asked of an index built from PostgreSQL and an index built
   from the new record. In **semantic only** mode — which depends on the vectors and nothing else —
   **28 of 30 were identical in order and membership**, and every document in the two that differed
   that was not in the old answer had been *created by a sync that ran after the old index was
   saved*. The vectors came across exactly.

5. **The application.** Every HTTP route driven against a copy of the corpus with the shipped
   binary: the session gate refusing an unauthenticated read, login, `/api/status`, the label
   drawer, all three search modes, a document, its thread, an attachment's record, its extracted
   text and its bytes out of the object store, then logout and the gate refusing again. 37 checks.
   The six MCP tools driven over stdio as an agent session drives them. **This is the check that
   found the 500s**, and the other four could not have: none of them builds a request.

Hybrid answers differ more (18 of 30 identical, 6 reordered, 6 with a boundary swap) and that is
expected rather than concerning: hybrid fuses BM25, whose scores depend on corpus statistics, and the
new index holds 1,869 more chunks. That is why the check that matters was run in semantic mode.

**The sync afterwards**, which is what proves the write path rather than the read path:

| | before | after |
|---|---:|---:|
| email documents | 64,365 | **64,378** |
| documents | 66,776 | **66,793** |
| chunks | 601,862 | **602,022** |
| chunks with a vector | 601,862 | **602,022** |
| tombstoned | 118 | **122** |
| embedding queue | 0 | **0** |

`17 messages fetched, 4 tombstoned, 17 documents chunked, 160 chunks embedded, 17 index documents
updated, 4 removed` — Gmail to record to vectors to index, on inillucent, including an append to an
index whose vectors are in a file.

---

## 7. What it is like to operate now

- **One file is one process.** `nikaya-server status`, `list-users` and `sync` answer `database is
  locked` while the server is running. With PostgreSQL they ran alongside it. Anything the CLI used to
  do either goes through the server's own routes or waits for it.
- **The page cache is a real setting.** The engine's default of 128 MiB against a 6.5 GB file meant
  almost every point lookup was a page fault: 881 MB/s of reads sustained for minutes on a file that
  only needed to be read once. `NIKAYA_DB_POOL_MB` sizes it, defaulting to 1 GiB, and a rebuild uses
  more. `PRAGMA cache_size` is accepted and ignored, so it has to be set at open.
- **A deploy leaves every agent's MCP transport on the old binary.** An MCP process is spawned once
  per agent session and lives as long as that session; two on this box had been up since August. They
  keep answering from whatever code existed when they started — including the `retrievalEngine` field,
  which is the one field that is supposed to tell you what you are running on.
- **Object paths in the record are absolute**, which predates this migration and did not change
  with it, but it is the thing that makes a data root not copyable: the verification instance
  answered 404 for every attachment download until its object store was a junction back to the
  original directory. Worth knowing before you copy a corpus to another machine.
- **The way back is one environment variable.** PostgreSQL, pgvector and the sealed connection URL
  are untouched.

## 8. Would it be worth doing again

For this application, yes, and the reason is not the record layer — that is a wash on point reads and
worse on aggregates. It is that a retrieval assistant with an embedded store is **one process and one
file**: no server to keep running, no extension loaded by absolute path, no connection URL that has to
be right before anything works, and a corpus that can be handed to another machine by copying it.

It is not worth doing on the strength of the numbers in §5 alone, and anyone reading this to justify
a migration should read `index.pending` and `status.counts` first. Two of the planner problems in §3 are
things an application can work around, four of them changed the shape of the code permanently, and
one of them served 500s from the main detail view of the application until an end-to-end pass caught
it.
