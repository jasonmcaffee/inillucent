# Retrieval quality against PostgreSQL with pgvector

This page compares inillucent's search with PostgreSQL and the pgvector extension on the same data.
It gives the results first, then explains how each measurement becomes a verdict, then how to run
the comparison yourself.

**17 graded comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Every correctness gate
passes.**

Every number on this page comes from one run, taken on **20 September 2026** at commit `cd53317`.
`inillucent-scorecard.md` in the repository root is that run's own output file. It holds every
interval, every p-value and every diagnostic.

## Terms used on this page

| Term | Meaning |
|---|---|
| chunk | One searchable piece of a document, about a paragraph. A search returns chunks. |
| pgvector | A PostgreSQL extension that stores vectors and searches them with an HNSW index. It is the baseline. |
| HNSW | The graph index both engines use to find the nearest vectors without comparing against every one. See [the glossary](glossary.md). |
| BM25 | The keyword scoring formula inillucent uses. See [the glossary](glossary.md). |
| `ef_search` | How many candidates an HNSW search keeps while it walks the graph. Higher finds more of the true nearest vectors and takes longer. |
| exhaustive search | Comparing the query with every chunk. The answer is always exact. Every recall figure is measured against it. |
| recall@10 | Of the 10 chunks exhaustive search says are best, the share the engine returned. 1.000 means all ten. |
| mean reciprocal rank (MRR) | For each query, 1 divided by the position of the first correct result, then averaged. 1.0 means the first result was always correct. |
| nDCG@10 | A score from 0 to 1 for the first 10 results that gives more credit to correct results near the top. "Graded" nDCG also gives more credit to a better answer than to a partial one. |
| evidence recall@10 | For a question that needs two documents, the share of the needed documents found in the first 10 results. |
| abstention | Returning nothing, or a result marked as not confident, when nothing in the corpus answers the question. |
| confident answer rate | The share of unanswerable questions where the engine still returned a top result above its own confidence threshold. Lower is better. |
| paired comparison | Both engines answer the same queries, and the verdict is computed from the difference on each query. |
| confidence interval | The range the true difference probably lies in. This page uses a 95% bootstrap interval: the queries are resampled 2,000 times and the middle 95% of the resulting mean differences is kept. |
| p-value | How often a difference at least this large would appear if the two engines were the same. The test flips the sign of each query's difference at random 2,000 times. The smallest value it can report is 0.0005. |
| threshold | The smallest difference that counts, declared before the run: 0.01 for ranking measurements, five per cent for latency. |
| correctness gate | A check that passes or fails. It is never averaged into a score. |

## The results

Each row compares inillucent with the **better** of two pgvector configurations for that row. The
difference column is inillucent's lead in the row's own measurement. Every row except the last three
is a score where higher is better.

| Family | Measurement | inillucent | Best pgvector | Difference | Verdict |
|---|---|---|---|---|---|
| Lexical | rare identifiers, mean reciprocal rank | **0.5455** | 0.1357 | 302% higher | better |
| Two sources | evidence in two sources, evidence recall@10 | **0.6212** | 0.1923 | 223% higher | better |
| Filtered | `source = jira`, recall@10 inside the filter | **1.000** | 0.3280 | 205% higher | better |
| Filtered | `source = github`, recall@10 inside the filter | **1.000** | 0.3320 | 201% higher | better |
| Passage | one transposed character, graded nDCG@10 | **0.6788** | 0.3969 | 71% higher | better |
| Filtered | `source = slack`, recall@10 inside the filter | **1.000** | 0.6120 | 63% higher | better |
| Passage | three keywords, graded nDCG@10 | **0.6259** | 0.4651 | 35% higher | better |
| Lexical | natural language headings, mean reciprocal rank | **0.7222** | 0.5896 | 22% higher | better |
| Hybrid | document identity, nDCG@10 | **0.9774** | 0.8148 | 20% higher | better |
| Hybrid | natural language headings, nDCG@10 | **0.7458** | 0.6271 | 19% higher | better |
| Passage | passage evidence, graded nDCG@10 | **0.7055** | 0.6027 | 17% higher | better |
| Filtered | `source = miro`, recall@10 inside the filter | **1.000** | 0.8840 | 13% higher | better |
| Filtered | `source = confluence`, recall@10 inside the filter | 1.000 | 0.9760 | 2.5% higher | **inconclusive** |
| Filtered | `source = figma`, recall@10 inside the filter | 1.000 | 1.000 | none | **equivalent**, both at 1.000 |
| Abstention | questions with no answer, confident answer rate | **0.0050** | 1.000 | 99.5% fewer | better |
| Latency | no filter, median milliseconds | **0.8462** | 1.492 | 76% faster | better |
| Latency | `source = slack`, median milliseconds | **0.5820** | 1.114 | 91% faster | better |

The confluence row is **inconclusive**. inillucent leads 1.000 to 0.9760 over 25 queries. The 95%
interval on the difference runs from 0.0040 to 0.0480. The low end is below the 0.01 threshold, so
the run does not call it a win.

The lexical rows have one pgvector column. PostgreSQL's keyword search has no `hnsw.*` settings, so
the two pgvector configurations give the same keyword results. The latency rows compare with
pgvector at its defaults, because that configuration is faster. [Latency](#latency) also compares
with the configured one.

### Intervals and p-values

The 15 comparisons with a score on every query are decided by a paired test. `n` is the number of
queries. `moved` is how many of those queries the two engines scored differently.

| Measurement | Difference | 95% interval | p | n | moved |
|---|---|---|---|---|---|
| `source = confluence` | 0.0240 | 0.0040 to 0.0480 | 0.1244 | 25 | 4 |
| `source = github` | 0.6680 | 0.5280 to 0.7960 | 0.0005 | 25 | 24 |
| `source = slack` | 0.3880 | 0.3120 to 0.4640 | 0.0005 | 25 | 25 |
| `source = jira` | 0.6720 | 0.5360 to 0.7960 | 0.0005 | 25 | 22 |
| `source = figma` | 0.0000 | 0.0000 to 0.0000 | 1.0000 | 25 | 0 |
| `source = miro` | 0.1160 | 0.0520 to 0.1920 | 0.0020 | 25 | 11 |
| lexical, natural language headings | 0.1326 | 0.0855 to 0.1816 | 0.0005 | 300 | 154 |
| lexical, rare identifiers | 0.4098 | 0.3593 to 0.4604 | 0.0005 | 300 | 182 |
| hybrid, document identity | 0.1627 | 0.1324 to 0.1917 | 0.0005 | 600 | 195 |
| hybrid, natural language headings | 0.1187 | 0.0786 to 0.1600 | 0.0005 | 300 | 128 |
| passage evidence | 0.1028 | 0.0782 to 0.1291 | 0.0005 | 414 | 114 |
| passage, one transposed character | 0.2819 | 0.2460 to 0.3184 | 0.0005 | 385 | 186 |
| passage, three keywords | 0.1608 | 0.1298 to 0.1916 | 0.0005 | 414 | 191 |
| evidence in two sources | 0.4289 | 0.3825 to 0.4793 | 0.0005 | 200 | 141 |
| questions with no answer | 0.9950 | 0.9850 to 1.000 | 0.0005 | 200 | 199 |

The two latency rows have no per query pairing. Each is decided by comparing the medians against
the five per cent threshold.

## How a measurement becomes a verdict

```mermaid
flowchart TB
    A["One corpus, embedded once"] --> B["Same vectors to inillucent and to PostgreSQL"]
    Q["Each query, embedded once"] --> C["inillucent answers"]
    Q --> D["pgvector answers, in two configurations"]
    B --> C
    B --> D
    C --> E["A score for every query"]
    D --> E
    E --> F["Correctness gates: pass or fail"]
    E --> G["Difference on each query against the better pgvector"]
    G --> H["95% interval and p-value"]
    H --> I["Compare with the threshold"]
    I --> J["better, equivalent, worse or inconclusive"]
```

1. **One primary measurement per family.** Each family names the one measurement that is judged.
   Every other measurement is a diagnostic. The score card prints diagnostics, and they do not vote.
   nDCG, success@1, success@10 and mean reciprocal rank all move together when one behavior changes.
   Counting each of them would turn one result into four wins.
2. **A paired test.** Both engines answer the same queries. The harness takes the difference on each
   query, then computes a 95% bootstrap interval and a randomization p-value from those differences.
   Both use a fixed seed, `20260901`, so the same run always reaches the same verdict.
3. **A threshold declared before the run.** 0.01 on ranking measurements, five per cent on latency.
   With enough queries any difference becomes detectable, including differences too small to matter.
4. **Four verdicts.**

| Verdict | When |
|---|---|
| better | the whole interval is above the threshold |
| worse | the whole interval is below minus the threshold |
| equivalent | the whole interval is between minus the threshold and the threshold, or both engines score 1.000 |
| inconclusive | anything else: the run cannot tell |

5. **Completeness is a gate.** Returning fewer rows than the filter admits is a defect, however good
   the returned rows are. So the row count is checked as pass or fail and never scored.

## The correctness gates

| Gate | Result |
|---|---|
| Filtered vector search returns every row the filter admits | pass for inillucent on every source |
| Filter correctness | pass: 2,500 returned rows across 11 filter shapes, every one satisfying its filter |
| Invariants | pass: the same answer on every run, the cap on chunks per document, deleted rows excluded, queries of stopwords only, empty and oversized queries, and `k = 0` |

pgvector did not pass the first gate. Each query asked for 50 rows, and the filter admitted more
than 50 on every source.

| pgvector configuration | Sources that came back short | Queries short, of 25 |
|---|---|---|
| extension defaults | all six | 25 on every source |
| correctly configured | github | 12 |
| correctly configured | jira | 9 |
| correctly configured | miro | 1 |

At its defaults, pgvector returned an average of 1.36 rows of 50 on github and 0.32 on slack. The
score card's filtered search table lists every source.

## Abstention

A question that nothing in the corpus answers is built by mixing the distinctive words of two
documents from two sources that share no articles. No chunk holds material from both.

| Measurement | inillucent | pgvector defaults | pgvector configured |
|---|---|---|---|
| confident answer rate on 200 questions with no answer | **0.0050** | 1.000 | 1.000 |
| confidence threshold (5th percentile of the top result on answerable questions) | 0.3474 | 0.0325 | 0.0325 |
| mean top result confidence, answerable minus unanswerable | **0.4424** | -0.0572 | -0.0509 |

pgvector returned a result above its own threshold on all 200 questions. inillucent did on 1 of
200. Each engine's threshold is set on its own scores, from 200 answerable calibration questions the
run does not score. So the comparison does not assume the two engines' scores mean the same thing.

inillucent's confidence is a separate number from the score that orders the results. The score
that ranks best on this corpus scales the top hit of every list to 1.0, so no threshold could be set
on it. [Vector search](vector-search.md#confidence-is-a-separate-number-from-score) explains how
the confidence is computed.

## Latency

Time per query, measured inside the calling process after a warmup pass, in milliseconds.

| Query | inillucent | pgvector configured | inillucent is | pgvector defaults | inillucent is |
|---|---|---|---|---|---|
| no filter, median | **0.8462** | 2.315 | 174% faster | 1.492 | 76% faster |
| no filter, 95th percentile | **1.522** | 3.568 | 134% faster | 2.276 | 50% faster |
| `source = slack`, median | **0.5820** | 36.486 | 6,169% faster | 1.114 | 91% faster |
| `source = slack`, 95th percentile | **0.7109** | 89.848 | 12,539% faster | 2.075 | 192% faster |

The two pgvector columns are two different trades. At its defaults pgvector is quick and returns
incomplete results under a filter. Configured, it returns the rows and takes much longer, because
it repeats the scan until enough rows pass the filter. inillucent's search skips nodes that fail the
filter while it walks the graph and keeps walking until it has enough rows that pass. For the four
smallest sources it compares against every chunk that passes the filter instead of walking the
graph. `source = slack` is one of those four.

inillucent is a library inside the calling process. pgvector is reached over a loopback network
connection, and that round trip is part of its time. That is a real cost in a deployed system, and
it is separate from index quality. Read the latency rows together with the ranking rows.

Latency moves between runs more than any other measurement. It is wall clock time on a shared
machine. The ranking measurements repeat to four decimal places from run to run. Do not compare
latency figures taken while something else was running on the machine.

## Build cost and footprint

From the same run:

| Measurement | inillucent |
|---|---|
| chunks, documents | 185,078 chunks, 38,847 documents |
| index build time | 16.8 seconds, using every core |
| graph layers, graph edges | 4 layers, 6,116,033 edges |
| keyword terms, keyword postings | 494,293 terms, 11,645,056 postings |
| vectors, 768 dimensions, full precision | 568.6 MB |
| int8 codes for the same vectors | 142.9 MB |

The index is built with every core by default (`HnswParams::build_threads`). A parallel build links
nodes in a different order from a serial build, so its graph differs. The ranking verdicts above
were measured on the parallel build.

The following were measured on the same corpus outside the graded run, and are not on the score
card:

| Measurement | inillucent | PostgreSQL with pgvector |
|---|---|---|
| index on disk | 952 MB, int8 quantized | 800 MB: 722 MB of HNSW and 78 MB of GIN |
| everything the queries read | the 952 MB index | a 1,750 MB database |
| serving process memory | 1,216 MiB in one process, which opens the saved index in 0.8 seconds | a PostgreSQL server whose `shared_buffers` alone is 10,240 MiB on that machine |

PostgreSQL's memory cannot be written as one number next to one process. `shared_buffers` is shared
memory counted against every backend process that touches it.

## Accuracy against exhaustive search

These measure how much of the exact answer inillucent's HNSW graph gives up for speed. They are
diagnostics, reported for inillucent only.

| Measurement | Result |
|---|---|
| recall@10, no filter, 600 queries | 0.9283 |
| recall@50, no filter, 600 queries | 0.9134 |

`ef_search` is the setting a caller changes to trade accuracy for speed:

| `ef_search` | 64 | 128 | 256 | 512 |
|---|---|---|---|---|
| recall@10, no filter | 0.8775 | 0.9525 | 0.9575 | 0.9875 |
| median milliseconds | 0.6076 | 1.077 | 1.938 | 3.273 |

The score card also compares fusion methods and the effect of int8 quantization and fewer
dimensions.

## The query families

The first three families grade finding the right **document**. The corpus writes each document's
title and heading into the start of every chunk, so a title query is answered by any chunk of the
right page. An agent usually needs the right paragraph, so the last five families grade the
paragraph.

| Family | Queries | The query | What counts as correct |
|---|---|---|---|
| document identity | 600 | the document's own title | any chunk of that document |
| heading | 300 | a section heading | the chunks under that heading |
| identifier | 300 | a rare literal token, such as a function name | the chunks holding that token |
| passage evidence | 414 | one body sentence, with every word of the chunk's title and heading removed, and the two rarest remaining words removed | graded: 3 for the passage that answers, 2 for the rest of its document |
| transposition | 385 | the passage queries with two adjacent characters swapped in the rarest word | the same as passage evidence |
| shorthand | 414 | the passage queries cut to their three rarest content words | the same as passage evidence |
| two sources | 200 | two headings from documents in two sources, joined | scored on whether **both** documents arrived |
| unanswerable | 200 | distinctive words of two documents from sources that share no articles | nothing is correct |

Transposition and shorthand change the query and keep the correct answers. The drop from the
passage score measures what the typo or the short query cost.

The passage family still has one bias. Its words come from the passage it grades. Removing the
title words and the two rarest words makes that bias much weaker than a title query's. The correct
answers stay objective.

## Where the correct answers come from

No person judges any result.

| Source of truth | Used for |
|---|---|
| Exhaustive search | the correct nearest chunks for vector search |
| Direct SQL queries on the corpus | which chunks a filter admits, and which chunks contain a string |
| The corpus structure | a document's own title as the query, with any chunk of that document correct. A title shared by two documents is skipped |

## The baseline

The baseline is PostgreSQL with pgvector configured for this workload. It runs the same SQL against
the same schema and reads the same vectors. Its HNSW index uses `m = 16` and `ef_construction = 64`.
It combines its keyword and vector results with the same fusion method inillucent uses: the harness
sets one method on both engines.

pgvector is graded twice. `pgvector (extension defaults)` has nothing set. `pgvector (correctly
configured)` sets these values on each query:

| Setting | Query with a filter | Query with no filter | Why |
|---|---|---|---|
| `hnsw.iterative_scan` | `relaxed_order` | `off` | Without an iterative scan a filtered search returns almost nothing. On an unfiltered search it changed nothing |
| `hnsw.ef_search` | 400, or the rows requested if larger | 100, or the rows requested if larger | A scan cannot return more rows than it collected |
| `hnsw.max_scan_tuples` | 40,000 | reset | Raising it to 200,000 left mean recall at 0.788 and only cost time |
| `hnsw.scan_mem_multiplier` | 4 | reset | At the default of 1 the scan runs out of memory budget and stops early, returning as few as 30 rows of 50. At 4, mean recall rose from 0.788 to 0.856. At 8 nothing changed |

`relaxed_order` reached mean recall 0.856 against 0.727 for `strict_order` at the same cost. The
recall figures in this table were measured on the private database the baseline was first validated
against, and are the reason for each value. This repository's corpus does not reproduce them.

`hnsw.scan_mem_multiplier` is the setting most often missed. It needs pgvector 0.8.0 or later.

`pg_session_settings` in `crates/inillucent-bench/src/engine.rs` builds these statements.

## Running it

The corpus is built from public data. [Synthetic corpus](../tests/synthetic-corpus.md) describes
every step, from download to a loaded PostgreSQL database.

This is the command the 20 September run used, with the password removed:

```sh
inillucent-bench grade --database-url postgres://postgres:***@127.0.0.1:5433/inillucent_synth \
  --cache corpus.cache --model-dir nomic-embed-text-v1.5 --device cuda:0 \
  --per-source 100 --runs-dir runs --out inillucent-scorecard.md
```

| `inillucent-bench` command | What it does |
|---|---|
| `build --cache <file> --quantized` | builds an index from the cache and reports what it built |
| `grade --cache <file>` | runs every family against inillucent and both pgvector configurations and writes the score card |
| `grade --cache <file> --inillucent-only` | skips both pgvector configurations, for work on inillucent alone |
| `tune --cache <file>` | builds one index and sweeps the ranking settings against it |

Defaults for `grade`:

| Flag | Default |
|---|---|
| `--database-url` | `postgres://127.0.0.1:5433/inillucent_synth` |
| `--model-dir` | `~/.cache/inillucent-models/nomic-embed-text-v1.5` |
| `--device` | `cpu` |
| `--per-source` | 40 title queries per source |
| `--out` | `inillucent-scorecard.md` in the working directory |
| `--runs-dir` | `runs` |
| `--stats-seed` | `20260901` |

Set `ORT_DYLIB_PATH` to the ONNX Runtime library before `grade`, because `grade` embeds its queries
in process. Every ranking setting has a flag, and each default is the value the score card was
measured with.

`grade` rebuilds the index on every run. `tune` builds one index and asks it the same questions
under many settings. It swept 55 settings in 31 seconds on an 18,685 chunk corpus. `--seed-offset`
(default 100) shifts the query seeds, so a setting is chosen on queries the graded run does not use:

```sh
inillucent-bench tune --cache corpus.cache --coverages 0,1,2,3 --proximities 0,0.5,1 \
  --weights 0.2,0.35,0.5 --prefixes true,false --tiers true,false --seed-offset 100
```

### What every run writes

```
inillucent-scorecard.md   the score card, and the same measurements as JSON beside it
runs/<unix time>-<commit>/
  manifest.json           commit, corpus, model, device, query seeds, ranking settings, host, thresholds
  per-query.jsonl         one line per engine per query: the ranking, each hit's relevance grade,
                          the component scores, the latency, and the measurements that query added
```

The per query file lets anyone recompute the intervals without running the search again, look at a
single miss, or judge a run again after a correct answer is fixed. The 20 September run wrote 8,139
lines.

## What these numbers do not cover

- **One corpus and one embedding model.** The corpus is 185,078 chunks, embedded with
  `nomic-embed-text-v1.5` at 768 dimensions. The harness works on any corpus. These figures
  describe this one.
- **Title queries favor keyword search.** A title shares words with the text under it. That helps
  both engines equally, so the comparison holds, but the absolute document identity scores are
  high.
- **The graph and the keyword postings are held in memory.** That suits a corpus that fits in
  memory, as this one does.
- **Only searching was measured.** Writing new rows into an index was not graded here.
- **A prefix of this corpus is not a sample of it.** Chunks are numbered in the order they were
  added, and that order follows the source. `--limit N` gives nearly all one source. The harness
  samples with a stride instead, and `synth-check` checks that the ordering still follows the
  source.
