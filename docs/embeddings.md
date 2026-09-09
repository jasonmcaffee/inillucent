# Embeddings

The retrieval engine takes vectors. It does not require you to produce them here — that is what lets
the grading harness hand two engines identical vectors — but it can produce them, in your own
process, with no embedding server and no socket.

## The model in your process

`embed_onnx.rs` runs `nomic-embed-text-v1.5` through ONNX Runtime, in process, at full precision. It
sits behind the `onnx` feature, so an application that supplies its own vectors never links a native
machine learning runtime.

From SQL, `embed(TEXT)` returns the 3,072 bytes of a 768 component vector ready to store in a
`VECTOR(768)` column. It is behind `--features embed` and off by default, for the same reason: a SQL
engine that linked a machine learning runtime whether or not anybody asked would charge the binary
size and the load time to every caller, and most callers supply their own vectors.

`embed.rs` defines the boundary. It carries the `search_document: ` and `search_query: ` prefixes the
model is trained with, and the narrowed widths the model supports.

### What it needs

```sh
brew install onnxruntime
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib

# the weights, in ~/.cache/inillucent-models/nomic-embed-text-v1.5/
#   model.onnx  tokenizer.json  tokenizer_config.json
#   special_tokens_map.json  config.json
```

One shared library file, not a process. Nothing has to be running.

`ort` uses `load-dynamic` because its system linking strategy wants a static library and Homebrew
ships only a dynamic one.

### Was replacing the server actually safe

It was measured rather than assumed. Over 400 chunks embedded both ways, mean cosine similarity was
**0.9860** with none below 0.95. The remaining difference is expected: the server was running the
model quantised to Q5_K_M and inillucent runs it at full precision.

Similarity is not what decides it. Retrieval quality is. Running 120 queries through the same index,
once with each set of query vectors:

| | an embedding server over HTTP | inillucent, same process |
|---|---|---|
| correct answer ranked first | 0.8250 | **0.8250** |
| correct answer in the top ten | 0.8917 | **0.8917** |
| overall rank quality | 0.8509 | 0.8487 |

Identical on the first two and within 0.002 on the third. The two disagree on the first result for
12% of queries and are equally often correct, so the disagreement is reshuffling among equally good
answers.

Those three figures were taken against a corpus that is no longer distributed, because they need two
embedders running over the same text and this repository no longer ships the server. They are the
measurement that justified removing the server, and this repository cannot re-run them. What it *can*
check is that the vectors in a cache were produced from the text in that cache, which is a different
question and a necessary one — see [embed-check](#embed-check).

## Embedding a corpus

```sh
./target/release/inillucent-bench synth-embed \
  --corpus  ~/.cache/inillucent-corpus/corpus.jsonl \
  --cache   ~/.cache/inillucent-corpus/corpus.cache \
  --devices cuda:0,cuda:1 --batch 64 --window-batches 16
```

Resumable: run the same command again and it continues from where it stopped.

Texts are sorted by length before batching, because every sequence in a batch is padded to the
longest one in it and the corpus runs from 6 to 6,227 characters a chunk.

### Running it on GPUs

`--devices` names the processors: `cpu`, `cuda`, `cuda:1`, or several separated by commas. Each gets
its own session, and a window of the corpus is split across them by total text length — longest first
into whichever device is least loaded — so a genuinely slower card is simply given less of the next
window. The vector file is still appended in strict corpus order, so chunk N is record N whatever ran
it, and the run stays resumable.

**Measured on 185,078 chunks: 7 minutes 10 seconds**, against eight to twelve hours on a laptop
processor. Where that comes from, on an 18,685 chunk corpus at batch 64:

| configuration | wall clock |
|---|---|
| one session, one card | 38 s |
| two sessions, one card | 17 s |
| two sessions, two cards | 18 s |
| four sessions, two cards | 22 to 27 s |

Two sessions is 2.1x, and **a second card is worth nothing over a second session on the first one**.
A 137M parameter encoder does not saturate a modern GPU. What a second session hides is the host side
serial work between inference calls: tokenising, and mean pooling a `[batch, seq, 768]` tensor. A
fourth session is worse than two, because that work starts contending with itself.

The CUDA execution provider is registered with `error_on_failure`. `ort` defaults to logging the
failure and falling back to the processor, which is the worst outcome available here: a run meant to
take an hour silently becomes one that takes a day, and nothing in the output says why. Set
`INILLUCENT_CUDA_BIN` and `INILLUCENT_CUDNN_BIN` to preload the libraries from a specific install
rather than finding them on `PATH`.

CoreML is not usable for this model. The execution provider registers, fails to compile the rotary
embedding operators because `attention_mask` has an unbounded dimension, falls back to the processor,
and runs slower than plain CPU.

### Why a batch is bounded by tokens rather than by texts

Attention allocates one score per pair of positions per head, so a batch's memory grows with the
**square** of its longest sequence. `batch_size` alone does not bound it: 64 texts at the 1,900 token
limit asks ONNX Runtime for 64 × 12 × 1900² × 4 bytes, which is 11.1 GB in one allocation, and a
corpus run dies 45% of the way through.

So texts are tokenised once up front, sorted by true token count, and grouped against
`max_batch_cells`, a ceiling on *texts in the batch × longest sequence, squared*. A single text that
exceeds the budget on its own is still run, because refusing it would drop a chunk from the corpus.

**The budget counts cells and does not know what a cell costs**, and what it costs is the model's head
count. `--max-batch-cells` defaults to 24,000,000, calibrated on a 12 head encoder: ONNX Runtime's
fused attention wants roughly `cells × heads × 4 bytes × 2.4`, so the same budget is about 2.8 GB at
12 heads and **3.7 GB at 16**. `qwen3-embedding-0.6b` has 16 heads and failed twice on a 32 GB card at
exactly that ceiling — once on one text of 5,327 tokens (28.4M cells, 3.76 GB) and once on 29 texts of
895 tokens (23.2M cells, 3.61 GB), two batch shapes with nothing in common but their cell count.
Lower the budget for a model with more heads than the default assumes; 8,000,000 brings that model's
worst batch to 1.1 GB.

It is a flag rather than a field in the model manifest on purpose. It describes the card, not the
model, and a manifest field would move every manifest digest whenever somebody tuned a batch — which
would make every cache on disk unreadable to a comparison, for a reason that has nothing to do with
any model.

## embed-check

```sh
./target/release/inillucent-bench embed-check --cache ~/.cache/inillucent-corpus/corpus.cache
```

`embed-check` asks whether the vectors in the cache were made from the text in the cache.

That is a real failure this pipeline can produce. The text is assembled in one step and embedding
takes hours in another, so rebuilding the text without running the embedding again pairs every vector
with the wrong chunk. Nothing would look broken: the index would build, queries would return rows,
and every retrieval number would be quietly wrong.

## Comparing embedding models

`grade` holds the embedding constant and compares engines, and it says so in its own caveats. That is
the right design for grading an index and exactly the wrong one for grading an embedder. So
`grade-embedding` is the other half: the engine is held constant and the model varies.

### A model is described by a manifest

`model.json`, beside the weights, rather than by constants in the harness:

```json
{
  "id": "nomic-embed-text-v1.5",
  "dims": 768,
  "mrl_widths": [64, 128, 256, 512, 768],
  "prefixes": { "query": "search_query: ", "document": "search_document: " },
  "pooling": "mean",
  "max_tokens": 1900,
  "model_file": "model.onnx",
  "token_type_ids": true,
  "backend": "onnx",
  "output": "token_embeddings",
  "tokenizer_sha256": "d241a60d…",
  "weights_sha256": "147d5aa8…"
}
```

`inillucent-bench models --dir <d>` seals one, filling in the two file digests.

Every property a model needs in order to be run the way its author intended lives there and nowhere
else, which is what lets eight models share one code path: a BERT export that declares
`token_type_ids`, a ModernBERT export that does not, a decoder export that also wants `position_ids`
and an empty cache, and one model that has no ONNX at all and is served by `llama-server`. The
embedder fills in exactly the inputs the graph declares, read off the session rather than off the
manifest, because the graph is the authority on what the graph needs.

### The cache carries its own provenance

```
INLCACH4  corpus 110858c33ffd  model nomic-embed-text-v1.5  manifest db59adb9504a
          185078 chunks  768 dims  1900 max tokens  34 truncated  seeds d28c510dda24
```

So a comparison can refuse rather than warn:

```sh
./target/release/inillucent-bench grade-embedding \
  --cache-set caches/nomic-embed-text-v1.5.cache \
  --cache-set caches/gte-modernbert-base.cache \
  --baseline nomic-embed-text-v1.5 --device cuda:0 --out embedding-scorecard.md
```

It reads every cache's header, and nothing else, before loading a single vector, and stops with a
named error if two caches disagree on the corpus digest, the chunk count or the query seed table; if
a cache's width contradicts its manifest; if a manifest has been edited since the vectors were made;
or if a cache carries no provenance at all. Refusing costs a few hundred bytes of reading. Finding
out half way through costs the run.

### The four lanes

**Dense** is exhaustive cosine over every chunk, with no graph, no keyword side and no fusion: the
embedding on its own. The graph reaches 0.925 recall against exhaustive cosine on this corpus, and
letting 7.5% of the answer move for reasons unrelated to the model would be larger than the effect
being measured.

**Hybrid** is the real pipeline with the shipped ranking settings, applied identically to every arm —
left exactly as `IndexConfig::default()` built them rather than copied into setters, because a second
copy of a default is somewhere the two can drift. A model that wins in isolation and loses once BM25
is fused beside it has not helped an agent.

**Cost** is chunks per second on the processor and on `cuda:0`, weights on disk, tokens per chunk, and
the share of the corpus each arm truncated. It is timed on **distinct** chunks sampled by stride: a
benchmark that embeds the same input repeatedly reports roughly three times the real rate on this
machine, because shared prefixes collapse in the prompt cache. Truncation is printed beside throughput
because a model that is fast for having read less of each chunk is not fast.

**Narrowed widths** is each model's shortened ranking against its own full width exact ranking, so the
storage saving is priced per model instead of taken from a model card. A model with no such training
appears only at its full width.

Every primary row is judged by the same paired bootstrap and randomisation test `grade` uses, against
the same 0.01 practical threshold, and per query rows go to `runs/<id>/per-query.jsonl` with a
`model / lane` column so a miss can be compared model against model.

### Two things this found before they became numbers

`snowflake-arctic-embed-m-v2.0` ships `padding: BatchLongest` and `truncation: max_length 512` inside
its own `tokenizer.json`. Its first run reported **exactly 512.0 tokens for every chunk** in a corpus
whose median chunk is about 240: every text padded to the batch maximum with the padding attended to
as though it were text, and every text cut at 512 before the harness could see its real length. The
arm would have been graded as an 8,192 token model with a truncation share of zero. Every tokenizer is
now disarmed on load, so the manifest's bound is the only bound.

The manifest digest covers every field, and adding one field half way through an embedding run moved
every digest and made every cache written before it unreadable to a comparison. That is the guard
working, and the answer is not to soften the digest: `synth-embed` resumes from the vectors already on
disk and stamps the header again, and `embed-check` then embeds a sample again and refuses unless the
stored vectors are what the current manifest produces.

## Narrowing the vectors

`nomic-embed-text-v1.5` advertises the ability to use a prefix of its output. Measured on the graded
corpus, that is expensive here and int8 quantisation is free:

| configuration | bytes per embedding | accuracy |
|---|---|---|
| full, uncompressed | 3,072 | 0.995 |
| full, one byte per number | 772 | **0.995** |
| shortened to 512 numbers | 516 | 0.770 |
| shortened to 256 numbers | 260 | 0.635 |
| shortened to 64 numbers | 68 | 0.345 |

Quantisation is a quarter of the memory at identical accuracy. Shortening the embedding is not
recommended on this corpus, and knowing that is useful precisely because the model card advertises
the capability and the cost is not obvious until it is measured.

## Where to go next

- [Vector search](vector-search.md) — what the engine does with these vectors
- [Retrieval quality](retrieval-quality.md) — the graded comparison against pgvector
- [Synthetic corpus](../tests/synthetic-corpus.md) — building the corpus these numbers are taken on
