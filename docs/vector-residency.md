# Where the vectors live

An inillucent retrieval index holds four things: the chunk store, the BM25 postings, the HNSW graph,
and the vectors. On a real corpus the vectors are the largest of them, and they used to be
read onto the heap whenever an index was opened.

They are not any more. **A loaded index leaves its vectors in the file by default**, and reads them
back as it scores. `IndexConfig::resident_vectors = true` asks for the old behaviour.

This page says what that costs, what it buys, and how to decide. Every number here was measured on
Nikaya's mailbox — a 600,589 chunk index at 768 dimensions — rather than estimated.

## What the two modes are

| | `resident_vectors: false` (default) | `resident_vectors: true` |
|---|---|---|
| where the f32 vectors are | in `vectors.bin`, read as they are scored | on this process's heap |
| what holds them in memory | the operating system's page cache, which is reclaimable | this process, which is not |
| what an open costs | the store, the postings and the graph | those, plus 1.85 GB |
| what a scan costs | a sequential read per block, then the same dot products | the same dot products |

Nothing else differs. The two paths return **the same neighbours in the same order**, which is
asserted rather than assumed: `flat.rs`'s `a_filed_set_returns_what_the_resident_set_returns` runs
both over a corpus larger than one block and compares the whole result, chunk for chunk and distance
for distance, on every filter shape and at three values of k. A recall figure would have hidden a
reordering; an equality does not.

## Why the default changed

The vectors were being paid for by every process that opened the index, whether or not it ever ran a
semantic search. On this box that was most of them. Nikaya's MCP transport is the clearest case: it is
spawned once per agent session, and each copy opened its own index — most of 4 GB per session, for a
process that usually answers two or three questions and exits. That is the reason it was configured to
open no index at all, which meant an agent's searches ran on a different engine from the workspace's.

Serving that index peaks at 3,840 MB with the vectors resident and 2,080 MB with them filed, and the
1,760 MB between the two is the vectors. Leaving them in the file does
not make those bytes disappear when they are being used — the operating system caches the file, so a
warm scan reads from memory either way. What changes is *whose* memory it is. Page cache is
reclaimable: a machine that suddenly needs 2 GB for something else takes it back, and the next scan
pays a disk read. A heap allocation is not reclaimable, and a machine that needs the memory swaps or
fails.

## What it costs and what it buys

Measured on Nikaya's corpus with the deployed configuration, which puts every search on the exhaustive
path over all 600,589 chunks. The arms are interleaved, so drift on a shared machine falls across
all of them rather than on one. See `docs/real-world-use-cases/nikaya-postgres-to-inillucent.md`
for the whole measurement and the conditions it ran under.

Two rounds, interleaved, the first pass of each discarded so the page cache is warm and neither arm
is charged for the other's cold start. `k = 20` over the whole corpus. **The query is embedded before
the clock starts and the matched documents are not read back**, so what is timed is the index and
nothing else — an earlier version of this page timed a search through the whole application, where
the embedder and twenty record reads swamped the difference this page is about. Peak resident is the
process high water mark from `PeakWorkingSet64`, not a sample taken after the search finished.

| | p50 | p95 | peak resident |
|---|---:|---:|---:|
| exhaustive scan, vectors **filed** (default) | 19.03 ms | 23.70 ms | **2,080 MB** |
| exhaustive scan, vectors resident | **17.79 ms** | 21.69 ms | **3,840 MB** |
| graph, vectors **filed** (default) | 4.22 ms | 6.19 ms | **2,080 MB** | 
| graph, vectors resident | 3.82 ms | 5.75 ms | 3,839 MB |

**Residency costs about 1.76 GB and buys about 6% on a search that reads every vector, and nothing
outside the round-to-round spread on one that walks the graph.**

Those two results are consistent with each other rather than in tension. An exhaustive scan reads all
600,589 vectors, so where the vectors are is most of what it does, and the resident arm was faster in
both rounds by roughly a millisecond and a half. A graph search touches a few hundred vectors, the
read is a rounding error next to the rest of the work, and the difference changes sign between rounds.

Two things the table does not cover, and both favour residency:

- **A cold start.** The first search after a reboot reads the vector file, which the resident arm paid
  for at open instead.
- **A machine under memory pressure.** Page cache is reclaimable, which is the argument for filing it;
  the other side is that a machine which reclaims it makes the next scan pay a real disk read. If the
  index is competing with something that will actually take the memory, residency is how you keep it.

Nothing here says residency is slow. It says that on a warm, adequately provisioned machine it is
1.76 GB for six percent at best, and 1.76 GB is a cost every process that opens the index pays.

## Which to choose

Turn residency **on** when all of these hold:

- the machine has the memory to spare with nothing else competing for it,
- the index is opened once by a long lived process rather than per request,
- and semantic search latency is the thing you are optimising.

Leave it **off** — the default — when any of these hold:

- several processes open the same index,
- the process is short lived, so an open that reads 1.85 GB is most of what it does,
- the machine runs other things that would rather have the memory,
- or the corpus is large enough that the vectors do not fit comfortably anyway.

## How to turn it on

```rust
let config = IndexConfig { resident_vectors: true, ..IndexConfig::default() };
```

The setting is saved with the index, like every other one in `IndexConfig`, so an index built with it
reopens with it. An index written before this option existed opens with the vectors filed, which is
the new default and is what the build that wrote it would do today.

In Nikaya: `NIKAYA_RETRIEVAL_RESIDENT_VECTORS=1`. `GET /api/status` reports which mode is running,
under `retrievalIndex.residentVectors`, for the same reason it reports the engine name — a latency
number measured in one mode and read as though it were the other is a wrong number that looks right.

## What is going on underneath

A filed set keeps the open file and the byte offset of its first vector. Scoring one vector is a
positional read of `dims * 4` bytes; scanning many is a read of 2,730 at a time, which is 8 MB at 768
dimensions. The blocks are independent, so the exhaustive scan still runs across every core, and each
thread reads its own block positionally — which is why the file is read rather than seeked: a shared
cursor could not be split.

Reading rather than mapping is deliberate. A mapping would be one fewer copy. It would also make the
file undeletable while it is mapped, which the generation reclaim would then have to tolerate, and it
would need a platform primitive that positional reads do not.

**A filed index can still be appended to.** Vectors added after the load are held on the heap until
the index is saved, at which point they are written into the file and the next load has them filed
like the rest. So the heap holds what has arrived since the last save, not the corpus — which is what
lets Nikaya sync a mailbox into an index it is not holding.

**A build is always resident.** A build has just produced the vectors and has nowhere else to put
them, so `resident_vectors` describes what a *load* does. The peak memory of building an index is
therefore unchanged by this setting; what changes is the memory of serving one.
