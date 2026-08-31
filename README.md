# rust-db

An embedded vector search engine for retrieval augmented generation, over a corpus of workplace documents: pages, chat messages, issues, source files, design files and boards. It does the job of PostgreSQL with the pgvector extension plus `llama.cpp` serving an embedding model over HTTP, in one library that runs inside the calling process. That combination is also the baseline it is graded against.

Two crates:

- `rustdb-core` is the engine. It links no database client. Storage with dictionary encoded filter columns, cosine over L2 normalized vectors, exhaustive search, an HNSW graph with traversal that honours a predicate, int8 scalar quantization, an inverted index with BM25, Reciprocal Rank Fusion and normalized score fusion, and persistence.
- `rustdb-bench` is the grading harness. It builds the corpus, embeds it, loads it into PostgreSQL, and grades both engines. It is the only crate that talks to PostgreSQL, because its job is to query the baseline engine.

The baseline is graded in two configurations. One runs pgvector's extension defaults, to show what the extension does before anyone configures it, and nothing is scored against it. The other is a correctly configured PostgreSQL, and it is the one every comparison is scored against. Its scan settings are `hnsw.iterative_scan = relaxed_order` with `hnsw.ef_search = 400`, `hnsw.max_scan_tuples = 40000` and `hnsw.scan_mem_multiplier = 4` on a filtered search, and `hnsw.iterative_scan = off` with `hnsw.ef_search = 100` on an unfiltered one, where the other two are reset rather than left set so a query cannot inherit a filtered query's scan budget on the same connection. The iterative scan is off on an unfiltered query because it changes neither the rows nor the latency there, the only remaining clause excluding 298 chunks of 186,827. `hnsw.scan_mem_multiplier` is the one most easily missed: left at the pgvector default of 1 the iterative scan exhausts its memory budget and stops early, returning as few as 30 rows of 50.

## The corpus

The engine is graded on a corpus assembled from public data, built by this repository. Every number on the score card can be reproduced by anyone with this repository, an internet connection and a few hours.

It is synthetic in the sense that matters: the six sources, the documents, the titles, the authors, the spaces, the labels and the identifiers are all constructed by `synth.rs`. The sentences inside the chunks are real public text, because a lexical index scored on generated filler measures nothing. Term frequencies, sentence length, vocabulary growth and the way rare words cluster are properties BM25 depends on, and text from a template has none of them.

| source | stands for | built from | licence |
|---|---|---|---|
| confluence | wiki pages | English and Simple English Wikipedia articles | CC BY-SA 4.0 |
| github | source files | eight repositories in eight languages | MIT, BSD 3 Clause, Apache 2.0 |
| slack | chat threads | Wikipedia Talk and User talk pages | CC BY-SA 4.0 |
| jira | issue threads | GitHub issues from those repositories | factual metadata |
| figma | design files | the longest articles, reformatted as frames and text layers | CC BY-SA 4.0 |
| miro | boards | articles reformatted as clustered notes | CC BY-SA 4.0 |

None of this text is committed here. It is downloaded and rebuilt on demand, which keeps the repository small and satisfies the share alike licences by attribution rather than by redistribution. Each source draws from a disjoint pool: if one article supplied both a page chunk and a design file chunk, a query matching one would match its twin in another source and every filtered measurement would be distorted.

### Building it

```sh
# 1. Download the public material. Wikipedia dumps, eight shallow clones and the
#    issue threads. About 2 GB, mostly the clones.
./scripts/fetch-public-corpus.sh

# 2. Turn it into the compact files the builder reads.
python3 scripts/extract-wikipedia.py ~/.cache/rust-db-corpus/raw ~/.cache/rust-db-corpus/derived
python3 scripts/extract-github.py    ~/.cache/rust-db-corpus/raw ~/.cache/rust-db-corpus/derived

# 3. Assemble the corpus: 186,786 chunks across 39,366 documents.
./target/release/rustdb-bench synth-build --out ~/.cache/rust-db-corpus/corpus.jsonl

# 4. Check it supports every graded scenario, and that those scenarios can be
#    answered rather than only generated. Do this before step 5, which is the
#    step that costs hours: skipping it cost two complete embedding runs.
./target/release/rustdb-bench synth-check --corpus ~/.cache/rust-db-corpus/corpus.jsonl

# 5. Embed it. Eight to twelve hours for the full corpus on a laptop processor,
#    depending on how busy the machine is and how many of its cores are the fast
#    kind. Resumable: rerun the same command and it continues where it stopped.
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
./target/release/rustdb-bench synth-embed \
  --corpus ~/.cache/rust-db-corpus/corpus.jsonl \
  --cache  ~/.cache/rust-db-corpus/corpus.cache

# 6. Load the same rows and the same vectors into PostgreSQL for the baseline.
createdb -h 127.0.0.1 -p 5433 rustdb_synth
./target/release/rustdb-bench synth-load \
  --corpus ~/.cache/rust-db-corpus/corpus.jsonl \
  --cache  ~/.cache/rust-db-corpus/corpus.cache
```

`--scale` on `synth-build` multiplies every source's document and chunk count while keeping the proportions between sources, so a smaller corpus can be built for a faster cycle and a larger one to test beyond this size. Embedding time scales with it.

The vectors loaded into PostgreSQL are the same bytes the cache holds. Nothing is recomputed, so a difference between the two engines cannot come from the embedder.

## Running the graded comparison

```sh
cargo build --release
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib

# Build an index and report what it built.
./target/release/rustdb-bench build --cache ~/.cache/rust-db-corpus/corpus.cache --quantized

# Run every scenario against rust-db and both pgvector configurations, and write
# the score card. Takes around twenty minutes, most of it index builds.
./target/release/rustdb-bench grade --cache ~/.cache/rust-db-corpus/corpus.cache --per-source 30

# Iterate on rust-db alone, skipping the two pgvector configurations.
./target/release/rustdb-bench grade --cache ~/.cache/rust-db-corpus/corpus.cache --rustdb-only
```

`grade` writes `rust-db-scorecard.md` and, beside it, the same measurements as JSON so the card can be re-rendered or re-judged without repaying the run.

A prefix of this corpus is not a sample of it. Chunks are numbered in ingestion order and that order correlates with source, so `--limit N` gives nearly all one source. `strided_sample` exists for this reason, and `synth-check` asserts the property still holds.

Defaults: `--database-url postgres://127.0.0.1:5433/rustdb_synth`, `--model-dir ~/.cache/rust-db-models/nomic-embed-text-v1.5`.

## Tests

```sh
cargo test --release
```

The engine's own tests cover the pieces a search engine gets quietly wrong: that filtered traversal returns a full result set on a minority source, that filtered recall against exhaustive cosine stays high, that a filter naming a value the corpus lacks selects nothing rather than everything, that stemming matches what PostgreSQL's `english` configuration produces, that BM25 saturates term frequency and normalizes for document length, that quantization keeps the ranking, and that a saved index answers the same queries after loading.

The corpus builder's tests cover what makes a corpus usable for grading rather than merely large: that chunks stay near their target length and none swallows the rest of its document, that chunking never splits a word or a line of code, that the article pool is shared so no source is starved, that chunk counts per document reproduce the measured quantiles, and that authors are unique and stable across rebuilds.

## Using the engine

```rust
use rustdb_core::filter::Filter;
use rustdb_core::index::{Index, IndexConfig};

let mut index = Index::new(IndexConfig { dims: 768, quantized: true, ..Default::default() });
index.add(chunks, &vectors);   // one vector per chunk
index.commit();                // builds the graph, the lexical index and the codes

let filter = Filter::source("slack");
let compiled = index.compile(&filter);
let hits = index.hybrid_search("how does the release process work", &query_vector, &compiled, 10, None);
```

`exhaustive_search` is a first class query path, not a test fixture. It is exact, the cost model routes selective predicates to it because at that size it is also faster, and it is the reference every accuracy number is measured against.

## Embeddings

The engine takes vectors and never embeds anything itself, which is what lets the harness give both engines identical vectors. `embed.rs` defines the boundary and carries the `search_document: ` and `search_query: ` prefixes `nomic-embed-text-v1.5` is trained with, plus the Matryoshka widths the model supports.

`embed_onnx.rs` runs the model as ONNX through `ort`, in process, with no child process and no HTTP hop. It sits behind the `onnx` feature so the engine stays free of a native library for callers that supply their own vectors. Texts are sorted by length before batching, because every sequence in a batch is padded to the longest one in it and the corpus runs from 6 to 6227 characters a chunk.

It needs the weights and the ONNX runtime library present:

```sh
brew install onnxruntime
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
# weights in ~/.cache/rust-db-models/nomic-embed-text-v1.5/
#   model.onnx  tokenizer.json  tokenizer_config.json
#   special_tokens_map.json  config.json

./target/release/rustdb-bench embed-check --cache ~/.cache/rust-db-corpus/corpus.cache
```

`embed-check` asks whether the vectors in the cache were made from the text in the cache. That is a real failure this pipeline can produce: the text is assembled in one step and embedding takes hours in another, so rebuilding the text without rerunning the embedding pairs every vector with the wrong chunk. Nothing would look broken, the index would build and queries would return rows, and every retrieval number would be quietly wrong.

`ort` uses `load-dynamic` because its system linking strategy wants a static library and Homebrew ships only a dylib. CoreML is not usable for this model: the execution provider registers, fails to compile the rotary embedding operators because `attention_mask` has an unbounded dimension, falls back to CPU, and runs slower than plain CPU.
