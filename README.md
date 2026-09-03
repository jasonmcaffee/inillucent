# rust-db

An embedded vector search engine for retrieval augmented generation, over a corpus of workplace documents: pages, chat messages, issues, source files, design files and boards. It does the job of PostgreSQL with the pgvector extension plus `llama.cpp` serving an embedding model over HTTP, in one library that runs inside the calling process. That combination is also the baseline it is graded against.

Since task-1782 the repository also holds the beginning of a second, separate engine: a first-party
SQL database aiming at SQLite file-format and behaviour parity, designed in
`tasks/task-1781-sqlite-feature-parity-tdd.md`. It shares nothing with the retrieval engine yet and
does not change it. See [The relational engine](#the-relational-engine) below.

Two crates carry the retrieval engine:

- `rustdb-core` is the engine. It links no database client. Storage with dictionary encoded filter columns, cosine over L2 normalized vectors, exhaustive search, an HNSW graph with traversal that honours a predicate, int8 scalar quantization, an inverted index with BM25 that weights a hit by how much of the query it holds and by how tightly those terms sit together, three fusion methods, and persistence.
- `rustdb-bench` is the grading harness. It builds the corpus, embeds it, loads it into PostgreSQL, and grades both engines. It is the only crate that talks to PostgreSQL, because its job is to query the baseline engine.

The baseline is graded in two configurations. One runs pgvector's extension defaults, to show what the extension does before anyone configures it, and nothing is scored against it. The other is a correctly configured PostgreSQL, and it is the one every comparison is scored against. Its scan settings are `hnsw.iterative_scan = relaxed_order` with `hnsw.ef_search = 400`, `hnsw.max_scan_tuples = 40000` and `hnsw.scan_mem_multiplier = 4` on a filtered search, and `hnsw.iterative_scan = off` with `hnsw.ef_search = 100` on an unfiltered one, where the other two are reset rather than left set so a query cannot inherit a filtered query's scan budget on the same connection. The iterative scan is off on an unfiltered query because it changes neither the rows nor the latency there, the only remaining clause excluding 298 chunks of 186,827. `hnsw.scan_mem_multiplier` is the one most easily missed: left at the pgvector default of 1 the iterative scan exhausts its memory budget and stops early, returning as few as 30 rows of 50.

## Where it stands

On the corpus this repository builds, over 2,613 queries in nine families, against the better of the
two pgvector configurations:

**17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates:**
**all pass.**

Every family declares one measurement that is judged; the rest are diagnostics that are printed and
do not vote. A comparison is called *better* only when a 95% paired bootstrap interval over the
per-query scores clears both zero and a practical threshold declared before the run. The one
*equivalent* is a source where both engines reach recall 1.000 within the filter and neither can do
better. The one *inconclusive* is confluence filtered recall, where rust-db leads 0.9960 to 0.9760
and the interval runs 0.0000 to 0.0440 on 25 queries — a lead the run declines to call a win.

| family | measurement | rust-db | best pgvector |
|---|---|---|---|
| Hybrid | document identity, nDCG@10 | **0.9756** | 0.8148 |
| Hybrid | natural language headings, nDCG@10 | **0.7477** | 0.6271 |
| Passage | passage evidence, graded nDCG@10 | **0.7045** | 0.6027 |
| Passage | one transposed character, graded nDCG@10 | **0.6794** | 0.3969 |
| Passage | three keywords, graded nDCG@10 | **0.6266** | 0.4651 |
| Multi-source | evidence in two sources, evidence recall@10 | **0.6237** | 0.1923 |
| Abstention | questions with no answer, confident answer rate | **0.0050** | 1.0000 |
| Lexical | natural language headings, MRR | **0.7221** | 0.5896 |
| Lexical | rare identifiers, MRR | **0.5442** | 0.1357 |
| Filtered | `source = jira`, recall@10 in filter | **1.000** | 0.3280 |
| Latency | no predicate, p50 | **0.704 ms** | 1.519 ms |

The abstention row is the one worth pausing on. Given a question that nothing in the corpus answers,
the baseline returns a confident top result every single time; rust-db does it on one query in two
hundred. That is not a ranking difference, it is the difference between a system that can say "no"
and one that cannot — and it is the failure that never announces itself, because ten confident
looking passages about nothing look exactly like ten good ones.

The full card, including every diagnostic, every interval and every measurement that is a rust-db
setting rather than a comparison, is in [rust-db-scorecard.md](rust-db-scorecard.md).

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
| `lexical_phrase` | scales a score by whether the matched terms came in the query's own order inside that window, blended by this weight | 0.75 |

`lexical_phrase` is the one `ts_rank_cd` does not have an answer to. Cover density asks how tightly
the terms sit; it does not ask whether they came in the order the question asked them in, and
"offer eligibility rules" and "rules for eligibility of an offer" have the same window width and are
not the same answer.

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

## How it is graded

### How a measurement becomes a verdict

The card used to count measurements won, with anything above `1e-4` a win. All three parts of that
were wrong in the same direction. `1e-4` is a hundredth of what one query in ninety changing its
mind moves a mean by, so noise was being counted. Every row got a vote, so nDCG, success@1,
success@10 and reciprocal rank turned one behaviour into four wins. And `rows returned` was scored
higher-is-better, so fifty irrelevant chunks beat ten useful ones.

What replaced it:

- **One primary metric per family.** Everything else is a diagnostic: printed, argued about, never
  voted on.
- **Paired statistics.** Every primary comparison is decided by a 95% paired bootstrap interval and
  a paired randomization test over the per-query scores, both seeded so a verdict is reproducible.
  Smucker, Allan and Carterette found these agree with each other and are the right tests for
  retrieval; both are reported because they answer different questions — the interval says how large
  the difference is, the p-value says whether it could be noise.
- **A practical threshold declared before the run.** 0.01 on the ranking measures, five per cent on
  latency. With enough queries every difference eventually becomes detectable, including differences
  far too small to matter.
- **Four verdicts, not three.** *better* when the interval clears both zero and the threshold,
  *equivalent* when the whole interval sits inside it, *worse* in the other direction, and
  *inconclusive* when the run cannot tell. A run that cannot separate two engines says so. "Both
  engines are at the metric's ceiling" is reported separately from "we cannot tell", because those
  are not the same statement.
- **Completeness is a gate.** Returning thirty rows where fifty exist is still a defect; it is just
  not a relevance win.

### What every run leaves behind

An aggregate card can be read but not interrogated. Every run now writes, beside the card:

```
runs/<unix time>-<commit>/
  manifest.json     commit and dirty flag, corpus file and size, model, device,
                    every query seed, every ranking setting, host, thresholds
  per-query.jsonl   one line per engine per query: the ranking, each hit's
                    relevance grade, the component scores, the latency, and the
                    metrics that query contributed
```

That is what makes the intervals recomputable without repaying the run, lets a miss be looked at
rather than guessed at, and lets a run be re-judged after a relevance judgement is corrected.

### The query families

The first three grade a **document**. This corpus writes each document's title and heading into the
front of every one of its chunks — because the corpus it reproduces did — so a title query is
answered by any chunk of the right page. That is worth grading and it is not what an agent needs,
which is the paragraph.

| family | the query | what counts as correct |
|---|---|---|
| document identity | the document's own title | any chunk of that document |
| heading | a section heading | the chunks under it |
| identifier | a rare literal token | the chunks holding it |
| **passage evidence** | one body sentence, with every word of the chunk's breadcrumb removed so it cannot be answered by the shared title text, and the two rarest remaining words removed as a deliberate vocabulary gap | **graded**: 3 for the passage that answers, 2 for the rest of its document |
| **transposition** | the same queries, two adjacent characters swapped in the rarest word | unchanged, so the gap between the two scores is exactly what the mistake cost |
| **shorthand** | the same queries cut to their three rarest content words | unchanged |
| **multi-source** | two headings from documents in two different sources, joined | both sets are answer bearing, and the family is scored on whether **both** arrived |
| **unanswerable** | distinctive words of two documents from sources the builder draws from disjoint pools | nothing is relevant |

The passage family's remaining bias is stated rather than hidden: its words are still drawn from the
passage it grades. It is a much weaker bias than a title query — the container leak is gone and the
two strongest lexical anchors with it — and the ground truth stays objective, which a generated
paraphrase would not.

### Confidence is a different number from score

The unanswerable family is the failure that does not announce itself: ten confident-looking passages
for a question with no answer, and an agent writes a paragraph out of them. Measuring it needs an
absolute notion of confidence, and per-list min-max normalization destroys one by construction —
it maps the best hit of every list to exactly 1.0, whether the list is good or hopeless.

Theoretical min-max fixes that by dividing each side by a bound the results had no say in: cosine
over normalized vectors is bounded by one, and BM25 by the query's own idf mass at saturation. It
took the confident-answer rate on unanswerable questions from 1.000 to 0.000.

As a *ranker* it lost, and for a structural reason. Its lexical bound assumes some chunk could hold
every query term, and a multi-source question is built so that none can, so the whole lexical side
collapses towards zero and the ranking becomes vector-only: 0.446 against min-max's 0.690 on that
family. That is correct behaviour for a confidence and wrong behaviour for an order.

So the engine stopped asking one number to do both. Every hit carries a `score`, from whichever
fusion ranks best, and a `confidence`, always computed on absolute bounds whatever fusion ordered
the list. The abstention threshold is set on confidence, the ranking is decided by score, and each
engine is calibrated on its own scale against held-out answerable queries — so the comparison
assumes nothing about a rust-db score and a `ts_rank_cd` score meaning the same thing.

## The relational engine

A first-party SQL engine, built to SQLite's file format and observable behaviour. It links no
database engine and no SQL parser: `docs/dependency-policy.md` records the rule, and a test walks
every crate manifest and fails on a dependency that breaks it. SQLite appears in this repository in
exactly one form - a pinned 3.53.4 build, compiled from the official amalgamation, run as a child
process, and compared against as a black-box oracle.

The work is sequenced into fifteen phases by the design document. Phases 0 through 7 are done, which
is the point at which the engine reads *and writes*: it creates tables and indexes, inserts, updates
and deletes rows, enforces constraints, runs transactions and savepoints, and commits through a
rollback journal that survives a power loss at every cut point.

Phase 8 is under way and most of it has landed: every join form including `RIGHT` and `FULL`,
compound selects, subqueries in every position, ordinary and recursive CTEs, window functions with
all three frame units and all four `EXCLUDE` forms, views, `STRICT` tables, generated columns both
`VIRTUAL` and `STORED`, `EXPLAIN`, `REINDEX`,
`ANALYZE` with a costed planner that reorders joins on what it measured, and the core, aggregate,
date-time and math built-ins. Triggers, `WITHOUT ROWID` writes, `ALTER TABLE` and
a full `VACUUM` are the parts still to come, and the manifest says so - 25 of phase 8's 31 rows read
`pass`, and the other six read `missing`.

```sql
CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT UNIQUE, score REAL CHECK (score >= 0));
INSERT INTO people(name, score) VALUES('ada', 9.5) RETURNING id, name;
CREATE INDEX people_score ON people(score);
BEGIN; UPDATE people SET score = score + 1; SAVEPOINT s; DELETE FROM people; ROLLBACK TO s; COMMIT;

CREATE VIEW ranked AS
  SELECT name, rank() OVER (PARTITION BY team ORDER BY score DESC) AS place FROM people;
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10)
  SELECT sum(i) FROM n;
SELECT p.name, t.region FROM people AS p LEFT JOIN teams AS t USING (team)
  WHERE p.score > (SELECT avg(score) FROM people);
ANALYZE;
EXPLAIN QUERY PLAN SELECT * FROM people WHERE score > 5;
```

A file rust-db writes is one SQLite opens, reads, `PRAGMA integrity_check`s and keeps writing to,
and the reverse holds too - both directions are tested against the pinned 3.53.4 build rather than
asserted.

| crate | what it holds |
|---|---|
| `rustdb-base` | checked big-endian codecs, the varint, WAL and CRC-32 checksums, page arithmetic, fallible buffers, run-time limits, the stable error table |
| `rustdb-vfs` | the VFS contract plus Windows, POSIX and in-memory implementations, the SQLite byte-range locking protocol, and shared memory |
| `rustdb-value` | values, storage classes, affinity, collation, comparison, and the record codec |
| `rustdb-storage` | the file header, the pager and its page cache, the four B-tree page kinds, cursors, mutation and balancing, the freelist, pointer maps and vacuum |
| `rustdb-transaction` | the rollback journal and its five modes, the four durability levels, hot-journal recovery, and the connection's transaction machine |
| `rustdb-sql` | the lexer, the parser, the arena AST, the binder, and the physical plan |
| `rustdb-catalog` | `sqlite_schema` read and written, the immutable snapshot, and the schema cookie |
| `rustdb-vm` | the opcode set, the compiler, the bytecode verifier, and the machine |
| `rustdb-session` | connections, prepared statements, the statement lifecycle, and DDL |
| `rustdb` | the public facade |
| `rustdb-sim` | a deterministic simulator: layered media, torn and dropped sectors, failure injection, a replayable scheduler, event traces |
| `rustdb-compat` | the parity manifest, the report that gates a release, the oracle protocol, and the dependency-direction check |

Four crates exist as declared layers with no behaviour yet - `rustdb-ext`, `rustdb-capi`,
`rustdb-cli` and `rustdb-search`. They are there so the dependency graph is enforced from the first
commit rather than retrofitted once the edges exist.

### The compatibility report

`compat/sqlite-3.53.4.toml` carries one row per capability rust-db owes, including the ones nothing
has been written for yet: 260 rows, of which 215 pass and 45 are missing. That is the denominator on
purpose. A capability with no row cannot be reported as owed.

A row reaches `pass` only when a test run recorded a passing result for every test it cites, on both
Windows and Linux. The generator refuses a manifest with a duplicated identifier, a claim with no
test behind it, a source link that is not in `compat/sources.toml`, or a release claim with no
recorded platform evidence.

```bash
cargo run -p rustdb-compat --bin rustdb-manifest -- check      # validate the manifest
cargo run -p rustdb-compat --bin rustdb-evidence               # run the suites, record results
cargo run -p rustdb-compat --bin rustdb-manifest -- report     # regenerate compat-report.{json,md}
cargo run -p rustdb-compat --bin rustdb-manifest -- layering   # check the dependency contract
```

### The oracle

```bash
pwsh tools/sqlite-reference.ps1     # Windows
bash tools/sqlite-reference.sh      # Linux
```

Both download the pinned amalgamation and shell, verify them against the SHA3-256 sums sqlite.org
publishes - using rust-db's own SHA3 - and compile `compat/oracle/sqlite_driver.c` into a driver that
speaks the harness protocol. Values cross that protocol as tagged bytes: an integer as its
big-endian hex, a double as its exact IEEE-754 bits, text and blobs as their bytes. A decimal
rendering would compare the harness's formatting rather than the two engines.

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

The relational engine's crates are tested separately, and quickly:

```sh
cargo test -p rustdb-base -p rustdb-vfs -p rustdb-sim -p rustdb-compat
```

They cover what a storage layer gets quietly wrong. Every codec is exercised with hundreds of
thousands of seeded random inputs and must return an error rather than panic on any of them. The
locking protocol is proved across two real processes, not two handles in one, because POSIX advisory
locks are per process and a same-process test would pass on a broken implementation. A dead process
must release its locks. The simulator must lose an unsynced write sometimes and a synced one never,
over every seed. A recorded schedule must replay an event-for-event identical trace. And the same
26-case conformance suite runs against the in-memory VFS, the real file system, and the simulator,
so "the simulator behaves like a disk" is a checked claim rather than a hope.

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
