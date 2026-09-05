# inillucent Technical Design Document

## 1. What this is and why it exists

inillucent is an embedded search engine for a corpus of workplace documents: wiki pages, chat messages, issue threads, source files, design files and boards. The usual way to build retrieval over such a corpus needs two separate programs running alongside the application. inillucent replaces both with one library that runs inside the calling process:

| Replaced | Replaced by |
|---|---|
| PostgreSQL 18.6 with the pgvector 0.8.6 extension | an in process vector index and inverted index owned by inillucent |
| `llama.cpp` serving the embedding model as a separate process, reached over HTTP | ONNX Runtime called in process through the `ort` crate |

The corpus is 39,366 documents and 186,781 chunks. Every chunk carries a 768 dimensional embedding produced by `nomic-embed-text-v1.5`. That is small enough to hold entirely in memory on a laptop, which is the fact that makes a specialized engine worth building: PostgreSQL is paying for durability, multi version concurrency control, a query planner, a network protocol and a general purpose page cache, and this workload needs none of them.

The goal is not novelty. The goal is to reach and then beat the accuracy of a correctly configured PostgreSQL, pgvector and `llama.cpp` stack across every retrieval scenario that stack supports, and to prove it with a graded test suite rather than an assertion.

### 1.1 What the baseline stack does, precisely

This is the specification inillucent has to meet. It is the behaviour of a retrieval stack built on PostgreSQL with pgvector and `llama.cpp` serving the embedding model, and `inillucent-bench` issues that stack's SQL directly so the baseline is real behaviour rather than an approximation of it.

Embedding: `nomic-embed-text-v1.5`, served by `llama.cpp` over HTTP, context 2048, batch 32. Nomic requires task prefixes, so documents are embedded as `search_document: <text>` and queries as `search_query: <text>`. Text over 1900 tokens is split at newline or whitespace boundaries and the resulting piece vectors are averaged into one vector, so each chunk maps to exactly one row.

Vector retrieval: `chunks.embedding` is `vector(768)`, indexed with HNSW using `vector_cosine_ops`, `m = 16`, `ef_construction = 64`. Ranking is cosine distance through the `<=>` operator, ascending, limit 50.

Lexical retrieval: a GIN index over `to_tsvector('english', content)`, ranked by `ts_rank_cd`, limit 50. The query is built by stripping non word characters from each term, appending `:*` for prefix matching, and joining the terms with `&`.

Fusion: Reciprocal Rank Fusion with `k = 60` over the two candidate lists of 50, then a cap of 2 chunks per document, then truncation to the caller's `top_k`.

Filters: `source`, several `sources` at once, `space_key`, a single `author` matched against display name or identifier, strict per source `(source, author_id)` tuples, `updated_after`, and label overlap. Soft deleted documents are excluded with `deleted_at IS NULL`.

### 1.2 Two limitations of that stack, measured

These were measured against a loaded PostgreSQL database, not inferred.

**A filtered vector search returns nothing until pgvector is configured for it.** pgvector applies a `WHERE` clause after the HNSW scan, and the initial scan yields only `hnsw.ef_search` candidates, which defaults to 40. An application that filters by source on every call and leaves `hnsw.ef_search` and `hnsw.iterative_scan` at their defaults therefore gets almost nothing: the two largest sources hold 141,435 of the 186,781 chunks, so the 40 globally nearest chunks nearly always belong to one of them, and filtering them to another source leaves nothing. This is the default behaviour of the extension rather than a bug in it, and it is why the baseline graded here is configured rather than default.

With a real nomic query vector for "how does the release process work", asking for 50 rows:

| pgvector setting | slack rows returned | jira rows returned |
|---|---|---|
| default, `hnsw.ef_search = 40` | 0 | 0 |
| `hnsw.ef_search = 1000` | 7 | 50 |
| `hnsw.iterative_scan = relaxed_order`, `hnsw.max_scan_tuples = 200000` | 50 | 50 |

An application built this way runs its filtered searches without any vector candidates, and Reciprocal Rank Fusion conceals that because the lexical side still returns rows. Turning on `hnsw.iterative_scan` and raising `hnsw.scan_mem_multiplier` restores the full result set, at 45 milliseconds per filtered search against 1.6 for an unfiltered one, because the scan is repeated rather than the filter avoided. That latency is the cost the baseline pays and the reason a specialized traversal is worth writing.

**The lexical query requires every term.** Joining terms with `&` means a chunk must contain all of them. For a question like "how does the release process work" that is a small fraction of the chunks containing `release` or `process`. The queries most likely to return nothing lexically are long natural language queries, which are the same queries whose vector side is filtered to zero. `ts_rank_cd` also has no document length normalization and no term saturation, both of which BM25 provides.

## 2. Design principles

1. **Measure, do not assume.** Every accuracy claim in the score card comes from the harness. Vendor numbers and paper numbers inform which techniques to try, never what to report.
2. **Grade the index, not the embeddings.** Both engines are loaded with byte identical vectors and the same chunk text. A score difference then measures indexing and ranking, which is what inillucent is responsible for.
3. **Exhaustive search is the ground truth.** Approximate accuracy is meaningless without an exact reference, so inillucent ships an exhaustive scan and the harness uses it to define correct answers.
4. **Filters are part of the query, not a postprocessing step.** The traversal has to honour the predicate, because filtered queries are the common case in this corpus, not the exception.
5. **No hidden state.** An index is a directory of files that loads by memory mapping. There is no background process, no daemon and no network port.

## 3. Architecture

```
inillucent/
  inillucent-tdd.md
  inillucent-scorecard.md              generated by the harness
  crates/
    inillucent-core/                    the engine, no I/O beyond the index files
      src/lib.rs
      src/store.rs                  documents, chunks, metadata columns
      src/filter.rs                 predicate representation and evaluation
      src/distance.rs               cosine and dot product over f32 and int8
      src/quantize.rs               int8 scalar quantization and rescoring
      src/flat.rs                   exhaustive search, the accuracy reference
      src/hnsw.rs                   the graph index and filtered traversal
      src/tokenize.rs               Snowball tokenization matching Postgres
      src/bm25.rs                   inverted index and BM25 ranking
      src/rank.rs                   Reciprocal Rank Fusion and the per document cap
      src/index.rs                  the public Index type
      src/embed.rs                  the Embedder trait and implementations
      src/persist.rs                save and load, four files with versioned headers
    inillucent-bench/                   the grading harness
      src/main.rs
      src/corpus.rs                 load chunks and vectors from PostgreSQL
      src/queryset.rs               the graded query sets
      src/engine.rs                 the two engines behind one trait
      src/metrics.rs                recall, nDCG, MRR, success, latency
      src/report.rs                 score card generation
```

`inillucent-core` has no knowledge of PostgreSQL. `inillucent-bench` is the only crate that links a database client, because its job is to read the baseline corpus and to query the baseline engine.

### 3.1 The storage model

The unit of retrieval is a chunk, exactly as in the baseline. A chunk belongs to a document, and the filterable attributes live on the document.

*Design sketch, not yet source.*

```rust
pub struct ChunkId(pub u32);
pub struct DocId(pub u32);

pub struct Document {
    pub doc_id: DocId,
    pub source: u16,            // dictionary encoded: confluence, slack, ...
    pub space_key: Option<u16>, // dictionary encoded
    pub author: Option<u32>,    // dictionary encoded display name
    pub author_id: Option<u32>, // dictionary encoded stable identifier
    pub updated_at: i64,        // unix seconds, i64::MIN when absent
    pub labels: Range<u32>,     // slice into a shared label identifier arena
    pub deleted: bool,
    pub title: String,
    pub url: String,
}

pub struct Chunk {
    pub chunk_id: ChunkId,
    pub doc_id: DocId,
    pub chunk_index: u32,
    pub heading_path: Range<u32>, // slice into a shared string arena
    pub content: Range<u64>,      // slice into one contiguous text blob
}
```

Every attribute that a filter touches is a fixed width integer, and the string values behind them are dictionary encoded once at build time. Evaluating a predicate is then an integer comparison against a value already in cache, not a string comparison and not a join. This matters because the filtered traversal in section 3.4 evaluates the predicate on every node it visits, so predicate evaluation sits in the innermost loop.

Vectors live in one contiguous `Vec<f32>` of `n_chunks * 768` elements, addressed by chunk identifier. Nothing is boxed per vector, and a chunk's vector is one slice at a known offset.

### 3.2 Distance

All vectors are L2 normalized once at insert time. Cosine similarity is then a dot product, and cosine distance is `1 - dot`. This removes two square roots and a division from every comparison, and it is why the baseline's magnitude invariance argument for averaging piece vectors still holds.

The dot product is written so that the compiler can vectorize it: accumulate into four independent sums over chunks of the slice, then add the four. Four accumulators break the dependency chain that otherwise serializes floating point addition, which is the single change that matters most for this loop. Explicit SIMD intrinsics are deliberately avoided at first; the harness measures whether the autovectorized version is fast enough before any unsafe code is introduced.

### 3.3 Quantization

Qdrant's published results say int8 scalar quantization costs under 1% accuracy for a 4x memory reduction, and that binary quantization needs high dimensionality, with documented results at 1536 and 4096 dimensions and reported degradation below roughly 1000 dimensions. nomic is 768 dimensional, so binary quantization is the wrong default and int8 is the right one. The harness measures this rather than accepting it.

The scheme is symmetric int8 with one scale per vector. For a normalized vector the components are already bounded, so a per vector scale of `max(|component|) / 127` keeps the full range without clipping. Search runs over the int8 codes to produce a candidate list, then the top candidates are rescored with the f32 vectors. Oversampling is a query parameter: request `k * oversample` candidates from the quantized pass and return `k` after rescoring.

Matryoshka truncation is a second, independent lever. The model is trained so a 768 dimensional embedding can be truncated to 512, 256, 128 or 64 dimensions and renormalized. Combining truncation with int8 gives a memory ladder from 2,304 bytes per vector down to 64, and the harness reports accuracy at every rung so the tradeoff is a table rather than an opinion.

### 3.4 The vector index and filtered traversal

The index is HNSW, the same algorithm pgvector uses, with the same defaults so the comparison is fair: `m = 16`, `ef_construction = 64`. Level assignment, the greedy descent through upper layers and the neighbour selection heuristic follow the original algorithm.

The difference is what happens when a predicate is present. Filtering after the search is what produces the zero row results in section 1.2. inillucent instead does what ACORN describes: traverse the graph as though it contained only the nodes that satisfy the predicate, without materializing that subgraph.

Concretely, the search at layer zero keeps two structures, a candidate frontier and a result heap, and treats them differently:

- A visited node is **expanded** regardless of whether it satisfies the predicate. A node that fails the predicate is still a useful stepping stone, and refusing to walk through it is what disconnects the graph and collapses accuracy.
- A visited node **enters the result heap only if it satisfies the predicate**.
- The frontier stops growing when its best remaining candidate is worse than the worst result *and* the result heap holds `ef` entries. Because only passing nodes enter the result heap, a selective predicate naturally forces the traversal to keep walking, which is exactly the behaviour `hnsw.iterative_scan` bolts on afterwards in pgvector.

When the predicate is very selective, graph traversal stops being the right tool, so inillucent scans the passing set instead. An exhaustive scan of 17,000 normalized 768 dimensional vectors takes a few milliseconds and is exact, so on a selective predicate it is both faster and more accurate than a graph walk.

The choice is made by a cost model rather than a threshold. Exhaustive search costs one distance computation per passing chunk. A filtered walk has to visit roughly `ef / selectivity` nodes before it finds `ef` that pass, and each visit expands about `2m` neighbours, so it costs roughly `ef * 2m / selectivity`. Scanning wins when

```text
pass_count < sqrt(ef * 2m * n_chunks)
```

A constant would have been wrong twice over: wrong for a corpus ten times this size, and wrong again whenever a caller raises `ef`. Both appear in the formula, so the crossover moves with them. On this corpus at `ef_search = 128` it lands near 27,700 chunks, which puts slack, jira, figma and miro on the exact path and leaves confluence and github on the graph. The score card prints, per source, how many chunks the predicate admits and which path was chosen.

The count of passing chunks therefore sits on the query path, so it cannot be computed by scanning every chunk. The store maintains the number of chunks per source that are not soft deleted, and a predicate constraining at most the source reports its count from that table. Anything more complex falls back to a scan, and a test checks the two agree across every filter shape.

The predicate is compiled once per query into a closure over the dictionary encoded columns:

*Design sketch, not yet source.*

```rust
pub struct Filter {
    pub sources: Option<Vec<u16>>,
    pub space_key: Option<u16>,
    pub author: Option<u32>,
    pub authors: Option<Vec<(u16, u32)>>,   // strict per source pairs
    pub updated_after: Option<i64>,
    pub labels: Option<Vec<u32>>,           // overlap, not containment
    pub include_deleted: bool,
}
```

`authors` reproduces the baseline's rule exactly: a source named in the list is constrained to its own author identifiers, and a source not named passes through unconstrained.

### 3.5 The lexical index

Term normalization has to match PostgreSQL, because term parity is what makes the ranking comparison meaningful. The live database confirms `english` uses `english_stem`, the Snowball English stemmer: `eligibility` and `eligible` both stem to `elig`, `offers` and `offering` both to `offer`. inillucent uses the `rust-stemmers` crate, which implements the same Snowball algorithm, over the same English stopword list.

The index is a standard inverted index: a term dictionary mapping each stemmed term to a postings list of `(chunk_id, term_frequency)`, plus a per chunk length and the collection mean length.

Ranking is BM25 with `k1 = 1.2` and `b = 0.75`:

```
score(q, d) = sum over terms t in q of
    idf(t) * (tf(t, d) * (k1 + 1)) / (tf(t, d) + k1 * (1 - b + b * len(d) / avg_len))

idf(t) = ln(1 + (N - df(t) + 0.5) / (df(t) + 0.5))
```

BM25 fixes both weaknesses of `ts_rank_cd`. The `b` term normalizes for document length, so a long Figma chunk averaging 2,222 characters no longer outscores a short precise JIRA chunk merely by containing more words. The `k1` term saturates term frequency, so the tenth occurrence of a term adds far less than the second.

Query semantics change from requiring every term to scoring any term. A chunk matching four of five query terms scores highly and is returned, where today it is excluded entirely. Because BM25 weights rare terms far above common ones through `idf`, this does not flood the results with matches on common words.

Prefix matching is kept, because PostgreSQL full text search offers it through `:*` and identifiers matter in this corpus. A query term is expanded through the term dictionary to the terms it prefixes, capped at a fixed expansion count, and the expansions contribute at the query term's own weight so one prefix cannot outvote the rest of the query.

One place inillucent deliberately stops matching PostgreSQL: compound identifiers. `ts_debug` shows that `to_tsvector('english', 'PROJ-1932')` produces `'proj':1 '-1932':2`, so Postgres splits on the hyphen and the issue key stops existing as a searchable term. `author_id` becomes `author` and `id`. Copying that was the right starting point, because term parity is what makes the ranking comparison meaningful, but inheriting the weakness defeats the purpose of a specialized engine, and this corpus is full of issue keys, function names and file paths.

The tokenizer therefore emits the whole identifier as an additional term when a word carries an internal separator and reads as a name rather than as prose: a letter together with a digit, more than one separator, or an underscore. `PROJ-1932`, `author_id`, `v1.5.2` and `src/search/vector` are kept whole as well as split. `well-known` and `long-running` are not, because filling the dictionary with ordinary hyphenated English costs entries and buys nothing. The parts are still emitted in every case, so a query for `author` still matches `author_id`.

### 3.6 Ranking and fusion

The default is Reciprocal Rank Fusion with `k = 60` and a cap of 2 chunks per document, identical to the baseline, so the first comparison isolates retrieval accuracy from ranking policy.

Reciprocal Rank Fusion throws away score magnitude and keeps only rank, which is robust but lossy: it cannot tell a top hit that is nearly identical to the query from one that merely came first in a weak list. inillucent therefore also implements normalized score fusion, where each side's scores are mapped onto a comparable range before a weighted sum, with the weight as a query parameter. Which one wins is an empirical question per scenario, so the harness grades both and the score card reports both.

### 3.7 Embedding

The embedder is a trait so that grading and ordinary use can differ without the engine changing:

*Design sketch, not yet source.*

```rust
pub trait Embedder {
    fn embed_documents(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    fn dimensions(&self) -> usize;
}
```

The shipping implementation runs `nomic-embed-text-v1.5` as ONNX through `ort`, in process, with no child process and no HTTP hop. It applies the `search_document: ` and `search_query: ` prefixes the model requires, mean pools the token embeddings, applies the layer normalization the model expects, truncates to the configured Matryoshka width and L2 normalizes.

The grading implementation reads vectors already computed. This is not a shortcut: it is the only way principle 2 holds. If the two engines embedded independently, a score difference could come from a quantization difference between the GGUF Q5_K_M weights and the ONNX weights, and the harness would be measuring the wrong thing.

**Implemented and verified.** An earlier draft of this document claimed the ONNX path was written and unit tested when only the trait boundary existed. It is now genuinely implemented, in `embed_onnx.rs`, after the weights were fetched by hand because the corporate proxy returns HTTP 403 for `huggingface.co` downloads.

When this engine was first graded, its stored vectors had been produced by `llama.cpp` serving the model over HTTP. Embedding 400 of those chunks in process gave a mean cosine of 0.9860 against the stored vectors, with none below 0.95, the residual being the gap between a quantised build at Q5_K_M and the full precision export. That comparison cannot be rerun here, because the corpus is now embedded in process to begin with and there is no second embedder to disagree with.

Cosine agreement was not the acceptance criterion, because a difference in the vector only matters if it changes what a person is shown. Running 120 document identity queries through the same index, once with each embedder's query vectors, gave success@1 of 0.8250 both ways, success@10 of 0.8917 both ways, and mean reciprocal rank of 0.8509 against 0.8487: equally often right, so the disagreement was reshuffling among equally good answers. That is what made it safe to drop the server.

What is checked now is a different and still necessary property: that the vectors in the cache were produced from the text in the cache. The corpus text is assembled in one step and embedding takes hours in another, so rebuilding the text without rerunning the embedding pairs every vector with the wrong chunk, and nothing about that failure looks broken. `embed-check` re-embeds a sample and compares.

Two implementation notes. `ort` is used with `load-dynamic`, which loads `libonnxruntime` through `dlopen` at runtime, because its system linking strategy wants a static library and Homebrew ships only a dylib, and its binary download feature does not currently compile against the resolved `ureq`. And the layer normalisation the model card documents for Matryoshka use is immaterial here, measuring above 0.999 cosine agreement at every width: dividing by the standard deviation is a uniform scale the final L2 normalisation cancels exactly, leaving a mean subtraction that barely moves the direction. The flag is kept and the measurement recorded in a test, so its absence cannot look like an oversight.

Grading still reuses the vectors already in Postgres, because feeding both engines byte identical vectors is the only way a score difference measures the index and the ranking rather than the embedding model.

### 3.8 Persistence

An index is a directory of four files: the store, the vectors, the graph and the configuration. Each carries a magic string, a format version and a section tag, so a file written by a different layout is refused rather than misread. The vectors are one raw little endian f32 array, which makes loading them a read rather than a parse.

The graph is stored. The lexical index and the int8 codes are not, and that is deliberate: both are deterministic functions of the store and the vectors, so recomputing them on load costs less than the disk they would occupy. The graph is the one structure where that argument fails, because rebuilding it on this corpus costs the three minutes measured in section 6.

There is no daemon, no port and no background process. Opening an index is opening files.

## 4. The test suite and the grading system

The suite is the real deliverable. An engine nobody has graded is an engine nobody should trust.

Both engines are driven through one trait, so no scenario can accidentally be run against only one of them:

*Design sketch, not yet source.*

```rust
pub trait SearchEngine {
    fn name(&self) -> &str;
    fn vector_search(&mut self, q: &[f32], f: &Filter, k: usize) -> Result<Vec<Hit>>;
    fn lexical_search(&mut self, q: &str, f: &Filter, k: usize) -> Result<Vec<Hit>>;
    fn hybrid_search(&mut self, q: &str, qv: &[f32], f: &Filter, k: usize) -> Result<Vec<Hit>>;
}
```

The pgvector implementation issues the SQL a PostgreSQL and pgvector retrieval stack issues, against the same schema, and fuses with the same Reciprocal Rank Fusion constants, so the baseline is that stack's behaviour and not a reconstruction of it.

It is graded in two configurations. The first is pgvector's extension defaults, reported to show what the extension does before anyone configures it, and nothing is scored against it. The second is a correctly configured PostgreSQL, and it is the one every comparison is scored against, because beating a misconfiguration would prove nothing. Its settings, each chosen from a measured sweep against exhaustive cosine rather than by feel:

| setting | filtered search | unfiltered search | why this value |
|---|---|---|---|
| `hnsw.iterative_scan` | `relaxed_order` | `off` | Without it a filtered search returns almost nothing. On an unfiltered search it changes nothing worth having: recall at 50 was identical with it off and on at every `hnsw.ef_search` tried, 0.9644 at 100, 0.9752 at 200 and 0.9792 at 400, because the only remaining clause excludes 298 chunks of 186,827. Latency was 5.85 ms against 5.98 ms at 100, so the reason to turn it off is that it buys nothing, not that it costs much. When it is off the harness issues `RESET hnsw.max_scan_tuples` and `RESET hnsw.scan_mem_multiplier` rather than leaving them set, so an unfiltered query cannot inherit a filtered query's scan budget on the same connection. |
| `hnsw.ef_search` | 400 | 100 | A scan cannot return more rows than it collected, so this has to be at least the requested row count. Higher raises recall inside a filter. |
| `hnsw.max_scan_tuples` | 40,000 | not applicable | Measured against 200,000, mean recall was 0.788 either way, so the larger value only costs latency. |
| `hnsw.scan_mem_multiplier` | 4 | not applicable | At the pgvector default of 1 the iterative scan exhausts its memory budget and stops early, returning as few as 30 rows of 50 and holding mean recall to 0.788. At 4 the short results stop and mean recall reaches 0.856. At 8 nothing changes. |
| ordering | `relaxed_order` rather than `strict_order` | not applicable | 0.856 against 0.727 mean recall at the same cost, and Reciprocal Rank Fusion recomputes the ranking, so nothing downstream depends on the within scan ordering. Those figures come from the private corpus this engine was first graded on, measured against a different database. They are the reason the setting has the value it has, not a result this repository reproduces. |

`hnsw.scan_mem_multiplier` is the setting most easily missed, and leaving it unset produces a baseline that looks tuned and is not. It is written down here rather than left only in the code for that reason.

### 4.1 Ground truth

Three independent sources of truth, none of which requires a human to judge results:

**Exhaustive cosine, for vector accuracy.** For a query vector and a filter, the correct answer is the exact top k by cosine distance over the chunks passing the filter, computed by scanning all of them. This is the standard measure for approximate nearest neighbour search and it is not an opinion.

**SQL, for filter and lexical correctness.** The set of chunks passing a filter is a `SELECT`. Whether a chunk contains a literal identifier is a `SELECT`. Any hit outside the SQL answer is a correctness failure, not a ranking difference.

**Document identity, for end to end retrieval quality.** Take a document, use its own title as the query, and the correct answer is a chunk of that document. Titles in this corpus are written by people to describe their own content, so they behave like real queries, and the answer is objective. This is generated at scale, and it grades the whole pipeline including fusion, which the other two do not.

### 4.2 Scenario families

| Family | What it measures | Metric |
|---|---|---|
| Unfiltered vector accuracy | approximation quality with no predicate | recall@10 and recall@50 against exhaustive cosine |
| Filtered vector accuracy, by source | the defect in section 1.2, for all six sources | recall@k against exhaustive cosine over the passing set, plus rows actually returned |
| Filtered vector accuracy, selective predicates | author, `updated_after`, label overlap, and combinations | recall@k, plus rows returned |
| Filter correctness | whether any returned row violates the predicate | exact set agreement with SQL, pass or fail |
| Lexical retrieval, natural language | the `&` semantics problem | success@k and MRR over document identity |
| Lexical retrieval, identifiers | JIRA keys, function names, rare literal strings | success@k against a SQL literal match |
| Hybrid retrieval | the whole pipeline, both fusion methods | nDCG@10, success@1, success@10, MRR |
| Quantization ladder | int8 and each Matryoshka width | recall@10 against exhaustive f32 cosine, bytes per vector |
| Latency | per query cost at fixed accuracy | p50 and p95 per scenario family |
| Build and footprint | cost of getting to a queryable index | build seconds, resident bytes, on disk bytes |
| Invariants | determinism, soft delete exclusion, per document cap, empty and pathological queries | pass or fail |

### 4.3 Metrics, defined

- **recall@k** against a reference answer set: the fraction of the reference top k that the engine also returned in its top k. The measure of approximation quality.
- **success@k**: the fraction of queries where a correct answer appears in the top k. The measure a person actually feels.
- **MRR**: mean of `1 / rank of the first correct answer`, zero when none appears. Rewards putting the answer first, not merely somewhere.
- **nDCG@10** with binary relevance and the standard `log2(rank + 1)` discount. Sensitive to position throughout the list, not just at the cutoff.
- **Latency**: wall clock per query inside the process, excluding embedding, since both engines share the vectors. Reported as p50 and p95 over at least 200 queries, after a warmup pass, because a mean hides the tail that users notice.

### 4.4 The grade

Each family produces a normalized score in `[0, 1]`, and the score card reports the per family scores, a weighted total, and every underlying number. Correctness families are gates rather than scores: a filter correctness failure or a broken invariant caps the total, because an engine that returns rows it was told to exclude is not a faster engine, it is a wrong one.

The pass condition is parity across all scenarios. Concretely: inillucent must be at least equal to the better of the two pgvector configurations on every accuracy family, with no correctness gate failing. Where inillucent is worse, the score card says so plainly, because a score card that cannot report a loss is not measuring anything.

## 5. Risks

**A handwritten HNSW is easy to get subtly wrong.** A graph bug shows up as slightly reduced accuracy, not as a crash, so it can pass casual inspection. This is why exhaustive search is a first class part of the engine and why unfiltered recall against it is the first scenario: a graph defect is visible as a recall number below what the algorithm should reach at these parameters.

**Document identity ground truth has a bias.** A title as a query favours engines that weight titles, and inillucent indexes chunk content just as the baseline does, so neither engine sees the title as text. The bias that remains is that titles share vocabulary with their own content, which flatters lexical retrieval slightly on both sides equally. It is reported as a caveat rather than hidden.

**The ONNX path needs a system library.** Section 3.7 records the measurements that verify it. It requires `libonnxruntime` present on the machine, installed with `brew install onnxruntime` on macOS, because the Rust binding's automatic download does not currently build. That is a library file rather than a process, so no daemon comes back with it.

**Parity on this corpus is not parity in general.** Every number is measured on one corpus, assembled by this repository from public data, with one embedding model. The suite is reusable against another corpus, but the score card's numbers describe this one.

## 6. Sequence

1. `inillucent-core`: storage, distance, exhaustive search, tokenization, BM25, Reciprocal Rank Fusion. Exhaustive search first, because it is the reference everything else is graded against.
2. `inillucent-bench`: load the corpus and the vectors once into a local cache file, so repeated runs do not repay the cost of reading them.
3. HNSW, then filtered traversal, graded against exhaustive search at each step.
4. Quantization and the Matryoshka ladder.
5. The full suite against both pgvector configurations and inillucent, then iterate on whatever loses.
6. The score card.

Measured cost of step 3 on the whole corpus, for anyone reproducing this: building the index over 186,829 chunks across 39,365 documents takes 176 seconds single threaded and produces a four layer graph with 6,177,312 directed edges, alongside a lexical index of 179,234 distinct stemmed terms across 8,875,007 postings. The vectors occupy 573.9 MB as f32 and 144.2 MB as int8 codes. Pulling the corpus out of PostgreSQL takes 4.2 seconds through a server side cursor and caches to 805 MB.

**Status of the baseline figures in this document.** They come from the first full graded run, whose configured baseline had `hnsw.ef_search` at 100 on filtered searches rather than 400, `hnsw.max_scan_tuples` at 200,000, iterative scan left on for unfiltered searches, and `hnsw.scan_mem_multiplier` never set, so it ran at the pgvector default of 1. Each difference makes that baseline weaker than the settings in section 4, so the pgvector figures quoted here understate a correctly configured PostgreSQL, most of all on filtered recall. The run against the settings in section 4 is in progress and this document will carry its numbers. Measurements of inillucent alone do not depend on the baseline.

The score card is generated by the harness and judges itself: every measurement that compares the engines is scored against the better of the two pgvector configurations, and the losses are listed in their own table at the top. Comparing against the unconfigured extension alone would be easy and would prove nothing. Rows whose columns are inillucent settings rather than engines, the quantization ladder and the `ef_search` sweep, are excluded from that count, because inillucent cannot beat itself.
