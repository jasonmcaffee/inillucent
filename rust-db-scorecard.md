# rust-db Score Card

Generated at unix time 1788152090. Corpus: 185078 chunks across 38847 documents, 768 dimensional embeddings from `nomic-embed-text-v1.5` run in process at full precision.

The corpus is assembled from public data by this repository and embedded once. The identical vectors are written to the cache rust-db reads and to the PostgreSQL column pgvector reads, and every query is embedded once and handed to both engines, so the embedding model cancels out of the comparison entirely. A score difference is therefore attributable to indexing and ranking.

Engines graded:

- rust-db
- pgvector (extension defaults)
- pgvector (correctly configured)

## Verdict

Each comparable measurement below is scored against the better of the two pgvector configurations, not against the production one, because comparing against a misconfiguration would prove nothing. Rows whose columns are rust-db settings rather than engines, the quantization ladder and the `ef_search` sweep, are excluded: rust-db cannot beat itself.

**30 comparable measurements: 26 won, 4 tied, 0 lost. Correctness gates: all pass.**

No measurement was worse than the best the configured PostgreSQL baseline can do.

## Correctness gates

These pass or fail rather than scoring. An engine that returns rows it was told to exclude is not a faster engine, it is a wrong one, so a failure here caps the result regardless of any accuracy number.

| gate | result | detail |
|---|---|---|
| Filter correctness | pass | 2500 returned rows across 11 filter shapes, every one satisfying its predicate |
| Invariants | pass | determinism, the per document cap, soft delete exclusion, stopword only and empty and oversized queries, and k = 0 all behave as specified |

## Approximation accuracy against exhaustive cosine

Exhaustive cosine over the whole corpus defines the exact answer, so this is the measure of how much the graph gives up for its speed. It is reported for rust-db only: the same question cannot be asked of pgvector without reading its index internals, and pgvector's own accuracy against exact search is a property of the same HNSW algorithm at the same parameters. A figure well below what HNSW should reach at m = 16 and ef_construction = 64 would indicate a graph defect rather than a tuning choice.

| measurement | metric | rust-db |
|---|---|---|
| all sources, no predicate (180 queries) | recall@10 | 0.9111 |
| all sources, no predicate (180 queries) | recall@50 | 0.8968 |

## The ef_search tradeoff

`ef_search` is how wide the traversal keeps its candidate list, and it is the one knob a caller turns. Accuracy is measured against exhaustive cosine over the whole corpus, latency alongside it, so the default rust-db ships is a choice with a table behind it. The columns are settings, not engines.

| measurement | metric | ef_search = 64 | ef_search = 128 | ef_search = 256 | ef_search = 512 |
|---|---|---|---|---|---|
| no predicate | recall@10 | 0.8600 | 0.9250 | 0.9725 | **0.9775** |
| no predicate | vector search p50 ms | **0.3934** | 0.6075 | 1.095 | 1.762 |

## Filtered vector search, per source

An application with one search tool per source filters on `source` on every call, so this is the common case rather than a corner case. pgvector applies the filter after the index scan and the initial scan yields only `hnsw.ef_search` candidates, so a query restricted to a minority source can come back empty. rust-db expands nodes that fail the predicate but admits only nodes that pass, so the walk continues until it has found enough passing chunks. Two numbers are reported per source: how many rows came back at all, and whether they were the right ones, measured against exhaustive cosine over the same passing set.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| source = confluence (93813 chunks, rust-db path: graph) | rows returned of 50 requested | **50.000** | 34.080 | **50.000** |
| source = confluence (93813 chunks, rust-db path: graph) | recall@10 within the filter | **0.9960** | 0.8200 | 0.9720 |
| source = github (47497 chunks, rust-db path: graph) | rows returned of 50 requested | **50.000** | 1.360 | 34.960 |
| source = github (47497 chunks, rust-db path: graph) | recall@10 within the filter | **1.000** | 0.0680 | 0.3280 |
| source = slack (17616 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 0.3200 | **50.000** |
| source = slack (17616 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0280 | 0.6120 |
| source = jira (9429 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 3.080 | 37.880 |
| source = jira (9429 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0880 | 0.3280 |
| source = figma (9144 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 0.4800 | **50.000** |
| source = figma (9144 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0480 | **1.000** |
| source = miro (7396 chunks, rust-db path: exhaustive) | rows returned of 50 requested | **50.000** | 0.6400 | 49.440 |
| source = miro (7396 chunks, rust-db path: exhaustive) | recall@10 within the filter | **1.000** | 0.0640 | 0.8800 |

## Lexical retrieval

PostgreSQL full text search joins query terms with `&` through `to_tsquery`, so a chunk must contain every term, and ranks with `ts_rank_cd`, which has no document length normalization and no term frequency saturation. Joining the terms with `|` instead would return more rows and has not been measured. rust-db scores any term with BM25, which returns partial matches and weights rare terms above common ones. The natural language set is drawn from document headings, which read like questions; the identifier set is rare literal tokens, where a lexical index should be at its strongest and where exact matching matters most.

| measurement | metric | rust-db | pgvector (extension defaults) |
|---|---|---|---|
| natural language, from headings | success@10 | **0.9222** | 0.7778 |
| natural language, from headings | mean reciprocal rank | **0.7153** | 0.6245 |
| natural language, from headings | rows returned of 50 | **49.467** | 16.856 |
| identifiers, rare literal tokens | success@10 | **0.6222** | 0.1444 |
| identifiers, rare literal tokens | mean reciprocal rank | **0.5509** | 0.1250 |
| identifiers, rare literal tokens | rows returned of 50 | **35.811** | 3.267 |

## Hybrid retrieval, whole pipeline

Both sides, fused, capped at two chunks per document, truncated to ten. This is the only family that grades fusion, and it is the closest measure of what a person asking a question actually experiences. success@1 is the strictest: it asks whether the right document is the first thing returned.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| document identity, title as query | nDCG@10 | **0.9816** | 0.8098 | 0.8184 |
| document identity, title as query | success@1 | **0.9722** | 0.7944 | 0.8000 |
| document identity, title as query | success@10 | **0.9944** | 0.8611 | 0.8722 |
| document identity, title as query | mean reciprocal rank | **0.9824** | 0.8196 | 0.8257 |
| natural language, heading as query | nDCG@10 | **0.7588** | 0.6323 | 0.6439 |
| natural language, heading as query | success@1 | **0.6222** | 0.5556 | 0.5667 |
| natural language, heading as query | success@10 | **0.9222** | 0.7444 | 0.7556 |
| natural language, heading as query | mean reciprocal rank | **0.7196** | 0.6172 | 0.6289 |

## Fusion methods compared

Reciprocal Rank Fusion keeps only position and discards score magnitude, which makes it robust across two scoring scales that are not comparable but blind to how good the top hit actually was. Normalized score fusion keeps the magnitude. This family decides which one rust-db should default to on this corpus, rather than inheriting the choice. The columns here are fusion methods, not engines.

| measurement | metric | Reciprocal Rank Fusion, k = 60 | min-max, vector weight 0.35 (default) | min-max, vector weight 0.5 | min-max, vector weight 0.7 | convex, vector weight 0.35 |
|---|---|---|---|---|---|---|
| document identity | nDCG@10 | 0.9126 | **0.9816** | 0.9496 | 0.8481 | 0.9759 |
| document identity | success@1 | 0.8444 | **0.9722** | 0.8889 | 0.7889 | 0.9556 |

## Quantization and the Matryoshka ladder

Two independent levers for memory. int8 scalar quantization keeps all 768 dimensions and spends one byte each; Matryoshka truncation keeps full precision on fewer dimensions. Qdrant reports int8 costing under 1% accuracy and binary quantization degrading below roughly 1000 dimensions, which is why binary is not offered here at 768. These are the measured numbers on this corpus, not the vendor's. The columns are configurations of rust-db, not engines. This family builds eleven separate indexes, so it runs on an evenly strided sample of the corpus rather than all of it; the sample size is in the row label.

| measurement | metric | 64 dims, f32 | 64 dims, int8 | 128 dims, f32 | 128 dims, int8 | 256 dims, f32 | 256 dims, int8 | 512 dims, f32 | 512 dims, int8 | 768 dims, f32 | 768 dims, int8 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| against the 768 dimension f32 exact ranking, 25000 chunks | recall@10 | 0.2950 | 0.3000 | 0.4450 | 0.4450 | 0.5300 | 0.5300 | 0.6850 | 0.6850 | **0.8600** | **0.8600** |
| storage | bytes per vector | 256.000 | **68.000** | 512.000 | 132.000 | 1024 | 260.000 | 2048 | 516.000 | 3072 | 772.000 |

## Latency

Wall clock per query, measured inside the calling process after a warmup pass, reported as median and 95th percentile because a mean hides the tail people notice. rust-db pays no network cost because it is a library, while pgvector pays a loopback round trip. That is a genuine difference in the deployed system, but it is not a difference in index quality, so read this alongside the accuracy families rather than instead of them.

| measurement | metric | rust-db | pgvector (extension defaults) | pgvector (correctly configured) |
|---|---|---|---|---|
| no predicate | vector search p50 ms | **0.6061** | 1.595 | 2.200 |
| no predicate | vector search p95 ms | **0.9945** | 2.225 | 3.341 |
| source = slack | vector search p50 ms | **0.5651** | 1.263 | 34.567 |
| source = slack | vector search p95 ms | **1.139** | 1.982 | 87.422 |

## Build cost and footprint

| engine | chunks | documents | build seconds | graph layers | graph edges | lexical terms | lexical postings | vectors MB | int8 codes MB |
|---|---|---|---|---|---|---|---|---|---|
| rust-db | 185078 | 38847 | 120.0 | 4 | 6119440 | 494179 | 11643704 | 568.6 | 142.9 |

## What these numbers do not say

- Every number is measured on the synthetic corpus this repository builds, with one embedding model. The suite is reusable against another corpus, but these numbers describe this one. The corpus reproduces the size, the per source split, the chunk length distribution and the ingestion ordering of the private corpus this engine was first graded on; retrieval difficulty is not identical, because encyclopedia articles are not the same material as one company's pages.
- Document identity ground truth uses a document's own title as the query. Titles share vocabulary with their own bodies, which flatters lexical retrieval. It flatters both engines equally, so the comparison holds even though the absolute figure is optimistic.
- Both engines are graded on the same vectors: the corpus is embedded once and the identical bytes are written to the cache rust-db reads and to the PostgreSQL column pgvector reads. A score difference therefore cannot come from the embedding model. The `embed-check` command verifies that pairing by re-embedding a sample and comparing against what is stored.
- Latency is measured inside the calling process. rust-db pays no network cost because it is a library; pgvector pays a loopback round trip. That is a real difference in the deployed system rather than a measurement artefact, but it is not a difference in index quality.

