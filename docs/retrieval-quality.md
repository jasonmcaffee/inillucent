# Retrieval quality against PostgreSQL with pgvector

**17 graded comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Every correctness gate
passes.**

Both engines read byte identical vectors and are handed the same embedded query, so the embedding
model cancels out and what is left measures indexing and ranking. The baseline is a correctly
configured PostgreSQL, not a default one — [The baseline](#the-baseline) says exactly how it is
configured and why each setting has the value it has.

The corpus is 185,078 chunks assembled from public data by this repository:
[Synthetic corpus](../tests/synthetic-corpus.md) builds it, and every number here can be reproduced
by anyone with this repository, an internet connection and a few hours.

## Ranking

Against the **better** of the two pgvector configurations on every row, never the misconfigured one.
Higher is better everywhere except abstention, where the measurement is how often an engine
confidently answers a question the corpus cannot answer.

| family | measurement | inillucent | best pgvector | |
|---|---|---|---|---|
| Lexical | rare identifiers, mean reciprocal rank | **0.5467** | 0.1568 | 249% higher |
| Filtered | `source = jira`, recall@10 inside the filter | **1.000** | 0.3280 | 205% higher |
| Filtered | `source = github`, recall@10 inside the filter | **1.000** | 0.3320 | 201% higher |
| Multi-source | evidence in two sources, evidence recall@10 | **0.6254** | 0.2104 | 197% higher |
| Passage | one transposed character, graded nDCG@10 | **0.7616** | 0.4460 | 71% higher |
| Filtered | `source = slack`, recall@10 inside the filter | **1.000** | 0.6120 | 63% higher |
| Passage | three keywords, graded nDCG@10 | **0.6868** | 0.5499 | 25% higher |
| Hybrid | document identity, nDCG@10 | **0.9773** | 0.8101 | 21% higher |
| Hybrid | natural language headings, nDCG@10 | **0.7540** | 0.6498 | 16% higher |
| Lexical | natural language headings, mean reciprocal rank | **0.7246** | 0.6346 | 14% higher |
| Filtered | `source = miro`, recall@10 inside the filter | **1.000** | 0.8800 | 14% higher |
| Passage | passage evidence, graded nDCG@10 | **0.7745** | 0.6898 | 12% higher |
| Filtered | `source = confluence`, recall@10 inside the filter | 0.9960 | 0.9720 | **inconclusive** |
| Filtered | `source = figma`, recall@10 inside the filter | 1.000 | 1.000 | **equivalent**, both at the ceiling |
| Abstention | questions with no answer, confident answer rate | **0.0125** | 1.000 | 99% fewer confident wrong answers |

The one **inconclusive** row is confluence, where inillucent leads 0.9960 to 0.9720 and the interval
runs 0.0000 to 0.0440 over 25 queries. The run declines to call that a win.

Fifteen of the seventeen comparisons are tabulated above. The score card carries all seventeen, with
every interval and every p-value beside them.

## The correctness gate

This is the row that matters most and it is not a percentage.

**inillucent returned every row its predicate admits, on every source.** pgvector did not. At the
extension's defaults it returned fewer than the 50 rows the predicate admits on **25 of 25 queries
for every one of the six sources**. Correctly configured it still fell short on github (12 queries of
25), jira (9 of 25) and miro (1 of 25).

An engine that returns fewer rows than the filter allows is not a faster engine, it is an incomplete
one. Completeness is graded as a gate rather than scored as relevance, because returning thirty rows
where fifty exist is a defect however good the thirty are.

## Abstention

Given a question that nothing in the corpus answers, PostgreSQL with pgvector returns a confident top
result **every single time**. inillucent does it on about **one question in a hundred**.

That is not a ranking difference. It is the difference between a system that can say "nothing here
answers that" and one that cannot, and it is the failure that never announces itself: ten confident
looking passages about nothing look exactly like ten good ones, and an agent writes a paragraph out
of either.

[Vector search](vector-search.md#confidence-is-a-separate-number-from-score) explains how the
confidence is computed and why it had to stop being the same number as the score.

## Latency

Median over the same queries, measured inside the calling process. Both pgvector columns are given
because they are two different bargains: the defaults are quick and return incomplete results, and
the configured one returns the rows and pays for them.

| query | inillucent | pgvector, configured | | pgvector, defaults | |
|---|---|---|---|---|---|
| no predicate, p50 | **0.8954 ms** | 2.459 ms | **175% faster** | 1.729 ms | **93% faster** |
| no predicate, p95 | **1.630 ms** | 3.575 ms | **119% faster** | 2.482 ms | **52% faster** |
| `source = slack`, p50 | **0.6631 ms** | 42.182 ms | **6,262% faster** | 1.398 ms | **111% faster** |
| `source = slack`, p95 | **1.292 ms** | 101.038 ms | **7,720% faster** | 1.969 ms | **52% faster** |

The filtered row is the shape of the whole comparison. pgvector's cost of being *correct* under a
filter is to repeat the scan, and that is two orders of magnitude. inillucent's probe widens itself
instead: ask the graph for *k*, run the residual predicate, and if fewer than *k* rows survive, ask
for four times as many. It needs no setting, and it is why the filtered query here takes **less** time
than the unfiltered one rather than sixty times more.

**inillucent pays no network cost, because it is a library, and pgvector pays a loopback round trip.**
That is a real difference in a deployed system rather than a measurement artefact, and it is not a
difference in index quality. Read the latency rows alongside the ranking rows rather than instead of
them.

## Footprint

| | inillucent | PostgreSQL + pgvector | |
|---|---|---|---|
| index on disk, 185,078 chunks | 952 MB, int8 quantised | 800 MB — 722 MB of HNSW plus 78 MB of GIN | 19% more on disk |
| the whole store the queries run against | the 952 MB index | a 1,750 MB database | **46% less on disk** |
| the serving process | **1,216 MiB**, one process, opening a saved index in **0.8 s** | a PostgreSQL server, whose `shared_buffers` alone is 10,240 MiB on this machine | see below |
| processes to keep alive | **none** — it is a library inside the caller | PostgreSQL, plus an embedding server | **two fewer** |

**The resident set row is a note rather than a percentage** because PostgreSQL's memory is not one
number that can be placed beside a single process's. It is a shared memory segment charged to every
backend that touches it, spread over 35 processes on this machine, with two instances running.

The figure that *is* comparable is the one production produced, where the swap was actually made.
[Removing PostgreSQL from a 5.8 GB Gmail assistant](real-world-use-cases/nikaya-postgres-to-inillucent.md)
has the whole account.

## How a measurement becomes a verdict

The scoring system was rebuilt, because the first one counted measurements won with anything above
`1e-4` treated as a win. All three parts of that were wrong in the same direction. `1e-4` is a
hundredth of what one query in ninety changing its mind moves a mean by, so noise was being counted.
Every row got a vote, so nDCG, success@1, success@10 and reciprocal rank turned one behaviour into
four wins. And rows returned was scored higher is better, so fifty irrelevant chunks beat ten useful
ones.

What replaced it:

- **One primary measurement per family.** Everything else is a diagnostic: printed, argued about,
  never voted on.
- **Paired statistics.** Every primary comparison is decided by a 95% paired bootstrap interval and a
  paired randomisation test over the per query scores, both seeded so a verdict is reproducible. Both
  are reported because they answer different questions: the interval says how large the difference
  is, the p-value says whether it could be noise.
- **A practical threshold declared before the run.** 0.01 on the ranking measurements, five per cent
  on latency. With enough queries every difference eventually becomes detectable, including
  differences far too small to matter.
- **Four verdicts, not three.** *better* when the interval clears both zero and the threshold,
  *equivalent* when the whole interval sits inside it, *worse* in the other direction, and
  *inconclusive* when the run cannot tell. "Both engines are at the ceiling" is reported separately
  from "we cannot tell", because those are not the same statement.
- **Completeness is a gate, not a score.**

## The query families

The first three grade a **document**. The corpus writes each document's title and heading into the
front of every one of its chunks, because the corpus it reproduces did, so a title query is answered
by any chunk of the right page. That is worth grading and it is not what an agent needs, which is the
paragraph. The last five grade the paragraph.

| family | the query | what counts as correct |
|---|---|---|
| document identity | the document's own title | any chunk of that document |
| heading | a section heading | the chunks under it |
| identifier | a rare literal token | the chunks holding it |
| **passage evidence** | one body sentence, with every word of the chunk's breadcrumb removed so it cannot be answered by the shared title text, and the two rarest remaining words removed as a deliberate vocabulary gap | graded: 3 for the passage that answers, 2 for the rest of its document |
| **transposition** | the same queries, with two adjacent characters swapped in the rarest word | unchanged, so the gap between the two scores is exactly what the mistake cost |
| **shorthand** | the same queries cut to their three rarest content words | unchanged |
| **multi-source** | two headings from documents in two different sources, joined | both sets bear evidence, and the family is scored on whether **both** arrived |
| **unanswerable** | distinctive words of two documents from sources the builder draws from disjoint pools | nothing is relevant |

The passage family has a remaining bias: its words are still drawn from the passage it grades. It is a much weaker bias than a title query, because the shared container text is
gone and the two strongest keyword anchors with it, and the ground truth stays objective, which a
generated paraphrase would not.

## What every run leaves behind

An aggregate card can be read but not interrogated. Every run writes, beside the card:

```
runs/<unix time>-<commit>/
  manifest.json     commit and dirty flag, corpus file and size, model, device,
                    every query seed, every ranking setting, host, thresholds
  per-query.jsonl   one line per engine per query: the ranking, each hit's
                    relevance grade, the component scores, the latency, and the
                    measurements that query contributed
```

That is what makes the intervals recomputable without repaying the run, lets a miss be looked at
rather than guessed at, and lets a run be judged again after a relevance judgement is corrected.

## The baseline

Beating a badly configured PostgreSQL would prove nothing, so the baseline is a correctly configured
one. It runs the same shape of SQL against the same schema with the same HNSW parameters — `m = 16`,
`ef_construction = 64` — fuses its two result lists with the same reciprocal rank fusion constants,
and reads the same vectors.

Each scan setting was chosen from a measured sweep against an exhaustive comparison:

| setting | filtered | unfiltered | why |
|---|---|---|---|
| `hnsw.iterative_scan` | `relaxed_order` | `off` | without it a filtered search returns almost nothing. On an unfiltered search it changes nothing worth having: recall was identical either way and latency barely moved |
| `hnsw.ef_search` | 400 | 100 | a scan cannot return more rows than it collected, so this has to be at least the number of rows requested, and higher raises recall inside a filter |
| `hnsw.max_scan_tuples` | 40,000 | not applicable | measured against 200,000, mean recall was 0.788 either way, so the larger value only costs latency |
| `hnsw.scan_mem_multiplier` | 4 | not applicable | at the default of 1 the iterative scan exhausts its memory budget and stops early, returning as few as 30 rows of 50 and holding mean recall to 0.788. At 4 the short results stop and mean recall reaches 0.856. At 8 nothing changes |
| ordering | `relaxed_order` | not applicable | 0.856 against 0.727 mean recall at the same cost, and nothing downstream depends on the within scan ordering because reciprocal rank fusion recomputes it |

`hnsw.scan_mem_multiplier` is the one most easily missed, and missing it produces a baseline that
looks tuned and is not.

## Where the correct answers come from

None of the three needs a person to judge a result:

- **Exhaustive comparison** defines the correct answer for semantic search. It compares the query
  against everything, so it cannot be wrong.
- **Direct database queries** define which chunks a filter should admit and which chunks contain a
  given string.
- **Document identity** defines the correct answer for the whole pipeline: take a document, use its
  own title as the query, count any chunk of that document as correct. People write titles to
  describe their own content, so a title behaves like a real query. A title shared by two documents is
  skipped, because the correct answer would then be ambiguous.

## Choosing the ranking defaults without paying for a graded run

A `grade` rebuilds the index every time and the build is most of the run. Nothing in the ranking
settings needs a new index, so `tune` builds one and sweeps every setting against it:

```sh
./target/release/inillucent-bench tune --cache ~/.cache/inillucent-corpus/corpus.cache \
  --coverages 0,1,2,3 --proximities 0,0.5,1 --weights 0.2,0.35,0.5 \
  --prefixes true,false --tiers true,false --seed-offset 100
```

55 settings in 31 seconds on an 18,685 chunk corpus, against 1 minute 19 for one `grade` of the same
corpus and 4 minutes 21 for one on the full one. `--seed-offset` shifts the query set seeds, so a
setting is chosen on queries the graded run will not use.

## Running it

```sh
cargo build --release
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib

# Build an index and report what it built.
./target/release/inillucent-bench build --cache ~/.cache/inillucent-corpus/corpus.cache --quantized

# Run every scenario against inillucent and both pgvector configurations, and
# write the score card. Four and a half minutes on the full corpus, half of it
# the index build. Every ranking setting has a flag and defaults to the measured
# winner.
./target/release/inillucent-bench grade --cache ~/.cache/inillucent-corpus/corpus.cache --per-source 30

# Iterate on inillucent alone, skipping both pgvector configurations.
./target/release/inillucent-bench grade --cache ~/.cache/inillucent-corpus/corpus.cache --inillucent-only
```

`grade` writes `inillucent-scorecard.md` in the repository root, and the same measurements as JSON
beside it, so the card can be rendered again or judged again without repaying the run. That card is
the full detail: every interval, every p-value, every diagnostic, and every measurement that is a
setting rather than a comparison.

Defaults: `--database-url postgres://127.0.0.1:5433/inillucent_synth`,
`--model-dir ~/.cache/inillucent-models/nomic-embed-text-v1.5`.

## What these numbers do not cover

- **One corpus and one embedding model.** The harness works against any corpus; these figures
  describe this one.
- **Where the correct answer is a document's own title**, titles share vocabulary with the text
  beneath them, which flatters keyword search. It flatters both engines equally, so the comparison
  holds, but the absolute figures are optimistic.
- **The graph and the keyword postings are held in memory.** This suits a corpus that fits in memory,
  which this one does. A corpus far larger than the memory available needs a different design.
- **Only searching was measured.** Adding content to an existing index rebuilds the graph.
- **A prefix of this corpus is not a sample of it.** Chunks are numbered in ingestion order and that
  order correlates with source, so `--limit N` gives nearly all one source. `strided_sample` exists
  for this reason and `synth-check` asserts the property still holds.
