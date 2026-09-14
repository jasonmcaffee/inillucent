# Embeddings

The retrieval engine takes vectors. It does not require you to produce them here — that is what lets
the grading harness hand two engines identical vectors — but it can produce them, in your own
process, with no embedding server and no socket.

## Installing it

```sh
inillucent setup-embeddings all
```

That is the whole of it, on Windows, macOS and Linux. It downloads ONNX Runtime and
`nomic-embed-text-v1.5` into a per-user directory, checks every byte against a digest pinned in the
build, prints a progress bar, and leaves the engine able to embed with **nothing exported by hand**:

```sh
inillucent --db notes.rdb query "SELECT length(embed('hello'))"
3072
```

About 620 MB the first time and nothing on a later run. Run it with no component at all and it
reports what is installed and downloads nothing, which is what stops the fetch being a surprise.

**The binary has to carry the `embed` feature for that query to answer, and the published 0.1.2
archives carry it**, because `packaging/release-all.ps1` passes `--features inillucent-cli/embed`.
The 0.1.1 archives do not. Run the query above against one of those and it answers `no such function:
embed`, which makes `setup-embeddings` a command that downloads 620 MB the program that downloaded it
cannot use. Replace a copy installed before 0.1.2, or build the command line from a checkout:

```sh
cargo build --release -p inillucent-cli --features inillucent-cli/embed
```

| | |
|---|---|
| `inillucent setup-embeddings all` | the runtime and the weights |
| `inillucent setup-embeddings runtime` | just ONNX Runtime, 69 MB |
| `inillucent setup-embeddings model` | just the weights, 522 MB |
| `inillucent setup-embeddings --status` | what is there, where, and how it is configured |
| `--residency resident`, `on-demand`, `idle:90s` | when the model is in memory; see below |
| `--gpu` | the build carrying the CUDA execution provider, Windows and Linux on x86-64 |
| `--dir <path>` | somewhere other than the per-user directory |

It is one row in the command table every front end is generated from, so it is also an MCP tool
called `inillucent_setup_embeddings`, with the same parameters and the same description.

### Where it goes

| | |
|---|---|
| Windows | `%LOCALAPPDATA%\inillucent` |
| macOS | `~/Library/Application Support/inillucent` |
| Linux | `$XDG_DATA_HOME/inillucent`, else `~/.local/share/inillucent` |

`INILLUCENT_HOME` overrides it, and inside it:

```
runtime/onnxruntime-1.22.0/lib/onnxruntime.dll     the shared library, and nothing else from the archive
models/nomic-embed-text-v1.5/                      model.onnx tokenizer.json model.json config.json ...
embeddings.json                                    what is installed, and the residency profile
```

The engine finds both without being told. `OnnxEmbedder` hands the installed library to
`ort::init_from` before it opens its first session, so `ORT_DYLIB_PATH` is an override rather than a
requirement, and `INILLUCENT_ONNX_DIR` still names a model directory for a machine that keeps its
weights somewhere the installer would never have put them.

`INILLUCENT_ONNX_DIR` is only used for the model it actually holds. It names one directory and a
caller asks for one model id, so a build that honoured it unconditionally would hand a caller asking
for one model a directory holding another. Nothing would error: the session opens, vectors come out,
and every neighbour they are ever compared against was made by something else.

### What is pinned, and why by digest

A version pins what was asked for; a digest pins what arrived. The two differ whenever a release
asset is replaced, a content network serves a truncated body, or something in between rewrites it —
and the failure mode of the first two is a shared library that loads and misbehaves rather than one
that refuses. So every archive and every weights file is checked against a SHA-256 in the source, a
mismatch deletes what it fetched and names both digests, and a download is written to a `.part` file
that is renamed only once it matches.

**ONNX Runtime 1.22.0**, rather than the newest release, for two reasons: it is the version this
page's numbers were taken on, and it is the last release Microsoft publishes a `universal2` macOS
archive for. After it, macOS is two archives and an installer that guesses wrong on one of them is a
support question. `--onnxruntime-version` installs another; a version with no pinned digest is
fetched and **reported as unverified**, in the output and in the state file, rather than refused.

**The fp32 export**, `onnx/model.onnx`. That repository publishes seven other exports whose names
differ by a suffix, and one of them — `model_int8.onnx` — produces a query vector that agrees with
this one at 0.9727 cosine. That is a retrieval change wearing the clothes of a speed change, which is
why the file is named in a test rather than assembled from a pattern.

**No new dependency was added to fetch any of this.** The download is a `GET` over the platform's own
verified TLS, which `inillucent-remote` already reaches for the PostgreSQL and MySQL clients, and the
zip and gzip reading is over the inflate that is already in `inillucent-base`.
[The dependency policy](dependency-policy.md) carries the argument.

### Where `embed(TEXT)` can be called

Everywhere an expression goes: a projection, a `WHERE` predicate, an `ORDER BY`, a `VALUES` row, an
`UPDATE ... SET`, a `RETURNING` clause and an `INSERT ... SELECT`.

```sql
INSERT INTO note (body, v) VALUES (?1, embed(?1));
UPDATE note SET v = embed(body) WHERE v IS NULL;
SELECT id, body FROM note
ORDER BY vector_distance_cos(v, embed('what time is my plane')) LIMIT 10;
```

The three write shapes were refused with the `unsupported` status and exit code 3 until task-1911:
the write path compiled its expressions against a space built from a table's layout rather than from
a catalog, so the function body was not there to be found and the translation refused by name. It now
takes the catalog as a parameter, which is why a `RowSpace` still carries no lifetime.

### It is called once for the statement, not once for each row

`ORDER BY vector_distance_cos(v, embed('search_query: ' || ?1)) LIMIT 5` embeds the question **once**,
however many rows the table holds. That is the ordinary rule for a deterministic function with
constant arguments — [SQLite states it the same way](https://sqlite.org/deterministic.html) — and
`embed` is registered deterministic because the same text through the same weights gives the same
vector.

It is worth knowing how much this is worth, because the query looks identical either way. Over the
2,661 passages of `examples/rag-agent`, before this was read: **105.7 seconds**, of which all but one
and a half were 2,661 embeddings of one sentence. After: **1.50 seconds**.

The fold happens in two places, and the difference matters if you register a function of your own. A
call whose arguments are all literals is folded once when the statement is compiled. A call that
reads a bound parameter is folded **once per execution**, at setup, because a compiled chain is
re-bound and re-run — folding a parameter at compile time would make the chain correct only for the
values it was built against. A call that reads a column is not folded at all, because it genuinely
differs per row.

A function you register yourself is folded only if you set `FunctionFlags::deterministic`. The
default for anything registered from outside is `false`, which is the safe assumption about code this
engine did not write: folding `random()` would make a whole scan return one number.

## When the model is in memory

The model is 522 MB of weights. Opening a session on it costs **650 to 800 ms**, and an embedding
through an already-open one costs **12 to 36 ms**. So holding it is worth about a factor of thirty on
a query and costs about 1.9 GB of a machine, and which of those matters depends entirely on what the
process is.

```sh
inillucent setup-embeddings --residency idle:5m       # the default, recorded for this machine
INILLUCENT_EMBED_RESIDENCY=on-demand inillucent ...   # for one process
```

| | first query after a quiet period | second query straight after | held between queries | what it is for |
|---|---|---|---|---|
| `resident` | 12 to 36 ms | 12 to 36 ms | about 1.9 GB | an ingestion run, a server that searches constantly |
| `on-demand` | about 800 ms | about 800 ms | nothing | a process that answers one question and exits |
| `idle:<t>` | about 800 ms | 12 to 36 ms | 1.9 GB until the timer | a person asking questions |

`idle:5m` is the default. Somebody searching their mail for a flight asks two or three questions in a
row and then stops: they pay the load once, get the resident latency for the rest, and the machine
gets the memory back a few minutes later. Neither of the other two does that.

`on-demand` is not a fallback. An agent's MCP transport is spawned per session, usually answers two
or three questions and exits, and there can be several at once — paying 800 ms in a process that was
going to exit anyway is cheaper than holding 1.9 GB per session. That is the argument
[vector residency](vector-residency.md) already makes about the vectors, applied to the weights.

In precedence order: `INILLUCENT_EMBED_RESIDENCY`, then what `setup-embeddings --residency` recorded,
then `idle:5m`. The environment first, because one process wanting a different answer from the
machine's is the common case and it should not have to rewrite a file to get it.

The manager counts what it did — loads, evictions, time spent loading, time spent embedding — because
a profile whose numbers say it loaded the model four hundred times is a profile chosen wrongly, and
that has to be visible without a profiler.

## What loading the model costs, and what does not move it

Measured with `inillucent-bench embed-residency` on this box — Windows, an RTX 5090, weights on a
local volume — five load-and-drop cycles per arm, twenty embeddings through each open session,
medians reported. Re-runnable:

```sh
./target/release/inillucent-bench embed-residency \
  --model-dir ~/.cache/inillucent-models/nomic-embed-text-v1.5 \
  --devices cpu,cuda:0 --repeats 5 --steady 20
```

| arm | weights | open | first embed | later embed | drop | load per query | resident per query | agrees with fp32 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| fp32, all optimizations, cpu | 522 MB | 793 ms | 70.7 ms | 36.4 ms | 66 ms | **929 ms** | 36.4 ms | 1.0000 |
| fp32, extended optimizations, cpu | 522 MB | 787 ms | 74.5 ms | 41.0 ms | 63 ms | 924 ms | 41.0 ms | 1.0000 |
| fp32, basic optimizations, cpu | 522 MB | 698 ms | 62.1 ms | 33.4 ms | 50 ms | 810 ms | 33.4 ms | 1.0000 |
| fp32, no optimization, cpu | 522 MB | 681 ms | 60.2 ms | 38.9 ms | 50 ms | 792 ms | 38.9 ms | 1.0000 |
| fp32, pre-optimized graph, cpu | 522 MB | 673 ms | 61.4 ms | 38.5 ms | 49 ms | **784 ms** | 38.5 ms | 1.0000 |
| fp32, 1 intra-op thread, cpu | 522 MB | 734 ms | 55.4 ms | 54.4 ms | 48 ms | 838 ms | 54.4 ms | 1.0000 |
| fp32, 4 intra-op threads, cpu | 522 MB | 731 ms | 20.8 ms | 21.4 ms | 55 ms | 806 ms | **21.4 ms** | 1.0000 |
| fp16, cpu | 261 MB | 469 ms | 287.9 ms | 116.0 ms | 77 ms | 834 ms | 116.0 ms | 1.0000 |
| int8, cpu | 131 MB | 477 ms | 34.5 ms | 18.7 ms | 32 ms | **543 ms** | 18.7 ms | **0.9727** |
| fp32, all optimizations, cuda:0 | 522 MB | 774 ms | 15.5 ms | 12.0 ms | 28 ms | 818 ms | **12.0 ms** | 1.0000 |
| fp32, pre-optimized graph, cuda:0 | 522 MB | 665 ms | 13.5 ms | 12.0 ms | 29 ms | **707 ms** | 12.0 ms | 1.0000 |
| fp16, cuda:0 | 261 MB | 753 ms | 18.6 ms | 17.4 ms | 27 ms | 799 ms | 17.4 ms | 1.0000 |
| int8, cuda:0 | 131 MB | 712 ms | 55.9 ms | 28.9 ms | 35 ms | 804 ms | 28.9 ms | **0.9669** |

**The load is reading and materializing half a gigabyte of weights, and nothing available moves it
much.** Turning off every graph optimization saves about 15%. Serializing the optimized graph and
loading that instead saves the same 15% and keeps the fast inference, which makes it the best of the
load-time levers and still leaves 673 ms on the clock. That one number is what makes three residency
profiles necessary rather than one.

Four other things the run found.

**Four intra-op threads is the free win on the processor.** 21.4 ms an embedding against 36.4 ms at
the default and 54.4 ms pinned to one thread, at no load-time cost. It describes the machine rather
than the model, so it is `OnnxOptions::intra_threads` rather than a manifest field.

**fp16 on the processor is a trap.** 116 ms an embedding, three times slower than the fp32 export it
was meant to speed up, because there are no fp16 kernels there and every weight is converted. On a
card it is fine and still not better than fp32.

**int8 is fast and is not free.** 543 ms load-per-query and 18.7 ms resident, against 929 ms and
36.4 ms — and a query vector that agrees with fp32 at 0.9727. This repository already has the
machinery to price a retrieval change properly ([retrieval quality](retrieval-quality.md), and
`grade-embedding` below); until somebody runs it, int8 is a file you can name and not a default.

**The first CUDA session in a process costs about 1.6 s** rather than 774 ms, because the driver
context is built with it. A profile that unloads and reloads on a card pays a different first load
from its later ones.

One correctness note that only appears at the top of that table: `Optimization::All` maps to ONNX
Runtime's `ORT_ENABLE_ALL` and not to `ort`'s `Level3`. `Level3` is `ORT_ENABLE_LAYOUT`, which is the
value 3 and was only added to the C API in ONNX Runtime 1.23; an older runtime refuses it outright
with `graph_optimization_level is not valid` and the session never opens. `ORT_ENABLE_ALL` is 99 and
has meant "every pass" since the enum existed.

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

`ort` uses `load-dynamic` because its system linking strategy wants a static library and the platform
packages ship only a dynamic one. That is also what lets the installed runtime be chosen at run time
rather than at build time: a machine with no runtime installed still builds, and every command that
does not embed still runs.

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

### A model that has no ONNX export

Not every model can be run this way, and the application this engine was built for is the example.
Nikaya embeds with `nomic-embed-text-v2-moe`, whose Hugging Face repository publishes safetensors and
a sentencepiece tokenizer and **no `onnx/` directory at all** — it is a mixture of experts, which is
the class hardest to export. So its `model.json` says `"backend": "llama_cpp"`, and that is what
decides how it is run rather than a flag somebody has to keep in step with the model. A model with no
export is not something to work around; it is something the manifest describes honestly, and running
it in process would mean running a different model and rebuilding every vector in the corpus.


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
