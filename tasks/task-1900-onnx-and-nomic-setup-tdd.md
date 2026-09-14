# task-1900: one command that installs the embedder, and three answers to where the model lives

## The problem

inillucent can embed text in the calling process. `docs/embeddings.md` has said so since the
`llama-server` child process was removed, and the numbers behind that removal are on the same page.
What it has never said is how a person gets to the point where that works, and the honest answer
today is four manual steps:

1. install ONNX Runtime from somewhere — the page says `brew install onnxruntime`, which is one
   platform out of three;
2. set `ORT_DYLIB_PATH` to the shared library that install produced;
3. find `nomic-embed-text-v1.5`'s ONNX export on Hugging Face, download five files out of a
   repository that holds eight variants of the weights, and put them in a directory;
4. set `INILLUCENT_ONNX_DIR` to that directory, or place it under one of two hard-coded roots, one of
   which was a drive letter on one developer's machine.

Every one of those steps is a place to get it wrong quietly. The wrong ONNX Runtime version refuses
to open a session with a message about an enum value. The wrong weights file — `model_int8.onnx`
looks like a reasonable choice and is 4x smaller — produces vectors that agree with the real ones at
0.97 cosine, which is not an error and is not correct either. A missing `tokenizer.json` fails at
load; a missing `model.json` silently falls back to a baseline contract.

And there is a second question the same ticket asks, which the setup step cannot answer on its own:
**once the model is installed, when is it in memory?** The model is 522 MB of weights. A search
application wants it for the length of one query. An ingestion run wants it for hours. A `.rdb` file
opened by an agent's MCP transport, which answers two questions and exits, wants it for as short a
time as possible. One answer cannot serve all three.

## What this delivers

1. **`inillucent setup-embeddings`** — one command, on Windows, macOS and Linux, that downloads and
   installs ONNX Runtime and the weights into a per-user directory, verifies every byte it fetched
   against a pinned digest, prints a progress bar, and leaves the engine able to embed with no
   environment variable set by hand.
2. **Three residency profiles** — `resident`, `on-demand` and `idle`, selectable at setup and
   overridable per process, with the cost of each measured rather than asserted.
3. **The measurement** that decided the default, as a command anybody can re-run:
   `inillucent-bench embed-residency`.
4. **Nikaya on ONNX**, so the application this engine was built for stops needing a `llama-server`
   process on loopback to embed a query.
5. The pages: `docs/embeddings.md`, the README, the agent skill, the command table the CLI and MCP
   are generated from, and the site.

## What this does not do

- It does not change the default embedding model, its precision, or any ranking setting. The
  quantized exports are measured here and reported; none of them becomes a default.
- It does not add a third-party HTTP or TLS crate. `docs/dependency-policy.md`'s worked argument
  against the `postgres` client applies unchanged to a download client, and the platform's own TLS
  is already reached from `inillucent-remote` for exactly this reason.
- It does not make the `embed` feature default. A caller that supplies its own vectors still links no
  machine learning runtime.

---

## 1. What loading the model actually costs

Measured on this box — Windows, an RTX 5090, weights on a local volume — with
`inillucent-bench embed-residency`: five load-and-drop cycles per arm, twenty embeddings through each
open session, medians reported. The query is one realistic sentence, because the ticket's own example
is a person asking their mail about a flight and a query's token count is what the first `Run` is
charged for.

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

### The one number that decides the design

**Opening the session costs 650 to 800 ms, and nothing available moves it much.** Turning off every
graph optimization saves about 15%. Serializing the optimized graph and loading that instead saves
the same 15% and keeps the fast inference, which makes it the best of the load-time levers and still
leaves 673 ms on the clock. Halving the weights with fp16 takes the open to 469 ms and then loses
three times that on every embedding, because the processor has no fp16 kernels and converts.

The load is reading and materializing half a gigabyte of weights. That is a fixed cost, and it is
about 20x to 70x what an embedding through an already-open session costs.

So a search that takes 12 ms with the model resident takes about 800 ms if the model is loaded for
it. That is the answer to the ticket's question, and it is what makes three profiles necessary rather
than one.

### What the other levers found

**Four intra-op threads is the free win on the processor.** 21.4 ms an embedding against 36.4 ms at
the default and 54.4 ms pinned to one thread, with no load-time cost. It is a machine setting, so it
goes on the profile rather than in a manifest.

**int8 is fast and is not free.** 543 ms load-per-query and 18.7 ms resident, against 929 ms and
36.4 ms — and a query vector that agrees with the fp32 one at 0.9727 cosine. That is a retrieval
change, not a speed change, and this repository already has the machinery to price a retrieval change
properly (`inillucent-bench grade-embedding`, `docs/retrieval-quality.md`). Until somebody runs that,
int8 is offered as a selectable weights file and is not a default.

**fp16 on the processor is a trap** and is written down here so nobody has to rediscover it: 116 ms an
embedding, three times slower than the fp32 export it was meant to speed up. On a card it is fine
(17.4 ms) and still not better than fp32 (12.0 ms).

**The first CUDA session in a process costs about 1.6 s** rather than 774 ms, because the driver
context is built with it. A profile that unloads and reloads on a card therefore pays a different
first load from its later ones.

---

## 2. The three profiles

```
resident    load on first use, keep it for the life of the process
on-demand   load for the call, drop when the call returns
idle:<t>    load on use, drop after <t> with no call   (default, t = 5 minutes)
```

| | resident | on-demand | idle |
|---|---|---|---|
| first query after a quiet period | 12–36 ms | ~800 ms | ~800 ms |
| second query straight after | 12–36 ms | ~800 ms | 12–36 ms |
| held between queries | ~1.9 GB | nothing | ~1.9 GB, until the timer |
| what it is for | an ingestion run, a server that searches constantly | a one-shot tool, a process that will exit | a person asking questions |

`idle` is the default because it is the shape of the use the ticket describes. Somebody searching
their mail for a flight asks two or three questions in a row and then stops; they pay the load once,
get the resident latency for the rest of the session, and the machine gets the memory back a few
minutes later. Neither of the other two does that.

`on-demand` is kept because it is the right answer for a process that answers one question and exits
— an agent's MCP transport is exactly this, and `docs/vector-residency.md` already records that this
is the case the vectors were moved out of the heap for. Paying 800 ms once in a process that was
going to exit anyway is cheaper than holding 1.9 GB.

### How it is chosen

In precedence order:

1. `INILLUCENT_EMBED_RESIDENCY` in the environment — `resident`, `on-demand`, `idle`, or `idle:90s`.
2. What `setup-embeddings --residency` wrote into the install state.
3. `idle:5m`.

An environment variable first, because a single process wanting a different answer from the machine's
default is the common case and it should not have to rewrite a file to get it.

### How it is built

`inillucent_core::residency::ManagedEmbedder` holds the policy and a `Mutex<Option<OnnxEmbedder>>`.
Every call takes the lock, loads if there is nothing loaded, runs, and then either drops the session
(`on-demand`), stamps the clock (`resident`), or stamps the clock and makes sure a reaper thread is
awake (`idle`).

The reaper is a thread holding a `Weak` to the shared state, parked on a condition variable with a
timeout. It wakes at the deadline, drops the session if nothing has used it since, and exits. It holds
a `Weak` rather than an `Arc` so that dropping the last handle drops the session immediately rather
than waiting for a timer, and so that a process which creates and discards embedders does not
accumulate threads that keep the weights alive.

The lock is held across the inference call, which serializes concurrent embeddings. That is what the
existing `embed(TEXT)` already does — the session is behind a `Mutex` today — so this changes nothing
about concurrency, and one ONNX session is not usable from two threads at once in any case.

Counted, and reported by `inillucent setup-embeddings --status`: loads, evictions, total time spent
loading, total time spent embedding. A profile whose numbers say it reloaded four hundred times is a
profile chosen wrongly, and that has to be visible without a profiler.

---

## 3. `inillucent setup-embeddings`

```sh
inillucent setup-embeddings                     # runtime + weights, into the per-user directory
inillucent setup-embeddings runtime             # just ONNX Runtime
inillucent setup-embeddings model               # just the weights
inillucent setup-embeddings --status            # what is installed, where, and how it is configured
inillucent setup-embeddings --residency resident
inillucent setup-embeddings --gpu               # the CUDA build of ONNX Runtime
inillucent setup-embeddings --dir D:/inillucent # somewhere else
inillucent setup-embeddings --output json
```

### Where things go

| | |
|---|---|
| Windows | `%LOCALAPPDATA%\inillucent` |
| macOS | `~/Library/Application Support/inillucent` |
| Linux | `$XDG_DATA_HOME/inillucent`, else `~/.local/share/inillucent` |

overridden by `INILLUCENT_HOME`, and inside it:

```
runtime/onnxruntime-1.22.0/lib/onnxruntime.dll     the shared library, and nothing else from the archive
models/nomic-embed-text-v1.5/                      model.onnx tokenizer.json model.json config.json …
embeddings.json                                    what is installed, and the residency profile
downloads/                                         partial fetches, removed on success
```

The engine then finds both without being told. `inillucent_core::install::runtime_library()` returns
the installed shared library and `OnnxEmbedder` hands it to `ort::init_from` before it opens its
first session, so `ORT_DYLIB_PATH` becomes an override rather than a requirement.
`install::model_dir()` looks in the install root first and then in the two roots the code looks in
today, so a machine that already has the weights on `J:` keeps working unchanged.

### What it downloads

**ONNX Runtime 1.22.0**, from the Microsoft release on GitHub, pinned by version *and* by SHA-256 per
platform archive:

| platform | archive |
|---|---|
| Windows x86-64 | `onnxruntime-win-x64-1.22.0.zip` |
| Windows arm64 | `onnxruntime-win-arm64-1.22.0.zip` |
| macOS, both architectures | `onnxruntime-osx-universal2-1.22.0.tgz` |
| Linux x86-64 | `onnxruntime-linux-x64-1.22.0.tgz` |
| Linux aarch64 | `onnxruntime-linux-aarch64-1.22.0.tgz` |
| Windows x86-64, `--gpu` | `onnxruntime-win-x64-gpu-1.22.0.zip` |
| Linux x86-64, `--gpu` | `onnxruntime-linux-x64-gpu-1.22.0.tgz` |

1.22.0 rather than the newest release for two reasons. It is the version this workspace's numbers
were taken on, and 1.22.0 is the last release that publishes a `universal2` macOS archive — after it,
macOS is two archives and an installer that guesses wrong on one of them is a support question.

The version is a flag, so a machine that wants a newer runtime can have one; a version with no pinned
digest is fetched and reported rather than refused, and says in its output that it was not verified.

**`nomic-embed-text-v1.5`**, from Hugging Face, five files, each pinned by SHA-256:

```
onnx/model.onnx          547,310,275 bytes   147d5aa8…
tokenizer.json               711,396 bytes   d241a60d…
tokenizer_config.json          1,191 bytes
special_tokens_map.json          695 bytes
config.json                    2,538 bytes
```

and `model.json` — the manifest that says what the model is — written locally rather than downloaded,
because it is this repository's contract and not the model author's. The digests it carries are
recomputed from the files that were actually installed, so a manifest can never describe weights that
are not there.

### How it downloads

`inillucent-remote::http` — a GET over the same verified TLS the migration clients use, following
redirects across hosts because both Hugging Face and GitHub redirect to a content network. Resumable
with a `Range` header, so a 547 MB fetch that dies at 80% resumes rather than restarting. No new
dependency: the argument in `docs/dependency-policy.md` against pulling in a client for the
PostgreSQL protocol applies to an HTTP client identically, and `inillucent-remote` already has the
socket, the bounded reader and the platform's TLS.

`inillucent-remote::archive` — zip and tar.gz, over `inillucent_base::deflate::inflate`, which is
already in the workspace and already checked against the format. **Only the shared library is
extracted**, not the 70 MB of headers, import libraries and provider stubs the archive also holds.

A refusal rather than a warning at every step. A digest that does not match deletes what it fetched
and says which file and both digests. An archive with no shared library in it says so rather than
reporting success over an empty directory. A member whose name escapes the destination directory
stops the extraction: an installer that writes outside where it said it would write is the bug this
check exists for, and it is checked by a test with a `../` member in it.

### The progress bar

One line, rewritten in place, on standard error so `--output json` on standard output stays parseable:

```
onnxruntime-win-x64-1.22.0.zip   [##########··········]  47%   34.1/72.4 MB   18.2 MB/s   00:02
```

and nothing at all when standard error is not a terminal, because a progress bar in a log file is
noise. It falls back to one line per 10% in that case, so a CI log still shows the download moving.

---

## 4. Nikaya

Nikaya embeds through `EmbeddingClient`, which posts to a `llama-server` on loopback. That server is
a second process to install, start and keep running, it runs the model quantized to Q5_K_M, and
`docs/embeddings.md` already records that removing exactly this arrangement from the engine cost
nothing measurable in retrieval quality.

The swap keeps the service's interface — `embed_documents`, `embed_query`,
`embed_document_truncated`, `is_available` — and changes what is behind it to
`inillucent_core::residency::ManagedEmbedder`. Three things follow from the swap rather than from
choice:

- **The character caps go away.** `MAX_EMBED_INPUT_CHARS` is 720 because llama.cpp answers HTTP 500
  rather than truncating past 512 tokens, and `MAX_EMBED_BATCH_CHARS` is 30,000 because it rejects a
  batch that exceeds its physical batch. The in-process embedder truncates at the manifest's token
  bound and plans its own batches against an attention-memory ceiling, so both caps are replaced by
  the model's real bound and the corpus stops being cut at 720 characters.
- **The calls stop being async.** They become `spawn_blocking`, because inference on a thread that an
  async runtime needs for its other work is how a server stops answering.
- **Residency becomes Nikaya's choice per surface.** The HTTP server ingesting mail wants `resident`;
  the MCP transport, which is spawned per agent session, wants `on-demand`. That is the same argument
  `docs/vector-residency.md` makes about the vectors, applied to the weights.

The `EMBEDDING_DIMENSION` check and the normalization stay: both are cheap, and a vector of the wrong
width reaching the index is the failure they exist to stop.

---

## 5. The contracts this touches

| contract | what changes |
|---|---|
| `docs/invariants/layering.toml` | `inillucent-cli` may depend on `inillucent-core`. It already links it through `inillucent-engine → inillucent-search → inillucent-core`, unconditionally, today — this declares the edge so that where the model is installed has one definition instead of two. |
| `crates/inillucent-cli/src/command/registry.rs` | one new row, `setup-embeddings`, which is what makes it appear in `inillucent help`, in the CLI's argument parsing and in `inillucent-mcp`'s `tools/list` at once. |
| `tests/selection.toml` | a row for the new test target. |
| `docs/dependency-policy.md` | the argument for writing the HTTP client here rather than adding one, beside the two that are already there. |

No new third-party dependency, so `[[external]]` is untouched.

---

## 6. The tests

Written against the standard in `tests/inillucent-testing-tdd.md`, and the rule that bites hardest
here is its second: a test that cannot fail is worse than no test. An installer's tests are mostly
about refusals, and a refusal test that never reaches the refusing branch is exactly the failure that
rule names.

| what | where | what would fail it |
|---|---|---|
| the home directory rule, per platform, and `INILLUCENT_HOME` winning | `inillucent-core::install` | a platform whose path is wrong, or an override that is ignored |
| a residency policy parses from every form it is written in, and refuses the rest | `inillucent-core::residency` | `idle:90s` read as `idle` at the default |
| `on-demand` drops the session and `resident` does not, counted rather than inferred | `inillucent-core::residency` | a policy that holds the weights when it said it would not |
| `idle` drops the session after its timer and reloads on the next call | `inillucent-core::residency` | a reaper that never fires, or one that fires while a call is in flight |
| a zip and a tar.gz round trip, and a member named `../escape` stops the extraction | `inillucent-remote::archive` | an installer that writes outside its destination |
| an HTTP response is parsed from both framings, and a cross-host redirect is followed | `inillucent-remote::http` | a chunked body read as a length-delimited one |
| a fetch whose digest does not match leaves nothing behind | `inillucent-remote::http` | a half-written file left where a later run would trust it |
| `setup-embeddings` appears in the CLI and in MCP with a description on every parameter | `command_parity.rs`, already | a command added to one surface and not the other |
| the command's `--status` on a machine with nothing installed says so and exits 0 | `tests/setup.rs` | a status that reports an install that is not there |

The end-to-end check is the command itself, run on this machine into an empty directory, followed by
`inillucent --db x.rdb query "SELECT length(embed('hello'))"` answering 3072 with no environment
variable set. That is the claim the ticket makes and it is the one worth checking directly.
