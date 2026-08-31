# Findings

What the engine measured at, what changed, what each test costs, and how to keep running these tests
over the life of the project without paying for a full graded run every time.

Everything here was measured on one machine: two RTX 5090s, a 24 core processor, 127.5 GB of memory,
PostgreSQL 17.2 on an NVMe, Windows 11.

---

## 1. The result

| | comparable measurements | won | tied | lost |
|---|---|---|---|---|
| The card that shipped with the repository | 30 | 17 | 5 | 8 |
| The same code, rebuilt on this corpus | 30 | 19 | 4 | 7 |
| **After this work** | **30** | **26** | **4** | **0** |

Correctness gates pass throughout: 2,367 returned rows across 11 filter shapes each satisfying its
predicate, and determinism, the per-document cap, soft-delete exclusion, stopword-only and empty and
oversized queries, and `k = 0`.

**The four ties are the ceiling of their metric.** Three are sources where both engines return all
50 rows asked for, one is a source where both reach recall 1.000. There is no number above them.

### Where the wins are

| family | measurement | rust-db | best pgvector |
|---|---|---|---|
| Lexical | natural language headings, success@10 | **0.9222** | 0.7778 |
| Lexical | natural language headings, MRR | **0.7153** | 0.6245 |
| Lexical | natural language headings, rows of 50 | **49.467** | 16.856 |
| Lexical | rare identifiers, success@10 | **0.6222** | 0.1444 |
| Lexical | rare identifiers, MRR | **0.5509** | 0.1250 |
| Hybrid | document identity, nDCG@10 | **0.9816** | 0.8184 |
| Hybrid | document identity, success@1 | **0.9722** | 0.8000 |
| Hybrid | natural language, nDCG@10 | **0.7588** | 0.6439 |
| Hybrid | natural language, success@1 | **0.6222** | 0.5667 |
| Hybrid | natural language, success@10 | **0.9222** | 0.7556 |
| Filtered | `source = github`, rows of 50 | **50.000** | 34.960 |
| Filtered | `source = jira`, recall@10 in filter | **1.000** | 0.3280 |
| Latency | no predicate, p50 | **0.606 ms** | 1.595 ms |
| Latency | `source = slack`, p50 | **0.565 ms** | 1.263 ms |

---

## 2. What was actually wrong, and what fixed it

The seven losses were one hair-thin row count and, six times over, **natural-language queries**. The
whole diagnosis is in a pair of numbers that was not on the losing list: on heading queries rust-db's
lexical side returned **49.5 rows of 50** and PostgreSQL returned **6.7** — and PostgreSQL scored
higher.

PostgreSQL's full text search does two things BM25 does not:

- **`to_tsquery` joins terms with `&`.** A chunk missing one query word is not a worse answer, it is
  not an answer. On 185,078 chunks that is a very strong prior, because thousands of chunks contain
  *some* of any question.
- **`ts_rank_cd` is cover density ranking.** A chunk whose query terms sit close together outranks
  one that mentions the same words in different paragraphs.

rust-db was finding more of the right chunks and putting them lower.

### The four changes

**Coverage weighting.** A hit's score is scaled by the share of the query's inverse-document-frequency
mass the chunk holds, raised to an exponent (default 3.0). The same preference `&` expresses, as a
gradient: everything stays reachable, but a chunk holding one word of a six-word question ranks below
one holding five however often it repeats that word.

**Positional proximity.** The inverted index records token positions, and the leading hits are scaled
by `matched terms / width of the smallest window holding one of each` — 1.0 for an exact phrase,
falling as the terms spread out. Only the top `6k` are rescored; a covering window costs more than
scoring and almost every chunk BM25 touched was never going to be returned.

**Top-k selection in the exhaustive scan.** Unrelated to the lexical work, and the reason two of the
published losses were already gone before any of it. The filtered path used to collect every passing
chunk and sort all of it to return 50 — 17,642 chunks sorted so 50 could be read. It now reduces to
the best `k` per thread as candidates are produced. `source = slack` p50 **1.558 ms → 0.730 ms**,
p95 **2.728 → 0.947**.

**Search budget parity on filtered queries.** pgvector's well-configured mode uses
`hnsw.ef_search = 400` on a filtered query and 100 on an unfiltered one; rust-db used 128 for
everything, so on the one family that is entirely about filtered search it was walking a quarter as
wide. It now takes the same asymmetry, decided by the *same* predicate test both engines use. That
also moves rust-db's own cost model — exhaustive search is chosen below
`sqrt(ef_search x 32 x chunks)`, which at 400 is 48,672 chunks — so `github`'s 47,497 is scanned
exactly rather than walked approximately: **34.800 rows → 50.000, recall@10 0.408 → 1.000.**

### Fairness

Fusion is a ranking policy, not a retrieval capability, so **both engines are now fused the same
way**: the harness sets one method on rust-db and on both pgvector configurations together, and the
PostgreSQL engine fuses through the same arithmetic over its keys. pgvector's scores went *up* as a
result — its natural-language nDCG rose from 0.557 to 0.644 — and the hybrid family stayed a
measurement of retrieval.

Coverage and proximity are one-sided, and only because PostgreSQL already has what they buy.

### Two settings that measured off, against expectation

- **Prefix matching** (`town` also matching `township`, what `:*` does). On a 494,000 term dictionary
  it mostly credits a chunk with holding a query term it does not hold — the exact judgement coverage
  weighting depends on. Off is better on heading MRR and across the hybrid family; it costs a
  thousandth of identifier MRR in a scenario rust-db wins better than four to one.
- **Tiering** (rank by how many query terms a chunk holds, then by score — the ordering `&` gives
  PostgreSQL). Kept, defaulted off. It is what rescues a caller who sets the coverage exponent to 0,
  lifting heading success@10 from 0.778 to 0.889; but at coverage 3 the two orderings agree, and
  where they disagree idf mass is the better judge than a count of terms (success@1 0.5778 untiered
  against 0.5667 tiered).

### A bug worth naming

The first version of the proximity pass rescored `hits[..depth]` **before** the ranking existed, when
`hits` was still in `HashMap` iteration order — an arbitrary subset. It still improved the scores
slightly, which is exactly what made it hard to see. Rescoring after the sort and re-sorting took
heading success@10 from 0.678 to **0.889** and MRR from 0.435 to **0.671**.

The same lesson applies to a check in the harness. `synth-check` was failing at 98.9% of rare
identifier queries being answerable, one short of its floor. Not the corpus — the check: identifier
tokens are cut with `char::is_alphanumeric`, which is Unicode aware, and were compared with
`eq_ignore_ascii_case` against a needle that had been through Unicode `to_lowercase`. An accented
letter never matches itself.

---

## 3. What each test costs

| stage | corpus | wall clock | what it is for |
|---|---|---|---|
| `cargo test --release -p rustdb-core` | fixtures | **1.8 s**, 121 tests | every ranking property, in isolation |
| `synth-build` | either | **17 s** | assemble the corpus from the derived files |
| `synth-check` | 185,078 chunks | **2.7 s** | every gate, before paying to embed |
| `synth-embed` | 18,685 chunks | **38 s** | two GPU sessions |
| `synth-embed` | 185,078 chunks | **7 m 10 s** | two GPU sessions |
| `embed-check` | 185,078 chunks | **~30 s** | 200 chunks re-embedded and compared |
| `synth-load` | 18,685 chunks | **~15 s** | rows, full text index, HNSW |
| `synth-load` | 185,078 chunks | **48 s + 32 s** | rows and indexes, then a parallel HNSW build |
| `tune` | 18,685 chunks | **31 s** for 55 settings | choosing a default |
| `tune` | 185,078 chunks | **~30 min** for 48 settings | confirming one at scale |
| `grade` | 18,685 chunks | **1 m 19 s** | the whole card, both engines |
| `grade` | 185,078 chunks | **4 m 21 s** | the number that ships |

The one-off setup, which is not paid again: ~2 GB of public material downloaded (both simplewiki
CirrusSearch dumps, eight shallow clones, 8 x 10 pages of issues), 60,000 articles streamed out of
the 43 GB English dump, and the 547 MB ONNX model.

Two things dominate a `grade`: the index build (120 s of the 261) and, in the ladder family, eleven
more index builds on a 25,000 chunk sample.

### The embedding numbers in detail

**185,078 chunks in 7 minutes 10 seconds**, against the README's estimate of eight to twelve hours on
a laptop processor. Verified rather than assumed: `embed-check` reports mean cosine **1.000000**
between the stored vectors and a fresh re-embedding of 200 chunks spread across the corpus, no
vectors of the wrong width and none off unit length.

Where the speed comes from is not what I expected. On 18,685 chunks at batch 64:

| configuration | wall clock |
|---|---|
| one session, `cuda:0` | 38 s |
| one session, `cuda:1` | 43 s |
| two sessions, `cuda:0` + `cuda:1` | 18 s |
| two sessions, both on `cuda:0` | 17 s |
| two sessions, both on `cuda:1` | 19 s |
| four sessions across both cards | 22–27 s |

Two sessions is 2.1x, **a second card is worth nothing over a second session on the first one**, and
a fourth session is worse than two. A 137M parameter encoder does not saturate a 5090; what a second
session hides is the host-side serial work between inference calls — tokenizing, and mean-pooling
`[batch, seq, 768]` in Rust. The run uses both cards because the ticket asked for both, and the
honest reading is that one card with two sessions would do the same job.

Two defects the GPU run exposed, both fixed:

- **The batch size did not bound attention memory.** Batches were formed by count after sorting by
  length, so 64 chunks at the 1,900 token limit asked for 64 x 12 heads x 1900² x 4 bytes = **11.1
  GB** in one allocation, and the run died 45% of the way through. Attention is quadratic in sequence
  length; batches are now formed against a budget on `texts x longest²`, computed from real token
  counts.
- **The arena over-reserved.** ONNX Runtime extends its device arena by the next power of two, so the
  first session grew into the whole card and the second failed on an allocation that would have
  fitted. Now `kSameAsRequested`.

---

## 4. Testing strategy

### The principle

**Most of the cost of evaluating a search engine is index builds, and almost none of the questions
need one.** A `grade` on the full corpus is 4 m 21 s, and 120 s of that is building the index. None
of the ranking settings — coverage, proximity, tiering, prefix, fusion, fusion weight — changes the
postings, the positions, the graph or the codes. They are read at query time.

So `tune` builds **one** index and sweeps every setting against it. 55 settings in 31 seconds on the
small corpus. That is the difference between choosing a default in a coffee break and choosing it
over a day.

```sh
./target/release/rustdb-bench tune --cache corpus-small.cache \
  --coverages 0,1,2,3 --proximities 0,0.5,1 --weights 0.2,0.35,0.5 \
  --prefixes true,false --tiers true,false --seed-offset 100
```

It reports the three families the settings can move — lexical retrieval on its own and both hybrid
query sets — best first. It deliberately reports nothing else: no latency, no filtered recall, no
correctness gates, because no setting in it touches them. `grade` is for those.

### The ladder

Run the cheapest thing that can still say no.

1. **`cargo test`** — 1.8 seconds. Every ranking property that can be stated on a five-chunk fixture
   is stated there: that coverage raises a complete match, that proximity prefers terms sitting
   together, that the covering window is right when the best one is at the end, that a batch never
   exceeds the attention budget. A change that breaks one of these never reaches a corpus.
2. **`tune` on the small corpus** — 31 seconds. Choose the setting.
3. **`grade` on the small corpus** — 1 m 19 s. Does it hold up against both baselines, and did any
   correctness gate move?
4. **`grade` on the full corpus** — 4 m 21 s. The number that ships.

Step 4 is the only one that needs the full corpus, and the small corpus is a real proxy for it:
`--scale 0.1` multiplies every source's document and chunk count while keeping the proportions
between sources and the ingestion ordering, so it is a smaller version of the same problem rather
than a slice of one source.

It is not a *perfect* proxy, and the difference is worth knowing. Two settings that won on the small
corpus lost on the full one, both for the same reason: the dictionary is ten times larger, so prefix
expansion has ten times as many terms to be wrong about. **Confirm on the full corpus before
shipping a default.**

### Not fitting the answer to the test

`tune --seed-offset N` shifts the query set seeds, so a setting is chosen on queries the graded run
will not use. Every default in the engine today was chosen at offset 100 and reported at offset 0.

That matters more than it sounds. Without it, a sweep of 48 settings on 90 queries will find
something that looks like a 0.02 improvement purely by picking the arm that suits those 90 queries,
and the score card would be restating the fitting rather than measuring anything.

### Incremental work, and what is not incremental yet

- **Embedding is resumable and incremental.** The vector file is append-only with fixed width
  records, so the count of whole records in it is the count of chunks already done; rerunning
  `synth-embed` continues from there. Growing the corpus and re-embedding costs only the new chunks.
- **The corpus is deterministic.** `synth-build` from the same derived files gives byte-identical
  output, so a rebuild does not invalidate a cache.
- **Index builds are not incremental.** `Index::add` then `commit` is the only path, and `commit`
  builds the HNSW graph from scratch, single threaded. On 185,078 chunks that is 120 seconds against
  PostgreSQL's 32 for the same graph with 7 parallel workers. It is the single biggest remaining
  performance gap, it is not on the score card because there is no comparable row for it, and it is
  the thing to fix next: not for the card, but because it is 60% of every iteration.
- **The score card's JSON is kept beside the markdown**, so a card can be re-rendered or re-judged
  without repaying the run.

### Keeping the baseline

The comparison needs a PostgreSQL with pgvector holding the same rows and the same vectors. On this
machine that is a **separate cluster on port 5433** with its data directory on `J:`, so a 185,078 row
HNSW build never competes with the cluster the rest of the machine uses. It is configured generously
on purpose — 8 GB shared buffers, 2 GB maintenance work memory (Windows caps it there), 7 parallel
maintenance workers, JIT off per pgvector's own guidance — because a comparison against a PostgreSQL
that was not given what it needs proves nothing.

```sh
pg_ctl -D J:/rust-db-embeddings/pgdata -l J:/rust-db-embeddings/logs/pg.log start
```

It holds `rustdb_synth` (185,078 chunks, 1.7 GB with its indexes) and `rustdb_synth_small` (18,685),
so the fast loop and the full loop each have a loaded baseline waiting.

---

## 5. What these numbers still do not say

- Every number is measured on the corpus this repository builds, with one embedding model. The
  suite is reusable against another corpus; these numbers describe this one.
- Document identity ground truth uses a document's own title as the query, which flatters lexical
  retrieval. It flatters both engines equally, so the comparison holds even though the absolute
  figure is optimistic. It is the reason field weighting on the title was **not** implemented: on
  this corpus it would encode the answer rather than measure retrieval.
- Latency is measured inside the calling process. rust-db pays no network cost because it is a
  library; pgvector pays a loopback round trip. That is a real difference in the deployed system
  rather than a measurement artefact, but it is not a difference in index quality.
- The `jira` source is 1,448 documents against a target of 1,967, because GitHub caps issue
  pagination at 1,000 results per repository and only about 15% of issues have a body of 300+
  characters. The corpus is 185,078 chunks against the published 186,786 — within 1%.
- The quantization ladder still says what it said: int8 costs nothing measurable (recall@10 0.8600
  either way at 768 dimensions) and Matryoshka truncation costs a great deal (0.6850 at 512, 0.2950
  at 64). Compress, do not shorten.
