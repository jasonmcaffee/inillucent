# inillucent Score Card

Generated at unix time 1788238061. Corpus: 185078 chunks across 38847 documents, 768 dimensional embeddings from `nomic-embed-text-v1.5` run in process at full precision.

The corpus is assembled from public data by this repository and embedded once. The identical vectors are written to the cache inillucent reads and to the PostgreSQL column pgvector reads, and every query is embedded once and handed to both engines, so the embedding model cancels out of the comparison entirely. A score difference is therefore attributable to indexing and ranking.

Engines graded:

- inillucent
- pgvector (extension defaults)
- pgvector (correctly configured)

## Verdict

Every family below declares **one** primary measurement, and only those are judged. The rest are diagnostics: they are measured and printed, and they do not vote, because nDCG, success@1, success@10 and reciprocal rank all move together when one behaviour changes and counting each of them separately turns one result into four.

Each primary comparison is against the better of the two pgvector configurations, never the production one, because beating a misconfiguration proves nothing. It is decided by a **paired bootstrap interval** and a **paired randomization test** over the per-query scores, against a practical threshold declared before the run: 0.01 on the ranking measures, five per cent on latency. A difference is called *better* only when the 95% interval clears both zero and that threshold, *equivalent* only when the whole interval sits inside it, and *inconclusive* otherwise. A run that cannot separate two engines says so.

**17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates: all pass.**

No primary measurement was worse than the best the configured PostgreSQL baseline can do.

### Every primary comparison, with its uncertainty

`delta` is inillucent minus the baseline, oriented so positive is better whatever the metric's own direction. The interval is the 95% paired bootstrap on that delta; `p` is the paired randomization test. `n` is the queries behind it and `moved` is how many of them the two engines answered differently — a comparison resting on three queries is worth reading with suspicion however small its p-value.

| family | measurement | metric | inillucent | baseline | delta | 95% interval | p | n | moved | threshold | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Filtered vector search, per source | source = confluence (93813 chunks, inillucent path: graph) | recall@10 within the filter | 0.9960 | 0.9760 | 0.0200 | 0.0000 to 0.0440 | 0.2529 | 25 | 3 | 0.0100 | inconclusive |
| Filtered vector search, per source | source = github (47497 chunks, inillucent path: graph) | recall@10 within the filter | 1.000 | 0.3320 | 0.6680 | 0.5280 to 0.7960 | 0.0005 | 25 | 24 | 0.0100 | better |
| Filtered vector search, per source | source = slack (17616 chunks, inillucent path: exhaustive) | recall@10 within the filter | 1.000 | 0.6120 | 0.3880 | 0.3120 to 0.4640 | 0.0005 | 25 | 25 | 0.0100 | better |
| Filtered vector search, per source | source = jira (9429 chunks, inillucent path: exhaustive) | recall@10 within the filter | 1.000 | 0.3280 | 0.6720 | 0.5360 to 0.7960 | 0.0005 | 25 | 22 | 0.0100 | better |
| Filtered vector search, per source | source = figma (9144 chunks, inillucent path: exhaustive) | recall@10 within the filter | 1.000 | 1.000 | 0.0000 | 0.0000 to 0.0000 | 1.0000 | 25 | 0 | 0.0100 | equivalent, at the ceiling |
| Filtered vector search, per source | source = miro (7396 chunks, inillucent path: exhaustive) | recall@10 within the filter | 1.000 | 0.8840 | 0.1160 | 0.0520 to 0.1920 | 0.0020 | 25 | 11 | 0.0100 | better |
| Lexical retrieval | natural language, from headings | mean reciprocal rank | 0.7221 | 0.5896 | 0.1325 | 0.0855 to 0.1816 | 0.0005 | 300 | 154 | 0.0100 | better |
| Lexical retrieval | identifiers, rare literal tokens | mean reciprocal rank | 0.5442 | 0.1363 | 0.4079 | 0.3574 to 0.4592 | 0.0005 | 300 | 182 | 0.0100 | better |
| Hybrid retrieval, whole pipeline | document identity, title as query | nDCG@10 | 0.9756 | 0.8148 | 0.1609 | 0.1305 to 0.1901 | 0.0005 | 600 | 195 | 0.0100 | better |
| Hybrid retrieval, whole pipeline | natural language, heading as query | nDCG@10 | 0.7477 | 0.6271 | 0.1206 | 0.0817 to 0.1615 | 0.0005 | 300 | 127 | 0.0100 | better |
| Passage evidence, perturbation and multi-source | passage evidence | graded nDCG@10 | 0.7045 | 0.6027 | 0.1018 | 0.0773 to 0.1278 | 0.0005 | 414 | 114 | 0.0100 | better |
| Passage evidence, perturbation and multi-source | passage evidence, one transposed character | graded nDCG@10 | 0.6794 | 0.3969 | 0.2825 | 0.2463 to 0.3185 | 0.0005 | 385 | 185 | 0.0100 | better |
| Passage evidence, perturbation and multi-source | passage evidence, three keywords | graded nDCG@10 | 0.6266 | 0.4651 | 0.1615 | 0.1310 to 0.1922 | 0.0005 | 414 | 192 | 0.0100 | better |
| Passage evidence, perturbation and multi-source | multi-source, evidence in two sources | evidence recall@10 | 0.6237 | 0.1923 | 0.4314 | 0.3850 to 0.4818 | 0.0005 | 200 | 140 | 0.0100 | better |
| Abstention on questions nothing answers | questions with no answer in the corpus | confident answer rate at the calibrated threshold | 0.0050 | 1.000 | 0.9950 | 0.9850 to 1.000 | 0.0005 | 200 | 199 | 0.0100 | better |
| Latency | no predicate | vector search p50 ms | 0.7040 | 1.519 | -0.8150 | not paired | n/a | n/a | n/a | 0.0760 | better |
| Latency | source = slack | vector search p50 ms | 0.8642 | 1.393 | -0.5287 | not paired | n/a | n/a | n/a | 0.0696 | better |

### Queries behind each family

| family | queries |
|---|---|
| abstention calibration (not scored) | 200 |
| document identity | 600 |
| heading | 300 |
| identifier | 300 |
| multi-source | 200 |
| passage evidence | 414 |
| passage evidence, shorthand | 414 |
| passage evidence, typo | 385 |
| unanswerable | 200 |

## Correctness gates

These pass or fail rather than scoring. An engine that returns rows it was told to exclude is not a faster engine, it is a wrong one, so a failure here caps the result regardless of any accuracy number.

| gate | result | detail |
|---|---|---|
| Filtered vector search, per source | pass | inillucent returned every row the predicate admits, on every source. The baseline did not: pgvector (extension defaults) on confluence returned fewer than the 50 rows the predicate admits, on 25 of 25 queries; pgvector (extension defaults) on github returned fewer than the 50 rows the predicate admits, on 25 of 25 queries; pgvector (correctly configured) on github returned fewer than the 50 rows the predicate admits, on 12 of 25 queries; pgvector (extension defaults) on slack returned fewer than the 50 rows the predicate admits, on 25 of 25 queries; pgvector (extension defaults) on jira returned fewer than the 50 rows the predicate admits, on 25 of 25 queries; pgvector (correctly configured) on jira returned fewer than the 50 rows the predicate admits, on 9 of 25 queries; pgvector (extension defaults) on figma returned fewer than the 50 rows the predicate admits, on 25 of 25 queries; pgvector (extension defaults) on miro returned fewer than the 50 rows the predicate admits, on 25 of 25 queries; pgvector (correctly configured) on miro returned fewer than the 50 rows the predicate admits, on 1 of 25 queries |
| Filter correctness | pass | 2500 returned rows across 11 filter shapes, every one satisfying its predicate |
| Invariants | pass | determinism, the per document cap, soft delete exclusion, stopword only and empty and oversized queries, and k = 0 all behave as specified |

## Approximation accuracy against exhaustive cosine

Exhaustive cosine over the whole corpus defines the exact answer, so this is the measure of how much the graph gives up for its speed. It is reported for inillucent only: the same question cannot be asked of pgvector without reading its index internals, and pgvector's own accuracy against exact search is a property of the same HNSW algorithm at the same parameters. A figure well below what HNSW should reach at m = 16 and ef_construction = 64 would indicate a graph defect rather than a tuning choice.

| measurement | metric | inillucent |
|---|---|---|
| all sources, no predicate (600 queries) | recall@10 | 0.9252 |
| all sources, no predicate (600 queries) | recall@50 | 0.9140 |

## The ef_search tradeoff

`ef_search` is how wide the traversal keeps its candidate list, and it is the one knob a caller turns. Accuracy is measured against exhaustive cosine over the whole corpus, latency alongside it, so the default inillucent ships is a choice with a table behind it. The columns are settings, not engines.

| measurement | metric | ef_search = 64 | ef_search = 128 | ef_search = 256 | ef_search = 512 |
|---|---|---|---|---|---|
| no predicate | recall@10 | 0.8975 | 0.9300 | 0.9725 | **0.9800** |
| no predicate | vector search p50 ms | **0.4915** | 0.9073 | 1.410 | 2.066 |

## Filtered vector search, per source

An application with one search tool per source filters on `source` on every call, so this is the common case rather than a corner case. pgvector applies the filter after the index scan and the initial scan yields only `hnsw.ef_search` candidates, so a query restricted to a minority source can come back empty. inillucent expands nodes that fail the predicate but admits only nodes that pass, so the walk continues until it has found enough passing chunks. Recall within the filter is the primary measurement, against exhaustive cosine over the same passing set. Rows returned is a diagnostic and a gate rather than a score: fifty irrelevant chunks are not better than ten useful ones, so a count must never be a relevance win, but coming back with thirty rows when fifty exist is still a defect and that is what the completeness gate is for.

| measurement | metric | inillucent | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| source = confluence (93813 chunks, inillucent path: graph) | rows returned of 50 requested | **50.000** | 34.080 | **50.000** |
| source = confluence (93813 chunks, inillucent path: graph) | recall@10 within the filter | **0.9960** | 0.8160 | 0.9760 |
| source = github (47497 chunks, inillucent path: graph) | rows returned of 50 requested | **50.000** | 1.360 | 34.960 |
| source = github (47497 chunks, inillucent path: graph) | recall@10 within the filter | **1.000** | 0.0680 | 0.3320 |
| source = slack (17616 chunks, inillucent path: exhaustive) | rows returned of 50 requested | **50.000** | 0.3200 | **50.000** |
| source = slack (17616 chunks, inillucent path: exhaustive) | recall@10 within the filter | **1.000** | 0.0280 | 0.6120 |
| source = jira (9429 chunks, inillucent path: exhaustive) | rows returned of 50 requested | **50.000** | 3.080 | 37.880 |
| source = jira (9429 chunks, inillucent path: exhaustive) | recall@10 within the filter | **1.000** | 0.0880 | 0.3280 |
| source = figma (9144 chunks, inillucent path: exhaustive) | rows returned of 50 requested | **50.000** | 0.4800 | **50.000** |
| source = figma (9144 chunks, inillucent path: exhaustive) | recall@10 within the filter | **1.000** | 0.0480 | **1.000** |
| source = miro (7396 chunks, inillucent path: exhaustive) | rows returned of 50 requested | **50.000** | 0.6400 | 49.440 |
| source = miro (7396 chunks, inillucent path: exhaustive) | recall@10 within the filter | **1.000** | 0.0640 | 0.8840 |

## Lexical retrieval

PostgreSQL full text search joins query terms with `&` through `to_tsquery`, so a chunk must contain every term, and ranks with `ts_rank_cd`, which has no document length normalization and no term frequency saturation. Joining the terms with `|` instead would return more rows and has not been measured. inillucent scores any term with BM25, which returns partial matches and weights rare terms above common ones. The natural language set is drawn from document headings, which read like questions; the identifier set is rare literal tokens, where a lexical index should be at its strongest and where exact matching matters most.

| measurement | metric | inillucent | pgvector (extension defaults) |
|---|---|---|---|
| natural language, from headings | mean reciprocal rank | **0.7221** | 0.5896 |
| natural language, from headings | success@10 | **0.9200** | 0.7500 |
| natural language, from headings | rows returned of 50 | **49.690** | 17.033 |
| identifiers, rare literal tokens | mean reciprocal rank | **0.5442** | 0.1363 |
| identifiers, rare literal tokens | success@10 | **0.6633** | 0.1633 |
| identifiers, rare literal tokens | rows returned of 50 | **39.900** | 3.280 |

## Hybrid retrieval, whole pipeline

Both sides, fused, capped at two chunks per document, truncated to ten. nDCG@10 is the primary measurement and the rest are diagnostics, because success@1, success@10 and reciprocal rank are the same ranking looked at from three more angles and giving each of them a vote turns one behaviour into four wins. The ground truth here is a whole document: a title or heading query is answered by any chunk of the document that carries it, since the corpus writes both into the front of every chunk. That grades finding the right page, which is worth grading and is not what an agent needs; the passage family below grades finding the right paragraph.

| measurement | metric | inillucent | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| document identity, title as query | nDCG@10 | **0.9756** | 0.7961 | 0.8148 |
| document identity, title as query | success@1 | **0.9600** | 0.7750 | 0.7933 |
| document identity, title as query | success@10 | **0.9967** | 0.8450 | 0.8600 |
| document identity, title as query | mean reciprocal rank | **0.9762** | 0.8010 | 0.8186 |
| document identity, title as query | precision@10 | **0.1637** | 0.1430 | 0.1454 |
| document identity, title as query | hybrid search ms | **6.044** | 11.753 | 12.172 |
| natural language, heading as query | nDCG@10 | **0.7477** | 0.6270 | 0.6271 |
| natural language, heading as query | success@1 | **0.5900** | 0.5000 | 0.5000 |
| natural language, heading as query | success@10 | **0.8967** | 0.7767 | 0.7733 |
| natural language, heading as query | mean reciprocal rank | **0.7104** | 0.5933 | 0.5950 |
| natural language, heading as query | precision@10 | **0.0973** | 0.0864 | 0.0849 |
| natural language, heading as query | hybrid search ms | **4.182** | 17.312 | 18.322 |

## Passage evidence, perturbation and multi-source

The families the document-level ground truth cannot express. A passage query is built from one body sentence with the chunk's own breadcrumb words removed, so it cannot be answered by the title text every chunk of that document shares, and with the two rarest remaining words removed, so it is a description of the passage rather than a quotation of it. Judgements are graded: the passage that answers is grade 3, the rest of its document is grade 2 supporting context, and graded nDCG is what separates them. The transposed-character and three-keyword packs are those same queries made harder, on ground truth that did not move, so the gap between the clean score and the perturbed one is exactly what the perturbation cost. The multi-source pack needs evidence from two documents in two sources and is scored on whether both arrived: success@10 counts half an answer as a success, and evidence recall does not.

| measurement | metric | inillucent | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| passage evidence | graded nDCG@10 | **0.7045** | 0.5839 | 0.6027 |
| passage evidence | success@1 | **0.7778** | 0.6184 | 0.6329 |
| passage evidence | success@10 | **0.8019** | 0.6787 | 0.6981 |
| passage evidence | mean reciprocal rank | **0.7838** | 0.6361 | 0.6520 |
| passage evidence | precision@10 | **0.0809** | 0.0687 | 0.0702 |
| passage evidence, one transposed character | graded nDCG@10 | **0.6794** | 0.3707 | 0.3969 |
| passage evidence, one transposed character | success@1 | **0.7584** | 0.3299 | 0.3558 |
| passage evidence, one transposed character | success@10 | **0.7818** | 0.4494 | 0.4805 |
| passage evidence, one transposed character | mean reciprocal rank | **0.7647** | 0.3690 | 0.3970 |
| passage evidence, one transposed character | precision@10 | **0.0786** | 0.0459 | 0.0484 |
| passage evidence, three keywords | graded nDCG@10 | **0.6266** | 0.4516 | 0.4651 |
| passage evidence, three keywords | success@1 | **0.6256** | 0.4638 | 0.4686 |
| passage evidence, three keywords | success@10 | **0.7681** | 0.5604 | 0.5773 |
| passage evidence, three keywords | mean reciprocal rank | **0.6798** | 0.4962 | 0.5050 |
| passage evidence, three keywords | precision@10 | **0.0769** | 0.0563 | 0.0579 |
| multi-source, evidence in two sources | evidence recall@10 | **0.6237** | 0.1498 | 0.1923 |
| multi-source, evidence in two sources | graded nDCG@10 | **0.6197** | 0.1459 | 0.1786 |
| multi-source, evidence in two sources | success@10 | **0.9450** | 0.3200 | 0.3950 |
| multi-source, evidence in two sources | mean reciprocal rank | **0.7916** | 0.2099 | 0.2474 |

## Abstention on questions nothing answers

Queries built by mixing the distinctive words of two documents from two sources, which the corpus builder draws from disjoint pools, so no chunk holds material from both and the question sounds entirely plausible with no answer. This is the failure mode that does not announce itself: ten confident looking passages about nothing, and an agent writes an answer out of them. Each engine is calibrated on its own scale rather than on a shared one, so the comparison needs no assumption that a inillucent score and a `ts_rank_cd` score mean the same thing: the threshold is the fifth percentile of that engine's own top result score over answerable calibration queries the report does not score, and the measurement is how often it exceeds its own threshold on a question with no answer. Lower is better. The number the threshold is set on is deliberately not the fused score: per-list min-max normalization, which is the fusion that ranks best here, maps the best hit of every list to 1.0 whether the list is good or hopeless, so no threshold on it exists. Every hit therefore also carries a confidence computed the theoretical min-max way — each side divided by a bound the results had no say in — whatever fusion ordered the list. Ranking and confidence are two questions and one number could not answer both.

| measurement | metric | inillucent | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| questions with no answer in the corpus | confident answer rate at the calibrated threshold | **0.0050** | 1.000 | 1.000 |
| calibration queries, 5th percentile of the top result confidence | abstention threshold | **0.3474** | 0.0325 | 0.0325 |
| answerable minus unanswerable | mean top result confidence gap | **0.4445** | -0.0572 | -0.0509 |

## Fusion methods compared

Reciprocal Rank Fusion keeps only position and discards score magnitude, which makes it robust across two scoring scales that are not comparable but blind to how good the top hit actually was. Normalized score fusion keeps the magnitude. This family decides which one inillucent should default to on this corpus, rather than inheriting the choice. The columns here are fusion methods, not engines.

| measurement | metric | Reciprocal Rank Fusion, k = 60 | min-max, vector weight 0.35 (default) | min-max, vector weight 0.5 | min-max, vector weight 0.7 | convex, vector weight 0.35 |
|---|---|---|---|---|---|---|
| document identity | nDCG@10 | 0.9114 | **0.9808** | 0.9522 | 0.8577 | 0.9671 |
| document identity | success@1 | 0.8383 | **0.9667** | 0.8900 | 0.8133 | 0.9367 |

## Quantization and the Matryoshka ladder

Two independent levers for memory. int8 scalar quantization keeps all 768 dimensions and spends one byte each; Matryoshka truncation keeps full precision on fewer dimensions. Qdrant reports int8 costing under 1% accuracy and binary quantization degrading below roughly 1000 dimensions, which is why binary is not offered here at 768. These are the measured numbers on this corpus, not the vendor's. The columns are configurations of inillucent, not engines. This family builds eleven separate indexes, so it runs on an evenly strided sample of the corpus rather than all of it; the sample size is in the row label.

| measurement | metric | 64 dims, f32 | 64 dims, int8 | 128 dims, f32 | 128 dims, int8 | 256 dims, f32 | 256 dims, int8 | 512 dims, f32 | 512 dims, int8 | 768 dims, f32 | 768 dims, int8 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| against the 768 dimension f32 exact ranking, 25000 chunks | recall@10 | 0.2950 | 0.3000 | 0.4450 | 0.4450 | 0.5300 | 0.5300 | 0.6850 | 0.6850 | **0.8600** | **0.8600** |
| storage | bytes per vector | 256.000 | **68.000** | 512.000 | 132.000 | 1024 | 260.000 | 2048 | 516.000 | 3072 | 772.000 |

## Latency

Wall clock per query, measured inside the calling process after a warmup pass. The median is the primary measurement and the mean is a diagnostic beside it, which is the one place on this card where a paired test over per-query values is *not* the better evidence: a mean latency is not robust, a few scheduler stalls from something else on the machine move it by more than the difference being measured, and a run that catches one reports a filtered mean of 2.0 ms against a median of 0.8 ms. The 95th percentile is reported because a mean also hides the tail people notice. inillucent pays no network cost because it is a library, while pgvector pays a loopback round trip. That is a genuine difference in the deployed system, but it is not a difference in index quality, so read this alongside the accuracy families rather than instead of them.

| measurement | metric | inillucent | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| no predicate | vector search p50 ms | **0.7040** | 1.519 | 2.439 |
| no predicate | vector search mean ms | **0.7577** | 1.598 | 2.513 |
| no predicate | vector search p95 ms | **1.228** | 2.316 | 3.709 |
| source = slack | vector search p50 ms | **0.8642** | 1.393 | 39.767 |
| source = slack | vector search mean ms | **0.8805** | 1.455 | 44.171 |
| source = slack | vector search p95 ms | **1.087** | 2.243 | 99.317 |

## Build cost and footprint

| engine | chunks | documents | build seconds | graph layers | graph edges | lexical terms | lexical postings | vectors MB | int8 codes MB |
|---|---|---|---|---|---|---|---|---|---|
| inillucent | 185078 | 38847 | 165.2 | 4 | 6119440 | 494179 | 11643704 | 568.6 | 142.9 |

## Provenance

What this run was, so a number on this card can be reproduced rather than only repeated. The per-query file named here holds one line per engine per query, with the ranking, the component scores and the metrics that query contributed, which is what makes the intervals above recomputable without paying for the run again.

| field | value |
|---|---|
| baseline database | postgres://postgres:***@127.0.0.1:5433/inillucent_synth |
| command | C:\jason\dev\inillucent\target\release\inillucent-bench.exe grade --database-url postgres://postgres:inillucent@127.0.0.1:5433/inillucent_synth --cache J:/inillucent-embeddings/corpus.cache --model-dir J:/inillucent-embeddings/models/nomic-embed-text-v1.5 --device cuda:0 --per-source 100 --runs-dir J:/inillucent-embeddings/runs --out C:/jason/dev/inillucent/inillucent-scorecard.md |
| commit | 0c14f71d979e38345138479caff3988513d372fb (working tree dirty) |
| corpus cache | J:/inillucent-embeddings/corpus.cache |
| device | Cuda(0) |
| embedding model | J:/inillucent-embeddings/models/nomic-embed-text-v1.5/model.onnx |
| host | windows x86_64, 24 logical processors |
| per-query records | 8139 lines in J:/inillucent-embeddings/runs\1788237655-0c14f71d\per-query.jsonl |
| query seeds | calibration=1012, heading=12, identifier=13, identity=11, multi_source=16, passage=14, unanswerable=15 |
| ranking settings | adaptive=AdaptiveWeights { base: 0.35, out_of_vocabulary_gain: 0.1, identifier_gain: 0.1, separation_gain: 0.1, coverage_gain: 0.1, floor: 0.05, ceiling: 0.95 }, adaptive_fusion=true, filtered_ef_search=400, fusion=NormalizedScore { vector_weight: 0.35 }, lexical_coverage=3, lexical_phrase=0.75, lexical_prefix=false, lexical_proximity=1, lexical_rescore_depth=6, lexical_tier=false, mmr_lambda=1, per_source=100 |
| run id | 1788237655-0c14f71d |
| run manifest | J:/inillucent-embeddings/runs\1788237655-0c14f71d\manifest.json |
| statistics seed | 20260901 |

## What these numbers do not say

- Every number is measured on the synthetic corpus this repository builds, with one embedding model. The suite is reusable against another corpus, but these numbers describe this one. The corpus reproduces the size, the per source split, the chunk length distribution and the ingestion ordering of the private corpus this engine was first graded on; retrieval difficulty is not identical, because encyclopedia articles are not the same material as one company's pages.
- Document identity ground truth uses a document's own title as the query. Titles share vocabulary with their own bodies, which flatters lexical retrieval. It flatters both engines equally, so the comparison holds even though the absolute figure is optimistic.
- Both engines are graded on the same vectors: the corpus is embedded once and the identical bytes are written to the cache inillucent reads and to the PostgreSQL column pgvector reads. A score difference therefore cannot come from the embedding model. The `embed-check` command verifies that pairing by re-embedding a sample and comparing against what is stored.
- Latency is measured inside the calling process. inillucent pays no network cost because it is a library; pgvector pays a loopback round trip. That is a real difference in the deployed system rather than a measurement artefact, but it is not a difference in index quality.

