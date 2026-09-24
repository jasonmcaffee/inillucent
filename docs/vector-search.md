# Vector search and hybrid retrieval

inillucent does semantic search and keyword search in the same file as your tables, and it fuses the
two result lists into one ranking. There is no separate vector database, no extension to load, and no
embedding server to keep alive.

There are two ways in. **From SQL**, through `VECTOR(N)` columns and an HNSW index the planner uses.
**From the library**, through the retrieval engine directly, which is what the grading harness drives
and what the measurements in [Retrieval quality](retrieval-quality.md) were taken against.

## From SQL

```sql
CREATE TABLE passage (
  id     INTEGER PRIMARY KEY,
  source TEXT,
  body   TEXT,
  v      VECTOR(768)
);

CREATE INDEX passage_v ON passage USING inillucent_hnsw (v);

INSERT INTO passage (source, body, v)
VALUES ('handbook.md', 'the discount applies here', '[0.10, -0.25, 0.81, ...]');

SELECT id, body
FROM   passage
ORDER  BY vector_distance_cos(v, ?1)
LIMIT  10;
```

**That query does not use the index.** `mode` defaults to `exact`, which is a linear scan over
every row, and the HNSW graph the `CREATE INDEX` built is opt in. The scan is the correct answer by
construction and it is the slow one, so the query a reader copies out of here should say which it
wants:

```sql
SELECT id, body
FROM   passage
WHERE  mode = 'approximate'
ORDER  BY vector_distance_cos(v, ?1)
LIMIT  10;
```

Which of the two ought to be the default is a decision rather than a defect, and it is open.
Until it is made, every example on this page names the mode it is using
rather than leaving a reader to find out from a benchmark.

### Writing a vector

A `VECTOR(N)` column holds N finite 32-bit floats. Three spellings reach it, and they store the same
bytes:

| | |
|---|---|
| a JSON array of numbers | `'[0.10, -0.25, 0.81]'` into a `VECTOR(3)` column, which is pgvector's own spelling |
| a blob of little-endian `f32` | `x'cdcccc3d0000803e...'`, which is what the column stores and what `hex(v)` prints |
| a parameter | `--params '[[0.10, -0.25, 0.81]]'` on the command line, and a byte string from a driver |

The JSON form is also what the distance functions read, so `vector_distance_cos(v, '[1,0,0]')` works
against a literal as well as against a bound parameter.

**Anything else is refused where it is written.** A vector of the wrong width, a value that is not a
vector at all, and a component that is NaN or infinite each report a `constraint` error naming the
column. That is deliberate: a NaN component makes every distance against the row NaN, sorts it ahead
of every real neighbour, and makes a later `CREATE INDEX` fail, so the write is the last place it can
be caught by the application that made it.

`CREATE INDEX ... USING inillucent_hnsw (v)` builds a store over the column and backfills the rows
already in the table. It is kept in step by the engine applying a statement's row images to the index
after the write and before the commit, so **the table and its index are one change**: they commit
together and they roll back together.

`ORDER BY vector_distance_cos(v, ?) LIMIT k` is recognised by the planner and turned into a probe of
that index followed by an exact rescore. Measured against a cosine the test computes itself over
20,000 vectors at 256 dimensions, **recall is 1.000**, and the SQL path costs 7.204 ms against the
store's own 7.325 ms — so going through SQL is free.

### The distance functions

| | |
|---|---|
| `vector_distance_cos(a, b)` | cosine distance over vectors normalised to unit length |
| `vector_distance_l2(a, b)` | Euclidean distance |
| `vector_dot(a, b)` | inner product |

pgvector's operator spellings `<->`, `<#>`, `<=>`, `<+>`, `<~>` and `<%>` all parse and bind, as do
its distance and vector function names, and `CREATE INDEX ... USING ivfflat` builds a second index
structure beside the graph. That means a query written for pgvector usually runs here unchanged.

### Which metric the index minimises

Cosine, unless the index says otherwise:

```sql
CREATE INDEX passage_v ON passage USING inillucent_hnsw (v) WITH (metric = 'l2');
```

`'cosine'` is the default and `'l2'` is Euclidean. The metric decides more than the comparison — a
cosine index normalises every vector it stores to unit length, and an L2 index must not, because
normalising destroys the magnitude L2 measures. So it is fixed when the index is built, recorded in
the generation, and a generation whose metric disagrees with the table's declaration is refused
naming both rather than searched.

**The planner probes the index only when the `ORDER BY` function matches the metric the index was
built under.** `ORDER BY vector_distance_l2(...)` over a cosine index, or `vector_distance_cos` over
an L2 one, falls back to the exhaustive scan and a temporary tree. That is a correct answer rather
than a refusal, and it is slower — if a query is scanning where you expected a probe, the metric is
the first thing to check.

An index built before this existed reads as cosine, which is what it was.

`vector_dot` has no index of its own: an inner product ordering plans as a scan whatever the index
says.

## From the library

```rust
use inillucent_core::filter::Filter;
use inillucent_core::index::{Index, IndexConfig};

let mut index = Index::new(IndexConfig { dims: 768, quantized: true, ..Default::default() });
index.add(chunks, &vectors);   // one vector per chunk
index.commit();                // builds the graph, the keyword index and the codes

let filter   = Filter::source("slack");
let compiled = index.compile(&filter);
let hits     = index.hybrid_search(
    "how does the release process work",
    &query_vector,
    &compiled,
    10,
    None,
);
```

Each hit carries a `score`, which decides the order, and a `confidence`, which is a separate number
computed on absolute bounds. [Confidence](#confidence-is-a-separate-number-from-score) explains why
those are two numbers and not one.

## What is inside

**Semantic search** is an HNSW graph, `m = 16`, `ef_construction = 64`, over vectors normalised to
unit length so cosine and the inner product agree. The build runs on every core.

**Exhaustive search is a real query plan, not a test fixture.** When a filter admits a small enough
slice of the corpus, comparing the query against every admitted vector is both exactly correct and
faster than walking a graph over the whole corpus. A cost model counts what the filter admits and
picks the exhaustive scan when it wins. The consequence is that a narrow filter is the case where
accuracy is perfect, rather than the case where it collapses.

**Filters are applied inside the traversal, not after it.** A node that fails the filter is still
expanded, so the walk can pass through it to reach the region behind it, but it is never admitted to
the results. The walk continues until it has collected enough passing rows. The cost is a longer
walk; what it avoids is a result set that comes back short.

From SQL, a predicate reaches the traversal through a `VECTOR(N)` column's own `WHERE` clause, and
on an `inillucent_search` table through a [facet column](#filtering-a-search-table-facet-columns).
A predicate written anywhere else runs after the ranking, which is a different answer rather than a
slower spelling of the same one.

That is the difference that shows up most in the measurements. pgvector evaluates a `WHERE` clause
after the index scan has already chosen its candidates, so a plain HNSW scan produces only
`hnsw.ef_search` candidates and a filter on a minority source can be left with almost none of them.
pgvector's answer is `hnsw.iterative_scan`, which keeps restarting the scan until enough rows pass.
It works, and it costs latency: a filtered search that took a few milliseconds takes tens of them.

**Int8 quantisation with full precision rescoring.** One byte per number instead of four, with the
top candidates rescored against the full precision vectors. Measured on the graded corpus, it is a
quarter of the memory at identical accuracy: 0.995 either way.

**Vectors are read from the file by default** rather than held in memory. Holding them resident costs
1.76 GB on a 600,589 chunk index and buys about 6% on a scan that reads every vector, consistently,
and nothing outside the run to run spread on a graph search. [Where the vectors live](vector-residency.md)
has both modes and how to choose.

## Keyword search

Semantic search is bad at exact tokens. A user who types `PROJ-1932` or `parse_headers` wants that
identifier, not passages about vaguely similar ones. So there is an inverted index beside the graph:
BM25, Snowball stemming, and identifiers kept whole rather than split.

Five weights sit on top of plain BM25, each measured rather than assumed, and each with an off switch
that restores plain BM25:

| setting | what it does | default |
|---|---|---|
| `lexical_coverage` | scales a score by the share of the query's inverse document frequency the chunk holds, raised to this exponent | 3.0 |
| `lexical_proximity` | scales it by matched terms divided by the smallest window holding one of each, blended by this weight | 1.0 |
| `lexical_phrase` | scales it by whether the matched terms arrived in the query's own order inside that window, blended by this weight | 0.75 |
| `lexical_tier` | rank by how many query terms a chunk holds first and by score second | off |
| `lexical_prefix` | let a query term match the terms it is a prefix of | off |

PostgreSQL full text search has two properties plain BM25 lacks, and both of them matter on a corpus
of this size. `to_tsquery` joins query terms with `&`, so a chunk missing one word never appears at
all. And `ts_rank_cd` is cover density ranking, so a chunk whose query terms sit close together
outranks one that mentions the same words in different paragraphs.

inillucent takes both as gradients rather than as gates. Scoring any term with BM25 finds far more of
the right chunks — 49.5 rows of 50 against 6.7 — and `lexical_coverage` and `lexical_proximity` then
put the right ones on top instead of throwing the rest away.

`lexical_phrase` is the one `ts_rank_cd` has no answer to. Cover density asks how tightly the terms
sit. It does not ask whether they came in the order the question asked them in, and "offer
eligibility rules" and "rules for eligibility of an offer" have the same window width and are not the
same answer.

`lexical_tier` is off because `lexical_coverage` does the same job better where the two disagree, and
it is there because it rescues a caller who sets the coverage exponent to 0. `lexical_prefix` is off
because on a dictionary of 494,000 terms it credits a chunk with holding a query term it does not
hold, which is the exact judgement coverage weighting depends on.

## Filtering a search table: facet columns

A column of an `inillucent_search` table declared `FACET` is stored and can be constrained inside a
search. Its value is not indexed as text.

```sql
CREATE VIRTUAL TABLE docs USING inillucent_search(
    body,
    live FACET,
    region FACET,
    dims = 768
);

INSERT INTO docs(rowid, body, live, region, vector) VALUES (1, 'the discount applies here', '1', 'eu', ?1);

SELECT rowid, body
FROM   docs
WHERE  docs MATCH 'discount eligibility' AND k = 10 AND live = '1' AND region = 'eu'
ORDER  BY rank;
```

Several facet constraints narrow rather than widen, which is what the `AND` reads as. A facet is an
ordinary column otherwise: it comes back from a `SELECT`, and on a query that is not a search it is
an ordinary predicate the engine evaluates itself. `FACET` is read without case and only as the last
word of a column's declaration, so quote a column whose name ends in it: `"live facet"` is one
column called `live facet`, and `"live" FACET` is a facet called `live`.

**The constraint is applied inside the scan, and that is the difference the feature is for.** Writing
the same predicate outside the search - joining to another table and filtering there, which is what
FTS5 leaves you with - is not the same answer. The keyword ranking rescores the best `k * 6` hits by
where the query's terms sit inside them, the rescore only ever lowers a score, and a hit below that
window keeps its full score and competes against rescored ones. Which hits are in the window depends
on which rows the scan admitted, so removing rows afterwards produces a different order. Measured on
a 400 row corpus: one hit of the top ten survived. Filtering afterwards also returns fewer rows than
the `LIMIT` asked for, because some of what it ranked is then thrown away.

A value is matched as text, so `live = 1` and `live = '1'` select the same rows. Write every row's
facet: a column left `NULL` reads back as the empty string, which is a value no query is likely to
ask for, so the row answers nothing.

**What it costs.** A constrained search makes the engine count how many rows pass before it runs,
which is one pass over the table, because that count is what chooses between walking the graph and
comparing every admitted vector. Measured on a 20,000 row table in a debug build, 40 queries each
way interleaved: 45.4 ms against 48.7 ms, so about 7% on top. Both the count and the scoring pass
grow with the table, so the share stays about the same as the table does not.

**A table that declares a facet is stored in format 2** and a build older than this one refuses to
open it, by name, saying which release to install. A table that declares none is stored in format 1
exactly as before, so nothing already written becomes unreadable.

## Hybrid retrieval

The two result lists are fused into one. The default is a normalised score fusion: each list is
rescaled onto its own range and the two are added with a weight of 0.35 on the vector side, adapted
per query between 0.05 and 0.95 from the query's own shape. Reciprocal rank fusion and a convex
combination are also available and were measured against it; the normalised score fusion won on every
hybrid metric of the graded corpus, which is why it is the one that ships.

Fusing is what lets one query answer both `PROJ-1932` and "how does the release process work". The
graded comparison in [Retrieval quality](retrieval-quality.md) runs the whole pipeline, not either
branch on its own, because a ranking that wins in isolation and loses once the other branch is fused
beside it has not helped anybody.

## Confidence is a separate number from score

The failure that does not announce itself is ten confident looking passages for a question nothing
in the corpus answers, from which an agent then writes a paragraph. Measuring that needs an absolute
notion of confidence, and the usual per list normalisation destroys one by construction: it maps the
best hit of every list to exactly 1.0, whether the list is good or hopeless.

Dividing each side by a bound the results had no say in fixes that. Cosine over normalised vectors is
bounded by one, and BM25 by the query's own inverse document frequency mass at saturation. It took
the confident answer rate on unanswerable questions from 1.000 to 0.000.

**As a ranker it lost**, for a structural reason. Its keyword bound assumes some chunk could hold
every query term, and a question drawing on two sources is built so that none can, so the whole
keyword side collapses towards zero and the ranking becomes vector only: 0.446 against 0.690 on that
family. That is correct behaviour for a confidence and wrong behaviour for an order.

So the engine stopped asking one number to do both jobs. Every hit carries a **`score`**, from
whichever fusion ranks best, and a **`confidence`**, always computed on absolute bounds whatever
fusion ordered the list. An abstention threshold is set on confidence; the ranking is decided by
score.

Asked 200 questions the corpus does not answer, PostgreSQL with pgvector returns a confident top
result **every single time**. inillucent does it on **one question in two hundred**.

## Trading accuracy against speed

`ef_search` is how wide the graph traversal keeps its candidate list, and it is the one setting most
callers turn. Accuracy here is against an exhaustive comparison over the whole corpus:

| `ef_search` | accuracy | time |
|---|---|---|
| 64 | 0.850 | 0.43 ms |
| 128 (default) | 0.907 | 0.65 ms |
| 256 | 0.938 | 1.17 ms |
| 512 | 0.973 | 2.10 ms |

Below the crossover the cost model does not use the graph at all, so a narrow filter reaches 1.000
whatever this is set to.

## Limits

- **Adding content folds into the graph rather than rebuilding it.** A commit loads the published
  generation and inserts each entry of the delta log into it, so the cost is one graph insert per row
  written rather than one per row in the table. **Publishing is proportional to the batch too**:
  a flush builds a new immutable segment out of its own rows and writes nothing else,
  and a search folds the live segments, with a newer one shadowing an older for the same row. The
  default flush trigger is a constant 1,024 entries rather than a share of the table, because the
  share existed only to make a whole-index rewrite rare and there is no longer a whole-index rewrite. The single-pass build over everything is still reachable, by
  `INSERT INTO t(t) VALUES('compact')`.
- **The graph and the keyword postings are held in memory.** The vectors are not, by default. A
  3.1 GB index of 600,589 chunks serves from 1.3 GB resident with the vectors filed.
- **The index probe orders by the metric the index was built under**, cosine by default and L2 when
  the index says `WITH (metric = 'l2')`. The distance functions answer for every metric whether or
  not an index does; an ordering by one the index was not built under plans as a scan.
- **Deleting and reinserting the same rows grows the file, and `VACUUM` is what gives the space
  back.** Measured on 2,000 rows of `VECTOR(16)` with a `USING inillucent_hnsw` index, where each
  cycle deletes 1,000 rows and reinserts the same 1,000 in one transaction, checkpointing after
  each:

  | after | bytes |
  |---|---|
  | the build | 1,605,632 |
  | 1 cycle | 2,064,384 |
  | 3 cycles | 4,390,912 |
  | 5 cycles | 5,308,416 |
  | 10 cycles | 8,880,128 |

  The row count is 2,000 at every measurement and recall is unaffected. `compact` does not shrink
  the file - it writes a new generation, so the file grows again, to 9,666,560 - and
  `drop-old-generations` frees b-tree pages without returning them to the operating system. The
  sequence that reclaims is all three in order:

  ```sql
  INSERT INTO c_v(c_v) VALUES('compact');
  INSERT INTO c_v(c_v) VALUES('drop-old-generations');
  VACUUM;
  ```

  After it the file is **1,966,080 bytes, which is a fresh build of the same 2,000 rows to the
  byte**. It was 2.5 times a fresh build until a fix stopped `VACUUM` writing a second copy of
  every shadow table.

## Where to go next

- [Retrieval quality](retrieval-quality.md) — the 17 graded comparisons against pgvector
- [Embeddings](embeddings.md) — producing the vectors, in your process
- [Where the vectors live](vector-residency.md) — resident or filed, and what each costs
- [Architecture](architecture.md) — how the graph, the filter and the fusion work
