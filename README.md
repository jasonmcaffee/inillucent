# rust-db

An embedded vector search engine for retrieval augmented generation, over a corpus of workplace documents: pages, chat messages, issues, source files, design files and boards. It does the job of PostgreSQL with the pgvector extension plus `llama.cpp` serving an embedding model over HTTP, in one library that runs inside the calling process. That combination is also the baseline it is graded against.

Two crates:

- `rustdb-core` is the engine. It links no database client. Storage with dictionary encoded filter columns, cosine over L2 normalized vectors, exhaustive search, an HNSW graph with traversal that honours a predicate, int8 scalar quantization, an inverted index with BM25 that weights a hit by how much of the query it holds and by how tightly those terms sit together, three fusion methods, and persistence.
- `rustdb-bench` is the grading harness. It builds the corpus, embeds it, loads it into PostgreSQL, and grades both engines. It is the only crate that talks to PostgreSQL, because its job is to query the baseline engine.

The baseline is graded in two configurations. One runs pgvector's extension defaults, to show what the extension does before anyone configures it, and nothing is scored against it. The other is a correctly configured PostgreSQL, and it is the one every comparison is scored against. Its scan settings are `hnsw.iterative_scan = relaxed_order` with `hnsw.ef_search = 400`, `hnsw.max_scan_tuples = 40000` and `hnsw.scan_mem_multiplier = 4` on a filtered search, and `hnsw.iterative_scan = off` with `hnsw.ef_search = 100` on an unfiltered one, where the other two are reset rather than left set so a query cannot inherit a filtered query's scan budget on the same connection. The iterative scan is off on an unfiltered query because it changes neither the rows nor the latency there, the only remaining clause excluding 298 chunks of 186,827. `hnsw.scan_mem_multiplier` is the one most easily missed: left at the pgvector default of 1 the iterative scan exhausts its memory budget and stops early, returning as few as 30 rows of 50.

## Where it stands

On the corpus this repository builds, against the better of the two pgvector configurations:

**30 comparable measurements: 26 won, 4 tied, 0 lost. Correctness gates: all pass.**

The four ties are the ceiling of their metric — three sources where both engines return all 50
rows asked for, and one where both reach recall 1.000. Neither engine can exceed those.

The full card, including every measurement that is a rust-db setting rather than a comparison, is
in [rust-db-scorecard.md](rust-db-scorecard.md).

### What the lexical side does that plain BM25 does not

PostgreSQL full text search has two properties BM25 lacks, and both of them matter on a corpus this
size. `to_tsquery` joins query terms with `&`, so a chunk missing one word never appears at all; and
`ts_rank_cd` is cover density ranking, so a chunk whose query terms sit close together outranks one
that mentions the same words in different paragraphs. Scoring any term with BM25 finds far more of
the right chunks — 49.5 rows of 50 against 6.7 — and puts them lower.

rust-db keeps the recall and takes the two properties as gradients rather than gates:

| setting | what it does | default |
|---|---|---|
| `lexical_coverage` | scales a score by the share of the query's idf mass the chunk holds, raised to this exponent | 3.0 |
| `lexical_proximity` | scales it by `matched terms / smallest window holding one of each`, blended by this weight | 1.0 |
| `lexical_tier` | rank by how many query terms a chunk holds first, score second — the ordering `&` gives PostgreSQL | off |
| `lexical_prefix` | let a query term match the terms it prefixes, as `:*` does | off |

Every one of them is measured rather than assumed, and every one has an off switch that restores
plain BM25. `lexical_tier` is off because `lexical_coverage` does the same job better where they
disagree, and on because it is what rescues a caller who sets the coverage exponent to 0.
`lexical_prefix` is off because on a 494,000 term dictionary it credits a chunk with holding a query
term it does not hold, which is the exact judgement coverage weighting depends on.

### Choosing those defaults without paying for a graded run

A `grade` rebuilds the index every time and the build is most of the run. Nothing in the ranking
settings needs a new index, so `tune` builds one and sweeps every setting against it:

```sh
./target/release/rustdb-bench tune --cache ~/.cache/rust-db-corpus/corpus.cache \
  --coverages 0,1,2,3 --proximities 0,0.5,1 --weights 0.2,0.35,0.5 \
  --prefixes true,false --tiers true,false --seed-offset 100
```

55 settings in 31 seconds on an 18,685 chunk corpus, against 1 minute 19 for one `grade` of the same
corpus and 4 minutes 21 for one on the full one. `--seed-offset` shifts the query set seeds, so a
setting is chosen on queries the graded run will not use.

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

# 5. Embed it. On the processor this is eight to twelve hours for the full corpus;
#    on one GPU it is minutes. Resumable either way: rerun the same command and it
#    continues where it stopped.
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
./target/release/rustdb-bench synth-embed \
  --corpus  ~/.cache/rust-db-corpus/corpus.jsonl \
  --cache   ~/.cache/rust-db-corpus/corpus.cache \
  --devices cuda:0,cuda:1 --batch 64 --window-batches 16

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
# the score card. Four and a half minutes on the full corpus, half of it the index
# build. The ranking settings all have flags, and all default to the measured winners.
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

The engine's own tests cover the pieces a search engine gets quietly wrong: that filtered traversal returns a full result set on a minority source, that filtered recall against exhaustive cosine stays high, that a filter naming a value the corpus lacks selects nothing rather than everything, that stemming matches what PostgreSQL's `english` configuration produces, that BM25 saturates term frequency and normalizes for document length, that quantization keeps the ranking, and that a saved index answers the same queries after loading. It also covers the ranking work the score card turns on: that reduce-as-you-go top k selection agrees with sorting every candidate, on ties as well; that coverage weighting raises a chunk holding the whole query and leaves a one term query alone; that proximity prefers the chunk whose terms sit together and that its weight is a real off switch; that the smallest covering window is found, including when the best one is at the end; that tiering puts every-term matches first and still returns the partial ones; that the two score based fusions normalise in the documented way; and that a batch never exceeds the attention budget, never drops a text and never mixes lengths.

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

### Running it on a GPU

`--devices` names the processors the corpus is embedded on: `cpu`, `cuda`, `cuda:1`, or several
comma separated. Each one gets its own session, and a window of the corpus is split across them by
total text length — longest first into whichever device is least loaded — so a genuinely slower
card is simply given less of the next window. The vector file is still appended in strict corpus
order, so chunk N is record N whatever ran it, and the run stays resumable.

The CUDA execution provider is registered with `error_on_failure`. `ort` defaults to logging the
failure and falling back to the processor, which is the worst outcome available here: a run meant
to take an hour silently becomes one that takes a day and nothing in the output says why. Set
`RUSTDB_CUDA_BIN` and `RUSTDB_CUDNN_BIN` to have the libraries preloaded from a specific install
rather than found on `PATH`.

**Measured on 185,078 chunks: 7 minutes 10 seconds against the README's eight to twelve hour**
**estimate on a laptop processor.** What is worth knowing is where that comes from. On the 18,685
chunk corpus at batch 64:

| configuration | wall clock |
|---|---|
| one session, one card | 38 s |
| two sessions, one card | 17 s |
| two sessions, two cards | 18 s |
| four sessions, two cards | 22-27 s |

Two sessions is 2.1x and **a second card is worth nothing over a second session on the first one**.
A 137M parameter encoder does not saturate a modern GPU; what a second session hides is the
host-side serial work between inference calls, tokenizing and mean-pooling `[batch, seq, 768]`. A
fourth session is worse than two, because that work starts contending with itself.

### Why a batch is bounded by tokens rather than by texts

Attention allocates one score per pair of positions per head, so a batch's memory grows with the
**square** of its longest sequence. `batch_size` alone does not bound it: 64 texts at the 1,900
token limit asks ONNX Runtime for 64 x 12 x 1900² x 4 bytes, which is 11.1 GB in one allocation, and
a corpus run dies 45% of the way through. Texts are tokenized once up front, sorted by true token
count, and grouped against `max_batch_cells`, a ceiling on `texts in the batch x longest, squared`.
A single text that exceeds the budget on its own is still run: refusing it would drop a chunk from
the corpus.

### Building on Windows

`ORT_DYLIB_PATH` points at `onnxruntime.dll` from the GPU release rather than at a Homebrew dylib.
Git Bash does not inherit the MSVC `INCLUDE` and `LIB` that `onig_sys` needs, so dump them from
`vcvars64.bat` once and export them into the shell before `cargo build`. `scripts/`
`fetch-public-corpus.sh` resolves the interpreter (`python` where there is no `python3`), falls back
to a hard link where symlinks do not work, and calls `api.github.com` directly when `gh` is not
authenticated — its unauthenticated limit is 60 requests an hour and the corpus needs 48.

If pgvector was installed by running its SQL with absolute paths to `vector.dll`, because the
PostgreSQL install directory is not writable, two things follow. `synth-load` no longer requires
`CREATE EXTENSION vector` to succeed; it checks the type exists instead. And the cluster needs
`dynamic_library_path = '$libdir;C:/path/to/pgvector'`, because pgvector's parallel HNSW build
launches background workers with `bgw_library_name = "vector"`, which is resolved through that path
rather than through the absolute paths in the function definitions.

`ort` uses `load-dynamic` because its system linking strategy wants a static library and Homebrew ships only a dylib. CoreML is not usable for this model: the execution provider registers, fails to compile the rotary embedding operators because `attention_mask` has an unbounded dimension, falls back to CPU, and runs slower than plain CPU.
