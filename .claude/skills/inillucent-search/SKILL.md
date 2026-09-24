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

## A corpus to try it on, with nothing to build

`examples/rag-agent/` is a database of Greek philosophy that is already embedded and committed: 80
Wikipedia articles, 2,661 passages, a 768 dimension vector on each. Install the model and search it:

```sh
inillucent setup-embeddings all
inillucent --db examples/rag-agent/greek-philosophy.rdb query \
  "SELECT title, body FROM passage
   ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5" \
  --params '["who was Seneca"]'
```

**`embed` runs once for the statement, not once per row**, because it is registered deterministic and
its argument does not vary within one execution. Before this was fixed, nothing read that flag and the same
query took 105 seconds on that corpus instead of one and a half. A function you register yourself
gets the same treatment only if you set `FunctionFlags::deterministic` — the default for anything
registered from outside is `false`, which is the safe assumption about code this engine did not write.

Its `AGENTS.md` is the page to copy when you build one of these for somebody else.

## Where the vectors come from

You can supply them, and most callers do. inillucent can also produce them, in this process, with no
embedding server and no socket:

```sh
inillucent setup-embeddings all     # ONNX Runtime + nomic-embed-text-v1.5, about 620 MB, once
```

```sql
INSERT INTO note (body, v) SELECT ?1, embed(?1);
SELECT id FROM note ORDER BY vector_distance_cos(v, embed('flight details')) LIMIT 10;
```

`embed(TEXT)` returns the 3,072 bytes a `VECTOR(768)` column holds. Three things to know before
reaching for it:

- **The published 0.1.2 archives carry it**, because `packaging/release-all.ps1` passes
  `--features inillucent-cli/embed`. The 0.1.1 archives do not, and a build from a checkout needs
  that flag as well, because the feature is off by default. A build without it says
  `no such function: embed`, with the status `not_found`. A build with it and no model installed
  refuses by name, with the status `invalid_state` and the command that installs one, rather than
  returning a NULL or a vector of zeroes:

  ```
  Error [invalid_state]: embed: no embedding model is installed. Run `inillucent setup-embeddings`
  to download nomic-embed-text-v1.5 and the ONNX Runtime it needs, or set INILLUCENT_ONNX_DIR to a
  directory that already holds them
  ```

  A vector whose provenance is unknown is worse than no vector: it goes into an index, and every
  neighbour it is ever compared against is wrong.

  The 0.1.2 archives printed `Error [syntax]: bad parameter or other API misuse` for that case, which
  named neither the function nor the fix. This is now fixed.
- **Nothing has to be exported after the install.** The engine finds the runtime and the weights
  where the command put them. `ORT_DYLIB_PATH` and `INILLUCENT_ONNX_DIR` still override.
- **A registered function reaches the write path.** `INSERT ... VALUES`, `UPDATE ... SET` and
  `RETURNING` all take one, so `INSERT INTO note (body, v) VALUES (?1, embed(?1))` writes the vector
  the function returns. Until this was fixed, those three were refused with the `unsupported` status and
  `INSERT ... SELECT` was the only shape that worked.
- **Loading the model costs 650 to 800 ms and an embedding costs 12 to 36 ms**, so when it is in
  memory matters. `--residency resident` keeps it, `on-demand` drops it after every call, and the
  default `idle:5m` keeps it through a burst of questions and lets it go afterwards. A process that
  answers one question and exits wants `on-demand`; an ingestion run wants `resident`.
  → [Embeddings](../../docs/embeddings.md)

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
- Ordering on the index is cosine unless the index says `WITH (metric = 'l2')`, and a query whose
  distance function does not match the index's metric plans as a scan rather than a probe;
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

## Filtering a hybrid table — facet columns

A column declared `FACET` is stored and can be constrained inside the search. Its value is not
indexed as text, so it changes no ranking of the prose beside it.

```sql
CREATE VIRTUAL TABLE store USING inillucent_search(title, body, live FACET, region FACET, dims = 768);
INSERT INTO store (rowid, title, body, live, region, vector) VALUES (1, 'a title', 'body text', '1', 'eu', x'…');

SELECT title FROM store
 WHERE store MATCH 'body' AND k = 10 AND live = '1' AND region = 'eu'
 ORDER BY rank;
```

**Do not filter outside the search instead.** Joining to another table and putting the predicate
there is a different answer, not a slower spelling of the same one: the keyword ranking rescores the
best `k * 6` hits by where the query's terms sit inside them, so which hits get rescored depends on
which rows the scan admitted. Measured on a 400 row corpus, the two shared one hit of the top ten.
It also returns fewer rows than the `LIMIT` asked for. Facets are how a predicate reaches the scan.

A facet is otherwise an ordinary column: it comes back from a `SELECT`, and on a query that is not a
search the engine evaluates the predicate itself. A table declaring one is stored in format 2 and an
older build refuses to open it by name.

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

And read `docs/feature-comparison.md` — it is the measured side-by-side, not a feature list.
