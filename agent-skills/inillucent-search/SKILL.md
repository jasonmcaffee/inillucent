---
name: inillucent-search
description: Full-text and vector search inside inillucent - VECTOR(N) columns, HNSW indexes, FTS5 with bm25, the inillucent_search hybrid table, and the CLI verbs for each. Use when asked to add semantic or keyword search, build a RAG store, replace pgvector, or query embeddings from SQL.
---

# Search: full text and vectors, in the same file

inillucent does the job of "PostgreSQL + pgvector + an embedding server", in process. There is no
extension to load and no second service to run — a `.rdb` can hold ordinary tables *and* a hybrid
index, and the two commit and roll back together as one change.

Three surfaces, and picking the right one is most of the work:

| you want | use |
|---|---|
| nearest-neighbour search over embeddings you already have | a **`VECTOR(N)` column** + `CREATE INDEX … USING inillucent_hnsw` |
| keyword search over text | an **FTS5** table, with `bm25()` |
| both at once over one corpus, fused and scored | an **`inillucent_search`** virtual table |

## Vectors, from ordinary SQL

```sql
CREATE TABLE embedding (id INTEGER PRIMARY KEY, source TEXT, v VECTOR(768));
INSERT INTO embedding (id, source, v) VALUES (1, 'note-1', x'3f80000000000000…');
CREATE INDEX ix ON embedding USING inillucent_hnsw (v);
SELECT id, source FROM embedding ORDER BY vector_distance_cos(v, ?1) LIMIT 10;
```

- **A `VECTOR(N)` literal is a blob of N little-endian `f32`s** — `x'…'`, four bytes per element. A
  vector of the wrong width is refused rather than resized.
- `vector_distance_cos`, `vector_distance_l2` and `vector_dot`, plus pgvector's operator spellings
  `<->`, `<#>`, `<=>`, `<+>`, `<~>`, `<%>`.
- **`ORDER BY vector_distance_cos(v, ?) LIMIT k` is planned onto the index** and rescored exactly.
  Graded at recall 1.000 against an exhaustive cosine, and measured at 0.98x the cost of querying the
  store directly — the SQL path is free.
- **`CREATE INDEX … USING inillucent_hnsw` backfills**: an index made over a table that already holds
  rows is not an empty index, and rows inserted, updated and deleted afterwards are applied to it
  before the commit.
- With no index the same query is an exhaustive scan and **is still correct**. Build the index when
  it is slow, not before.
- Ordering on the index is cosine-only today. `WITH (metric = …)` is where a second metric goes;
  `USING ivfflat` is a second structure beside the graph.

From the command line, which writes the query for you:

```sh
inillucent --db app.rdb vector-search embedding --column v \
  --vector '[0.01, -0.42, …]' --k 10 --measure cos
```

## Full text

```sql
CREATE VIRTUAL TABLE doc USING fts5(title, body);
INSERT INTO doc (title, body) VALUES ('a title', 'some body text');
SELECT title, bm25(doc) AS score FROM doc WHERE doc MATCH 'body OR text' ORDER BY rank;
```

```sh
inillucent --db app.rdb search 'body OR text' --table doc --k 10
```

The verb writes the `MATCH … ORDER BY rank` idiom, which is the part nobody remembers. Query syntax
is FTS5's: bare words are ANDed, `"a phrase"` is quoted, `OR` and `NOT` are available. The `porter`
tokenizer stems whatever tokenizer it wraps.

## Both at once — the hybrid table

```sql
CREATE VIRTUAL TABLE store USING inillucent_search(title, body, dims = 768);
INSERT INTO store (rowid, title, body, vector) VALUES (1, 'a title', 'body text', x'…');

-- lexical
SELECT title FROM store WHERE store MATCH 'body' ORDER BY rank;
-- vector
SELECT title FROM store WHERE vector = x'…' AND k = 10;
-- both, fused
SELECT title FROM store WHERE store MATCH 'body' AND vector = x'…' AND k = 10 ORDER BY rank;
```

`dims = N` makes it a vector table as well as a lexical one; without it, a vector is **refused
rather than ignored**. This is the same HNSW the retrieval engine uses — there is one implementation
of an approximate index in this repository, not two — with BM25 carrying coverage and proximity
weighting, three fusion methods, and a calibrated confidence beside every score.

`inillucent-search` is also what `CREATE INDEX … USING inillucent_hnsw` builds underneath, which is
why the two agree.

## Choosing between them

- **Only embeddings, and the rows live in a table you already have** → `VECTOR(N)` plus an HNSW
  index. It keeps your schema and your joins.
- **Only keywords** → FTS5. It is SQLite's, it behaves as SQLite's does, and `bm25()` is there.
- **A retrieval corpus you will query both ways** → `inillucent_search`. One table, one commit, one
  score, and the fusion is done for you.

## What to expect of it

Measured against PostgreSQL 17 with pgvector on the same corpus and the same recall target
(`README.md` and `inillucent-scorecard.md` carry the full table): semantic p50 **1.44 ms** against
36.71 ms, filtered to a minority source **1.65 ms** against 45.06 ms, **no processes to run** against
a server plus an embedding service. In production on a 598,560-chunk mailbox: recall@100 **1.000**
against an exact scan, 27 ms p95.

Before you design around a construct, ask:

```sh
inillucent capabilities            # every row checked against the running engine, both directions
```

And read `feature-comparison.md` — it is the measured side-by-side, not a feature list.
