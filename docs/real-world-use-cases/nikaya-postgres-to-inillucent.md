# Nikaya: removing PostgreSQL from a 5.8 GB Gmail assistant

Nikaya is a local Gmail retrieval assistant — every message and attachment pulled down, normalized,
chunked, embedded, and searchable three ways through a web UI and an MCP tool surface. It has run on
PostgreSQL 17 with pgvector since it was built. In September 2026 it stopped.

This is what that took, on a real mailbox rather than a fixture: **64,378 messages, 66,793 documents,
602,022 chunks, 12,485 attachments, 5,852 MB of PostgreSQL**. It is written for someone deciding
whether to do the same thing, so it leads with what went wrong.

Everything here was measured on the box it ran on, a Windows 11 workstation with 127.5 GB of memory
and an NVMe SSD, while several other things were running. The arms are interleaved for that reason.

---

## 1. What actually moved

Nikaya's *retrieval* was already inillucent: task-1774 moved the vector and lexical branches into the
in-process `inillucent-core::Index` and measured recall@100 going from pgvector's 0.899 to 1.000. What
had not moved was the **record** — documents, chunks, participants, attachments, jobs, sessions, and
the vectors themselves — which was still PostgreSQL reached through `sqlx`, with the retrieval index
as a projection of it.

So this is the second half: 16 tables, 1,626,000 rows, and the 19 files that touched `sqlib`.

| | before | after |
|---|---|---|
| record | PostgreSQL 17 + pgvector, over a socket | one `.rdb` file, in process |
| durable vectors | `halfvec(768)` in `chunk_embedding` | `VECTOR(768)` in the same file |
| retrieval | `inillucent-core::Index` | unchanged |
| processes to run | PostgreSQL, the embedder, the server | the embedder, the server |
| on disk | 5,852 MB | 7,422 MB |

The file is **larger**, and that is the first honest number. PostgreSQL compresses large values in
TOAST; this engine does not. The vectors also widen from `halfvec`'s two bytes an element to `f32`'s
four — 925 MB becomes 1,850 MB — which is offset by 1,811 MB of HNSW and roughly 900 MB of GIN index
that are not carried at all.

---

## 2. The single most important thing that happened

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

| operation | PostgreSQL, custom plan | PostgreSQL, auto | inillucent | |
|---|---:|---:|---:|---|
| `doc.attachments` | 0.285 ms | 0.255 ms | **0.192 ms** | 1.48x faster |
| `doc.email` | 0.160 ms | 0.166 ms | **0.137 ms** | 1.17x |
| `attachment.text` | 0.247 ms | 0.308 ms | **0.224 ms** | 1.10x |
| `doc.participants` | **0.212 ms** | 0.201 ms | 0.246 ms | 0.86x |
| `doc.load` | **0.177 ms** | 0.153 ms | 0.242 ms | 0.73x |
| `thread.load` | **0.308 ms** | 0.363 ms | 1.716 ms | 0.18x |
| `labels.distinct` | **45.9 ms** | 46.7 ms | 91.2 ms | 0.50x |
| `status.counts` | **367.8 ms** | 359.5 ms | 6,228 ms | 0.06x |
| `index.pending` | **0.553 ms** | 0.554 ms | 3,303 ms | 0.0002x |

**Point reads are a wash to slightly better. Anything that aggregates or scans is worse, and one of
them is catastrophically worse.** `index.pending` is the ingestion queue check; it is 6,000x slower
because the index that should serve it is not chosen, because there are no statistics, because
`ANALYZE` fails. `status.counts` is five `count(*)` over 600,000 row tables: PostgreSQL spreads that
across parallel workers, and this engine walks a covering index on one core.

`corpus.scan20k` is excluded rather than reported as a ratio: the PostgreSQL side counts a subquery,
so the server never sends the 20,000 vectors anywhere, while the inillucent side builds 20,000 chunk
values with their 768 floats each. Both are honest costs on their own side and they are not the same
work.

### Retrieval, and what holding the vectors in memory buys

The deployed configuration puts every search on the exhaustive path over all 600,589 chunks. Two
rounds, interleaved, first pass discarded so the page cache is warm.

| | p50 | p95 | peak resident |
|---|---:|---:|---:|
| semantic, vectors **filed** (default) | 71.1–76.3 ms | 86.6–88.1 ms | **2,215 MB** |
| semantic, vectors **resident** | 66.3–75.4 ms | 83.4–89.5 ms | **3,975 MB** |
| hybrid, vectors **filed** (default) | 99.4–101.7 ms | 134.2–135.9 ms | **2,272 MB** |
| hybrid, vectors **resident** | 95.7–103.6 ms | 140.0–145.6 ms | **4,032 MB** |

**Holding the vectors on the heap costs 1,760 MB and buys nothing measurable.** The differences in
p50 are inside the round-to-round spread on a shared machine — the filed arm is faster than the
resident one in one of the four comparisons. That is why the default changed: the same bytes are in
memory either way while they are being used, but as reclaimable page cache rather than as a heap
allocation this process will never give back.

The caveat that number needs: this is the steady state. The first search after a reboot, with nothing
cached, pays a read of the vector file that the resident arm paid at open instead.

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
