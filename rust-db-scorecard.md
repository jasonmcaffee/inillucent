# rust-db Score Card

Generated at unix time 1788139066. Corpus: 186786 chunks across 39366 documents, 768 dimensional embeddings from `nomic-embed-text-v1.5` run in process at full precision.

The corpus is assembled from public data by this repository and embedded once. The identical vectors are written to the cache rust-db reads and to the PostgreSQL column pgvector reads, and every query is embedded once and handed to both engines, so the embedding model cancels out of the comparison entirely. A score difference is therefore attributable to indexing and ranking.

Engines graded:

- rust-db
- pgvector (extension defaults)
- pgvector (correctly configured)

## Verdict

Each comparable measurement below is scored against the better of the two pgvector configurations, not against the production one, because comparing against a misconfiguration would prove nothing. Rows whose columns are rust-db settings rather than engines, the quantization ladder and the `ef_search` sweep, are excluded: rust-db cannot beat itself.

**30 comparable measurements: 17 won, 5 tied, 8 lost. Correctness gates: all pass.**

Measurements where rust-db is worse than the best pgvector configuration, stated because a score card that cannot report a loss is not measuring anything:

| scenario | measurement | metric | rust-db | best baseline | which baseline |
|---|---|---|---|---|---|
| Lexical retrieval | natural language, from headings | success@10 | 0.6889 | 0.7000 | pgvector (extension defaults) |
| Lexical retrieval | natural language, from headings | mean reciprocal rank | 0.5262 | 0.5774 | pgvector (extension defaults) |
| Hybrid retrieval, whole pipeline | natural language, heading as query | nDCG@10 | 0.4071 | 0.5668 | pgvector (correctly configured) |
| Hybrid retrieval, whole pipeline | natural language, heading as query | success@1 | 0.2667 | 0.3889 | pgvector (extension defaults) |
| Hybrid retrieval, whole pipeline | natural language, heading as query | success@10 | 0.6333 | 0.7444 | pgvector (extension defaults) |
| Hybrid retrieval, whole pipeline | natural language, heading as query | mean reciprocal rank | 0.3585 | 0.5231 | pgvector (correctly configured) |
| Latency | source = slack | vector search p50 ms | 1.558 | 1.437 | pgvector (extension defaults) |
| Latency | source = slack | vector search p95 ms | 2.728 | 2.667 | pgvector (extension defaults) |

## Correctness gates

These pass or fail rather than scoring. An engine that returns rows it was told to exclude is not a faster engine, it is a wrong one, so a failure here caps the result regardless of any accuracy number.

| gate | result | detail |
|---|---|---|
| Filter correctness | pass | 2367 returned rows across 11 filter shapes, every one satisfying its predicate |
| Invariants | pass | determinism, the per document cap, soft delete exclusion, stopword only and empty and oversized queries, and k = 0 all behave as specified |

## Approximation accuracy against exhaustive cosine

Exhaustive cosine over the whole corpus defines the exact answer, so this is the measure of how much the graph gives up for its speed. It is reported for rust-db only: the same question cannot be asked of pgvector without reading its index internals, and pgvector's own accuracy against exact search is a property of the same HNSW algorithm at the same parameters. A figure well below what HNSW should reach at m = 16 and ef_construction = 64 would indicate a graph defect rather than a tuning choice.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| all sources, no predicate (180 queries) | recall@10 | 0.9028 | n/a | n/a |
| all sources, no predicate (180 queries) | recall@50 | 0.8992 | n/a | n/a |

## The ef_search tradeoff

`ef_search` is how wide the traversal keeps its candidate list, and it is the one knob a caller turns. Accuracy is measured against exhaustive cosine over the whole corpus, latency alongside it, so the default rust-db ships is a choice with a table behind it. The columns are settings, not engines.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| no predicate | recall@10 | n/a | n/a | n/a |
| no predicate | vector search p50 ms | n/a | n/a | n/a |

## Filtered vector search, per source

An application with one search tool per source filters on `source` on every call, so this is the common case rather than a corner case. pgvector applies the filter after the index scan and the initial scan yields only `hnsw.ef_search` candidates, so a query restricted to a minority source can come back empty. rust-db expands nodes that fail the predicate but admits only nodes that pass, so the walk continues until it has found enough passing chunks. Two numbers are reported per source: how many rows came back at all, and whether they were the right ones, measured against exhaustive cosine over the same passing set.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| source = confluence (93813 chunks, rust-db path: graph) | rows returned of 50 requested | **50.000** | 30.840 | **50.000** |
| source = confluence (93813 chunks, rust-db path: graph) | recall@10 within the filter | **0.9920** | 0.7040 | 0.9720 |
| source = github (47497 chunks, rust-db path: graph) | rows returned of 50 requested | **32.480** | 4.640 | 29.200 |
| source = github (47497 chunks, rust-db path: graph) | recall@10 within the filter | **0.3800** | 0.0760 | 0.3360 |
| source = slack (17616 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 1.800 | **50.000** |
| source = slack (17616 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0400 | 0.6160 |
| source = jira (11145 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 1.760 | 24.040 |
| source = jira (11145 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0440 | 0.1800 |
| source = figma (9091 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 0.3200 | **50.000** |
| source = figma (9091 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0320 | **1.000** |
| source = miro (7351 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 0.6000 | **50.000** |
| source = miro (7351 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0600 | 0.8760 |

## Lexical retrieval

PostgreSQL full text search joins query terms with `&` through `to_tsquery`, so a chunk must contain every term, and ranks with `ts_rank_cd`, which has no document length normalization and no term frequency saturation. Joining the terms with `|` instead would return more rows and has not been measured. rust-db scores any term with BM25, which returns partial matches and weights rare terms above common ones. The natural language set is drawn from document headings, which read like questions; the identifier set is rare literal tokens, where a lexical index should be at its strongest and where exact matching matters most.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| natural language, from headings | success@10 | 0.6889 | **0.7000** | n/a |
| natural language, from headings | mean reciprocal rank | 0.5262 | **0.5774** | n/a |
| natural language, from headings | rows returned of 50 | **50.000** | 14.389 | n/a |
| identifiers, rare literal tokens | success@10 | **0.4667** | 0.1889 | n/a |
| identifiers, rare literal tokens | mean reciprocal rank | **0.4214** | 0.1519 | n/a |
| identifiers, rare literal tokens | rows returned of 50 | **39.533** | 3.078 | n/a |

## Hybrid retrieval, whole pipeline

Both sides, fused, capped at two chunks per document, truncated to ten. This is the only family that grades fusion, and it is the closest measure of what a person asking a question actually experiences. success@1 is the strictest: it asks whether the right document is the first thing returned.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| document identity, title as query | nDCG@10 | **0.9008** | 0.7779 | 0.8059 |
| document identity, title as query | success@1 | **0.8056** | 0.7389 | 0.7778 |
| document identity, title as query | success@10 | **1.000** | 0.8389 | 0.8556 |
| document identity, title as query | mean reciprocal rank | **0.8810** | 0.7784 | 0.8074 |
| natural language, heading as query | nDCG@10 | 0.4071 | 0.5573 | **0.5668** |
| natural language, heading as query | success@1 | 0.2667 | **0.3889** | **0.3889** |
| natural language, heading as query | success@10 | 0.6333 | **0.7444** | **0.7444** |
| natural language, heading as query | mean reciprocal rank | 0.3585 | 0.5138 | **0.5231** |

## Fusion methods compared

Reciprocal Rank Fusion keeps only position and discards score magnitude, which makes it robust across two scoring scales that are not comparable but blind to how good the top hit actually was. Normalized score fusion keeps the magnitude. This family decides which one rust-db should default to on this corpus, rather than inheriting the choice. The columns here are fusion methods, not engines.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| document identity | nDCG@10 | n/a | n/a | n/a |
| document identity | success@1 | n/a | n/a | n/a |

## Quantization and the Matryoshka ladder

Two independent levers for memory. int8 scalar quantization keeps all 768 dimensions and spends one byte each; Matryoshka truncation keeps full precision on fewer dimensions. Qdrant reports int8 costing under 1% accuracy and binary quantization degrading below roughly 1000 dimensions, which is why binary is not offered here at 768. These are the measured numbers on this corpus, not the vendor's. The columns are configurations of rust-db, not engines. This family builds eleven separate indexes, so it runs on an evenly strided sample of the corpus rather than all of it; the sample size is in the row label.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| against the 768 dimension f32 exact ranking, 25000 chunks | recall@10 | n/a | n/a | n/a |
| storage | bytes per vector | n/a | n/a | n/a |

## Latency

Wall clock per query, measured inside the calling process after a warmup pass, reported as median and 95th percentile because a mean hides the tail people notice. rust-db pays no network cost because it is a library, while pgvector pays a loopback round trip. That is a genuine difference in the deployed system, but it is not a difference in index quality, so read this alongside the accuracy families rather than instead of them.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| no predicate | vector search p50 ms | **0.6295** | 1.577 | 2.558 |
| no predicate | vector search p95 ms | **1.161** | 3.008 | 4.297 |
| source = slack | vector search p50 ms | 1.558 | **1.437** | 58.170 |
| source = slack | vector search p95 ms | 2.728 | **2.667** | 107.117 |

## Build cost and footprint

| engine | chunks | documents | build seconds | graph layers | graph edges | lexical terms | lexical postings | vectors MB | int8 codes MB |
|---|---|---|---|---|---|---|---|---|---|
| rust-db | 186786 | 39366 | 170.8 | 4 | 6175920 | 512045 | 11727421 | 573.8 | 144.2 |

## What these numbers do not say

- Every number is measured on the synthetic corpus this repository builds, with one embedding model. The suite is reusable against another corpus, but these numbers describe this one. The corpus reproduces the size, the per source split, the chunk length distribution and the ingestion ordering of the private corpus this engine was first graded on; retrieval difficulty is not identical, because encyclopedia articles are not the same material as one company's pages.
- Document identity ground truth uses a document's own title as the query. Titles share vocabulary with their own bodies, which flatters lexical retrieval. It flatters both engines equally, so the comparison holds even though the absolute figure is optimistic.
- Both engines are graded on the same vectors: the corpus is embedded once and the identical bytes are written to the cache rust-db reads and to the PostgreSQL column pgvector reads. A score difference therefore cannot come from the embedding model. The `embed-check` command verifies that pairing by re-embedding a sample and comparing against what is stored.
- Latency is measured inside the calling process. rust-db pays no network cost because it is a library; pgvector pays a loopback round trip. That is a real difference in the deployed system rather than a measurement artefact, but it is not a difference in index quality.

